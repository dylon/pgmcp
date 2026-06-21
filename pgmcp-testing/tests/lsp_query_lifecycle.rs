//! Integration test for `lsp_query` (ADR-026, item 11) over seeded
//! `file_symbols` + `symbol_occurrences`. Drives the dispatched tool through
//! `call_tool_cli` (Layer-D coverage gate) and asserts the core LSP ops.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

#[tokio::test]
async fn lsp_query_core_ops() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // Seed project + file.
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ('/ws/lspq', '/ws/lspq/p', 'lspq')
         ON CONFLICT (path) DO UPDATE SET name = 'lspq' RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, '/ws/lspq/p/src/lib.rs', 'src/lib.rs', 'rust', 100, 'seed', 'lspqhash', 20, NOW())
         ON CONFLICT (path) DO UPDATE SET content = 'seed' RETURNING id",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .expect("file");

    // Two symbols: a function `alphaFn` and a struct `BetaType`.
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO file_symbols (file_id, name, kind, visibility, start_line, end_line, signature)
         VALUES ($1, 'alphaFn', 'function', 'public', 5, 10, 'fn alphaFn(x: i32) -> bool')
         RETURNING id",
    )
    .bind(file_id)
    .fetch_one(&pool)
    .await
    .expect("sym alphaFn");
    sqlx::query(
        "INSERT INTO file_symbols (file_id, name, kind, visibility, start_line, end_line)
         VALUES ($1, 'BetaType', 'struct', 'public', 12, 14)",
    )
    .bind(file_id)
    .execute(&pool)
    .await
    .expect("sym BetaType");

    // Occurrences of alphaFn: a definition + a code reference; one in a comment.
    let occurrences: [(i32, i32, &str); 3] = [
        (5, 3, "definition"),
        (7, 8, "code_reference"),
        (3, 4, "comment"),
    ];
    for (line, col, kind) in occurrences {
        sqlx::query(
            "INSERT INTO symbol_occurrences (file_id, name, start_line, start_col, end_col, occurrence_kind, enclosing_symbol_id)
             VALUES ($1, 'alphaFn', $2, $3, $4, $5, $6)",
        )
        .bind(file_id)
        .bind(line)
        .bind(col)
        .bind(col + 7)
        .bind(kind)
        .bind(sid)
        .execute(&pool)
        .await
        .expect("occurrence");
    }

    // capabilities — no project needed.
    let caps = body(
        &server
            .call_tool_cli("lsp_query", json!({"op": "capabilities"}))
            .await
            .expect("caps"),
    );
    let ops = caps["ops"].as_array().expect("ops");
    assert!(ops.iter().any(|o| o == "references"));
    assert!(ops.iter().any(|o| o == "document_symbol"));

    // document_symbol.
    let ds = body(
        &server
            .call_tool_cli(
                "lsp_query",
                json!({"project": "lspq", "op": "document_symbol", "file_path": "src/lib.rs"}),
            )
            .await
            .expect("document_symbol"),
    );
    let names: Vec<&str> = ds["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"alphaFn"), "{names:?}");
    assert!(names.contains(&"BetaType"), "{names:?}");

    // workspace_symbol (fuzzy).
    let ws = body(
        &server
            .call_tool_cli(
                "lsp_query",
                json!({"project": "lspq", "op": "workspace_symbol", "symbol": "alpha"}),
            )
            .await
            .expect("workspace_symbol"),
    );
    assert!(
        ws["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "alphaFn")
    );

    // definition.
    let def = body(
        &server
            .call_tool_cli(
                "lsp_query",
                json!({"project": "lspq", "op": "definition", "symbol": "alphaFn"}),
            )
            .await
            .expect("definition"),
    );
    assert_eq!(def["definitions"].as_array().unwrap().len(), 1);
    assert_eq!(def["definitions"][0]["start_line"], 5);

    // references — all occurrences of alphaFn.
    let refs = body(
        &server
            .call_tool_cli(
                "lsp_query",
                json!({"project": "lspq", "op": "references", "symbol": "alphaFn"}),
            )
            .await
            .expect("references"),
    );
    assert_eq!(refs["count"], 3, "def + code_reference + comment: {refs}");

    // document_highlight — same file only.
    let dh = body(&server.call_tool_cli("lsp_query", json!({"project": "lspq", "op": "document_highlight", "symbol": "alphaFn", "file_path": "src/lib.rs"})).await.expect("document_highlight"));
    assert_eq!(dh["count"], 3);

    // hover — signature + found.
    let hv = body(
        &server
            .call_tool_cli(
                "lsp_query",
                json!({"project": "lspq", "op": "hover", "symbol": "alphaFn"}),
            )
            .await
            .expect("hover"),
    );
    assert_eq!(hv["found"], true);
    assert!(hv["signature"].as_str().unwrap().contains("alphaFn"));

    // invalid op fails closed.
    assert!(
        server
            .call_tool_cli("lsp_query", json!({"project": "lspq", "op": "nonsense"}))
            .await
            .is_err()
    );
}
