//! GitHub-flavored Markdown renderer (the default format). Collapsible
//! `<details>` per finding category keeps multi-thousand-finding reports
//! navigable; severity glyphs and sparklines are inline unicode.

use super::*;
use crate::quality::findings::Finding;

pub fn render(report: &QualityReport) -> String {
    let mut s = String::new();

    // ── Header ───────────────────────────────────────────────────────────
    s.push_str(&format!("# Quality Report: {}\n\n", report.project));
    s.push_str(&format!(
        "**Overall:** {} (GPA {}) · **ORR:** {} · _generated {} · pgmcp {}_\n\n",
        grade_str(report.overall_grade()),
        gpa_str(report.overall_gpa()),
        if report.orr_pass() { "PASS" } else { "FAIL" },
        report.computed_at.format("%Y-%m-%d %H:%M UTC"),
        report.pgmcp_version,
    ));

    // ── Pillar summary ───────────────────────────────────────────────────
    s.push_str("## Pillars\n\n");
    s.push_str("| Pillar | Grade | GPA | Findings | Biggest lever | Trend |\n");
    s.push_str("|---|---|---|---|---|---|\n");
    for (pillar, pr, trend) in pillars_in_order(report) {
        let lever = pr
            .and_then(biggest_lever)
            .unwrap_or_else(|| "—".to_string());
        let findings = pillar_findings(report, pillar);
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            pillar.title(),
            grade_str(pr.and_then(|p| p.grade())),
            gpa_str(pr.and_then(|p| p.gpa())),
            severity_summary(&findings),
            lever,
            delta_phrase(trend).unwrap_or_default(),
        ));
    }
    s.push('\n');

    // ── Trend strip ──────────────────────────────────────────────────────
    if trend_enabled(report) {
        s.push_str("## Trend\n\n");
        for (pillar, _pr, trend) in pillars_in_order(report) {
            if let Some(t) = trend
                && !t.gpas.is_empty()
            {
                s.push_str(&format!(
                    "- **{}**: `{}` ({} → {})\n",
                    pillar.title(),
                    sparkline(&t.ewma(3), 0.0, 4.0),
                    gpa_str(t.gpas.first().copied()),
                    gpa_str(t.gpas.last().copied()),
                ));
            }
        }
        s.push('\n');
    }

    // ── Top issues ───────────────────────────────────────────────────────
    let top = report.top_issues();
    if !top.is_empty() {
        s.push_str("## Top issues (worst files)\n\n");
        s.push_str("| File | Weighted | Findings | Worst |\n|---|---|---|---|\n");
        for t in &top {
            s.push_str(&format!(
                "| `{}` | {:.2} | {} | {} {} |\n",
                t.path,
                t.weighted,
                t.count,
                t.worst.glyph(),
                t.worst.label(),
            ));
        }
        s.push('\n');
    }

    // ── Per-pillar sections ──────────────────────────────────────────────
    for (pillar, pr, _trend) in pillars_in_order(report) {
        let Some(p) = pr else {
            s.push_str(&format!(
                "## {}\n\n_N/A — no scorable dimensions (data unavailable)._\n\n",
                pillar.title()
            ));
            continue;
        };
        s.push_str(&format!(
            "## {} — {} (GPA {})\n\n",
            pillar.title(),
            grade_str(p.grade()),
            gpa_str(p.gpa()),
        ));
        if let Some(lever) = biggest_lever(p) {
            s.push_str(&format!("_{lever}_\n\n"));
        }
        s.push_str("| Dimension | Score | Grade | Notes |\n|---|---|---|---|\n");
        for (name, score, grade, desc) in dimension_rows(p) {
            s.push_str(&format!("| {name} | {score} | {grade} | {desc} |\n"));
        }
        s.push('\n');

        if pillar == Pillar::Engineering {
            if !report.orr.is_empty() {
                let gates: Vec<String> = report
                    .orr
                    .iter()
                    .map(|g| format!("{} {}", if g.pass { '✓' } else { '✗' }, g.name))
                    .collect();
                s.push_str(&format!(
                    "**Operational Readiness Review:** {}\n\n",
                    gates.join(", ")
                ));
            }
            let eff = effect_breakdown_lines(&report.effect_breakdown);
            if !eff.is_empty() {
                s.push_str(&format!("**Effect breakdown:** {}\n\n", eff.join(", ")));
            }
        }
    }

    // ── Findings by category ─────────────────────────────────────────────
    if report.options.include_findings {
        s.push_str("## Findings\n\n");
        for cat in category_order() {
            let items = report.displayed_in_category(cat);
            s.push_str(&format!(
                "<details><summary>{} ({} findings)</summary>\n\n",
                cat.title(),
                items.len()
            ));
            if items.is_empty() {
                s.push_str("_No findings._\n\n");
            } else {
                for f in items {
                    render_finding(&mut s, f, report.options.include_recommended_fixes);
                }
                s.push('\n');
            }
            s.push_str("</details>\n\n");
        }
    }

    // ── Appendix ─────────────────────────────────────────────────────────
    s.push_str("## Appendix — tool runs\n\n");
    s.push_str("| Tool | Category | Findings | ms | Outcome |\n|---|---|---|---|---|\n");
    for tr in &report.tool_runs {
        let note = tr
            .note
            .as_deref()
            .map(|n| format!(" — {n}"))
            .unwrap_or_default();
        s.push_str(&format!(
            "| {} | {} | {} | {} | {}{} |\n",
            tr.tool,
            tr.category.title(),
            tr.finding_count,
            tr.millis,
            outcome_label(tr.outcome),
            note,
        ));
    }
    s.push('\n');

    // ── Footer ───────────────────────────────────────────────────────────
    s.push_str(&format!(
        "---\n_Reproduce: `pgmcp tool quality_report project={} format=markdown`_\n",
        report.project
    ));
    s
}

fn render_finding(s: &mut String, f: &Finding, include_fix: bool) {
    s.push_str(&format!(
        "- {} **{}** `{}` — {} _({})_\n",
        f.severity.glyph(),
        f.severity.label(),
        f.location_label(),
        f.description,
        f.source_tool,
    ));
    for loc in &f.additional_locations {
        s.push_str(&format!("  - also: `{}:{}`\n", loc.path, loc.start_line));
    }
    if include_fix && let Some(fix) = &f.recommended_fix {
        s.push_str(&format!(
            "  - fix: `{}` ({} effort, confidence {:.2})\n",
            fix.action.as_str(),
            effort_label(fix.estimated_effort),
            fix.confidence,
        ));
    }
}
