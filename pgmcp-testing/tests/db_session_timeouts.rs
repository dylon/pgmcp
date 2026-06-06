//! Tier 1a verification — confirm the production pool actually applies
//! the per-session timeouts configured on `DatabaseConfig`.
//!
//! Two checks:
//! 1. `statement_timeout` causes a long `SELECT pg_sleep(...)` to fail
//!    with SQLSTATE `57014` (query_canceled).
//! 2. `SET LOCAL statement_timeout` inside a transaction overrides the
//!    daemon-wide ceiling, so legitimate long analytic queries succeed.

use pgmcp::config::DatabaseConfig;
use pgmcp::db::DbClient;
use pgmcp::db::pool;
use pgmcp::db::queries;
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn session_statement_timeout_cancels_long_query() {
    let db = require_test_db!();
    let mut cfg = database_config_from_url(&db.connection_url());
    cfg.statement_timeout_ms = 500;
    cfg.test_before_acquire = false;
    cfg.max_connections = 1;

    let pool = pool::create_pool(&cfg)
        .await
        .expect("create_pool should succeed with short statement_timeout");

    let result = sqlx::query("SELECT pg_sleep(5)").execute(&pool).await;
    let err = result.expect_err("pg_sleep(5) must fail under a 500ms statement_timeout");
    let sqlstate = match &err {
        sqlx::Error::Database(db_err) => db_err.code().map(|s| s.into_owned()),
        _ => None,
    };
    assert_eq!(
        sqlstate.as_deref(),
        Some("57014"),
        "expected SQLSTATE 57014 (query_canceled), got error: {err}",
    );
}

#[tokio::test]
async fn set_local_statement_timeout_overrides_default() {
    let db = require_test_db!();
    let mut cfg = database_config_from_url(&db.connection_url());
    cfg.statement_timeout_ms = 500;
    cfg.test_before_acquire = false;
    cfg.max_connections = 1;

    let pool = pool::create_pool(&cfg)
        .await
        .expect("create_pool should succeed with short statement_timeout");

    let mut tx = pool.begin().await.expect("BEGIN");
    sqlx::query("SET LOCAL statement_timeout = '10s'")
        .execute(&mut *tx)
        .await
        .expect("SET LOCAL must succeed inside the transaction");
    sqlx::query("SELECT pg_sleep(1)")
        .execute(&mut *tx)
        .await
        .expect("pg_sleep(1) under SET LOCAL 10s must succeed despite daemon-wide 500ms cap");
    tx.commit().await.expect("COMMIT");
}

#[tokio::test]
async fn text_search_bounded_finds_content_via_stored_tsv() {
    // Proves (a) the v13 `content_tsv` stored column exists in the migrated
    // schema and (b) `text_search_bounded` — the transaction + `SET LOCAL`
    // wrapper hybrid_search's text leg uses — returns correct results under a
    // generous per-call statement_timeout. `content_tsv` is GENERATED, so it
    // auto-populates from `content` (no explicit insert of the tsvector).
    let db = require_test_db!();
    let pool = db.pool().clone();

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) \
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1 RETURNING id",
    )
    .bind("/ws/tsb")
    .bind("/ws/tsb/p")
    .bind("tsb_proj")
    .fetch_one(&pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, 'rust', $4, $5, $6, $7, NOW()) \
         ON CONFLICT (path) DO UPDATE SET content = $5 RETURNING id",
    )
    .bind(project_id)
    .bind("/ws/tsb/p/src/lib.rs")
    .bind("src/lib.rs")
    .bind(64_i64)
    .bind("the quick brown fox jumps")
    .bind(4242_i64)
    .bind(1_i32)
    .fetch_one(&pool)
    .await
    .expect("file");
    // Mirror the known-good chunk-insert shape (embedding columns are nullable
    // but populated here exactly as the production indexer would).
    let embedding = pgvector::Vector::from(vec![0.0_f32; 1024]);
    sqlx::query(
        "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line, embedding_v2, embedding_signature) \
         VALUES ($1, 0, $2, 1, 1, $3, 'bge-m3-v1')",
    )
    .bind(file_id)
    .bind("the quick brown fox jumps over the lazy dog")
    .bind(embedding)
    .execute(&pool)
    .await
    .expect("chunk");

    let hits = pool
        .text_search_bounded("brown fox", 10, None, None, false, 5_000)
        .await
        .expect("text_search_bounded must succeed under a 5s budget");
    assert!(
        hits.iter().any(|h| h.relative_path == "src/lib.rs"),
        "stored-tsv full-text search must find the seeded chunk; got {hits:?}"
    );
}

#[tokio::test]
async fn replace_indexed_file_rolls_back_when_chunk_delete_hits_lock_timeout() {
    let db = require_test_db!();
    let mut cfg = database_config_from_url(&db.connection_url());
    cfg.lock_timeout_ms = 250;
    cfg.statement_timeout_ms = 5_000;
    cfg.test_before_acquire = false;
    cfg.max_connections = 4;

    let pool = pool::create_pool(&cfg)
        .await
        .expect("create_pool should succeed with short lock_timeout");

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name)
         VALUES ('/ws/replace-lock', '/ws/replace-lock/project', 'replace-lock')
         ON CONFLICT (path) DO UPDATE SET name = EXCLUDED.name
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files
            (project_id, path, relative_path, language, size_bytes, content,
             content_hash, line_count, truncated, content_recoverable_from_disk,
             modified_at)
         VALUES
            ($1, '/ws/replace-lock/project/src/lib.rs', 'src/lib.rs', 'rust',
             64, 'old content', 111, 1, false, false, NOW())
         ON CONFLICT (path) DO UPDATE SET
             project_id = EXCLUDED.project_id,
             content = EXCLUDED.content,
             content_hash = EXCLUDED.content_hash,
             modified_at = NOW()
         RETURNING id",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .expect("file");
    let old_embedding = pgvector::Vector::from(vec![0.0_f32; 1024]);
    let chunk_id: i64 = sqlx::query_scalar(
        "INSERT INTO file_chunks
            (file_id, chunk_index, content, start_line, end_line,
             embedding_v2, embedding_signature)
         VALUES ($1, 0, 'old chunk', 1, 1, $2, 'bge-m3-v1')
         ON CONFLICT (file_id, chunk_index) DO UPDATE SET
             content = EXCLUDED.content,
             embedding_v2 = EXCLUDED.embedding_v2,
             embedding_signature = EXCLUDED.embedding_signature
         RETURNING id",
    )
    .bind(file_id)
    .bind(old_embedding)
    .fetch_one(&pool)
    .await
    .expect("chunk");

    let mut holder = pool.begin().await.expect("BEGIN holder");
    sqlx::query("SELECT id FROM file_chunks WHERE id = $1 FOR KEY SHARE")
        .bind(chunk_id)
        .fetch_one(&mut *holder)
        .await
        .expect("hold key-share lock on old chunk");

    let replacement_embedding = vec![1.0_f32; 1024];
    let replacement_chunks = [queries::ChunkInsert {
        chunk_index: 0,
        content: "new chunk",
        start_line: 1,
        end_line: 1,
        embedding: replacement_embedding.as_slice(),
    }];
    let err = queries::replace_indexed_file(
        &pool,
        queries::IndexedFileReplacement {
            project_id,
            path: "/ws/replace-lock/project/src/lib.rs",
            relative_path: "src/lib.rs",
            language: "rust",
            size_bytes: 64,
            content: Some("new content"),
            content_hash: 222,
            line_count: 1,
            truncated: false,
            content_recoverable_from_disk: false,
            modified_at: chrono::Utc::now(),
            chunks: &replacement_chunks,
        },
    )
    .await
    .expect_err("chunk delete should hit lock_timeout while key-share holder is active");
    let sqlstate = match &err {
        sqlx::Error::Database(db_err) => db_err.code().map(|s| s.into_owned()),
        _ => None,
    };
    assert_eq!(
        sqlstate.as_deref(),
        Some("55P03"),
        "expected SQLSTATE 55P03 (lock_not_available), got error: {err}"
    );

    holder.rollback().await.expect("release holder");

    let (content_hash, content): (Option<i64>, Option<String>) =
        sqlx::query_as("SELECT content_hash, content FROM indexed_files WHERE id = $1")
            .bind(file_id)
            .fetch_one(&pool)
            .await
            .expect("file state");
    assert_eq!(
        content_hash,
        Some(111),
        "failed replace must roll back hash"
    );
    assert_eq!(content.as_deref(), Some("old content"));

    let chunks: Vec<String> = sqlx::query_scalar(
        "SELECT content FROM file_chunks WHERE file_id = $1 ORDER BY chunk_index",
    )
    .bind(file_id)
    .fetch_all(&pool)
    .await
    .expect("chunks state");
    assert_eq!(chunks, vec!["old chunk".to_string()]);
}

fn database_config_from_url(url: &str) -> DatabaseConfig {
    let without_scheme = url
        .strip_prefix("postgres://")
        .expect("test harness uses postgres:// URLs");
    let (authority, name) = without_scheme
        .rsplit_once('/')
        .expect("test harness URL includes database name");
    let (userinfo, host_port) = authority
        .rsplit_once('@')
        .map_or((None, authority), |(user, host)| (Some(user), host));
    let (user, password) = match userinfo.and_then(|u| u.split_once(':')) {
        Some((user, password)) => (user.to_string(), Some(password.to_string())),
        None => (
            userinfo
                .map(str::to_string)
                .unwrap_or_else(|| DatabaseConfig::default().user),
            None,
        ),
    };
    let (host, port) = host_port
        .rsplit_once(':')
        .map_or((host_port, 5432), |(h, p)| {
            (h, p.parse::<u16>().expect("test DB port is numeric"))
        });
    DatabaseConfig {
        host: host.to_string(),
        port,
        name: name.to_string(),
        user,
        password,
        ..DatabaseConfig::default()
    }
}
