//! Migration step 65: `durable_mandates_global_scope` — admit durable mandates
//! that apply before project/workspace-specific mandates.
//!
//! The initial session-mandate implementation restricted durable mandate scope
//! to `project | workspace`. The web UI needs to surface and manage global
//! rules from `~/.claude`, so this step widens the stamped CHECK constraint to
//! include `global`. The base schema constraint is widened too; this migration
//! handles existing installations.

use sqlx::PgPool;

pub(super) const GLOBAL_MANDATES: i32 = 65;
pub(super) const GLOBAL_MANDATES_NAME: &str = "durable_mandates_global_scope";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    super::v4_work_items::install_check(
        pool,
        "durable_mandates",
        "durable_mandates_scope_check",
        "scope IN ('global','project','workspace')",
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(GLOBAL_MANDATES, 65);
        assert_eq!(GLOBAL_MANDATES_NAME, "durable_mandates_global_scope");
    }
}
