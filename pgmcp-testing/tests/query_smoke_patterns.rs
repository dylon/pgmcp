//! Layer C addendum: SQL-execution smoke tests for `src/db/patterns.rs`.
//!
//! The software-pattern catalog is seeded at every migration run, so
//! its tables (`software_patterns`, `software_pattern_paradigms`,
//! `software_pattern_sources`, `software_pattern_source_patterns`,
//! `software_pattern_chunks`, `software_pattern_import_runs`,
//! `programming_paradigms`) already have rows when our test DB comes
//! up. We exercise every public function in `db::patterns` to catch
//! column-name drift the same way Layer B does for `db::queries`.
//!
//! 21 functions total: 3 pure (`content_hash`, `content_sha256`,
//! `chunk_text`) and 18 DB-backed.

mod common;

use chrono::Utc;
use pgmcp::db::patterns::{self, PatternListOptions, PatternSearchOptions, SourceUpsert};
use pgmcp::patterns::{ParadigmSeed, PatternSeed};
use pgmcp_testing::fixtures::synthetic_corpus::SyntheticCorpus;
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

// =============================================================================
// Helpers
// =============================================================================

/// Construct a ParadigmSeed with sensible defaults.
fn fake_paradigm(slug: &'static str) -> ParadigmSeed {
    ParadigmSeed {
        slug,
        name: "test paradigm",
        description: "test",
        wikipedia_url: "https://en.wikipedia.org/wiki/test",
    }
}

/// Construct a PatternSeed with sensible defaults.
fn fake_pattern(slug: &'static str) -> PatternSeed {
    PatternSeed {
        slug,
        name: "Test Pattern",
        kind: "pattern",
        category: "test",
        summary: "Test summary",
        intent: "test intent",
        problem: "test problem",
        solution: "test solution",
        consequences: "test consequences",
        paradigms: &[],
        tags: &["test"],
        canonical_url: "https://example.com",
    }
}

/// Construct a SourceUpsert with sensible defaults.
fn fake_source<'a>(title: &'a str) -> SourceUpsert<'a> {
    SourceUpsert {
        source_family: "test_family",
        title,
        url: None,
        license_label: None,
        source_type: "manual",
        ingest_policy: "stored",
        content: Some("test content"),
        status: "ok",
        error: None,
        metadata: serde_json::json!({}),
        fetched_at: Some(Utc::now()),
    }
}

/// Pull any seeded pattern id from the catalog.
async fn any_pattern_id(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT id FROM software_patterns LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("any_pattern_id")
}

fn test_embedding() -> Vec<f32> {
    pgmcp_testing::fixtures::synthetic_corpus::basis(0)
}

// =============================================================================
// Pure helpers (3 functions)
// =============================================================================

#[test]
fn patterns_content_hash_returns_i64() {
    let h = patterns::content_hash("hello");
    let _ = h; // function only requires that it returns an i64
}

#[test]
fn patterns_content_sha256_returns_hex() {
    let s = patterns::content_sha256("hello");
    assert_eq!(s.len(), 64, "sha256 hex must be 64 chars");
    assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn patterns_chunk_text_returns_chunks() {
    // chunk_text(content, max_chars_per_chunk, max_chunks).
    let chunks = patterns::chunk_text("hello world", 5, 10);
    assert!(!chunks.is_empty(), "chunk_text must produce >=1 chunk");
}

// =============================================================================
// DB-backed functions (18 functions)
// =============================================================================

#[tokio::test]
async fn patterns_upsert_paradigm_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = patterns::upsert_paradigm(db.pool(), &fake_paradigm("test-paradigm-1"))
        .await
        .expect("upsert_paradigm");
}

#[tokio::test]
async fn patterns_upsert_pattern_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = patterns::upsert_pattern(db.pool(), &fake_pattern("test-pattern-1"))
        .await
        .expect("upsert_pattern");
}

#[tokio::test]
async fn patterns_link_pattern_paradigm_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = patterns::upsert_paradigm(db.pool(), &fake_paradigm("test-link-paradigm"))
        .await
        .expect("paradigm");
    let pid = patterns::upsert_pattern(db.pool(), &fake_pattern("test-link-pattern"))
        .await
        .expect("pattern");
    patterns::link_pattern_paradigm(db.pool(), pid, "test-link-paradigm")
        .await
        .expect("link_pattern_paradigm");
}

#[tokio::test]
async fn patterns_find_pattern_id_by_slug_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = patterns::find_pattern_id_by_slug(db.pool(), "nonexistent-slug")
        .await
        .expect("find_pattern_id_by_slug");
}

#[tokio::test]
async fn patterns_upsert_source_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = patterns::upsert_source(db.pool(), fake_source("test-source-1"))
        .await
        .expect("upsert_source");
    // Run again to exercise the existing-id update branch.
    let _ = patterns::upsert_source(db.pool(), fake_source("test-source-1"))
        .await
        .expect("upsert_source (existing)");
}

#[tokio::test]
async fn patterns_find_source_state_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = patterns::find_source_state(db.pool(), "test_family", "no-such-title", None)
        .await
        .expect("find_source_state");
}

#[tokio::test]
async fn patterns_update_source_status_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let sid = patterns::upsert_source(db.pool(), fake_source("test-status-source"))
        .await
        .expect("upsert");
    patterns::update_source_status(
        db.pool(),
        sid,
        "ok",
        None,
        serde_json::json!({"note": "updated"}),
        Some(Utc::now()),
    )
    .await
    .expect("update_source_status");
}

#[tokio::test]
async fn patterns_link_source_pattern_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let sid = patterns::upsert_source(db.pool(), fake_source("test-link-source"))
        .await
        .expect("source");
    let pid = patterns::upsert_pattern(db.pool(), &fake_pattern("test-link-source-pattern"))
        .await
        .expect("pattern");
    patterns::link_source_pattern(db.pool(), sid, pid, "describes")
        .await
        .expect("link_source_pattern");
}

#[tokio::test]
async fn patterns_delete_source_chunks_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let sid = patterns::upsert_source(db.pool(), fake_source("test-delete-source"))
        .await
        .expect("source");
    patterns::delete_source_chunks(db.pool(), sid)
        .await
        .expect("delete_source_chunks");
}

#[tokio::test]
async fn patterns_insert_source_chunk_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let sid = patterns::upsert_source(db.pool(), fake_source("test-chunk-source"))
        .await
        .expect("source");
    let emb = test_embedding();
    patterns::insert_source_chunk(db.pool(), sid, 0, "chunk text", 1, 5, &emb)
        .await
        .expect("insert_source_chunk");
}

#[tokio::test]
async fn patterns_semantic_search_patterns_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let emb = test_embedding();
    let _ = patterns::semantic_search_patterns(
        db.pool(),
        &emb,
        5,
        100,
        PatternSearchOptions {
            kind: None,
            paradigms: None,
            category: None,
            source_family: None,
            source_type: None,
        },
    )
    .await
    .expect("semantic_search_patterns");
}

#[tokio::test]
async fn patterns_list_patterns_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = patterns::list_patterns(
        db.pool(),
        PatternListOptions {
            kind: None,
            paradigm: None,
            category: None,
            source_family: None,
            limit: 10,
            offset: 0,
        },
    )
    .await
    .expect("list_patterns");
}

#[tokio::test]
async fn patterns_get_pattern_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    // Either by slug or by id.  The seeded catalog has gof_singleton.
    let _ = patterns::get_pattern(db.pool(), "gof_singleton")
        .await
        .expect("get_pattern by slug");
    let pid = any_pattern_id(db.pool()).await;
    let _ = patterns::get_pattern(db.pool(), &pid.to_string())
        .await
        .expect("get_pattern by id");
}

#[tokio::test]
async fn patterns_get_pattern_sources_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let pid = any_pattern_id(db.pool()).await;
    let _ = patterns::get_pattern_sources(db.pool(), pid)
        .await
        .expect("get_pattern_sources");
}

#[tokio::test]
async fn patterns_get_source_excerpts_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let sid = patterns::upsert_source(db.pool(), fake_source("test-excerpt-source"))
        .await
        .expect("source");
    let _ = patterns::get_source_excerpts(db.pool(), sid, 5)
        .await
        .expect("get_source_excerpts");
}

#[tokio::test]
async fn patterns_catalog_stats_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = patterns::catalog_stats(db.pool())
        .await
        .expect("catalog_stats");
}

#[tokio::test]
async fn patterns_count_patterns_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = patterns::count_patterns(db.pool())
        .await
        .expect("count_patterns");
}

#[tokio::test]
async fn patterns_start_and_finish_import_run_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let run_id = patterns::start_import_run(db.pool(), "seed_only", None)
        .await
        .expect("start_import_run");
    patterns::finish_import_run(db.pool(), run_id, "ok", 0, 0, 0, None)
        .await
        .expect("finish_import_run");
}
