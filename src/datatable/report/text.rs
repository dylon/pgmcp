//! Plain-text renderer — monospace, box-drawing borders, `═` rules, and a
//! unicode block sparkline for the trend. Reuses `crate::render::{glyphs,
//! sparkline}` so the style matches the quality report.

use super::*;
use crate::render::{glyphs, sparkline};

pub fn render(r: &TableReport) -> String {
    let mut s = String::new();

    let title = format!("DATA TABLE: {}", r.title);
    s.push_str(&title);
    s.push('\n');
    s.push_str(
        &glyphs::DOUBLE_H
            .to_string()
            .repeat(title.chars().count().max(40)),
    );
    s.push('\n');
    let showing = if r.truncated {
        format!(" (showing {})", r.rows.len())
    } else {
        String::new()
    };
    s.push_str(&format!(
        "generated {}   rows {}{}\n\n",
        r.generated_at.format("%Y-%m-%d %H:%M UTC"),
        r.total_rows,
        showing,
    ));

    // Trend sparkline over the first numeric column.
    if let Some((col, vals)) = numeric_series(r) {
        let (lo, hi) = min_max(&vals);
        s.push_str(&format!("TREND {col}: {}\n\n", sparkline(&vals, lo, hi)));
    }

    // Summary (aggregation) section.
    if let Some(agg) = &r.summary {
        s.push_str("SUMMARY\n");
        let (headers, aligns, rows) = agg_table(agg);
        s.push_str(&box_table(&headers, &rows, &aligns));
        for g in &agg.groups {
            for (field, n) in &g.n_ignored {
                if n.as_i64().unwrap_or(0) > 0 {
                    s.push_str(&format!(
                        "  note: skipped {n} non-numeric value(s) in {field}\n"
                    ));
                }
            }
        }
        s.push('\n');
    }

    // Detail table.
    s.push_str("DETAIL\n");
    let headers: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
    let rows: Vec<Vec<String>> = r
        .rows
        .iter()
        .map(|row| row.iter().map(cell_human).collect())
        .collect();
    s.push_str(&box_table(&headers, &rows, &r.aligns()));

    if let Some(cap) = &r.caption {
        s.push_str(&format!("\n{cap}\n"));
    }
    s
}

/// Pad `cell` to `width` per `align`.
fn pad(cell: &str, width: usize, align: Align) -> String {
    let len = cell.chars().count();
    let total = width.saturating_sub(len);
    match align {
        Align::Left => format!("{cell}{}", " ".repeat(total)),
        Align::Right => format!("{}{cell}", " ".repeat(total)),
        Align::Center => {
            let l = total / 2;
            let r = total - l;
            format!("{}{cell}{}", " ".repeat(l), " ".repeat(r))
        }
    }
}

/// Box-drawing table sized to content, with per-column alignment. Single-line
/// cells only (long text widens the column).
fn box_table(headers: &[String], rows: &[Vec<String>], aligns: &[Align]) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let align_of = |i: usize| aligns.get(i).copied().unwrap_or(Align::Left);
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
            line.push(' ');
            line.push_str(&pad(cell, *w, align_of(i)));
            line.push(' ');
            line.push(glyphs::V);
        }
        line.push('\n');
        line
    };
    let mut out = String::new();
    out.push_str(&border(glyphs::TL, glyphs::T_DOWN, glyphs::TR));
    out.push_str(&row_line(headers));
    out.push_str(&border(glyphs::T_RIGHT, glyphs::CROSS, glyphs::T_LEFT));
    for row in rows {
        out.push_str(&row_line(row));
    }
    out.push_str(&border(glyphs::BL, glyphs::T_UP, glyphs::BR));
    out
}
