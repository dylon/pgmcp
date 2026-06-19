//! Migration step 48: hierarchical metric rollup tables (ADR-027 Stage 3).
//!
//! `module_metrics` persists the currently-transient `graph::metrics::ModuleMetrics`
//! (Martin Ca/Ce/I/A/D* + cohesion + cyclomatic sum); `project_metrics` holds the
//! project level (rolled-up intra metrics + inter-project columns +
//! `architecture_quality_score`) that has no table today; `hier_group_metrics`
//! covers the group and workspace levels (discriminated by `level`). Every row
//! carries a `level` (`HierLevel` CHECK) so the rollup engine and the category
//! `categorical_lint` can treat them uniformly. Additive + idempotent.

use sqlx::PgPool;

use crate::hierarchy::HierLevel;

pub(super) const HIER_METRICS: i32 = 48;
pub(super) const HIER_METRICS_NAME: &str = "hier_metrics";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS module_metrics (
            id                          BIGSERIAL PRIMARY KEY,
            project_id                  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            level                       TEXT NOT NULL DEFAULT 'module',
            module_path                 TEXT NOT NULL,
            file_count                  INTEGER NOT NULL DEFAULT 0,
            afferent_coupling           INTEGER NOT NULL DEFAULT 0,
            efferent_coupling           INTEGER NOT NULL DEFAULT 0,
            instability                 DOUBLE PRECISION NOT NULL DEFAULT 0,
            abstractness                DOUBLE PRECISION NOT NULL DEFAULT 0,
            distance_from_main_sequence DOUBLE PRECISION NOT NULL DEFAULT 0,
            cohesion                    DOUBLE PRECISION,
            cyclomatic_sum              BIGINT NOT NULL DEFAULT 0,
            updated_at                  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE (project_id, module_path)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS project_metrics (
            id                          BIGSERIAL PRIMARY KEY,
            project_id                  INTEGER NOT NULL UNIQUE REFERENCES projects(id) ON DELETE CASCADE,
            level                       TEXT NOT NULL DEFAULT 'project',
            file_count                  INTEGER NOT NULL DEFAULT 0,
            module_count                INTEGER NOT NULL DEFAULT 0,
            cyclomatic_sum              BIGINT NOT NULL DEFAULT 0,
            avg_instability             DOUBLE PRECISION NOT NULL DEFAULT 0,
            avg_abstractness            DOUBLE PRECISION NOT NULL DEFAULT 0,
            avg_distance                DOUBLE PRECISION NOT NULL DEFAULT 0,
            -- inter-project (E4): coupling computed over the project graph.
            inter_afferent              INTEGER NOT NULL DEFAULT 0,
            inter_efferent              INTEGER NOT NULL DEFAULT 0,
            inter_instability           DOUBLE PRECISION,
            architecture_quality_score  DOUBLE PRECISION,
            updated_at                  TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS hier_group_metrics (
            id                          BIGSERIAL PRIMARY KEY,
            level                       TEXT NOT NULL,
            ref_id                      BIGINT,
            label                       TEXT,
            project_count               INTEGER NOT NULL DEFAULT 0,
            file_count                  INTEGER NOT NULL DEFAULT 0,
            cyclomatic_sum              BIGINT NOT NULL DEFAULT 0,
            avg_instability             DOUBLE PRECISION NOT NULL DEFAULT 0,
            avg_distance                DOUBLE PRECISION NOT NULL DEFAULT 0,
            architecture_quality_score  DOUBLE PRECISION,
            updated_at                  TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    // One row per (level, ref_id); workspace uses ref_id = 0 (single row).
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS ux_hier_group_metrics
         ON hier_group_metrics (level, COALESCE(ref_id, 0))",
    )
    .execute(pool)
    .await?;

    for (table, constraint) in [
        ("module_metrics", "module_metrics_level_check"),
        ("project_metrics", "project_metrics_level_check"),
        ("hier_group_metrics", "hier_group_metrics_level_check"),
    ] {
        super::v4_work_items::install_check(
            pool,
            table,
            constraint,
            &format!("level IN ({})", HierLevel::sql_in_list()),
        )
        .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(HIER_METRICS, 48);
        assert_eq!(HIER_METRICS_NAME, "hier_metrics");
    }
}
