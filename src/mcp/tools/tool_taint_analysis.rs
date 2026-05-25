//! `tool_taint_analysis` — source→sink taint (graph-roadmap Phase 2.1; Newsome-Song
//! NDSS 2005; CPG reachability framing Yamaguchi et al. S&P 2014).
//!
//! For languages with a def-use backend (currently Rust) this runs a **real
//! intraprocedural data-flow analysis**: a finding requires that a value
//! *derived from* a taint source *reaches* a dangerous sink without passing a
//! sanitizer (`crate::code_analysis::taint_dataflow`). Languages without a
//! def-use backend fall back to the previous regex source/sink *co-occurrence*
//! heuristic — separated and labeled, so real flows aren't confused with
//! review candidates.

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use sqlx::PgPool;
use std::sync::atomic::Ordering;

use crate::code_analysis::taint_dataflow::{TaintFinding, analyze_function};
use crate::code_analysis::taint_interproc::{InterprocFinding, analyze_file};
use crate::context::SystemContext;
use crate::mcp::server::TaintAnalysisParams;
use crate::mcp::tools::sema_helpers::effects::symbols_with_any_effect;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::parsing::LanguageRegistry;
use crate::parsing::type_tags::vocabulary::{
    EFFECT_CRYPTO, EFFECT_DATABASE, EFFECT_FILESYSTEM, EFFECT_NETWORK,
};

/// Languages whose backend implements `extract_dataflow` (real flow). Others
/// use the regex co-occurrence fallback. Append as backends grow.
pub(crate) const DATAFLOW_LANGUAGES: &[&str] = &["rust"];

/// One real source→sink flow, tagged with its file and language. Shared by
/// `taint_analysis` and `injection_candidates`.
pub(crate) struct DataflowHit {
    pub path: String,
    pub language: String,
    pub finding: TaintFinding,
}

/// An interprocedural taint finding tagged with its file (Phase 3.4).
pub(crate) struct InterprocHit {
    pub path: String,
    pub language: String,
    pub finding: InterprocFinding,
}

/// Run the real taint engine over every def-use-capable file in the project.
/// Fetches only `DATAFLOW_LANGUAGES` files (so the regex fallback owns the
/// rest). Each file yields both intraprocedural findings (per function) and
/// interprocedural findings (source-tainted arg → callee param → sink, via
/// within-file summaries, Phase 3.4) from one extraction pass. Pure CPU per
/// file; no transaction.
#[allow(clippy::type_complexity)]
pub(crate) async fn scan_project_dataflow(
    pool: &PgPool,
    project_id: i32,
) -> Result<(Vec<DataflowHit>, Vec<InterprocHit>), sqlx::Error> {
    let rows: Vec<(String, String, Option<String>)> =
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT relative_path, language, content
             FROM indexed_files
             WHERE project_id = $1 AND content IS NOT NULL AND language = ANY($2)",
        )
        .bind(project_id)
        .bind(DATAFLOW_LANGUAGES)
        .fetch_all(pool)
        .await?;

    let mut out = Vec::new();
    let mut interproc = Vec::new();
    for (path, lang, content) in rows {
        let Some(c) = content else { continue };
        let Some(backend) = LanguageRegistry::for_language(&lang) else {
            continue;
        };
        let dfs = backend.extract_dataflow(&c);
        for df in &dfs {
            for finding in analyze_function(df) {
                out.push(DataflowHit {
                    path: path.clone(),
                    language: lang.clone(),
                    finding,
                });
            }
        }
        for finding in analyze_file(&dfs) {
            interproc.push(InterprocHit {
                path: path.clone(),
                language: lang.clone(),
                finding,
            });
        }
    }
    Ok((out, interproc))
}

pub async fn tool_taint_analysis(
    ctx: &SystemContext,
    params: TaintAnalysisParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "taint_analysis", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(30).max(0) as usize;

    // Real source→sink flows (def-use backends): intraprocedural + (Phase 3.4)
    // interprocedural via within-file summaries.
    let (intra_hits, interproc_hits) = scan_project_dataflow(pool, project_id)
        .await
        .map_err(|e| McpError::internal_error(format!("Dataflow scan failed: {}", e), None))?;
    let mut dataflow_findings: Vec<serde_json::Value> = intra_hits
        .into_iter()
        .map(|h| {
            json!({
                "file": h.path,
                "language": h.language,
                "function": h.finding.function,
                "source_kind": h.finding.source_kind,
                "source_line": h.finding.source_line,
                "sink_kind": h.finding.sink_kind,
                "sink_callee": h.finding.sink_callee,
                "sink_line": h.finding.sink_line,
                "flow_hops": h.finding.path.len(),
            })
        })
        .collect();
    dataflow_findings.truncate(limit);

    let mut interprocedural_findings: Vec<serde_json::Value> = interproc_hits
        .into_iter()
        .map(|h| {
            json!({
                "file": h.path,
                "language": h.language,
                "caller": h.finding.caller,
                "source_kind": h.finding.source_kind,
                "source_line": h.finding.source_line,
                "callee": h.finding.callee,
                "param_index": h.finding.param_index,
                "call_line": h.finding.call_line,
                "sink_kind": h.finding.sink_kind,
            })
        })
        .collect();
    interprocedural_findings.truncate(limit);

    // Regex source/sink co-occurrence for languages without a def-use backend.
    let source_re = Regex::new(
        r"(?m)\b(req\.body|req\.params|req\.query|request\.json|request\.form|request\.args|argv|env::var|std::env::var|getenv|input\(\)|stdin)\b",
    )
    .expect("source regex");
    let sink_re = Regex::new(
        r"(?m)\b(Command::new|exec\(|eval\(|spawn_shell|subprocess\.run|os\.system|sql\.query\(|execute\(|Runtime\.exec|shell_exec|sqlx::query_unchecked)\b",
    )
    .expect("sink regex");
    let rows: Vec<(String, String, Option<String>)> =
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT relative_path, language, content
             FROM indexed_files
             WHERE project_id = $1 AND content IS NOT NULL AND NOT (language = ANY($2))",
        )
        .bind(project_id)
        .bind(DATAFLOW_LANGUAGES)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;
    let mut heuristic_findings: Vec<serde_json::Value> = Vec::new();
    for (path, lang, content) in rows {
        if heuristic_findings.len() >= limit {
            break;
        }
        let Some(c) = content else { continue };
        let sources: Vec<u32> = source_re
            .find_iter(&c)
            .map(|m| c[..m.start()].bytes().filter(|b| *b == b'\n').count() as u32 + 1)
            .collect();
        let sinks: Vec<u32> = sink_re
            .find_iter(&c)
            .map(|m| c[..m.start()].bytes().filter(|b| *b == b'\n').count() as u32 + 1)
            .collect();
        if sources.is_empty() || sinks.is_empty() {
            continue;
        }
        heuristic_findings.push(json!({
            "file": path,
            "language": lang,
            "source_lines": sources,
            "sink_lines": sinks,
        }));
    }

    let io_effects = vec![
        EFFECT_NETWORK.to_string(),
        EFFECT_FILESYSTEM.to_string(),
        EFFECT_DATABASE.to_string(),
        EFFECT_CRYPTO.to_string(),
    ];
    let io_symbols = symbols_with_any_effect(pool, project_id, &io_effects)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(symbol_id, file_id, name, scope_path)| {
            json!({
                "symbol_id": symbol_id,
                "file_id": file_id,
                "name": name,
                "scope_path": scope_path,
            })
        })
        .collect::<Vec<_>>();

    json_result(&json!({
        "project": params.project,
        "dataflow_findings": dataflow_findings,
        "interprocedural_findings": interprocedural_findings,
        "heuristic_findings": heuristic_findings,
        "io_effect_symbols": io_symbols,
        "guidance": "`dataflow_findings` are REAL intraprocedural source→sink flows (Rust): a value from a \
            taint source (env/argv/stdin/request) provably reaches a dangerous sink (command/sql/eval/deserialize/\
            path) without passing a sanitizer — high confidence. `interprocedural_findings` (Phase 3.4) extend \
            this ACROSS function boundaries within a file: a source-tainted argument reaches a sink inside a \
            called helper (via the callee's param→sink summary, bounded IFDS) — `caller` passes the tainted arg \
            at `call_line` into `callee` param `param_index`, which routes it to a `sink_kind` sink. \
            `heuristic_findings` are the older regex source/sink *co-occurrence* in one file (languages without a \
            def-use backend yet): review candidates, not confirmed flows. `io_effect_symbols` lists symbols \
            carrying network/filesystem/database/crypto effects."
    }))
}
