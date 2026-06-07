//! Integration tests for the rebuilt composed-phonetic tools:
//!
//! - `tool_phonetic_symbol_search` (G5): now SEARCHES the project's persistent
//!   symbol trie via a composed phonetic∘edit query (it no longer requires the
//!   caller to supply candidates).
//! - `tool_correct_query` (G2): now corrects against the project's persistent
//!   symbol vocabulary using pgmcp's own WFST corrector (not the llammer
//!   stub), so a near-miss token is corrected to a real symbol.
//!
//! Both self-skip without a CREATEDB-capable test DB (`require_test_db!`).

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::{CorrectQueryParams, PhoneticSymbolSearchParams};
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::{tool_correct_query, tool_phonetic_symbol_search};
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

async fn seed_symbols(pool: &sqlx::PgPool, project_name: &str, symbol_names: &[&str]) {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1 RETURNING id",
    )
    .bind(format!("/ws/{project_name}"))
    .bind(format!("/ws/{project_name}/proj"))
    .bind(project_name)
    .fetch_one(pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, 'rust', $4, $5, $6, $7, NOW()) \
         ON CONFLICT (path) DO UPDATE SET content = $5 RETURNING id"
    )
    .bind(project_id)
    .bind(format!("/ws/{project_name}/proj/src/lib.rs"))
    .bind("src/lib.rs")
    .bind(2048_i64)
    .bind("seed")
    .bind(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0) ^ (project_name.len() as i64),
    )
    .bind(20_i32)
    .fetch_one(pool)
    .await
    .expect("file");
    for name in symbol_names {
        sqlx::query(
            "INSERT INTO file_symbols (file_id, name, kind, visibility, start_line, end_line) \
             VALUES ($1, $2, 'function', 'public', 1, 1) ON CONFLICT DO NOTHING",
        )
        .bind(file_id)
        .bind(*name)
        .execute(pool)
        .await
        .expect("symbol");
    }
}

fn build_ctx(db: Arc<dyn DbClient>, data_dir: std::path::PathBuf) -> SystemContext {
    let mut cfg = Config::default();
    cfg.fuzzy.data_dir = data_dir;
    SystemContext::production(
        db,
        EmbedSource::backend(Arc::new(DeterministicEmbeddingBackend::new(1024))),
        Arc::new(StatsTracker::new()),
        Arc::new(ArcSwap::from_pointee(cfg)),
        Arc::new(LogBroadcaster::new()),
        Arc::new(TaskStore::new()),
        DaemonLifecycle::new(),
    )
}

fn result_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text content")
}

#[tokio::test(flavor = "multi_thread")]
async fn phonetic_symbol_search_searches_the_trie() {
    let testdb = require_test_db!();
    seed_symbols(
        testdb.pool(),
        "phon_search_test",
        &["phone_handler", "telephone_ringer", "decode_frame"],
    )
    .await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = build_ctx(Arc::new(testdb.pool().clone()), tmp.path().to_path_buf());

    // "fone" is a phonetic variant of "phone" — the composed phonetic∘edit
    // search should surface the phone-bearing symbols from the trie WITHOUT the
    // caller supplying candidates.
    let result = tool_phonetic_symbol_search::run(
        &ctx,
        PhoneticSymbolSearchParams {
            query: "fone".to_string(),
            project: "phon_search_test".to_string(),
            max_distance: Some(2),
            limit: Some(20),
        },
    )
    .await
    .expect("phonetic_symbol_search");
    let val: serde_json::Value = serde_json::from_str(&result_text(&result)).expect("json");
    let symbols: Vec<&str> = val["matches"]
        .as_array()
        .expect("matches array")
        .iter()
        .filter_map(|m| m.get("symbol").and_then(|v| v.as_str()))
        .collect();
    assert!(
        symbols.iter().any(|s| s.contains("phone")),
        "expected a phone-bearing symbol from the trie; got {symbols:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn phonetic_symbol_search_rejects_blank_query_before_trie_open() {
    let testdb = require_test_db!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = build_ctx(Arc::new(testdb.pool().clone()), tmp.path().to_path_buf());

    let err = tool_phonetic_symbol_search::run(
        &ctx,
        PhoneticSymbolSearchParams {
            query: "   ".to_string(),
            project: "   ".to_string(),
            max_distance: Some(2),
            limit: Some(20),
        },
    )
    .await
    .expect_err("blank query should reject before trie open");
    assert!(
        err.to_string().contains("query must be non-empty"),
        "unexpected blank query error: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn phonetic_symbol_search_trims_project_query_and_reports_effective_bounds() {
    let testdb = require_test_db!();
    seed_symbols(
        testdb.pool(),
        "phon_trim_test",
        &["phone_handler", "telephone_ringer", "decode_frame"],
    )
    .await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = build_ctx(Arc::new(testdb.pool().clone()), tmp.path().to_path_buf());

    let result = tool_phonetic_symbol_search::run(
        &ctx,
        PhoneticSymbolSearchParams {
            query: "  fone  ".to_string(),
            project: "  phon_trim_test  ".to_string(),
            max_distance: Some(u32::MAX),
            limit: Some(0),
        },
    )
    .await
    .expect("phonetic_symbol_search");
    let val: serde_json::Value = serde_json::from_str(&result_text(&result)).expect("json");
    assert_eq!(val["query"].as_str(), Some("fone"));
    assert_eq!(val["project"].as_str(), Some("phon_trim_test"));
    assert_eq!(
        val["max_distance"].as_u64(),
        Some(pgmcp::fuzzy::limits::MAX_FUZZY_DISTANCE as u64)
    );
    assert_eq!(val["limit"].as_u64(), Some(1));
    assert!(
        val["matches"].as_array().expect("matches array").len() <= 1,
        "effective limit must cap results to one: {val:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn correct_query_corrects_against_project_vocab() {
    let testdb = require_test_db!();
    seed_symbols(
        testdb.pool(),
        "correct_q_test",
        &["receive", "decode", "render"],
    )
    .await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = build_ctx(Arc::new(testdb.pool().clone()), tmp.path().to_path_buf());

    // No trained LM model exists for this project → edit/phonetic-only path.
    // "recieve" is a one-transposition typo of the seeded "receive".
    let result = tool_correct_query::run(
        &ctx,
        CorrectQueryParams {
            query: "recieve".to_string(),
            project: "correct_q_test".to_string(),
            max_distance: Some(2),
            lm_weight: Some(0.0),
        },
    )
    .await
    .expect("correct_query");
    let val: serde_json::Value = serde_json::from_str(&result_text(&result)).expect("json");
    assert_eq!(
        val["used_lm"].as_bool(),
        Some(false),
        "no model present → used_lm must be false"
    );
    assert_eq!(
        val["model_available"].as_bool(),
        Some(false),
        "no trained model on disk for this project"
    );
    assert_eq!(val["input"].as_str(), Some("recieve"));
    // The no-LM edit/phonetic path must COMMIT the correction to the real
    // seeded symbol (the Bug-1 fix), not merely echo the input back.
    assert_eq!(
        val["corrected"].as_str(),
        Some("receive"),
        "OOV typo must be corrected to the seeded symbol without an LM"
    );
    assert_eq!(val["changed"].as_bool(), Some(true));
    assert_eq!(
        val["confidence"].as_f64(),
        Some(0.25),
        "edit-only correction → 0.25 confidence tier"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn correct_query_does_not_overcorrect_valid_symbol() {
    let testdb = require_test_db!();
    seed_symbols(testdb.pool(), "correct_q_guard", &["chunker", "chunked"]).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = build_ctx(Arc::new(testdb.pool().clone()), tmp.path().to_path_buf());

    // "chunked" IS a seeded symbol; it must not be nudged to its distance-1
    // neighbor "chunker" (over-correction guard).
    let result = tool_correct_query::run(
        &ctx,
        CorrectQueryParams {
            query: "chunked".to_string(),
            project: "correct_q_guard".to_string(),
            max_distance: Some(2),
            lm_weight: Some(0.0),
        },
    )
    .await
    .expect("correct_query");
    let val: serde_json::Value = serde_json::from_str(&result_text(&result)).expect("json");
    assert_eq!(val["corrected"].as_str(), Some("chunked"));
    assert_eq!(val["changed"].as_bool(), Some(false));
    assert_eq!(val["confidence"].as_f64(), Some(1.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn correct_query_corrects_mixedcase_symbol() {
    let testdb = require_test_db!();
    seed_symbols(testdb.pool(), "correct_q_case", &["ChunkerInput"]).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = build_ctx(Arc::new(testdb.pool().clone()), tmp.path().to_path_buf());

    // Regression for the camelCase repro: the trie is queried in the original
    // case, so "ChunkerInpt" is distance 1 from the seeded "ChunkerInput" and
    // is corrected (a lowercasing query path would miss it at max_distance=2).
    let result = tool_correct_query::run(
        &ctx,
        CorrectQueryParams {
            query: "ChunkerInpt".to_string(),
            project: "correct_q_case".to_string(),
            max_distance: Some(2),
            lm_weight: Some(0.0),
        },
    )
    .await
    .expect("correct_query");
    let val: serde_json::Value = serde_json::from_str(&result_text(&result)).expect("json");
    assert_eq!(val["corrected"].as_str(), Some("ChunkerInput"));
    assert_eq!(val["changed"].as_bool(), Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn correct_query_normalizes_project_query_and_bounds() {
    let testdb = require_test_db!();
    seed_symbols(testdb.pool(), "correct_q_norm", &["receive"]).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = build_ctx(Arc::new(testdb.pool().clone()), tmp.path().to_path_buf());

    let result = tool_correct_query::run(
        &ctx,
        CorrectQueryParams {
            query: "  recieve  ".to_string(),
            project: "  correct_q_norm  ".to_string(),
            max_distance: Some(u32::MAX),
            lm_weight: Some(9.0),
        },
    )
    .await
    .expect("correct_query");
    let val: serde_json::Value = serde_json::from_str(&result_text(&result)).expect("json");
    assert_eq!(val["project"].as_str(), Some("correct_q_norm"));
    assert_eq!(val["input"].as_str(), Some("recieve"));
    assert_eq!(val["corrected"].as_str(), Some("receive"));
    assert_eq!(val["max_distance"].as_u64(), Some(64));
    assert_eq!(val["lm_weight"].as_f64(), Some(1.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn correct_query_rejects_blank_query_and_nonfinite_lm_weight() {
    let testdb = require_test_db!();
    seed_symbols(testdb.pool(), "correct_q_invalid", &["receive"]).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = build_ctx(Arc::new(testdb.pool().clone()), tmp.path().to_path_buf());

    let blank = tool_correct_query::run(
        &ctx,
        CorrectQueryParams {
            query: "   ".to_string(),
            project: "correct_q_invalid".to_string(),
            max_distance: Some(2),
            lm_weight: Some(0.0),
        },
    )
    .await;
    assert!(blank.is_err(), "blank query must be rejected");

    let nan_weight = tool_correct_query::run(
        &ctx,
        CorrectQueryParams {
            query: "recieve".to_string(),
            project: "correct_q_invalid".to_string(),
            max_distance: Some(2),
            lm_weight: Some(f64::NAN),
        },
    )
    .await;
    assert!(nan_weight.is_err(), "non-finite lm_weight must be rejected");
}

#[tokio::test(flavor = "multi_thread")]
async fn correct_query_rejects_duplicate_project_names() {
    let testdb = require_test_db!();
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws/correct_dup_a")
        .bind("/ws/correct_dup_a/p")
        .bind("correct_dup")
        .execute(testdb.pool())
        .await
        .expect("project a");
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws/correct_dup_b")
        .bind("/ws/correct_dup_b/p")
        .bind("correct_dup")
        .execute(testdb.pool())
        .await
        .expect("project b");

    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = build_ctx(Arc::new(testdb.pool().clone()), tmp.path().to_path_buf());

    let err = tool_correct_query::run(
        &ctx,
        CorrectQueryParams {
            query: "recieve".to_string(),
            project: "correct_dup".to_string(),
            max_distance: Some(2),
            lm_weight: Some(0.0),
        },
    )
    .await
    .expect_err("duplicate project display names must fail closed");
    assert!(
        err.to_string().contains("ambiguous project name"),
        "unexpected duplicate-name error: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn correct_query_uses_project_id_artifact_key_for_slug_collisions() {
    let testdb = require_test_db!();
    seed_symbols(testdb.pool(), "correct/slug", &["alpha_handler"]).await;
    seed_symbols(testdb.pool(), "correct_slug", &["beta_handler"]).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ctx = build_ctx(Arc::new(testdb.pool().clone()), tmp.path().to_path_buf());

    let alpha = tool_correct_query::run(
        &ctx,
        CorrectQueryParams {
            query: "alpha_hanlder".to_string(),
            project: "correct/slug".to_string(),
            max_distance: Some(2),
            lm_weight: Some(0.0),
        },
    )
    .await
    .expect("alpha correct_query");
    let alpha_val: serde_json::Value =
        serde_json::from_str(&result_text(&alpha)).expect("alpha json");
    assert_eq!(
        alpha_val["corrected"].as_str(),
        Some("alpha_handler"),
        "slash project must use its own trie"
    );

    let beta = tool_correct_query::run(
        &ctx,
        CorrectQueryParams {
            query: "beta_hanlder".to_string(),
            project: "correct_slug".to_string(),
            max_distance: Some(2),
            lm_weight: Some(0.0),
        },
    )
    .await
    .expect("beta correct_query");
    let beta_val: serde_json::Value = serde_json::from_str(&result_text(&beta)).expect("beta json");
    assert_eq!(
        beta_val["corrected"].as_str(),
        Some("beta_handler"),
        "underscore project must not reuse the slash project's trie"
    );
}
