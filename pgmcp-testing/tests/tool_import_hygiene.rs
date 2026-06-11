//! Real-Postgres oracle for the import-hygiene check (`import_hygiene` MCP tool +
//! the `collect_import_hygiene` sweep collector + the shared
//! `nested_import_violations` query).
//!
//! Two layers of confidence:
//!   * **Direct-seed** (`import_hygiene_query_and_collector_flag_only_callable_bodies`)
//!     seeds `import_use` rows with controlled `source_symbol_id` (→ function /
//!     method / module / NULL) and proves the predicate precisely + deterministically:
//!     only callable-body imports surface, the duplication count is right, the
//!     language filter works, and the collector maps severity correctly.
//!   * **End-to-end** (`*_end_to_end`, `*_python_*`) seeds real source, runs the real
//!     `symbol-extraction` driver (which resolves `source_symbol_id` exactly as the
//!     cron does), and asserts through the MCP tool envelope — proving file-top and
//!     `mod tests { … }`-top imports are NOT flagged while function-body imports are.
//!
//! `require_test_db!` skips cleanly when no test DB is configured, so this runs
//! inside `verify.sh` Gate 5 without an `#[ignore]`. The literal
//! `call_tool_cli("import_hygiene", …)` also satisfies the dispatch coverage gate.

use std::sync::Arc;

use pgmcp::cron::symbol_extraction;
use pgmcp::db::DbClient;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::pool_tool_helpers::{
    context_with_pool, seed_file, seed_file_symbol, seed_project, server_with_pool,
};
use pgmcp_testing::require_test_db;

/// Extract the first text block of a tool result as JSON.
fn tool_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present");
    serde_json::from_str(&text).expect("tool output is JSON")
}

/// Seed one `indexed_files` row with real `content` + `language` so the
/// symbol-extraction driver has something to parse. `seed_file` hardcodes
/// `fn f() {}`/`rust`; this one is parameterized.
async fn seed_source_file(
    pool: &sqlx::PgPool,
    project_id: i32,
    path: &str,
    rel: &str,
    language: &str,
    content: &str,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO indexed_files \
             (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW()) \
         ON CONFLICT (path) DO UPDATE \
             SET content = EXCLUDED.content, content_hash = EXCLUDED.content_hash, \
                 language = EXCLUDED.language, size_bytes = EXCLUDED.size_bytes, \
                 line_count = EXCLUDED.line_count \
         RETURNING id",
    )
    .bind(project_id)
    .bind(path)
    .bind(rel)
    .bind(language)
    .bind(content.len() as i64)
    .bind(content)
    .bind(content.len() as i64)
    .bind(content.lines().count() as i32)
    .fetch_one(pool)
    .await
    .expect("seed source file")
}

/// Seed one resolved `import_use` reference. `source_symbol_id = Some(id)` models a
/// nested import (the resolve pass pointed it at its enclosing symbol); `None`
/// models a file-top import (enclosed by no symbol). Idempotent on the natural key.
async fn seed_import_use(
    pool: &sqlx::PgPool,
    source_file_id: i64,
    source_symbol_id: Option<i64>,
    target_raw: &str,
    source_line: i32,
) {
    sqlx::query(
        "INSERT INTO symbol_references \
             (source_file_id, source_symbol_id, target_raw, ref_kind, source_line) \
         VALUES ($1, $2, $3, 'import_use', $4) \
         ON CONFLICT (source_file_id, source_line, target_raw, ref_kind) \
             DO UPDATE SET source_symbol_id = EXCLUDED.source_symbol_id",
    )
    .bind(source_file_id)
    .bind(source_symbol_id)
    .bind(target_raw)
    .bind(source_line)
    .execute(pool)
    .await
    .expect("seed import_use");
}

/// Direct-seed the *resolved* state and assert the predicate precisely: only
/// callable-body imports (function/method) are violations; module-top and file-top
/// imports are not; duplication counts and the language filter are correct; and the
/// collector maps `dup_count` → severity.
#[tokio::test(flavor = "multi_thread")]
async fn import_hygiene_query_and_collector_flag_only_callable_bodies() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project = seed_project(&pool, "ih-seed", "/ws/ih-seed").await;
    let file = seed_file(&pool, project, "/ws/ih-seed/m.rs", "m.rs").await;

    let f1 = seed_file_symbol(&pool, file, "f1", "function", 10, None).await;
    let f2 = seed_file_symbol(&pool, file, "f2", "function", 20, None).await;
    let method = seed_file_symbol(&pool, file, "do_it", "method", 30, None).await;
    let module = seed_file_symbol(&pool, file, "tests", "module", 40, None).await;

    // Same import re-typed in two function bodies → duplication = 2.
    seed_import_use(&pool, file, Some(f1), "std::fs", 11).await;
    seed_import_use(&pool, file, Some(f2), "std::fs", 21).await;
    // A method-body import → flagged, duplication = 1.
    seed_import_use(&pool, file, Some(method), "std::io::Write", 31).await;
    // A test-module-top import → NOT flagged (module is not a callable body).
    seed_import_use(&pool, file, Some(module), "std::fmt::Debug", 41).await;
    // A file-top import (no enclosing symbol) → NOT flagged.
    seed_import_use(&pool, file, None, "std::collections::HashMap", 1).await;

    // ── Shared query ────────────────────────────────────────────────────────
    let rows = pgmcp::db::queries::nested_import_violations(&pool, project, None)
        .await
        .expect("nested_import_violations");
    assert_eq!(
        rows.len(),
        3,
        "only the 3 callable-body imports are violations: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.target_raw.contains("HashMap")),
        "file-top import must be excluded: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.target_raw.contains("Debug")),
        "module-top import must be excluded: {rows:?}"
    );
    let fs_rows: Vec<_> = rows
        .iter()
        .filter(|r| r.target_raw.contains("std::fs"))
        .collect();
    assert_eq!(fs_rows.len(), 2, "both std::fs imports flagged: {rows:?}");
    assert!(
        fs_rows.iter().all(|r| r.dup_count == 2),
        "duplicated import → dup_count 2: {fs_rows:?}"
    );
    let io_row = rows
        .iter()
        .find(|r| r.target_raw.contains("Write"))
        .expect("the method-body import");
    assert_eq!(io_row.dup_count, 1, "singleton import → dup_count 1");
    assert_eq!(io_row.enclosing_kind, "method");

    // ── Language filter ─────────────────────────────────────────────────────
    let rust_only = pgmcp::db::queries::nested_import_violations(&pool, project, Some("rust"))
        .await
        .expect("rust filter");
    assert_eq!(rust_only.len(), 3, "rust filter matches the rust file");
    let py_only = pgmcp::db::queries::nested_import_violations(&pool, project, Some("python"))
        .await
        .expect("python filter");
    assert!(py_only.is_empty(), "python filter excludes the rust file");

    // ── Collector (sweep path) ──────────────────────────────────────────────
    let ctx = context_with_pool(pool.clone());
    let findings =
        pgmcp::quality::collectors::hygiene::collect_import_hygiene(&ctx, project, "ih-seed")
            .await
            .expect("collect_import_hygiene");
    assert_eq!(
        findings.len(),
        3,
        "collector emits one finding per violation"
    );
    assert!(findings.iter().all(|f| f.source_tool == "import_hygiene"));
    assert!(findings.iter().all(|f| f.category.title() == "Hygiene"));
    let medium = findings
        .iter()
        .filter(|f| f.severity.label() == "Medium")
        .count();
    let low = findings
        .iter()
        .filter(|f| f.severity.label() == "Low")
        .count();
    assert_eq!(
        (medium, low),
        (2, 1),
        "two duplicated (Medium) + one singleton (Low): {findings:?}"
    );
}

const RUST_FIXTURE: &str = r#"use std::collections::HashMap;

pub fn good() -> HashMap<u8, u8> {
    HashMap::new()
}

pub fn bad_one() {
    use std::fs;
    let _ = fs::metadata(".");
}

pub fn bad_two() {
    use std::fs;
    let _ = fs::metadata("..");
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    #[test]
    fn t_inner() {
        use std::io::Write;
        let mut s = std::io::sink();
        let _ = s.write(b"x");
        let _: Option<&dyn Debug> = None;
    }
}
"#;

/// End-to-end through real extraction + the MCP tool: file-top and test-module-top
/// imports are NOT flagged; function-body and test-function-body imports ARE; and a
/// `use` re-typed across two function bodies reports duplication 2 (Medium).
#[tokio::test(flavor = "multi_thread")]
async fn import_hygiene_flags_function_body_imports_end_to_end() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project = seed_project(&pool, "ih-rust", "/ws/ih-rust").await;
    seed_source_file(
        &pool,
        project,
        "/ws/ih-rust/lib.rs",
        "lib.rs",
        "rust",
        RUST_FIXTURE,
    )
    .await;

    // Real extraction populates import_use rows AND resolves source_symbol_id.
    let db_client: Arc<dyn DbClient> = Arc::new(pool.clone());
    let stats = Arc::new(StatsTracker::new());
    symbol_extraction::run_symbol_extraction_for_project(db_client.as_ref(), &stats, "ih-rust")
        .await;

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "import_hygiene",
            serde_json::json!({ "project": "ih-rust" }),
        )
        .await
        .expect("import_hygiene call");
    let v = tool_json(&result);

    assert_eq!(
        v["health"]["symbols_present"], true,
        "symbols present after extraction: {v}"
    );
    let viols = v["violations"].as_array().expect("violations array");
    assert_eq!(
        v["total_violations"].as_i64(),
        Some(3),
        "exactly the 3 function-body imports are flagged: {v}"
    );
    assert!(
        !viols
            .iter()
            .any(|x| x["import"].as_str().unwrap_or("").contains("HashMap")),
        "file-top import must not be flagged: {v}"
    );
    assert!(
        !viols
            .iter()
            .any(|x| x["import"].as_str().unwrap_or("").contains("Debug")),
        "test-module-top import must not be flagged: {v}"
    );
    let fs: Vec<_> = viols
        .iter()
        .filter(|x| x["import"].as_str().unwrap_or("").contains("std::fs"))
        .collect();
    assert_eq!(
        fs.len(),
        2,
        "both std::fs function-body imports flagged: {v}"
    );
    assert!(
        fs.iter().all(|x| x["duplication"].as_i64() == Some(2)),
        "duplicated std::fs → duplication 2: {v}"
    );
    assert!(
        fs.iter().all(|x| x["severity"].as_str() == Some("medium")),
        "dup ≥ 2 → medium severity: {v}"
    );
    assert!(
        viols
            .iter()
            .any(|x| x["import"].as_str().unwrap_or("").contains("Write")),
        "test-function-body import must be flagged: {v}"
    );
}

const PY_FIXTURE: &str = r#"import os


def handler():
    import json
    return json.dumps({"pid": os.getpid()})
"#;

/// Cross-language: a Python `import` inside a `def` body is flagged; the
/// module-level `import os` is not. Proves the all-languages decision end-to-end.
#[tokio::test(flavor = "multi_thread")]
async fn import_hygiene_flags_python_function_body_imports() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project = seed_project(&pool, "ih-py", "/ws/ih-py").await;
    seed_source_file(
        &pool,
        project,
        "/ws/ih-py/app.py",
        "app.py",
        "python",
        PY_FIXTURE,
    )
    .await;

    let db_client: Arc<dyn DbClient> = Arc::new(pool.clone());
    let stats = Arc::new(StatsTracker::new());
    symbol_extraction::run_symbol_extraction_for_project(db_client.as_ref(), &stats, "ih-py").await;

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli("import_hygiene", serde_json::json!({ "project": "ih-py" }))
        .await
        .expect("import_hygiene call");
    let v = tool_json(&result);

    let viols = v["violations"].as_array().expect("violations array");
    assert_eq!(
        v["total_violations"].as_i64(),
        Some(1),
        "only the def-body `import json` is flagged (module-top `import os` is not): {v}"
    );
    assert!(
        viols
            .iter()
            .any(|x| x["import"].as_str().unwrap_or("").contains("json")),
        "the function-body import must be `json`: {v}"
    );
}

/// Unknown project soft-fails (never errors) with `health.symbols_present:false`.
#[tokio::test(flavor = "multi_thread")]
async fn import_hygiene_soft_fails_on_unknown_project() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "import_hygiene",
            serde_json::json!({ "project": "does-not-exist-xyz" }),
        )
        .await
        .expect("import_hygiene soft-fail call");
    let v = tool_json(&result);
    assert_eq!(
        v["health"]["symbols_present"], false,
        "unknown project → symbols_present false: {v}"
    );
    assert_eq!(v["total_violations"].as_i64(), Some(0));
    assert!(
        v["violations"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "no violations for unknown project: {v}"
    );
}
