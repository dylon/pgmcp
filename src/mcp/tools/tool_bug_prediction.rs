//! `tool_bug_prediction` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_bug_prediction(
    ctx: &SystemContext,
    params: BugPredictionParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().bug_predictions.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(20);

    debug!(
        tool = "bug_prediction",
        project = %params.project,
        limit,
        "MCP tool invoked",
    );

    #[derive(sqlx::FromRow)]
    struct BugRow {
        relative_path: String,
        language: String,
        line_count: i32,
        churn_rate: Option<f64>,
        fix_commit_ratio: Option<f64>,
        commit_count: Option<i32>,
        author_count: Option<i32>,
        in_degree: Option<i32>,
        out_degree: Option<i32>,
    }

    let rows: Vec<BugRow> = sqlx::query_as::<_, BugRow>(
        "SELECT f.relative_path, f.language, f.line_count,
                fm.churn_rate, fm.fix_commit_ratio, fm.commit_count,
                fm.author_count, fm.in_degree, fm.out_degree
         FROM indexed_files f
         JOIN file_metrics fm ON fm.file_id = f.id
         JOIN projects p ON f.project_id = p.id
         WHERE p.name = $1",
    )
    .bind(&params.project)
    .fetch_all(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No file metrics found. The graph-analysis cron job may not have run yet.",
        )]));
    }

    // Trained model (Phase 3.5): fit logistic regression on this project's own
    // history — features = process/structural metrics, label = "touched by a
    // bug-fix commit" (fix_commit_ratio > 0). fix_commit_ratio is NOT a feature
    // (no label leakage). Falls back to the hand-weighted heuristic on cold
    // start (one class only / too little history). The scoring is the shared
    // `code_analysis::findings::score_bug_files` primitive, so this tool and the
    // `findings-promotion` cron compute identical scores. The per-file metric
    // fields (commit/author/coupling) are looked up alongside for the output.
    use crate::code_analysis::findings::BugFeatures;
    let features: Vec<BugFeatures> = rows
        .iter()
        .map(|r| BugFeatures {
            relative_path: r.relative_path.clone(),
            language: r.language.clone(),
            line_count: r.line_count,
            churn_rate: r.churn_rate,
            fix_commit_ratio: r.fix_commit_ratio,
            commit_count: r.commit_count,
            author_count: r.author_count,
            in_degree: r.in_degree,
            out_degree: r.out_degree,
        })
        .collect();
    let (ranked, score_kind_enum) = crate::code_analysis::findings::score_bug_files(&features);
    let score_kind = score_kind_enum.as_str();

    // Re-attach the raw metric fields (by path) for the rendered output.
    let by_path: std::collections::HashMap<&str, &BugRow> =
        rows.iter().map(|r| (r.relative_path.as_str(), r)).collect();
    let mut scored: Vec<serde_json::Value> = ranked
        .iter()
        .map(|s| {
            let r = by_path.get(s.relative_path.as_str());
            serde_json::json!({
                "path": s.relative_path,
                "language": s.language,
                "bug_score": format!("{:.4}", s.bug_score),
                "score_kind": score_kind,
                "churn_rate": format!("{:.2}", r.and_then(|r| r.churn_rate).unwrap_or(0.0)),
                "fix_ratio": format!("{:.2}", s.fix_ratio),
                "line_count": s.line_count,
                "commit_count": r.and_then(|r| r.commit_count).unwrap_or(0),
                "author_count": r.and_then(|r| r.author_count).unwrap_or(0),
                "coupling": r.map(|r| r.in_degree.unwrap_or(0) + r.out_degree.unwrap_or(0)).unwrap_or(0),
            })
        })
        .collect();
    scored.truncate(limit as usize);

    // Shadow-ASR channel: bug-prone-effect symbols (unsafe / may_panic /
    // blocking_io). Composite bug-prediction can weigh these as features.
    let bug_prone_effect_symbols = if let Some(pool) = ctx.db().pool() {
        let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);
        match project_id {
            Some(pid) => crate::mcp::tools::sema_helpers::effects::symbols_with_any_effect(
                pool,
                pid,
                &[
                    crate::parsing::type_tags::vocabulary::EFFECT_UNSAFE.to_string(),
                    crate::parsing::type_tags::vocabulary::EFFECT_MAY_PANIC.to_string(),
                    crate::parsing::type_tags::vocabulary::EFFECT_BLOCKING_IO.to_string(),
                ],
            )
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(symbol_id, file_id, name, scope_path)| {
                serde_json::json!({
                    "symbol_id": symbol_id, "file_id": file_id, "name": name, "scope_path": scope_path,
                })
            })
            .collect::<Vec<_>>(),
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let model_info = match score_kind_enum {
        crate::code_analysis::findings::ScoreKind::TrainedLogreg => serde_json::json!({
            "kind": "logistic_regression",
            "n_samples": features.len(),
        }),
        crate::code_analysis::findings::ScoreKind::Heuristic => {
            serde_json::json!({ "kind": "heuristic_fallback" })
        }
    };

    let result = serde_json::json!({
        "project": params.project,
        "file_count": scored.len(),
        "score_kind": score_kind,
        "model": model_info,
        "files": scored,
        "bug_prone_effect_symbols": bug_prone_effect_symbols,
        "guidance": "When `score_kind=trained_logreg`, `bug_score` is a logistic-regression \
                     defect-proneness PROBABILITY (0-1) learned from this project's own history \
                     (features = churn/commits/authors/in+out-degree/LOC; label = touched by a bug-fix \
                     commit; fix_ratio excluded from features to avoid leakage). On cold start (one class \
                     only / sparse git history) it falls back to `score_kind=heuristic` (the prior \
                     hand-weighted sum). Prioritize review/testing for high-score files; high fix_ratio \
                     (>0.3) means >30% of commits are bug fixes. The `bug_prone_effect_symbols` channel \
                     surfaces unsafe / may_panic / blocking_io symbols — orthogonal review priorities.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "bug_prediction",
        files = scored.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
