//! Migration step 67: `durable_mandates_operator_columns` — provenance +
//! soft-delete columns for web-UI operator-authored / edited durable mandates.
//! `source_mandate_id` is already nullable, so operator-authored rules (no
//! originating session mandate) need no change there; these columns record who
//! created/updated a rule and support a soft `retired_at` instead of a hard
//! delete.

use sqlx::PgPool;

pub(super) const DURABLE_MANDATES_OPERATOR: i32 = 67;
pub(super) const DURABLE_MANDATES_OPERATOR_NAME: &str = "durable_mandates_operator_columns";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    for stmt in [
        "ALTER TABLE durable_mandates ADD COLUMN IF NOT EXISTS created_by TEXT",
        "ALTER TABLE durable_mandates ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ",
        "ALTER TABLE durable_mandates ADD COLUMN IF NOT EXISTS retired_at TIMESTAMPTZ",
    ] {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(DURABLE_MANDATES_OPERATOR, 67);
        assert_eq!(
            DURABLE_MANDATES_OPERATOR_NAME,
            "durable_mandates_operator_columns"
        );
    }
}
