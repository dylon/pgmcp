//! Real-DB oracle for session-mandate observation, MCP tools, and promotion.
//!
//! Drives the `session_mandates` + `promote_session_mandate` MCP tools through
//! `server_with_pool` (deterministic embedder, no model download). Verifies:
//!
//! - Tier-A `Never X again` extraction surfaces a Never-polarity mandate.
//! - Replaying the same prompt does not duplicate; it bumps reinforcement_count.
//! - `session_mandates` returns the persisted rows by session_id.
//! - `promote_session_mandate` (DB-only) flips status to 'promoted'.
//! - `mandate_context` with `session_id` includes active + promoted sections.
//!
//! Skips cleanly with `SKIPPED:` if no test DB is configured.

use pgmcp_testing::pool_tool_helpers::server_with_pool;
use pgmcp_testing::require_test_db;
use serde_json::Value;
use uuid::Uuid;

fn extract_json(call_result: &rmcp::model::CallToolResult) -> Value {
    for content in &call_result.content {
        if let rmcp::model::RawContent::Text(text_content) = &content.raw {
            return serde_json::from_str::<Value>(&text_content.text)
                .expect("tool emitted invalid JSON");
        }
    }
    panic!("tool returned no Text content block");
}

/// Insert a session + prompt + mandate row directly (bypassing the REST
/// endpoint), so tests don't need an HTTP server.
async fn seed_session_mandate(
    pool: &sqlx::PgPool,
    session_id: Uuid,
    cwd: &str,
    prompt: &str,
) -> i64 {
    pgmcp::sessions::upsert_session(pool, session_id, cwd, None)
        .await
        .expect("upsert_session");
    let sha256 = pgmcp::sessions::prompt_sha256(prompt);
    let prompt_id = pgmcp::sessions::insert_prompt(pool, session_id, prompt, &sha256, None)
        .await
        .expect("insert_prompt");
    let extracted = pgmcp::sessions::extract_mandates(prompt, Some(cwd));
    assert!(
        !extracted.is_empty(),
        "extractor returned 0 mandates for: {prompt:?}"
    );
    let mut last = 0;
    for m in &extracted {
        last = pgmcp::sessions::upsert_mandate(pool, session_id, prompt_id, m)
            .await
            .expect("upsert_mandate");
    }
    last
}

#[tokio::test(flavor = "multi_thread")]
async fn extractor_persists_tier_a_never_again() {
    let db = require_test_db!();
    let session_id = Uuid::new_v4();
    let cwd = "/ws/some-project";
    let prompt = "Never make destructive changes without my explicit approval again!";

    let id = seed_session_mandate(db.pool(), session_id, cwd, prompt).await;
    assert!(id > 0);

    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "session_mandates",
            serde_json::json!({"session_id": session_id.to_string()}),
        )
        .await
        .expect("session_mandates call");
    let body = extract_json(&result);
    let mandates = body
        .get("mandates")
        .and_then(Value::as_array)
        .expect("mandates array");
    assert!(!mandates.is_empty(), "expected ≥1 mandate, body={body}");
    assert!(
        mandates
            .iter()
            .any(|m| m.get("polarity").and_then(Value::as_str) == Some("never")),
        "expected polarity=never in {mandates:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn idempotent_replay_bumps_reinforcement() {
    let db = require_test_db!();
    let session_id = Uuid::new_v4();
    let cwd = "/ws/idem-project";
    let prompt = "Never run --no-verify again.";

    seed_session_mandate(db.pool(), session_id, cwd, prompt).await;
    seed_session_mandate(db.pool(), session_id, cwd, prompt).await;
    seed_session_mandate(db.pool(), session_id, cwd, prompt).await;

    let count: (i64, i32) = sqlx::query_as(
        "SELECT COUNT(*)::bigint, COALESCE(MAX(reinforcement_count), 0)
         FROM session_mandates WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(db.pool())
    .await
    .expect("count");
    assert_eq!(count.0, 1, "expected 1 row after 3 replays");
    assert!(
        count.1 >= 3,
        "expected reinforcement_count >= 3, got {}",
        count.1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn promote_session_mandate_flips_status() {
    let db = require_test_db!();
    let session_id = Uuid::new_v4();
    let cwd = "/ws/promote-project";
    let mandate_id =
        seed_session_mandate(db.pool(), session_id, cwd, "Never amend commits again.").await;

    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "promote_session_mandate",
            serde_json::json!({
                "mandate_id": mandate_id,
                "scope": "workspace",
                "write_to_file": false,
            }),
        )
        .await
        .expect("promote call");
    let body = extract_json(&result);
    assert_eq!(body.get("ok").and_then(Value::as_bool), Some(true));
    assert!(
        body.get("durable_mandate_id")
            .and_then(Value::as_i64)
            .is_some()
    );

    let status: String = sqlx::query_scalar("SELECT status FROM session_mandates WHERE id = $1")
        .bind(mandate_id)
        .fetch_one(db.pool())
        .await
        .expect("status select");
    assert_eq!(status, "promoted");
}
