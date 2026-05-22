//! MetricsConfig + defaults — extracted from `config.rs` as part of the
//! D.2 god-file split.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsConfig {
    #[serde(default = "default_http_enabled")]
    pub http_enabled: bool,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_http_bind")]
    pub http_bind: String,
    /// Master switch for the `mcp_tool_calls` durable telemetry pipeline.
    /// When false, no rows are written to the DB; in-memory counters
    /// (`tool_invocations`, `tool_telemetry_by_client`) and Prometheus
    /// exposition continue to work, but historical aggregation queries
    /// via the `mcp_tool_telemetry` MCP tool will see only the
    /// in-memory window.
    #[serde(default = "default_telemetry_db_write_enabled")]
    pub telemetry_db_write_enabled: bool,
    /// Retain `mcp_tool_calls` rows for this many days; older rows are
    /// purged by the daily `telemetry-retention` cron job.
    #[serde(default = "default_telemetry_retention_days")]
    pub telemetry_retention_days: u32,
    /// Fraction of tool calls (0.0 – 1.0) that get a DB row written.
    /// In-memory counters always update regardless; sampling only reduces
    /// durable-storage volume.
    #[serde(default = "default_telemetry_sample_rate")]
    pub telemetry_sample_rate: f64,
    /// Telemetry-writer batches up to this many rows before issuing
    /// the bulk INSERT.
    #[serde(default = "default_telemetry_batch_size")]
    pub telemetry_batch_size: usize,
    /// Telemetry-writer flushes a partial batch after this many
    /// milliseconds even if `telemetry_batch_size` hasn't been reached.
    #[serde(default = "default_telemetry_batch_interval_ms")]
    pub telemetry_batch_interval_ms: u64,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            http_enabled: default_http_enabled(),
            http_port: default_http_port(),
            http_bind: default_http_bind(),
            telemetry_db_write_enabled: default_telemetry_db_write_enabled(),
            telemetry_retention_days: default_telemetry_retention_days(),
            telemetry_sample_rate: default_telemetry_sample_rate(),
            telemetry_batch_size: default_telemetry_batch_size(),
            telemetry_batch_interval_ms: default_telemetry_batch_interval_ms(),
        }
    }
}

fn default_http_enabled() -> bool {
    true
}
fn default_http_port() -> u16 {
    9464
}
fn default_http_bind() -> String {
    "127.0.0.1".into()
}
fn default_telemetry_db_write_enabled() -> bool {
    true
}
fn default_telemetry_retention_days() -> u32 {
    30
}
fn default_telemetry_sample_rate() -> f64 {
    1.0
}
fn default_telemetry_batch_size() -> usize {
    256
}
fn default_telemetry_batch_interval_ms() -> u64 {
    500
}
