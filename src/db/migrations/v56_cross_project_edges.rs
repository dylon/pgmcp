//! Migration step 56: cross-project import edges on `code_graph_edges`.
//!
//! ## Why
//!
//! A Rust `use <ident>::…` may target a crate that lives in a *different* indexed
//! project (a sibling repo depended on via cargo `path=`). Resolving it produces
//! a file→file `import` edge whose `target_file_id` belongs to another project.
//! The edge's `project_id` stays the SOURCE file's project (forced by the
//! `DELETE FROM code_graph_edges WHERE project_id = $1` rebuild lifecycle and by
//! every reader's `WHERE e.project_id = $1`), so cross-project edges are owned,
//! deleted, and rebuilt with their source project.
//!
//! `target_project_id` makes such an edge **self-identifying**: it is `NULL` for
//! every intra-project or unresolved edge (no behavior change, no backfill) and
//! the target's project for a cross-project edge. This single predicate lets the
//! ~8 per-file intra-project readers filter cross-project rows with
//! `target_project_id IS NULL`, while the unified-graph KG view and the opt-in
//! `dependency_graph` mode deliberately include them. Martin's per-package
//! metrics (`coupling_cohesion_report` / `architecture_quality`) already exclude
//! foreign targets via their `tf.project_id = e.project_id` join, so they are
//! unaffected.
//!
//! `ON DELETE CASCADE` mirrors `target_file_id`'s FK: deleting the target file
//! (or its project) removes the edge, and the source project's next
//! graph-analysis pass rebuilds it if the target reappears. `target_project_id`
//! is deliberately NOT added to `idx_cge_unique` — `target_file_id` is globally
//! unique to one project, so it functionally determines `target_project_id` and
//! adds nothing to uniqueness. Additive + idempotent.

use sqlx::PgPool;

pub(super) const CROSS_PROJECT_EDGES: i32 = 56;
pub(super) const CROSS_PROJECT_EDGES_NAME: &str = "cross_project_edges";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "ALTER TABLE code_graph_edges
            ADD COLUMN IF NOT EXISTS target_project_id INTEGER
            REFERENCES projects(id) ON DELETE CASCADE",
    )
    .execute(pool)
    .await?;
    // Cross-project edges are a small minority; a partial index keeps the
    // cross-project consumers (KG view, dependency_graph opt-in) cheap without
    // bloating the common intra-project path.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_cge_target_project
            ON code_graph_edges (target_project_id)
            WHERE target_project_id IS NOT NULL",
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(CROSS_PROJECT_EDGES, 56);
        assert_eq!(CROSS_PROJECT_EDGES_NAME, "cross_project_edges");
    }
}
