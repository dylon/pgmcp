//! Multi-format rendering of a [`QualityReport`].
//!
//! No templating crate — the codebase renders by hand (`work_items/reporting.rs`,
//! `experiment/ledger.rs`, `sessions::render_session_mandates_md`). We follow
//! that idiom with a closed [`ReportFormat`] enum dispatching to five per-module
//! `render(&QualityReport) -> String` functions, rather than introducing the
//! tree's first renderer trait.
//!
//! Shared view-model helpers (severity tallies, sparklines, grade strings) live
//! here so the five formats stay thin and consistent. Unicode glyph policy is in
//! [`glyphs`]; the LaTeX backend is the only one that prefers LaTeX commands
//! where a typographic equivalent exists.
#![allow(dead_code)]

mod html;
mod json;
mod latex;
mod markdown;
mod org;
mod text;

use crate::quality::report::PillarTrend;
use crate::quality::{Finding, FindingCategory, Pillar, PillarReport, QualityReport, Severity};

/// The five output renditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    Markdown,
    Org,
    Latex,
    Html,
    Text,
    /// Structured JSON for automated tooling (computed grades inlined).
    Json,
}

impl ReportFormat {
    /// Parse a `format` param value. Returns `None` for unrecognized input so
    /// the caller errors cleanly instead of silently defaulting.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "markdown" | "md" | "gfm" => Some(ReportFormat::Markdown),
            "org" | "orgmode" | "org-mode" => Some(ReportFormat::Org),
            "latex" | "tex" => Some(ReportFormat::Latex),
            "html" => Some(ReportFormat::Html),
            "text" | "txt" | "plain" => Some(ReportFormat::Text),
            "json" => Some(ReportFormat::Json),
            _ => None,
        }
    }

    /// Pipe-delimited valid values, for error messages.
    pub fn valid_values() -> &'static str {
        "markdown|org|latex|html|text|json"
    }
}

/// The structured report as a JSON value (for the `include_underlying_json`
/// envelope; the same shape the `json` format renders as text).
pub fn report_json_value(report: &QualityReport) -> serde_json::Value {
    json::value(report)
}

/// Render `report` in `fmt`.
pub fn render(report: &QualityReport, fmt: ReportFormat) -> String {
    match fmt {
        ReportFormat::Markdown => markdown::render(report),
        ReportFormat::Org => org::render(report),
        ReportFormat::Latex => latex::render(report),
        ReportFormat::Html => html::render(report),
        ReportFormat::Text => text::render(report),
        ReportFormat::Json => json::render(report),
    }
}

/// Unicode glyph constants shared by all renderers (geometric/box-drawing/block —
/// never emoji, per the user's rendering policy). The LaTeX backend substitutes
/// LaTeX commands where one exists; everything else uses these verbatim.
pub mod glyphs {
    /// Block-element ramp for sparklines (lowest → highest).
    pub const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    /// Box-drawing (plain-text tables only).
    pub const TL: char = '┌';
    pub const TR: char = '┐';
    pub const BL: char = '└';
    pub const BR: char = '┘';
    pub const H: char = '─';
    pub const V: char = '│';
    pub const CROSS: char = '┼';
    pub const T_DOWN: char = '┬';
    pub const T_UP: char = '┴';
    pub const T_RIGHT: char = '├';
    pub const T_LEFT: char = '┤';
    pub const DOUBLE_H: char = '═';
    /// Delta arrows.
    pub const UP: char = '▲';
    pub const DOWN: char = '▼';
    pub const FLAT: char = '→';
}

// ── Shared view-model helpers ────────────────────────────────────────────────

/// `Some(gpa)` → `"3.50"`, `None` → `"N/A"`.
pub(crate) fn gpa_str(gpa: Option<f64>) -> String {
    match gpa {
        Some(g) => format!("{g:.2}"),
        None => "N/A".to_string(),
    }
}

/// `Some("B")` → `"B"`, `None` → `"N/A"`.
pub(crate) fn grade_str(grade: Option<&str>) -> &str {
    grade.unwrap_or("N/A")
}

/// Severity tally `[Critical, High, Medium, Low, Info]` over a finding slice.
pub(crate) fn severity_counts(findings: &[&Finding]) -> [usize; 5] {
    let mut c = [0usize; 5];
    for f in findings {
        let idx = match f.severity {
            Severity::Critical => 0,
            Severity::High => 1,
            Severity::Medium => 2,
            Severity::Low => 3,
            Severity::Info => 4,
        };
        c[idx] += 1;
    }
    c
}

/// Displayed findings whose category maps to `pillar`.
pub(crate) fn pillar_findings(report: &QualityReport, pillar: Pillar) -> Vec<&Finding> {
    report
        .displayed_findings()
        .into_iter()
        .filter(|f| f.category.pillar() == pillar)
        .collect()
}

/// Compact `"C:1 H:2 M:0 L:3"` severity summary (omits zero buckets; empty → "—").
pub(crate) fn severity_summary(findings: &[&Finding]) -> String {
    let c = severity_counts(findings);
    let labels = ["C", "H", "M", "L", "I"];
    let parts: Vec<String> = labels
        .iter()
        .zip(c.iter())
        .filter(|(_, n)| **n > 0)
        .map(|(l, n)| format!("{l}:{n}"))
        .collect();
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" ")
    }
}

/// Unicode block sparkline over `values` scaled to `[min,max]`.
pub(crate) fn sparkline(values: &[f64], min: f64, max: f64) -> String {
    if values.is_empty() {
        return String::new();
    }
    let range = (max - min).max(1e-9);
    values
        .iter()
        .map(|&v| {
            let frac = ((v - min) / range).clamp(0.0, 1.0);
            let idx = (frac * (glyphs::SPARK.len() - 1) as f64).round() as usize;
            glyphs::SPARK[idx.min(glyphs::SPARK.len() - 1)]
        })
        .collect()
}

/// Per-pillar delta phrase, e.g. `"▲ from C"`. `None` if there is no prior point
/// or trends are disabled.
pub(crate) fn delta_phrase(trend: Option<&PillarTrend>) -> Option<String> {
    let (prev, latest) = trend?.delta()?;
    let arrow = if latest > prev + 1e-6 {
        glyphs::UP
    } else if latest < prev - 1e-6 {
        glyphs::DOWN
    } else {
        glyphs::FLAT
    };
    Some(format!(
        "{arrow} from {}",
        crate::quality::report::gpa_letter(prev)
    ))
}

/// Whether the trend strip should render at all (history present & enabled).
pub(crate) fn trend_enabled(report: &QualityReport) -> bool {
    report.options.trend_points > 0 && report.trend.iter().any(|t| !t.gpas.is_empty())
}

/// Dimension rows for a pillar table: `(name, score_str, grade_str, description)`.
pub(crate) fn dimension_rows(pillar: &PillarReport) -> Vec<(String, String, String, String)> {
    pillar
        .dimensions
        .iter()
        .map(|d| {
            let (score, grade) = match d.score {
                Some(s) => (format!("{s:.1}"), d.grade().unwrap_or("N/A").to_string()),
                None => ("N/A".to_string(), "N/A".to_string()),
            };
            (d.name.clone(), score, grade, d.description.clone())
        })
        .collect()
}

/// "biggest lever" phrase for a pillar, e.g.
/// `"lowest: coupling → run coupling_cohesion_report"`. The remediation tool is
/// looked up from a small dimension→tool map; unknown dims omit the suffix.
pub(crate) fn biggest_lever(pillar: &PillarReport) -> Option<String> {
    let weakest = pillar.weakest()?;
    let tool = remediation_tool(&weakest.name);
    Some(match tool {
        Some(t) => format!("lowest: {} → run `{t}`", weakest.name),
        None => format!("lowest: {}", weakest.name),
    })
}

/// Dimension name → the MCP tool a reader should run to drill in.
fn remediation_tool(dim: &str) -> Option<&'static str> {
    Some(match dim {
        "coupling" | "loose_coupling" | "oo_coupling" => "coupling_cohesion_report",
        "dependency_health" | "acyclicity" => "circular_dependencies",
        "complexity" => "complexity_hotspots",
        "test_quality" | "test_coverage" => "test_coverage_gaps",
        "documentation" | "doc_coverage" => "doc_coverage_gaps",
        "code_stability" | "api_stability" => "bug_prediction",
        "propagation_cost" | "code_organization" | "module_balance" | "sdp_compliance" => {
            "architecture_violations"
        }
        "secret_hygiene" => "secret_detection",
        "injection_risk" => "injection_candidates",
        "crypto_hygiene" => "crypto_misuse",
        "supply_chain" => "cve_supply_chain",
        "finding_density" => "engineering_scorecard",
        _ => return None,
    })
}

/// Iteration order pairing each pillar with its (optional) report + trend.
pub(crate) fn pillars_in_order(
    report: &QualityReport,
) -> Vec<(Pillar, Option<&PillarReport>, Option<&PillarTrend>)> {
    Pillar::ALL
        .iter()
        .map(|&p| (p, report.pillar(p), report.trend_for(p)))
        .collect()
}

/// Categories that actually have at least one displayed finding (for "No
/// findings" handling the renderers still iterate all eight).
pub(crate) fn category_order() -> [FindingCategory; 8] {
    FindingCategory::ALL
}

/// Human lines for the effect-breakdown channel (a JSON object of
/// `effect → count`, from `sema_helpers::effects::effect_counts`). Sorted by
/// count desc; empty/non-object yields no lines.
pub(crate) fn effect_breakdown_lines(v: &serde_json::Value) -> Vec<String> {
    let Some(obj) = v.as_object() else {
        return Vec::new();
    };
    let mut pairs: Vec<(&String, i64)> = obj
        .iter()
        .filter_map(|(k, val)| val.as_i64().map(|n| (k, n)))
        .filter(|(_, n)| *n > 0)
        .collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    pairs
        .into_iter()
        .map(|(k, n)| format!("{k}: {n}"))
        .collect()
}

/// Lowercase tool-outcome label for the appendix.
pub(crate) fn outcome_label(outcome: crate::quality::report::ToolOutcome) -> &'static str {
    use crate::quality::report::ToolOutcome::*;
    match outcome {
        Ran => "ran",
        DataUnavailable => "data unavailable",
        ErroredOrTimedOut => "errored / timed out",
    }
}

/// Effort label for a `RecommendedFix`.
pub(crate) fn effort_label(e: crate::mcp::tools::fix_actions::EstimatedEffort) -> &'static str {
    use crate::mcp::tools::fix_actions::EstimatedEffort::*;
    match e {
        Small => "small",
        Medium => "medium",
        Large => "large",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_aliases_and_rejects_garbage() {
        assert_eq!(ReportFormat::parse("MD"), Some(ReportFormat::Markdown));
        assert_eq!(ReportFormat::parse("org-mode"), Some(ReportFormat::Org));
        assert_eq!(ReportFormat::parse("tex"), Some(ReportFormat::Latex));
        assert_eq!(ReportFormat::parse("HTML"), Some(ReportFormat::Html));
        assert_eq!(ReportFormat::parse("plain"), Some(ReportFormat::Text));
        assert_eq!(ReportFormat::parse("pdf"), None);
    }

    #[test]
    fn sparkline_maps_extremes() {
        let s = sparkline(&[0.0, 4.0], 0.0, 4.0);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars[0], '▁');
        assert_eq!(chars[1], '█');
        assert_eq!(sparkline(&[], 0.0, 4.0), "");
    }

    #[test]
    fn severity_summary_omits_zeros() {
        // Build cheap findings inline.
        use crate::quality::findings::Finding;
        let c = Finding::new("t", FindingCategory::Security, "p", Severity::Critical, "x");
        let l = Finding::new("t", FindingCategory::Security, "p", Severity::Low, "x");
        let refs = vec![&c, &l];
        assert_eq!(severity_summary(&refs), "C:1 L:1");
        assert_eq!(severity_summary(&[]), "—");
    }

    use crate::quality::findings::Finding as F;
    use crate::quality::report::{DimensionScore, OrrGate, ReportOptions, ToolOutcome, ToolRun};

    fn fixture() -> QualityReport {
        let eng = PillarReport {
            pillar: Pillar::Engineering,
            dimensions: vec![
                DimensionScore::present("complexity", "Absence of complex files", 85.0),
                DimensionScore::present("finding_density", "Finding load", 95.0),
            ],
        };
        let arch = PillarReport {
            pillar: Pillar::Architecture,
            dimensions: vec![
                DimensionScore::present("loose_coupling", "Coupling", 75.0),
                DimensionScore::absent("propagation_cost", "DSM (no graph)"),
            ],
        };
        let sec = PillarReport {
            pillar: Pillar::Security,
            dimensions: vec![DimensionScore::present("secret_hygiene", "Secrets", 60.0)],
        };
        let findings = vec![
            F::new(
                "secret_detection",
                FindingCategory::Security,
                "demo",
                Severity::Critical,
                "hardcoded key",
            )
            .at("src/a.rs", 3),
            F::new(
                "complexity_hotspots",
                FindingCategory::CodeHealth,
                "demo",
                Severity::High,
                "very complex",
            )
            .at_file("src/b.rs"),
            F::new(
                "find_orphans",
                FindingCategory::Hygiene,
                "demo",
                Severity::Low,
                "weak topic",
            )
            .at_file("src/c.rs"),
        ];
        QualityReport {
            project: "demo".into(),
            computed_at: chrono::Utc::now(),
            pgmcp_version: "test".into(),
            pillars: vec![eng, arch, sec],
            findings,
            orr: vec![
                OrrGate {
                    name: "no_circular_deps".into(),
                    pass: true,
                },
                OrrGate {
                    name: "test_coverage".into(),
                    pass: false,
                },
            ],
            effect_breakdown: serde_json::json!({ "unsafe": 2, "blocking_io": 1 }),
            tool_runs: vec![ToolRun {
                tool: "secret_detection".into(),
                category: FindingCategory::Security,
                finding_count: 1,
                millis: 5,
                outcome: ToolOutcome::Ran,
                note: None,
            }],
            trend: vec![crate::quality::report::PillarTrend {
                pillar: Pillar::Engineering,
                gpas: vec![3.0, 3.2, 3.5],
            }],
            options: ReportOptions::default(),
        }
    }

    #[test]
    fn every_format_renders_nonempty_with_project_name() {
        let r = fixture();
        for fmt in [
            ReportFormat::Markdown,
            ReportFormat::Org,
            ReportFormat::Latex,
            ReportFormat::Html,
            ReportFormat::Text,
            ReportFormat::Json,
        ] {
            let s = render(&r, fmt);
            assert!(!s.trim().is_empty(), "{fmt:?} produced empty output");
            assert!(s.contains("demo"), "{fmt:?} missing project name");
        }
    }

    #[test]
    fn latex_is_a_complete_document() {
        let s = render(&fixture(), ReportFormat::Latex);
        assert!(s.contains("\\documentclass{article}"));
        assert!(s.contains("\\begin{document}"));
        assert!(s.contains("\\end{document}"));
    }

    #[test]
    fn html_escapes_finding_text() {
        let mut r = fixture();
        r.findings.push(F::new(
            "x",
            FindingCategory::Hygiene,
            "demo",
            Severity::Low,
            "<script>alert(1)</script>",
        ));
        let s = render(&r, ReportFormat::Html);
        assert!(s.contains("&lt;script&gt;"), "must escape angle brackets");
        assert!(!s.contains("<script>alert"), "must not emit raw script");
    }

    #[test]
    fn json_is_valid_and_inlines_grades() {
        let s = render(&fixture(), ReportFormat::Json);
        let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
        assert_eq!(v["project"], "demo");
        assert!(v["overall"]["grade"].is_string());
        assert!(v["pillars"].is_array());
        assert!(v["findings"].as_array().is_some());
    }

    #[test]
    fn min_severity_filters_and_include_findings_toggles() {
        let mut r = fixture();
        r.options.min_severity = Severity::High;
        assert!(
            r.displayed_findings()
                .iter()
                .all(|f| f.severity_rank >= Severity::High.rank()),
            "Low finding must be filtered out"
        );
        // include_findings=false → markdown omits the per-category findings section.
        r.options.include_findings = false;
        let md = render(&r, ReportFormat::Markdown);
        assert!(
            !md.contains("<details><summary>Security"),
            "findings section must be skipped"
        );
    }
}
