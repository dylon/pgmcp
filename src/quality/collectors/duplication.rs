//! Duplication collectors over intra-project chunk-similarity pairs.
//!
//! `lsh_clone_detection`, `boilerplate_clusters`, and `internal_dry` are not
//! collected here: each needs its own materialized algorithm output (MinHash
//! LSH buckets, AST boilerplate clusters, intra-file DRY analysis) that isn't
//! stored in a queryable table — so rather than fake the signal, the aggregator
//! omits them and `find_duplicates` + `clone_density` carry the duplication
//! pillar from the similarity table.

use std::collections::HashMap;

use rmcp::ErrorData as McpError;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::tools::sota_helpers::pool_or_err;
use crate::quality::findings::{Finding, FindingCategory, Severity};

const DUP: FindingCategory = FindingCategory::Duplication;

#[derive(sqlx::FromRow)]
struct Pair {
    path_a: String,
    path_b: String,
    chunk_similarity: f64,
}

async fn intra_project_pairs(ctx: &SystemContext, project_id: i32) -> Result<Vec<Pair>, McpError> {
    let pool = pool_or_err(ctx)?;
    sqlx::query_as::<_, Pair>(
        "SELECT path_a, path_b, chunk_similarity
         FROM cross_project_similarities
         WHERE project_id_a = $1 AND project_id_b = $1 AND path_a <> path_b
           AND chunk_similarity >= 0.85
         ORDER BY chunk_similarity DESC
         LIMIT 1000",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("duplication query failed: {e}"), None))
}

/// Near-duplicate file pairs within the project.
pub async fn collect_find_duplicates(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pairs = intra_project_pairs(ctx, project_id).await?;
    Ok(pairs
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            Finding::new(
                "find_duplicates",
                DUP,
                project_name,
                Severity::Low,
                format!(
                    "{} ≈ {} ({:.0}% similar) — duplicate candidate",
                    p.path_a,
                    p.path_b,
                    p.chunk_similarity * 100.0
                ),
            )
            .with_score(p.chunk_similarity)
            .at_file(&p.path_a)
            .with_kind(format!("dup_pair:{i}"))
            .with_raw(
                json!({ "path_a": p.path_a, "path_b": p.path_b, "similarity": p.chunk_similarity }),
            )
        })
        .collect())
}

/// Files participating in many near-duplicate pairs — clone hotspots.
pub async fn collect_clone_density(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pairs = intra_project_pairs(ctx, project_id).await?;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for p in &pairs {
        *counts.entry(p.path_a.clone()).or_insert(0) += 1;
        *counts.entry(p.path_b.clone()).or_insert(0) += 1;
    }
    let mut out: Vec<Finding> = counts
        .into_iter()
        .filter(|(_, n)| *n >= 3)
        .map(|(path, n)| {
            Finding::new(
                "clone_density",
                DUP,
                project_name,
                Severity::Low,
                format!("{path} participates in {n} near-duplicate pairs — clone hotspot"),
            )
            .with_score(n as f64)
            .at_file(&path)
            .with_kind("clone_hotspot")
            .with_raw(json!({ "path": path, "clone_pairs": n }))
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .unwrap_or(0.0)
            .partial_cmp(&a.score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}
