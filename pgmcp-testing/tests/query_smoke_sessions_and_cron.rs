//! Layer C of the integration-test plan: SQL-execution smoke tests for
//! the session/mandates module (`src/sessions.rs`) and the
//! telemetry-retention cron (`src/cron/telemetry_retention.rs`).
//!
//! These are the two query sets not directly invoked by MCP tools
//! (covered by Layer A) and not in `src/db/queries.rs` (covered by
//! Layer B). Without these tests, a column-name drift bug in
//! `session_mandates`, `durable_mandates`, `session_prompts`, or
//! `mcp_tool_calls` would slip through `scripts/verify.sh`.

mod common;

use std::sync::Arc;

use pgmcp::cron::telemetry_retention;
use pgmcp::sessions::{self, ExtractedMandate, MandatePolarity};
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::fixtures::synthetic_corpus::SyntheticCorpus;
use pgmcp_testing::require_test_db;
use sqlx::PgPool;
use uuid::Uuid;

// =============================================================================
// Helpers
// =============================================================================

fn fake_mandate(text: &str) -> ExtractedMandate {
    ExtractedMandate {
        polarity: MandatePolarity::Remember,
        imperative: text.to_string(),
        target: None,
        cwd_prefix: None,
        cue_tier: pgmcp::sessions::CueTier::C,
        salience: 0.5,
    }
}

/// Seed a session row so the FK from session_prompts / session_mandates
/// passes; return the session UUID.
async fn seed_session(pool: &PgPool, cwd: &str, project_id: Option<i32>) -> Uuid {
    let id = Uuid::now_v7();
    sessions::upsert_session(pool, id, cwd, project_id)
        .await
        .expect("upsert_session");
    id
}

/// Seed a prompt so the FK from session_mandates.source_prompt_id passes.
async fn seed_prompt(pool: &PgPool, session_id: Uuid) -> i64 {
    sessions::insert_prompt(pool, session_id, "test prompt", "deadbeef", None)
        .await
        .expect("insert_prompt")
}

// =============================================================================
// sessions.rs — pure helpers (no DB)
// =============================================================================

#[test]
fn sessions_extract_mandates_returns_typed_vec() {
    let mandates = sessions::extract_mandates("Never deploy on Friday", None);
    // We don't assert specific extraction quality (that's covered by
    // other tests); only that the function runs and returns a Vec.
    let _ = mandates.len();
}

#[test]
fn sessions_render_session_mandates_md_runs() {
    let s = sessions::render_session_mandates_md(&[], 4096);
    let _ = s.len();
}

#[test]
fn sessions_prompt_sha256_returns_hex() {
    let h = sessions::prompt_sha256("hello");
    assert_eq!(h.len(), 64, "sha256 hex must be 64 chars");
    assert!(
        h.chars().all(|c| c.is_ascii_hexdigit()),
        "sha256 must be all hex"
    );
}

// =============================================================================
// sessions.rs — DB-backed (8 functions)
// =============================================================================

#[tokio::test]
async fn sessions_upsert_session_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = seed_session(db.pool(), "/ws/auth/proj-auth", None).await;
}

#[tokio::test]
async fn sessions_insert_prompt_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let sid = seed_session(db.pool(), "/ws/auth/proj-auth", None).await;
    let _ = seed_prompt(db.pool(), sid).await;
}

#[tokio::test]
async fn sessions_upsert_mandate_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let sid = seed_session(db.pool(), "/ws/auth/proj-auth", None).await;
    let pid = seed_prompt(db.pool(), sid).await;
    let _ = sessions::upsert_mandate(db.pool(), sid, pid, &fake_mandate("test"))
        .await
        .expect("upsert_mandate");
}

#[tokio::test]
async fn sessions_list_active_mandates_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let sid = seed_session(db.pool(), "/ws/auth/proj-auth", None).await;
    let _ = sessions::list_active_mandates(db.pool(), Some(sid), None, 10)
        .await
        .expect("list_active_mandates");
    // Also test the cwd path.
    let _ = sessions::list_active_mandates(db.pool(), None, Some("/ws/auth/proj-auth"), 10)
        .await
        .expect("list_active_mandates by cwd");
}

#[tokio::test]
async fn sessions_retire_mandate_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let sid = seed_session(db.pool(), "/ws/auth/proj-auth", None).await;
    let pid = seed_prompt(db.pool(), sid).await;
    let mid = sessions::upsert_mandate(db.pool(), sid, pid, &fake_mandate("retire-me"))
        .await
        .expect("upsert");
    sessions::retire_mandate(db.pool(), mid)
        .await
        .expect("retire_mandate");
}

#[tokio::test]
async fn sessions_get_mandate_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let sid = seed_session(db.pool(), "/ws/auth/proj-auth", None).await;
    let pid = seed_prompt(db.pool(), sid).await;
    let mid = sessions::upsert_mandate(db.pool(), sid, pid, &fake_mandate("fetch-me"))
        .await
        .expect("upsert");
    let row = sessions::get_mandate(db.pool(), mid)
        .await
        .expect("get_mandate");
    assert!(
        row.is_some(),
        "get_mandate must return Some for inserted id"
    );
}

#[tokio::test]
async fn sessions_promote_mandate_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let sid = seed_session(db.pool(), "/ws/auth/proj-auth", Some(h.auth_project_id)).await;
    let pid = seed_prompt(db.pool(), sid).await;
    let mid = sessions::upsert_mandate(db.pool(), sid, pid, &fake_mandate("promote-me"))
        .await
        .expect("upsert");
    let _ = sessions::promote_mandate(db.pool(), mid, "project", Some(h.auth_project_id), None)
        .await
        .expect("promote_mandate");
}

#[tokio::test]
async fn sessions_list_durable_mandates_for_project_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = sessions::list_durable_mandates_for_project(db.pool(), h.auth_project_id)
        .await
        .expect("list_durable_mandates_for_project");
}

// =============================================================================
// cron::telemetry_retention (2 functions)
// =============================================================================

#[tokio::test]
async fn telemetry_retention_run_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let stats = StatsTracker::new();
    // No rows in mcp_tool_calls in fresh test DB → expect 0 deletions
    // without error. The point is the DELETE statement parses and the
    // function returns Ok.
    let deleted = telemetry_retention::run_telemetry_retention(db.pool(), &stats, 30)
        .await
        .expect("run_telemetry_retention");
    assert_eq!(deleted, 0, "fresh DB has no telemetry rows to purge");
}

#[tokio::test]
async fn telemetry_retention_run_or_log_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let pool = Arc::new(db.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    // run_or_log swallows errors via warn!(); call only proves the
    // wrapper compiles and runs to completion.
    telemetry_retention::run_or_log(pool, stats, 30).await;
}

// =============================================================================
// End-to-end telemetry — seed a row, then verify the retention SQL
// actually deletes it under a small horizon.
// =============================================================================

#[tokio::test]
async fn telemetry_retention_deletes_old_rows() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    // Plant a stale telemetry row dated 7 days ago.
    sqlx::query(
        "INSERT INTO mcp_tool_calls (ts, tool, client_name, duration_ms, outcome) \
         VALUES (now() - interval '7 days', 'fake_tool', 'test', 1, 'ok')",
    )
    .execute(db.pool())
    .await
    .expect("insert stale row");

    let stats = StatsTracker::new();
    // 1-day horizon → the 7-day-old row must be deleted.
    let deleted = telemetry_retention::run_telemetry_retention(db.pool(), &stats, 1)
        .await
        .expect("retention");
    assert_eq!(deleted, 1, "retention must delete the stale row");
}
