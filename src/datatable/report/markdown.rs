//! GitHub-flavored Markdown renderer (the default `data_table_report` format).
//! Pipe tables with per-column alignment markers; an inline backtick sparkline
//! for the trend. Reuses `crate::render::sparkline`.

use super::*;
use crate::render::sparkline;

pub fn render(r: &TableReport) -> String {
    let mut s = String::new();
    s.push_str(&format!("# Data Table: {}\n\n", r.title));
    let count = if r.truncated {
        format!(" · showing {} of {}", r.rows.len(), r.total_rows)
    } else {
        format!(" · {} rows", r.total_rows)
    };
    s.push_str(&format!(
        "_generated {}{}_\n\n",
        r.generated_at.format("%Y-%m-%d %H:%M UTC"),
        count,
    ));

    if let Some((col, vals)) = numeric_series(r) {
        let (lo, hi) = min_max(&vals);
        s.push_str(&format!(
            "**Trend ({col}):** `{}`\n\n",
            sparkline(&vals, lo, hi)
        ));
    }

    if let Some(agg) = &r.summary {
        s.push_str("## Summary\n\n");
        let (headers, aligns, rows) = agg_table(agg);
        s.push_str(&md_table(&headers, &rows, &aligns));
        for g in &agg.groups {
            for (field, n) in &g.n_ignored {
                if n.as_i64().unwrap_or(0) > 0 {
                    s.push_str(&format!(
                        "\n_skipped {n} non-numeric value(s) in `{field}`._\n"
                    ));
                }
            }
        }
        s.push('\n');
    }

    s.push_str("## Detail\n\n");
    let headers: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
    let rows: Vec<Vec<String>> = r
        .rows
        .iter()
        .map(|row| row.iter().map(cell_human).collect())
        .collect();
    s.push_str(&md_table(&headers, &rows, &r.aligns()));

    if let Some(cap) = &r.caption {
        s.push_str(&format!("\n_{}_\n", md_escape(cap)));
    }
    s
}

fn md_align(a: Align) -> &'static str {
    match a {
        Align::Left => ":---",
        Align::Right => "---:",
        Align::Center => ":--:",
    }
}

/// Escape a cell for a GitHub pipe table: collapse newlines, escape `|`.
fn md_escape(s: &str) -> String {
    s.replace('\n', " ").replace('|', "\\|")
}

fn md_table(headers: &[String], rows: &[Vec<String>], aligns: &[Align]) -> String {
    let cols = headers.len();
    let mut s = String::new();
    s.push('|');
    for h in headers {
        s.push(' ');
        s.push_str(&md_escape(h));
        s.push_str(" |");
    }
    s.push('\n');
    s.push('|');
    for i in 0..cols {
        s.push_str(md_align(aligns.get(i).copied().unwrap_or(Align::Left)));
        s.push('|');
    }
    s.push('\n');
    for row in rows {
        s.push('|');
        for i in 0..cols {
            let c = row.get(i).map(|x| x.as_str()).unwrap_or("");
            s.push(' ');
            s.push_str(&md_escape(c));
            s.push_str(" |");
        }
        s.push('\n');
    }
    s
}
