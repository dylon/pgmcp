//! Descriptive aggregation for data tables: the closed [`AggFunc`] vocabulary
//! and the pure [`compute_aggregation`] engine (`data_table_aggregate` and the
//! report summary section).
//!
//! Aggregation is computed **in Rust over the filtered rows** the query layer
//! loads (capped), not pushed into SQL. That keeps it correct and unit-testable
//! without a database, sidesteps Postgres `NUMERIC` decode plumbing, and makes
//! the type-aware `min`/`max` (numeric vs lexical vs temporal) and the
//! non-coercible-value accounting (`n_ignored`) straightforward. Median reuses
//! the vetted [`crate::stats::inference::median`].
//!
//! Like [`crate::datatable::filter`], the vocabulary is closed + golden-tested.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::datatable::column_type::ColumnType;

/// A descriptive aggregation function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggFunc {
    /// `count` of rows (no field) or of non-null field values (with a field).
    Count,
    Sum,
    Avg,
    Min,
    Max,
    /// Sample standard deviation (n−1; `0` for a single value).
    Stddev,
    Median,
    /// Count of distinct non-null field values.
    CountDistinct,
}

impl AggFunc {
    pub const ALL: &'static [AggFunc] = &[
        Self::Count,
        Self::Sum,
        Self::Avg,
        Self::Min,
        Self::Max,
        Self::Stddev,
        Self::Median,
        Self::CountDistinct,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Avg => "avg",
            Self::Min => "min",
            Self::Max => "max",
            Self::Stddev => "stddev",
            Self::Median => "median",
            Self::CountDistinct => "count_distinct",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.as_str() == s)
    }

    /// Whether the function needs a target field. Only `count` (of rows) does
    /// not.
    pub fn needs_field(self) -> bool {
        !matches!(self, Self::Count)
    }

    /// Whether the function requires a NUMERIC field (`sum`/`avg`/`stddev`/
    /// `median`). `count`/`count_distinct`/`min`/`max` work on any type.
    pub fn requires_numeric(self) -> bool {
        matches!(self, Self::Sum | Self::Avg | Self::Stddev | Self::Median)
    }

    /// Default output key for a `(func, field)` pair, e.g. `avg_value`.
    pub fn default_alias(self, field: Option<&str>) -> String {
        match field {
            Some(f) => format!("{}_{}", self.as_str(), f),
            None => self.as_str().to_string(),
        }
    }
}

/// One requested aggregation: function, optional target field, output key.
#[derive(Debug, Clone)]
pub struct MetricSpec {
    pub func: AggFunc,
    pub field: Option<String>,
    pub alias: String,
}

/// One group of an aggregation result: the group key(s), the computed metrics,
/// and per-field counts of values skipped as non-coercible (omitted when zero).
#[derive(Debug, Clone, Serialize)]
pub struct AggGroup {
    pub group: Map<String, Value>,
    pub metrics: Map<String, Value>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub n_ignored: Map<String, Value>,
}

/// The full aggregation result; also the optional summary section embedded in a
/// [`crate::datatable::report::TableReport`].
#[derive(Debug, Clone, Serialize)]
pub struct AggResult {
    pub group_by: Vec<String>,
    pub total_rows: i64,
    pub groups: Vec<AggGroup>,
}

/// Coerce a JSON value to a finite `f64` (a number, or a numeric string).
/// `None` if not coercible (the caller counts these toward `n_ignored`).
pub fn coerce_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// Pure descriptive aggregation over `rows` (each a JSON object). Groups by the
/// `group_by` fields (one overall group when empty) and computes each metric per
/// group. `types` provides declared column types for type-aware `min`/`max`.
pub fn compute_aggregation(
    rows: &[Value],
    group_by: &[String],
    metrics: &[MetricSpec],
    types: &HashMap<String, ColumnType>,
) -> AggResult {
    // First-seen group order, keyed by the serialized group-key tuple.
    let mut order: Vec<(String, Vec<Value>)> = Vec::new();
    let mut buckets: HashMap<String, Vec<&Value>> = HashMap::new();
    for row in rows {
        let key_vals: Vec<Value> = group_by
            .iter()
            .map(|g| row.get(g).cloned().unwrap_or(Value::Null))
            .collect();
        let key = serde_json::to_string(&key_vals).unwrap_or_default();
        if !buckets.contains_key(&key) {
            order.push((key.clone(), key_vals));
        }
        buckets.entry(key).or_default().push(row);
    }
    // With no grouping, always emit exactly one (possibly empty) group.
    if group_by.is_empty() && order.is_empty() {
        order.push(("[]".to_string(), Vec::new()));
        buckets.insert("[]".to_string(), Vec::new());
    }

    let groups = order
        .into_iter()
        .map(|(key, key_vals)| {
            let group_rows = buckets.get(&key).cloned().unwrap_or_default();
            let mut gmap = Map::new();
            for (i, g) in group_by.iter().enumerate() {
                gmap.insert(g.clone(), key_vals.get(i).cloned().unwrap_or(Value::Null));
            }
            let mut metrics_map = Map::new();
            let mut ignored_map = Map::new();
            for m in metrics {
                let (val, ignored) = compute_metric(m, &group_rows, types);
                metrics_map.insert(m.alias.clone(), val);
                if let Some((field, n)) = ignored
                    && n > 0
                {
                    // Keep the max across metrics sharing a field.
                    let prev = ignored_map
                        .get(&field)
                        .and_then(|v: &Value| v.as_i64())
                        .unwrap_or(0);
                    ignored_map.insert(field, Value::from(prev.max(n)));
                }
            }
            AggGroup {
                group: gmap,
                metrics: metrics_map,
                n_ignored: ignored_map,
            }
        })
        .collect();

    AggResult {
        group_by: group_by.to_vec(),
        total_rows: rows.len() as i64,
        groups,
    }
}

/// Compute one metric over a group's rows. Returns the metric value and, for
/// numeric functions, an optional `(field, n_ignored)` count of present-but-
/// non-coercible values.
fn compute_metric(
    m: &MetricSpec,
    rows: &[&Value],
    types: &HashMap<String, ColumnType>,
) -> (Value, Option<(String, i64)>) {
    match m.func {
        AggFunc::Count => match &m.field {
            None => (Value::from(rows.len() as i64), None),
            Some(f) => {
                let n = rows.iter().filter(|r| present_non_null(r, f)).count();
                (Value::from(n as i64), None)
            }
        },
        AggFunc::CountDistinct => {
            let f = field_or_empty(&m.field);
            let set: HashSet<String> = rows
                .iter()
                .filter_map(|r| r.get(f))
                .filter(|v| !v.is_null())
                .map(|v| v.to_string())
                .collect();
            (Value::from(set.len() as i64), None)
        }
        AggFunc::Sum | AggFunc::Avg | AggFunc::Stddev | AggFunc::Median => {
            let f = field_or_empty(&m.field);
            let (nums, ignored) = collect_numbers(rows, f);
            if nums.is_empty() {
                return (Value::Null, Some((f.to_string(), ignored)));
            }
            let v = match m.func {
                AggFunc::Sum => nums.iter().sum::<f64>(),
                AggFunc::Avg => nums.iter().sum::<f64>() / nums.len() as f64,
                AggFunc::Stddev => sample_stddev(&nums),
                AggFunc::Median => crate::stats::inference::median(&nums),
                _ => unreachable!(),
            };
            (number_value(v), Some((f.to_string(), ignored)))
        }
        AggFunc::Min | AggFunc::Max => {
            let f = field_or_empty(&m.field);
            compute_min_max(m.func, rows, f, types.get(f).copied())
        }
    }
}

fn field_or_empty(field: &Option<String>) -> &str {
    field.as_deref().unwrap_or("")
}

fn present_non_null(row: &Value, field: &str) -> bool {
    row.get(field).is_some_and(|v| !v.is_null())
}

/// Coercible numbers + count of present-non-null-but-non-numeric values.
fn collect_numbers(rows: &[&Value], field: &str) -> (Vec<f64>, i64) {
    let mut nums = Vec::new();
    let mut ignored = 0i64;
    for r in rows {
        match r.get(field) {
            None => {}
            Some(Value::Null) => {}
            Some(v) => match coerce_number(v) {
                Some(n) => nums.push(n),
                None => ignored += 1,
            },
        }
    }
    (nums, ignored)
}

fn sample_stddev(nums: &[f64]) -> f64 {
    let n = nums.len();
    if n < 2 {
        return 0.0;
    }
    let mean = nums.iter().sum::<f64>() / n as f64;
    let var = nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
    var.sqrt()
}

/// JSON number from an `f64`, trimming a `.0` integer to an integer JSON number
/// (so `count`-shaped sums render cleanly), else a float.
fn number_value(v: f64) -> Value {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        Value::from(v as i64)
    } else if v.is_finite() {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    }
}

/// Type-aware min/max. Numeric columns (and open columns with numeric values)
/// compare numerically; `Text` lexically; `Timestamp` by parsed instant
/// (returning the original string).
fn compute_min_max(
    func: AggFunc,
    rows: &[&Value],
    field: &str,
    ty: Option<ColumnType>,
) -> (Value, Option<(String, i64)>) {
    let want_max = matches!(func, AggFunc::Max);
    match ty {
        Some(ColumnType::Text) => {
            let best = rows
                .iter()
                .filter_map(|r| r.get(field))
                .filter_map(|v| v.as_str())
                .fold(None::<&str>, |acc, s| match acc {
                    None => Some(s),
                    Some(cur) => Some(if (s > cur) == want_max { s } else { cur }),
                });
            (best.map(Value::from).unwrap_or(Value::Null), None)
        }
        Some(ColumnType::Timestamp) => {
            let mut best: Option<(chrono::DateTime<chrono::FixedOffset>, String)> = None;
            for r in rows {
                if let Some(Value::String(s)) = r.get(field)
                    && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s)
                {
                    let take = match &best {
                        None => true,
                        Some((cur, _)) => (dt > *cur) == want_max,
                    };
                    if take {
                        best = Some((dt, s.clone()));
                    }
                }
            }
            (
                best.map(|(_, s)| Value::from(s)).unwrap_or(Value::Null),
                None,
            )
        }
        _ => {
            // Numeric (declared Integer/Number, or open with numeric values).
            let (nums, ignored) = collect_numbers(rows, field);
            if nums.is_empty() {
                return (Value::Null, Some((field.to_string(), ignored)));
            }
            let best = nums.iter().copied().fold(
                if want_max {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                },
                |acc, x| if want_max { acc.max(x) } else { acc.min(x) },
            );
            (number_value(best), Some((field.to_string(), ignored)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vocabulary_is_pinned() {
        let got: HashSet<&str> = AggFunc::ALL.iter().map(|f| f.as_str()).collect();
        let expected: HashSet<&str> = [
            "count",
            "sum",
            "avg",
            "min",
            "max",
            "stddev",
            "median",
            "count_distinct",
        ]
        .into_iter()
        .collect();
        assert_eq!(got, expected, "AggFunc vocabulary drifted");
        assert_eq!(AggFunc::ALL.len(), 8);
        assert_eq!(got.len(), 8, "duplicate as_str() value in AggFunc");
    }

    #[test]
    fn parse_roundtrips() {
        for f in AggFunc::ALL {
            assert_eq!(AggFunc::parse(f.as_str()), Some(*f));
        }
        assert_eq!(AggFunc::parse("variance"), None);
    }

    #[test]
    fn field_and_numeric_requirements() {
        assert!(!AggFunc::Count.needs_field());
        assert!(AggFunc::CountDistinct.needs_field());
        assert!(AggFunc::Avg.requires_numeric());
        assert!(AggFunc::Median.requires_numeric());
        assert!(!AggFunc::Min.requires_numeric());
        assert!(!AggFunc::Count.requires_numeric());
    }

    #[test]
    fn default_alias_shapes() {
        assert_eq!(AggFunc::Avg.default_alias(Some("value")), "avg_value");
        assert_eq!(AggFunc::Count.default_alias(None), "count");
    }

    #[test]
    fn coerce_number_accepts_numbers_and_numeric_strings() {
        assert_eq!(coerce_number(&json!(3)), Some(3.0));
        assert_eq!(coerce_number(&json!(3.5)), Some(3.5));
        assert_eq!(coerce_number(&json!("4.2")), Some(4.2));
        assert_eq!(coerce_number(&json!("nope")), None);
        assert_eq!(coerce_number(&json!(true)), None);
    }

    fn rows() -> Vec<Value> {
        vec![
            json!({"metric":"lat","value":10.0}),
            json!({"metric":"lat","value":20.0}),
            json!({"metric":"lat","value":"bad"}), // ignored by numeric funcs
            json!({"metric":"tput","value":4200}),
        ]
    }

    #[test]
    fn grouped_avg_median_count_with_ignored() {
        let metrics = vec![
            MetricSpec {
                func: AggFunc::Count,
                field: None,
                alias: "count".into(),
            },
            MetricSpec {
                func: AggFunc::Avg,
                field: Some("value".into()),
                alias: "avg_value".into(),
            },
            MetricSpec {
                func: AggFunc::Median,
                field: Some("value".into()),
                alias: "median_value".into(),
            },
        ];
        let mut types = HashMap::new();
        types.insert("value".to_string(), ColumnType::Number);
        let agg = compute_aggregation(&rows(), &["metric".to_string()], &metrics, &types);
        assert_eq!(agg.total_rows, 4);
        assert_eq!(agg.groups.len(), 2);
        let lat = agg
            .groups
            .iter()
            .find(|g| g.group.get("metric") == Some(&json!("lat")))
            .unwrap();
        assert_eq!(lat.metrics.get("count"), Some(&json!(3)));
        assert_eq!(lat.metrics.get("avg_value"), Some(&json!(15))); // (10+20)/2
        assert_eq!(lat.metrics.get("median_value"), Some(&json!(15)));
        // The "bad" value was present-non-null but non-numeric → ignored.
        assert_eq!(lat.n_ignored.get("value"), Some(&json!(1)));
    }

    #[test]
    fn overall_group_when_no_group_by() {
        let metrics = vec![MetricSpec {
            func: AggFunc::Sum,
            field: Some("value".into()),
            alias: "sum_value".into(),
        }];
        let agg = compute_aggregation(&rows(), &[], &metrics, &HashMap::new());
        assert_eq!(agg.groups.len(), 1);
        assert_eq!(agg.groups[0].metrics.get("sum_value"), Some(&json!(4230))); // 10+20+4200
    }

    #[test]
    fn empty_rows_no_group_by_yields_one_zero_group() {
        let metrics = vec![MetricSpec {
            func: AggFunc::Count,
            field: None,
            alias: "count".into(),
        }];
        let agg = compute_aggregation(&[], &[], &metrics, &HashMap::new());
        assert_eq!(agg.groups.len(), 1);
        assert_eq!(agg.groups[0].metrics.get("count"), Some(&json!(0)));
    }

    #[test]
    fn min_max_numeric_and_text() {
        let mut types = HashMap::new();
        types.insert("value".to_string(), ColumnType::Number);
        types.insert("metric".to_string(), ColumnType::Text);
        let metrics = vec![
            MetricSpec {
                func: AggFunc::Min,
                field: Some("value".into()),
                alias: "min_value".into(),
            },
            MetricSpec {
                func: AggFunc::Max,
                field: Some("metric".into()),
                alias: "max_metric".into(),
            },
        ];
        let agg = compute_aggregation(&rows(), &[], &metrics, &types);
        let g = &agg.groups[0];
        assert_eq!(g.metrics.get("min_value"), Some(&json!(10)));
        assert_eq!(g.metrics.get("max_metric"), Some(&json!("tput"))); // lexical max
    }
}
