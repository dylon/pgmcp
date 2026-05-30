//! Structured JSON rendition — the machine-readable rendition. Rows are emitted
//! as objects keyed by column name carrying the *raw* JSON values (no display
//! formatting), alongside the column schema and the optional aggregation
//! summary. This is the shape automated tooling consumes.

use super::*;
use serde_json::json;

pub fn render(r: &TableReport) -> String {
    let cols: Vec<Value> = r
        .columns
        .iter()
        .map(|c| json!({ "name": c.name, "type": c.ty.as_str() }))
        .collect();

    let rows: Vec<Value> = r
        .rows
        .iter()
        .map(|row| {
            let mut obj = Map::new();
            for (i, cell) in row.iter().enumerate() {
                if let Some(c) = r.columns.get(i) {
                    obj.insert(c.name.clone(), cell.raw.clone());
                }
            }
            Value::Object(obj)
        })
        .collect();

    let summary = r
        .summary
        .as_ref()
        .and_then(|a| serde_json::to_value(a).ok())
        .unwrap_or(Value::Null);
    let caption = match &r.caption {
        Some(c) => json!(c),
        None => Value::Null,
    };

    let doc = json!({
        "title": r.title,
        "generated_at": r.generated_at.to_rfc3339(),
        "total_rows": r.total_rows,
        "truncated": r.truncated,
        "columns": cols,
        "rows": rows,
        "summary": summary,
        "caption": caption,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}
