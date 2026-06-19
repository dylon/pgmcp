//! Hierarchical metric rollup engine (ADR-027 Stage 3).
//!
//! `persist_project_rollup` persists the per-project `ModuleMetrics` that
//! `graph_analysis` already computes (so the rollup is a cheap add at the point
//! the data exists, not a second graph build) and rolls modules up to one
//! `project_metrics` row. `persist_group_workspace_rollup` then aggregates
//! `project_metrics` over groups and the whole workspace. Strict extensive sums
//! (file counts) roll up by addition; intensive means (instability, distance)
//! are averaged — the `RollupLaw` distinction the category layer (item 4) checks.

use sqlx::PgPool;

use crate::graph::metrics::ModuleMetrics;

/// Architecture-quality composite from the mean distance-from-main-sequence:
/// closeness to Martin's main sequence, clamped to [0,1] (1 = on the sequence).
fn quality_from_distance(avg_distance: f64) -> f64 {
    (1.0 - avg_distance).clamp(0.0, 1.0)
}

/// Persist `modules` for `project_id` and roll them up into one `project_metrics`
/// row. Replaces the project's prior module rows (delete-then-insert in one tx).
pub async fn persist_project_rollup(
    pool: &PgPool,
    project_id: i32,
    modules: &[ModuleMetrics],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM module_metrics WHERE project_id = $1")
        .bind(project_id)
        .execute(&mut *tx)
        .await?;
    for m in modules {
        sqlx::query(
            "INSERT INTO module_metrics
                (project_id, module_path, file_count, afferent_coupling, efferent_coupling,
                 instability, abstractness, distance_from_main_sequence, cohesion)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (project_id, module_path) DO UPDATE SET
                file_count = EXCLUDED.file_count,
                afferent_coupling = EXCLUDED.afferent_coupling,
                efferent_coupling = EXCLUDED.efferent_coupling,
                instability = EXCLUDED.instability,
                abstractness = EXCLUDED.abstractness,
                distance_from_main_sequence = EXCLUDED.distance_from_main_sequence,
                cohesion = EXCLUDED.cohesion,
                updated_at = NOW()",
        )
        .bind(project_id)
        .bind(&m.module_path)
        .bind(m.file_count as i32)
        .bind(m.afferent_coupling as i32)
        .bind(m.efferent_coupling as i32)
        .bind(m.instability)
        .bind(m.abstractness)
        .bind(m.distance_from_main_sequence)
        .bind(m.cohesion)
        .execute(&mut *tx)
        .await?;
    }

    let n = modules.len().max(1) as f64;
    let file_count: i32 = modules.iter().map(|m| m.file_count as i32).sum();
    let module_count = modules.len() as i32;
    let avg_i = modules.iter().map(|m| m.instability).sum::<f64>() / n;
    let avg_a = modules.iter().map(|m| m.abstractness).sum::<f64>() / n;
    let avg_d = modules
        .iter()
        .map(|m| m.distance_from_main_sequence)
        .sum::<f64>()
        / n;
    let aqs = quality_from_distance(avg_d);
    sqlx::query(
        "INSERT INTO project_metrics
            (project_id, file_count, module_count, avg_instability, avg_abstractness,
             avg_distance, architecture_quality_score)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (project_id) DO UPDATE SET
            file_count = EXCLUDED.file_count,
            module_count = EXCLUDED.module_count,
            avg_instability = EXCLUDED.avg_instability,
            avg_abstractness = EXCLUDED.avg_abstractness,
            avg_distance = EXCLUDED.avg_distance,
            architecture_quality_score = EXCLUDED.architecture_quality_score,
            updated_at = NOW()",
    )
    .bind(project_id)
    .bind(file_count)
    .bind(module_count)
    .bind(avg_i)
    .bind(avg_a)
    .bind(avg_d)
    .bind(aqs)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Roll `project_metrics` up to every group and to the workspace (level rows in
/// `hier_group_metrics`). Rebuilds the table each pass (cheap; one row per group
/// + one workspace row). Run once after all per-project rollups.
pub async fn persist_group_workspace_rollup(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM hier_group_metrics")
        .execute(&mut *tx)
        .await?;

    // Group level: aggregate member projects' metrics.
    let groups: Vec<(i64, Option<String>)> =
        sqlx::query_as("SELECT id, label FROM project_groups ORDER BY id")
            .fetch_all(&mut *tx)
            .await?;
    for (gid, label) in groups {
        let (project_count, file_count, avg_i, avg_d): (i64, i64, f64, f64) = sqlx::query_as(
            "SELECT COUNT(*)::int8,
                    COALESCE(SUM(pm.file_count), 0)::int8,
                    COALESCE(AVG(pm.avg_instability), 0)::float8,
                    COALESCE(AVG(pm.avg_distance), 0)::float8
               FROM project_group_members m
               JOIN project_metrics pm ON pm.project_id = m.project_id
              WHERE m.group_id = $1 AND m.valid_to IS NULL",
        )
        .bind(gid)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO hier_group_metrics
                (level, ref_id, label, project_count, file_count, avg_instability,
                 avg_distance, architecture_quality_score)
             VALUES ('group', $1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(gid)
        .bind(label)
        .bind(project_count as i32)
        .bind(file_count)
        .bind(avg_i)
        .bind(avg_d)
        .bind(quality_from_distance(avg_d))
        .execute(&mut *tx)
        .await?;
    }

    // Workspace level: aggregate all projects (ref_id = 0).
    let (project_count, file_count, avg_i, avg_d): (i64, i64, f64, f64) = sqlx::query_as(
        "SELECT COUNT(*)::int8,
                COALESCE(SUM(file_count), 0)::int8,
                COALESCE(AVG(avg_instability), 0)::float8,
                COALESCE(AVG(avg_distance), 0)::float8
           FROM project_metrics",
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO hier_group_metrics
            (level, ref_id, label, project_count, file_count, avg_instability,
             avg_distance, architecture_quality_score)
         VALUES ('workspace', 0, 'workspace', $1, $2, $3, $4, $5)",
    )
    .bind(project_count as i32)
    .bind(file_count)
    .bind(avg_i)
    .bind(avg_d)
    .bind(quality_from_distance(avg_d))
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_is_one_on_the_main_sequence() {
        assert_eq!(quality_from_distance(0.0), 1.0);
        assert_eq!(quality_from_distance(1.0), 0.0);
        assert_eq!(quality_from_distance(0.25), 0.75);
        // Clamped.
        assert_eq!(quality_from_distance(2.0), 0.0);
    }
}
