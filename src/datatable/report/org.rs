//! Emacs Org-mode renderer. `*` headings, `|`-delimited tables with an
//! `|---+---|` rule (Org realigns on open), unicode sparkline for the trend.

use super::*;
use crate::render::sparkline;

pub fn render(r: &TableReport) -> String {
    let mut s = String::new();
    s.push_str(&format!("#+TITLE: Data Table: {}\n", r.title));
    s.push_str(&format!(
        "#+DATE: {}\n",
        r.generated_at.format("%Y-%m-%d %H:%M UTC")
    ));
    let count = if r.truncated {
        format!("showing {} of {} rows", r.rows.len(), r.total_rows)
    } else {
        format!("{} rows", r.total_rows)
    };
    s.push_str(&format!("#+SUBTITLE: {count}\n\n"));

    if let Some((col, vals)) = numeric_series(r) {
        let (lo, hi) = min_max(&vals);
        s.push_str(&format!(
            "- *Trend ({col}):* {}\n\n",
            sparkline(&vals, lo, hi)
        ));
    }

    if let Some(agg) = &r.summary {
        s.push_str("* Summary\n");
        let (headers, _aligns, rows) = agg_table(agg);
        s.push_str(&org_table(&headers, &rows));
        for g in &agg.groups {
            for (field, n) in &g.n_ignored {
                if n.as_i64().unwrap_or(0) > 0 {
                    s.push_str(&format!(
                        "  - skipped {n} non-numeric value(s) in ={field}=\n"
                    ));
                }
            }
        }
        s.push('\n');
    }

    s.push_str("* Detail\n");
    let headers: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
    let rows: Vec<Vec<String>> = r
        .rows
        .iter()
        .map(|row| row.iter().map(cell_human).collect())
        .collect();
    s.push_str(&org_table(&headers, &rows));

    if let Some(cap) = &r.caption {
        s.push_str(&format!("\n/{}/\n", org_escape(cap)));
    }
    s
}

/// Escape a cell for an Org table: collapse newlines, replace the cell
/// separator `|` with the visually similar broken bar so the table parses.
fn org_escape(s: &str) -> String {
    s.replace('\n', " ").replace('|', "¦")
}

fn org_table(headers: &[String], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    let mut s = String::new();
    s.push('|');
    for h in headers {
        s.push(' ');
        s.push_str(&org_escape(h));
        s.push_str(" |");
    }
    s.push('\n');
    // `|---+---+---|` hline.
    let segs: Vec<&str> = (0..cols).map(|_| "---").collect();
    s.push_str(&format!("|{}|\n", segs.join("+")));
    for row in rows {
        s.push('|');
        for i in 0..cols {
            let c = row.get(i).map(|x| x.as_str()).unwrap_or("");
            s.push(' ');
            s.push_str(&org_escape(c));
            s.push_str(" |");
        }
        s.push('\n');
    }
    s
}
