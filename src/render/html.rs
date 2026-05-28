//! Self-contained HTML renderer — one document, inline `<style>`, no external
//! assets. `<details>` per finding category mirrors the Markdown collapsibility;
//! the trend strip is an inline SVG (HTML always gets the SVG, never the unicode
//! sparkline). All free text is HTML-escaped.

use super::*;
use crate::quality::findings::Finding;
use crate::quality::report::PillarTrend;

pub fn render(report: &QualityReport) -> String {
    let mut s = String::new();
    s.push_str("<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n");
    s.push_str(&format!(
        "<title>Quality Report: {}</title>\n",
        esc(&report.project)
    ));
    s.push_str(STYLE);
    s.push_str("</head>\n<body>\n");

    // ── Header ───────────────────────────────────────────────────────────
    s.push_str(&format!(
        "<h1>Quality Report: {}</h1>\n",
        esc(&report.project)
    ));
    s.push_str(&format!(
        "<p class=\"meta\"><strong>Overall:</strong> {} (GPA {}) &middot; <strong>ORR:</strong> <span class=\"{}\">{}</span> &middot; <em>generated {} &middot; pgmcp {}</em></p>\n",
        grade_str(report.overall_grade()),
        gpa_str(report.overall_gpa()),
        if report.orr_pass() { "pass" } else { "fail" },
        if report.orr_pass() { "PASS" } else { "FAIL" },
        esc(&report.computed_at.format("%Y-%m-%d %H:%M UTC").to_string()),
        esc(&report.pgmcp_version),
    ));

    // ── Pillars ──────────────────────────────────────────────────────────
    s.push_str("<h2>Pillars</h2>\n<table>\n<tr><th>Pillar</th><th>Grade</th><th>GPA</th><th>Findings</th><th>Biggest lever</th><th>Trend</th></tr>\n");
    for (pillar, pr, trend) in pillars_in_order(report) {
        let spark = trend
            .filter(|t| !t.gpas.is_empty())
            .map(|t| svg_sparkline(&t.ewma(3), 0.0, 4.0))
            .unwrap_or_default();
        s.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{} {}</td></tr>\n",
            esc(pillar.title()),
            grade_str(pr.and_then(|p| p.grade())),
            gpa_str(pr.and_then(|p| p.gpa())),
            esc(&severity_summary(&pillar_findings(report, pillar))),
            esc(&pr
                .and_then(biggest_lever)
                .unwrap_or_else(|| "—".to_string())),
            spark,
            esc(&delta_phrase_html(trend)),
        ));
    }
    s.push_str("</table>\n");

    // ── Top issues ───────────────────────────────────────────────────────
    let top = report.top_issues();
    if !top.is_empty() {
        s.push_str("<h2>Top issues (worst files)</h2>\n<table>\n<tr><th>File</th><th>Weighted</th><th>Findings</th><th>Worst</th></tr>\n");
        for t in &top {
            s.push_str(&format!(
                "<tr><td><code>{}</code></td><td>{:.2}</td><td>{}</td><td>{} {}</td></tr>\n",
                esc(&t.path),
                t.weighted,
                t.count,
                t.worst.glyph(),
                t.worst.label(),
            ));
        }
        s.push_str("</table>\n");
    }

    // ── Per-pillar sections ──────────────────────────────────────────────
    for (pillar, pr, _trend) in pillars_in_order(report) {
        let Some(p) = pr else {
            s.push_str(&format!(
                "<h2>{}</h2>\n<p><em>N/A — no scorable dimensions (data unavailable).</em></p>\n",
                esc(pillar.title())
            ));
            continue;
        };
        s.push_str(&format!(
            "<h2>{} — {} (GPA {})</h2>\n",
            esc(pillar.title()),
            grade_str(p.grade()),
            gpa_str(p.gpa()),
        ));
        if let Some(lever) = biggest_lever(p) {
            s.push_str(&format!(
                "<p class=\"lever\"><em>{}</em></p>\n",
                esc(&lever)
            ));
        }
        s.push_str(
            "<table>\n<tr><th>Dimension</th><th>Score</th><th>Grade</th><th>Notes</th></tr>\n",
        );
        for (name, score, grade, desc) in dimension_rows(p) {
            s.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                esc(&name),
                esc(&score),
                esc(&grade),
                esc(&desc),
            ));
        }
        s.push_str("</table>\n");

        if pillar == Pillar::Engineering {
            if !report.orr.is_empty() {
                s.push_str("<p><strong>Operational Readiness Review:</strong> ");
                let gates: Vec<String> = report
                    .orr
                    .iter()
                    .map(|g| {
                        format!(
                            "<span class=\"{}\">{} {}</span>",
                            if g.pass { "pass" } else { "fail" },
                            if g.pass { "✓" } else { "✗" },
                            esc(&g.name)
                        )
                    })
                    .collect();
                s.push_str(&gates.join(", "));
                s.push_str("</p>\n");
            }
            let eff = effect_breakdown_lines(&report.effect_breakdown);
            if !eff.is_empty() {
                s.push_str(&format!(
                    "<p><strong>Effect breakdown:</strong> {}</p>\n",
                    esc(&eff.join(", "))
                ));
            }
        }
    }

    // ── Findings ─────────────────────────────────────────────────────────
    if report.options.include_findings {
        s.push_str("<h2>Findings</h2>\n");
        for cat in category_order() {
            let items = report.displayed_in_category(cat);
            s.push_str(&format!(
                "<details><summary>{} ({} findings)</summary>\n",
                esc(cat.title()),
                items.len()
            ));
            if items.is_empty() {
                s.push_str("<p><em>No findings.</em></p>\n");
            } else {
                s.push_str("<ul>\n");
                for f in items {
                    render_finding(&mut s, f, report.options.include_recommended_fixes);
                }
                s.push_str("</ul>\n");
            }
            s.push_str("</details>\n");
        }
    }

    // ── Appendix ─────────────────────────────────────────────────────────
    s.push_str("<h2>Appendix — tool runs</h2>\n<table>\n<tr><th>Tool</th><th>Category</th><th>Findings</th><th>ms</th><th>Outcome</th></tr>\n");
    for tr in &report.tool_runs {
        let note = tr
            .note
            .as_deref()
            .map(|n| format!(" — {n}"))
            .unwrap_or_default();
        s.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            esc(&tr.tool),
            esc(tr.category.title()),
            tr.finding_count,
            tr.millis,
            esc(&format!("{}{}", outcome_label(tr.outcome), note)),
        ));
    }
    s.push_str("</table>\n");

    s.push_str(&format!(
        "<hr><p class=\"footer\"><em>Reproduce: <code>pgmcp tool quality_report project={} format=html</code></em></p>\n",
        esc(&report.project)
    ));
    s.push_str("</body></html>\n");
    s
}

const STYLE: &str = r#"<style>
body{font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;max-width:960px;margin:2rem auto;padding:0 1rem;color:#1a1a1a;line-height:1.5}
h1{border-bottom:2px solid #ddd;padding-bottom:.3rem}
h2{margin-top:2rem;border-bottom:1px solid #eee;padding-bottom:.2rem}
table{border-collapse:collapse;width:100%;margin:.5rem 0}
th,td{border:1px solid #ddd;padding:.3rem .5rem;text-align:left;font-size:.92rem}
th{background:#f5f5f5}
code{background:#f0f0f0;padding:.1rem .3rem;border-radius:3px;font-size:.85em}
details{margin:.4rem 0}
summary{cursor:pointer;font-weight:600}
.meta{color:#555}
.pass{color:#127a2b;font-weight:600}
.fail{color:#b00020;font-weight:600}
.lever{color:#555}
.footer{color:#888;font-size:.85rem}
svg{vertical-align:middle}
</style>
"#;

fn render_finding(s: &mut String, f: &Finding, include_fix: bool) {
    s.push_str(&format!(
        "<li>{} <strong>{}</strong> <code>{}</code> — {} <em>({})</em>",
        f.severity.glyph(),
        f.severity.label(),
        esc(&f.location_label()),
        esc(&f.description),
        esc(&f.source_tool),
    ));
    if !f.additional_locations.is_empty() {
        s.push_str("<ul>");
        for loc in &f.additional_locations {
            s.push_str(&format!(
                "<li>also: <code>{}:{}</code></li>",
                esc(&loc.path),
                loc.start_line
            ));
        }
        s.push_str("</ul>");
    }
    if include_fix && let Some(fix) = &f.recommended_fix {
        s.push_str(&format!(
            "<br><span class=\"lever\">fix: <code>{}</code> ({} effort, confidence {:.2})</span>",
            esc(fix.action.as_str()),
            effort_label(fix.estimated_effort),
            fix.confidence,
        ));
    }
    s.push_str("</li>\n");
}

/// Inline SVG sparkline (HTML's trend rendering — never the unicode fallback).
fn svg_sparkline(values: &[f64], min: f64, max: f64) -> String {
    if values.is_empty() {
        return String::new();
    }
    let bar_w = 4u32;
    let gap = 1u32;
    let height = 14u32;
    let range = (max - min).max(1e-9);
    let width = values.len() as u32 * (bar_w + gap);
    let mut svg = format!(
        "<svg width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" role=\"img\" aria-label=\"trend\">"
    );
    for (i, &v) in values.iter().enumerate() {
        let frac = ((v - min) / range).clamp(0.0, 1.0);
        let h = (frac * (height as f64 - 1.0)).round().max(1.0) as u32;
        let x = i as u32 * (bar_w + gap);
        let y = height - h;
        svg.push_str(&format!(
            "<rect x=\"{x}\" y=\"{y}\" width=\"{bar_w}\" height=\"{h}\" fill=\"#4a7\"/>"
        ));
    }
    svg.push_str("</svg>");
    svg
}

fn delta_phrase_html(trend: Option<&PillarTrend>) -> String {
    super::delta_phrase(trend).unwrap_or_default()
}

/// Minimal HTML escape: `& < > " '`.
fn esc(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}
