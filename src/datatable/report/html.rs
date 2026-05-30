//! Self-contained HTML renderer — one document, inline `<style>`, no external
//! assets. Per-column `text-align`; the trend is an inline SVG sparkline (HTML
//! always gets the SVG, never the unicode fallback). All free text is escaped.

use super::*;

pub fn render(r: &TableReport) -> String {
    let mut s = String::new();
    s.push_str("<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n");
    s.push_str(&format!("<title>Data Table: {}</title>\n", esc(&r.title)));
    s.push_str(STYLE);
    s.push_str("</head>\n<body>\n");

    s.push_str(&format!("<h1>Data Table: {}</h1>\n", esc(&r.title)));
    let count = if r.truncated {
        format!("showing {} of {} rows", r.rows.len(), r.total_rows)
    } else {
        format!("{} rows", r.total_rows)
    };
    s.push_str(&format!(
        "<p class=\"meta\"><em>generated {} · {}</em></p>\n",
        esc(&r.generated_at.format("%Y-%m-%d %H:%M UTC").to_string()),
        esc(&count),
    ));

    if let Some((col, vals)) = numeric_series(r) {
        let (lo, hi) = min_max(&vals);
        s.push_str(&format!(
            "<p class=\"meta\"><strong>Trend ({}):</strong> {}</p>\n",
            esc(&col),
            svg_sparkline(&vals, lo, hi),
        ));
    }

    if let Some(agg) = &r.summary {
        s.push_str("<h2>Summary</h2>\n");
        let (headers, aligns, rows) = agg_table(agg);
        s.push_str(&html_table(&headers, &aligns, &rows));
    }

    s.push_str("<h2>Detail</h2>\n");
    let headers: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
    let rows: Vec<Vec<String>> = r
        .rows
        .iter()
        .map(|row| row.iter().map(cell_human).collect())
        .collect();
    s.push_str(&html_table(&headers, &r.aligns(), &rows));

    if let Some(cap) = &r.caption {
        s.push_str(&format!("<p class=\"footer\"><em>{}</em></p>\n", esc(cap)));
    }
    s.push_str("</body></html>\n");
    s
}

const STYLE: &str = r#"<style>
body{font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;max-width:960px;margin:2rem auto;padding:0 1rem;color:#1a1a1a;line-height:1.5}
h1{border-bottom:2px solid #ddd;padding-bottom:.3rem}
h2{margin-top:2rem;border-bottom:1px solid #eee;padding-bottom:.2rem}
table{border-collapse:collapse;width:100%;margin:.5rem 0}
th,td{border:1px solid #ddd;padding:.3rem .5rem;font-size:.92rem}
th{background:#f5f5f5}
.meta{color:#555}
.footer{color:#888;font-size:.85rem}
svg{vertical-align:middle}
</style>
"#;

fn align_css(a: Align) -> &'static str {
    match a {
        Align::Left => "left",
        Align::Right => "right",
        Align::Center => "center",
    }
}

fn html_table(headers: &[String], aligns: &[Align], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    let mut s = String::from("<table>\n<tr>");
    for (i, h) in headers.iter().enumerate() {
        let a = align_css(aligns.get(i).copied().unwrap_or(Align::Left));
        s.push_str(&format!("<th style=\"text-align:{a}\">{}</th>", esc(h)));
    }
    s.push_str("</tr>\n");
    for row in rows {
        s.push_str("<tr>");
        for i in 0..cols {
            let a = align_css(aligns.get(i).copied().unwrap_or(Align::Left));
            let c = row.get(i).map(|x| x.as_str()).unwrap_or("");
            s.push_str(&format!("<td style=\"text-align:{a}\">{}</td>", esc(c)));
        }
        s.push_str("</tr>\n");
    }
    s.push_str("</table>\n");
    s
}

/// Inline SVG sparkline (HTML's trend rendering).
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
