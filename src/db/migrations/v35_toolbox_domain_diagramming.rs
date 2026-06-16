//! Migration step 35: widen the `tool_cards.domain` CHECK to admit `diagramming`.
//!
//! The v32 migration created `tool_cards` with a `domain` CHECK built from
//! [`ToolDomain::sql_in_list`] via `CREATE TABLE IF NOT EXISTS`, later widened to
//! admit `security` by v33. On installs provisioned before the `diagramming`
//! domain existed, that CHECK is frozen at the prior values and re-running v32/v33
//! will NOT widen it (the table already exists). This step drops and re-adds the
//! CHECK from the current `sql_in_list()`, so the Rust enum stays the single source
//! of truth (ADR-003).
//!
//! Idempotent: `DROP CONSTRAINT IF EXISTS` + re-`ADD`, version-gated by
//! `apply_step`. The constraint name `tool_cards_domain_check` is PostgreSQL's
//! deterministic auto-name for the inline unnamed CHECK in v32 (`<table>_<column>_check`).

use sqlx::PgPool;

use crate::tools_catalog::ToolDomain;

pub(super) const TOOLBOX_DOMAIN_DIAGRAMMING: i32 = 35;
pub(super) const TOOLBOX_DOMAIN_DIAGRAMMING_NAME: &str = "toolbox_domain_diagramming";

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
        assert_eq!(TOOLBOX_DOMAIN_DIAGRAMMING, 35);
        assert_eq!(
            TOOLBOX_DOMAIN_DIAGRAMMING_NAME,
            "toolbox_domain_diagramming"
        );
    }

    #[test]
    fn check_includes_diagramming() {
        // The widened CHECK must admit the new domain (and the prior three).
        let list = ToolDomain::sql_in_list();
        assert!(
            list.contains("'diagramming'"),
            "sql_in_list missing diagramming: {list}"
        );
        assert!(list.contains("'formal_verification'"));
        assert!(list.contains("'developer_tooling'"));
        assert!(list.contains("'security'"));
    }
}
