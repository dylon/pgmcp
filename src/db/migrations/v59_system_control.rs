//! Migration step 59: `system_control_v1` — a singleton control row for the
//! fleet-wide ALL-STOP (ADR-016 E8). `halted = true` makes the A2A dispatcher
//! refuse new tasks and abort in-flight tasks at the next round boundary. The
//! flag is **durable**, so an all-stop survives a daemon restart and the
//! operator must explicitly resume (`POST /api/control/resume` / `fleet_resume`).
//! A fixed primary key (`CHECK id = 1`) enforces the singleton. Idempotent.

use sqlx::PgPool;

pub(super) const SYSTEM_CONTROL_V1: i32 = 59;
pub(super) const SYSTEM_CONTROL_V1_NAME: &str = "system_control_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS system_control (
            id        INTEGER PRIMARY KEY DEFAULT 1 CHECK (id = 1),
            halted    BOOLEAN NOT NULL DEFAULT FALSE,
            halted_at TIMESTAMPTZ,
            reason    TEXT
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO system_control (id, halted) VALUES (1, FALSE)
         ON CONFLICT (id) DO NOTHING",
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
        assert_eq!(SYSTEM_CONTROL_V1, 59);
        assert_eq!(SYSTEM_CONTROL_V1_NAME, "system_control_v1");
    }
}
