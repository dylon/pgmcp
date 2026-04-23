//! MCP protocol integration test.
//!
//! This test spawns pgmcp as a subprocess with stdio transport and
//! sends JSON-RPC 2.0 requests to verify tool responses.
//!
//! Requires:
//! - pgmcp binary built (`cargo build`)
//! - PostgreSQL with pgvector running
//! - Valid config at ~/.config/pgmcp/config.toml (or PGMCP_CONFIG env var)
//!
//! Run with: `cargo test --test mcp_protocol -- --ignored`

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

/// Send a JSON-RPC message to the subprocess and read the response.
fn send_jsonrpc(
    stdin: &mut impl Write,
    stdout: &mut impl BufRead,
    request: &Value,
) -> Option<Value> {
    let msg = serde_json::to_string(request).expect("serialize request");
    writeln!(stdin, "{}", msg).expect("write to stdin");
    stdin.flush().expect("flush stdin");

    let mut line = String::new();
    match stdout.read_line(&mut line) {
        Ok(0) => None, // EOF
        Ok(_) => {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(serde_json::from_str(trimmed).expect("parse response"))
            }
        }
        Err(e) => panic!("Failed to read response: {}", e),
    }
}

/// This test is ignored by default because it requires a running PostgreSQL
/// instance and the pgmcp binary to be built.
///
/// To run: `cargo test --test mcp_protocol -- --ignored`
#[test]
#[ignore = "requires PostgreSQL and built binary"]
fn test_mcp_initialize_and_list_tools() {
    // Build the binary first
    let build_status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("Failed to run cargo build");
    assert!(build_status.success(), "cargo build failed");

    let binary = env!("CARGO_BIN_EXE_pgmcp");

    let mut child = Command::new(binary)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn pgmcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    // Give it a moment to start
    std::thread::sleep(Duration::from_secs(2));

    // Send initialize request
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "test-client",
                "version": "1.0.0"
            }
        }
    });

    let response = send_jsonrpc(&mut stdin, &mut reader, &init_request);
    if let Some(resp) = &response {
        assert!(
            resp.get("result").is_some(),
            "initialize should have result"
        );
        let result = &resp["result"];
        assert_eq!(result["serverInfo"]["name"], "pgmcp");
    }

    // Send initialized notification
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let msg = serde_json::to_string(&initialized).expect("serialize");
    writeln!(stdin, "{}", msg).expect("write");
    stdin.flush().expect("flush");

    // List tools
    let list_tools = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });

    let response = send_jsonrpc(&mut stdin, &mut reader, &list_tools);
    if let Some(resp) = &response {
        assert!(
            resp.get("result").is_some(),
            "tools/list should have result"
        );
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        let tool_names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().expect("tool name"))
            .collect();

        // Verify expected tools are present
        assert!(
            tool_names.contains(&"semantic_search"),
            "should have semantic_search"
        );
        assert!(
            tool_names.contains(&"text_search"),
            "should have text_search"
        );
        assert!(tool_names.contains(&"grep"), "should have grep");
        assert!(tool_names.contains(&"read_file"), "should have read_file");
        assert!(
            tool_names.contains(&"list_projects"),
            "should have list_projects"
        );
        assert!(
            tool_names.contains(&"project_tree"),
            "should have project_tree"
        );
        assert!(tool_names.contains(&"file_info"), "should have file_info");
        assert!(
            tool_names.contains(&"index_stats"),
            "should have index_stats"
        );
    }

    // Call index_stats tool
    let stats_call = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "index_stats",
            "arguments": {}
        }
    });

    let response = send_jsonrpc(&mut stdin, &mut reader, &stats_call);
    if let Some(resp) = &response {
        assert!(
            resp.get("result").is_some(),
            "index_stats should have result"
        );
    }

    // Clean up
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
}

/// Test that list_projects returns valid JSON.
#[test]
#[ignore = "requires PostgreSQL and built binary"]
fn test_mcp_list_projects() {
    let binary = env!("CARGO_BIN_EXE_pgmcp");

    let mut child = Command::new(binary)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn pgmcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    std::thread::sleep(Duration::from_secs(2));

    // Initialize
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1.0" }
        }
    });
    let _ = send_jsonrpc(&mut stdin, &mut reader, &init);

    // Notification
    let notif = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let msg = serde_json::to_string(&notif).expect("ser");
    writeln!(stdin, "{}", msg).expect("write");
    stdin.flush().expect("flush");

    // Call list_projects
    let call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "list_projects",
            "arguments": {}
        }
    });

    let response = send_jsonrpc(&mut stdin, &mut reader, &call);
    if let Some(resp) = &response {
        assert!(
            resp.get("result").is_some() || resp.get("error").is_some(),
            "should have result or error"
        );
    }

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
}
