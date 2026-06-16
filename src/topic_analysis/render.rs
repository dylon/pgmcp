//! Multi-format rendering for the `topic_analysis` reports. `crate::render`'s
//! `render()` is hard-typed to `QualityReport`, so we reuse only its
//! [`ReportFormat`](crate::render::ReportFormat) enum and `glyphs` constants and
//! render through a small format-agnostic [`View`] model: each report provides
//! [`Renderable::to_view`], and the six walkers below turn any view into
//! Markdown / Org / LaTeX / HTML / Text / JSON. Unicode policy follows the
//! house style (geometric/box-drawing glyphs, never emoji; LaTeX substitutes
//! commands where one exists).

use crate::render::ReportFormat;
use crate::render::glyphs;

/// A format-agnostic report view: a title, a flat key→value summary, and ordered
/// sections.
pub struct View {
    pub title: String,
    pub summary: Vec<(String, String)>,
    pub sections: Vec<Section>,
}

pub struct Section {
    pub heading: String,
    pub body: Body,
}

/// The renderable shapes a section body can take.
pub enum Body {
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Bullets(Vec<String>),
    KeyVals(Vec<(String, String)>),
    Note(String),
}

/// A report that can render to JSON (via `Serialize`) or any text format (via a
/// [`View`]).
pub trait Renderable: serde::Serialize {
    fn to_view(&self) -> View;
}

/// Parse an optional `format` param: absent → JSON (stable for tooling/tests),
/// present-but-unknown → `Err` (caller maps to `invalid_params`, never silently
/// defaults).
pub fn parse_format(s: Option<&str>) -> Result<ReportFormat, String> {
    match s {
        None => Ok(ReportFormat::Json),
        Some(v) => ReportFormat::parse(v)
            .ok_or_else(|| format!("unknown format '{v}' (use json|markdown|org|latex|html|text)")),
    }
}

/// Render `report` in `fmt`. JSON serializes the report struct directly; every
/// other format walks `report.to_view()`.
pub fn render<R: Renderable>(report: &R, fmt: ReportFormat) -> String {
    match fmt {
        ReportFormat::Json => serde_json::to_string_pretty(report)
            .unwrap_or_else(|e| format!("{{\"error\":\"serialize failed: {e}\"}}")),
        ReportFormat::Markdown => markdown(&report.to_view()),
        ReportFormat::Org => org(&report.to_view()),
        ReportFormat::Latex => latex(&report.to_view()),
        ReportFormat::Html => html(&report.to_view()),
        ReportFormat::Text => text(&report.to_view()),
    }
}

/// A unicode block-ramp sparkline over `values`, scaled to their own min/max
/// (reuses [`glyphs::SPARK`]). Empty / flat input → a flat baseline.
pub fn spark(values: &[f64]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let lo = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let span = hi - lo;
    values
        .iter()
        .map(|&v| {
            let idx = if span <= f64::EPSILON {
                0
            } else {
                (((v - lo) / span) * (glyphs::SPARK.len() - 1) as f64).round() as usize
            };
            glyphs::SPARK[idx.min(glyphs::SPARK.len() - 1)]
        })
        .collect()
}

// ── Markdown ────────────────────────────────────────────────────────────────
fn markdown(v: &View) -> String {
    let mut s = format!("# {}\n\n", v.title);
    for (k, val) in &v.summary {
        s.push_str(&format!("- **{k}:** {val}\n"));
    }
    if !v.summary.is_empty() {
        s.push('\n');
    }
    for sec in &v.sections {
        s.push_str(&format!("## {}\n\n", sec.heading));
        match &sec.body {
            Body::Table { headers, rows } => {
                s.push_str(&format!("| {} |\n", headers.join(" | ")));
                s.push_str(&format!(
                    "| {} |\n",
                    headers
                        .iter()
                        .map(|_| "---")
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
                for r in rows {
                    let cells: Vec<String> = r.iter().map(|c| c.replace('|', "\\|")).collect();
                    s.push_str(&format!("| {} |\n", cells.join(" | ")));
                }
            }
            Body::Bullets(items) => {
                for it in items {
                    s.push_str(&format!("- {it}\n"));
                }
            }
            Body::KeyVals(kv) => {
                for (k, val) in kv {
                    s.push_str(&format!("- **{k}:** {val}\n"));
                }
            }
            Body::Note(n) => s.push_str(&format!("{n}\n")),
        }
        s.push('\n');
    }
    s
}

// ── Org ─────────────────────────────────────────────────────────────────────
fn org(v: &View) -> String {
    let mut s = format!("* {}\n", v.title);
    for (k, val) in &v.summary {
        s.push_str(&format!("- *{k}:* {val}\n"));
    }
    for sec in &v.sections {
        s.push_str(&format!("** {}\n", sec.heading));
        match &sec.body {
            Body::Table { headers, rows } => {
                s.push_str(&format!("| {} |\n", headers.join(" | ")));
                s.push_str("|-\n");
                for r in rows {
                    s.push_str(&format!("| {} |\n", r.join(" | ")));
                }
            }
            Body::Bullets(items) => {
                for it in items {
                    s.push_str(&format!("- {it}\n"));
                }
            }
            Body::KeyVals(kv) => {
                for (k, val) in kv {
                    s.push_str(&format!("- *{k}:* {val}\n"));
                }
            }
            Body::Note(n) => s.push_str(&format!("{n}\n")),
        }
    }
    s
}

// ── LaTeX ───────────────────────────────────────────────────────────────────
fn tex_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' | '%' | '$' | '#' | '_' | '{' | '}' => {
                o.push('\\');
                o.push(c);
            }
            '~' => o.push_str("\\textasciitilde{}"),
            '^' => o.push_str("\\textasciicircum{}"),
            '\\' => o.push_str("\\textbackslash{}"),
            _ => o.push(c),
        }
    }
    o
}

fn latex(v: &View) -> String {
    let mut s = format!("\\section*{{{}}}\n", tex_escape(&v.title));
    if !v.summary.is_empty() {
        s.push_str("\\begin{itemize}\n");
        for (k, val) in &v.summary {
            s.push_str(&format!(
                "  \\item \\textbf{{{}:}} {}\n",
                tex_escape(k),
                tex_escape(val)
            ));
        }
        s.push_str("\\end{itemize}\n");
    }
    for sec in &v.sections {
        s.push_str(&format!("\\subsection*{{{}}}\n", tex_escape(&sec.heading)));
        match &sec.body {
            Body::Table { headers, rows } => {
                let cols = "l".repeat(headers.len().max(1));
                s.push_str(&format!("\\begin{{tabular}}{{{cols}}}\n\\hline\n"));
                s.push_str(&format!(
                    "{} \\\\\n\\hline\n",
                    headers
                        .iter()
                        .map(|h| tex_escape(h))
                        .collect::<Vec<_>>()
                        .join(" & ")
                ));
                for r in rows {
                    s.push_str(&format!(
                        "{} \\\\\n",
                        r.iter()
                            .map(|c| tex_escape(c))
                            .collect::<Vec<_>>()
                            .join(" & ")
                    ));
                }
                s.push_str("\\hline\n\\end{tabular}\n");
            }
            Body::Bullets(items) => {
                s.push_str("\\begin{itemize}\n");
                for it in items {
                    s.push_str(&format!("  \\item {}\n", tex_escape(it)));
                }
                s.push_str("\\end{itemize}\n");
            }
            Body::KeyVals(kv) => {
                s.push_str("\\begin{itemize}\n");
                for (k, val) in kv {
                    s.push_str(&format!(
                        "  \\item \\textbf{{{}:}} {}\n",
                        tex_escape(k),
                        tex_escape(val)
                    ));
                }
                s.push_str("\\end{itemize}\n");
            }
            Body::Note(n) => s.push_str(&format!("{}\n", tex_escape(n))),
        }
    }
    s
}

// ── HTML ────────────────────────────────────────────────────────────────────
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn html(v: &View) -> String {
    let mut s = format!("<h1>{}</h1>\n", html_escape(&v.title));
    if !v.summary.is_empty() {
        s.push_str("<ul>\n");
        for (k, val) in &v.summary {
            s.push_str(&format!(
                "  <li><strong>{}:</strong> {}</li>\n",
                html_escape(k),
                html_escape(val)
            ));
        }
        s.push_str("</ul>\n");
    }
    for sec in &v.sections {
        s.push_str(&format!("<h2>{}</h2>\n", html_escape(&sec.heading)));
        match &sec.body {
            Body::Table { headers, rows } => {
                s.push_str("<table>\n  <thead><tr>");
                for h in headers {
                    s.push_str(&format!("<th>{}</th>", html_escape(h)));
                }
                s.push_str("</tr></thead>\n  <tbody>\n");
                for r in rows {
                    s.push_str("    <tr>");
                    for c in r {
                        s.push_str(&format!("<td>{}</td>", html_escape(c)));
                    }
                    s.push_str("</tr>\n");
                }
                s.push_str("  </tbody>\n</table>\n");
            }
            Body::Bullets(items) => {
                s.push_str("<ul>\n");
                for it in items {
                    s.push_str(&format!("  <li>{}</li>\n", html_escape(it)));
                }
                s.push_str("</ul>\n");
            }
            Body::KeyVals(kv) => {
                s.push_str("<ul>\n");
                for (k, val) in kv {
                    s.push_str(&format!(
                        "  <li><strong>{}:</strong> {}</li>\n",
                        html_escape(k),
                        html_escape(val)
                    ));
                }
                s.push_str("</ul>\n");
            }
            Body::Note(n) => s.push_str(&format!("<p>{}</p>\n", html_escape(n))),
        }
    }
    s
}

// ── Plain text (box-drawing tables) ──────────────────────────────────────────
fn text(v: &View) -> String {
    let mut s = format!(
        "{}\n{}\n\n",
        v.title,
        glyphs::DOUBLE_H.to_string().repeat(v.title.len())
    );
    for (k, val) in &v.summary {
        s.push_str(&format!("  {k}: {val}\n"));
    }
    if !v.summary.is_empty() {
        s.push('\n');
    }
    for sec in &v.sections {
        s.push_str(&format!(
            "{}\n{}\n",
            sec.heading,
            glyphs::H.to_string().repeat(sec.heading.len())
        ));
        match &sec.body {
            Body::Table { headers, rows } => s.push_str(&text_table(headers, rows)),
            Body::Bullets(items) => {
                for it in items {
                    s.push_str(&format!("  • {it}\n"));
                }
            }
            Body::KeyVals(kv) => {
                for (k, val) in kv {
                    s.push_str(&format!("  {k}: {val}\n"));
                }
            }
            Body::Note(n) => s.push_str(&format!("{n}\n")),
        }
        s.push('\n');
    }
    s
}

fn text_table(headers: &[String], rows: &[Vec<String>]) -> String {
    let ncol = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            if i < ncol {
                widths[i] = widths[i].max(c.chars().count());
            }
        }
    }
    let sep = |left: char, mid: char, right: char| -> String {
        let mut line = String::new();
        line.push(left);
        for (i, w) in widths.iter().enumerate() {
            line.push_str(&glyphs::H.to_string().repeat(w + 2));
            line.push(if i + 1 == ncol { right } else { mid });
        }
        line.push('\n');
        line
    };
    let row_line = |cells: &[String]| -> String {
        let mut line = String::new();
        line.push(glyphs::V);
        for (i, w) in widths.iter().enumerate() {
            let c = cells.get(i).map(String::as_str).unwrap_or("");
            line.push_str(&format!(" {:<width$} ", c, width = w));
            line.push(glyphs::V);
        }
        line.push('\n');
        line
    };
    let mut out = sep(glyphs::TL, glyphs::T_DOWN, glyphs::TR);
    out.push_str(&row_line(headers));
    out.push_str(&sep(glyphs::T_RIGHT, glyphs::CROSS, glyphs::T_LEFT));
    for r in rows {
        out.push_str(&row_line(r));
    }
    out.push_str(&sep(glyphs::BL, glyphs::T_UP, glyphs::BR));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize)]
    struct Demo {
        n: i32,
    }
    impl Renderable for Demo {
        fn to_view(&self) -> View {
            View {
                title: "Demo".into(),
                summary: vec![("n".into(), self.n.to_string())],
                sections: vec![Section {
                    heading: "Rows".into(),
                    body: Body::Table {
                        headers: vec!["a".into(), "b".into()],
                        rows: vec![vec!["1".into(), "x|y".into()]],
                    },
                }],
            }
        }
    }

    #[test]
    fn all_formats_render_without_panic_and_escape() {
        let d = Demo { n: 7 };
        assert!(render(&d, ReportFormat::Json).contains("\"n\": 7"));
        assert!(render(&d, ReportFormat::Markdown).contains("# Demo"));
        // Markdown escapes pipe in a cell.
        assert!(render(&d, ReportFormat::Markdown).contains("x\\|y"));
        assert!(render(&d, ReportFormat::Org).starts_with("* Demo"));
        assert!(render(&d, ReportFormat::Latex).contains("\\section*{Demo}"));
        assert!(render(&d, ReportFormat::Html).contains("<h1>Demo</h1>"));
        assert!(render(&d, ReportFormat::Text).contains("Demo"));
    }

    #[test]
    fn spark_handles_flat_and_varying() {
        assert_eq!(spark(&[]), "");
        assert_eq!(spark(&[5.0, 5.0, 5.0]).chars().count(), 3);
        let s = spark(&[0.0, 1.0]);
        assert_eq!(s.chars().next(), Some(glyphs::SPARK[0]));
        assert_eq!(s.chars().last(), Some(glyphs::SPARK[7]));
    }
}
