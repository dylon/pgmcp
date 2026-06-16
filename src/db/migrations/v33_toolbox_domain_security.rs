//! Migration step 33: widen the `tool_cards.domain` CHECK to admit `security`.
//!
//! The v32 migration created `tool_cards` with a `domain` CHECK built from
//! [`ToolDomain::sql_in_list`] via `CREATE TABLE IF NOT EXISTS`. On installs
//! provisioned before the `security` domain existed, that CHECK is frozen at the
//! original two values and re-running v32 will NOT widen it (the table already
//! exists). This step drops and re-adds the CHECK from the current
//! `sql_in_list()`, so the Rust enum stays the single source of truth (ADR-003).
//!
//! Idempotent: `DROP CONSTRAINT IF EXISTS` + re-`ADD`, version-gated by
//! `apply_step`. The constraint name `tool_cards_domain_check` is PostgreSQL's
//! deterministic auto-name for the inline unnamed CHECK in v32 (`<table>_<column>_check`).

use sqlx::PgPool;

use crate::tools_catalog::ToolDomain;

pub(super) const TOOLBOX_DOMAIN_SECURITY: i32 = 33;
pub(super) const TOOLBOX_DOMAIN_SECURITY_NAME: &str = "toolbox_domain_security";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE tool_cards DROP CONSTRAINT IF EXISTS tool_cards_domain_check")
        .execute(pool)
        .await?;

    // The interpolated value is enum-derived and trusted (no user input).
    let add = format!(
        "ALTER TABLE tool_cards ADD CONSTRAINT tool_cards_domain_check CHECK (domain IN ({domains}))",
        domains = ToolDomain::sql_in_list(),
    );
    sqlx::query(sqlx::AssertSqlSafe(add.as_str()))
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(TOOLBOX_DOMAIN_SECURITY, 33);
        assert_eq!(TOOLBOX_DOMAIN_SECURITY_NAME, "toolbox_domain_security");
    }

    #[test]
    fn check_includes_security() {
        // The widened CHECK must admit the new domain (and the prior two).
        let list = ToolDomain::sql_in_list();
        assert!(
            list.contains("'security'"),
            "sql_in_list missing security: {list}"
        );
        assert!(list.contains("'formal_verification'"));
        assert!(list.contains("'developer_tooling'"));
    }
}
