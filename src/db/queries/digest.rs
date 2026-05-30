//! Rate-limit / dedup ledger queries for the proactive digest (Phase 4).
//!
//! These are the digest subsystem's ONLY writes against the database (one
//! INSERT in [`insert_digest_emission`]); everything else the digest does is a
//! `SELECT` (see `src/digest/mod.rs`). Mirrors the `nudge_emissions` helpers in
//! `src/sessions.rs` (`recently_nudged` / `session_nudge_count` /
//! `insert_nudge_emission`).
//!
//! Reached via the named `crate::db::queries::digest::*` path (this submodule is
//! declared `pub mod digest;` in `src/db/queries.rs`, NOT flattened with
//! `pub use`, to keep the ledger surface namespaced and obviously distinct from
//! the read queries).

use sqlx::PgPool;

/// True if an identical digest (same `content_sha256`) was emitted to
/// `session_id` within the last `ttl_secs` — the within-TTL dedup gate.
pub async fn recently_emitted(
    pool: &PgPool,
    session_id: &str,
    content_sha256: &str,
    ttl_secs: i64,
) -> Result<bool, sqlx::Error> {
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM digest_emissions
          WHERE session_id = $1 AND content_sha256 = $2
            AND ts > now() - ($3::bigint * interval '1 second')
          LIMIT 1",
    )
    .bind(session_id)
    .bind(content_sha256)
    .bind(ttl_secs.max(0))
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Lifetime count of digests emitted in this session (across channels) — the
/// per-session cap input.
pub async fn session_emit_count(pool: &PgPool, session_id: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*)::int8 FROM digest_emissions WHERE session_id = $1")
        .bind(session_id)
        .fetch_one(pool)
        .await
}

/// Record one digest emission. The sole write the digest subsystem performs.
pub async fn insert_digest_emission(
    pool: &PgPool,
    session_id: &str,
    channel: &str,
    project_id: Option<i32>,
    content_sha256: &str,
    item_count: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO digest_emissions
            (session_id, channel, project_id, content_sha256, item_count)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(session_id)
    .bind(channel)
    .bind(project_id)
    .bind(content_sha256)
    .bind(item_count)
    .execute(pool)
    .await?;
    Ok(())
}
