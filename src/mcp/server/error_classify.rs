//! Coarse `classify_error_kind` helper — extracted from `server.rs` as
//! part of the D.2 god-file split. Mirrors the rmcp McpError prefix
//! conventions so telemetry can bucket failures without parsing JSON.

/// Coarse error classification for telemetry. Matches a handful of
/// common message prefixes the rmcp `McpError` helpers produce. Anything
/// else falls into `"internal"`.
pub(crate) fn classify_error_kind(msg: &str) -> String {
    if msg.contains("invalid_params") || msg.contains("Invalid parameters") {
        "invalid_params".to_string()
    } else if msg.contains("not found") {
        "not_found".to_string()
    } else if msg.contains("requires") || msg.contains("Requires") {
        "precondition".to_string()
    } else if msg.contains("database") || msg.contains("sql") {
        "db_error".to_string()
    } else {
        "internal".to_string()
    }
}
