//! Multi-format rendering of a data-table view.
//!
//! The query/aggregation layer builds a neutral [`TableReport`] view-model; the
//! seven per-format renderers ([`DataReportFormat`]) are pure functions over it
//! — the same separation-of-concerns as [`crate::render`] (which renders a
//! `QualityReport`). We deliberately do NOT extend `crate::render::ReportFormat`
//! (CSV is meaningless for a quality report); instead we reuse only its `pub`
//! primitives — [`crate::render::glyphs`] and [`crate::render::sparkline`] — so
//! the box-drawing / sparkline style stays consistent across the tree.
//!
//! Unicode glyph policy (box-drawing / geometric / block; never emoji) applies;
//! the LaTeX backend substitutes LaTeX commands where one exists.

// Mirrors `crate::render`'s module attribute: some view-model helpers are
// exercised by only a subset of the seven renderers / by the tool layer, so a
// blanket allow keeps `-D warnings` green without per-item annotations.
#![allow(dead_code)]

mod csv;
mod html;
mod json;
mod latex;
mod markdown;
mod org;
mod text;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::datatable::column_type::ColumnType;

/// The seven output renditions. `parse` accepts the same aliases as
/// `crate::render::ReportFormat` plus `csv`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataReportFormat {
    Markdown,
    Org,
    Latex,
    Html,
    Text,
    Json,
    Csv,
}

impl DataReportFormat {
    /// Parse a `format` param value; `None` for unrecognized input so the caller
    /// errors cleanly instead of silently defaulting.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "markdown" | "md" | "gfm" => Some(Self::Markdown),
            "org" | "orgmode" | "org-mode" => Some(Self::Org),
            "latex" | "tex" => Some(Self::Latex),
            "html" => Some(Self::Html),
            "text" | "txt" | "plain" => Some(Self::Text),
            "json" => Some(Self::Json),
            "csv" => Some(Self::Csv),
            _ => None,
        }
    }

    /// Pipe-delimited valid values, for error messages.
    pub fn valid_values() -> &'static str {
        "markdown|org|latex|html|text|json|csv"
    }
}

/// Cell value type — drives column alignment and per-format value formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CellType {
    Text,
    Integer,
    Number,
    Boolean,
    Timestamp,
    Json,
}

impl CellType {
    /// Map a declared schema column type onto a cell type.
    pub fn from_column(t: ColumnType) -> Self {
        match t {
            ColumnType::Text => Self::Text,
            ColumnType::Integer => Self::Integer,
            ColumnType::Number => Self::Number,
            ColumnType::Boolean => Self::Boolean,
            ColumnType::Timestamp => Self::Timestamp,
            ColumnType::Json => Self::Json,
        }
    }

    /// Infer a cell type from a JSON value (open / schemaless tables, where the
    /// projection has no declared types). Strings are left as `Text` rather than
    /// guessed as timestamps to avoid surprising reformatting.
    pub fn infer(v: &Value) -> Self {
        match v {
            Value::Bool(_) => Self::Boolean,
            Value::Number(n) => {
                if n.is_i64() || n.is_u64() {
                    Self::Integer
                } else {
                    Self::Number
                }
            }
            Value::String(_) => Self::Text,
            Value::Null => Self::Text,
            _ => Self::Json,
        }
    }

    /// Default column alignment for this type.
    pub fn align(self) -> Align {
        match self {
            Self::Integer | Self::Number => Align::Right,
            Self::Boolean => Align::Center,
            _ => Align::Left,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Timestamp => "timestamp",
            Self::Json => "json",
        }
    }
}

/// Cell / column horizontal alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

/// A projected column: a name + its cell type (alignment derives from the type).
#[derive(Debug, Clone, Serialize)]
pub struct ColumnView {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: CellType,
}

impl ColumnView {
    pub fn new(name: impl Into<String>, ty: CellType) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }

    pub fn align(&self) -> Align {
        self.ty.align()
    }
}

/// One rendered-ready cell: its type (for formatting) + the raw JSON value
/// (`Null` when the field was absent on that row).
#[derive(Debug, Clone)]
pub struct Cell {
    pub ty: CellType,
    pub raw: Value,
}

// The aggregation result types live in `crate::datatable::aggregate` (their
// natural home — that module computes them); re-exported here so the renderers
// (`use super::*`) and external `report::AggResult` references resolve.
pub use crate::datatable::aggregate::AggResult;

/// The neutral view-model the renderers consume. Built once by the tool layer;
/// each renderer is a pure `fn(&TableReport) -> String`.
#[derive(Debug, Clone)]
pub struct TableReport {
    pub title: String,
    pub columns: Vec<ColumnView>,
    pub rows: Vec<Vec<Cell>>,
    pub summary: Option<AggResult>,
    pub caption: Option<String>,
    pub generated_at: DateTime<Utc>,
    /// Total rows matching the filter (may exceed `rows.len()` when limited).
    pub total_rows: i64,
    /// Whether `rows` is a truncated subset of `total_rows`.
    pub truncated: bool,
}

impl TableReport {
    /// Per-column alignment vector (parallel to `columns`).
    pub fn aligns(&self) -> Vec<Align> {
        self.columns.iter().map(|c| c.ty.align()).collect()
    }
}

/// Project raw JSONB row objects onto `columns`, yielding the cell grid. Absent
/// fields become `Null` cells (rendered as `—`).
pub fn cells_from_rows(columns: &[ColumnView], data: &[Value]) -> Vec<Vec<Cell>> {
    data.iter()
        .map(|row| {
            columns
                .iter()
                .map(|c| Cell {
                    ty: c.ty,
                    raw: row.get(&c.name).cloned().unwrap_or(Value::Null),
                })
                .collect()
        })
        .collect()
}

/// Render `report` in `fmt`.
pub fn render(report: &TableReport, fmt: DataReportFormat) -> String {
    match fmt {
        DataReportFormat::Markdown => markdown::render(report),
        DataReportFormat::Org => org::render(report),
        DataReportFormat::Latex => latex::render(report),
        DataReportFormat::Html => html::render(report),
        DataReportFormat::Text => text::render(report),
        DataReportFormat::Json => json::render(report),
        DataReportFormat::Csv => csv::render(report),
    }
}

// ── Shared cell / value formatting ───────────────────────────────────────────

/// Human-readable null placeholder (box-drawing / markdown / org / html).
pub(crate) const NULL_GLYPH: &str = "—";

/// Format a JSON number without a trailing `.0` / trailing zeros. Integers keep
/// full precision; floats are printed at up to 6 fractional digits, trimmed.
pub(crate) fn fmt_number(v: &Value) -> String {
    if let Some(i) = v.as_i64() {
        return i.to_string();
    }
    if let Some(u) = v.as_u64() {
        return u.to_string();
    }
    if let Some(f) = v.as_f64()
        && f.is_finite()
    {
        let s = format!("{f:.6}");
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        return trimmed.to_string();
    }
    v.to_string()
}

/// Format a timestamp value (RFC3339 string or epoch-seconds number) as
/// `YYYY-MM-DD HH:MM` (UTC). Falls back to the raw rendering on parse failure.
pub(crate) fn fmt_timestamp(v: &Value) -> String {
    match v {
        Value::String(s) => match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(dt) => dt.with_timezone(&Utc).format("%Y-%m-%d %H:%M").to_string(),
            Err(_) => s.clone(),
        },
        Value::Number(n) => match n.as_i64() {
            Some(secs) => DateTime::<Utc>::from_timestamp(secs, 0)
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| n.to_string()),
            None => n.to_string(),
        },
        _ => String::new(),
    }
}

/// Compact JSON serialization of a value (for `Json` cells).
pub(crate) fn compact_json(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
}

/// Generic human cell text used by the text / markdown / org / html backends
/// (booleans as `✓`/`✗`, null as `—`). LaTeX and CSV format booleans/nulls
/// differently and have their own cell formatters.
pub(crate) fn cell_human(cell: &Cell) -> String {
    if cell.raw.is_null() {
        return NULL_GLYPH.to_string();
    }
    match cell.ty {
        CellType::Boolean => match cell.raw.as_bool() {
            Some(true) => "✓".to_string(),
            Some(false) => "✗".to_string(),
            None => cell.raw.to_string(),
        },
        CellType::Integer | CellType::Number => fmt_number(&cell.raw),
        CellType::Timestamp => fmt_timestamp(&cell.raw),
        CellType::Text => cell
            .raw
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| cell.raw.to_string()),
        CellType::Json => compact_json(&cell.raw),
    }
}

/// Format a scalar metric / group-key value (numbers trimmed, strings raw,
/// null → `—`). Used for the aggregation summary tables.
pub(crate) fn value_scalar(v: &Value) -> String {
    match v {
        Value::Null => NULL_GLYPH.to_string(),
        Value::Number(_) => fmt_number(v),
        Value::String(s) => s.clone(),
        Value::Bool(b) => {
            if *b {
                "✓".to_string()
            } else {
                "✗".to_string()
            }
        }
        _ => compact_json(v),
    }
}

/// The headers, alignments, and string rows of an aggregation summary table:
/// the group-by fields (left) followed by every metric alias (right), one row
/// per group.
pub(crate) fn agg_table(agg: &AggResult) -> (Vec<String>, Vec<Align>, Vec<Vec<String>>) {
    let mut metric_keys: Vec<String> = Vec::new();
    for g in &agg.groups {
        for k in g.metrics.keys() {
            if !metric_keys.contains(k) {
                metric_keys.push(k.clone());
            }
        }
    }
    let mut headers = agg.group_by.clone();
    headers.extend(metric_keys.iter().cloned());
    let mut aligns: Vec<Align> = agg.group_by.iter().map(|_| Align::Left).collect();
    aligns.extend(metric_keys.iter().map(|_| Align::Right));
    let rows: Vec<Vec<String>> = agg
        .groups
        .iter()
        .map(|g| {
            let mut row: Vec<String> = agg
                .group_by
                .iter()
                .map(|k| g.group.get(k).map(value_scalar).unwrap_or_default())
                .collect();
            row.extend(metric_keys.iter().map(|k| {
                g.metrics
                    .get(k)
                    .map(value_scalar)
                    .unwrap_or_else(|| NULL_GLYPH.to_string())
            }));
            row
        })
        .collect();
    (headers, aligns, rows)
}

/// `(min, max)` over a finite slice (defaults to `(0,1)` when empty/degenerate).
pub(crate) fn min_max(values: &[f64]) -> (f64, f64) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for &v in values {
        if v.is_finite() {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    if !lo.is_finite() || !hi.is_finite() {
        (0.0, 1.0)
    } else {
        (lo, hi)
    }
}

/// The first numeric column's finite values, in row order — the source for the
/// optional trend sparkline. `None` when no numeric column or no finite values.
pub(crate) fn numeric_series(report: &TableReport) -> Option<(String, Vec<f64>)> {
    let (col_idx, col) = report
        .columns
        .iter()
        .enumerate()
        .find(|(_, c)| matches!(c.ty, CellType::Integer | CellType::Number))?;
    let values: Vec<f64> = report
        .rows
        .iter()
        .filter_map(|row| row.get(col_idx).and_then(|c| c.raw.as_f64()))
        .filter(|f| f.is_finite())
        .collect();
    if values.is_empty() {
        None
    } else {
        Some((col.name.clone(), values))
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    use super::*;
    use crate::datatable::aggregate::AggGroup;
    use serde_json::json;

    /// A small observations report used by the renderer snapshot tests.
    pub fn sample() -> TableReport {
        let columns = vec![
            ColumnView::new("ts", CellType::Timestamp),
            ColumnView::new("metric", CellType::Text),
            ColumnView::new("value", CellType::Number),
            ColumnView::new("ok", CellType::Boolean),
            ColumnView::new("note", CellType::Text),
        ];
        let data = vec![
            json!({"ts":"2026-05-20T14:03:00Z","metric":"latency_ms","value":12.4,"ok":true,"note":"warm"}),
            json!({"ts":"2026-05-21T14:03:00Z","metric":"latency_ms","value":11.8,"ok":true,"note":"warm"}),
            json!({"ts":"2026-05-22T14:03:00Z","metric":"latency_ms","value":19.2,"ok":false,"note":"<cold> & start"}),
            json!({"ts":"2026-05-22T14:05:00Z","metric":"throughput","value":4200.0,"ok":true}),
        ];
        let rows = cells_from_rows(&columns, &data);
        let mut g1_metrics = Map::new();
        g1_metrics.insert("count".into(), json!(3));
        g1_metrics.insert("avg_value".into(), json!(14.466667));
        let mut g2_metrics = Map::new();
        g2_metrics.insert("count".into(), json!(1));
        g2_metrics.insert("avg_value".into(), json!(4200.0));
        let mut k1 = Map::new();
        k1.insert("metric".into(), json!("latency_ms"));
        let mut k2 = Map::new();
        k2.insert("metric".into(), json!("throughput"));
        let summary = AggResult {
            group_by: vec!["metric".into()],
            total_rows: 4,
            groups: vec![
                AggGroup {
                    group: k1,
                    metrics: g1_metrics,
                    n_ignored: Map::new(),
                },
                AggGroup {
                    group: k2,
                    metrics: g2_metrics,
                    n_ignored: Map::new(),
                },
            ],
        };
        TableReport {
            title: "bench_observations".into(),
            columns,
            rows,
            summary: Some(summary),
            caption: Some("nightly benchmark observations".into()),
            generated_at: DateTime::<Utc>::from_timestamp(1_716_991_500, 0).expect("fixed ts"),
            total_rows: 4,
            truncated: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_aliases_and_csv_rejects_garbage() {
        assert_eq!(
            DataReportFormat::parse("MD"),
            Some(DataReportFormat::Markdown)
        );
        assert_eq!(
            DataReportFormat::parse("org-mode"),
            Some(DataReportFormat::Org)
        );
        assert_eq!(
            DataReportFormat::parse("tex"),
            Some(DataReportFormat::Latex)
        );
        assert_eq!(
            DataReportFormat::parse("HTML"),
            Some(DataReportFormat::Html)
        );
        assert_eq!(
            DataReportFormat::parse("plain"),
            Some(DataReportFormat::Text)
        );
        assert_eq!(DataReportFormat::parse("CSV"), Some(DataReportFormat::Csv));
        assert_eq!(DataReportFormat::parse("pdf"), None);
    }

    #[test]
    fn every_format_renders_nonempty_with_title() {
        let r = fixture::sample();
        for fmt in [
            DataReportFormat::Markdown,
            DataReportFormat::Org,
            DataReportFormat::Latex,
            DataReportFormat::Html,
            DataReportFormat::Text,
            DataReportFormat::Json,
            DataReportFormat::Csv,
        ] {
            let s = render(&r, fmt);
            assert!(!s.trim().is_empty(), "{fmt:?} produced empty output");
            // The LaTeX backend escapes `_` → `\_`; every other format keeps it.
            let needle = if fmt == DataReportFormat::Latex {
                "bench\\_observations"
            } else {
                "bench_observations"
            };
            assert!(s.contains(needle), "{fmt:?} missing table title");
        }
    }

    #[test]
    fn format_specific_invariants() {
        let r = fixture::sample();

        // LaTeX: a complete, pdflatex-compilable document.
        let tex = render(&r, DataReportFormat::Latex);
        assert!(tex.contains("\\documentclass{article}"));
        assert!(tex.contains("\\begin{document}") && tex.contains("\\end{document}"));

        // HTML: the `<cold> & start` note must be escaped, never raw markup.
        let html = render(&r, DataReportFormat::Html);
        assert!(
            html.contains("&lt;cold&gt; &amp; start"),
            "html must escape angle brackets + ampersand"
        );
        assert!(!html.contains("<cold>"), "html must not emit raw markup");

        // Plain text: box-drawing borders from `crate::render::glyphs`.
        let text = render(&r, DataReportFormat::Text);
        assert!(text.contains('┌') && text.contains('│'));

        // CSV: leading `#` provenance comment, then a header row with the columns.
        let csv = render(&r, DataReportFormat::Csv);
        let mut lines = csv.lines();
        assert!(lines.next().unwrap().starts_with('#'));
        assert!(lines.next().unwrap().contains("metric"));

        // JSON: valid + structured (title, rows array, summary groups).
        let js = render(&r, DataReportFormat::Json);
        let v: serde_json::Value = serde_json::from_str(&js).expect("valid JSON");
        assert_eq!(v["title"], "bench_observations");
        assert!(v["rows"].is_array());
        assert!(v["summary"]["groups"].is_array());

        // Markdown: numeric columns get a right-alignment marker.
        let md = render(&r, DataReportFormat::Markdown);
        assert!(md.contains("---:"), "numeric columns right-aligned");
    }

    #[test]
    fn number_and_timestamp_formatting() {
        assert_eq!(fmt_number(&serde_json::json!(12.0)), "12");
        assert_eq!(fmt_number(&serde_json::json!(12.40)), "12.4");
        assert_eq!(fmt_number(&serde_json::json!(4200)), "4200");
        assert_eq!(
            fmt_timestamp(&serde_json::json!("2026-05-20T14:03:00Z")),
            "2026-05-20 14:03"
        );
    }

    #[test]
    fn numeric_series_picks_first_numeric_column() {
        let r = fixture::sample();
        let (col, vals) = numeric_series(&r).expect("has a numeric column");
        assert_eq!(col, "value");
        assert_eq!(vals.len(), 4);
    }
}
