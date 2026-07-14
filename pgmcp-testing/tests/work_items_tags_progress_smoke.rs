//! Integration smoke tests for the work-item tracker **Phase 2** MCP tools
//! (tags + progress). Exercises all eight new tools end-to-end against real
//! Postgres:
//!   tag_create → tag_list → work_item_tag (auto-create + skip) →
//!   work_item_untag → tag_rename → tag_merge, and
//!   work_item_record_progress → work_item_progress_log.
//!
//! Self-skips (via `require_test_db!`) when `PGMCP_TEST_DATABASE_URL` is unset,
//! so it stays green for contributors without a local Postgres+pgvector — while
//! still satisfying `query_inventory_vs_coverage` (which greps these source
//! files for a `call_tool_cli("<tool>", …)` per dispatched tool).
//!
//! Uses the same local 1024-d deterministic embedder as the Phase-1 harness
//! (`work_items.embedding` is `vector(1024)`); the dimension is not load-bearing
//! for the tag/progress side tables, but keeps the harness identical.

use std::sync::Arc;

use crate::common::text_of;
use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use sqlx::PgPool;

/// Server with a real pool and a 1024-d deterministic embedder (matches the
/// `work_items.embedding vector(1024)` column).
fn server_1024(pool: PgPool) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(pool);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(Config::default()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
    let embed_source = EmbedSource::backend(embed_backend);
    let lifecycle = pgmcp::daemon_state::DaemonLifecycle::new();
    lifecycle.transition(pgmcp::daemon_state::DaemonPhase::Ready);
    let ctx = SystemContext::production(
        db,
        embed_source,
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    );
    McpServer::new(ctx)
}

/// Pull `public_id` out of a top-level work-item row body.
fn public_id_of(v: &Value) -> String {
    v["public_id"]
        .as_str()
        .expect("row carries a public_id")
        .to_string()
}

#[tokio::test]
async fn tags_and_progress_full_round_trip() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    // ── seed a work item to attach tags / progress to ──
    let item = server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": "Phase 2 tag+progress probe", "priority": 3 }),
        )
        .await
        .expect("work_item_create must succeed");
    let iv: Value = serde_json::from_str(&text_of(&item)).expect("item body JSON");
    let item_id = public_id_of(&iv);

    // ── tag_create (upsert with color + description) ──
    let created = server
        .call_tool_cli(
            "tag_create",
            json!({ "name": "Urgent", "color": "red", "description": "needs attention now" }),
        )
        .await
        .expect("tag_create must succeed");
    let cv: Value = serde_json::from_str(&text_of(&created)).expect("tag body JSON");
    assert_eq!(
        cv["slug"].as_str(),
        Some("urgent"),
        "slug derived from name"
    );
    assert_eq!(cv["color"].as_str(), Some("red"));
    assert!(cv["merged_into"].is_null(), "a fresh tag is active");

    // An empty tag name is rejected.
    assert!(
        server
            .call_tool_cli("tag_create", json!({ "name": "   " }))
            .await
            .is_err(),
        "an empty tag name must be rejected"
    );

    // ── tag_list (active only) includes our tag ──
    let listed = server
        .call_tool_cli("tag_list", json!({}))
        .await
        .expect("tag_list must succeed");
    let lv: Value = serde_json::from_str(&text_of(&listed)).expect("tag_list body JSON");
    let tags = lv.as_array().expect("tag_list returns an array");
    assert!(
        tags.iter().any(|t| t["slug"].as_str() == Some("urgent")),
        "the active tag list contains 'urgent'"
    );

    // ── work_item_tag: attach an existing tag + auto-create a new one ──
    let tagged = server
        .call_tool_cli(
            "work_item_tag",
            json!({ "public_id": item_id, "tags": ["Urgent", "Tech Debt"] }),
        )
        .await
        .expect("work_item_tag must succeed");
    let tv: Value = serde_json::from_str(&text_of(&tagged)).expect("work_item_tag body JSON");
    let applied = tv["applied"].as_array().expect("applied is an array");
    assert_eq!(applied.len(), 2, "both tags applied");
    assert!(
        applied.iter().any(|s| s.as_str() == Some("tech-debt")),
        "the new tag was auto-created and slugified"
    );
    assert_eq!(
        tv["skipped"].as_array().map(|a| a.len()),
        Some(0),
        "nothing skipped when auto_create defaults true"
    );
    let item_tags = tv["tags"].as_array().expect("tags echoed back");
    assert_eq!(item_tags.len(), 2, "item now carries two tags");

    // ── work_item_tag with auto_create=false skips unknown tags ──
    let partial = server
        .call_tool_cli(
            "work_item_tag",
            json!({
                "public_id": item_id,
                "tags": ["never-seen-tag"],
                "auto_create": false,
            }),
        )
        .await
        .expect("work_item_tag (no auto_create) must succeed");
    let pv: Value = serde_json::from_str(&text_of(&partial)).expect("partial body JSON");
    assert_eq!(
        pv["applied"].as_array().map(|a| a.len()),
        Some(0),
        "unknown tag is not applied when auto_create=false"
    );
    assert_eq!(
        pv["skipped"].as_array().map(|a| a.len()),
        Some(1),
        "unknown tag is reported as skipped"
    );

    // ── work_item_untag: remove one tag ──
    let removed = server
        .call_tool_cli(
            "work_item_untag",
            json!({ "public_id": item_id, "tag": "Tech Debt" }),
        )
        .await
        .expect("work_item_untag must succeed");
    let rv: Value = serde_json::from_str(&text_of(&removed)).expect("untag body JSON");
    assert_eq!(
        rv["removed"].as_bool(),
        Some(true),
        "the pairing was removed"
    );

    // Untagging an unknown tag is an error.
    assert!(
        server
            .call_tool_cli(
                "work_item_untag",
                json!({ "public_id": item_id, "tag": "no-such-tag-xyz" }),
            )
            .await
            .is_err(),
        "untag of an unknown tag must be rejected"
    );

    // ── tag_rename: keep the slug, change the display name ──
    let renamed = server
        .call_tool_cli(
            "tag_rename",
            json!({ "slug": "urgent", "new_name": "Top Priority" }),
        )
        .await
        .expect("tag_rename must succeed");
    let rnv: Value = serde_json::from_str(&text_of(&renamed)).expect("rename body JSON");
    assert_eq!(rnv["name"].as_str(), Some("Top Priority"));
    assert_eq!(rnv["slug"].as_str(), Some("urgent"), "slug is preserved");

    // Renaming a missing tag is an error.
    assert!(
        server
            .call_tool_cli(
                "tag_rename",
                json!({ "slug": "does-not-exist", "new_name": "x" }),
            )
            .await
            .is_err(),
        "rename of a missing tag must be rejected"
    );

    // ── tag_merge: fold a second tag into 'urgent' ──
    // Create a synonym, attach it to the item, then merge it into 'urgent'.
    server
        .call_tool_cli("tag_create", json!({ "name": "hot" }))
        .await
        .expect("tag_create (hot) must succeed");
    server
        .call_tool_cli(
            "work_item_tag",
            json!({ "public_id": item_id, "tags": ["hot"] }),
        )
        .await
        .expect("work_item_tag (hot) must succeed");
    let merged = server
        .call_tool_cli("tag_merge", json!({ "src": "hot", "dst": "Urgent" }))
        .await
        .expect("tag_merge must succeed");
    let mv: Value = serde_json::from_str(&text_of(&merged)).expect("merge body JSON");
    assert_eq!(mv["into"].as_str(), Some("urgent"), "merge target slug");
    assert!(
        mv["merged"].as_u64().unwrap_or(0) >= 1,
        "at least one assignment was repointed"
    );

    // Merge of an unknown tag is invalid_params.
    assert!(
        server
            .call_tool_cli("tag_merge", json!({ "src": "ghost", "dst": "urgent" }))
            .await
            .is_err(),
        "merge with an unknown source tag must be rejected"
    );

    // ── work_item_record_progress: note + percent updates claimed_percent ──
    let prog = server
        .call_tool_cli(
            "work_item_record_progress",
            json!({ "public_id": item_id, "note": "wired the dispatch arms", "percent": 40 }),
        )
        .await
        .expect("work_item_record_progress must succeed");
    let pgv: Value = serde_json::from_str(&text_of(&prog)).expect("progress body JSON");
    assert_eq!(pgv["note"].as_str(), Some("wired the dispatch arms"));
    assert_eq!(pgv["percent"].as_i64(), Some(40));
    assert_eq!(
        pgv["provenance"].as_str(),
        Some("agent_write"),
        "MCP-authored progress is always agent_write"
    );

    let high = server
        .call_tool_cli(
            "work_item_record_progress",
            json!({ "public_id": item_id, "note": "clamped high", "percent": 250 }),
        )
        .await
        .expect("work_item_record_progress clamps high percent");
    let highv: Value = serde_json::from_str(&text_of(&high)).expect("high progress body JSON");
    assert_eq!(
        highv["percent"].as_i64(),
        Some(100),
        "percent above 100 is clamped before insert"
    );

    let low = server
        .call_tool_cli(
            "work_item_record_progress",
            json!({ "public_id": item_id, "note": "clamped low", "percent": -5 }),
        )
        .await
        .expect("work_item_record_progress clamps low percent");
    let lowv: Value = serde_json::from_str(&text_of(&low)).expect("low progress body JSON");
    assert_eq!(
        lowv["percent"].as_i64(),
        Some(0),
        "percent below 0 is clamped before insert"
    );

    // A second note without a percent.
    server
        .call_tool_cli(
            "work_item_record_progress",
            json!({ "public_id": item_id, "note": "added smoke coverage" }),
        )
        .await
        .expect("work_item_record_progress (no percent) must succeed");

    // An empty note is rejected.
    assert!(
        server
            .call_tool_cli(
                "work_item_record_progress",
                json!({ "public_id": item_id, "note": "  " }),
            )
            .await
            .is_err(),
        "an empty progress note must be rejected"
    );

    // The claimed_percent on the item now reflects the latest reported percent.
    let after = server
        .call_tool_cli("work_item_get", json!({ "public_id": item_id }))
        .await
        .expect("work_item_get must succeed");
    let av: Value = serde_json::from_str(&text_of(&after)).expect("get body JSON");
    assert_eq!(
        av["item"]["claimed_percent"].as_i64(),
        Some(0),
        "claimed_percent updated from the latest percent-bearing note"
    );

    // ── work_item_progress_log: newest first, all notes present ──
    let log = server
        .call_tool_cli(
            "work_item_progress_log",
            json!({ "public_id": item_id, "limit": 10 }),
        )
        .await
        .expect("work_item_progress_log must succeed");
    let logv: Value = serde_json::from_str(&text_of(&log)).expect("log body JSON");
    let entries = logv.as_array().expect("progress log is an array");
    assert_eq!(entries.len(), 4, "four progress notes were recorded");
    assert_eq!(
        entries[0]["note"].as_str(),
        Some("added smoke coverage"),
        "newest note first"
    );

    // Progress log for a missing item is an error.
    assert!(
        server
            .call_tool_cli(
                "work_item_progress_log",
                json!({ "public_id": "missing-item-000000" }),
            )
            .await
            .is_err(),
        "progress log of a missing item must be rejected"
    );
}
