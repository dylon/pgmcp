//! Read/write helpers for the concurrency persistence + temporal tables
//! (migration `v22_concurrency_findings`): the `concurrency_findings` ledger,
//! the bitemporal `lock_order_edges` materialization (feeds the unified-graph
//! `lock_order` arm, Layer 4), and `concurrency_health_history` snapshots
//! (feed the forecast / trajectory machinery).

use sqlx::PgPool;

/// A finding to record (append-with-upsert, keyed by `provenance_key`).
#[derive(Debug, Clone)]
pub struct NewConcurrencyFinding {
    pub finding_kind: String,
    pub severity: String,
    pub confidence: f32,
    pub provenance_key: String,
    pub symbol_id: Option<i64>,
    pub file_id: Option<i64>,
    pub evidence: serde_json::Value,
    pub title: String,
}

/// Upsert a finding by `provenance_key`: a fresh key inserts; an existing key
/// refreshes `observed_at` + the mutable fields (severity/confidence/evidence/
/// title) while preserving `first_observed_at`. Idempotent — re-scanning never
/// duplicates. Returns the row id.
pub async fn record_concurrency_finding(
    pool: &PgPool,
    project_id: i32,
    f: &NewConcurrencyFinding,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO concurrency_findings
            (project_id, finding_kind, severity, confidence, provenance_key,
             symbol_id, file_id, evidence, title)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (provenance_key) DO UPDATE SET
            observed_at = now(),
            severity    = EXCLUDED.severity,
            confidence  = EXCLUDED.confidence,
            evidence    = EXCLUDED.evidence,
            title       = EXCLUDED.title
         RETURNING id",
    )
    .bind(project_id)
    .bind(&f.finding_kind)
    .bind(&f.severity)
    .bind(f.confidence)
    .bind(&f.provenance_key)
    .bind(f.symbol_id)
    .bind(f.file_id)
    .bind(&f.evidence)
    .bind(&f.title)
    .fetch_one(pool)
    .await
}

/// Back-patch a finding's `promoted_item_id` (audit link to the tracker item).
pub async fn set_finding_promoted_item(
    pool: &PgPool,
    finding_id: i64,
    item_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE concurrency_findings SET promoted_item_id = $2 WHERE id = $1")
        .bind(finding_id)
        .bind(item_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// One lock-order edge to materialize into `lock_order_edges`.
#[derive(Debug, Clone)]
pub struct NewLockEdge {
    pub from_key: String,
    pub to_key: String,
    pub from_mode: Option<String>,
    pub to_mode: Option<String>,
    pub min_confidence: f32,
    pub interprocedural: bool,
}

/// Bitemporal refresh of a project's lock-order edges, in one transaction:
/// 1. Upsert each currently-present edge (re-open / refresh `last_seen_at`),
///    keeping at most one OPEN row per edge (the partial unique index).
/// 2. Close (`valid_to = now()`) every open edge NOT seen this run — its
///    `last_seen_at` predates this transaction's `now()`.
///
/// So `valid_from`/`valid_to` give each edge's analysis-history validity, which
/// the unified-graph `lock_order` arm exposes for `as_of` queries.
pub async fn refresh_lock_order_edges(
    pool: &PgPool,
    project_id: i32,
    edges: &[NewLockEdge],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for e in edges {
        sqlx::query(
            "INSERT INTO lock_order_edges
                (project_id, from_key, to_key, from_mode, to_mode, min_confidence,
                 interprocedural, valid_from, valid_to, last_seen_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, now(), NULL, now())
             ON CONFLICT (project_id, from_key, to_key) WHERE valid_to IS NULL
             DO UPDATE SET
                last_seen_at    = now(),
                min_confidence  = EXCLUDED.min_confidence,
                from_mode       = EXCLUDED.from_mode,
                to_mode         = EXCLUDED.to_mode,
                interprocedural = EXCLUDED.interprocedural",
        )
        .bind(project_id)
        .bind(&e.from_key)
        .bind(&e.to_key)
        .bind(&e.from_mode)
        .bind(&e.to_mode)
        .bind(e.min_confidence)
        .bind(e.interprocedural)
        .execute(&mut *tx)
        .await?;
    }
    // Close edges no longer present (last_seen_at strictly before this tx's now()).
    sqlx::query(
        "UPDATE lock_order_edges SET valid_to = now()
         WHERE project_id = $1 AND valid_to IS NULL AND last_seen_at < now()",
    )
    .bind(project_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Insert a per-project concurrency-health snapshot (forecast/trajectory input).
#[allow(clippy::too_many_arguments)]
pub async fn insert_concurrency_health(
    pool: &PgPool,
    project_id: i32,
    deadlock_cycle_count: i32,
    channel_cycle_count: i32,
    blocked_recv_count: i32,
    max_lock_contention: f32,
    raw_summary: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO concurrency_health_history
            (project_id, deadlock_cycle_count, channel_cycle_count, blocked_recv_count,
             max_lock_contention, raw_summary)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(project_id)
    .bind(deadlock_cycle_count)
    .bind(channel_cycle_count)
    .bind(blocked_recv_count)
    .bind(max_lock_contention)
    .bind(raw_summary)
    .execute(pool)
    .await?;
    Ok(())
}

/// A lock-contention row: a lock acquired by many distinct symbols, weighted by
/// the hottest acquirer file (pagerank). Shared by the `concurrency_bottlenecks`
/// tool and the `concurrency-scan` cron (so the cron's `max_lock_contention`
/// snapshot is the same metric the tool ranks).
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct LockContentionRow {
    pub resource_key: String,
    pub resource_kind: String,
    pub distinct_acquirers: i64,
    pub total_acquires: i64,
    pub max_pagerank: f64,
}

impl LockContentionRow {
    /// Contention score = distinct acquirers × (1 + hottest-file pagerank).
    pub fn contention_score(&self) -> f64 {
        (self.distinct_acquirers as f64) * (1.0 + self.max_pagerank)
    }
}

/// Rank lock resources by contention (distinct acquirers × pagerank).
pub async fn lock_contention_ranking(
    pool: &PgPool,
    project_id: i32,
    limit: i32,
) -> Result<Vec<LockContentionRow>, sqlx::Error> {
    sqlx::query_as::<_, LockContentionRow>(
        "SELECT so.resource_key, so.resource_kind,
                COUNT(DISTINCT so.symbol_id) AS distinct_acquirers,
                COUNT(*) AS total_acquires,
                COALESCE(MAX(fm.pagerank), 0.0) AS max_pagerank
         FROM sync_ops so
         JOIN file_symbols fs ON fs.id = so.symbol_id
         JOIN indexed_files f ON f.id = fs.file_id
         LEFT JOIN file_metrics fm ON fm.file_id = f.id
         WHERE f.project_id = $1
           AND so.op_kind IN ('acquire', 'acquire_read', 'acquire_write')
           AND so.resource_key IS NOT NULL
         GROUP BY so.resource_key, so.resource_kind
         ORDER BY distinct_acquirers DESC, max_pagerank DESC
         LIMIT $2",
    )
    .bind(project_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// A `concurrency_health_history` time-series for one metric column, as
/// `(days_ago, value)` points (newest has the smallest `days_ago`). `column`
/// is validated against a fixed allow-list — column names cannot be bound, so
/// an unknown column yields an empty series rather than an injection. Backs the
/// `concurrency_forecast` tool (reuses `src/quality/forecast.rs`).
pub async fn concurrency_metric_series(
    pool: &PgPool,
    project_id: i32,
    column: &str,
    days: i32,
) -> Result<Vec<(f64, f64)>, sqlx::Error> {
    let col = match column {
        "deadlock_cycle_count"
        | "channel_cycle_count"
        | "blocked_recv_count"
        | "max_lock_contention" => column,
        _ => return Ok(Vec::new()),
    };
    sqlx::query_as::<_, (f64, f64)>(&format!(
        "SELECT EXTRACT(EPOCH FROM (now() - computed_at)) / 86400.0 AS days_ago,
                {col}::float8 AS value
         FROM concurrency_health_history
         WHERE project_id = $1 AND computed_at >= now() - ($2::int8 * INTERVAL '1 day')
         ORDER BY computed_at ASC",
    ))
    .bind(project_id)
    .bind(days)
    .fetch_all(pool)
    .await
}
