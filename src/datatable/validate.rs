//! Row validation against an optional typed-column schema.
//!
//! Pure functions over `serde_json::Value` — no DB. The query layer reads the
//! declared columns from `data_table_columns` into [`ColumnSpec`]s and hands
//! them here; an empty column slice means an *open* (free-form) table, for which
//! every object row validates. Validation is **check-only**: values are stored
//! exactly as the client sent them (after [`fill_defaults`] applies declared
//! defaults for absent fields), so there is no lossy normalization.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::datatable::column_type::ColumnType;

/// A declared column, projected from a `data_table_columns` row. Kept free of
/// any DB type so [`validate_row`] stays unit-testable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnSpec {
    /// Column name (also the JSON field key in a row's `data`).
    pub name: String,
    /// The accepted value type.
    pub data_type: ColumnType,
    /// Whether the field must be present and non-null on insert.
    pub required: bool,
    /// Optional default applied to absent fields before validation.
    pub default: Option<Value>,
}

/// A single validation failure. Serializes with an `error` tag so a tool can
/// return a structured list of what was wrong.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum FieldError {
    /// The row value was not a JSON object.
    NotAnObject,
    /// A required field was absent or null.
    Required {
        /// The offending field name.
        field: String,
    },
    /// A field's value did not match its declared type.
    WrongType {
        /// The offending field name.
        field: String,
        /// The declared `ColumnType` (as a string).
        expected: &'static str,
        /// The JSON type that was supplied instead.
        got: &'static str,
    },
    /// A field not present in the declared schema (only when `reject_unknown`).
    Unknown {
        /// The unexpected field name.
        field: String,
    },
}

impl FieldError {
    /// A human-readable one-line description (for `McpError::invalid_params`).
    pub fn message(&self) -> String {
        match self {
            Self::NotAnObject => "row must be a JSON object".to_string(),
            Self::Required { field } => format!("required field {field:?} is missing or null"),
            Self::WrongType {
                field,
                expected,
                got,
            } => format!("field {field:?} expected {expected}, got {got}"),
            Self::Unknown { field } => format!("unknown field {field:?} (not in schema)"),
        }
    }
}

/// The JSON type name of `v`, for `WrongType.got`.
pub fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Whether a string parses as an RFC3339 timestamp.
fn is_rfc3339(s: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(s).is_ok()
}

/// Whether `v` is acceptable for a column of type `ty`. Coercion is permissive
/// where it is lossless and unambiguous:
///
/// - `Integer` accepts a JSON integer, or a JSON float with a zero fractional
///   part (e.g. `5.0`);
/// - `Number` accepts any JSON number;
/// - `Timestamp` accepts an RFC3339 string or a finite number (epoch seconds);
/// - `Json` accepts anything;
/// - `Text` / `Boolean` accept only their JSON kind.
///
/// A JSON `null` is handled by the caller (allowed for non-required columns),
/// not here.
pub fn value_matches(ty: ColumnType, v: &Value) -> bool {
    match ty {
        ColumnType::Text => v.is_string(),
        ColumnType::Integer => match v {
            Value::Number(n) => {
                n.is_i64()
                    || n.is_u64()
                    || n.as_f64()
                        .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
            }
            _ => false,
        },
        ColumnType::Number => v.as_f64().is_some_and(|f| f.is_finite()),
        ColumnType::Boolean => v.is_boolean(),
        ColumnType::Timestamp => match v {
            Value::String(s) => is_rfc3339(s),
            Value::Number(n) => n.as_f64().is_some_and(|f| f.is_finite()),
            _ => false,
        },
        ColumnType::Json => true,
    }
}

/// Insert declared defaults for any field absent from `obj` (mutates in place).
/// Run before [`validate_row`] so a declared default can satisfy a `required`
/// column. Present-but-null fields are left untouched (an explicit null is the
/// caller's choice).
pub fn fill_defaults(columns: &[ColumnSpec], obj: &mut Map<String, Value>) {
    for col in columns {
        if !obj.contains_key(&col.name)
            && let Some(def) = &col.default
        {
            obj.insert(col.name.clone(), def.clone());
        }
    }
}

/// Validate `row` against `columns`. An empty `columns` slice (open table) only
/// requires that `row` be an object. `reject_unknown` adds an [`FieldError::Unknown`]
/// for any field not in the schema (closed-world rows); the default elsewhere is
/// to allow and store extra fields.
pub fn validate_row(
    columns: &[ColumnSpec],
    row: &Value,
    reject_unknown: bool,
) -> Result<(), Vec<FieldError>> {
    let Some(obj) = row.as_object() else {
        return Err(vec![FieldError::NotAnObject]);
    };
    // Open table: any object is fine.
    if columns.is_empty() {
        return Ok(());
    }
    let mut errs = Vec::new();
    for col in columns {
        match obj.get(&col.name) {
            None | Some(Value::Null) => {
                if col.required {
                    errs.push(FieldError::Required {
                        field: col.name.clone(),
                    });
                }
            }
            Some(v) => {
                if !value_matches(col.data_type, v) {
                    errs.push(FieldError::WrongType {
                        field: col.name.clone(),
                        expected: col.data_type.as_str(),
                        got: json_type_name(v),
                    });
                }
            }
        }
    }
    if reject_unknown {
        for key in obj.keys() {
            if !columns.iter().any(|c| &c.name == key) {
                errs.push(FieldError::Unknown { field: key.clone() });
            }
        }
    }
    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cols() -> Vec<ColumnSpec> {
        vec![
            ColumnSpec {
                name: "ts".into(),
                data_type: ColumnType::Timestamp,
                required: true,
                default: None,
            },
            ColumnSpec {
                name: "value".into(),
                data_type: ColumnType::Number,
                required: true,
                default: None,
            },
            ColumnSpec {
                name: "count".into(),
                data_type: ColumnType::Integer,
                required: false,
                default: None,
            },
            ColumnSpec {
                name: "ok".into(),
                data_type: ColumnType::Boolean,
                required: false,
                default: Some(json!(true)),
            },
            ColumnSpec {
                name: "note".into(),
                data_type: ColumnType::Text,
                required: false,
                default: None,
            },
        ]
    }

    #[test]
    fn integer_accepts_whole_floats_rejects_fractional() {
        assert!(value_matches(ColumnType::Integer, &json!(5)));
        assert!(value_matches(ColumnType::Integer, &json!(5.0)));
        assert!(!value_matches(ColumnType::Integer, &json!(5.5)));
        assert!(!value_matches(ColumnType::Integer, &json!("5")));
    }

    #[test]
    fn number_accepts_int_and_float_rejects_nonfinite_and_string() {
        assert!(value_matches(ColumnType::Number, &json!(3)));
        assert!(value_matches(ColumnType::Number, &json!(3.5)));
        assert!(!value_matches(ColumnType::Number, &json!("3.5")));
    }

    #[test]
    fn timestamp_accepts_rfc3339_or_epoch() {
        assert!(value_matches(
            ColumnType::Timestamp,
            &json!("2026-05-29T14:03:00Z")
        ));
        assert!(value_matches(ColumnType::Timestamp, &json!(1_716_988_980)));
        assert!(!value_matches(ColumnType::Timestamp, &json!("not a date")));
        assert!(!value_matches(ColumnType::Timestamp, &json!(true)));
    }

    #[test]
    fn json_and_text_and_bool() {
        assert!(value_matches(ColumnType::Json, &json!({"a":[1,2]})));
        assert!(value_matches(ColumnType::Text, &json!("hi")));
        assert!(!value_matches(ColumnType::Text, &json!(1)));
        assert!(value_matches(ColumnType::Boolean, &json!(false)));
        assert!(!value_matches(ColumnType::Boolean, &json!(0)));
    }

    #[test]
    fn open_table_accepts_any_object() {
        assert!(validate_row(&[], &json!({"anything": [1,2,3]}), false).is_ok());
        assert!(validate_row(&[], &json!("not an object"), false).is_err());
    }

    #[test]
    fn strict_table_enforces_required_and_types() {
        let c = cols();
        // Good row.
        assert!(
            validate_row(
                &c,
                &json!({"ts":"2026-05-29T14:03:00Z","value":12.4,"count":3,"note":"warm"}),
                false
            )
            .is_ok()
        );
        // Missing required `value`, wrong-typed `count`.
        let err = validate_row(
            &c,
            &json!({"ts":"2026-05-29T14:03:00Z","count":"three"}),
            false,
        )
        .unwrap_err();
        assert!(err.contains(&FieldError::Required {
            field: "value".into()
        }));
        assert!(err.iter().any(|e| matches!(
            e,
            FieldError::WrongType { field, .. } if field == "count"
        )));
    }

    #[test]
    fn null_required_is_an_error_null_optional_is_ok() {
        let c = cols();
        let err = validate_row(
            &c,
            &json!({"ts":"2026-05-29T14:03:00Z","value":null}),
            false,
        )
        .unwrap_err();
        assert!(err.contains(&FieldError::Required {
            field: "value".into()
        }));
        // optional null `note` is fine
        assert!(
            validate_row(
                &c,
                &json!({"ts":"2026-05-29T14:03:00Z","value":1.0,"note":null}),
                false
            )
            .is_ok()
        );
    }

    #[test]
    fn defaults_fill_absent_fields() {
        let c = cols();
        let mut obj = json!({"ts":"2026-05-29T14:03:00Z","value":1.0})
            .as_object()
            .unwrap()
            .clone();
        fill_defaults(&c, &mut obj);
        assert_eq!(obj.get("ok"), Some(&json!(true)));
        assert!(!obj.contains_key("count")); // no default
    }

    #[test]
    fn reject_unknown_flags_extra_fields() {
        let c = cols();
        let err = validate_row(
            &c,
            &json!({"ts":"2026-05-29T14:03:00Z","value":1.0,"surprise":9}),
            true,
        )
        .unwrap_err();
        assert!(err.contains(&FieldError::Unknown {
            field: "surprise".into()
        }));
        // Without reject_unknown, the extra field is allowed.
        assert!(
            validate_row(
                &c,
                &json!({"ts":"2026-05-29T14:03:00Z","value":1.0,"surprise":9}),
                false
            )
            .is_ok()
        );
    }
}
