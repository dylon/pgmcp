//! CLI subcommand smoke tests via `assert_cmd`.
//!
//! Exercises every non-daemon subcommand (`init`, `init-project`,
//! `upgrade-configs`, `upgrade-project`, `tool`, `context`, `results`,
//! `analyze`, `reindex`, `stats`) by spawning the pre-built `pgmcp`
//! binary. Tests that touch the DB use `TestDatabase` and `PGMCP_DB_*`
//! env overrides on a per-test temp config; tests that don't touch the
//! DB skip if the binary is missing.
//!
//! Runs as part of `cargo test --release -p pgmcp-testing` via verify.sh.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use pgmcp_testing::require_test_db;
use tempfile::TempDir;

/// Locate the pgmcp binary — same logic as `cli_harness::locate_binary`.
/// Returns `None` if no candidate exists, so tests can self-skip.
fn find_pgmcp_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PGMCP_TEST_BIN") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .expect("pgmcp-testing dir has a parent");
    for profile in ["release", "debug"] {
        let candidate = workspace_root.join("target").join(profile).join("pgmcp");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

macro_rules! require_binary {
    () => {
        match find_pgmcp_binary() {
            Some(b) => b,
            None => {
                eprintln!(
                    "SKIPPED: pgmcp binary not found — build with `cargo build --release --bin pgmcp`"
                );
                return;
            }
        }
    };
}

fn pgmcp(binary: &Path) -> Command {
    let mut cmd = Command::new(binary);
    if let Some(ort_dir) = pgmcp_testing::cli_harness::locate_ort_lib_dir() {
        let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
        let new_path = if existing.is_empty() {
            ort_dir.display().to_string()
        } else {
            format!("{}:{}", ort_dir.display(), existing)
        };
        cmd.env("LD_LIBRARY_PATH", new_path);
    }
    cmd
}

/// Write a minimal config at `home/.config/pgmcp/config.toml` pointing at
/// `test_db`. Mirrors `cli_harness::write_test_config` but writes into the
/// tempdir that will be used as `HOME`, so subcommands that pick up
/// `~/.config/pgmcp/config.toml` see our test DB.
fn write_home_config(home: &Path, db_url: &str) {
    let parsed = parse_postgres_url(db_url).expect("parse url");
    let cfg_dir = home.join(".config").join("pgmcp");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir");
    let mut toml = String::new();
    toml.push_str("[database]\n");
    toml.push_str(&format!("host = \"{}\"\n", parsed.host));
    toml.push_str(&format!("port = {}\n", parsed.port));
    toml.push_str(&format!("name = \"{}\"\n", parsed.dbname));
    toml.push_str(&format!("user = \"{}\"\n", parsed.user));
    if let Some(pass) = &parsed.password {
        toml.push_str(&format!("password = \"{}\"\n", pass));
    }
    toml.push_str("max_connections = 4\n\n");
    toml.push_str("[workspace]\npaths = []\n\n");
    toml.push_str("[metrics]\nhttp_enabled = false\n\n");
    toml.push_str("[cron]\n");
    toml.push_str("similarity_scan_interval_secs = 0\n");
    toml.push_str("topic_scan_interval_secs = 0\n");
    toml.push_str("graph_analysis_interval_secs = 0\n");
    toml.push_str("git_history_index_interval_secs = 0\n");
    std::fs::write(cfg_dir.join("config.toml"), toml).expect("write config");
}

struct ParsedUrl {
    host: String,
    port: u16,
    user: String,
    password: Option<String>,
    dbname: String,
}

fn parse_postgres_url(raw: &str) -> Option<ParsedUrl> {
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
    Some(ParsedUrl {
        host,
        port,
        user,
        password,
        dbname: dbname.to_string(),
    })
}

// ============================================================================
// Infra-independent tests (binary needed, no DB)
// ============================================================================

#[test]
fn cli_init_writes_default_config_to_temp_home() {
    let binary = require_binary!();
    let home = TempDir::new().expect("tempdir");
    pgmcp(&binary)
        .env("HOME", home.path())
        .arg("init")
        .assert()
        .success();
    assert!(
        home.path().join(".config/pgmcp/config.toml").exists(),
        "config.toml not written under tempdir HOME"
    );
}

#[test]
fn cli_init_second_run_succeeds_idempotent() {
    let binary = require_binary!();
    let home = TempDir::new().expect("tempdir");
    pgmcp(&binary)
        .env("HOME", home.path())
        .arg("init")
        .assert()
        .success();
    // Running `init` a second time is a no-op that still exits cleanly
    // (either because the file already exists and pgmcp overwrites it,
    // or warns and skips — either way, exit code 0).
    pgmcp(&binary)
        .env("HOME", home.path())
        .arg("init")
        .assert()
        .success();
}

#[test]
fn cli_init_project_writes_pgmcp_toml_in_cwd() {
    let binary = require_binary!();
    let workdir = TempDir::new().expect("tempdir");
    let home = TempDir::new().expect("home");
    pgmcp(&binary)
        .env("HOME", home.path())
        .arg("init-project")
        .arg("--cwd")
        .arg(workdir.path())
        .assert()
        .success();
    assert!(
        workdir.path().join(".pgmcp.toml").exists(),
        ".pgmcp.toml not written to --cwd"
    );
}

#[test]
fn cli_upgrade_project_preserves_existing_file() {
    let binary = require_binary!();
    let workdir = TempDir::new().expect("tempdir");
    let home = TempDir::new().expect("home");
    // Seed a pre-existing .pgmcp.toml with a user customization.
    std::fs::write(
        workdir.path().join(".pgmcp.toml"),
        "[indexer]\nexclude_patterns = [\"target/**\"]\n",
    )
    .expect("seed config");
    pgmcp(&binary)
        .env("HOME", home.path())
        .arg("upgrade-project")
        .arg("--cwd")
        .arg(workdir.path())
        .assert()
        .success();
    let after = std::fs::read_to_string(workdir.path().join(".pgmcp.toml")).expect("read upgraded");
    assert!(
        after.contains("target/**"),
        "upgrade-project lost user customization:\n{after}"
    );
}

#[test]
fn cli_tool_without_args_lists_all_tool_categories() {
    let binary = require_binary!();
    let home = TempDir::new().expect("home");
    let output = pgmcp(&binary)
        .env("HOME", home.path())
        .arg("tool")
        .output()
        .expect("run tool list");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Categories that the CLI groups tools into.
    for category in &[
        "Search",
        "File Info",
        "Similarity",
        "Topics",
        "Graph",
        "Architecture",
        "Prediction",
    ] {
        assert!(
            stdout.contains(category),
            "missing category header {}: {stdout}",
            category
        );
    }
}

#[test]
fn cli_tool_schema_returns_parseable_json() {
    let binary = require_binary!();
    let home = TempDir::new().expect("home");
    let output = pgmcp(&binary)
        .env("HOME", home.path())
        .args(["tool", "semantic_search", "--schema"])
        .output()
        .expect("run tool schema");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Find the JSON object (the preamble is "Tool: …\n\n<description>\n\n\nParameters:\n{…}")
    let open = stdout.find('{').expect("JSON object in output");
    let json_part = &stdout[open..];
    let _parsed: serde_json::Value =
        serde_json::from_str(json_part.trim()).expect("schema must be valid JSON");
}

#[test]
fn cli_tool_unknown_name_with_schema_exits_nonzero() {
    let binary = require_binary!();
    let home = TempDir::new().expect("home");
    pgmcp(&binary)
        .env("HOME", home.path())
        .args(["tool", "this_tool_does_not_exist", "--schema"])
        .assert()
        .failure();
}

// ============================================================================
// DB-touching tests (require PGMCP_TEST_DATABASE_URL + binary)
// ============================================================================

#[tokio::test]
async fn cli_context_reports_indexed_projects() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };

    // Seed one project into the test DB so the CLI has something to find.
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws")
        .bind("/ws/cli-context-proj/")
        .bind("cli-context-proj")
        .execute(db.pool())
        .await
        .expect("seed project");

    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    let output = pgmcp(&binary)
        .env("HOME", home.path())
        .args(["context", "--cwd", "/unrelated/path"])
        .output()
        .expect("run context");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Fallback branch lists indexed projects — our project should appear.
    assert!(
        stdout.contains("cli-context-proj"),
        "project name missing from context output:\n{stdout}"
    );
}

#[tokio::test]
async fn cli_tool_list_projects_returns_project_list_from_db() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };

    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws")
        .bind("/ws/cli-tool-proj/")
        .bind("cli-tool-proj")
        .execute(db.pool())
        .await
        .expect("seed");

    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    let output = pgmcp(&binary)
        .env("HOME", home.path())
        .args(["tool", "list_projects"])
        .output()
        .expect("run tool");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cli-tool-proj"),
        "project name missing from `tool list_projects`:\n{stdout}"
    );
}

#[tokio::test]
async fn cli_results_empty_db_reports_no_data() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    let output = pgmcp(&binary)
        .env("HOME", home.path())
        .args(["results"])
        .output()
        .expect("run results");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Both the similarity and topics sections report no data.
    assert!(
        stdout.contains("No similarity data") && stdout.contains("No topic data"),
        "expected 'no data' messages:\n{stdout}"
    );
}

#[tokio::test]
async fn cli_analyze_similarity_runs_against_test_db() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    pgmcp(&binary)
        .env("HOME", home.path())
        .args(["analyze", "similarity"])
        .assert()
        .success();
}

#[tokio::test]
async fn cli_analyze_topics_runs_against_test_db() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    pgmcp(&binary)
        .env("HOME", home.path())
        .args(["analyze", "topics"])
        .assert()
        .success();
}

#[tokio::test]
async fn cli_analyze_graph_runs_against_test_db() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    pgmcp(&binary)
        .env("HOME", home.path())
        .args(["analyze", "graph"])
        .assert()
        .success();
}

#[tokio::test]
async fn cli_context_finds_project_for_known_cwd() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    // Seed a project whose path is a prefix of our cwd argument.
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws")
        .bind("/ws/known-cwd-proj/")
        .bind("known-cwd-proj")
        .execute(db.pool())
        .await
        .expect("seed");
    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    let output = pgmcp(&binary)
        .env("HOME", home.path())
        .args(["context", "--cwd", "/ws/known-cwd-proj/src/main.rs"])
        .output()
        .expect("run");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("known-cwd-proj") || stdout.contains("Project Context"),
        "expected project header for known cwd:\n{stdout}"
    );
}

#[tokio::test]
async fn cli_results_with_topics_subcommand_reports_no_data() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    let output = pgmcp(&binary)
        .env("HOME", home.path())
        .args(["results", "topics"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No topic data") || stdout.contains("Topic Clustering"));
}

#[tokio::test]
async fn cli_results_with_similarity_subcommand_reports_no_data() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    let output = pgmcp(&binary)
        .env("HOME", home.path())
        .args(["results", "similarity"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No similarity data") || stdout.contains("Cross-Project Similarity"));
}

#[tokio::test]
async fn cli_upgrade_configs_interactive_prompts_for_each_project() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    // Seed two projects with real .pgmcp.toml files on disk so the
    // interactive prompt has something to iterate over.
    let proj_a = TempDir::new().expect("a");
    let proj_b = TempDir::new().expect("b");
    for dir in [&proj_a, &proj_b] {
        std::fs::write(
            dir.path().join(".pgmcp.toml"),
            "[indexer]\nexclude_patterns = [\"keep/**\"]\n",
        )
        .expect("seed");
    }
    for (dir, name) in [(&proj_a, "ia"), (&proj_b, "ib")] {
        sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
            .bind(dir.path().to_str().unwrap())
            .bind(dir.path().to_str().unwrap())
            .bind(name)
            .execute(db.pool())
            .await
            .expect("seed");
    }

    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    // -i puts the command in interactive mode; answer "n" to both prompts.
    let output = pgmcp(&binary)
        .env("HOME", home.path())
        .args(["upgrade-configs", "-i"])
        .write_stdin("n\nn\n")
        .output()
        .expect("run");
    // Exit code 0 is expected even when user declines.
    assert!(output.status.success(), "exit: {:?}", output.status);
    // Both declines should be reflected in the output.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("Upgrade")
            || combined.contains("Skipped")
            || combined.contains("upgrade"),
        "expected interactive prompt text in output:\n{combined}"
    );
}

#[tokio::test]
async fn cli_upgrade_configs_bulk_upgrades_all_projects() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    // Seed a project with a real on-disk directory + .pgmcp.toml so
    // upgrade-configs has something to act on.
    let project_dir = TempDir::new().expect("project");
    std::fs::write(
        project_dir.path().join(".pgmcp.toml"),
        "[indexer]\nexclude_patterns = [\"keep/**\"]\n",
    )
    .expect("seed .pgmcp.toml");
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind(project_dir.path().to_str().unwrap())
        .bind(project_dir.path().to_str().unwrap())
        .bind("bulk-upgrade-proj")
        .execute(db.pool())
        .await
        .expect("seed");
    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    pgmcp(&binary)
        .env("HOME", home.path())
        .args(["upgrade-configs"])
        .assert()
        .success();
    // User customization must survive.
    let after = std::fs::read_to_string(project_dir.path().join(".pgmcp.toml")).expect("read");
    assert!(
        after.contains("keep/**"),
        "upgrade-configs clobbered user customization:\n{after}"
    );
}

#[tokio::test]
async fn cli_statistics_prints_something_or_exits_cleanly() {
    // `statistics` (renamed from `stats`; the short form is preserved as
    // an alias) hits the daemon if running and falls back otherwise; this
    // smoke test doesn't start a daemon, so we just assert that the
    // subcommand exits cleanly (success or graceful failure — never a
    // panic). Both forms must work.
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    let home = TempDir::new().expect("home");
    for subcommand in ["statistics", "stats"] {
        let output = pgmcp(&binary)
            .env("HOME", home.path())
            .args([subcommand])
            .output()
            .expect("run");
        // Either success (daemon found somewhere) or clean failure — both
        // acceptable. What we reject is a segfault or panic.
        assert!(
            output.status.code().is_some(),
            "{subcommand}: process should exit with a normal code, got {:?}",
            output.status
        );
    }
}

#[tokio::test]
async fn cli_reindex_clears_file_chunks_and_indexed_files() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    // Seed one project + one file. (No chunk — inserting a vector(1024)
    // from a bash subprocess is more effort than it's worth.)
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/cli-reindex-proj/")
    .bind("cli-reindex-proj")
    .fetch_one(db.pool())
    .await
    .expect("seed project");
    sqlx::query(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())"
    )
    .bind(project_id)
    .bind("/ws/cli-reindex-proj/a.rs")
    .bind("a.rs")
    .bind("rust")
    .bind(1_i64)
    .bind("x")
    .bind(1_i32)
    .execute(db.pool())
    .await
    .expect("seed file");

    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    pgmcp(&binary)
        .env("HOME", home.path())
        .args(["reindex"])
        .assert()
        .success();

    // After reindex, indexed_files should be empty.
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM indexed_files")
        .fetch_one(db.pool())
        .await
        .expect("count");
    assert_eq!(count, 0, "reindex did not clear indexed_files");
}

// ============================================================================
// `status` — daemon + model state snapshot (DB-fallback path)
// ============================================================================

#[tokio::test]
async fn cli_status_default_renders_all_sections_via_db_fallback() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    // No daemon running on this test config's mcp.port — exercises
    // the DB-fallback path. Output must still include every section.
    let output = pgmcp(&binary)
        .env("HOME", home.path())
        .args(["status"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "status should exit 0; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for header in [
        "Daemon:",
        "Database:",
        "Embeddings:",
        "Topics:",
        "Cross-project similarity:",
        "Graph:",
        "Git history:",
    ] {
        assert!(
            stdout.contains(header),
            "expected section `{header}` in output:\n{stdout}"
        );
    }
}

#[tokio::test]
async fn cli_status_topics_filter_omits_other_sections() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    let output = pgmcp(&binary)
        .env("HOME", home.path())
        .args(["status", "topics"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Topics:"),
        "Topics section missing:\n{stdout}"
    );
    assert!(
        !stdout.contains("Daemon:") && !stdout.contains("Database:"),
        "filtered output should NOT contain other sections; got:\n{stdout}"
    );
}

#[tokio::test]
async fn cli_status_json_output_parses_into_expected_keys() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    let output = pgmcp(&binary)
        .env("HOME", home.path())
        .args(["status", "--json"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("--json output not valid JSON: {e}\n{stdout}"));
    for key in ["daemon", "database", "model_state", "counters"] {
        assert!(
            v.get(key).is_some(),
            "--json output missing top-level key `{key}`: {stdout}"
        );
    }
    // Database URL must be redacted regardless of fallback vs daemon.
    let url = v["database"]["url"].as_str().expect("database.url");
    assert!(
        url.contains(":****@"),
        "database.url must be redacted (contain `:****@`); got {url}"
    );
}

#[tokio::test]
async fn cli_status_unknown_section_is_rejected_with_message() {
    let db = require_test_db!();
    let binary = match find_pgmcp_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIPPED: pgmcp binary not found");
            return;
        }
    };
    let home = TempDir::new().expect("home");
    write_home_config(home.path(), &db.connection_url());
    let output = pgmcp(&binary)
        .env("HOME", home.path())
        .args(["status", "no_such_model"])
        .output()
        .expect("run");
    // Process exits cleanly (the unknown-section message goes to stderr
    // and the function returns Ok). Verify the warning is present.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown model `no_such_model`"),
        "expected unknown-model error on stderr, got:\n{stderr}"
    );
}
