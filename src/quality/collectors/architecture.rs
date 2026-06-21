//! Architecture collectors: cycles, god modules, design smells, coupling,
//! feature envy, shotgun surgery, cohesion, misplaced code.

use rmcp::ErrorData as McpError;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::tools::fix_actions::PathRange;
use crate::mcp::tools::sota_helpers::pool_or_err;
use crate::quality::findings::{Finding, FindingCategory, Severity};

const AR: FindingCategory = FindingCategory::Architecture;

/// Bidirectional import pairs (2-cycles) — the cheapest reliable cycle signal.
pub async fn collect_circular_dependencies(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        a: String,
        b: String,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT DISTINCT fa.relative_path AS a, fb.relative_path AS b
         FROM code_graph_edges e1
         JOIN code_graph_edges e2 ON e1.target_file_id = e2.source_file_id
              AND e2.target_file_id = e1.source_file_id
         JOIN indexed_files fa ON fa.id = e1.source_file_id
         JOIN indexed_files fb ON fb.id = e1.target_file_id
         WHERE e1.project_id = $1 AND e1.edge_type = 'import' AND e2.edge_type = 'import'
           AND e1.target_project_id IS NULL AND e2.target_project_id IS NULL
           AND e1.source_file_id < e1.target_file_id",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| {
        McpError::internal_error(format!("circular_dependencies query failed: {e}"), None)
    })?;

    Ok(rows
        .into_iter()
        .map(|r| {
            Finding::new(
                "circular_dependencies",
                AR,
                project_name,
                Severity::High,
                format!("Import cycle: {} ↔ {}", r.a, r.b),
            )
            .at_file(&r.a)
            .with_additional(vec![PathRange {
                path: r.b.clone(),
                start_line: 0,
                end_line: 0,
            }])
            .with_kind("import_cycle")
            .with_raw(json!({ "files": [r.a, r.b] }))
        })
        .collect())
}

/// God modules — files with very high combined in/out degree.
pub async fn collect_architecture_violations(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        in_degree: i32,
        out_degree: i32,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, COALESCE(fm.in_degree,0) AS in_degree,
                COALESCE(fm.out_degree,0) AS out_degree
         FROM indexed_files f JOIN file_metrics fm ON fm.file_id = f.id
         WHERE f.project_id = $1 AND (COALESCE(fm.in_degree,0) + COALESCE(fm.out_degree,0)) >= 20
         ORDER BY (COALESCE(fm.in_degree,0) + COALESCE(fm.out_degree,0)) DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| {
        McpError::internal_error(format!("architecture_violations query failed: {e}"), None)
    })?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let degree = r.in_degree + r.out_degree;
            let severity = if degree >= 40 { Severity::High } else { Severity::Medium };
            Finding::new(
                "architecture_violations",
                AR,
                project_name,
                severity,
                format!(
                    "{} is a hub (in {}, out {}) — god module",
                    r.relative_path, r.in_degree, r.out_degree
                ),
            )
            .with_score(degree as f64)
            .at_file(&r.relative_path)
            .with_kind("god_module")
            .with_raw(json!({ "path": r.relative_path, "in_degree": r.in_degree, "out_degree": r.out_degree }))
        })
        .collect())
}

/// God files — oversized files, escalated when also highly coupled.
pub async fn collect_design_smell_detection(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        line_count: i32,
        coupling: i64,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, f.line_count,
                (COALESCE(fm.afferent_coupling,0)+COALESCE(fm.efferent_coupling,0))::BIGINT AS coupling
         FROM indexed_files f LEFT JOIN file_metrics fm ON fm.file_id = f.id
         WHERE f.project_id = $1 AND f.line_count > 500
         ORDER BY f.line_count DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("design_smell query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let severity = if r.line_count > 1000 || r.coupling >= 15 {
                Severity::Medium
            } else {
                Severity::Low
            };
            Finding::new(
                "design_smell_detection",
                AR,
                project_name,
                severity,
                format!(
                    "{} — god class ({} lines, coupling {})",
                    r.relative_path, r.line_count, r.coupling
                ),
            )
            .with_score(r.line_count as f64)
            .at_file(&r.relative_path)
            .with_kind("god_class")
            .with_raw(json!({ "path": r.relative_path, "line_count": r.line_count, "coupling": r.coupling }))
        })
        .collect())
}

/// Module-level instability extremes (zone of pain / uselessness). Module-keyed
/// by top directory segment.
pub async fn collect_coupling_cohesion_report(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        module: String,
        instability: f64,
        n: i64,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT COALESCE((regexp_split_to_array(f.relative_path,'/'))[1],'.') AS module,
                AVG(fm.instability)::DOUBLE PRECISION AS instability,
                COUNT(*)::BIGINT AS n
         FROM indexed_files f JOIN file_metrics fm ON fm.file_id = f.id
         WHERE f.project_id = $1 AND fm.instability IS NOT NULL
         GROUP BY module HAVING COUNT(*) >= 3",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("coupling_cohesion query failed: {e}"), None))?;

    let mut out = Vec::new();
    for r in rows {
        // Flag the extremes: very stable (<0.2) or very unstable (>0.8) modules.
        let (zone, severity) = if r.instability < 0.2 {
            ("zone_of_pain", Severity::Low)
        } else if r.instability > 0.8 {
            ("highly_unstable", Severity::Low)
        } else {
            continue;
        };
        out.push(
            Finding::new(
                "coupling_cohesion_report",
                AR,
                project_name,
                severity,
                format!(
                    "module `{}` — avg instability {:.2} ({}, {} files)",
                    r.module, r.instability, zone, r.n
                ),
            )
            .with_score(r.instability)
            .with_kind(format!("module:{}", r.module))
            .with_raw(json!({ "module": r.module, "instability": r.instability, "files": r.n })),
        );
    }
    Ok(out)
}

/// Feature-envy proxy — files depending on many others while depended-on by few.
pub async fn collect_feature_envy(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        in_degree: i32,
        out_degree: i32,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, COALESCE(fm.in_degree,0) AS in_degree,
                COALESCE(fm.out_degree,0) AS out_degree
         FROM indexed_files f JOIN file_metrics fm ON fm.file_id = f.id
         WHERE f.project_id = $1 AND COALESCE(fm.out_degree,0) >= 5
           AND COALESCE(fm.out_degree,0) > 3 * GREATEST(COALESCE(fm.in_degree,0),1)",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("feature_envy query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            Finding::new(
                "feature_envy",
                AR,
                project_name,
                Severity::Low,
                format!(
                    "{} depends on {} files but is used by {} — feature envy",
                    r.relative_path, r.out_degree, r.in_degree
                ),
            )
            .at_file(&r.relative_path)
            .with_kind("feature_envy")
            .with_raw(json!({ "path": r.relative_path, "in_degree": r.in_degree, "out_degree": r.out_degree }))
        })
        .collect())
}

/// Shotgun surgery — files touched by an unusually large number of commits.
pub async fn collect_shotgun_surgery(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        file_path: String,
        commits: i64,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT gcf.file_path, COUNT(DISTINCT gcf.commit_id)::BIGINT AS commits
         FROM git_commit_files gcf
         JOIN git_commits gc ON gc.id = gcf.commit_id
         WHERE gc.project_id = $1
         GROUP BY gcf.file_path HAVING COUNT(DISTINCT gcf.commit_id) >= 20
         ORDER BY commits DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("shotgun_surgery query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            Finding::new(
                "shotgun_surgery",
                AR,
                project_name,
                Severity::Low,
                format!(
                    "{} changed in {} commits — change magnet",
                    r.file_path, r.commits
                ),
            )
            .with_score(r.commits as f64)
            .at_file(&r.file_path)
            .with_kind("change_magnet")
            .with_raw(json!({ "path": r.file_path, "commits": r.commits }))
        })
        .collect())
}

/// Low-cohesion proxy — files declaring an unusually large number of top-level
/// symbols (LCOM4 stand-in without full member-reference analysis).
pub async fn collect_lcom4(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        sym_count: i64,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, COUNT(fs.id)::BIGINT AS sym_count
         FROM indexed_files f JOIN file_symbols fs ON fs.file_id = f.id
         WHERE f.project_id = $1 AND fs.parent_id IS NULL
           AND fs.kind IN ('function','method','class','struct','impl','enum','trait')
         GROUP BY f.relative_path HAVING COUNT(fs.id) > 20
         ORDER BY sym_count DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("lcom4 query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            Finding::new(
                "lcom4",
                AR,
                project_name,
                Severity::Low,
                format!(
                    "{} declares {} top-level symbols — likely low cohesion",
                    r.relative_path, r.sym_count
                ),
            )
            .with_score(r.sym_count as f64)
            .at_file(&r.relative_path)
            .with_kind("low_cohesion")
            .with_raw(json!({ "path": r.relative_path, "symbols": r.sym_count }))
        })
        .collect())
}

/// Misplaced code — files whose dominant topic differs from their directory's
/// majority topic. Needs the topic-clustering cron (empty if not run).
pub async fn collect_find_misplaced_code(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        dir: String,
        file_topic: i32,
        dir_topic: i32,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "WITH file_topic AS (
            SELECT f.id AS file_id, f.relative_path,
                   COALESCE((regexp_split_to_array(f.relative_path,'/'))[1],'.') AS dir,
                   cta.topic_id,
                   ROW_NUMBER() OVER (PARTITION BY f.id ORDER BY COUNT(*) DESC, cta.topic_id) AS rn
            FROM indexed_files f
            JOIN file_chunks fc ON fc.file_id = f.id
            JOIN chunk_topic_assignments cta ON cta.chunk_id = fc.id
            WHERE f.project_id = $1
            GROUP BY f.id, f.relative_path, dir, cta.topic_id
        ),
        dom AS (SELECT file_id, relative_path, dir, topic_id FROM file_topic WHERE rn = 1),
        dir_topic AS (
            SELECT dir, topic_id,
                   ROW_NUMBER() OVER (PARTITION BY dir ORDER BY COUNT(*) DESC, topic_id) AS rn
            FROM dom GROUP BY dir, topic_id
        ),
        dir_major AS (SELECT dir, topic_id FROM dir_topic WHERE rn = 1),
        dir_size AS (SELECT dir, COUNT(*) AS n FROM dom GROUP BY dir)
        SELECT d.relative_path, d.dir, d.topic_id AS file_topic, m.topic_id AS dir_topic
        FROM dom d
        JOIN dir_major m ON m.dir = d.dir
        JOIN dir_size s ON s.dir = d.dir
        WHERE d.topic_id <> m.topic_id AND s.n >= 3",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| {
        McpError::internal_error(format!("find_misplaced_code query failed: {e}"), None)
    })?;

    Ok(rows
        .into_iter()
        .map(|r| {
            Finding::new(
                "find_misplaced_code",
                AR,
                project_name,
                Severity::Low,
                format!(
                    "{} (topic {}) sits in `{}` whose majority topic is {}",
                    r.relative_path, r.file_topic, r.dir, r.dir_topic
                ),
            )
            .at_file(&r.relative_path)
            .with_kind("misplaced")
            .with_raw(json!({
                "path": r.relative_path, "directory": r.dir,
                "file_topic": r.file_topic, "directory_majority_topic": r.dir_topic,
            }))
        })
        .collect())
}
