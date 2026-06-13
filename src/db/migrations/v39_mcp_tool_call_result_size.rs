//! Migration step 39: `mcp_tool_call_result_size`.
//!
//! Adds per-call result-size telemetry to `mcp_tool_calls`: `result_bytes` (the
//! serialized response size in bytes) and `result_tokens_est` (a ~4-chars/token
//! estimate). Recorded by `instrumented_tool_run` on the async telemetry path
//! (zero hot-path cost) and surfaced by the `mcp_tool_telemetry` tool's
//! `output_bytes` aggregation, so result-payload slimming can target the measured
//! top-N offenders per client rather than guesses.
//!
//! The bootstrap `CREATE TABLE mcp_tool_calls` (in `migrations.rs`) already
//! carries these columns for fresh installs; this `ALTER … ADD COLUMN IF NOT
//! EXISTS` retrofits existing installs. Additive, idempotent, version-gated.

use sqlx::PgPool;

pub(super) const MCP_TOOL_CALL_RESULT_SIZE: i32 = 39;
pub(super) const MCP_TOOL_CALL_RESULT_SIZE_NAME: &str = "mcp_tool_call_result_size";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE mcp_tool_calls ADD COLUMN IF NOT EXISTS result_bytes INTEGER")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE mcp_tool_calls ADD COLUMN IF NOT EXISTS result_tokens_est INTEGER")
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(MCP_TOOL_CALL_RESULT_SIZE, 39);
        assert_eq!(MCP_TOOL_CALL_RESULT_SIZE_NAME, "mcp_tool_call_result_size");
    }
}
