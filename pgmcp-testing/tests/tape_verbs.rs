//! Integration tests for the **tape verbs** — the agent-facing MCP surface over
//! the context-tape paging substrate (Phase 4).
//!
//! These exercise every one of the ten verbs through
//! `McpServer::call_tool_cli("tape_<verb>", …)` — the same CLI dispatch path the
//! `every_dispatched_tool_has_an_integration_test` coverage guard scans, so each
//! verb is reachable and the guard stays green.
//!
//! Two tiers, matching the verbs' data needs:
//!   - **Tree-store-only** verbs (`tape_put` / `tape_get`(resident) / `tape_peek`
//!     / `tape_excerpt` / `tape_slice` / `tape_grep`(tree) / `tape_fuzzy` / `tape_list` /
//!     `tape_stat`) need no database — they run over a `MockDbClient`-backed
//!     server and the in-RAM per-tree `TapeStore`. The headline test is a
//!     `put → get → fuzzy → grep → list → stat` round-trip on a fresh tree.
//!   - **Corpus-touching** verbs (`tape_get`(hydrate) / `tape_grep`(corpus) /
//!     `tape_semantic`) need a live Postgres + the deterministic embedder; those
//!     tests `require_test_db!` and seed a chunk to address.
//!
//! Trust-boundary note: these verbs never run a shell, never execute agent code,
//! and never write the user's source files; the corpus is read-only.
//!
//! ## Why the chained tests run on a `with_big_stack` thread
//!
//! `McpServer::call_tool_cli` returns a single future whose size is the maximum
//! over every arm of the ~330-tool `dispatch_tool!` match — i.e. it is large
//! regardless of which tool is named. Several such futures constructed across
//! the awaits of one `async fn` exceed a test thread's default 2 MiB stack and
//! overflow. This is a *test-harness* artifact, not a product defect: in
//! production each MCP request runs exactly one verb on its own fresh task
//! stack. The chained no-DB tests therefore run their body on a dedicated
//! thread with a generous stack ([`with_big_stack`]). Single-call corpus tests
//! stay plain `#[tokio::test]`s (one call never overflows).

mod common;

use common::{server_with_mock, server_with_pool, text_of};
use pgmcp::mcp::server::McpServer;
use pgmcp_testing::fixtures::test_embedding;
use pgmcp_testing::mocks::MockDbClient;
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use sqlx::PgPool;

const D: usize = 1024;

/// Parse a tool result's text payload as JSON.
fn json_of(result: &rmcp::model::CallToolResult) -> Value {
    serde_json::from_str(&text_of(result)).expect("tool body must be JSON")
}

/// Run an async test body on a dedicated thread with a generous (16 MiB) stack
/// and its own current-thread Tokio runtime. See the module docs for the
/// large-`call_tool_cli`-future rationale.
fn with_big_stack<F>(body: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build current-thread runtime")
                .block_on(body);
        })
        .expect("spawn big-stack test thread")
        .join()
        .expect("big-stack test thread panicked");
}

/// Await one tool-call future and return its parsed JSON body.
async fn call(
    fut: impl std::future::Future<Output = Result<rmcp::model::CallToolResult, rmcp::ErrorData>>,
) -> Value {
    json_of(&fut.await.expect("tool call must succeed"))
}

/// A fresh, unique tree id per test so the per-tree stores never collide.
fn fresh_tree() -> String {
    format!("tape-verbs-{}", uuid::Uuid::new_v4())
}

/// A server with no real DB — sufficient for the tree-store-only verbs.
fn mock_server() -> McpServer {
    server_with_mock(MockDbClient::new())
}

/// The per-tree UUID for `tree`, read back from a `tape_stat` call (so the test
/// constructs well-formed `scratch/<tree-uuid>/<hex-slot>` paths without
/// re-deriving the id or depending on the `hex` crate — the slot bytes are
/// written as plain hex-string literals).
async fn tree_uuid(server: &McpServer, tree: &str) -> String {
    call(server.call_tool_cli("tape_stat", json!({"tree": tree}))).await["tree_id"]
        .as_str()
        .expect("tape_stat returns the tree_id")
        .to_string()
}

// ===========================================================================
// Headline round-trip: put → get → fuzzy → grep → list → stat on a fresh tree.
// Tree-store-only, so no database is required.
// ===========================================================================
#[test]
fn put_get_fuzzy_grep_list_stat_round_trip_on_fresh_tree() {
    with_big_stack(async {
        let server = mock_server();
        let tree = fresh_tree();

        // A scratch address is tree-local; build it from this tree's id (read
        // back from tape_stat) so the path is well-formed (PageAddress::to_path).
        // The slot is a plain hex-string literal ("abcd" == bytes 0xab,0xcd).
        let tid = tree_uuid(&server, &tree).await;
        let address = format!("scratch/{tid}/abcd");

        // --- put: stage a named scratch page DIRTY. ---
        let put_v = call(server.call_tool_cli(
            "tape_put",
            json!({"tree": tree, "address": address, "content": "needle haystack ALPHA"}),
        ))
        .await;
        assert_eq!(put_v["address"], address, "echoes the target address");
        assert_eq!(put_v["dirty"], true, "a fresh write is dirty");

        // --- get: read the staged bytes back; dirty flag is reported. ---
        let got_v =
            call(server.call_tool_cli("tape_get", json!({"tree": tree, "address": address}))).await;
        assert_eq!(got_v["content"], "needle haystack ALPHA");
        assert_eq!(got_v["dirty"], true, "the page is still dirty");

        // --- fuzzy: an off-by-one query on the address path resolves it. ---
        let near = format!("scratch/{tid}/abc"); // one deletion from ".../abcd"
        let fuzzy_v = call(server.call_tool_cli(
            "tape_fuzzy",
            json!({"tree": tree, "query": near, "max_distance": 2}),
        ))
        .await;
        let hits = fuzzy_v["hits"].as_array().expect("hits array");
        assert!(
            hits.iter().any(|h| h["address"] == address),
            "fuzzy must surface the staged address within edit distance; got {hits:?}"
        );

        // --- grep (tree scope): substring search over resident content. ---
        let grep_v = call(server.call_tool_cli(
            "tape_grep",
            json!({"tree": tree, "pattern": "ALPHA", "scope": "tree"}),
        ))
        .await;
        let ghits = grep_v["hits"].as_array().expect("grep hits array");
        assert!(
            ghits.iter().any(|h| h["address"] == address),
            "tree-scope grep must find the page whose content contains the pattern"
        );

        // --- list: the staged address is enumerated; dirty_count reflects it. ---
        let list_v =
            call(server.call_tool_cli("tape_list", json!({"tree": tree, "prefix": "scratch/"})))
                .await;
        let addrs = list_v["addresses"].as_array().expect("addresses array");
        assert!(
            addrs.iter().any(|a| a == &Value::String(address.clone())),
            "list must include the staged scratch address"
        );
        assert_eq!(
            list_v["dirty_count"].as_u64(),
            Some(1),
            "exactly one dirty page staged"
        );

        // --- stat: residency accounting reflects the one dirty page. ---
        let stat_v = call(server.call_tool_cli("tape_stat", json!({"tree": tree}))).await;
        assert_eq!(stat_v["n_pages"].as_u64(), Some(1), "one resident page");
        assert_eq!(stat_v["n_dirty"].as_u64(), Some(1), "one dirty page");
        assert!(
            stat_v["resident_bytes"].as_u64().unwrap_or(0) >= "needle haystack ALPHA".len() as u64,
            "resident_bytes accounts for the staged content"
        );
    });
}

// ===========================================================================
// tape_put with omitted address mints a fresh Scratch slot.
// ===========================================================================
#[test]
fn put_without_address_mints_a_fresh_scratch_slot() {
    with_big_stack(async {
        let server = mock_server();
        let tree = fresh_tree();

        let put_v = call(server.call_tool_cli(
            "tape_put",
            json!({"tree": tree, "content": "auto-slot body"}),
        ))
        .await;
        let minted = put_v["address"]
            .as_str()
            .expect("a minted address string")
            .to_string();
        assert!(
            minted.starts_with("scratch/"),
            "an omitted address mints a tree-local scratch slot, got {minted}"
        );
        assert_eq!(put_v["dirty"], true);

        // The minted address is immediately fetchable.
        let got =
            call(server.call_tool_cli("tape_get", json!({"tree": tree, "address": minted}))).await;
        assert_eq!(got["content"], "auto-slot body");
    });
}

// ===========================================================================
// tape_peek returns a bounded head + size, not the full content.
// ===========================================================================
#[test]
fn peek_returns_bounded_head_and_size() {
    with_big_stack(async {
        let server = mock_server();
        let tree = fresh_tree();
        let tid = tree_uuid(&server, &tree).await;
        let address = format!("scratch/{tid}/01"); // slot byte 0x01

        let body = "0123456789ABCDEF".repeat(8); // 128 bytes
        call(server.call_tool_cli(
            "tape_put",
            json!({"tree": tree, "address": address, "content": body}),
        ))
        .await;

        let v = call(server.call_tool_cli(
            "tape_peek",
            json!({"tree": tree, "address": address, "bytes": 16}),
        ))
        .await;
        assert_eq!(v["resident"], true);
        assert_eq!(v["size_bytes"].as_u64(), Some(128), "reports full size");
        assert_eq!(
            v["head"].as_str().map(|s| s.len()),
            Some(16),
            "head is capped at the requested 16 bytes"
        );
        assert_eq!(v["head_truncated"], true, "the head is a truncated preview");
        assert_eq!(
            v["n_pages"].as_u64(),
            Some(1),
            "one resident page under this address prefix"
        );
    });
}

// ===========================================================================
// tape_excerpt returns bounded material without hydrating a full page into the response.
// ===========================================================================
#[test]
fn excerpt_returns_bounded_line_and_byte_ranges() {
    with_big_stack(async {
        let server = mock_server();
        let tree = fresh_tree();
        let tid = tree_uuid(&server, &tree).await;
        let address = format!("scratch/{tid}/02");
        let body = "line-1\nline-2\nline-3\nline-4\n";

        call(server.call_tool_cli(
            "tape_put",
            json!({"tree": tree, "address": address, "content": body}),
        ))
        .await;

        let line_v = call(server.call_tool_cli(
            "tape_excerpt",
            json!({"tree": tree, "address": address, "start_line": 2, "end_line": 3, "max_bytes": 1024}),
        ))
        .await;
        assert_eq!(line_v["found"], true);
        assert_eq!(line_v["content"], "line-2\nline-3\n");
        assert_eq!(line_v["content_truncated"], false);

        let byte_v = call(server.call_tool_cli(
            "tape_excerpt",
            json!({"tree": tree, "address": address, "start_byte": 0, "max_bytes": 6}),
        ))
        .await;
        assert_eq!(byte_v["content"], "line-1");
        assert_eq!(byte_v["excerpt_bytes"].as_u64(), Some(6));
        assert_eq!(byte_v["content_truncated"], true);

        let missing = call(server.call_tool_cli(
            "tape_excerpt",
            json!({"tree": tree, "address": format!("scratch/{tid}/03")}),
        ))
        .await;
        assert_eq!(missing["found"], false);

        let large_address = format!("scratch/{tid}/04");
        call(server.call_tool_cli(
            "tape_put",
            json!({"tree": tree, "address": large_address, "content": "x".repeat(10_000)}),
        ))
        .await;
        let capped = call(server.call_tool_cli(
            "tape_excerpt",
            json!({"tree": tree, "address": large_address, "max_bytes": 20_000}),
        ))
        .await;
        assert_eq!(capped["max_bytes"].as_u64(), Some(8192));
        assert_eq!(capped["excerpt_bytes"].as_u64(), Some(8192));
        assert_eq!(capped["content_truncated"], true);
    });
}

// ===========================================================================
// tape_slice scans resident pages in address order between two bounds.
// ===========================================================================
#[test]
fn slice_scans_resident_pages_in_address_order() {
    with_big_stack(async {
        let server = mock_server();
        let tree = fresh_tree();
        let tid = tree_uuid(&server, &tree).await;

        // Three scratch pages with ascending single-byte slots → ascending keys.
        // Slots written as plain hex-string literals (0x10 < 0x20 < 0x30).
        let mut addrs = Vec::new();
        for (i, slot_hex) in ["10", "20", "30"].iter().enumerate() {
            let a = format!("scratch/{tid}/{slot_hex}");
            call(server.call_tool_cli(
                "tape_put",
                json!({"tree": tree, "address": a, "content": format!("page-{i}")}),
            ))
            .await;
            addrs.push(a);
        }

        // Slice the whole scratch range [lo=0x10, hi=0x30].
        let v = call(server.call_tool_cli(
            "tape_slice",
            json!({"tree": tree, "lo": addrs[0], "hi": addrs[2], "max_pages": 64}),
        ))
        .await;
        let pages = v["pages"].as_array().expect("pages array");
        assert_eq!(pages.len(), 3, "all three pages are in range");
        assert_eq!(v["truncated"], false, "no truncation under the cap");
        // Address order: 0x10 < 0x20 < 0x30.
        assert_eq!(pages[0]["address"], addrs[0]);
        assert_eq!(pages[2]["address"], addrs[2]);

        // A cap of 2 truncates.
        let cv = call(server.call_tool_cli(
            "tape_slice",
            json!({"tree": tree, "lo": addrs[0], "hi": addrs[2], "max_pages": 2}),
        ))
        .await;
        assert_eq!(cv["pages"].as_array().map(|p| p.len()), Some(2));
        assert_eq!(cv["truncated"], true, "the cap was reached");
    });
}

// ===========================================================================
// tape_semantic degrades gracefully (no hits) in mock-DB mode — still reachable
// via call_tool_cli for the coverage guard.
// ===========================================================================
#[test]
fn semantic_is_reachable_and_degrades_without_db() {
    with_big_stack(async {
        let server = mock_server();
        let tree = fresh_tree();
        let v = call(server.call_tool_cli(
            "tape_semantic",
            json!({"tree": tree, "query": "authentication flow", "k": 4}),
        ))
        .await;
        assert_eq!(
            v["available"], false,
            "semantic retrieval is unavailable without a live pool"
        );
        assert_eq!(
            v["hits"].as_array().map(|h| h.len()),
            Some(0),
            "no hits without the corpus"
        );
    });
}

// ===========================================================================
// Corpus-touching: tape_get hydrates a seeded chunk from the READ-ONLY corpus.
// ===========================================================================
#[tokio::test]
async fn get_hydrates_a_seeded_corpus_chunk() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "tape-verbs-get").await;
    let file_id = insert_file(&pool, project_id, "src/get.rs", "rust").await;
    let chunk_id = insert_chunk(
        &pool,
        file_id,
        0,
        "pub fn answer() -> i32 { 42 }",
        1,
        1,
        "tape-verbs-get-k",
    )
    .await;

    let server = server_with_pool(pool);
    let tree = fresh_tree();
    let address = format!("corpus/chunk/{chunk_id}");

    let v = call(server.call_tool_cli("tape_get", json!({"tree": tree, "address": address}))).await;
    assert_eq!(v["address"], address);
    // The situating prefix wraps the raw chunk text; the body must be present.
    let content = v["content"].as_str().expect("hydrated content");
    assert!(
        content.contains("pub fn answer() -> i32 { 42 }"),
        "hydrated content carries the raw chunk body; got: {content}"
    );
    assert_eq!(v["dirty"], false, "a freshly-hydrated corpus page is clean");
}

// ===========================================================================
// Corpus-touching: tape_grep corpus scope resolves matching chunks (read-only).
// ===========================================================================
#[tokio::test]
async fn grep_corpus_scope_resolves_matching_chunks() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "tape-verbs-grep").await;
    let file_id = insert_file(&pool, project_id, "src/grep.rs", "rust").await;
    let _chunk_id = insert_chunk(
        &pool,
        file_id,
        0,
        "fn unique_grep_marker_zzy() {}",
        1,
        1,
        "tape-verbs-grep-k",
    )
    .await;

    let server = server_with_pool(pool);
    let tree = fresh_tree();

    let v = call(server.call_tool_cli(
        "tape_grep",
        json!({"tree": tree, "pattern": "unique_grep_marker_zzy", "scope": "corpus"}),
    ))
    .await;
    let hits = v["hits"].as_array().expect("grep hits array");
    assert!(
        hits.iter().any(|h| h["scope"] == "corpus"),
        "corpus-scope grep must return at least one corpus hit; got {hits:?}"
    );
}

// ===========================================================================
// Corpus-touching: tape_semantic retrieves the nearest seeded chunk.
// ===========================================================================
#[tokio::test]
async fn semantic_retrieves_nearest_seeded_chunk() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "tape-verbs-sem").await;
    let file_id = insert_file(&pool, project_id, "src/sem.rs", "rust").await;
    // The chunk's embedding is keyed on `embed_key`; query with text the
    // deterministic embedder maps near it so it is the nearest neighbor.
    let _chunk_id = insert_chunk(
        &pool,
        file_id,
        0,
        "fn semantic_target() {}",
        1,
        1,
        "tape-verbs-sem-k",
    )
    .await;

    let server = server_with_pool(pool);
    let tree = fresh_tree();

    let v = call(server.call_tool_cli(
        "tape_semantic",
        json!({"tree": tree, "query": "semantic_target function", "k": 5}),
    ))
    .await;
    assert_eq!(
        v["available"], true,
        "semantic path is available with a pool"
    );
    let hits = v["hits"].as_array().expect("semantic hits array");
    assert!(
        !hits.is_empty(),
        "semantic retrieval over a seeded corpus must return at least one hit"
    );
    for h in hits {
        assert!(h["address"].is_string(), "each hit carries an address");
        assert!(h["similarity"].is_number(), "each hit carries a similarity");
    }
}

// ---------------------------------------------------------------------------
// Seeders for the corpus-touching tests (mirror tape_hydration_bridge.rs).
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
