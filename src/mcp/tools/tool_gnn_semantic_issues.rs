//! `tool_gnn_semantic_issues` (Phase 8 — 20th of 20).
//!
//! Uses libgrammstein's `GnnSemanticScorer::detect_issues` which is a
//! heuristic CPG walk (the `code-neural` feature gates a placeholder
//! inference path; the actual default detection uses the structural
//! DFG-edge walk that ships with the `code` feature alone). pgmcp's
//! Cargo.toml stays on `code` (without `code-neural`) so the upstream
//! `ort` 2.x / `ndarray 0.17` vs libgrammstein-top-level `ndarray 0.16`
//! version skew does not block this tool.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use libgrammstein::code::ast::CodeParser;
use libgrammstein::code::gnn::{GnnSemanticScorer, IssueType};
use libgrammstein::code::languages::python::Python;
use libgrammstein::code::{CodePropertyGraph, ParsedCode};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::GnnSemanticIssuesParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: GnnSemanticIssuesParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let cpg = match params.language.as_str() {
        "python" => {
            let mut parser = CodeParser::new(Arc::new(Python))
                .map_err(|e| McpError::internal_error(format!("parser init: {e:?}"), None))?;
            let parsed: ParsedCode = parser
                .parse(&params.code)
                .map_err(|e| McpError::internal_error(format!("parse: {e:?}"), None))?;
            CodePropertyGraph::from_parsed_code(&parsed)
        }
        other => {
            return Err(McpError::invalid_params(
                format!("gnn_semantic_issues: unsupported language `{other}` (currently: python)"),
                None,
            ));
        }
    };

    let scorer = GnnSemanticScorer::default_scorer();
    let issues = scorer.detect_issues(&cpg);

    let issues_json: Vec<serde_json::Value> = issues
        .into_iter()
        .map(|i| {
            json!({
                "node_idx": i.node_idx,
                "issue_type": match i.issue_type {
                    IssueType::VariableMisuse => "VariableMisuse",
                    IssueType::TypeError => "TypeError",
                    IssueType::MissingErrorHandling => "MissingErrorHandling",
                    IssueType::NullDereference => "NullDereference",
                    IssueType::UnusedBinding => "UnusedBinding",
                    IssueType::ApiMisuse => "ApiMisuse",
                    IssueType::ResourceLeak => "ResourceLeak",
                    IssueType::Anomaly => "Anomaly",
                },
                "confidence": i.confidence,
                "suggestion": i.suggestion,
                "related_nodes": i.related_nodes,
            })
        })
        .collect();

    json_result(&json!({
        "language": params.language,
        "node_count": cpg.node_count(),
        "edge_count": cpg.edge_count(),
        "issue_count": issues_json.len(),
        "issues": issues_json,
        "guidance": "Heuristic CPG-walk detection. The libgrammstein code-neural \
                     feature gates a placeholder inference path that the current \
                     upstream detector does not use — the issues you see come from \
                     the structural DFG-edge walk."
    }))
}
