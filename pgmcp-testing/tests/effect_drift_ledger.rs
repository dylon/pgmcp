//! Phase 3 — temporal effect-drift ledger (`symbol_effect_history`, v15).
//!
//! Exercises the query layer the `effect_drift` MCP tool and the
//! symbol-extraction cron's drift capture rely on:
//!   - `record_effect_drift` appends `gained` / `lost` rows for a file.
//!   - `query_effect_drift` reads them back newest-first with optional
//!     project / effect / change filters, enriched with project + path.
//!
//! CREATEDB-gated via `require_test_db!()` (self-skips where the harness
//! can't create a scratch database).

use pgmcp::db::queries;
use pgmcp_testing::require_test_db;

async fn seed(pool: &sqlx::PgPool) -> (i32, i64) {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name)
         VALUES ('/ws/p15_drift', '/ws/p15_drift/p', 'p15_drift_test')
         ON CONFLICT (path) DO UPDATE SET workspace_path = EXCLUDED.workspace_path
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("project");

    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files
             (project_id, path, relative_path, language, size_bytes,
              content, content_hash, line_count, modified_at)
         VALUES ($1, '/ws/p15_drift/p/src/lib.rs', 'src/lib.rs', 'rust', 64,
                 'unused', 5150, 1, NOW())
         ON CONFLICT (path) DO UPDATE SET content_hash = EXCLUDED.content_hash
         RETURNING id",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .expect("file");

    (project_id, file_id)
}

#[tokio::test(flavor = "multi_thread")]
async fn drift_round_trips_with_filters() {
    let testdb = require_test_db!();
    let pool = testdb.pool();
    let (_project_id, file_id) = seed(pool).await;

    // foo gained `unsafe`; bar lost `async`.
    let rows = vec![
        (
            "function".to_string(),
            "foo".to_string(),
            "unsafe".to_string(),
            "gained",
        ),
        (
            "function".to_string(),
            "bar".to_string(),
            "async".to_string(),
            "lost",
        ),
    ];
    let n = queries::record_effect_drift(pool, file_id, &rows)
        .await
        .expect("record drift");
    assert_eq!(n, 2, "two drift rows inserted");

    // Unfiltered (project-scoped): both rows, newest-first.
    let all = queries::query_effect_drift(pool, Some("p15_drift_test"), None, None, None, 50)
        .await
        .expect("query all");
    assert_eq!(all.len(), 2, "both drift rows returned");
    assert!(all.iter().all(|r| r.relative_path == "src/lib.rs"));
    assert!(all.iter().all(|r| r.project_name == "p15_drift_test"));

    // Filter by effect + change: only foo/unsafe/gained.
    let gained_unsafe = queries::query_effect_drift(
        pool,
        Some("p15_drift_test"),
        Some("unsafe"),
        Some("gained"),
        None,
        50,
    )
    .await
    .expect("query filtered");
    assert_eq!(gained_unsafe.len(), 1, "one unsafe/gained row");
    assert_eq!(gained_unsafe[0].symbol_name, "foo");
    assert_eq!(gained_unsafe[0].effect, "unsafe");
    assert_eq!(gained_unsafe[0].change, "gained");

    // Filter by change = lost: only bar/async.
    let lost =
        queries::query_effect_drift(pool, Some("p15_drift_test"), None, Some("lost"), None, 50)
            .await
            .expect("query lost");
    assert_eq!(lost.len(), 1, "one lost row");
    assert_eq!(lost[0].symbol_name, "bar");
    assert_eq!(lost[0].change, "lost");

    // Empty result for a non-matching effect.
    let none =
        queries::query_effect_drift(pool, Some("p15_drift_test"), Some("crypto"), None, None, 50)
            .await
            .expect("query none");
    assert!(none.is_empty(), "no rows for unmatched effect");
}
