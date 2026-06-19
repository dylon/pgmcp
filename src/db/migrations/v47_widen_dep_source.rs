//! Migration step 47: widen `project_dependencies.source` CHECK for the new
//! multi-ecosystem `DepSource` arms (npm / pypi / go / maven / lake) — ADR-027
//! Stage 2.
//!
//! v28 created the column with an INLINE CHECK, which Postgres auto-names
//! `project_dependencies_source_check`. `install_check` drops that name (IF
//! EXISTS) and re-adds the constraint with the current full `DepSource` list, so
//! existing installs widen cleanly without a data rewrite. Idempotent.

use sqlx::PgPool;

use crate::deps::dep_source_sql_in_list;

pub(super) const WIDEN_DEP_SOURCE: i32 = 47;
pub(super) const WIDEN_DEP_SOURCE_NAME: &str = "widen_dep_source";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    super::v4_work_items::install_check(
        pool,
        "project_dependencies",
        "project_dependencies_source_check",
        &format!("source IN ({})", dep_source_sql_in_list()),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(WIDEN_DEP_SOURCE, 47);
        assert_eq!(WIDEN_DEP_SOURCE_NAME, "widen_dep_source");
    }
}
