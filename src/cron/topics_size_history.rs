//! `topics-size-history` cron: snapshot per-topic chunk counts into
//! `pgmcp_metadata['topics_size_history']` so `topic_trends` can read a
//! per-topic *trajectory* instead of a single point. Cheap — one read of
//! `code_topics` plus one metadata write — so it runs behind its own light lock
//! in `scheduler.rs`, interval-gated on `topics_size_history_interval_secs > 0`
//! (default 6h), the same idiom as `tool_policy_refresh` / `quality_history`.

use sqlx::PgPool;
use tracing::{error, info};

/// Append the current per-topic sizes to the bounded history. Best-effort: a
/// failure is logged, never fatal (the next run retries).
pub async fn run_or_log(pool: &PgPool) {
    match crate::db::queries::set_topics_size_snapshot(pool).await {
        Ok(n) => info!(
            job = "topics-size-history",
            topics = n,
            "size snapshot stored"
        ),
        Err(e) => error!(job = "topics-size-history", error = %e, "size snapshot failed"),
    }
}
