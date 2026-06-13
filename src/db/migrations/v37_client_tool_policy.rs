//! Migration step 37: `client_tool_policy`.
//!
//! The materialized output of the adaptive tool-surface learner
//! (`src/mcp/tool_policy.rs`): one row per `(client, tool)` carrying a
//! recency-decayed usage `weight`. The `tool-policy-refresh` cron fully
//! recomputes this table from `mcp_tool_calls`; `list_tools` never reads it
//! directly — it reads the in-memory `ToolPolicySnapshot` derived from it — so
//! the table is the durable seed that survives a daemon restart before the first
//! cron pass.
//!
//! Additive + `IF NOT EXISTS`, idempotent, version-gated.

use sqlx::PgPool;

pub(super) const CLIENT_TOOL_POLICY: i32 = 37;
pub(super) const CLIENT_TOOL_POLICY_NAME: &str = "client_tool_policy";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS client_tool_policy (
            client_name TEXT NOT NULL,
            tool_name   TEXT NOT NULL,
            weight      DOUBLE PRECISION NOT NULL DEFAULT 0,
            last_used   TIMESTAMPTZ,
            updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            PRIMARY KEY (client_name, tool_name)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_client_tool_policy_client
         ON client_tool_policy(client_name)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(CLIENT_TOOL_POLICY, 37);
        assert_eq!(CLIENT_TOOL_POLICY_NAME, "client_tool_policy");
    }
}
