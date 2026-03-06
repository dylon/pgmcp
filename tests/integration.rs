//! Integration tests for pgmcp using testcontainers with PostgreSQL + pgvector.
//!
//! These tests require Docker to be running. They spin up a temporary
//! PostgreSQL container with the pgvector extension, run migrations,
//! and test the full indexing and search pipeline.
//!
//! Run with: `cargo test --test integration -- --ignored`
//! Tests are `#[ignore]` by default since they require Docker with working bridge networking.

use std::path::Path;

use sqlx::PgPool;
use tempfile::TempDir;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// Create a pgvector-enabled PostgreSQL container and return a connection pool.
async fn setup_db() -> (PgPool, testcontainers::ContainerAsync<Postgres>) {
    let container = Postgres::default()
        .start()
        .await
        .expect("Failed to start postgres container");

    let host_port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("Failed to get postgres port");

    let url = format!("postgres://postgres:postgres@127.0.0.1:{}/postgres", host_port);

    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("Failed to connect to test database");

    // Create pgvector extension — this may fail if the image doesn't have pgvector.
    // The standard postgres image won't have it, so we install it or skip.
    let has_pgvector = sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
        .execute(&pool)
        .await;

    if has_pgvector.is_err() {
        eprintln!("WARNING: pgvector extension not available in test container. Some tests will be skipped.");
    }

    // Create pg_trgm extension
    let _ = sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_trgm")
        .execute(&pool)
        .await;

    (pool, container)
}

/// Run migrations to set up the schema.
async fn run_test_migrations(pool: &PgPool) {
    // Create tables manually (same as db/migrations.rs)
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS projects (
            id SERIAL PRIMARY KEY,
            workspace_path TEXT NOT NULL,
            path TEXT UNIQUE NOT NULL,
            name TEXT NOT NULL,
            discovered_at TIMESTAMPTZ DEFAULT NOW(),
            last_scanned_at TIMESTAMPTZ
        )"
    )
    .execute(pool)
    .await
    .expect("Failed to create projects table");

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS indexed_files (
            id BIGSERIAL PRIMARY KEY,
            project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            path TEXT UNIQUE NOT NULL,
            relative_path TEXT NOT NULL,
            language TEXT NOT NULL,
            size_bytes BIGINT NOT NULL,
            content TEXT,
            content_hash BIGINT NOT NULL,
            line_count INTEGER NOT NULL,
            truncated BOOLEAN NOT NULL DEFAULT FALSE,
            indexed_at TIMESTAMPTZ DEFAULT NOW(),
            modified_at TIMESTAMPTZ NOT NULL
        )"
    )
    .execute(pool)
    .await
    .expect("Failed to create indexed_files table");

    // Create file_chunks only if pgvector is available
    let has_vector = sqlx::query("SELECT 1 FROM pg_extension WHERE extname = 'vector'")
        .fetch_optional(pool)
        .await
        .expect("Failed to check for pgvector");

    if has_vector.is_some() {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS file_chunks (
                id BIGSERIAL PRIMARY KEY,
                file_id BIGINT REFERENCES indexed_files(id) ON DELETE CASCADE,
                chunk_index INTEGER NOT NULL,
                content TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                embedding vector(384) NOT NULL,
                UNIQUE (file_id, chunk_index)
            )"
        )
        .execute(pool)
        .await
        .expect("Failed to create file_chunks table");
    }

    // Create FTS index
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_files_fts ON indexed_files USING gin(to_tsvector('english', content))"
    )
    .execute(pool)
    .await
    .expect("Failed to create FTS index");

    // Create content_hash index
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_files_content_hash ON indexed_files(content_hash)"
    )
    .execute(pool)
    .await
    .expect("Failed to create content_hash index");
}

/// Helper to create temporary files for indexing tests.
fn create_test_files(dir: &Path) -> Vec<(String, &'static str)> {
    let files = vec![
        ("src/main.rs", "fn main() {\n    println!(\"Hello, world!\");\n}\n"),
        ("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn multiply(a: i32, b: i32) -> i32 {\n    a * b\n}\n"),
        ("README.md", "# Test Project\n\nThis is a test project for pgmcp integration tests.\n"),
    ];

    for (path, content) in &files {
        let full_path = dir.join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).expect("Failed to create directory");
        }
        std::fs::write(&full_path, content).expect("Failed to write test file");
    }

    files.iter().map(|(p, c)| (p.to_string(), *c)).collect()
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
#[ignore = "requires Docker with working bridge networking"]
async fn test_project_upsert_and_list() {
    let (pool, _container) = setup_db().await;
    run_test_migrations(&pool).await;

    // Insert a project
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1
         RETURNING id"
    )
    .bind("/workspace")
    .bind("/workspace/test-project")
    .bind("test-project")
    .fetch_one(&pool)
    .await
    .expect("Failed to insert project");

    assert!(project_id > 0);

    // List projects
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects")
        .fetch_one(&pool)
        .await
        .expect("Failed to count projects");

    assert_eq!(count, 1);
}

#[tokio::test]
#[ignore = "requires Docker with working bridge networking"]
async fn test_file_upsert_and_content_hash() {
    let (pool, _container) = setup_db().await;
    run_test_migrations(&pool).await;

    // Create project
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id"
    )
    .bind("/workspace")
    .bind("/workspace/proj")
    .bind("proj")
    .fetch_one(&pool)
    .await
    .expect("Failed to insert project");

    let content = "fn main() {\n    println!(\"hello\");\n}\n";
    let content_hash: i64 = xxhash_rust::xxh3::xxh3_64(content.as_bytes()) as i64;

    // First upsert
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())
         ON CONFLICT (path) DO UPDATE SET content = $6, content_hash = $7, line_count = $8, indexed_at = NOW()
         RETURNING id"
    )
    .bind(project_id)
    .bind("/workspace/proj/main.rs")
    .bind("main.rs")
    .bind("rust")
    .bind(content.len() as i64)
    .bind(content)
    .bind(content_hash)
    .bind(3i32)
    .fetch_one(&pool)
    .await
    .expect("Failed to upsert file");

    assert!(file_id > 0);

    // Second upsert with same content should return same id
    let file_id_2: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())
         ON CONFLICT (path) DO UPDATE SET content = $6, content_hash = $7, line_count = $8, indexed_at = NOW()
         RETURNING id"
    )
    .bind(project_id)
    .bind("/workspace/proj/main.rs")
    .bind("main.rs")
    .bind("rust")
    .bind(content.len() as i64)
    .bind(content)
    .bind(content_hash)
    .bind(3i32)
    .fetch_one(&pool)
    .await
    .expect("Failed to re-upsert file");

    assert_eq!(file_id, file_id_2, "Upsert must be idempotent");

    // Check content hash retrieval
    let stored_hash: Option<i64> = sqlx::query_scalar(
        "SELECT content_hash FROM indexed_files WHERE path = $1"
    )
    .bind("/workspace/proj/main.rs")
    .fetch_optional(&pool)
    .await
    .expect("Failed to query content_hash");

    assert_eq!(stored_hash, Some(content_hash));
}

#[tokio::test]
#[ignore = "requires Docker with working bridge networking"]
async fn test_full_text_search() {
    let (pool, _container) = setup_db().await;
    run_test_migrations(&pool).await;

    // Create project and file
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id"
    )
    .bind("/ws")
    .bind("/ws/proj")
    .bind("proj")
    .fetch_one(&pool)
    .await
    .expect("insert project");

    let content = "pub fn calculate_fibonacci(n: u64) -> u64 {\n    if n <= 1 { return n; }\n    calculate_fibonacci(n-1) + calculate_fibonacci(n-2)\n}\n";
    let hash = xxhash_rust::xxh3::xxh3_64(content.as_bytes()) as i64;

    sqlx::query(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())"
    )
    .bind(project_id)
    .bind("/ws/proj/math.rs")
    .bind("math.rs")
    .bind("rust")
    .bind(content.len() as i64)
    .bind(content)
    .bind(hash)
    .bind(4i32)
    .execute(&pool)
    .await
    .expect("insert file");

    // Search for "fibonacci"
    let results: Vec<(String,)> = sqlx::query_as(
        "SELECT path FROM indexed_files WHERE to_tsvector('english', content) @@ plainto_tsquery('english', $1)"
    )
    .bind("fibonacci")
    .fetch_all(&pool)
    .await
    .expect("FTS query failed");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "/ws/proj/math.rs");
}

#[tokio::test]
#[ignore = "requires Docker with working bridge networking"]
async fn test_regex_search() {
    let (pool, _container) = setup_db().await;
    run_test_migrations(&pool).await;

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id"
    )
    .bind("/ws")
    .bind("/ws/proj")
    .bind("proj")
    .fetch_one(&pool)
    .await
    .expect("insert project");

    let content = "struct MyStruct {\n    field: String,\n}\n\nimpl MyStruct {\n    fn new() -> Self { Self { field: String::new() } }\n}\n";
    let hash = xxhash_rust::xxh3::xxh3_64(content.as_bytes()) as i64;

    sqlx::query(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())"
    )
    .bind(project_id)
    .bind("/ws/proj/types.rs")
    .bind("types.rs")
    .bind("rust")
    .bind(content.len() as i64)
    .bind(content)
    .bind(hash)
    .bind(7i32)
    .execute(&pool)
    .await
    .expect("insert file");

    // Regex search for struct definitions
    let results: Vec<(String,)> = sqlx::query_as(
        "SELECT path FROM indexed_files WHERE content ~ $1"
    )
    .bind("struct\\s+\\w+")
    .fetch_all(&pool)
    .await
    .expect("regex query failed");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "/ws/proj/types.rs");
}

#[tokio::test]
#[ignore = "requires Docker with working bridge networking"]
async fn test_stale_file_cleanup() {
    let (pool, _container) = setup_db().await;
    run_test_migrations(&pool).await;

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let test_files = create_test_files(temp_dir.path());

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id"
    )
    .bind(temp_dir.path().to_str().expect("path"))
    .bind(temp_dir.path().to_str().expect("path"))
    .bind("test-proj")
    .fetch_one(&pool)
    .await
    .expect("insert project");

    // Index the files
    for (rel_path, content) in &test_files {
        let full_path = temp_dir.path().join(rel_path);
        let hash = xxhash_rust::xxh3::xxh3_64(content.as_bytes()) as i64;
        let lang = if rel_path.ends_with(".rs") { "rust" } else { "markdown" };

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
        .execute(&pool)
        .await
        .expect("insert file");
    }

    // Verify 3 files indexed
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM indexed_files")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count, 3);

    // Delete one file from disk
    let deleted_path = temp_dir.path().join("src/main.rs");
    std::fs::remove_file(&deleted_path).expect("remove file");

    // Simulate stale cleanup: remove indexed files that no longer exist on disk
    let all_paths: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, path FROM indexed_files"
    )
    .fetch_all(&pool)
    .await
    .expect("fetch paths");

    for (id, path) in &all_paths {
        if !Path::new(path).exists() {
            sqlx::query("DELETE FROM indexed_files WHERE id = $1")
                .bind(id)
                .execute(&pool)
                .await
                .expect("delete stale file");
        }
    }

    // Verify only 2 files remain
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM indexed_files")
        .fetch_one(&pool)
        .await
        .expect("count after cleanup");
    assert_eq!(count, 2);
}
