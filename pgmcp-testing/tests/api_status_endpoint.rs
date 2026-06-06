//! Real-Postgres end-to-end test for `GET /api/status`.
//!
//! Spawns `pgmcp daemon` against a `TestDatabase` and asserts that the
//! status endpoint returns a complete snapshot whose database section
//! contains the redacted URL (never the raw password).

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
    if let Some(p) = &db_pass {
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
    /// The password the daemon was configured with (or None). Tests
    /// assert this string never appears in /api/status output.
    password: Option<String>,
    _home: TempDir,
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
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
    let (_h, _p, _u, password, _n) = parse_postgres_url(db_url);

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
        password,
        _home: home,
    })
}

async fn wait_for_listen(port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
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

// ============================================================================
// Tests
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn status_endpoint_returns_complete_snapshot_for_seeded_corpus() {
    let db = require_test_db!();
    // Seed one project + one file so the model_state numbers are
    // non-zero and thus distinguishable from "endpoint failed silently".
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws")
        .bind("/ws/status-test/")
        .bind("status-test")
        .execute(db.pool())
        .await
        .expect("seed project");

    let daemon = require_binary_or_skip!(db);
    wait_for_listen(daemon.port, Duration::from_secs(20)).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://127.0.0.1:{}/api/status", daemon.port))
        .send()
        .await
        .expect("send");
    assert!(resp.status().is_success(), "status: {}", resp.status());
    let body = resp.text().await.expect("body");
    let v: serde_json::Value = serde_json::from_str(&body).expect("json");

    // Top-level shape.
    assert!(v.get("daemon").is_some(), "missing daemon section: {body}");
    assert!(v.get("database").is_some(), "missing database section");
    assert!(
        v.get("model_state").is_some(),
        "missing model_state section"
    );
    assert!(v.get("counters").is_some(), "missing counters section");

    // Daemon section sanity.
    let daemon_obj = &v["daemon"];
    assert!(daemon_obj["version"].is_string());
    // uptime_secs is u64 — accept 0 (test starts daemon and queries
    // immediately) but require the field to be present and numeric.
    assert!(daemon_obj["uptime_secs"].is_u64());
    assert!(daemon_obj["http_mcp_sessions"].is_u64());

    // Database section: project_count = 1 from our seed.
    let model = &v["model_state"];
    assert_eq!(
        model["project_count"].as_i64(),
        Some(1),
        "expected 1 seeded project; got {model}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn status_endpoint_database_url_is_redacted_no_password_leak() {
    let db = require_test_db!();
    let daemon = require_binary_or_skip!(db);
    wait_for_listen(daemon.port, Duration::from_secs(20)).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://127.0.0.1:{}/api/status", daemon.port))
        .send()
        .await
        .expect("send");
    let body = resp.text().await.expect("body");

    // The redaction marker must be present.
    assert!(
        body.contains(":****@"),
        "redacted URL marker `:****@` must appear in /api/status payload; got:\n{body}"
    );

    // The raw password (if any) must NEVER appear in the response.
    if let Some(pw) = daemon.password.as_deref()
        && !pw.is_empty()
    {
        assert!(
            !body.contains(pw),
            "LEAK: raw password substring `{pw}` appears in /api/status payload"
        );
    }
}
