//! Read/write for the `project_dependencies` graph: forward
//! (`dependencies_of`) and reverse (`dependents_of`) live-edge queries, plus the
//! bitemporal `upsert_dependency` used by the manifest/import/manual/asserted
//! sources (upsert-and-bump the open edge; the cron closes vanished ones).

use serde::Serialize;
use sqlx::PgPool;

use crate::deps::DepSource;

/// One live project→project dependency edge, with both project names resolved.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ProjectDepRow {
    pub dependent_project_id: i32,
    pub dependent_name: String,
    pub dependency_project_id: i32,
    pub dependency_name: String,
    pub dep_name: Option<String>,
    pub kind: Option<String>,
    pub source: String,
    pub confidence: f64,
}

/// Forward: the live dependencies of `project_id` (what it depends on).
pub async fn dependencies_of(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<ProjectDepRow>, sqlx::Error> {
    sqlx::query_as::<_, ProjectDepRow>(
        "SELECT pd.dependent_project_id, dp.name AS dependent_name,
                pd.dependency_project_id, up.name AS dependency_name,
                pd.dep_name, pd.kind, pd.source, pd.confidence
           FROM project_dependencies pd
           JOIN projects dp ON dp.id = pd.dependent_project_id
           JOIN projects up ON up.id = pd.dependency_project_id
          WHERE pd.dependent_project_id = $1 AND pd.valid_to IS NULL
          ORDER BY pd.confidence DESC, up.name",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// Reverse: the live dependents of `project_id` (who depends on it).
pub async fn dependents_of(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<ProjectDepRow>, sqlx::Error> {
    sqlx::query_as::<_, ProjectDepRow>(
        "SELECT pd.dependent_project_id, dp.name AS dependent_name,
                pd.dependency_project_id, up.name AS dependency_name,
                pd.dep_name, pd.kind, pd.source, pd.confidence
           FROM project_dependencies pd
           JOIN projects dp ON dp.id = pd.dependent_project_id
           JOIN projects up ON up.id = pd.dependency_project_id
          WHERE pd.dependency_project_id = $1 AND pd.valid_to IS NULL
          ORDER BY pd.confidence DESC, dp.name",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// Render a project's live `project_depends_on` neighborhood as two JSON arrays
/// — `(dependencies, dependents)` — for the cross-project section of the
/// temporal graph-RAG tools (ADR-009 §4.2). `dependencies` are the projects this
/// one depends on (it may break when *they* change); `dependents` are the
/// projects that depend on this one (they may break when *it* changes). Each is
/// `[]` when there are no live edges, so a tool can include the section
/// unconditionally. Shared by `dependency_graph`, `centrality_analysis`,
/// `effect_propagation`, and `code_ppr_search` so the cross-project surface is
/// uniform (mirrors the inline block in `change_impact_analysis`).
pub async fn cross_project_blocks(
    pool: &PgPool,
    project_id: i32,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let dependencies: Vec<serde_json::Value> = dependencies_of(pool, project_id)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            serde_json::json!({
                "project": r.dependency_name,
                "project_id": r.dependency_project_id,
                "dep_name": r.dep_name,
                "kind": r.kind,
                "source": format!("cross_project_{}", r.source),
                "confidence": r.confidence,
            })
        })
        .collect();
    let dependents: Vec<serde_json::Value> = dependents_of(pool, project_id)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            serde_json::json!({
                "project": r.dependent_name,
                "project_id": r.dependent_project_id,
                "dep_name": r.dep_name,
                "kind": r.kind,
                "source": format!("cross_project_{}", r.source),
                "confidence": r.confidence,
            })
        })
        .collect();
    (dependencies, dependents)
}

/// Upsert a live dependency edge for one `source`, bumping `last_seen_at` (and
/// `confidence`/`kind`/`dep_name`) on the open interval. Returns the edge id.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_dependency(
    pool: &PgPool,
    dependent_project_id: i32,
    dependency_project_id: i32,
    dep_name: Option<&str>,
    kind: Option<&str>,
    source: DepSource,
    confidence: f64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO project_dependencies
            (dependent_project_id, dependency_project_id, dep_name, kind, source,
             confidence, valid_from, last_seen_at)
         VALUES ($1, $2, $3, $4, $5, $6, now(), now())
         ON CONFLICT (dependent_project_id, dependency_project_id, source)
            WHERE valid_to IS NULL
         DO UPDATE SET
            dep_name     = EXCLUDED.dep_name,
            kind         = EXCLUDED.kind,
            confidence   = EXCLUDED.confidence,
            last_seen_at = now()
         RETURNING id",
    )
    .bind(dependent_project_id)
    .bind(dependency_project_id)
    .bind(dep_name)
    .bind(kind)
    .bind(source.as_str())
    .bind(confidence)
    .fetch_one(pool)
    .await
}

/// Close (set `valid_to = now()`) live edges of `source` for `dependent_project_id`
/// whose `last_seen_at` predates `cutoff` — i.e. dependencies that vanished from
/// the manifest since the cutoff. Returns the number closed. Keeps history.
pub async fn close_stale(
    pool: &PgPool,
    dependent_project_id: i32,
    source: DepSource,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE project_dependencies
            SET valid_to = now()
          WHERE dependent_project_id = $1 AND source = $2
            AND valid_to IS NULL AND last_seen_at < $3",
    )
    .bind(dependent_project_id)
    .bind(source.as_str())
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}
