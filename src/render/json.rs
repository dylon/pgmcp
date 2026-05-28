//! Structured-JSON renderer for automated tooling. Unlike the prose formats
//! this inlines the *computed* grades (overall + per-pillar GPA/letter, ORR
//! pass) that are otherwise methods on the report, so a consumer needn't
//! re-derive them. Honors `include_findings` / `min_severity` like the others.
//!
//! Custom Serialize types are routed through `serde_json::to_value` (not the
//! `json!` macro, which only accepts `Into<Value>` leaves).

use serde_json::{Value, json, to_value};

use super::QualityReport;

pub fn render(report: &QualityReport) -> String {
    serde_json::to_string_pretty(&value(report)).unwrap_or_else(|_| "{}".to_string())
}

/// The structured report as a `serde_json::Value` — used by both the `json`
/// format and the `include_underlying_json` envelope.
pub fn value(report: &QualityReport) -> Value {
    let pillars: Vec<Value> = report
        .pillars
        .iter()
        .map(|p| {
            json!({
                "pillar": p.pillar.title(),
                "gpa": p.gpa(),
                "grade": p.grade(),
                "weakest_dimension": p.weakest().map(|d| d.name.as_str()),
                "dimensions": to_value(&p.dimensions).unwrap_or(Value::Null),
            })
        })
        .collect();

    let findings: Vec<Value> = if report.options.include_findings {
        report
            .displayed_findings()
            .iter()
            .map(|f| to_value(f).unwrap_or(Value::Null))
            .collect()
    } else {
        Vec::new()
    };

    let top_issues: Vec<Value> = report
        .top_issues()
        .iter()
        .map(|t| json!({ "path": t.path, "weighted": t.weighted, "count": t.count, "worst": t.worst.label() }))
        .collect();

    let trend: Vec<Value> = report
        .trend
        .iter()
        .map(|t| json!({ "pillar": t.pillar.title(), "gpas": t.gpas.clone() }))
        .collect();

    json!({
        "project": report.project,
        "computed_at": report.computed_at.to_rfc3339(),
        "pgmcp_version": report.pgmcp_version,
        "overall": {
            "gpa": report.overall_gpa(),
            "grade": report.overall_grade(),
            "orr_pass": report.orr_pass(),
        },
        "orr": to_value(&report.orr).unwrap_or(Value::Null),
        "pillars": pillars,
        "trend": trend,
        "top_issues": top_issues,
        "effect_breakdown": report.effect_breakdown.clone(),
        "findings": findings,
        "tool_runs": to_value(&report.tool_runs).unwrap_or(Value::Null),
    })
}
