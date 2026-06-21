//! Integration tests for the Crucible **context-tape HYDRATION BRIDGE** (Phase 3
//! — the REAL [`RealTapeDataPlane`]), driven end-to-end against a real test
//! Postgres + the deterministic 1024-d embedder. Where
//! `tape_paging_lifecycle.rs` exercises the P5 control plane over the in-memory
//! `MockTapeDataPlane`, these tests exercise the production data plane that wires
//! the P0-P2 `context_tape` crate (its `PageAddress` / `Page` / `TapeStore`) plus
//! pgmcp's read-only corpus behind the same `TapeDataPlane` seam.
//!
//! Coverage (the discriminating properties the plan calls out):
//!   1. hydrate a seeded chunk → content carries the `build_context_prefix` header.
//!   2. PageAddr ↔ PageAddress round-trip (incl. the DB-backed `node_id`
//!      resolver).
//!   3. `resolve(Semantic/Grep/Chunk)` returns PageRefs WITHOUT hydrating bytes
//!      (the per-tree store stays empty after a resolve).
//!   4. **corpus-never-written**: a source-grep over `src/tape/` bans
//!      INSERT/UPDATE/DELETE on `file_chunks` / `indexed_files` (mirrors
//!      `digest_trust_boundary.rs`).
//!   5. the `PagingEngine` page-in → evict → demote cycle works end-to-end over
//!      `RealTapeDataPlane` against the test DB.
//!   6. `put` with `allow_promotion=false` stages the dirty bytes in the tree
//!      store but writes NOTHING durable; an observation hydrates and supersedes
//!      only when promotion is enabled.
//!
//! Every effect is a read of pgmcp's read-only corpus (`file_chunks` /
//! `indexed_files` / `memory_*`) plus reads/writes to pgmcp's OWN working-set
//! tables and (gated) `memory_observations` — no shell, no user files.

mod common;

use std::path::{Path, PathBuf};

use common::context_with_pool;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
// `context_tape` is re-exported through pgmcp's tape bridge module, so the test
// names its address vocabulary without a separate dependency edge.
use pgmcp::tape::address_resolve::{address_to_pageaddr, node_id_to_address, pageaddr_to_address};
use pgmcp::tape::context_tape::PageAddress;
use pgmcp::tape::data_plane::{PageQuery, TapeDataPlane, TapeError, TreePath};
use pgmcp::tape::engine::PagingEngine;
use pgmcp::tape::real_data_plane::RealTapeDataPlane;
use pgmcp::tape::store;
use pgmcp::tape::vocab::{EvictionPolicy, PageKind};
use pgmcp::tape::working_set::{PageAddr, WorkingSet};
use pgmcp_testing::fixtures::test_embedding;
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

const D: usize = 1024;

// ---------------------------------------------------------------------------
// Seeders (explicit content so the contextual-prefix header is assertable).
// ---------------------------------------------------------------------------

async fn insert_project(pool: &PgPool, name: &str) -> i32 {
    sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws")
    .bind(format!("/ws/{name}"))
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("insert project")
}

async fn insert_file(pool: &PgPool, project_id: i32, path: &str, language: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO indexed_files \
         (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW()) RETURNING id",
    )
    .bind(project_id)
    .bind(format!("/ws/{path}"))
    .bind(path)
    .bind(language)
    .bind(64_i64)
    .bind("body")
    .bind(1_i32)
    .fetch_one(pool)
    .await
    .expect("insert file")
}

/// Insert a chunk with explicit content + a deterministic embedding (keyed on
/// `embed_key` so a semantic query can target it). Returns the chunk id.
#[allow(clippy::too_many_arguments)]
async fn insert_chunk(
    pool: &PgPool,
    file_id: i64,
    idx: i32,
    content: &str,
    start_line: i32,
    end_line: i32,
    embed_key: &str,
) -> i64 {
    let v = pgvector::Vector::from(test_embedding(D, embed_key));
    sqlx::query_scalar(
        "INSERT INTO file_chunks \
         (file_id, chunk_index, content, start_line, end_line, embedding_v2, embedding_signature) \
         VALUES ($1, $2, $3, $4, $5, $6, 'bge-m3-v1') RETURNING id",
    )
    .bind(file_id)
    .bind(idx)
    .bind(content)
    .bind(start_line)
    .bind(end_line)
    .bind(v)
    .fetch_one(pool)
    .await
    .expect("insert chunk")
}

/// Seed a memory entity + one active observation; returns the observation id.
async fn insert_observation(pool: &PgPool, content: &str, importance: f32) -> i64 {
    let entity_id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, importance, source)
         VALUES ($1, 'concept', 0.5, 'agent_write') RETURNING id",
    )
    .bind(format!("entity-for-{}", &content[..content.len().min(12)]))
    .fetch_one(pool)
    .await
    .expect("insert entity");
    let sha = format!("{:064x}", (content.len() as u128) * 1_000_003 + 7);
    sqlx::query_scalar(
        "INSERT INTO memory_observations
            (entity_id, content, content_sha256, importance, source, valid_from)
         VALUES ($1, $2, $3, $4, 'agent_write', NOW()) RETURNING id",
    )
    .bind(entity_id)
    .bind(content)
    .bind(&sha)
    .bind(importance)
    .fetch_one(pool)
    .await
    .expect("insert observation")
}

/// Insert the parent `orchestration_sessions` row (FK target for the working-set
/// tables), cleaning any prior rows first.
async fn seed_session(pool: &PgPool, session_key: &str) {
    sqlx::query("DELETE FROM orchestration_sessions WHERE session_key = $1")
        .bind(session_key)
        .execute(pool)
        .await
        .expect("clean prior session");
    sqlx::query(
        "INSERT INTO orchestration_sessions (session_key, protocol_name, global_type)
         VALUES ($1, 'tape-hydration-test', '{}'::jsonb)",
    )
    .bind(session_key)
    .execute(pool)
    .await
    .expect("seed orchestration_sessions");
}

/// A `SystemContext` over the real pool with `[tape] allow_promotion` set as
/// requested (the `common` helper builds one with the default config; this
/// rebuilds with an overridden flag).
fn context_with_promotion(pool: PgPool, allow_promotion: bool) -> SystemContext {
    let base = context_with_pool(pool);
    let mut cfg = Config::default();
    cfg.tape.allow_promotion = allow_promotion;
    base.config().store(std::sync::Arc::new(cfg));
    base
}

// ===========================================================================
// 1. hydrate a seeded chunk → content carries the build_context_prefix header
// ===========================================================================
#[tokio::test]
async fn hydrate_chunk_carries_context_prefix_header() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "hydrate-prefix").await;
    let file_id = insert_file(&pool, project_id, "src/lib.rs", "rust").await;
    let chunk_body = "pub fn answer() -> i32 { 42 }";
    let chunk_id = insert_chunk(&pool, file_id, 0, chunk_body, 1, 1, "hydrate-prefix-k").await;

    let ctx = context_with_pool(pool.clone());
    let dp = RealTapeDataPlane::from_context(&ctx).expect("real data plane over a PgPool");
    let tree = TreePath::for_root_task("t-hydrate-1");

    let addr = address_to_pageaddr(&PageAddress::Chunk { chunk_id });
    let content = dp.get(&tree, &addr).await.expect("hydrate chunk");

    // The situating prefix wraps the raw chunk: `[File: src/lib.rs | Lang: rust …]\n`
    // ahead of the body (the build_context_prefix contract).
    assert!(
        content.bytes.starts_with("[File: src/lib.rs"),
        "content must lead with the contextual header; got: {}",
        content.bytes
    );
    assert!(
        content.bytes.contains("Lang: rust"),
        "header carries language: {}",
        content.bytes
    );
    assert!(
        content.bytes.ends_with(chunk_body),
        "raw chunk text follows the header: {}",
        content.bytes
    );
    assert!(content.est_tokens > 0, "token estimate is set");

    // A second get is served from the hot tier (no re-hydrate); same bytes.
    let again = dp.get(&tree, &addr).await.expect("hot re-get");
    assert_eq!(again.bytes, content.bytes, "hot-tier read is identical");
}

// ===========================================================================
// 2. PageAddr ↔ PageAddress round-trip (incl. the DB-backed node_id resolver)
// ===========================================================================
#[tokio::test]
async fn pageaddr_pageaddress_round_trip_and_node_id_resolver() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "addr-rt").await;
    let file_id = insert_file(&pool, project_id, "src/addr.rs", "rust").await;
    let chunk_id = insert_chunk(&pool, file_id, 0, "fn x() {}", 1, 1, "addr-rt-k").await;
    let obs_id = insert_observation(&pool, "a durable observation about x", 0.7).await;

    // Pure (no DB): every legal corpus address survives the PageAddr round-trip.
    for address in [
        PageAddress::Chunk { chunk_id },
        PageAddress::File { file_id },
        PageAddress::Observation { obs_id },
        PageAddress::FileRegion {
            file_id,
            start_chunk: 0,
            end_chunk: 3,
        },
    ] {
        let pa = address_to_pageaddr(&address);
        assert_eq!(
            pageaddr_to_address(&pa),
            Some(address.clone()),
            "PageAddr round-trip for {address:?}"
        );
    }

    // DB-backed node_id resolver: a numeric node_id resolves directly…
    let by_num = node_id_to_address(&pool, &format!("chunk:{chunk_id}"))
        .await
        .expect("resolve chunk node_id")
        .expect("chunk resolves");
    assert_eq!(by_num, PageAddress::Chunk { chunk_id });

    // …and a HUMAN-key node_id (a file path) resolves via resolve_graph_node_id.
    let by_path = node_id_to_address(&pool, "file:src/addr.rs")
        .await
        .expect("resolve file node_id by path")
        .expect("file path resolves to its id");
    assert_eq!(by_path, PageAddress::File { file_id });

    // An observation node_id resolves.
    let by_obs = node_id_to_address(&pool, &format!("observation:{obs_id}"))
        .await
        .expect("resolve observation node_id")
        .expect("observation resolves");
    assert_eq!(by_obs, PageAddress::Observation { obs_id });

    // A non-corpus node type yields None (well-formed, but not a tape page).
    assert!(
        node_id_to_address(&pool, "project:whatever")
            .await
            .expect("resolve")
            .is_none(),
        "a project node_id has no PageAddress"
    );
}

// ===========================================================================
// 3. resolve(Semantic/Grep/Chunk) returns PageRefs WITHOUT hydrating bytes
// ===========================================================================
#[tokio::test]
async fn resolve_returns_refs_without_hydrating_bytes() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "resolve-norehydrate").await;
    let file_id = insert_file(&pool, project_id, "src/resolve.rs", "rust").await;
    // Three chunks; one carries a grep-able token, all carry a semantic key.
    let c0 = insert_chunk(
        &pool,
        file_id,
        0,
        "fn alpha() { let needle = 1; }",
        1,
        1,
        "resolve-q",
    )
    .await;
    let _c1 = insert_chunk(&pool, file_id, 1, "fn beta() {}", 2, 2, "resolve-other").await;
    let _c2 = insert_chunk(&pool, file_id, 2, "fn gamma() {}", 3, 3, "resolve-misc").await;

    let ctx = context_with_pool(pool.clone());
    let dp = RealTapeDataPlane::from_context(&ctx).expect("real data plane");
    let tree = TreePath::for_root_task("t-resolve-1");

    // --- Chunk range resolve ---
    let chunk_refs = dp
        .resolve(
            &tree,
            &PageQuery::Chunk {
                path: "src/resolve.rs".into(),
                lo: 0,
                hi: 2,
            },
        )
        .await
        .expect("resolve chunk range");
    assert_eq!(
        chunk_refs.len(),
        3,
        "all three chunks in [0,2] are referenced"
    );
    assert!(chunk_refs.iter().all(|r| r.kind == PageKind::FileChunk));
    // The first ref addresses chunk c0 by its corpus path.
    let c0_path = address_to_pageaddr(&PageAddress::Chunk { chunk_id: c0 });
    assert!(
        chunk_refs.iter().any(|r| r.addr == c0_path),
        "resolve(Chunk) addresses the seeded chunk by its corpus/chunk/<id> path"
    );

    // --- Grep resolve ---
    let grep_refs = dp
        .resolve(
            &tree,
            &PageQuery::Grep {
                pattern: "needle".into(),
            },
        )
        .await
        .expect("resolve grep");
    assert!(
        !grep_refs.is_empty(),
        "grep finds the chunk containing the token"
    );
    assert!(
        grep_refs.iter().any(|r| r.addr == c0_path),
        "grep ref addresses the matching chunk"
    );

    // --- Semantic resolve (the embedding path P5 deferred) ---
    let sem_refs = dp
        .resolve(
            &tree,
            &PageQuery::Semantic {
                query: "resolve-q".into(),
                k: 3,
            },
        )
        .await
        .expect("resolve semantic");
    assert!(!sem_refs.is_empty(), "semantic resolve returns ranked refs");
    assert!(sem_refs.len() <= 3, "k bounds the candidate count");
    assert!(sem_refs.iter().all(|r| r.kind == PageKind::FileChunk));

    // THE DISCRIMINATING PROPERTY: resolve produced metadata only — NO page was
    // hydrated into the per-tree store. We confirm the store for this tree holds
    // zero pages (resolve never touches the store; get/get_many do).
    let tree_id = RealTapeDataPlane::tree_id(&tree);
    let resident = ctx.tape_registry().with_store(tree_id, |s| s.len());
    assert_eq!(
        resident, 0,
        "resolve must NOT hydrate bytes into the tree store"
    );
}

// ===========================================================================
// 4. corpus-never-written: source-grep over src/tape/ bans corpus writes
// ===========================================================================

/// Repo root (one level above pgmcp-testing's `CARGO_MANIFEST_DIR`).
fn repo_root() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    Path::new(&manifest)
        .parent()
        .expect("workspace root above pgmcp-testing")
        .to_path_buf()
}

/// Every `*.rs` file directly under `src/tape/`, as `(relative_label, contents)`.
fn tape_sources() -> Vec<(String, String)> {
    let dir = repo_root().join("src").join("tape");
    let mut out = Vec::new();
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let label = format!(
                "src/tape/{}",
                path.file_name().expect("file name").to_string_lossy()
            );
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            out.push((label, body));
        }
    }
    assert!(!out.is_empty(), "expected at least one src/tape/*.rs file");
    out
}

/// Strip `//`-prefixed line comments so a table/word merely *named* in prose
/// (like this very test's doc-notes) does not trip the grep — we guard code.
fn strip_line_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let code = match line.find("//") {
            Some(idx) => &line[..idx],
            None => line,
        };
        out.push_str(code);
        out.push('\n');
    }
    out
}

#[test]
fn tape_never_writes_the_corpus_tables() {
    // The corpus (`file_chunks` / `indexed_files`) is READ-ONLY to the tape; pi
    // owns the user's files. Any mutating SQL verb against either table from
    // `src/tape/` is a trust-boundary violation — fail before it ships.
    for (label, src) in tape_sources() {
        let code = strip_line_comments(&src).to_uppercase();
        for table in ["FILE_CHUNKS", "INDEXED_FILES"] {
            for verb in ["INSERT INTO", "UPDATE", "DELETE FROM"] {
                let needle = format!("{verb} {table}");
                assert!(
                    !code.contains(&needle),
                    "{label} must not `{verb} {table}` — the corpus is read-only to the tape \
                     (pi owns the user's files; the tape only SELECTs the corpus and writes \
                     its own working-set / memory_observations tables)"
                );
            }
        }
    }
}

// ===========================================================================
// 5. PagingEngine page-in → evict → demote, end-to-end over RealTapeDataPlane
// ===========================================================================
#[tokio::test]
async fn paging_engine_page_in_evict_demote_over_real_plane() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let session = "tape-hb-engine-1";
    seed_session(&pool, session).await;

    // Seed a file with several chunks; each chunk hydrates to a situated page.
    let project_id = insert_project(&pool, "engine-corpus").await;
    let file_id = insert_file(&pool, project_id, "src/engine.rs", "rust").await;
    // Bodies sized so each page is ~tens of tokens; importance is derived from
    // importer-count (0 here ⇒ neutral 0.5), so ranking falls back to address.
    // `.as_str()` on the middle term so `String + _` binds unambiguously to
    // `Add<&str>` (not the `smartstring` `Add<SmartString>` now in the graph via
    // the context-tape `rhai` REPL dependency). Behavior-identical.
    let body =
        "fn f() { /* padded body for token cost ".to_string() + "x".repeat(120).as_str() + " */ }";
    let n_chunks = 5;
    for i in 0..n_chunks {
        insert_chunk(
            &pool,
            file_id,
            i,
            &body,
            i + 1,
            i + 1,
            &format!("engine-k-{i}"),
        )
        .await;
    }

    let ctx = context_with_pool(pool.clone());
    let dp = RealTapeDataPlane::from_context(&ctx).expect("real data plane");
    let tree = TreePath::for_root_task("t-hb-engine-1");
    let engine = PagingEngine::new(&pool, &dp);

    // A budget that fits only ~2 of the 5 chunk pages ⇒ page-in must evict to
    // stay within budget. Each page is est_tokens = len/4 of (prefix + body).
    let one = dp
        .get(
            &tree,
            &address_to_pageaddr(&first_chunk_addr(&pool, file_id).await),
        )
        .await
        .expect("probe one page for its token cost");
    let per_page = one.est_tokens.max(1);
    // Clear the probe's residency so the engine starts from an empty working set
    // view (the store hot-admitted it; drop the whole tree to reset cleanly).
    ctx.tape_registry()
        .drop_tree(&RealTapeDataPlane::tree_id(&tree));
    let budget = per_page * 2 + per_page / 2; // room for 2, not 3.

    let mut ws = WorkingSet::new(session, 0, budget, EvictionPolicy::Lru);

    // Page in the whole file's chunk range. More candidates than fit ⇒ the engine
    // admits as many as the budget allows and reports the rest budget-exhausted /
    // evicts to fit.
    let outcome = engine
        .page_in(
            &mut ws,
            &tree,
            &PageQuery::Chunk {
                path: "src/engine.rs".into(),
                lo: 0,
                hi: (n_chunks - 1),
            },
        )
        .await
        .expect("page_in over real plane");

    // Budget invariant holds and is exactly Σ est_tokens.
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
    assert!(!outcome.admitted.is_empty(), "at least one page admitted");
    assert!(
        !outcome.budget_exhausted.is_empty() || !outcome.evicted.is_empty(),
        "more candidates than fit ⇒ some were evicted or reported budget-exhausted"
    );

    // --- Explicit evict pass: force eviction under pressure and confirm the
    //     bytes were not lost from the data plane (a clean victim re-hydrates on
    //     the next get). Capture the oldest resident page as the victim target. ---
    let victim = ws
        .pages
        .iter_in_order()
        .next()
        .map(|p| p.addr.clone())
        .expect("a resident page to evict");
    // Shrink the budget to force genuine pressure, then evict to fit.
    ws.budget_tokens = per_page; // room for ~1 page ⇒ must shed at least one.
    let mut pressure = pgmcp::tape::engine::PageInOutcome::default();
    let freed = engine
        .evict_to_fit(&mut ws, &tree, 0, &mut pressure)
        .await
        .expect("evict under pressure");
    assert!(freed, "eviction frees space when unpinned victims exist");
    assert!(
        ws.resident_tokens <= ws.budget_tokens,
        "within shrunken budget after eviction"
    );
    assert!(
        !pressure.evicted.is_empty(),
        "at least one page evicted under pressure"
    );

    // A clean evicted page is still re-fetchable from the data plane (it
    // re-hydrates from the corpus, proving eviction did not destroy the source).
    let refetched = dp
        .get(&tree, &victim)
        .await
        .expect("evicted clean page re-hydrates");
    assert!(refetched.est_tokens > 0, "re-hydrated page has content");

    // Reload persisted working set: evicted rows are not resident; the invariant
    // is reconstructed from the logical metadata.
    let reloaded = store::load_working_set(&pool, session, 0)
        .await
        .expect("reload ws");
    assert!(reloaded.resident_tokens <= budget.max(per_page));
}

/// First-chunk address for a file (chunk_index 0), for the token-probe above.
async fn first_chunk_addr(pool: &PgPool, file_id: i64) -> PageAddress {
    let chunk_id: i64 = sqlx::query_scalar(
        "SELECT id FROM file_chunks WHERE file_id = $1 AND chunk_index = 0 LIMIT 1",
    )
    .bind(file_id)
    .fetch_one(pool)
    .await
    .expect("first chunk id");
    PageAddress::Chunk { chunk_id }
}

// ===========================================================================
// 6. put: allow_promotion=false stages dirty only (no durable write);
//    allow_promotion=true supersedes the observation bi-temporally.
// ===========================================================================
#[tokio::test]
async fn put_gated_by_allow_promotion() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let obs_id = insert_observation(&pool, "original observation content", 0.5).await;
    let addr = address_to_pageaddr(&PageAddress::Observation { obs_id });
    let tree = TreePath::for_root_task("t-hb-put-1");

    // --- allow_promotion = false (default): put stages dirty, writes NOTHING. ---
    {
        let ctx = context_with_promotion(pool.clone(), false);
        let dp = RealTapeDataPlane::from_context(&ctx).expect("real data plane (no promotion)");
        dp.put(&tree, &addr, "edited observation content")
            .await
            .expect("put stages dirty");
        // The original observation is still the sole active row (no supersession).
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_observations WHERE id = $1 AND valid_to IS NULL",
        )
        .bind(obs_id)
        .fetch_one(&pool)
        .await
        .expect("count active");
        assert_eq!(
            active, 1,
            "promotion off ⇒ original observation remains active, un-superseded"
        );
        // The dirty bytes are staged in the tree store (a subsequent get sees them).
        let staged = dp.get(&tree, &addr).await.expect("get staged dirty");
        assert_eq!(
            staged.bytes, "edited observation content",
            "dirty bytes live in the store"
        );
    }

    // --- allow_promotion = true: put supersedes the observation bi-temporally. ---
    {
        let ctx = context_with_promotion(pool.clone(), true);
        let dp = RealTapeDataPlane::from_context(&ctx).expect("real data plane (promotion on)");
        let tree2 = TreePath::for_root_task("t-hb-put-2");
        dp.put(&tree2, &addr, "promoted observation content")
            .await
            .expect("put supersedes");
        // The prior row is now closed (valid_to set) and a fresh row is active.
        let prior_closed: Option<bool> = sqlx::query_scalar(
            "SELECT valid_to IS NOT NULL FROM memory_observations WHERE id = $1",
        )
        .bind(obs_id)
        .fetch_optional(&pool)
        .await
        .expect("read prior");
        assert_eq!(
            prior_closed,
            Some(true),
            "promotion on ⇒ prior version is bi-temporally closed"
        );
        // Exactly one ACTIVE successor carries the new content for the same entity.
        let successors: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_observations o
             JOIN memory_observations prev ON prev.id = $1
             WHERE o.entity_id = prev.entity_id
               AND o.valid_to IS NULL
               AND o.content = 'promoted observation content'",
        )
        .bind(obs_id)
        .fetch_one(&pool)
        .await
        .expect("count successors");
        assert_eq!(
            successors, 1,
            "exactly one active superseding observation was written"
        );
    }
}

// ===========================================================================
// 7. get of an unknown / superseded address is a benign NotFound (not a crash)
// ===========================================================================
#[tokio::test]
async fn get_missing_is_notfound() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let ctx = context_with_pool(pool.clone());
    let dp = RealTapeDataPlane::from_context(&ctx).expect("real data plane");
    let tree = TreePath::for_root_task("t-hb-missing-1");

    // A chunk id that does not exist hydrates to NotFound.
    let ghost = address_to_pageaddr(&PageAddress::Chunk {
        chunk_id: 9_999_999_999,
    });
    let err = dp
        .get(&tree, &ghost)
        .await
        .expect_err("missing chunk is an error");
    assert!(
        matches!(err, TapeError::NotFound(_)),
        "missing corpus row ⇒ NotFound, got {err:?}"
    );

    // A malformed address string is likewise a benign NotFound.
    let bad = dp.get(&tree, &PageAddr("not/a/valid/address".into())).await;
    assert!(
        matches!(bad, Err(TapeError::NotFound(_))),
        "malformed address ⇒ NotFound"
    );
}
