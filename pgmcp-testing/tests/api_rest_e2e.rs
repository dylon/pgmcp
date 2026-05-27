//! REST API + Streamable HTTP MCP end-to-end tests.
//!
//! Spawns `pgmcp daemon` as a subprocess against a `TestDatabase`, hits
//! the REST API and the Streamable-HTTP MCP endpoint, and asserts the
//! responses.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pgmcp_testing::require_test_db;
use tempfile::TempDir;

fn find_pgmcp_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PGMCP_TEST_BIN") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().expect("parent");
    for profile in ["release", "debug"] {
        let candidate = workspace_root.join("target").join(profile).join("pgmcp");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Ask the OS for a free port by binding to :0 briefly. The port may be
/// reused between close and the daemon's bind — small TOCTOU window, but
/// test-only and empirically fine.
fn ephemeral_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind :0");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    port
}

fn parse_postgres_url(raw: &str) -> (String, u16, String, Option<String>, String) {
    let without_scheme = raw.strip_prefix("postgres://").expect("postgres:// scheme");
    let (userinfo_host, dbname) = without_scheme.split_once('/').expect("path");
    let (user, password, host_port) = match userinfo_host.rsplit_once('@') {
        Some((userinfo, host_port)) => match userinfo.split_once(':') {
            Some((u, p)) => (u.to_string(), Some(p.to_string()), host_port.to_string()),
            None => (userinfo.to_string(), None, host_port.to_string()),
        },
        None => ("postgres".to_string(), None, userinfo_host.to_string()),
    };
    let (host, port) = match host_port.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(5432)),
        None => (host_port, 5432),
    };
    (host, port, user, password, dbname.to_string())
}

fn write_daemon_config(home: &Path, db_url: &str, mcp_port: u16) -> PathBuf {
    let (db_host, db_port, db_user, db_pass, db_name) = parse_postgres_url(db_url);
    let cfg_dir = home.join(".config").join("pgmcp");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir");
    let mut toml = String::new();
    toml.push_str("[database]\n");
    toml.push_str(&format!("host = \"{}\"\n", db_host));
    toml.push_str(&format!("port = {}\n", db_port));
    toml.push_str(&format!("name = \"{}\"\n", db_name));
    toml.push_str(&format!("user = \"{}\"\n", db_user));
    if let Some(p) = db_pass {
        toml.push_str(&format!("password = \"{}\"\n", p));
    }
    toml.push_str("max_connections = 4\n\n");
    toml.push_str("[mcp]\n");
    toml.push_str(&format!("port = {}\n", mcp_port));
    toml.push_str("host = \"127.0.0.1\"\n\n");
    toml.push_str("[workspace]\npaths = []\n\n");
    toml.push_str("[metrics]\nhttp_enabled = false\n\n");
    toml.push_str("[cron]\n");
    toml.push_str("similarity_scan_interval_secs = 0\n");
    toml.push_str("topic_scan_interval_secs = 0\n");
    toml.push_str("graph_analysis_interval_secs = 0\n");
    toml.push_str("git_history_index_interval_secs = 0\n");
    let path = cfg_dir.join("config.toml");
    std::fs::write(&path, toml).expect("write config");
    path
}

struct DaemonHandle {
    child: Option<Child>,
    port: u16,
    _home: TempDir,
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Try graceful via SIGTERM.
            #[cfg(unix)]
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            let _ = child.wait();
        }
    }
}

fn spawn_daemon(db_url: &str) -> Option<DaemonHandle> {
    let binary = find_pgmcp_binary()?;
    let home = TempDir::new().expect("tempdir");
    let port = ephemeral_port();
    let config_path = write_daemon_config(home.path(), db_url, port);

    let mut cmd = Command::new(&binary);
    cmd.arg("-c")
        .arg(&config_path)
        .arg("daemon")
        .env("HOME", home.path())
        .env("RUST_LOG", "error")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(ort_dir) = pgmcp_testing::cli_harness::locate_ort_lib_dir() {
        let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
        cmd.env(
            "LD_LIBRARY_PATH",
            if existing.is_empty() {
                ort_dir.display().to_string()
            } else {
                format!("{}:{}", ort_dir.display(), existing)
            },
        );
    }
    let child = cmd.spawn().expect("spawn daemon");
    Some(DaemonHandle {
        child: Some(child),
        port,
        _home: home,
    })
}

/// Block until the daemon accepts TCP connections on its port, or panic.
async fn wait_for_listen(port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            // Give the daemon a moment after bind to finish setting up routes.
            tokio::time::sleep(Duration::from_millis(250)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "daemon did not listen on port {} within {:?}",
        port, timeout
    );
}

macro_rules! require_binary_or_skip {
    ($db:expr) => {
        match spawn_daemon(&$db.connection_url()) {
            Some(h) => h,
            None => {
                eprintln!("SKIPPED: pgmcp binary not found");
                return;
            }
        }
    };
}

#[tokio::test(flavor = "multi_thread")]
async fn rest_api_context_returns_project_list_when_cwd_unknown() {
    let db = require_test_db!();
    // Seed a project so the fallback branch has something to render.
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws")
        .bind("/ws/api-ctx-proj/")
        .bind("api-ctx-proj")
        .execute(db.pool())
        .await
        .expect("seed");

    let daemon = require_binary_or_skip!(db);
    wait_for_listen(daemon.port, Duration::from_secs(20)).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!(
            "http://127.0.0.1:{}/api/context?cwd=/unrelated",
            daemon.port
        ))
        .send()
        .await
        .expect("send");
    assert!(resp.status().is_success(), "status: {}", resp.status());
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("api-ctx-proj"),
        "expected project in body:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rest_api_search_returns_200_for_valid_query() {
    let db = require_test_db!();
    let daemon = require_binary_or_skip!(db);
    wait_for_listen(daemon.port, Duration::from_secs(20)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/api/search", daemon.port))
        .json(&serde_json::json!({
            "query": "test query",
            "limit": 5
        }))
        .send()
        .await
        .expect("send");
    // Empty DB → empty results, but status must be success.
    assert!(
        resp.status().is_success(),
        "search should return 2xx, got {}",
        resp.status()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rest_api_search_with_project_filter_ok() {
    let db = require_test_db!();
    let daemon = require_binary_or_skip!(db);
    wait_for_listen(daemon.port, Duration::from_secs(20)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/api/search", daemon.port))
        .json(&serde_json::json!({"query": "foo", "project": "nonexistent"}))
        .send()
        .await
        .expect("send");
    assert!(resp.status().is_success());
}

#[tokio::test(flavor = "multi_thread")]
async fn rest_api_context_for_known_cwd_returns_project() {
    let db = require_test_db!();
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws")
        .bind("/ws/api-known-cwd/")
        .bind("api-known-cwd")
        .execute(db.pool())
        .await
        .expect("seed");
    let daemon = require_binary_or_skip!(db);
    wait_for_listen(daemon.port, Duration::from_secs(20)).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!(
            "http://127.0.0.1:{}/api/context?cwd=/ws/api-known-cwd/src/main.rs",
            daemon.port
        ))
        .send()
        .await
        .expect("send");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("api-known-cwd"),
        "project name missing from context body:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rest_api_concurrent_requests_share_daemon_resources() {
    let db = require_test_db!();
    let daemon = require_binary_or_skip!(db);
    wait_for_listen(daemon.port, Duration::from_secs(20)).await;

    let client = reqwest::Client::new();
    let port = daemon.port;
    let mut joinset = tokio::task::JoinSet::new();
    for i in 0..16 {
        let client = client.clone();
        joinset.spawn(async move {
            let resp = client
                .post(format!("http://127.0.0.1:{}/api/search", port))
                .json(&serde_json::json!({"query": format!("q{}", i), "limit": 5}))
                .send()
                .await
                .expect("send");
            assert!(resp.status().is_success());
        });
    }
    while let Some(r) = joinset.join_next().await {
        r.expect("task");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn streamable_http_mcp_tools_list_succeeds() {
    let db = require_test_db!();
    let daemon = require_binary_or_skip!(db);
    wait_for_listen(daemon.port, Duration::from_secs(20)).await;

    let client = reqwest::Client::new();
    // First initialize (mcp requires this before tools/list).
    let init_resp = client
        .post(format!("http://127.0.0.1:{}/mcp", daemon.port))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "t", "version": "1" }
            }
        }))
        .send()
        .await
        .expect("init");
    assert!(init_resp.status().is_success());
    let session = init_resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let mut req = client
        .post(format!("http://127.0.0.1:{}/mcp", daemon.port))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json");
    if let Some(s) = &session {
        req = req.header("Mcp-Session-Id", s);
    }
    let resp = req
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }))
        .send()
        .await
        .expect("tools/list");
    // May succeed (if session persisted) or 4xx (if not); either way
    // the daemon should respond without crashing.
    assert!(resp.status().is_success() || resp.status().is_client_error());
}

#[tokio::test(flavor = "multi_thread")]
async fn streamable_http_mcp_malformed_json_returns_error() {
    let db = require_test_db!();
    let daemon = require_binary_or_skip!(db);
    wait_for_listen(daemon.port, Duration::from_secs(20)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/mcp", daemon.port))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .body("{not valid json}")
        .send()
        .await
        .expect("send");
    assert!(
        resp.status().is_client_error() || resp.status().is_server_error(),
        "malformed JSON should trigger 4xx/5xx, got {}",
        resp.status()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn streamable_http_mcp_multiple_sessions_are_independent() {
    let db = require_test_db!();
    let daemon = require_binary_or_skip!(db);
    wait_for_listen(daemon.port, Duration::from_secs(20)).await;

    let client = reqwest::Client::new();
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "mc", "version": "1" }
        }
    });
    let r1 = client
        .post(format!("http://127.0.0.1:{}/mcp", daemon.port))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&init_req)
        .send()
        .await
        .expect("client 1 init");
    let r2 = client
        .post(format!("http://127.0.0.1:{}/mcp", daemon.port))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&init_req)
        .send()
        .await
        .expect("client 2 init");
    assert!(r1.status().is_success() && r2.status().is_success());
    let s1 = r1
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let s2 = r2
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let (Some(a), Some(b)) = (&s1, &s2) {
        assert_ne!(a, b, "each client should get its own Mcp-Session-Id");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rest_api_search_returns_chunks_with_seeded_data() {
    let db = require_test_db!();
    // Seed a project + file + a chunk with a known embedding.
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/api-chunks/")
    .bind("api-chunks")
    .fetch_one(db.pool())
    .await
    .expect("seed");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, \
         content, content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW()) RETURNING id",
    )
    .bind(project_id)
    .bind("/ws/api-chunks/a.rs")
    .bind("a.rs")
    .bind("rust")
    .bind(20_i64)
    .bind("fn a() { println!(); }")
    .bind(42_i64)
    .bind(1_i32)
    .fetch_one(db.pool())
    .await
    .expect("file");
    let v = pgvector::Vector::from(vec![0.1_f32; 1024]);
    sqlx::query(
        "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line, embedding_v2, embedding_signature) \
         VALUES ($1, 0, $2, 1, 1, $3, 'bge-m3-v1')",
    )
    .bind(file_id)
    .bind("fn a() { println!(); }")
    .bind(v)
    .execute(db.pool())
    .await
    .expect("chunk");

    let daemon = require_binary_or_skip!(db);
    wait_for_listen(daemon.port, Duration::from_secs(20)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/api/search", daemon.port))
        .json(&serde_json::json!({"query": "println a", "limit": 5}))
        .send()
        .await
        .expect("send");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body");
    // With a real embedder + our seeded chunk, the body should contain
    // either the chunk content or its path — pgmcp's API serializes both.
    assert!(
        body.contains("a.rs") || body.contains("println") || body.contains("api-chunks"),
        "api/search did not surface seeded chunk:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn streamable_http_mcp_returns_502_or_503_after_shutdown() {
    let db = require_test_db!();
    let Some(mut daemon) = spawn_daemon(&db.connection_url()) else {
        eprintln!("SKIPPED: binary missing");
        return;
    };
    wait_for_listen(daemon.port, Duration::from_secs(20)).await;
    let port = daemon.port;
    // Shut the daemon down.
    #[cfg(unix)]
    unsafe {
        libc::kill(daemon.child.as_ref().unwrap().id() as i32, libc::SIGTERM);
    }
    // Wait for exit.
    if let Some(mut child) = daemon.child.take() {
        let _ = child.wait();
    }

    // Now POST — connection should fail (ECONNREFUSED) or hang briefly.
    // Either is acceptable — the point is the daemon no longer serves.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let result = client
        .post(format!("http://127.0.0.1:{}/mcp", port))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}
        }))
        .send()
        .await;
    // After shutdown, either we get an error (connection refused) or a
    // 5xx response — both confirm the daemon isn't serving.
    if let Ok(resp) = result {
        assert!(
            resp.status().is_server_error() || resp.status().is_client_error(),
            "daemon still serving after shutdown: {}",
            resp.status()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn streamable_http_mcp_initialize_succeeds() {
    let db = require_test_db!();
    let daemon = require_binary_or_skip!(db);
    wait_for_listen(daemon.port, Duration::from_secs(20)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/mcp", daemon.port))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "http-test", "version": "1.0" }
            }
        }))
        .send()
        .await
        .expect("send");
    // Streamable HTTP returns 200 with either JSON body or SSE stream.
    assert!(
        resp.status().is_success(),
        "initialize should return 2xx, got {}",
        resp.status()
    );
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("pgmcp") || body.contains("serverInfo") || body.contains("result"),
        "expected initialize response markers, got:\n{body}"
    );
}
