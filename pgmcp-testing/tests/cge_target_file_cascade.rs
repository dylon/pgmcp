//! Regression: deleting an indexed file must not fail with
//! `duplicate key value violates unique constraint "idx_cge_unique"`.
//!
//! Before the fix, `code_graph_edges.target_file_id` used
//! `ON DELETE SET NULL`. Because `target_file_id` is a member of the unique
//! index `idx_cge_unique` via `COALESCE(target_file_id, -1)`, deleting a file
//! that an edge pointed at nulled the column on the surviving edge, collapsing
//! its key to `(source, -1, edge_type, COALESCE(target_raw,''))`. A second
//! such deletion from the same source produced a duplicate key and failed the
//! parent `DELETE FROM indexed_files` — surfacing as "Failed to delete file
//! from index" from `pgmcp::embed::pool`, notably on rotating
//! `~/.claude/sessions/*.json` files. The FK is now `ON DELETE CASCADE`.
//!
//! See docs/scientific-ledger/idx-cge-unique-set-null-collision-2026-05-27.md.

use pgmcp::db::queries;
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_referenced_file_does_not_violate_idx_cge_unique() {
    let testdb = require_test_db!();
    let pool = testdb.pool();

    // Seed a project with three files: A (edge source), B and C (edge targets).
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name)
         VALUES ('/ws/cge_cascade', '/ws/cge_cascade/p', 'cge_cascade_test')
         ON CONFLICT (path) DO UPDATE SET workspace_path = EXCLUDED.workspace_path
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("project");

    let mut ids: Vec<i64> = Vec::with_capacity(3);
    let mut paths: Vec<String> = Vec::with_capacity(3);
    for (rel, hash) in [("a.rs", 7001_i64), ("b.rs", 7002), ("c.rs", 7003)] {
        let path = format!("/ws/cge_cascade/p/{rel}");
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO indexed_files
                 (project_id, path, relative_path, language, size_bytes,
                  content, content_hash, line_count, modified_at)
             VALUES ($1, $2, $3, 'rust', 64, 'unused', $4, 1, NOW())
             ON CONFLICT (path) DO UPDATE SET content_hash = EXCLUDED.content_hash
             RETURNING id",
        )
        .bind(project_id)
        .bind(&path)
        .bind(rel)
        .bind(hash)
        .fetch_one(pool)
        .await
        .expect("file");
        ids.push(id);
        paths.push(path);
    }
    let file_a = ids[0];

    // Two semantic edges A→B and A→C. Semantic edges carry NULL target_raw, so
    // the two keys differ ONLY by target_file_id: (A,B,'semantic','') and
    // (A,C,'semantic',''). Under the old SET NULL FK, nulling B then C would
    // collapse both to (A,-1,'semantic','') and collide on idx_cge_unique.
    for target in [ids[1], ids[2]] {
        sqlx::query(
            "INSERT INTO code_graph_edges
                 (project_id, source_file_id, target_file_id, edge_type, target_raw, weight)
             VALUES ($1, $2, $3, 'semantic', NULL, 1.0)
             ON CONFLICT DO NOTHING",
        )
        .bind(project_id)
        .bind(file_a)
        .bind(target)
        .execute(pool)
        .await
        .expect("semantic edge");
    }

    // The FK must be CASCADE — the harness ran run_migrations, which re-tightens
    // it. confdeltype: 'c' = cascade, 'n' = set null.
    let confdeltype: String = sqlx::query_scalar(
        "SELECT confdeltype::text
           FROM pg_constraint c
           JOIN pg_attribute a
             ON a.attrelid = c.conrelid AND a.attnum = ANY (c.conkey)
          WHERE c.conrelid = 'code_graph_edges'::regclass
            AND c.contype = 'f'
            AND a.attname = 'target_file_id'
          LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .expect("target_file_id FK lookup");
    assert_eq!(
        confdeltype, "c",
        "code_graph_edges.target_file_id FK must be ON DELETE CASCADE"
    );

    // Delete B, then C. Pre-fix, the second delete failed with idx_cge_unique.
    queries::delete_file(pool, &paths[1])
        .await
        .expect("deleting file B must succeed");
    queries::delete_file(pool, &paths[2])
        .await
        .expect("deleting file C must NOT violate idx_cge_unique");

    // Both edges cascaded away with their target files; none orphaned as NULL.
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM code_graph_edges WHERE source_file_id = $1")
            .bind(file_a)
            .fetch_one(pool)
            .await
            .expect("count edges");
    assert_eq!(
        remaining, 0,
        "both A→B and A→C edges must cascade-delete with their target files"
    );
}
