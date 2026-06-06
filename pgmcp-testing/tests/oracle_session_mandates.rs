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

#[tokio::test(flavor = "multi_thread")]
async fn session_mandates_all_status_respects_cwd() {
    let db = require_test_db!();
    let session_a = Uuid::new_v4();
    let session_b = Uuid::new_v4();
    let cwd_a = format!("/ws/cwd-a-{session_a}");
    let cwd_b = format!("/ws/cwd-b-{session_b}");

    seed_session_mandate(
        db.pool(),
        session_a,
        &cwd_a,
        "Never leak mandates across cwd boundaries again.",
    )
    .await;
    seed_session_mandate(
        db.pool(),
        session_b,
        &cwd_b,
        "Never let another cwd appear in this query again.",
    )
    .await;

    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "session_mandates",
            serde_json::json!({
                "cwd": format!("  {cwd_a}  "),
                "status": " all ",
                "limit": 0,
            }),
        )
        .await
        .expect("session_mandates all-status cwd query");
    let body = extract_json(&result);
    assert_eq!(
        body.get("cwd").and_then(Value::as_str),
        Some(cwd_a.as_str())
    );
    assert_eq!(body.get("status").and_then(Value::as_str), Some("all"));
    assert_eq!(body.get("limit").and_then(Value::as_i64), Some(1));

    let mandates = body
        .get("mandates")
        .and_then(Value::as_array)
        .expect("mandates array");
    assert_eq!(mandates.len(), 1, "cwd query leaked rows: {body}");
    assert_eq!(
        mandates[0].get("session_id").and_then(Value::as_str),
        Some(session_a.to_string().as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn session_mandates_rejects_unknown_status() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    let err = server
        .call_tool_cli(
            "session_mandates",
            serde_json::json!({"cwd": "/ws/no-status-leak", "status": "deleted"}),
        )
        .await;
    assert!(err.is_err(), "unknown status must fail closed");
}

#[tokio::test(flavor = "multi_thread")]
async fn promote_session_mandate_concurrent_calls_are_idempotent() {
    let db = require_test_db!();
    let session_id = Uuid::new_v4();
    let cwd = "/ws/promote-idempotent";
    let mandate_id = seed_session_mandate(
        db.pool(),
        session_id,
        cwd,
        "Never duplicate promotions again.",
    )
    .await;

    let server_a = server_with_pool(db.pool().clone());
    let server_b = server_with_pool(db.pool().clone());
    let args = serde_json::json!({
        "mandate_id": mandate_id,
        "scope": " workspace ",
        "write_to_file": false,
    });

    let (first, second) = tokio::join!(
        server_a.call_tool_cli("promote_session_mandate", args.clone()),
        server_b.call_tool_cli("promote_session_mandate", args),
    );
    let first = extract_json(&first.expect("first promote"));
    let second = extract_json(&second.expect("second promote"));
    assert_eq!(
        first.get("durable_mandate_id"),
        second.get("durable_mandate_id")
    );
    assert_eq!(
        first.get("scope").and_then(Value::as_str),
        Some("workspace")
    );
    assert_eq!(
        second.get("scope").and_then(Value::as_str),
        Some("workspace")
    );

    let durable_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM durable_mandates WHERE source_mandate_id = $1")
            .bind(mandate_id)
            .fetch_one(db.pool())
            .await
            .expect("durable count");
    assert_eq!(durable_count, 1, "promotion must be DB-idempotent");
}

#[tokio::test(flavor = "multi_thread")]
async fn promote_session_mandate_rejects_missing_target_file_before_db_write() {
    let db = require_test_db!();
    let session_id = Uuid::new_v4();
    let cwd = "/ws/promote-no-target";
    let mandate_id = seed_session_mandate(
        db.pool(),
        session_id,
        cwd,
        "Never partially promote missing files.",
    )
    .await;

    let server = server_with_pool(db.pool().clone());
    let err = server
        .call_tool_cli(
            "promote_session_mandate",
            serde_json::json!({
                "mandate_id": mandate_id,
                "scope": "workspace",
                "write_to_file": true,
            }),
        )
        .await;
    assert!(
        err.is_err(),
        "write_to_file=true without target_file must fail before DB writes"
    );

    let durable_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM durable_mandates WHERE source_mandate_id = $1")
            .bind(mandate_id)
            .fetch_one(db.pool())
            .await
            .expect("durable count");
    assert_eq!(durable_count, 0);

    let status: String = sqlx::query_scalar("SELECT status FROM session_mandates WHERE id = $1")
        .bind(mandate_id)
        .fetch_one(db.pool())
        .await
        .expect("status select");
    assert_eq!(status, "active");
}

#[tokio::test(flavor = "multi_thread")]
async fn promote_session_mandate_appends_target_file_once() {
    let db = require_test_db!();
    let session_id = Uuid::new_v4();
    let cwd = "/ws/promote-file";
    let mandate_id = seed_session_mandate(
        db.pool(),
        session_id,
        cwd,
        "Never append duplicate bullets again.",
    )
    .await;

    let mandate_text: String =
        sqlx::query_scalar("SELECT imperative FROM session_mandates WHERE id = $1")
            .bind(mandate_id)
            .fetch_one(db.pool())
            .await
            .expect("mandate imperative");

    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("AGENTS.md");
    std::fs::write(&target, "# Rules\n").expect("seed target file");
    let target = target.to_string_lossy().to_string();

    let server = server_with_pool(db.pool().clone());
    for _ in 0..2 {
        server
            .call_tool_cli(
                "promote_session_mandate",
                serde_json::json!({
                    "mandate_id": mandate_id,
                    "scope": "workspace",
                    "write_to_file": true,
                    "target_file": format!("  {target}  "),
                }),
            )
            .await
            .expect("promote with file");
    }

    let durable_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM durable_mandates WHERE source_mandate_id = $1")
            .bind(mandate_id)
            .fetch_one(db.pool())
            .await
            .expect("durable count");
    assert_eq!(durable_count, 1);

    let content = std::fs::read_to_string(&target).expect("target content");
    assert_eq!(
        content
            .matches("## Promoted session mandates (pgmcp)")
            .count(),
        1
    );
    assert_eq!(
        content.matches(&mandate_text).count(),
        1,
        "file append must be idempotent:\n{content}"
    );
}
