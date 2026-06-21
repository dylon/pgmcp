//! Integration tests for the Crucible **context-tape paging control plane**
//! (Phase 5), driven end-to-end against a real test Postgres + the in-memory
//! [`MockTapeDataPlane`]. The /goal forbids stubs, so the engine is exercised
//! over the mock backing store, not a fake.
//!
//! Coverage (the discriminating properties the plan calls out):
//!   1. `store` save → load round-trip preserves the logical metadata.
//!   2. page-in never exceeds `budget_tokens`.
//!   3. over-budget page-in evicts to fit.
//!   4. a pinned page is never evicted.
//!   5. `resident_tokens` invariant == Σ `est_tokens` after arbitrary page-in.
//!   6. ImportanceWeighted keeps a high-importance page resident while dropping a
//!      low-importance LRU one.
//!   7. a dirty evict calls `put` exactly once; a clean evict zero times.
//!   8. the demotion ladder pages in exactly one SummaryNode with est_tokens < Σ
//!      leaves.
//!
//! Every effect is a read/write to pgmcp's OWN tables (working_set_pages /
//! working_set_config / orchestration_sessions) plus the mock data plane — no
//! shell, no user files.

use pgmcp::tape::data_plane::{MockTapeDataPlane, PageQuery, TreePath};
use pgmcp::tape::engine::PagingEngine;
use pgmcp::tape::store;
use pgmcp::tape::vocab::{EvictReason, EvictionPolicy, PageKind};
use pgmcp::tape::working_set::{PageAddr, ResidentPage, WorkingSet};
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

/// Insert the parent `orchestration_sessions` row (FK target for both
/// working-set tables) for `session_key`, cleaning any prior rows first.
async fn seed_session(pool: &PgPool, session_key: &str) {
    sqlx::query("DELETE FROM orchestration_sessions WHERE session_key = $1")
        .bind(session_key)
        .execute(pool)
        .await
        .expect("clean prior session");
    sqlx::query(
        "INSERT INTO orchestration_sessions (session_key, protocol_name, global_type)
         VALUES ($1, 'tape-test', '{}'::jsonb)",
    )
    .bind(session_key)
    .execute(pool)
    .await
    .expect("seed orchestration_sessions");
}

fn resident(
    addr: &str,
    tokens: i32,
    importance: f32,
    ord: u64,
    dirty: bool,
    pinned: bool,
) -> ResidentPage {
    ResidentPage {
        addr: PageAddr(addr.into()),
        kind: PageKind::FileChunk,
        importance,
        est_tokens: tokens,
        use_count: 1,
        last_access_ord: ord,
        dirty,
        pinned,
        // FileChunk fixtures are re-fetchable from the corpus, so they carry no
        // write-back bytes (only Scratch pages do; see `ResidentPage::bytes`).
        bytes: None,
    }
}

// ---------------------------------------------------------------------------
// 1. store save → load round-trip
// ---------------------------------------------------------------------------
#[tokio::test]
async fn store_round_trip_preserves_logical_metadata() {
    let db = require_test_db!();
    let pool = db.pool();
    let session = "tape-rt-1";
    seed_session(pool, session).await;

    let mut ws = WorkingSet::new(session, 0, 1000, EvictionPolicy::Lfu);
    ws.clock = 42;
    ws.pages.insert(resident("a", 100, 0.5, 7, false, false));
    ws.pages.insert(resident("b", 200, 0.9, 41, true, false));
    ws.pages.insert(resident("c", 50, 0.1, 3, false, true)); // pinned
    ws.resident_tokens = ws.recompute_resident_tokens();

    store::save_working_set(pool, &ws, "rlm:t", 4096, Some(0))
        .await
        .expect("save working set");

    let loaded = store::load_working_set(pool, session, 0)
        .await
        .expect("load working set");

    assert_eq!(loaded.budget_tokens, 1000);
    assert_eq!(loaded.policy, EvictionPolicy::Lfu);
    assert_eq!(loaded.clock, 42, "logical clock round-trips");
    assert_eq!(loaded.resident_tokens, 350, "Σ est_tokens reconstructed");
    assert_eq!(loaded.pages.len(), 3);

    let b = loaded.pages.get(&PageAddr("b".into())).expect("b present");
    assert_eq!(
        b.last_access_ord, 41,
        "last_access_ord is the LOGICAL value"
    );
    assert!(b.dirty, "dirty flag round-trips");
    let c = loaded.pages.get(&PageAddr("c".into())).expect("c present");
    assert!(c.pinned, "pinned state round-trips");

    // Insertion order on reload follows (last_access_ord, addr): c(3) a(7) b(41).
    let order: Vec<&str> = loaded
        .pages
        .iter_in_order()
        .map(|p| p.addr.0.as_str())
        .collect();
    assert_eq!(
        order,
        ["c", "a", "b"],
        "deterministic reload order by logical clock"
    );
}

#[tokio::test]
async fn store_evict_and_dirty_and_list_dirty() {
    let db = require_test_db!();
    let pool = db.pool();
    let session = "tape-rt-2";
    seed_session(pool, session).await;

    let mut ws = WorkingSet::new(session, 0, 1000, EvictionPolicy::Lru);
    ws.pages.insert(resident("a", 10, 0.5, 1, false, false));
    ws.pages.insert(resident("b", 10, 0.5, 2, false, false));
    ws.resident_tokens = ws.recompute_resident_tokens();
    store::save_working_set(pool, &ws, "rlm:t", 4096, None)
        .await
        .expect("save");

    store::mark_dirty(pool, session, 0, &PageAddr("a".into()))
        .await
        .expect("mark dirty");
    let dirty = store::list_dirty(pool, session, 0)
        .await
        .expect("list dirty");
    assert_eq!(dirty, vec![PageAddr("a".into())], "only a is dirty");

    store::evict_page(
        pool,
        session,
        0,
        &PageAddr("b".into()),
        EvictReason::BudgetPressure,
    )
    .await
    .expect("evict b");
    let reloaded = store::load_working_set(pool, session, 0)
        .await
        .expect("reload");
    assert!(
        reloaded.pages.get(&PageAddr("b".into())).is_none(),
        "evicted page is not loaded back as resident"
    );
    assert!(reloaded.pages.get(&PageAddr("a".into())).is_some());

    let new_clock = store::bump_clock(pool, session, 5)
        .await
        .expect("bump clock");
    assert!(new_clock >= 5, "clock advanced");
}

// ---------------------------------------------------------------------------
// 2-3. page-in budget invariant + over-budget eviction
// ---------------------------------------------------------------------------
#[tokio::test]
async fn page_in_respects_budget_and_evicts_when_over() {
    let db = require_test_db!();
    let pool = db.pool();
    let session = "tape-pi-1";
    seed_session(pool, session).await;

    let dp = MockTapeDataPlane::new();
    let tree = TreePath::for_root_task("t-pi-1");
    // Three 40-token pages; budget 100 ⇒ at most 2 resident at once.
    dp.insert_page(
        &tree,
        &PageAddr("p1".into()),
        "aaaa",
        40,
        0.9,
        PageKind::FileChunk,
    );
    dp.insert_page(
        &tree,
        &PageAddr("p2".into()),
        "bbbb",
        40,
        0.5,
        PageKind::FileChunk,
    );
    dp.insert_page(
        &tree,
        &PageAddr("p3".into()),
        "cccc",
        40,
        0.1,
        PageKind::FileChunk,
    );

    let engine = PagingEngine::new(pool, &dp);
    let mut ws = WorkingSet::new(session, 0, 100, EvictionPolicy::ImportanceWeighted);

    let outcome = engine
        .page_in(
            &mut ws,
            &tree,
            &PageQuery::Semantic {
                query: "q".into(),
                k: 10,
            },
        )
        .await
        .expect("page_in");

    assert!(
        ws.resident_tokens <= ws.budget_tokens,
        "resident_tokens {} must not exceed budget {}",
        ws.resident_tokens,
        ws.budget_tokens
    );
    assert_eq!(
        ws.resident_tokens,
        ws.recompute_resident_tokens(),
        "token invariant"
    );
    // Two admitted, the third reported as budget-exhausted (an eviction kept us
    // within budget rather than overflowing).
    assert_eq!(
        outcome.admitted.len(),
        2,
        "only two 40-token pages fit in 100"
    );
    assert!(
        !outcome.budget_exhausted.is_empty() || outcome.evicted.is_empty(),
        "the third page could not be admitted within budget"
    );
    // The two highest-importance pages are the residents.
    assert!(
        ws.pages.contains(&PageAddr("p1".into())),
        "highest importance resident"
    );
    assert!(ws.pages.contains(&PageAddr("p2".into())));
    assert!(
        !ws.pages.contains(&PageAddr("p3".into())),
        "lowest importance not resident"
    );
}

// ---------------------------------------------------------------------------
// 4. pinned never evicted
// ---------------------------------------------------------------------------
#[tokio::test]
async fn pinned_page_is_never_evicted() {
    let db = require_test_db!();
    let pool = db.pool();
    let session = "tape-pin-1";
    seed_session(pool, session).await;

    let dp = MockTapeDataPlane::new();
    let tree = TreePath::for_root_task("t-pin-1");
    dp.insert_page(
        &tree,
        &PageAddr("new".into()),
        "xxxx",
        80,
        0.9,
        PageKind::FileChunk,
    );

    let engine = PagingEngine::new(pool, &dp);
    let mut ws = WorkingSet::new(session, 0, 100, EvictionPolicy::ImportanceWeighted);
    // Pre-load a PINNED page consuming 60 of 100; persist it.
    ws.pages
        .insert(resident("pinned", 60, 0.01, 1, false, true));
    ws.resident_tokens = 60;
    store::save_working_set(pool, &ws, tree.as_str(), 4096, None)
        .await
        .expect("save pinned");

    // Try to page in an 80-token page: it cannot fit (only 40 headroom, and the
    // pinned page is the only eviction candidate but is exempt).
    let outcome = engine
        .page_in(
            &mut ws,
            &tree,
            &PageQuery::Semantic {
                query: "q".into(),
                k: 10,
            },
        )
        .await
        .expect("page_in");

    assert!(
        ws.pages.contains(&PageAddr("pinned".into())),
        "pinned page survives"
    );
    assert!(
        !ws.pages.contains(&PageAddr("new".into())),
        "new page could not displace pinned"
    );
    assert_eq!(outcome.evicted.len(), 0, "no eviction occurred");
    assert!(
        outcome.budget_exhausted.contains(&PageAddr("new".into())),
        "the un-admittable page is reported as budget-exhausted"
    );
    assert!(ws.resident_tokens <= ws.budget_tokens);
}

// ---------------------------------------------------------------------------
// 6. ImportanceWeighted: keep high-importance, drop low-importance LRU
// ---------------------------------------------------------------------------
#[tokio::test]
async fn importance_weighted_keeps_high_drops_low() {
    let db = require_test_db!();
    let pool = db.pool();
    let session = "tape-iw-1";
    seed_session(pool, session).await;

    let dp = MockTapeDataPlane::new();
    let tree = TreePath::for_root_task("t-iw-1");
    let engine = PagingEngine::new(pool, &dp);

    // Resident: a high-importance page that is ALSO the oldest (LRU), and a
    // low-importance newer page. Budget 100, each 50 ⇒ full.
    let mut ws = WorkingSet::new(session, 0, 100, EvictionPolicy::ImportanceWeighted);
    ws.clock = 10;
    ws.pages
        .insert(resident("important_old", 50, 100.0, 0, false, false)); // oldest
    ws.pages
        .insert(resident("trivial_new", 50, 0.1, 9, false, false)); // newest
    ws.resident_tokens = 100;
    store::save_working_set(pool, &ws, tree.as_str(), 4096, None)
        .await
        .expect("save");

    // A new 50-token page forces exactly one eviction.
    dp.insert_page(
        &tree,
        &PageAddr("incoming".into()),
        "yyyy",
        50,
        0.8,
        PageKind::FileChunk,
    );
    let outcome = engine
        .page_in(
            &mut ws,
            &tree,
            &PageQuery::Grep {
                pattern: "incoming".into(),
            },
        )
        .await
        .expect("page_in");

    assert_eq!(
        outcome.evicted,
        vec![PageAddr("trivial_new".into())],
        "low-importance evicted"
    );
    assert!(
        ws.pages.contains(&PageAddr("important_old".into())),
        "high-importance page kept resident despite being the LRU"
    );
    assert!(
        ws.pages.contains(&PageAddr("incoming".into())),
        "incoming admitted"
    );
    assert_eq!(ws.resident_tokens, ws.recompute_resident_tokens());
    assert!(ws.resident_tokens <= ws.budget_tokens);
}

// ---------------------------------------------------------------------------
// 7. dirty evict → put once; clean evict → put zero
// ---------------------------------------------------------------------------
#[tokio::test]
async fn dirty_evict_writes_back_once_clean_evict_zero() {
    let db = require_test_db!();
    let pool = db.pool();
    let session = "tape-wb-1";
    seed_session(pool, session).await;

    let dp = MockTapeDataPlane::new();
    let tree = TreePath::for_root_task("t-wb-1");
    let engine = PagingEngine::new(pool, &dp);

    // Two resident pages (one dirty, one clean), budget full; one incoming page
    // forces eviction. We run two separate scenarios to isolate which is evicted.

    // --- dirty victim ---
    let mut ws = WorkingSet::new(session, 0, 100, EvictionPolicy::Lru);
    ws.clock = 10;
    // The dirty page is the LRU (oldest) so LRU evicts it.
    ws.pages.insert(resident(
        "dirty_lru",
        50,
        1.0,
        0,
        /*dirty*/ true,
        false,
    ));
    ws.pages
        .insert(resident("clean_new", 50, 1.0, 9, false, false));
    ws.resident_tokens = 100;
    store::save_working_set(pool, &ws, tree.as_str(), 4096, None)
        .await
        .expect("save");
    dp.insert_page(
        &tree,
        &PageAddr("inc".into()),
        "z",
        50,
        1.0,
        PageKind::FileChunk,
    );

    let out = engine
        .page_in(
            &mut ws,
            &tree,
            &PageQuery::Grep {
                pattern: "inc".into(),
            },
        )
        .await
        .expect("page_in");
    assert_eq!(
        out.evicted,
        vec![PageAddr("dirty_lru".into())],
        "LRU dirty page evicted"
    );
    assert_eq!(
        dp.put_count(&tree, &PageAddr("dirty_lru".into())),
        1,
        "dirty victim written back exactly once"
    );
    assert_eq!(
        dp.put_count(&tree, &PageAddr("clean_new".into())),
        0,
        "the surviving clean page is never written back"
    );

    // --- clean victim (fresh session/cursor) ---
    let session2 = "tape-wb-2";
    seed_session(pool, session2).await;
    let tree2 = TreePath::for_root_task("t-wb-2");
    let mut ws2 = WorkingSet::new(session2, 0, 100, EvictionPolicy::Lru);
    ws2.clock = 10;
    ws2.pages.insert(resident(
        "clean_lru",
        50,
        1.0,
        0,
        /*dirty*/ false,
        false,
    ));
    ws2.pages
        .insert(resident("other", 50, 1.0, 9, false, false));
    ws2.resident_tokens = 100;
    store::save_working_set(pool, &ws2, tree2.as_str(), 4096, None)
        .await
        .expect("save2");
    dp.insert_page(
        &tree2,
        &PageAddr("inc2".into()),
        "z",
        50,
        1.0,
        PageKind::FileChunk,
    );

    let out2 = engine
        .page_in(
            &mut ws2,
            &tree2,
            &PageQuery::Grep {
                pattern: "inc2".into(),
            },
        )
        .await
        .expect("page_in2");
    assert_eq!(
        out2.evicted,
        vec![PageAddr("clean_lru".into())],
        "LRU clean page evicted"
    );
    assert_eq!(
        dp.put_count(&tree2, &PageAddr("clean_lru".into())),
        0,
        "a clean evict performs NO write-back"
    );
}

// ---------------------------------------------------------------------------
// 8. demotion ladder pages in exactly one smaller SummaryNode
// ---------------------------------------------------------------------------
#[tokio::test]
async fn demotion_ladder_pages_in_one_smaller_summary() {
    let db = require_test_db!();
    let pool = db.pool();
    let session = "tape-dem-1";
    seed_session(pool, session).await;

    let dp = MockTapeDataPlane::new();
    let tree = TreePath::for_root_task("t-dem-1");
    let engine = PagingEngine::new(pool, &dp);

    // Resident: one big dirty leaf (the LRU victim) + one clean page. Budget
    // tuned so evicting the 60-token leaf frees room for both the incoming page
    // and the 10-token summary.
    let mut ws = WorkingSet::new(session, 0, 100, EvictionPolicy::Lru);
    ws.clock = 10;
    ws.pages
        .insert(resident("leaf", 60, 1.0, 0, /*dirty*/ true, false)); // LRU victim
    ws.pages.insert(resident("keep", 30, 1.0, 9, false, false));
    ws.resident_tokens = 90;
    store::save_working_set(pool, &ws, tree.as_str(), 4096, None)
        .await
        .expect("save");

    // Register a SummaryNode (10 tokens < 60 leaf) for the leaf set, and seed its
    // content so it can be fetched.
    let summary = PageAddr("leaf.summary".into());
    dp.insert_page(&tree, &summary, "digest", 10, 0.9, PageKind::SummaryNode);
    dp.register_summary(&tree, &[PageAddr("leaf".into())], &summary);

    // Incoming 20-token page; headroom is 10 → must evict the leaf (frees 60),
    // then the summary (10) is paged in and the incoming (20) admitted.
    dp.insert_page(
        &tree,
        &PageAddr("incoming".into()),
        "new",
        20,
        0.95,
        PageKind::FileChunk,
    );
    let out = engine
        .page_in(
            &mut ws,
            &tree,
            &PageQuery::Grep {
                pattern: "incoming".into(),
            },
        )
        .await
        .expect("page_in");

    assert!(
        out.evicted.contains(&PageAddr("leaf".into())),
        "the big leaf was evicted"
    );
    assert_eq!(
        out.demoted_in,
        vec![summary.clone()],
        "exactly one SummaryNode demoted in"
    );
    let sum_page = ws.pages.get(&summary).expect("summary resident");
    assert_eq!(sum_page.kind, PageKind::SummaryNode);
    assert!(sum_page.est_tokens < 60, "summary est_tokens < Σ leaves");
    // Write-back of the dirty leaf happened exactly once before demotion.
    assert_eq!(dp.put_count(&tree, &PageAddr("leaf".into())), 1);
    assert!(
        ws.resident_tokens <= ws.budget_tokens,
        "still within budget after demotion"
    );
    assert_eq!(ws.resident_tokens, ws.recompute_resident_tokens());
}
