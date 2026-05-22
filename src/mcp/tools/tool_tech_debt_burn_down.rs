//! `tool_tech_debt_burn_down` — phased remediation plan capstone.
//!
//! Aggregates `recommended_fix` items from prior tools (architecture_violations,
//! design_smell_detection, stale_zombie_detector, fix_circular_dependency,
//! technical_debt_analysis, bug_prediction) and bin-packs them into
//! `now / next / later` for a chosen time horizon.
//!
//! Effort budgets per horizon (rough engineer-days):
//! - week:    5 × engineer_count
//! - month:   20 × engineer_count
//! - quarter: 60 × engineer_count
//!
//! Effort cost mapping: small=1, medium=3, large=8 engineer-days.

#![allow(unused_imports)]

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::{debug, info};

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::fix_actions::{EstimatedEffort, FixAction, RecommendedFix};
use crate::mcp::tools::fix_helpers::pool_or_err;

pub async fn tool_tech_debt_burn_down(
    ctx: &SystemContext,
    params: TechDebtBurnDownParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().burn_down_plans.fetch_add(1, Ordering::Relaxed);

    let time_horizon = params.time_horizon.as_deref().unwrap_or("month");
    let time_horizon = match time_horizon {
        "week" | "month" | "quarter" => time_horizon,
        _ => "month",
    };
    let engineer_count = params.engineer_count.unwrap_or(1).max(1);
    let limit = params.limit.unwrap_or(50).max(1);

    let budget_days: f64 = match time_horizon {
        "week" => 5.0,
        "month" => 20.0,
        "quarter" => 60.0,
        _ => 20.0,
    } * engineer_count as f64;

    debug!(
        tool = "tech_debt_burn_down",
        project = %params.project,
        time_horizon,
        engineer_count,
        budget_days,
        limit,
        "MCP tool invoked",
    );

    // Aggregate items from each upstream tool. Each call is best-effort;
    // tools that soft-fail simply contribute 0 items.
    let mut items: Vec<DebtItem> = Vec::new();

    // architecture_violations (Tier 1 enhanced).
    if let Ok(call_result) = super::tool_architecture_violations::tool_architecture_violations(
        ctx,
        ArchitectureViolationsParams {
            project: params.project.clone(),
            layer_config: None,
            severity_threshold: Some("medium".to_string()),
            include_fixes: Some(true),
            excluded_god_module_prefixes: None,
        },
    )
    .await
    {
        items.extend(extract_items(
            &call_result,
            "architecture_violations",
            "violations",
        ));
    }

    // design_smell_detection (Tier 1 enhanced).
    if let Ok(call_result) = super::tool_design_smell_detection::tool_design_smell_detection(
        ctx,
        DesignSmellDetectionParams {
            project: params.project.clone(),
            smells: None,
            limit: Some(limit),
            include_fixes: Some(true),
        },
    )
    .await
    {
        items.extend(extract_items(
            &call_result,
            "design_smell_detection",
            "smells",
        ));
    }

    // stale_zombie_detector (Tier 3).
    if let Ok(call_result) = super::tool_stale_zombie::tool_stale_zombie(
        ctx,
        StaleZombieParams {
            project: params.project.clone(),
            min_days_idle: Some(540),
            max_pagerank_pct: Some(0.25),
            limit: Some(limit),
        },
    )
    .await
    {
        items.extend(extract_items(
            &call_result,
            "stale_zombie_detector",
            "candidates",
        ));
    }

    // fix_circular_dependency (Tier 3).
    if let Ok(call_result) = super::tool_fix_circular_dependency::tool_fix_circular_dependency(
        ctx,
        FixCircularDependencyParams {
            project: params.project.clone(),
            max_cycle_length: Some(8),
            limit: Some(limit),
            prefer_strategy: None,
        },
    )
    .await
    {
        items.extend(extract_items(
            &call_result,
            "fix_circular_dependency",
            "fixes",
        ));
    }

    // technical_debt_analysis (existing diagnostic). It doesn't yet have
    // recommended_fix; treat each finding as a synthetic item with action=add_test.
    if let Ok(call_result) = super::tool_technical_debt_analysis::tool_technical_debt_analysis(
        ctx,
        TechnicalDebtAnalysisParams {
            project: params.project.clone(),
            limit: Some(limit),
            include_todos: Some(true),
        },
    )
    .await
    {
        items.extend(extract_items(
            &call_result,
            "technical_debt_analysis",
            "debt_files",
        ));
    }

    if items.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "scope": params.project,
                "items": [],
                "summary": {
                    "total_items": 0,
                    "by_phase": {"now": 0, "next": 0, "later": 0},
                    "by_action": {},
                },
                "guidance": "No tech-debt items found across upstream tools. Either the project is \
                             healthy, or required data (graph metrics, topics, git history) hasn't \
                             been computed yet — check `index_stats`.",
                "parameters": {
                    "project": params.project,
                    "time_horizon": time_horizon,
                    "engineer_count": engineer_count,
                    "budget_days": budget_days,
                },
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    // Score each item: cost-benefit ratio ranks them.
    for item in &mut items {
        item.cost_days = effort_to_days(&item.effort);
        item.cost_benefit_ratio = item.expected_benefit / item.cost_days.max(0.5);
    }
    items.sort_by(|a, b| {
        b.cost_benefit_ratio
            .partial_cmp(&a.cost_benefit_ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.source_tool.cmp(&b.source_tool))
    });
    items.truncate(limit as usize);

    // Bin-pack into phases.
    let mut accum_now = 0.0_f64;
    let mut accum_next = 0.0_f64;
    let next_budget = budget_days * 2.0;
    for item in &mut items {
        if accum_now + item.cost_days <= budget_days {
            item.phase = "now".into();
            accum_now += item.cost_days;
        } else if accum_next + item.cost_days <= next_budget - budget_days {
            item.phase = "next".into();
            accum_next += item.cost_days;
        } else {
            item.phase = "later".into();
        }
    }

    // Build summary.
    let mut by_phase = HashMap::<&str, u64>::new();
    let mut by_action = HashMap::<String, u64>::new();
    for item in &items {
        *by_phase.entry(item.phase.as_str()).or_default() += 1;
        if let Some(act) = &item.action {
            *by_action.entry(act.clone()).or_default() += 1;
        }
    }

    let item_jsons: Vec<serde_json::Value> = items
        .iter()
        .map(|i| {
            json!({
                "source_tool": i.source_tool,
                "location": i.location,
                "severity": i.severity,
                "expected_benefit": format!("{:.2}", i.expected_benefit),
                "estimated_effort": format!("{:?}", i.effort).to_ascii_lowercase(),
                "cost_days": format!("{:.2}", i.cost_days),
                "cost_benefit_ratio": format!("{:.4}", i.cost_benefit_ratio),
                "phase": i.phase,
                "action": i.action,
                "recommended_fix": i.recommended_fix,
            })
        })
        .collect();

    let result = json!({
        "scope": params.project,
        "items": item_jsons,
        "summary": {
            "total_items": items.len(),
            "by_phase": json!({
                "now": by_phase.get("now").copied().unwrap_or(0),
                "next": by_phase.get("next").copied().unwrap_or(0),
                "later": by_phase.get("later").copied().unwrap_or(0),
            }),
            "by_action": by_action,
            "budget_days_per_horizon": budget_days,
            "accum_now_days": format!("{:.2}", accum_now),
            "accum_next_days": format!("{:.2}", accum_next),
        },
        "parameters": {
            "project": params.project,
            "time_horizon": time_horizon,
            "engineer_count": engineer_count,
            "limit": limit,
        },
        "guidance": format!(
            "Capstone plan composed from {} upstream tools. Items ranked by cost-benefit; \
             phase 'now' fits within {:.0} engineer-days, 'next' is the following sprint, \
             'later' is backlog.",
            5,
            budget_days
        ),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "tech_debt_burn_down",
        items = items.len(),
        accum_now,
        accum_next,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

// ============================================================================
// DebtItem + extraction helpers
// ============================================================================

#[derive(Debug, Clone)]
struct DebtItem {
    source_tool: String,
    location: String,
    severity: String,
    expected_benefit: f64,
    effort: EstimatedEffort,
    action: Option<String>,
    recommended_fix: Option<serde_json::Value>,
    cost_days: f64,
    cost_benefit_ratio: f64,
    phase: String,
}

fn effort_to_days(e: &EstimatedEffort) -> f64 {
    match e {
        EstimatedEffort::Small => 1.0,
        EstimatedEffort::Medium => 3.0,
        EstimatedEffort::Large => 8.0,
    }
}

fn parse_effort(s: &str) -> EstimatedEffort {
    match s {
        "small" => EstimatedEffort::Small,
        "large" => EstimatedEffort::Large,
        _ => EstimatedEffort::Medium,
    }
}

fn severity_weight(s: &str) -> f64 {
    match s {
        "critical" => 4.0,
        "high" => 3.0,
        "medium" => 2.0,
        "low" => 1.0,
        _ => 1.0,
    }
}

/// Extract items from a tool's CallToolResult by walking its `findings_key`
/// array. Each item must have at least one of: type/smell/action.
fn extract_items(
    result: &rmcp::model::CallToolResult,
    source_tool: &str,
    findings_key: &str,
) -> Vec<DebtItem> {
    let mut out = Vec::new();
    for content in result.content.iter() {
        let text = match &content.raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => continue,
        };
        let parsed: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let findings = match parsed.get(findings_key).and_then(|v| v.as_array()) {
            Some(a) => a,
            None => continue,
        };
        for f in findings {
            let location = f["path"]
                .as_str()
                .or_else(|| f["file"].as_str())
                .or_else(|| f["hub_file"].as_str())
                .or_else(|| {
                    f["cycle_files"]
                        .as_array()
                        .and_then(|arr| arr.first())
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("")
                .to_string();
            let severity = f["severity"].as_str().unwrap_or("low").to_string();
            let recommended_fix = f.get("recommended_fix").cloned();
            let action = recommended_fix
                .as_ref()
                .and_then(|fx| fx["action"].as_str().map(String::from));
            let effort = recommended_fix
                .as_ref()
                .and_then(|fx| fx["estimated_effort"].as_str())
                .map(parse_effort)
                .unwrap_or(EstimatedEffort::Medium);

            // expected_benefit: severity-driven, with churn / fix-ratio bonuses when present.
            let mut benefit = severity_weight(&severity);
            if let Some(bp) = f["bug_proneness"].as_f64() {
                benefit *= 1.0 + bp;
            }
            if let Some(churn) = f["churn_rate"].as_f64() {
                benefit *= 1.0 + (churn / 5.0).min(2.0);
            }

            out.push(DebtItem {
                source_tool: source_tool.to_string(),
                location,
                severity,
                expected_benefit: benefit,
                effort,
                action,
                recommended_fix,
                cost_days: 0.0,
                cost_benefit_ratio: 0.0,
                phase: String::new(),
            });
        }
    }
    out
}
