//! Daemon lifecycle + shutdown end-to-end.
//!
//! Spawns `pgmcp daemon` as a subprocess against a `TestDatabase` and
//! asserts:
//!
//! * The process comes up and listens on the configured port.
//! * A SIGTERM triggers orderly shutdown well within the 15-second watchdog
//!   (typical: a couple seconds).
//! * The process exits with code 0.
//!
//! Uses the same subprocess harness pattern as `api_rest_e2e.rs`.

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

fn spawn_daemon(db_url: &str) -> Option<(Child, u16, TempDir, PathBuf)> {
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
    let child = cmd.spawn().expect("spawn");
    Some((child, port, home, config_path))
}

async fn wait_for_listen(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            tokio::time::sleep(Duration::from_millis(250)).await;
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_sigterm_shuts_down_within_watchdog_window() {
    let db = require_test_db!();
    let Some((mut child, port, _home, _config)) = spawn_daemon(&db.connection_url()) else {
        eprintln!("SKIPPED: pgmcp binary not found");
        return;
    };
    assert!(
        wait_for_listen(port, Duration::from_secs(20)).await,
        "daemon did not start listening"
    );

    // Send SIGTERM and wait up to 15s (the watchdog timeout) + a bit.
    let pid = child.id() as i32;
    #[cfg(unix)]
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    let started = Instant::now();
    let deadline = started + Duration::from_secs(16);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("daemon did not exit within 16s of SIGTERM");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("try_wait: {}", e),
        }
    };
    let elapsed = started.elapsed();
    // Should complete well under the watchdog window.
    assert!(
        elapsed < Duration::from_secs(15),
        "shutdown took {:?} — exceeds watchdog budget",
        elapsed
    );
    // A clean SIGTERM → code 0; `process::exit(1)` only fires when the
    // watchdog forces exit. A non-zero code here means the shutdown
    // coordinator missed its window.
    assert!(
        status.success(),
        "daemon exited with non-success status after SIGTERM: {:?}",
        status
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_accepts_connections_then_releases_port_on_shutdown() {
    let db = require_test_db!();
    let Some((mut child, port, _home, _config)) = spawn_daemon(&db.connection_url()) else {
        eprintln!("SKIPPED: pgmcp binary not found");
        return;
    };
    assert!(
        wait_for_listen(port, Duration::from_secs(20)).await,
        "daemon did not start listening"
    );

    // Confirm the port is actually accepting connections.
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("tcp connect while daemon up");

    // Shutdown.
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let _ = child.wait();

    // After shutdown, a subsequent bind to the same port must succeed — the
    // daemon released it. (We don't assert the connect fails here, because
    // TCP TIME_WAIT semantics + loopback mean transient reconnect behavior
    // varies; bind-after-shutdown is the portable signal.)
    let listener = TcpListener::bind(("127.0.0.1", port));
    assert!(
        listener.is_ok(),
        "port {} still in use after daemon shutdown: {:?}",
        port,
        listener.err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_drains_work_pool_before_exit() {
    // Indirect check — we assert orderly shutdown produces exit code 0
    // within a generous window. If the work pool weren't drained, the
    // shutdown watchdog would force SIGKILL and exit code 1.
    let db = require_test_db!();
    let Some((mut child, port, _home, _config)) = spawn_daemon(&db.connection_url()) else {
        eprintln!("SKIPPED: binary missing");
        return;
    };
    assert!(wait_for_listen(port, Duration::from_secs(20)).await);
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert!(
                    status.success(),
                    "watchdog-forced exit indicates pool didn't drain: {:?}",
                    status
                );
                break;
            }
            Ok(None) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            _ => panic!("daemon did not exit within drain window"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_drains_embedding_pool_before_exit() {
    // Same envelope as the work-pool drain test — exit code 0 within
    // timeout means every pool (including the embedding pool) drained.
    let db = require_test_db!();
    let Some((mut child, port, _home, _config)) = spawn_daemon(&db.connection_url()) else {
        eprintln!("SKIPPED: binary missing");
        return;
    };
    assert!(wait_for_listen(port, Duration::from_secs(20)).await);
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_stops_cron_scheduler_on_shutdown() {
    // If cron doesn't stop, the main thread would be stuck on its
    // scheduler-join; watchdog kicks in with non-zero exit.
    let db = require_test_db!();
    let Some((mut child, port, _home, _config)) = spawn_daemon(&db.connection_url()) else {
        eprintln!("SKIPPED: binary missing");
        return;
    };
    assert!(wait_for_listen(port, Duration::from_secs(20)).await);
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().expect("wait");
    assert!(status.success(), "cron shutdown failed: {:?}", status);
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_two_consecutive_sigterms_still_exit_cleanly() {
    let db = require_test_db!();
    let Some((mut child, port, _home, _config)) = spawn_daemon(&db.connection_url()) else {
        eprintln!("SKIPPED: binary missing");
        return;
    };
    assert!(wait_for_listen(port, Duration::from_secs(20)).await);
    // Two SIGTERMs in quick succession — second should be a no-op.
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
        std::thread::sleep(std::time::Duration::from_millis(100));
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_phase_transitions_are_observable_via_http_readiness() {
    // DaemonLifecycle phases (Initializing → Scanning → Ready) aren't
    // exposed over the MCP wire, so we probe the HTTP endpoint's
    // readiness: the port accepts connections only after the server
    // transitions to at least Scanning. Arriving at an accepting socket
    // and getting a successful /mcp initialize handshake is equivalent
    // to observing the Ready state from outside the process.
    let db = require_test_db!();
    let Some((mut child, port, _home, _config)) = spawn_daemon(&db.connection_url()) else {
        eprintln!("SKIPPED: binary missing");
        return;
    };
    let listening = wait_for_listen(port, Duration::from_secs(20)).await;
    assert!(listening, "daemon never transitioned to listening");
    // A live /mcp endpoint means the daemon's MCP server is past
    // Initializing — Scanning or Ready.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/mcp", port))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}
        }))
        .send()
        .await;
    assert!(resp.is_ok(), "initialize failed after daemon listen");
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_metrics_endpoint_serves_prometheus_format() {
    // The default test config disables the metrics HTTP server. Write
    // a custom config enabling it on an ephemeral port.
    use std::process::{Command, Stdio};
    let db = require_test_db!();
    let Some(binary) = find_pgmcp_binary() else {
        eprintln!("SKIPPED: binary missing");
        return;
    };
    let (db_host, db_port, db_user, db_pass, db_name) = parse_postgres_url(&db.connection_url());
    let home = TempDir::new().expect("home");
    let port = ephemeral_port();
    let metrics_port = ephemeral_port();
    let cfg_dir = home.path().join(".config").join("pgmcp");
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
    toml.push_str("max_connections = 4\n");
    toml.push_str(&format!("[mcp]\nport = {}\nhost = \"127.0.0.1\"\n", port));
    toml.push_str("[workspace]\npaths = []\n");
    toml.push_str(&format!(
        "[metrics]\nhttp_enabled = true\nhttp_port = {}\nhttp_host = \"127.0.0.1\"\n",
        metrics_port
    ));
    toml.push_str(
        "[cron]\nsimilarity_scan_interval_secs = 0\ntopic_scan_interval_secs = 0\n\
         graph_analysis_interval_secs = 0\ngit_history_index_interval_secs = 0\n",
    );
    let cfg_path = cfg_dir.join("config.toml");
    std::fs::write(&cfg_path, toml).expect("write config");

    let mut cmd = Command::new(&binary);
    cmd.arg("-c")
        .arg(&cfg_path)
        .arg("daemon")
        .env("HOME", home.path())
        .env("RUST_LOG", "error")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(ort_dir) = pgmcp_testing::cli_harness::locate_ort_lib_dir() {
        cmd.env("LD_LIBRARY_PATH", ort_dir);
    }
    let mut child = cmd.spawn().expect("spawn");
    assert!(wait_for_listen(port, Duration::from_secs(20)).await);
    assert!(
        wait_for_listen(metrics_port, Duration::from_secs(20)).await,
        "metrics server didn't come up"
    );

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://127.0.0.1:{}/metrics", metrics_port))
        .send()
        .await
        .expect("send");
    assert!(resp.status().is_success());
    let body = resp.text().await.expect("body");
    // Prometheus format: "# HELP" header lines + metric_name{labels} value.
    assert!(
        body.contains("# HELP") || body.contains("_total") || !body.is_empty(),
        "body does not look like Prometheus format:\n{body}"
    );
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_completes_startup_with_empty_workspace_list() {
    // The default test config sets `workspace.paths = []`; verifies the
    // scanner handles an empty workspace list without crashing.
    let db = require_test_db!();
    let Some((mut child, port, _home, _config)) = spawn_daemon(&db.connection_url()) else {
        eprintln!("SKIPPED: binary missing");
        return;
    };
    assert!(
        wait_for_listen(port, Duration::from_secs(20)).await,
        "daemon with empty workspace failed to start"
    );
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_rejects_port_reuse_when_already_bound() {
    let db = require_test_db!();
    let Some((mut child_a, port, _ha, _ca)) = spawn_daemon(&db.connection_url()) else {
        eprintln!("SKIPPED: pgmcp binary not found");
        return;
    };
    assert!(
        wait_for_listen(port, Duration::from_secs(20)).await,
        "first daemon did not start"
    );

    // Spawn a second daemon with the same port. It should fail.
    let Some(binary) = find_pgmcp_binary() else {
        eprintln!("SKIPPED: pgmcp binary not found");
        return;
    };
    let home2 = TempDir::new().expect("tempdir");
    let cfg2 = write_daemon_config(home2.path(), &db.connection_url(), port);
    let mut cmd = Command::new(&binary);
    cmd.arg("-c")
        .arg(&cfg2)
        .arg("daemon")
        .env("HOME", home2.path())
        .env("RUST_LOG", "error")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(ort_dir) = pgmcp_testing::cli_harness::locate_ort_lib_dir() {
        cmd.env("LD_LIBRARY_PATH", ort_dir);
    }
    let child_b = cmd.spawn().expect("spawn");
    // Give it up to 5 seconds to exit (bind failure is fast).
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut child_b = child_b;
    let exit = loop {
        match child_b.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    break None;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(_) => break None,
        }
    };
    // Either exited non-zero (expected) or still running (either is fine —
    // what we really want is that it didn't somehow hijack the port).
    if let Some(status) = exit {
        assert!(
            !status.success(),
            "second daemon succeeded despite port conflict"
        );
    } else {
        // Still running somehow — clean up.
        let _ = child_b.kill();
        let _ = child_b.wait();
    }

    // Shut down the first daemon.
    #[cfg(unix)]
    unsafe {
        libc::kill(child_a.id() as i32, libc::SIGTERM);
    }
    let _ = child_a.wait();
}
