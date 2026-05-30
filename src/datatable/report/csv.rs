//! CSV renderer (RFC4180 quoting) — the data-export rendition: the detail rows
//! only, with a leading `#` provenance comment (CSV readers commonly skip `#`).
//! Values are emitted faithfully (raw), not display-formatted, so the data
//! round-trips.

use super::*;

pub fn render(r: &TableReport) -> String {
    let mut s = String::new();
    let showing = if r.truncated {
        format!(" (showing {})", r.rows.len())
    } else {
        String::new()
    };
    s.push_str(&format!(
        "# {} — generated {} — {} rows{}\n",
        r.title,
        r.generated_at.format("%Y-%m-%d %H:%M UTC"),
        r.total_rows,
        showing,
    ));

    let headers: Vec<String> = r.columns.iter().map(|c| csv_quote(&c.name)).collect();
    s.push_str(&headers.join(","));
    s.push('\n');

    for row in &r.rows {
        let cells: Vec<String> = row.iter().map(|c| csv_quote(&cell_csv(c))).collect();
        s.push_str(&cells.join(","));
        s.push('\n');
    }
    s
}

/// Faithful scalar rendering for CSV (raw values, machine-friendly).
fn cell_csv(cell: &Cell) -> String {
    match &cell.raw {
        Value::Null => String::new(),
        Value::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Value::Number(_) => fmt_number(&cell.raw),
        Value::String(s) => s.clone(),
        other => compact_json(other),
    }
}

/// RFC4180 quoting: wrap in double quotes (doubling internal quotes) when the
/// field contains a comma, quote, CR, or LF.
fn csv_quote(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}
