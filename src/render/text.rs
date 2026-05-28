//! Plain-text renderer — monospace, box-drawing table borders, `═` dividers.
//! Same severity glyphs and unicode sparklines as the other text formats.

use super::*;
use crate::quality::findings::Finding;

pub fn render(report: &QualityReport) -> String {
    let mut s = String::new();

    // ── Header ───────────────────────────────────────────────────────────
    let title = format!("QUALITY REPORT: {}", report.project);
    s.push_str(&title);
    s.push('\n');
    s.push_str(
        &glyphs::DOUBLE_H
            .to_string()
            .repeat(title.chars().count().max(40)),
    );
    s.push('\n');
    s.push_str(&format!(
        "Overall: {} (GPA {})   ORR: {}   generated {}   pgmcp {}\n\n",
        grade_str(report.overall_grade()),
        gpa_str(report.overall_gpa()),
        if report.orr_pass() { "PASS" } else { "FAIL" },
        report.computed_at.format("%Y-%m-%d %H:%M UTC"),
        report.pgmcp_version,
    ));

    // ── Pillars ──────────────────────────────────────────────────────────
    s.push_str("PILLARS\n");
    let mut rows = Vec::new();
    for (pillar, pr, trend) in pillars_in_order(report) {
        rows.push(vec![
            pillar.title().to_string(),
            grade_str(pr.and_then(|p| p.grade())).to_string(),
            gpa_str(pr.and_then(|p| p.gpa())),
            severity_summary(&pillar_findings(report, pillar)),
            pr.and_then(biggest_lever)
                .unwrap_or_else(|| "-".to_string()),
            delta_phrase(trend).unwrap_or_default(),
        ]);
    }
    s.push_str(&table(
        &[
            "Pillar",
            "Grade",
            "GPA",
            "Findings",
            "Biggest lever",
            "Trend",
        ],
        &rows,
    ));
    s.push('\n');

    // ── Trend ────────────────────────────────────────────────────────────
    if trend_enabled(report) {
        s.push_str("TREND\n");
        for (pillar, _pr, trend) in pillars_in_order(report) {
            if let Some(t) = trend
                && !t.gpas.is_empty()
            {
                s.push_str(&format!(
                    "  {:<14} {}  ({} -> {})\n",
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
        s.push_str("TOP ISSUES (worst files)\n");
        let rows: Vec<Vec<String>> = top
            .iter()
            .map(|t| {
                vec![
                    t.path.clone(),
                    format!("{:.2}", t.weighted),
                    t.count.to_string(),
                    format!("{} {}", t.worst.glyph(), t.worst.label()),
                ]
            })
            .collect();
        s.push_str(&table(&["File", "Weighted", "Findings", "Worst"], &rows));
        s.push('\n');
    }

    // ── Per-pillar sections ──────────────────────────────────────────────
    for (pillar, pr, _trend) in pillars_in_order(report) {
        let Some(p) = pr else {
            s.push_str(&format!(
                "{}\n{}\nN/A - no scorable dimensions (data unavailable).\n\n",
                pillar.title().to_uppercase(),
                glyphs::H.to_string().repeat(pillar.title().len()),
            ));
            continue;
        };
        let head = format!(
            "{} - {} (GPA {})",
            pillar.title().to_uppercase(),
            grade_str(p.grade()),
            gpa_str(p.gpa()),
        );
        s.push_str(&head);
        s.push('\n');
        s.push_str(&glyphs::H.to_string().repeat(head.chars().count()));
        s.push('\n');
        if let Some(lever) = biggest_lever(p) {
            s.push_str(&format!("  {lever}\n"));
        }
        let rows: Vec<Vec<String>> = dimension_rows(p)
            .into_iter()
            .map(|(name, score, grade, desc)| vec![name, score, grade, desc])
            .collect();
        s.push_str(&table(&["Dimension", "Score", "Grade", "Notes"], &rows));

        if pillar == Pillar::Engineering {
            if !report.orr.is_empty() {
                let gates: Vec<String> = report
                    .orr
                    .iter()
                    .map(|g| format!("{} {}", if g.pass { '✓' } else { '✗' }, g.name))
                    .collect();
                s.push_str(&format!("  ORR: {}\n", gates.join(", ")));
            }
            let eff = effect_breakdown_lines(&report.effect_breakdown);
            if !eff.is_empty() {
                s.push_str(&format!("  Effect breakdown: {}\n", eff.join(", ")));
            }
        }
        s.push('\n');
    }

    // ── Findings ─────────────────────────────────────────────────────────
    if report.options.include_findings {
        s.push_str("FINDINGS\n");
        s.push_str(&glyphs::DOUBLE_H.to_string().repeat(40));
        s.push('\n');
        for cat in category_order() {
            let items = report.displayed_in_category(cat);
            s.push_str(&format!("\n{} ({} findings)\n", cat.title(), items.len()));
            s.push_str(&glyphs::H.to_string().repeat(cat.title().len() + 12));
            s.push('\n');
            if items.is_empty() {
                s.push_str("  No findings.\n");
            } else {
                for f in items {
                    render_finding(&mut s, f, report.options.include_recommended_fixes);
                }
            }
        }
        s.push('\n');
    }

    // ── Appendix ─────────────────────────────────────────────────────────
    s.push_str("APPENDIX - tool runs\n");
    let rows: Vec<Vec<String>> = report
        .tool_runs
        .iter()
        .map(|tr| {
            let note = tr
                .note
                .as_deref()
                .map(|n| format!(" - {n}"))
                .unwrap_or_default();
            vec![
                tr.tool.clone(),
                tr.category.title().to_string(),
                tr.finding_count.to_string(),
                tr.millis.to_string(),
                format!("{}{}", outcome_label(tr.outcome), note),
            ]
        })
        .collect();
    s.push_str(&table(
        &["Tool", "Category", "Findings", "ms", "Outcome"],
        &rows,
    ));
    s.push('\n');

    s.push_str(&format!(
        "Reproduce: pgmcp tool quality_report project={} format=text\n",
        report.project
    ));
    s
}

fn render_finding(s: &mut String, f: &Finding, include_fix: bool) {
    s.push_str(&format!(
        "  {} {:<8} {} - {} ({})\n",
        f.severity.glyph(),
        f.severity.label(),
        f.location_label(),
        f.description,
        f.source_tool,
    ));
    for loc in &f.additional_locations {
        s.push_str(&format!("      also: {}:{}\n", loc.path, loc.start_line));
    }
    if include_fix && let Some(fix) = &f.recommended_fix {
        s.push_str(&format!(
            "      fix: {} ({} effort, confidence {:.2})\n",
            fix.action.as_str(),
            effort_label(fix.estimated_effort),
            fix.confidence,
        ));
    }
}

/// Render a box-drawing table sized to its content. Multi-line cells are not
/// supported (cells are single-line); long text simply widens the column.
fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let border = |left: char, mid: char, right: char| -> String {
        let mut b = String::new();
        b.push(left);
        for (i, w) in widths.iter().enumerate() {
            b.push_str(&glyphs::H.to_string().repeat(w + 2));
            b.push(if i + 1 == cols { right } else { mid });
        }
        b.push('\n');
        b
    };
    let row_line = |cells: &[String]| -> String {
        let mut line = String::new();
        line.push(glyphs::V);
        for (i, w) in widths.iter().enumerate() {
            let cell = cells.get(i).map(|s| s.as_str()).unwrap_or("");
            let pad = w - cell.chars().count();
            line.push(' ');
            line.push_str(cell);
            line.push_str(&" ".repeat(pad + 1));
            line.push(glyphs::V);
        }
        line.push('\n');
        line
    };
    out.push_str(&border(glyphs::TL, glyphs::T_DOWN, glyphs::TR));
    out.push_str(&row_line(
        &headers.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
    ));
    out.push_str(&border(glyphs::T_RIGHT, glyphs::CROSS, glyphs::T_LEFT));
    for row in rows {
        out.push_str(&row_line(row));
    }
    out.push_str(&border(glyphs::BL, glyphs::T_UP, glyphs::BR));
    out
}
