//! Lightweight schema-introspection and idempotent-DDL helpers shared across
//! migration steps.
//!
//! These make legacy-column DDL tolerant of the post-cutover
//! schema. The pre-BGE-M3 384-dim `embedding` column is permanently dropped at
//! cutover, but `ALTER TABLE … ALTER COLUMN embedding DROP NOT NULL` (and
//! `CREATE INDEX … (embedding …)`) have no `IF EXISTS` form — so callers gate
//! the legacy DDL on [`column_exists`] to keep `run_migrations` idempotent
//! across all three states (fresh / mid-migration dual-write / post-cutover).
//!
//! The inline initial-schema block in `run_migrations` re-runs on *every*
//! daemon boot, so any lock-escalating statement it issues unconditionally
//! (`ALTER … DROP NOT NULL`, `ADD CONSTRAINT …`) takes an ACCESS EXCLUSIVE lock
//! every start — which can collide with a long-running analytic query mid
//! restart and abort startup at `lock_timeout`. [`column_is_nullable`] and
//! [`ensure_named_constraint`] gate those statements so the common (unchanged)
//! path is a lock-free catalog read.

use sqlx::PgPool;

/// True if `column` exists on `table` in the current database's `public`
/// schema.
pub(super) async fn column_exists(
    pool: &PgPool,
    table: &str,
    column: &str,
) -> Result<bool, sqlx::Error> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = $1 AND column_name = $2
         )",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

/// True if `column` on `table` is currently nullable (`is_nullable = 'YES'` in
/// `information_schema.columns`). A missing column reports `false`.
///
/// Used to skip an `ALTER COLUMN … DROP NOT NULL` — which has no `IF` form and
/// takes ACCESS EXCLUSIVE on the table — when the column is already nullable, so
/// the every-boot re-run of the inline schema block does not request a lock it
/// does not need.
pub(super) async fn column_is_nullable(
    pool: &PgPool,
    table: &str,
    column: &str,
) -> Result<bool, sqlx::Error> {
    let is_nullable: Option<String> = sqlx::query_scalar::<_, String>(
        "SELECT is_nullable FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = $1 AND column_name = $2",
    )
    .bind(table)
    .bind(column)
    .fetch_optional(pool)
    .await?;
    Ok(is_nullable.as_deref() == Some("YES"))
}

/// Idempotently install a named table constraint, skipping the expensive
/// `DROP CONSTRAINT … ; ADD CONSTRAINT …` re-install when the constraint is
/// already present with the same definition.
///
/// `ADD CONSTRAINT` has no `IF NOT EXISTS` form (before PG 17), takes ACCESS
/// EXCLUSIVE, and for CHECK / FK constraints revalidates every existing row.
/// Because the inline schema block re-runs on every boot, the historical
/// DROP-then-ADD idiom paid that full-table revalidation — and an
/// ACCESS-EXCLUSIVE lock that can collide with a long-running query during a
/// restart — on *every* start.
///
/// We stamp the desired `definition` into the constraint's `COMMENT` and compare
/// it on the next boot. A definition change (e.g. a new mandate polarity)
/// changes the stamp and triggers a correct re-install; an unchanged definition
/// costs one catalog read with no lock escalation.
///
/// `table`, `name`, and `definition` are compile-time-constant SQL fragments
/// from this crate (never user input), so they are interpolated directly.
/// `definition` is the constraint body, e.g. `"CHECK (scope IN ('project'))"`.
pub(super) async fn ensure_named_constraint(
    pool: &PgPool,
    table: &str,
    name: &str,
    definition: &str,
) -> Result<(), sqlx::Error> {
    // `obj_description` is NULL when the constraint exists without a comment and
    // the row is absent when the constraint does not exist; both mean "stamp
    // does not match" and fall through to a (re)install.
    let stamped: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT obj_description(c.oid, 'pg_constraint')
           FROM pg_constraint c
           JOIN pg_class t ON t.oid = c.conrelid
          WHERE c.conname = $1 AND t.relname = $2",
    )
    .bind(name)
    .bind(table)
    .fetch_optional(pool)
    .await?
    .flatten();

    if stamped.as_deref() == Some(definition) {
        return Ok(());
    }

    sqlx::query(&format!(
        "ALTER TABLE {table} DROP CONSTRAINT IF EXISTS {name}"
    ))
    .execute(pool)
    .await?;
    sqlx::query(&format!(
        "ALTER TABLE {table} ADD CONSTRAINT {name} {definition}"
    ))
    .execute(pool)
    .await?;
    // `COMMENT` is a utility statement and does not accept bind parameters, so
    // the (constant) definition is single-quote-escaped and inlined.
    let escaped = definition.replace('\'', "''");
    sqlx::query(&format!(
        "COMMENT ON CONSTRAINT {name} ON {table} IS '{escaped}'"
    ))
    .execute(pool)
    .await?;
    Ok(())
}
