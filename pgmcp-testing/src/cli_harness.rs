//! Subprocess harness for the `pgmcp` binary.
//!
//! Provides [`PgmcpProcess`] — a managed child process running `pgmcp
//! serve` or `pgmcp daemon` against a test database. The harness takes
//! care of writing a temporary config file, locating the release binary
//! (rebuilding via `cargo build --release --bin pgmcp` if missing),
//! isolating `HOME` under a tempdir, and orderly teardown.
//!
//! The binary location is resolved in this order:
//!
//! 1. `PGMCP_TEST_BIN` env var — absolute path to a pre-built binary.
//! 2. `<workspace_root>/target/release/pgmcp` — the usual location after
//!    `cargo build --release --bin pgmcp` or a `verify.sh` run.
//! 3. `<workspace_root>/target/debug/pgmcp` — debug fallback.
//!
//! When none of the above exist, [`PgmcpProcess::spawn_serve`] /
//! [`spawn_daemon`] returns a `PgmcpSpawnError::BinaryMissing` error so the
//! test can skip with a helpful message.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

use crate::db_harness::TestDatabase;

/// Errors that can happen while starting a subprocess harness. Deliberately
/// distinct from [`crate::db_harness::TestDbUnavailable`] because a missing
/// binary is a hard test failure, not a "skip cleanly" situation — the test
/// has already signaled it needs the binary by calling `spawn_*`.
#[derive(Debug, thiserror::Error)]
pub enum PgmcpSpawnError {
    #[error("pgmcp binary not found at {0} (build with `cargo build --release --bin pgmcp`)")]
    BinaryMissing(PathBuf),
    #[error("failed to write test config {0}: {1}")]
    ConfigWrite(PathBuf, std::io::Error),
    #[error("failed to spawn subprocess: {0}")]
    Spawn(std::io::Error),
    #[error("failed to build temp HOME: {0}")]
    TempDir(std::io::Error),
}

/// Locate the ort-downloaded libonnxruntime.so directory. Mirrors
/// `pgmcp-testing/build.rs::find_ort_lib_dir`. Public so other test files
/// (e.g. `cli_subcommands_smoke.rs`) can mix this into their own spawns.
pub fn locate_ort_lib_dir() -> Option<PathBuf> {
    let cache_root = if let Ok(p) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(p)
    } else {
        PathBuf::from(std::env::var("HOME").ok()?).join(".cache")
    };
    let triple = cache_root
        .join("ort.pyke.io")
        .join("dfbin")
        .join("x86_64-unknown-linux-gnu");
    if !triple.exists() {
        return None;
    }
    for entry in std::fs::read_dir(triple).ok()?.flatten() {
        let lib = entry.path().join("onnxruntime").join("lib");
        if lib.join("libonnxruntime.so").exists() {
            return Some(lib);
        }
    }
    None
}

/// Locate the `pgmcp` binary. Returns [`PgmcpSpawnError::BinaryMissing`] if
/// no candidate exists on disk.
fn locate_binary() -> Result<PathBuf, PgmcpSpawnError> {
    if let Ok(path) = std::env::var("PGMCP_TEST_BIN") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
        return Err(PgmcpSpawnError::BinaryMissing(p));
    }
    // pgmcp-testing/Cargo.toml lives at <workspace>/pgmcp-testing; target is
    // at <workspace>/target.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .expect("pgmcp-testing dir has a parent");
    for profile in ["release", "debug"] {
        let candidate = workspace_root.join("target").join(profile).join("pgmcp");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(PgmcpSpawnError::BinaryMissing(
        workspace_root.join("target/release/pgmcp"),
    ))
}

/// Write a minimal `config.toml` with `[database]` pointing at the test DB.
/// All other config sections rely on `#[serde(default)]` — the spawned
/// pgmcp accepts the partial config and fills in defaults.
fn write_test_config(config_path: &Path, test_db_url: &str) -> Result<(), PgmcpSpawnError> {
    let parsed = parse_postgres_url(test_db_url)
        .unwrap_or_else(|| ParsedPgUrl::default_for_tests(test_db_url));
    let mut toml = String::new();
    toml.push_str("# pgmcp test config — written by pgmcp_testing::cli_harness\n\n");
    toml.push_str("[database]\n");
    toml.push_str(&format!("host = \"{}\"\n", parsed.host));
    toml.push_str(&format!("port = {}\n", parsed.port));
    toml.push_str(&format!("name = \"{}\"\n", parsed.dbname));
    toml.push_str(&format!("user = \"{}\"\n", parsed.user));
    if let Some(pass) = &parsed.password {
        toml.push_str(&format!("password = \"{}\"\n", pass));
    }
    toml.push_str("max_connections = 4\n\n");
    // Keep workspace paths empty so the indexer does nothing (tests drive
    // the DB directly; no real filesystem scan needed).
    toml.push_str("[workspace]\n");
    toml.push_str("paths = []\n\n");
    // Disable metrics HTTP server by default — tests that want it set
    // their own flag.
    toml.push_str("[metrics]\n");
    toml.push_str("http_enabled = false\n\n");
    // Turn off all cron jobs by default — tests that need them override.
    toml.push_str("[cron]\n");
    toml.push_str("similarity_scan_interval_secs = 0\n");
    toml.push_str("topic_scan_interval_secs = 0\n");
    toml.push_str("graph_analysis_interval_secs = 0\n");
    toml.push_str("git_history_index_interval_secs = 0\n");
    std::fs::write(config_path, toml)
        .map_err(|e| PgmcpSpawnError::ConfigWrite(config_path.to_path_buf(), e))
}

/// Minimal Postgres URL parser. We don't want a `url` crate dep for a
/// three-field string split.
#[derive(Debug, Clone)]
struct ParsedPgUrl {
    host: String,
    port: u16,
    user: String,
    password: Option<String>,
    dbname: String,
}

impl ParsedPgUrl {
    fn default_for_tests(raw: &str) -> Self {
        // Fallback: `postgres://…` URL that couldn't be parsed. Use
        // localhost defaults and let Postgres reject the bad setup.
        Self {
            host: "localhost".into(),
            port: 5432,
            user: "postgres".into(),
            password: None,
            dbname: raw.rsplit('/').next().unwrap_or("postgres").to_string(),
        }
    }
}

fn parse_postgres_url(raw: &str) -> Option<ParsedPgUrl> {
    // Accepts: postgres://[user[:password]@]host[:port]/dbname
    let without_scheme = raw.strip_prefix("postgres://")?;
    let (userinfo_host, dbname) = without_scheme.split_once('/')?;
    let (user, password, host_port) = match userinfo_host.rsplit_once('@') {
        Some((userinfo, host_port)) => {
            let (u, p) = match userinfo.split_once(':') {
                Some((u, p)) => (u.to_string(), Some(p.to_string())),
                None => (userinfo.to_string(), None),
            };
            (u, p, host_port.to_string())
        }
        None => ("postgres".to_string(), None, userinfo_host.to_string()),
    };
    let (host, port) = match host_port.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (host_port, 5432),
    };
    Some(ParsedPgUrl {
        host,
        port,
        user,
        password,
        dbname: dbname.to_string(),
    })
}

/// A live `pgmcp serve` subprocess speaking JSON-RPC over stdio. On
/// `Drop`, sends SIGKILL and waits for exit — tests that want a graceful
/// SIGTERM shutdown should call [`PgmcpProcess::shutdown`] explicitly.
pub struct PgmcpProcess {
    /// `None` after [`shutdown`] — preserved for fallback `Drop`.
    child: Option<Child>,
    /// Stdin for JSON-RPC requests. `None` if the caller took it via
    /// [`take_stdio`].
    stdin: Option<ChildStdin>,
    /// Stdout reader for JSON-RPC responses.
    stdout: Option<BufReader<ChildStdout>>,
    /// Held so the temp HOME (and its config) outlive the subprocess.
    _home: TempDir,
    /// Held so the temp config dir outlives the subprocess.
    _config_dir: TempDir,
}

impl PgmcpProcess {
    /// Spawn `pgmcp serve` (stdio transport) against `test_db`. Waits up
    /// to 10s for the subprocess to accept input. Returns the handle the
    /// test uses to exchange JSON-RPC messages.
    pub fn spawn_serve(test_db: &TestDatabase) -> Result<Self, PgmcpSpawnError> {
        Self::spawn_subcommand(test_db, "serve")
    }

    /// Spawn `pgmcp daemon` (Streamable HTTP transport). The caller is
    /// responsible for knowing/probing the bind port configured in
    /// the default config (`127.0.0.1:3100`) or via a custom config
    /// passed through a later harness extension.
    pub fn spawn_daemon(test_db: &TestDatabase) -> Result<Self, PgmcpSpawnError> {
        Self::spawn_subcommand(test_db, "daemon")
    }

    fn spawn_subcommand(test_db: &TestDatabase, sub: &str) -> Result<Self, PgmcpSpawnError> {
        let binary = locate_binary()?;
        let home = tempfile::tempdir().map_err(PgmcpSpawnError::TempDir)?;
        let config_dir = tempfile::tempdir().map_err(PgmcpSpawnError::TempDir)?;
        let config_path = config_dir.path().join("config.toml");
        write_test_config(&config_path, &test_db.connection_url())?;

        let mut cmd = Command::new(&binary);
        cmd.arg("-c")
            .arg(&config_path)
            .arg(sub)
            .env("HOME", home.path())
            // Force a quiet stderr so test output stays focused on
            // JSON-RPC framing.
            .env("RUST_LOG", "error")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // The main pgmcp binary has no rpath entry for the ort-downloaded
        // libonnxruntime.so (it lives under ~/.cache/ort.pyke.io/…). Cargo
        // normally sets LD_LIBRARY_PATH at test time from rustc-link-search,
        // but that's fragile across test harnesses. Inject it explicitly so
        // subprocess tests don't depend on cargo's env plumbing.
        if let Some(ort_dir) = locate_ort_lib_dir() {
            let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
            let new_path = if existing.is_empty() {
                ort_dir.display().to_string()
            } else {
                format!("{}:{}", ort_dir.display(), existing)
            };
            cmd.env("LD_LIBRARY_PATH", new_path);
        }

        let mut child = cmd.spawn().map_err(PgmcpSpawnError::Spawn)?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));

        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout: Some(stdout),
            _home: home,
            _config_dir: config_dir,
        })
    }

    /// Stdin for writing JSON-RPC requests.
    pub fn stdin(&mut self) -> &mut ChildStdin {
        self.stdin.as_mut().expect("stdin still owned")
    }

    /// Stdout reader for parsing JSON-RPC responses line-by-line.
    pub fn stdout(&mut self) -> &mut BufReader<ChildStdout> {
        self.stdout.as_mut().expect("stdout still owned")
    }

    /// Take both stdin and stdout — useful when the test wants to move
    /// them into helper functions.
    pub fn take_stdio(&mut self) -> (ChildStdin, BufReader<ChildStdout>) {
        (
            self.stdin.take().expect("stdin not taken"),
            self.stdout.take().expect("stdout not taken"),
        )
    }

    /// Read the next line of stdout. Returns `None` on EOF.
    pub fn read_line(&mut self) -> Option<String> {
        let mut line = String::new();
        let reader = self.stdout.as_mut().expect("stdout still owned");
        match reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(line.trim_end().to_string()),
            Err(e) => panic!("PgmcpProcess::read_line: {}", e),
        }
    }

    /// Graceful shutdown: close stdin (signals EOF to stdio-transport
    /// servers), wait up to `timeout`, then SIGKILL on timeout.
    pub fn shutdown(mut self, timeout: Duration) -> std::io::Result<()> {
        // Closing stdin signals EOF, which rmcp's stdio transport treats
        // as "client disconnected → shut down".
        self.stdin.take();
        self.stdout.take();
        let mut child = self.child.take().expect("child alive");
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        child.kill()?;
                        let _ = child.wait();
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for PgmcpProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_url() {
        let p =
            parse_postgres_url("postgres://alice:s3cret@db.example.com:5433/mydb").expect("parses");
        assert_eq!(p.host, "db.example.com");
        assert_eq!(p.port, 5433);
        assert_eq!(p.user, "alice");
        assert_eq!(p.password.as_deref(), Some("s3cret"));
        assert_eq!(p.dbname, "mydb");
    }

    #[test]
    fn parse_url_without_password() {
        let p = parse_postgres_url("postgres://alice@localhost/mydb").expect("parses");
        assert_eq!(p.user, "alice");
        assert!(p.password.is_none());
        assert_eq!(p.port, 5432);
    }

    #[test]
    fn parse_url_without_userinfo() {
        let p = parse_postgres_url("postgres://localhost:5432/mydb").expect("parses");
        assert_eq!(p.user, "postgres");
        assert_eq!(p.dbname, "mydb");
    }

    #[test]
    fn parse_url_rejects_non_postgres_scheme() {
        assert!(parse_postgres_url("mysql://u@h/db").is_none());
    }

    #[test]
    fn parse_url_rejects_missing_dbname() {
        assert!(parse_postgres_url("postgres://u@h:5432").is_none());
    }
}
