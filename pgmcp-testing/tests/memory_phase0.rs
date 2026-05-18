//! Phase 0 memory-server integration tests.
//!
//! Exercises the three quick-win deliverables from
//! `docs/memory-server/02-phases.md` Phase 0:
//!
//! - `recall_prompts` MCP tool — vector search over `session_prompts`
//!   surfaces the existing embedding column that previously had zero
//!   readers.
//! - `search_mandates` MCP tool — full-text search over
//!   `durable_mandates`, which previously had only a project-scope dump.
//! - Mandate supersession — `mark_near_duplicate_superseded` marks active
//!   mandates with `lower(imperative)` Levenshtein ≤ N as `superseded`,
//!   keeping the active list scannable across reinforcements.
//!
//! Skips cleanly with `SKIPPED:` if no test DB is configured.

use pgmcp::sessions::{self, MandatePolarity};
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

async fn seed_prompt(
    pool: &sqlx::PgPool,
    session_id: Uuid,
    cwd: &str,
    text: &str,
    embedding: &[f32],
) -> i64 {
    sessions::upsert_session(pool, session_id, cwd, None)
        .await
        .expect("upsert_session");
    let sha256 = sessions::prompt_sha256(text);
    sessions::insert_prompt(pool, session_id, text, &sha256, Some(embedding))
        .await
        .expect("insert_prompt")
}

#[tokio::test(flavor = "multi_thread")]
async fn recall_prompts_returns_top_k_by_vector_similarity() {
    let db = require_test_db!();
    let pool = db.pool();
    let session_id = Uuid::new_v4();

    // Two prompts with deterministic embeddings: the "target" sits on a
    // distinctive axis so it's easy to query for.
    let target_text = "let's investigate the auth refactor compliance requirements";
    let target_embedding: Vec<f32> = (0..384)
        .map(|i| if i == 7 { 1.0_f32 } else { 0.0 })
        .collect();
    seed_prompt(
        pool,
        session_id,
        "/ws/recall-target",
        target_text,
        &target_embedding,
    )
    .await;

    let distractor_text = "totally unrelated question about cron schedules";
    let distractor_embedding: Vec<f32> = (0..384)
        .map(|i| if i == 42 { 1.0_f32 } else { 0.0 })
        .collect();
    seed_prompt(
        pool,
        session_id,
        "/ws/recall-target",
        distractor_text,
        &distractor_embedding,
    )
    .await;

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "recall_prompts",
            serde_json::json!({
                "query": "auth refactor compliance",
                "session": session_id.to_string(),
                "limit": 5
            }),
        )
        .await
        .expect("recall_prompts call");
    let body = extract_json(&result);

    let count = body.get("count").and_then(Value::as_i64).unwrap_or(-1);
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .expect("results array present");

    // At least one of our seeded prompts must come back.
    assert!(count >= 1, "expected ≥1 recalled prompt, body={body}");
    assert!(
        results.iter().any(
            |r| r.get("prompt_text").and_then(Value::as_str) == Some(target_text)
                || r.get("prompt_text").and_then(Value::as_str) == Some(distractor_text)
        ),
        "expected one of the seeded prompts in {results:?}"
    );

    // Every recalled row must carry a similarity score.
    for r in results {
        assert!(
            r.get("similarity").and_then(Value::as_f64).is_some(),
            "every recalled prompt should have a similarity score: {r}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn recall_prompts_filters_by_session() {
    let db = require_test_db!();
    let pool = db.pool();

    let session_a = Uuid::new_v4();
    let session_b = Uuid::new_v4();
    let embedding: Vec<f32> = (0..384)
        .map(|i| if i == 11 { 1.0_f32 } else { 0.0 })
        .collect();

    seed_prompt(pool, session_a, "/ws/sess-a", "fact about A", &embedding).await;
    seed_prompt(pool, session_b, "/ws/sess-b", "fact about B", &embedding).await;

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "recall_prompts",
            serde_json::json!({
                "query": "fact about",
                "session": session_a.to_string(),
                "limit": 50
            }),
        )
        .await
        .expect("recall_prompts call");
    let body = extract_json(&result);
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .expect("results array present");

    // No row should belong to the unrelated session.
    for r in results {
        let sid = r
            .get("session_id")
            .and_then(Value::as_str)
            .expect("session_id present")
            .to_string();
        assert_ne!(
            sid,
            session_b.to_string(),
            "session-filter leak: row from session_b in results {results:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn search_mandates_fts_matches_imperative_text() {
    let db = require_test_db!();
    let pool = db.pool();

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET name = $3 RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/search-mandates-test")
    .bind("search-mandates-test")
    .fetch_one(pool)
    .await
    .expect("project");

    // Insert two durable mandates directly (bypassing promotion to keep
    // the test focused on the search surface).
    sqlx::query(
        "INSERT INTO durable_mandates (scope, project_id, polarity, imperative, target, source_mandate_id, file_path)
         VALUES ('project', $1, 'never', 'use unwrap on Option in production code', NULL, NULL, NULL)",
    )
    .bind(project_id)
    .execute(pool)
    .await
    .expect("insert mandate A");

    sqlx::query(
        "INSERT INTO durable_mandates (scope, project_id, polarity, imperative, target, source_mandate_id, file_path)
         VALUES ('project', $1, 'prefer', 'cargo clippy --all-targets in CI', NULL, NULL, NULL)",
    )
    .bind(project_id)
    .execute(pool)
    .await
    .expect("insert mandate B");

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "search_mandates",
            serde_json::json!({
                "query": "unwrap",
                "project_id": project_id,
                "limit": 5
            }),
        )
        .await
        .expect("search_mandates call");
    let body = extract_json(&result);

    let count = body.get("count").and_then(Value::as_i64).unwrap_or(-1);
    assert!(count >= 1, "expected ≥1 match for 'unwrap', body={body}");
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .expect("results array present");
    assert!(
        results.iter().any(|r| r
            .get("imperative")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("unwrap")),
        "expected the unwrap mandate in {results:?}"
    );
    assert_eq!(body.get("mode").and_then(Value::as_str), Some("fts"));
}

#[tokio::test(flavor = "multi_thread")]
async fn search_mandates_polarity_filter_applies() {
    let db = require_test_db!();
    let pool = db.pool();

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET name = $3 RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/polarity-filter-test")
    .bind("polarity-filter-test")
    .fetch_one(pool)
    .await
    .expect("project");

    sqlx::query(
        "INSERT INTO durable_mandates (scope, project_id, polarity, imperative, target, source_mandate_id, file_path)
         VALUES ('project', $1, 'never', 'commit untested migrations', NULL, NULL, NULL),
                ('project', $1, 'prefer', 'commit small focused diffs',  NULL, NULL, NULL)",
    )
    .bind(project_id)
    .execute(pool)
    .await
    .expect("insert mandates");

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "search_mandates",
            serde_json::json!({
                "query": "commit",
                "project_id": project_id,
                "polarity": "never",
                "limit": 20
            }),
        )
        .await
        .expect("search_mandates call");
    let body = extract_json(&result);
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .expect("results array");
    assert!(!results.is_empty(), "expected ≥1 'never'-polarity match");
    for r in results {
        assert_eq!(
            r.get("polarity").and_then(Value::as_str),
            Some("never"),
            "polarity filter leak: {r}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn search_mandates_rejects_invalid_polarity() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let err = server
        .call_tool_cli(
            "search_mandates",
            serde_json::json!({
                "query": "test",
                "polarity": "not-a-real-polarity"
            }),
        )
        .await;
    // Either the tool returns an error, or it returns `is_error=true`. Both
    // are acceptable invalid-parameter signals.
    match err {
        Err(_) => {}
        Ok(result) => {
            assert_eq!(
                result.is_error,
                Some(true),
                "expected error or is_error=true, got {result:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mark_near_duplicate_superseded_collapses_edit_distance_3() {
    let db = require_test_db!();
    let pool = db.pool();
    let session_id = Uuid::new_v4();
    sessions::upsert_session(pool, session_id, "/ws/dedupe-test", None)
        .await
        .expect("upsert_session");

    // Insert the prompt row first so source_prompt_id is satisfiable.
    let sha256 = sessions::prompt_sha256("dedup test prompt");
    let prompt_id = sessions::insert_prompt(pool, session_id, "dedup test prompt", &sha256, None)
        .await
        .expect("insert_prompt");

    // Insert two active mandates with `lower(imperative)` edit distance 1.
    let m_a = sessions::ExtractedMandate {
        polarity: MandatePolarity::Never,
        imperative: "use unwrap".into(),
        target: None,
        cwd_prefix: None,
        cue_tier: sessions::CueTier::A,
        salience: 1.0,
    };
    let m_b = sessions::ExtractedMandate {
        polarity: MandatePolarity::Never,
        imperative: "use unwraps".into(), // edit distance 1 from "use unwrap"
        target: None,
        cwd_prefix: None,
        cue_tier: sessions::CueTier::A,
        salience: 1.0,
    };
    let a_id = sessions::upsert_mandate(pool, session_id, prompt_id, &m_a)
        .await
        .expect("upsert a");
    let b_id = sessions::upsert_mandate(pool, session_id, prompt_id, &m_b)
        .await
        .expect("upsert b");
    assert_ne!(
        a_id, b_id,
        "different lower(imperative) should yield separate rows"
    );

    // Now mark near-dups of b_id within distance 3.
    let count =
        sessions::mark_near_duplicate_superseded(pool, session_id, b_id, "never", "use unwraps", 3)
            .await
            .expect("mark_near_duplicate_superseded");
    assert!(count >= 1, "expected to mark ≥1 row, got {count}");

    // Verify a_id is now superseded and b_id is still active.
    let a_status: String = sqlx::query_scalar("SELECT status FROM session_mandates WHERE id = $1")
        .bind(a_id)
        .fetch_one(pool)
        .await
        .expect("status a");
    let b_status: String = sqlx::query_scalar("SELECT status FROM session_mandates WHERE id = $1")
        .bind(b_id)
        .fetch_one(pool)
        .await
        .expect("status b");
    assert_eq!(a_status, "superseded", "older a should be superseded");
    assert_eq!(b_status, "active", "keeper b should stay active");
}

#[tokio::test(flavor = "multi_thread")]
async fn mark_near_duplicate_superseded_ignores_distant_imperatives() {
    let db = require_test_db!();
    let pool = db.pool();
    let session_id = Uuid::new_v4();
    sessions::upsert_session(pool, session_id, "/ws/no-dedupe", None)
        .await
        .expect("upsert_session");

    let sha256 = sessions::prompt_sha256("distant test");
    let prompt_id = sessions::insert_prompt(pool, session_id, "distant test", &sha256, None)
        .await
        .expect("insert_prompt");

    let m_a = sessions::ExtractedMandate {
        polarity: MandatePolarity::Never,
        imperative: "use unwrap".into(),
        target: None,
        cwd_prefix: None,
        cue_tier: sessions::CueTier::A,
        salience: 1.0,
    };
    let m_b = sessions::ExtractedMandate {
        polarity: MandatePolarity::Never,
        imperative: "skip integration tests".into(),
        target: None,
        cwd_prefix: None,
        cue_tier: sessions::CueTier::A,
        salience: 1.0,
    };
    let a_id = sessions::upsert_mandate(pool, session_id, prompt_id, &m_a)
        .await
        .expect("upsert a");
    let b_id = sessions::upsert_mandate(pool, session_id, prompt_id, &m_b)
        .await
        .expect("upsert b");

    let count = sessions::mark_near_duplicate_superseded(
        pool,
        session_id,
        b_id,
        "never",
        "skip integration tests",
        3,
    )
    .await
    .expect("mark_near_duplicate_superseded");
    assert_eq!(
        count, 0,
        "edit distance >> 3 should not be marked: marked {count}"
    );

    let a_status: String = sqlx::query_scalar("SELECT status FROM session_mandates WHERE id = $1")
        .bind(a_id)
        .fetch_one(pool)
        .await
        .expect("status a");
    assert_eq!(a_status, "active", "distant mandate must stay active");
}

#[tokio::test(flavor = "multi_thread")]
async fn mark_near_duplicate_respects_polarity_separation() {
    let db = require_test_db!();
    let pool = db.pool();
    let session_id = Uuid::new_v4();
    sessions::upsert_session(pool, session_id, "/ws/polarity-sep", None)
        .await
        .expect("upsert_session");

    let sha256 = sessions::prompt_sha256("polarity test");
    let prompt_id = sessions::insert_prompt(pool, session_id, "polarity test", &sha256, None)
        .await
        .expect("insert_prompt");

    // Same imperative, opposite polarities: NEVER and PREFER should not
    // be near-duplicates of each other, even though the text is identical.
    let m_never = sessions::ExtractedMandate {
        polarity: MandatePolarity::Never,
        imperative: "use unwrap".into(),
        target: None,
        cwd_prefix: None,
        cue_tier: sessions::CueTier::A,
        salience: 1.0,
    };
    let m_prefer = sessions::ExtractedMandate {
        polarity: MandatePolarity::Prefer,
        imperative: "use unwrap".into(),
        target: None,
        cwd_prefix: None,
        cue_tier: sessions::CueTier::A,
        salience: 1.0,
    };
    let never_id = sessions::upsert_mandate(pool, session_id, prompt_id, &m_never)
        .await
        .expect("upsert never");
    let prefer_id = sessions::upsert_mandate(pool, session_id, prompt_id, &m_prefer)
        .await
        .expect("upsert prefer");

    // Dedupe should not see the opposite-polarity row even though the text
    // matches exactly.
    let count = sessions::mark_near_duplicate_superseded(
        pool,
        session_id,
        prefer_id,
        "prefer",
        "use unwrap",
        3,
    )
    .await
    .expect("mark_near_duplicate_superseded");
    assert_eq!(count, 0, "polarity should isolate dedupe scope");

    let never_status: String =
        sqlx::query_scalar("SELECT status FROM session_mandates WHERE id = $1")
            .bind(never_id)
            .fetch_one(pool)
            .await
            .expect("status never");
    assert_eq!(
        never_status, "active",
        "opposite-polarity row must remain untouched"
    );
}
