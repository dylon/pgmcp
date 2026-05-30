//! Standalone LaTeX renderer — a complete `article` document that compiles with
//! `pdflatex`. Per the glyph policy, booleans use `$\checkmark$` / `$\times$`
//! and the sparkline blocks are remapped to `\rule` bars via `\newunicodechar`
//! (so the unicode source still compiles). Free text is escaped; any non-ASCII
//! byte becomes `?` so an exotic value can never break the build.

use super::*;
use crate::render::sparkline;

pub fn render(r: &TableReport) -> String {
    let mut s = String::new();
    s.push_str(PREAMBLE);
    s.push_str(&format!(
        "\\title{{Data Table: {}}}\n\\date{{{}}}\n\\begin{{document}}\n\\maketitle\n\n",
        latex_escape(&r.title),
        latex_escape(&r.generated_at.format("%Y-%m-%d %H:%M UTC").to_string()),
    ));
    let count = if r.truncated {
        format!("showing {} of {} rows", r.rows.len(), r.total_rows)
    } else {
        format!("{} rows", r.total_rows)
    };
    s.push_str(&format!("\\noindent\\textit{{{count}}}\\par\\medskip\n\n"));

    if let Some((col, vals)) = numeric_series(r) {
        let (lo, hi) = min_max(&vals);
        s.push_str(&format!(
            "\\noindent\\textbf{{Trend ({}):}} {}\\par\\medskip\n\n",
            latex_escape(&col),
            sparkline(&vals, lo, hi),
        ));
    }

    if let Some(agg) = &r.summary {
        s.push_str("\\section*{Summary}\n");
        let (headers, aligns, rows) = agg_table(agg);
        let spec: String = aligns.iter().map(|a| align_spec(*a)).collect();
        let esc_headers: Vec<String> = headers.iter().map(|h| latex_escape(h)).collect();
        let esc_rows: Vec<Vec<String>> = rows
            .iter()
            .map(|row| row.iter().map(|c| latex_escape(c)).collect())
            .collect();
        s.push_str(&latex_table(&spec, &esc_headers, &esc_rows));
    }

    s.push_str("\\section*{Detail}\n");
    let spec: String = r.columns.iter().map(|c| col_spec(c.ty)).collect();
    let headers: Vec<String> = r.columns.iter().map(|c| latex_escape(&c.name)).collect();
    let rows: Vec<Vec<String>> = r
        .rows
        .iter()
        .map(|row| row.iter().map(cell_latex).collect())
        .collect();
    s.push_str(&latex_table(&spec, &headers, &rows));

    if let Some(cap) = &r.caption {
        s.push_str(&format!(
            "\\medskip\\noindent\\textit{{{}}}\\par\n",
            latex_escape(cap)
        ));
    }
    s.push_str("\n\\end{document}\n");
    s
}

const PREAMBLE: &str = r#"\documentclass{article}
\usepackage[margin=1in]{geometry}
\usepackage[T1]{fontenc}
\usepackage[utf8]{inputenc}
\usepackage{amssymb}
\usepackage{longtable}
\usepackage{booktabs}
\usepackage{newunicodechar}
\usepackage[hidelinks]{hyperref}
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

/// LaTeX column spec letter for an aggregation alignment.
fn align_spec(a: Align) -> &'static str {
    match a {
        Align::Left => "l",
        Align::Right => "r",
        Align::Center => "c",
    }
}

/// LaTeX column spec for a detail cell type (wrap wide text/json).
fn col_spec(ty: CellType) -> &'static str {
    match ty {
        CellType::Integer | CellType::Number => "r",
        CellType::Boolean => "c",
        CellType::Timestamp => "l",
        CellType::Text | CellType::Json => "p{4cm}",
    }
}

fn latex_table(colspec: &str, headers: &[String], rows: &[Vec<String>]) -> String {
    let mut s = String::new();
    s.push_str(&format!("\\begin{{longtable}}{{{colspec}}}\n\\toprule\n"));
    s.push_str(&headers.join(" & "));
    s.push_str(" \\\\\n\\midrule\n\\endhead\n");
    for row in rows {
        s.push_str(&row.join(" & "));
        s.push_str(" \\\\\n");
    }
    s.push_str("\\bottomrule\n\\end{longtable}\n\n");
    s
}

fn cell_latex(cell: &Cell) -> String {
    if cell.raw.is_null() {
        return "--".to_string();
    }
    match cell.ty {
        CellType::Boolean => match cell.raw.as_bool() {
            Some(true) => "$\\checkmark$".to_string(),
            Some(false) => "$\\times$".to_string(),
            None => latex_escape(&cell.raw.to_string()),
        },
        CellType::Integer | CellType::Number => latex_escape(&fmt_number(&cell.raw)),
        CellType::Timestamp => latex_escape(&fmt_timestamp(&cell.raw)),
        CellType::Text => {
            let owned = cell
                .raw
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| cell.raw.to_string());
            latex_escape(&owned)
        }
        CellType::Json => latex_escape(&compact_json(&cell.raw)),
    }
}

/// Escape LaTeX specials; control chars → space; non-ASCII → `?`.
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
