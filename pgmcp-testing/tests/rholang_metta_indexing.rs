//! End-to-end indexing integration test for the Rholang and MeTTa
//! tree-sitter backends.
//!
//! Seeds a project with one Rholang file and one MeTTa file (real content),
//! runs the symbol-extraction cron directly, and asserts that
//! `file_symbols` + `symbol_references` rows landed for both languages.
//!
//! Requires the test database harness (run via `cargo test --test
//! rholang_metta_indexing -- --include-ignored`). The test is **not** marked
//! `#[ignore]` because `require_test_db!` already skips gracefully when no
//! test DB is available.

use std::sync::Arc;

use pgmcp::cron::symbol_extraction;
use pgmcp::db::DbClient;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

const RHOLANG_HELLO: &str = "new stdout(`rho:io:stdout`) in {\n\
  contract helloworld(@name) = {\n\
    stdout!(\"hello, \" ++ name)\n\
  }\n\
  |\n\
  helloworld!(\"world\")\n\
}\n";

const RHOLANG_BANK: &str = "new bank, ack in {\n\
  contract bank(@\"deposit\", @amount, return) = {\n\
    return!(amount)\n\
  } |\n\
  contract bank(@\"withdraw\", @amount, return) = {\n\
    return!(amount)\n\
  } |\n\
  bank!(\"deposit\", 100, *ack)\n\
}\n";

const METTA_SAMPLE: &str = "(: Nat Type)\n\
(: Z Nat)\n\
(: S (-> Nat Nat))\n\
(= (add Z $y) $y)\n\
(= (add (S $x) $y) (S (add $x $y)))\n\
!(import! &self stdlib)\n\
!(add (S Z) (S (S Z)))\n";

async fn seed_project(pool: &PgPool, name: &str, path: &str) -> i32 {
    sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET name = $3 RETURNING id",
    )
    .bind("/ws")
    .bind(path)
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("project")
}

async fn seed_file_with_content(
    pool: &PgPool,
    project_id: i32,
    path: &str,
    relative_path: &str,
    language: &str,
    content: &str,
) -> i64 {
    let line_count = content.lines().count() as i32;
    let size_bytes = content.len() as i64;
    sqlx::query_scalar(
        "INSERT INTO indexed_files \
            (project_id, path, relative_path, language, size_bytes, content, \
             content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW()) \
         ON CONFLICT (path) DO UPDATE SET \
            language = $4, content = $6, content_hash = $7, \
            line_count = $8, modified_at = NOW() \
         RETURNING id",
    )
    .bind(project_id)
    .bind(path)
    .bind(relative_path)
    .bind(language)
    .bind(size_bytes)
    .bind(content)
    .bind(content_hash(content))
    .bind(line_count)
    .fetch_one(pool)
    .await
    .expect("file")
}

fn content_hash(content: &str) -> i64 {
    // Simple FNV-1a — sufficient as a unique key for tests.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in content.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash as i64
}

#[tokio::test(flavor = "multi_thread")]
async fn rholang_and_metta_indexing_e2e() {
    let db = require_test_db!();
    let pool = db.pool();
    let project_id = seed_project(pool, "rho-metta-test", "/ws/rho-metta-test").await;

    let rho_id_1 = seed_file_with_content(
        pool,
        project_id,
        "/ws/rho-metta-test/hello.rho",
        "hello.rho",
        "rholang",
        RHOLANG_HELLO,
    )
    .await;
    let rho_id_2 = seed_file_with_content(
        pool,
        project_id,
        "/ws/rho-metta-test/bank.rho",
        "bank.rho",
        "rholang",
        RHOLANG_BANK,
    )
    .await;
    let metta_id = seed_file_with_content(
        pool,
        project_id,
        "/ws/rho-metta-test/peano.metta",
        "peano.metta",
        "metta",
        METTA_SAMPLE,
    )
    .await;

    let db_client: Arc<dyn DbClient> = Arc::new(pool.clone());
    let stats = Arc::new(StatsTracker::new());
    symbol_extraction::run_symbol_extraction(db_client.as_ref(), &stats).await;

    // Assert Rholang file_symbols rows.
    let rholang_symbol_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM file_symbols WHERE file_id = ANY($1::bigint[])")
            .bind(&[rho_id_1, rho_id_2])
            .fetch_one(pool)
            .await
            .expect("rholang symbol count");
    assert!(
        rholang_symbol_count > 0,
        "expected file_symbols rows for Rholang files, got {}",
        rholang_symbol_count
    );

    let helloworld_present: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM file_symbols WHERE file_id = $1 AND name = 'helloworld' AND kind = 'function')",
    )
    .bind(rho_id_1)
    .fetch_one(pool)
    .await
    .expect("helloworld lookup");
    assert!(
        helloworld_present,
        "expected `helloworld` contract symbol in file_symbols for rho_id_1"
    );

    let bank_contract_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM file_symbols WHERE file_id = $1 AND name = 'bank' AND kind = 'function'",
    )
    .bind(rho_id_2)
    .fetch_one(pool)
    .await
    .expect("bank contract count");
    assert!(
        bank_contract_count >= 2,
        "expected ≥2 `bank` contract symbols (overloaded), got {}",
        bank_contract_count
    );

    // Assert MeTTa file_symbols rows.
    let metta_symbol_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM file_symbols WHERE file_id = $1")
            .bind(metta_id)
            .fetch_one(pool)
            .await
            .expect("metta symbol count");
    assert!(
        metta_symbol_count > 0,
        "expected file_symbols rows for MeTTa file, got {}",
        metta_symbol_count
    );

    let add_function_present: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM file_symbols WHERE file_id = $1 AND name = 'add' AND kind = 'function')",
    )
    .bind(metta_id)
    .fetch_one(pool)
    .await
    .expect("add function lookup");
    assert!(
        add_function_present,
        "expected `add` function symbol from MeTTa rule definition"
    );

    let nat_type_present: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM file_symbols WHERE file_id = $1 AND name = 'Nat' AND kind = 'trait')",
    )
    .bind(metta_id)
    .fetch_one(pool)
    .await
    .expect("Nat type lookup");
    assert!(
        nat_type_present,
        "expected `Nat` trait symbol from MeTTa type annotation"
    );

    // Assert symbol_references rows for both languages.
    let rholang_ref_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM symbol_references WHERE source_file_id = ANY($1::bigint[])",
    )
    .bind(&[rho_id_1, rho_id_2])
    .fetch_one(pool)
    .await
    .expect("rholang reference count");
    assert!(
        rholang_ref_count > 0,
        "expected symbol_references rows for Rholang files, got {}",
        rholang_ref_count
    );

    let metta_ref_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM symbol_references WHERE source_file_id = $1")
            .bind(metta_id)
            .fetch_one(pool)
            .await
            .expect("metta reference count");
    assert!(
        metta_ref_count > 0,
        "expected symbol_references rows for MeTTa file, got {}",
        metta_ref_count
    );
}
