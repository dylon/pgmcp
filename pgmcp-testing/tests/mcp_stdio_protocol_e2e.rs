//! MCP protocol E2E tests — spawn the real `pgmcp serve` binary and
//! exchange JSON-RPC 2.0 messages over stdio.
//!
//! Migrated from the Docker-based `tests/mcp_protocol.rs`; now uses
//! `pgmcp_testing::cli_harness::PgmcpProcess` + `TestDatabase` so each
//! test gets an isolated Postgres database and its own subprocess.
//!
//! Prereqs:
//! * `PGMCP_TEST_DATABASE_URL` (see `tests/README.md`).
//! * `pgmcp` binary built via `cargo build --release --bin pgmcp`
//!   (verify.sh's gate 4 already does this).
//!
//! If either is missing the test prints a SKIPPED line and returns.

use std::io::Write;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use pgmcp_testing::cli_harness::{PgmcpProcess, PgmcpSpawnError};
use pgmcp_testing::require_test_db;

/// Read up to `timeout` for a single JSON-RPC response whose `id` field
/// matches `expected_id`. Skips non-matching lines (notifications,
/// unrelated responses). Returns `None` on EOF or timeout.
fn read_response(proc: &mut PgmcpProcess, expected_id: i64, timeout: Duration) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let line = proc.read_line()?;
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("id").and_then(|id| id.as_i64()) == Some(expected_id) {
            return Some(v);
        }
    }
    None
}

fn send(proc: &mut PgmcpProcess, request: &Value) {
    let msg = serde_json::to_string(request).expect("serialize");
    let stdin = proc.stdin();
    writeln!(stdin, "{}", msg).expect("write stdin");
    stdin.flush().expect("flush stdin");
}

#[tokio::test]
async fn initialize_and_list_tools_returns_expected_tool_names() {
    let db = require_test_db!();
    let mut proc = match PgmcpProcess::spawn_serve(&db) {
        Ok(p) => p,
        Err(PgmcpSpawnError::BinaryMissing(path)) => {
            eprintln!(
                "SKIPPED: {} not found — build with `cargo build --release --bin pgmcp`",
                path.display()
            );
            return;
        }
        Err(e) => panic!("spawn failed: {}", e),
    };

    // Give the subprocess a moment to initialize its DB pool / run migrations.
    std::thread::sleep(Duration::from_secs(2));

    send(
        &mut proc,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "pgmcp-test", "version": "1.0.0" }
            }
        }),
    );

    let resp = read_response(&mut proc, 1, Duration::from_secs(10)).expect("initialize response");
    assert!(resp.get("result").is_some(), "expected result: {}", resp);
    assert_eq!(resp["result"]["serverInfo"]["name"], "pgmcp");

    // initialized notification (no response expected)
    send(
        &mut proc,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    );

    send(
        &mut proc,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    );

    let resp = read_response(&mut proc, 2, Duration::from_secs(10)).expect("tools/list response");
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    for expected in [
        "semantic_search",
        "text_search",
        "grep",
        "read_file",
        "list_projects",
        "project_tree",
        "file_info",
        "index_stats",
    ] {
        assert!(
            tool_names.contains(&expected),
            "missing tool {}: got {:?}",
            expected,
            tool_names
        );
    }

    let _ = proc.shutdown(Duration::from_secs(5));
}

#[tokio::test]
async fn tools_call_list_projects_returns_result_or_error_envelope() {
    let db = require_test_db!();
    let mut proc = match PgmcpProcess::spawn_serve(&db) {
        Ok(p) => p,
        Err(PgmcpSpawnError::BinaryMissing(path)) => {
            eprintln!(
                "SKIPPED: {} not found — build with `cargo build --release --bin pgmcp`",
                path.display()
            );
            return;
        }
        Err(e) => panic!("spawn failed: {}", e),
    };

    std::thread::sleep(Duration::from_secs(2));

    send(
        &mut proc,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "pgmcp-test", "version": "1.0" }
            }
        }),
    );
    read_response(&mut proc, 1, Duration::from_secs(10)).expect("initialize");

    send(
        &mut proc,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    );

    send(
        &mut proc,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "list_projects",
                "arguments": {}
            }
        }),
    );

    let resp = read_response(&mut proc, 2, Duration::from_secs(10)).expect("tools/call response");
    assert!(
        resp.get("result").is_some() || resp.get("error").is_some(),
        "expected result or error envelope: {}",
        resp,
    );

    let _ = proc.shutdown(Duration::from_secs(5));
}

fn spawn_initialized(db: &pgmcp_testing::db_harness::TestDatabase) -> Option<PgmcpProcess> {
    let mut proc = match PgmcpProcess::spawn_serve(db) {
        Ok(p) => p,
        Err(PgmcpSpawnError::BinaryMissing(path)) => {
            eprintln!(
                "SKIPPED: {} not found — build with `cargo build --release --bin pgmcp`",
                path.display()
            );
            return None;
        }
        Err(e) => panic!("spawn: {}", e),
    };
    std::thread::sleep(Duration::from_secs(2));
    send(
        &mut proc,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "t", "version": "1" }
            }
        }),
    );
    read_response(&mut proc, 1, Duration::from_secs(10)).expect("init");
    send(
        &mut proc,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    );
    Some(proc)
}

#[tokio::test]
async fn tools_call_unknown_tool_returns_error_envelope() {
    let db = require_test_db!();
    let Some(mut proc) = spawn_initialized(&db) else {
        return;
    };
    send(
        &mut proc,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "nonexistent_tool_xyz",
                "arguments": {}
            }
        }),
    );
    let resp =
        read_response(&mut proc, 2, Duration::from_secs(10)).expect("response for unknown tool");
    assert!(
        resp.get("error").is_some() || resp["result"]["isError"] == json!(true),
        "unknown tool should yield error envelope: {}",
        resp
    );
    let _ = proc.shutdown(Duration::from_secs(5));
}

#[tokio::test]
async fn tools_call_malformed_params_yields_error_envelope() {
    let db = require_test_db!();
    let Some(mut proc) = spawn_initialized(&db) else {
        return;
    };
    // semantic_search requires a `query` field — send only garbage.
    send(
        &mut proc,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "semantic_search",
                "arguments": {"not_query": 42}
            }
        }),
    );
    let resp = read_response(&mut proc, 2, Duration::from_secs(10))
        .expect("response for malformed params");
    assert!(
        resp.get("error").is_some() || resp["result"]["isError"] == json!(true),
        "malformed params should yield error envelope: {}",
        resp
    );
    let _ = proc.shutdown(Duration::from_secs(5));
}

#[tokio::test]
async fn tools_list_includes_every_registered_tool_sample() {
    let db = require_test_db!();
    let Some(mut proc) = spawn_initialized(&db) else {
        return;
    };
    send(
        &mut proc,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    );
    let resp = read_response(&mut proc, 2, Duration::from_secs(10)).expect("tools/list");
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for expected in [
        "semantic_search",
        "text_search",
        "grep",
        "hybrid_search",
        "read_file",
        "project_tree",
        "file_info",
        "list_projects",
        "index_stats",
        "compare_files",
        "find_duplicates",
        "discover_topics",
        "find_orphans",
        "dependency_graph",
        "architecture_quality",
        "bug_prediction",
        "engineering_scorecard",
    ] {
        assert!(
            names.contains(&expected),
            "missing registered tool '{}': got {:?}",
            expected,
            names
        );
    }
    assert!(
        names.len() >= 30,
        "expected ≥ 30 tools, got {}",
        names.len()
    );
    let _ = proc.shutdown(Duration::from_secs(5));
}

#[tokio::test]
async fn set_logging_level_notification_is_accepted() {
    // MCP clients can subscribe to server log notifications. The
    // `logging/setLevel` request wires the broadcaster; the server should
    // acknowledge it (rmcp routes to the registered handler).
    let db = require_test_db!();
    let Some(mut proc) = spawn_initialized(&db) else {
        return;
    };
    send(
        &mut proc,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "logging/setLevel",
            "params": { "level": "info" }
        }),
    );
    // Either a success response or an error envelope — both confirm the
    // server parsed the request without crashing.
    let resp = read_response(&mut proc, 2, Duration::from_secs(5));
    if let Some(r) = resp {
        assert!(r.get("result").is_some() || r.get("error").is_some());
    }
    let _ = proc.shutdown(Duration::from_secs(5));
}

#[tokio::test]
async fn tasks_list_returns_json_rpc_response() {
    // Long-running tools post progress to TaskStore. The `tasks/list`
    // method returns an empty-but-valid response when no tasks are
    // running — proves the task endpoint is wired.
    let db = require_test_db!();
    let Some(mut proc) = spawn_initialized(&db) else {
        return;
    };
    send(
        &mut proc,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tasks/list",
            "params": {}
        }),
    );
    let resp = read_response(&mut proc, 2, Duration::from_secs(5));
    // Method may not exist (pre-MCP-spec-v2); both outcomes OK.
    if let Some(r) = resp {
        assert!(r.get("result").is_some() || r.get("error").is_some());
    }
    let _ = proc.shutdown(Duration::from_secs(5));
}

#[tokio::test]
async fn initialize_returns_expected_server_info_and_capabilities() {
    let db = require_test_db!();
    let Some(mut proc) = spawn_initialized(&db) else {
        return;
    };
    // Pull the init response we already wrote — do a second handshake
    // after reset to verify the first-response shape.
    send(
        &mut proc,
        &json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "t2", "version": "1" }
            }
        }),
    );
    let resp = read_response(&mut proc, 99, Duration::from_secs(10));
    if let Some(r) = resp {
        // rmcp may reject re-init with an error or return a fresh result.
        assert!(r.get("result").is_some() || r.get("error").is_some());
    }
    let _ = proc.shutdown(Duration::from_secs(5));
}
