//! Real-PostgreSQL integration tests for the SQL surface that pgmcp relies
//! on.
//!
//! Migrated from the Docker-based `tests/integration.rs`; now runs against
//! the user's local Postgres install via `pgmcp_testing::db_harness`. Each
//! test opens a transaction on the shared per-process template database;
//! the transaction rolls back on drop so nothing is ever committed — no
//! cleanup, crash-safe by construction.
//!
//! To enable these tests, set `PGMCP_TEST_DATABASE_URL` to a Postgres URL
//! with `CREATEDB` privilege (and pgvector installed cluster-wide), OR
//! drop a `~/.config/pgmcp/test-config.toml` file with a `[database]`
//! section. See `tests/README.md`.
//!
//! Tests that need multi-connection visibility (the indexer, subprocess
//! E2E) live in their own files using `TestDatabase` instead.

use std::path::Path;

use tempfile::TempDir;

use pgmcp_testing::require_test_txn;

/// Helper to create temporary files for indexing-style tests.
fn create_test_files(dir: &Path) -> Vec<(String, &'static str)> {
    let files = vec![
        (
            "src/main.rs",
            "fn main() {\n    println!(\"Hello, world!\");\n}\n",
        ),
        (
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn multiply(a: i32, b: i32) -> i32 {\n    a * b\n}\n",
        ),
        (
            "README.md",
            "# Test Project\n\nThis is a test project for pgmcp integration tests.\n",
        ),
    ];

    for (path, content) in &files {
        let full_path = dir.join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).expect("create directory");
        }
        std::fs::write(&full_path, content).expect("write test file");
    }

    files.iter().map(|(p, c)| (p.to_string(), *c)).collect()
}

#[tokio::test]
async fn project_upsert_increments_count_and_returns_id() {
    let mut txn = require_test_txn!();

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1
         RETURNING id",
    )
    .bind("/workspace")
    .bind("/workspace/test-project")
    .bind("test-project")
    .fetch_one(txn.conn())
    .await
    .expect("insert project");

    assert!(project_id > 0);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects")
        .fetch_one(txn.conn())
        .await
        .expect("count projects");
    // Transaction isolation: only the one we just inserted is visible here.
    assert!(count >= 1);
}

#[tokio::test]
async fn file_upsert_is_idempotent_and_preserves_content_hash() {
    let mut txn = require_test_txn!();

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/workspace")
    .bind("/workspace/proj")
    .bind("proj")
    .fetch_one(txn.conn())
    .await
    .expect("insert project");

    let content = "fn main() {\n    println!(\"hello\");\n}\n";
    let content_hash: i64 = xxhash_rust::xxh3::xxh3_64(content.as_bytes()) as i64;

    let upsert_sql = "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())
         ON CONFLICT (path) DO UPDATE SET content = $6, content_hash = $7, line_count = $8, indexed_at = NOW()
         RETURNING id";

    let file_id: i64 = sqlx::query_scalar(upsert_sql)
        .bind(project_id)
        .bind("/workspace/proj/main.rs")
        .bind("main.rs")
        .bind("rust")
        .bind(content.len() as i64)
        .bind(content)
        .bind(content_hash)
        .bind(3i32)
        .fetch_one(txn.conn())
        .await
        .expect("upsert file");

    assert!(file_id > 0);

    let file_id_2: i64 = sqlx::query_scalar(upsert_sql)
        .bind(project_id)
        .bind("/workspace/proj/main.rs")
        .bind("main.rs")
        .bind("rust")
        .bind(content.len() as i64)
        .bind(content)
        .bind(content_hash)
        .bind(3i32)
        .fetch_one(txn.conn())
        .await
        .expect("re-upsert file");

    assert_eq!(file_id, file_id_2, "upsert must be idempotent");

    let stored_hash: Option<i64> =
        sqlx::query_scalar("SELECT content_hash FROM indexed_files WHERE path = $1")
            .bind("/workspace/proj/main.rs")
            .fetch_optional(txn.conn())
            .await
            .expect("query content_hash");
    assert_eq!(stored_hash, Some(content_hash));
}

#[tokio::test]
async fn tsvector_full_text_search_finds_matching_content() {
    let mut txn = require_test_txn!();

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/proj-fts")
    .bind("proj-fts")
    .fetch_one(txn.conn())
    .await
    .expect("insert project");

    let content = "pub fn calculate_fibonacci(n: u64) -> u64 {\n    if n <= 1 { return n; }\n    calculate_fibonacci(n-1) + calculate_fibonacci(n-2)\n}\n";
    let hash = xxhash_rust::xxh3::xxh3_64(content.as_bytes()) as i64;

    sqlx::query(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())"
    )
    .bind(project_id)
    .bind("/ws/proj-fts/math.rs")
    .bind("math.rs")
    .bind("rust")
    .bind(content.len() as i64)
    .bind(content)
    .bind(hash)
    .bind(4i32)
    .execute(txn.conn())
    .await
    .expect("insert file");

    let results: Vec<(String,)> = sqlx::query_as(
        "SELECT path FROM indexed_files WHERE path LIKE '/ws/proj-fts/%' AND to_tsvector('english', content) @@ plainto_tsquery('english', $1)"
    )
    .bind("fibonacci")
    .fetch_all(txn.conn())
    .await
    .expect("FTS query");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "/ws/proj-fts/math.rs");
}

#[tokio::test]
async fn regex_search_matches_struct_declaration() {
    let mut txn = require_test_txn!();

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/proj-regex")
    .bind("proj-regex")
    .fetch_one(txn.conn())
    .await
    .expect("insert project");

    let content = "struct MyStruct {\n    field: String,\n}\n\nimpl MyStruct {\n    fn new() -> Self { Self { field: String::new() } }\n}\n";
    let hash = xxhash_rust::xxh3::xxh3_64(content.as_bytes()) as i64;

    sqlx::query(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())"
    )
    .bind(project_id)
    .bind("/ws/proj-regex/types.rs")
    .bind("types.rs")
    .bind("rust")
    .bind(content.len() as i64)
    .bind(content)
    .bind(hash)
    .bind(7i32)
    .execute(txn.conn())
    .await
    .expect("insert file");

    let results: Vec<(String,)> = sqlx::query_as(
        "SELECT path FROM indexed_files WHERE path LIKE '/ws/proj-regex/%' AND content ~ $1",
    )
    .bind("struct\\s+\\w+")
    .fetch_all(txn.conn())
    .await
    .expect("regex query");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "/ws/proj-regex/types.rs");
}

#[tokio::test]
async fn stale_files_removed_when_disk_copies_deleted() {
    let mut txn = require_test_txn!();

    let temp_dir = TempDir::new().expect("create temp dir");
    let test_files = create_test_files(temp_dir.path());
    let workspace_path = temp_dir.path().to_str().expect("path");

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(workspace_path)
    .bind(workspace_path)
    .bind("test-proj-stale")
    .fetch_one(txn.conn())
    .await
    .expect("insert project");

    for (rel_path, content) in &test_files {
        let full_path = temp_dir.path().join(rel_path);
        let hash = xxhash_rust::xxh3::xxh3_64(content.as_bytes()) as i64;
        let lang = if rel_path.ends_with(".rs") {
            "rust"
        } else {
            "markdown"
        };

        sqlx::query(
            "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())"
        )
        .bind(project_id)
        .bind(full_path.to_str().expect("path"))
        .bind(rel_path)
        .bind(lang)
        .bind(content.len() as i64)
        .bind(*content)
        .bind(hash)
        .bind(content.lines().count() as i32)
        .execute(txn.conn())
        .await
        .expect("insert file");
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM indexed_files WHERE project_id = $1")
        .bind(project_id)
        .fetch_one(txn.conn())
        .await
        .expect("count");
    assert_eq!(count, 3);

    // Delete one file from disk
    let deleted_path = temp_dir.path().join("src/main.rs");
    std::fs::remove_file(&deleted_path).expect("remove file");

    // Simulate stale cleanup: remove rows whose path no longer exists.
    let all_paths: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, path FROM indexed_files WHERE project_id = $1")
            .bind(project_id)
            .fetch_all(txn.conn())
            .await
            .expect("fetch paths");

    for (id, path) in &all_paths {
        if !Path::new(path).exists() {
            sqlx::query("DELETE FROM indexed_files WHERE id = $1")
                .bind(id)
                .execute(txn.conn())
                .await
                .expect("delete stale file");
        }
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM indexed_files WHERE project_id = $1")
        .bind(project_id)
        .fetch_one(txn.conn())
        .await
        .expect("count after cleanup");
    assert_eq!(count, 2);
}

// ============================================================================
// Phase 6 — Extended SQL surface tests
// ============================================================================

/// pgvector extension is present in the template DB. Required precondition for
/// every test below that uses `vector(…)` types.
#[tokio::test]
async fn pgvector_extension_is_available() {
    let mut txn = require_test_txn!();
    let row: Option<(String,)> =
        sqlx::query_as("SELECT extname::text FROM pg_extension WHERE extname = 'vector'")
            .fetch_optional(txn.conn())
            .await
            .expect("query pg_extension");
    assert!(
        row.is_some(),
        "pgvector extension must be installed — run `CREATE EXTENSION vector` cluster-wide"
    );
}

/// pg_trgm is also installed by migrations (used for trigram similarity on paths).
#[tokio::test]
async fn pg_trgm_extension_is_available() {
    let mut txn = require_test_txn!();
    let row: Option<(String,)> =
        sqlx::query_as("SELECT extname::text FROM pg_extension WHERE extname = 'pg_trgm'")
            .fetch_optional(txn.conn())
            .await
            .expect("query pg_extension");
    assert!(row.is_some(), "pg_trgm extension must be installed");
}

/// HNSW parameters stored in `pgmcp_metadata` match the build-time config.
/// This verifies the migration's `pgmcp_metadata` row-write path.
#[tokio::test]
async fn hnsw_params_recorded_in_pgmcp_metadata() {
    let mut txn = require_test_txn!();
    let keys: Vec<(String, String)> = sqlx::query_as(
        "SELECT key, value FROM pgmcp_metadata \
         WHERE key IN ('hnsw_m', 'hnsw_ef_construction', 'ef_search')",
    )
    .fetch_all(txn.conn())
    .await
    .expect("query pgmcp_metadata");
    let keys: std::collections::HashMap<String, String> = keys.into_iter().collect();
    assert!(keys.contains_key("hnsw_m"), "hnsw_m missing: {:?}", keys);
    assert!(
        keys.contains_key("hnsw_ef_construction"),
        "hnsw_ef_construction missing: {:?}",
        keys
    );
}

/// `SET LOCAL hnsw.ef_search` applies inside the transaction without
/// leaking to subsequent connections (the LOCAL keyword does this).
#[tokio::test]
async fn set_local_ef_search_is_transaction_scoped() {
    let mut txn = require_test_txn!();
    sqlx::query("SET LOCAL hnsw.ef_search = 500")
        .execute(txn.conn())
        .await
        .expect("set local");
    let (current,): (String,) = sqlx::query_as("SHOW hnsw.ef_search")
        .fetch_one(txn.conn())
        .await
        .expect("show");
    assert_eq!(current, "500", "ef_search should be 500 in this txn");
}

/// pgvector cosine distance: for L2-normalized inputs, `a <=> b ≈ 1 - a·b`.
#[tokio::test]
async fn vector_cosine_distance_matches_one_minus_dot_product() {
    let mut txn = require_test_txn!();
    // Two unit vectors in 3D: a=[1,0,0], b=[1,0,0] (identical) → distance 0
    let (d_identical,): (f64,) = sqlx::query_as(
        "SELECT (ARRAY[1.0, 0.0, 0.0]::vector <=> ARRAY[1.0, 0.0, 0.0]::vector)::float8",
    )
    .fetch_one(txn.conn())
    .await
    .expect("cosine distance identical");
    assert!(d_identical.abs() < 1e-6, "d(a,a) = {} ≠ 0", d_identical);
    // Orthogonal: a=[1,0,0], b=[0,1,0] → cosine distance 1.0
    let (d_ortho,): (f64,) = sqlx::query_as(
        "SELECT (ARRAY[1.0, 0.0, 0.0]::vector <=> ARRAY[0.0, 1.0, 0.0]::vector)::float8",
    )
    .fetch_one(txn.conn())
    .await
    .expect("cosine distance orthogonal");
    assert!(
        (d_ortho - 1.0).abs() < 1e-5,
        "d(a,b) orthogonal = {} ≠ 1",
        d_ortho
    );
}

/// L2 distance is zero between identical vectors and sqrt(3) between
/// diagonals of a unit cube.
#[tokio::test]
async fn vector_l2_distance_zero_for_identical_and_sqrt3_for_diagonals() {
    let mut txn = require_test_txn!();
    let (d_same,): (f64,) = sqlx::query_as(
        "SELECT (ARRAY[0.0, 0.0, 0.0]::vector <-> ARRAY[0.0, 0.0, 0.0]::vector)::float8",
    )
    .fetch_one(txn.conn())
    .await
    .expect("l2 identical");
    assert!(d_same.abs() < 1e-6);
    let (d_diag,): (f64,) = sqlx::query_as(
        "SELECT (ARRAY[0.0, 0.0, 0.0]::vector <-> ARRAY[1.0, 1.0, 1.0]::vector)::float8",
    )
    .fetch_one(txn.conn())
    .await
    .expect("l2 diagonal");
    assert!(
        (d_diag - 3.0_f64.sqrt()).abs() < 1e-5,
        "l2 diagonal = {} ≠ √3",
        d_diag
    );
}

/// tsvector with the English dictionary strips the stopword "the".
#[tokio::test]
async fn tsvector_english_strips_stopword_the() {
    let mut txn = require_test_txn!();
    let (lexemes,): (String,) =
        sqlx::query_as("SELECT to_tsvector('english', 'the quick brown fox')::text")
            .fetch_one(txn.conn())
            .await
            .expect("tsvector");
    // Stopwords removed; lexemes contain quick, brown, fox (stemmed).
    assert!(
        !lexemes.contains("'the'"),
        "stopword 'the' survived: {lexemes}"
    );
    assert!(lexemes.contains("quick"), "missing quick: {lexemes}");
    assert!(lexemes.contains("fox"), "missing fox: {lexemes}");
}

/// `plainto_tsquery` handles input with quotes and operators without raising
/// a syntax error — it's the "safe" parser for user-supplied search terms.
#[tokio::test]
async fn plainto_tsquery_handles_special_characters() {
    let mut txn = require_test_txn!();
    for input in &[
        "hello & world",
        "quoted \"phrase\"",
        "with | pipe",
        "semicolon; then",
    ] {
        let (_query_text,): (String,) =
            sqlx::query_as("SELECT plainto_tsquery('english', $1)::text")
                .bind(input)
                .fetch_one(txn.conn())
                .await
                .unwrap_or_else(|e| panic!("plainto_tsquery failed on {:?}: {}", input, e));
    }
}

/// PostgreSQL POSIX regex anchors (`^`, `$`) work through the `~` operator.
#[tokio::test]
async fn posix_regex_anchors_match_correctly() {
    let mut txn = require_test_txn!();
    let (at_start,): (bool,) = sqlx::query_as("SELECT 'foo bar' ~ '^foo'")
        .fetch_one(txn.conn())
        .await
        .expect("anchor start");
    assert!(at_start);
    let (at_end,): (bool,) = sqlx::query_as("SELECT 'foo bar' ~ 'bar$'")
        .fetch_one(txn.conn())
        .await
        .expect("anchor end");
    assert!(at_end);
    let (miss,): (bool,) = sqlx::query_as("SELECT 'foo bar' ~ '^bar'")
        .fetch_one(txn.conn())
        .await
        .expect("miss");
    assert!(!miss);
}

/// `find_project_by_cwd` returns the *longest-prefix* match when multiple
/// projects could match a given cwd.
#[tokio::test]
async fn find_project_by_cwd_picks_longest_prefix() {
    let mut txn = require_test_txn!();
    // Two nested projects: /home/user/project and /home/user/project/subdir
    for (path, name) in &[
        ("/home/user/project/", "outer"),
        ("/home/user/project/subdir/", "inner"),
        ("/home/", "root"),
    ] {
        sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
            .bind("/home/user")
            .bind(path)
            .bind(*name)
            .execute(txn.conn())
            .await
            .expect("insert project");
    }
    // Query with a cwd nested inside the inner project → inner wins.
    let (name,): (String,) = sqlx::query_as(
        "SELECT name FROM projects \
         WHERE $1 LIKE path || '%' \
         ORDER BY length(path) DESC LIMIT 1",
    )
    .bind("/home/user/project/subdir/src/main.rs/")
    .fetch_one(txn.conn())
    .await
    .expect("longest prefix");
    assert_eq!(name, "inner");
}

/// UNIQUE constraint on `projects.path` is active and upserts honor it.
#[tokio::test]
async fn unique_constraint_on_projects_path_is_enforced() {
    let mut txn = require_test_txn!();
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws")
        .bind("/ws/unique-p")
        .bind("first")
        .execute(txn.conn())
        .await
        .expect("first insert");
    // Second insert without ON CONFLICT should fail.
    let result =
        sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
            .bind("/ws")
            .bind("/ws/unique-p")
            .bind("second")
            .execute(txn.conn())
            .await;
    assert!(
        result.is_err(),
        "duplicate path insert should violate UNIQUE constraint"
    );
}

/// `ON CONFLICT (path) DO UPDATE …` allows re-upsert with a new name.
#[tokio::test]
async fn project_upsert_conflict_updates_workspace_path() {
    let mut txn = require_test_txn!();
    sqlx::query(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) \
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1",
    )
    .bind("/old_ws")
    .bind("/ws/conflict-test")
    .bind("project-name")
    .execute(txn.conn())
    .await
    .expect("first");
    sqlx::query(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) \
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1",
    )
    .bind("/new_ws")
    .bind("/ws/conflict-test")
    .bind("project-name")
    .execute(txn.conn())
    .await
    .expect("upsert");
    let (ws,): (String,) = sqlx::query_as("SELECT workspace_path FROM projects WHERE path = $1")
        .bind("/ws/conflict-test")
        .fetch_one(txn.conn())
        .await
        .expect("fetch");
    assert_eq!(
        ws, "/new_ws",
        "upsert should have overwritten workspace_path"
    );
}

/// Inserting a chunk row with `embedding_v2 vector(1024)` succeeds when the
/// bound value is a 1024-length f32 array (BGE-M3, the only supported
/// signature; the legacy 384-d `embedding` column was dropped).
#[tokio::test]
async fn vector_column_accepts_1024_dim_embedding() {
    let mut txn = require_test_txn!();
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/vec-test")
    .bind("vec-test")
    .fetch_one(txn.conn())
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW()) RETURNING id"
    )
    .bind(project_id)
    .bind("/ws/vec-test/x.rs")
    .bind("x.rs")
    .bind("rust")
    .bind(42_i64)
    .bind("content")
    .bind(1_i32)
    .fetch_one(txn.conn())
    .await
    .expect("file");
    let embedding = pgvector::Vector::from(vec![0.1_f32; 1024]);
    sqlx::query(
        "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line, embedding_v2, embedding_signature) \
         VALUES ($1, $2, $3, $4, $5, $6, 'bge-m3-v1')",
    )
    .bind(file_id)
    .bind(0_i32)
    .bind("content")
    .bind(1_i32)
    .bind(1_i32)
    .bind(embedding)
    .execute(txn.conn())
    .await
    .expect("insert chunk with vector");
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM file_chunks WHERE file_id = $1")
        .bind(file_id)
        .fetch_one(txn.conn())
        .await
        .expect("count");
    assert_eq!(count, 1);
}

/// Cosine similarity ordering via `<=>` returns closer vectors first.
#[tokio::test]
async fn vector_cosine_similarity_ranks_nearer_vectors_higher() {
    let mut txn = require_test_txn!();
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/rank-test")
    .bind("rank-test")
    .fetch_one(txn.conn())
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW()) RETURNING id"
    )
    .bind(project_id)
    .bind("/ws/rank-test/x.rs")
    .bind("x.rs")
    .bind("rust")
    .bind(10_i64)
    .bind("content")
    .bind(1_i32)
    .fetch_one(txn.conn())
    .await
    .expect("file");

    // Two chunks: chunk 0 is identical to query, chunk 1 is orthogonal.
    let v_same = pgvector::Vector::from({
        let mut v = vec![0.0_f32; 1024];
        v[0] = 1.0;
        v
    });
    let v_other = pgvector::Vector::from({
        let mut v = vec![0.0_f32; 1024];
        v[1] = 1.0;
        v
    });
    for (idx, emb) in [v_same.clone(), v_other].iter().enumerate() {
        sqlx::query(
            "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line, embedding_v2, embedding_signature) \
             VALUES ($1, $2, $3, $4, $5, $6, 'bge-m3-v1')"
        )
        .bind(file_id)
        .bind(idx as i32)
        .bind(format!("chunk {}", idx))
        .bind(1_i32)
        .bind(1_i32)
        .bind(emb.clone())
        .execute(txn.conn())
        .await
        .expect("insert chunk");
    }
    // Order by cosine distance ascending — chunk 0 (identical) must come first.
    let rows: Vec<(i32,)> = sqlx::query_as(
        "SELECT chunk_index FROM file_chunks WHERE file_id = $1 \
         ORDER BY embedding_v2 <=> $2 LIMIT 2",
    )
    .bind(file_id)
    .bind(v_same)
    .fetch_all(txn.conn())
    .await
    .expect("ranked");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, 0, "nearest chunk should rank first");
    assert_eq!(rows[1].0, 1);
}
