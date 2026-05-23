//! `tool_ownership_coupling_mismatch` — High co-change × disjoint authors
//! (SOTA Phase 4.3). Files that co-change frequently but have disjoint
//! ownership are merge-conflict-prone refactor candidates.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::OwnershipCouplingMismatchParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_ownership_coupling_mismatch(
    ctx: &SystemContext,
    params: OwnershipCouplingMismatchParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "ownership_coupling_mismatch", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let min_coupling = params.min_coupling.unwrap_or(0.3);
    let min_commits = params.min_commits.unwrap_or(3) as i64;
    let limit = params.limit.unwrap_or(30);

    // Co-change Jaccard via git_commit_files.
    let pair_rows: Vec<(String, String, i64)> = sqlx::query_as::<_, (String, String, i64)>(
        "WITH fp AS (
            SELECT gcf.file_path AS path, gcf.commit_id
            FROM git_commit_files gcf
            JOIN git_commits gc ON gcf.commit_id = gc.id
            WHERE gc.project_id = $1
        ),
        co AS (
            SELECT a.path AS a, b.path AS b, COUNT(*)::int8 AS n_ab
            FROM fp a JOIN fp b ON a.commit_id = b.commit_id AND a.path < b.path
            GROUP BY a.path, b.path
            HAVING COUNT(*) >= $2
        )
        SELECT a, b, n_ab FROM co",
    )
    .bind(project_id)
    .bind(min_commits)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Co-change query failed: {}", e), None))?;

    // Marginal counts per file.
    let marg_rows: Vec<(String, i64)> = sqlx::query_as::<_, (String, i64)>(
        "SELECT gcf.file_path, COUNT(DISTINCT gcf.commit_id)::int8
         FROM git_commit_files gcf
         JOIN git_commits gc ON gcf.commit_id = gc.id
         WHERE gc.project_id = $1
         GROUP BY gcf.file_path",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Marginal query failed: {}", e), None))?;
    let marg: HashMap<String, i64> = marg_rows.into_iter().collect();

    // Per-file author sets (top authors by chunk count).
    let auth_rows: Vec<(String, String)> = sqlx::query_as::<_, (String, String)>(
        "SELECT DISTINCT f.relative_path, COALESCE(fc.blame_author, '<unknown>') AS author
         FROM indexed_files f
         JOIN file_chunks fc ON fc.file_id = f.id
         WHERE f.project_id = $1 AND fc.blame_author IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Author query failed: {}", e), None))?;
    let mut authors: HashMap<String, HashSet<String>> = HashMap::new();
    for (path, author) in auth_rows {
        authors.entry(path).or_default().insert(author);
    }

    let mut out: Vec<(String, String, f64, f64, usize)> = Vec::new();
    for (a, b, n_ab) in pair_rows {
        let n_a = marg.get(&a).copied().unwrap_or(0);
        let n_b = marg.get(&b).copied().unwrap_or(0);
        let union = n_a + n_b - n_ab;
        if union <= 0 {
            continue;
        }
        let jaccard = n_ab as f64 / union as f64;
        if jaccard < min_coupling {
            continue;
        }
        let auth_a = authors.get(&a).cloned().unwrap_or_default();
        let auth_b = authors.get(&b).cloned().unwrap_or_default();
        let intersect = auth_a.intersection(&auth_b).count();
        let mismatch_authors = auth_a.symmetric_difference(&auth_b).count();
        // Mismatch score: 0 means same authors, 1 means fully disjoint.
        let denom = (auth_a.len() + auth_b.len()).max(1) as f64;
        let mismatch = (mismatch_authors as f64) / denom;
        if intersect == 0 || mismatch > 0.5 {
            out.push((a, b, jaccard, mismatch, intersect));
        }
    }
    out.sort_by(|x, y| {
        // Highest mismatch × jaccard first
        let xs = x.2 * x.3;
        let ys = y.2 * y.3;
        ys.partial_cmp(&xs).unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(limit.max(0) as usize);
    let rows_json: Vec<_> = out
        .iter()
        .map(|(a, b, j, m, i)| {
            json!({
                "file_a": a,
                "file_b": b,
                "jaccard": j,
                "author_mismatch": m,
                "shared_authors": i,
            })
        })
        .collect();
    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when the project lookup or
    // shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let project_id_opt: Option<i32> =
            sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
                .bind(&params.project)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);
        match project_id_opt {
            Some(pid) => crate::mcp::tools::sema_helpers::effects::effect_counts(pool, pid)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                .collect(),
            None => Vec::new(),
        }
    })
    .await;

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "project": params.project,
        "min_coupling": min_coupling,
        "min_commits": min_commits,
        "pairs": rows_json,
        "guidance": "High Jaccard (files co-change) + low shared_authors = files that need ownership reconciliation. Merge conflicts likely."
    }))
}
