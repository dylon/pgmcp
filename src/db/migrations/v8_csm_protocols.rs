//! Migration step 8: `csm_protocols_v1` — the Communicating-State-Machine /
//! Multiparty-Session-Type tables (ADR-009). Three tables back the `csm_*`
//! observer tools:
//!
//! - `csm_protocols`: the registry of encoded global types (one per pattern; adjacent-tagged JSONB).
//! - `csm_projections`: cached per-role local types (re-derivable by `csm::mpst::project`); `projection_error` records a role that does not project.
//! - `csm_run_traces`: each validated run's lifted trace + conformance verdict, FK to `a2a_tasks`. `encoded_series` is column-compatible with `agent_trajectories.encoded_series` and `trajectory_id` links to it — both populated by the Phase-3 MSM bridge.
//!
//! `a2a_tasks` is created earlier in `run_migrations` (the inline A2A block), so
//! the `csm_run_traces` FK resolves.

use sqlx::PgPool;

pub(super) const CSM_PROTOCOLS_V1: i32 = 8;
pub(super) const CSM_PROTOCOLS_V1_NAME: &str = "csm_protocols_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS csm_protocols (
            id BIGSERIAL PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            pattern_skill_id TEXT NOT NULL,
            global_type JSONB NOT NULL,
            participants TEXT[] NOT NULL,
            wellformed BOOLEAN NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS csm_projections (
            id BIGSERIAL PRIMARY KEY,
            protocol_id BIGINT NOT NULL REFERENCES csm_protocols(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            local_type JSONB,
            n_states INTEGER NOT NULL DEFAULT 0,
            projection_error TEXT,
            UNIQUE (protocol_id, role)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS csm_run_traces (
            id BIGSERIAL PRIMARY KEY,
            task_id UUID NOT NULL REFERENCES a2a_tasks(id) ON DELETE CASCADE,
            protocol_name TEXT NOT NULL,
            conformant BOOLEAN NOT NULL,
            conformance_error TEXT,
            events JSONB NOT NULL DEFAULT '[]'::jsonb,
            encoded_series DOUBLE PRECISION[],
            trajectory_id BIGINT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_csm_run_traces_task ON csm_run_traces(task_id)")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_csm_run_traces_protocol ON csm_run_traces(protocol_name)",
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
        assert_eq!(CSM_PROTOCOLS_V1, 8);
        assert_eq!(CSM_PROTOCOLS_V1_NAME, "csm_protocols_v1");
    }
}
