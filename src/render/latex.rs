//! Standalone LaTeX renderer — emits a complete `article` document that
//! compiles directly with `pdflatex`.
//!
//! Per the user's glyph policy we prefer LaTeX commands where one exists:
//! severity uses `amssymb` shapes (`\blacksquare`/`\blacklozenge`/…), arrows use
//! `\rightarrow`/`\blacktriangle…`, math uses `\geq`/`\leq`. The sparkline blocks
//! have no LaTeX idiom, so the unicode block chars are emitted and remapped to
//! `\rule` bars via `\newunicodechar` (keeps the source unicode while still
//! compiling under pdflatex). Free-text from findings is escaped, and any
//! non-ASCII byte is replaced with `?` so an exotic code preview can never break
//! the build.

use super::*;
use crate::quality::findings::Finding;
use crate::quality::report::PillarTrend;

pub fn render(report: &QualityReport) -> String {
    let mut s = String::new();
    s.push_str(PREAMBLE);
    s.push_str(&format!(
        "\\title{{Quality Report: {}}}\n\\date{{{}}}\n\\begin{{document}}\n\\maketitle\n\n",
        latex_escape(&report.project),
        latex_escape(&report.computed_at.format("%Y-%m-%d %H:%M UTC").to_string()),
    ));

    s.push_str(&format!(
        "\\noindent\\textbf{{Overall:}} {} (GPA {}) \\quad \\textbf{{ORR:}} {} \\quad \\textit{{pgmcp {}}}\\par\\medskip\n\n",
        grade_str(report.overall_grade()),
        gpa_str(report.overall_gpa()),
        if report.orr_pass() { "PASS" } else { "FAIL" },
        latex_escape(&report.pgmcp_version),
    ));

    // ── Pillars ──────────────────────────────────────────────────────────
    s.push_str("\\section*{Pillars}\n");
    s.push_str("\\begin{longtable}{l l l l p{4.5cm} l}\n\\toprule\n");
    s.push_str(
        "Pillar & Grade & GPA & Findings & Biggest lever & Trend \\\\\n\\midrule\n\\endhead\n",
    );
    for (pillar, pr, trend) in pillars_in_order(report) {
        s.push_str(&format!(
            "{} & {} & {} & {} & {} & {} \\\\\n",
            latex_escape(pillar.title()),
            grade_str(pr.and_then(|p| p.grade())),
            gpa_str(pr.and_then(|p| p.gpa())),
            latex_escape(&severity_summary(&pillar_findings(report, pillar))),
            latex_escape(
                &pr.and_then(biggest_lever)
                    .unwrap_or_else(|| "--".to_string())
            ),
            delta_tex(trend),
        ));
    }
    s.push_str("\\bottomrule\n\\end{longtable}\n\n");

    // ── Trend ────────────────────────────────────────────────────────────
    if trend_enabled(report) {
        s.push_str("\\section*{Trend}\n\\begin{itemize}\n");
        for (pillar, _pr, trend) in pillars_in_order(report) {
            if let Some(t) = trend
                && !t.gpas.is_empty()
            {
                s.push_str(&format!(
                    "\\item \\textbf{{{}}}: {} ({} $\\rightarrow$ {})\n",
                    latex_escape(pillar.title()),
                    sparkline(&t.ewma(3), 0.0, 4.0),
                    gpa_str(t.gpas.first().copied()),
                    gpa_str(t.gpas.last().copied()),
                ));
            }
        }
        s.push_str("\\end{itemize}\n\n");
    }

    // ── Top issues ───────────────────────────────────────────────────────
    let top = report.top_issues();
    if !top.is_empty() {
        s.push_str("\\section*{Top issues (worst files)}\n");
        s.push_str("\\begin{longtable}{p{8cm} l l l}\n\\toprule\n");
        s.push_str("File & Weighted & Findings & Worst \\\\\n\\midrule\n\\endhead\n");
        for t in &top {
            s.push_str(&format!(
                "\\texttt{{{}}} & {:.2} & {} & {} {} \\\\\n",
                latex_escape(&t.path),
                t.weighted,
                t.count,
                sev_tex(t.worst),
                t.worst.label(),
            ));
        }
        s.push_str("\\bottomrule\n\\end{longtable}\n\n");
    }

    // ── Per-pillar sections ──────────────────────────────────────────────
    for (pillar, pr, _trend) in pillars_in_order(report) {
        let Some(p) = pr else {
            s.push_str(&format!(
                "\\section*{{{}}}\nN/A --- no scorable dimensions (data unavailable).\n\n",
                latex_escape(pillar.title())
            ));
            continue;
        };
        s.push_str(&format!(
            "\\section*{{{} --- {} (GPA {})}}\n",
            latex_escape(pillar.title()),
            grade_str(p.grade()),
            gpa_str(p.gpa()),
        ));
        if let Some(lever) = biggest_lever(p) {
            s.push_str(&format!(
                "\\textit{{{}}}\\par\\medskip\n",
                latex_escape(&lever)
            ));
        }
        s.push_str("\\begin{longtable}{l l l p{7cm}}\n\\toprule\n");
        s.push_str("Dimension & Score & Grade & Notes \\\\\n\\midrule\n\\endhead\n");
        for (name, score, grade, desc) in dimension_rows(p) {
            s.push_str(&format!(
                "{} & {} & {} & {} \\\\\n",
                latex_escape(&name),
                latex_escape(&score),
                grade,
                latex_escape(&desc),
            ));
        }
        s.push_str("\\bottomrule\n\\end{longtable}\n\n");

        if pillar == Pillar::Engineering {
            if !report.orr.is_empty() {
                let gates: Vec<String> = report
                    .orr
                    .iter()
                    .map(|g| {
                        let mark = if g.pass { "$\\checkmark$" } else { "$\\times$" };
                        format!("{} {}", mark, latex_escape(&g.name))
                    })
                    .collect();
                s.push_str(&format!(
                    "\\noindent\\textbf{{ORR:}} {}\\par\\medskip\n",
                    gates.join(", ")
                ));
            }
            let eff = effect_breakdown_lines(&report.effect_breakdown);
            if !eff.is_empty() {
                let joined = eff
                    .iter()
                    .map(|l| latex_escape(l))
                    .collect::<Vec<_>>()
                    .join(", ");
                s.push_str(&format!(
                    "\\noindent\\textbf{{Effect breakdown:}} {joined}\\par\\medskip\n"
                ));
            }
        }
    }

    // ── Findings ─────────────────────────────────────────────────────────
    if report.options.include_findings {
        s.push_str("\\section*{Findings}\n");
        for cat in category_order() {
            let items = report.displayed_in_category(cat);
            s.push_str(&format!(
                "\\subsection*{{{} ({} findings)}}\n",
                latex_escape(cat.title()),
                items.len()
            ));
            if items.is_empty() {
                s.push_str("No findings.\\par\n");
            } else {
                s.push_str("\\begin{itemize}\n");
                for f in items {
                    render_finding(&mut s, f, report.options.include_recommended_fixes);
                }
                s.push_str("\\end{itemize}\n");
            }
        }
        s.push('\n');
    }

    // ── Appendix ─────────────────────────────────────────────────────────
    s.push_str("\\section*{Appendix --- tool runs}\n");
    s.push_str("\\begin{longtable}{l l l l l}\n\\toprule\n");
    s.push_str("Tool & Category & Findings & ms & Outcome \\\\\n\\midrule\n\\endhead\n");
    for tr in &report.tool_runs {
        let note = tr
            .note
            .as_deref()
            .map(|n| format!(" --- {n}"))
            .unwrap_or_default();
        s.push_str(&format!(
            "{} & {} & {} & {} & {} \\\\\n",
            latex_escape(&tr.tool),
            latex_escape(tr.category.title()),
            tr.finding_count,
            tr.millis,
            latex_escape(&format!("{}{}", outcome_label(tr.outcome), note)),
        ));
    }
    s.push_str("\\bottomrule\n\\end{longtable}\n\n");

    s.push_str(&format!(
        "\\vfill\\noindent\\rule{{\\linewidth}}{{0.4pt}}\\par\n\\textit{{Reproduce: pgmcp tool quality\\_report project={} format=latex}}\n\n\\end{{document}}\n",
        latex_escape(&report.project)
    ));
    s
}

const PREAMBLE: &str = r#"\documentclass{article}
\usepackage[margin=1in]{geometry}
\usepackage[T1]{fontenc}
\usepackage[utf8]{inputenc}
\usepackage{amssymb}
\usepackage{xcolor}
\usepackage{longtable}
\usepackage{booktabs}
\usepackage{newunicodechar}
\usepackage[hidelinks]{hyperref}
% Sparkline blocks have no LaTeX idiom: remap the eight unicode block elements
% to rules of increasing height so the unicode source still compiles + renders.
\newunicodechar{▁}{\rule{0.45em}{0.12ex}}
\newunicodechar{▂}{\rule{0.45em}{0.30ex}}
\newunicodechar{▃}{\rule{0.45em}{0.50ex}}
\newunicodechar{▄}{\rule{0.45em}{0.70ex}}
\newunicodechar{▅}{\rule{0.45em}{0.95ex}}
\newunicodechar{▆}{\rule{0.45em}{1.20ex}}
\newunicodechar{▇}{\rule{0.45em}{1.45ex}}
\newunicodechar{█}{\rule{0.45em}{1.70ex}}
\setlength{\parindent}{0pt}
"#;

/// Severity → amssymb LaTeX command (colored for the top two tiers).
fn sev_tex(sev: Severity) -> &'static str {
    match sev {
        Severity::Critical => "\\textcolor{red}{$\\blacksquare$}",
        Severity::High => "\\textcolor{orange}{$\\blacklozenge$}",
        Severity::Medium => "$\\lozenge$",
        Severity::Low => "$\\bigcirc$",
        Severity::Info => "$\\cdot$",
    }
}

/// Per-pillar delta phrase using LaTeX arrows.
fn delta_tex(trend: Option<&PillarTrend>) -> String {
    let Some((prev, latest)) = trend.and_then(|t| t.delta()) else {
        return String::new();
    };
    let arrow = if latest > prev + 1e-6 {
        "$\\blacktriangle$"
    } else if latest < prev - 1e-6 {
        "$\\blacktriangledown$"
    } else {
        "$\\rightarrow$"
    };
    format!("{arrow} from {}", crate::quality::report::gpa_letter(prev))
}

fn render_finding(s: &mut String, f: &Finding, include_fix: bool) {
    s.push_str(&format!(
        "\\item {} \\textbf{{{}}} \\texttt{{{}}} --- {} \\textit{{({})}}\n",
        sev_tex(f.severity),
        f.severity.label(),
        latex_escape(&f.location_label()),
        latex_escape(&f.description),
        latex_escape(&f.source_tool),
    ));
    if !f.additional_locations.is_empty() {
        s.push_str("\\begin{itemize}\n");
        for loc in &f.additional_locations {
            s.push_str(&format!(
                "\\item also: \\texttt{{{}:{}}}\n",
                latex_escape(&loc.path),
                loc.start_line
            ));
        }
        s.push_str("\\end{itemize}\n");
    }
    if include_fix && let Some(fix) = &f.recommended_fix {
        s.push_str(&format!(
            "\\par\\quad fix: \\texttt{{{}}} ({} effort, confidence {:.2})\n",
            latex_escape(fix.action.as_str()),
            effort_label(fix.estimated_effort),
            fix.confidence,
        ));
    }
}

/// Escape LaTeX specials; control chars → space; non-ASCII → `?` so no exotic
/// byte from a code preview can break `pdflatex`.
fn latex_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for c in input.chars() {
        match c {
            '\\' => out.push_str("\\textbackslash{}"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '$' => out.push_str("\\$"),
            '&' => out.push_str("\\&"),
            '#' => out.push_str("\\#"),
            '_' => out.push_str("\\_"),
            '%' => out.push_str("\\%"),
            '~' => out.push_str("\\textasciitilde{}"),
            '^' => out.push_str("\\textasciicircum{}"),
            c if (c as u32) < 0x20 => out.push(' '),
            c if (c as u32) > 0x7e => out.push('?'),
            c => out.push(c),
        }
    }
    out
}
