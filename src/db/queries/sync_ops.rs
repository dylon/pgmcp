//! Read/write helpers for the `sync_ops` ordered synchronization skeleton
//! (migration `v21_sync_ops`).
//!
//! Write path (`bulk_insert_sync_ops`) is called by the symbol-extraction cron
//! inside the same per-file transaction that writes `file_symbols`; it mirrors
//! `bulk_insert_symbol_effects` (column-oriented UNNEST, delete-then-insert
//! replace). Read path (`sync_skeleton_for_project` / `sync_ops_for_symbol`) is
//! called by the concurrency analysis layer (lock-order graph + Petri net).

use sqlx::PgPool;

use crate::parsing::sync_ops::FunctionSyncOps;

/// Bulk-insert ordered sync ops for the given symbols. `symbol_ids[i]` is the
/// `file_symbols.id` of `fn_ops[i]` (the cron resolves each `FunctionSyncOps` to
/// its symbol_id by `(name, start_line)` before calling). Existing rows for each
/// touched symbol_id are deleted first — defensive (the file-symbols delete
/// already cascades via `ON DELETE CASCADE`), so a double call replaces rather
/// than duplicates. Returns the number of op rows inserted.
pub async fn bulk_insert_sync_ops(
    pool: &PgPool,
    symbol_ids: &[i64],
    fn_ops: &[FunctionSyncOps],
) -> Result<u64, sqlx::Error> {
    debug_assert_eq!(symbol_ids.len(), fn_ops.len());
    if fn_ops.is_empty() {
        return Ok(0);
    }

    let total: usize = fn_ops.iter().map(|f| f.ops.len()).sum();
    let mut sids: Vec<i64> = Vec::with_capacity(total);
    let mut seqs: Vec<i32> = Vec::with_capacity(total);
    let mut op_kinds: Vec<String> = Vec::with_capacity(total);
    let mut resource_keys: Vec<Option<String>> = Vec::with_capacity(total);
    let mut resource_kinds: Vec<String> = Vec::with_capacity(total);
    let mut paradigms: Vec<String> = Vec::with_capacity(total);
    let mut nesting_depths: Vec<i32> = Vec::with_capacity(total);
    let mut guard_ids: Vec<Option<i32>> = Vec::with_capacity(total);
    let mut confidences: Vec<f32> = Vec::with_capacity(total);
    let mut lines: Vec<i32> = Vec::with_capacity(total);
    let mut affected_sids: Vec<i64> = Vec::new();

    for (sid, fops) in symbol_ids.iter().zip(fn_ops.iter()) {
        if !fops.ops.is_empty() {
            affected_sids.push(*sid);
        }
        for op in &fops.ops {
            sids.push(*sid);
            seqs.push(op.seq as i32);
            op_kinds.push(op.op_kind.as_db_str().to_string());
            resource_keys.push(op.resource_key.clone());
            resource_kinds.push(op.resource_kind.as_db_str().to_string());
            paradigms.push(op.paradigm.as_db_str().to_string());
            nesting_depths.push(op.nesting_depth as i32);
            guard_ids.push(op.guard_id.map(|g| g as i32));
            confidences.push(op.resource_confidence);
            lines.push(op.line as i32);
        }
    }

    let mut tx = pool.begin().await?;

    if !affected_sids.is_empty() {
        sqlx::query("DELETE FROM sync_ops WHERE symbol_id = ANY($1::int8[])")
            .bind(&affected_sids)
            .execute(&mut *tx)
            .await?;
    }

    if !sids.is_empty() {
        sqlx::query(
            "INSERT INTO sync_ops
                 (symbol_id, seq, op_kind, resource_key, resource_kind, paradigm,
                  nesting_depth, guard_id, resource_confidence, line)
             SELECT * FROM UNNEST(
                 $1::int8[], $2::int4[], $3::text[], $4::text[], $5::text[], $6::text[],
                 $7::int4[], $8::int4[], $9::float4[], $10::int4[])
             ON CONFLICT (symbol_id, seq) DO NOTHING",
        )
        .bind(&sids)
        .bind(&seqs)
        .bind(&op_kinds)
        .bind(&resource_keys)
        .bind(&resource_kinds)
        .bind(&paradigms)
        .bind(&nesting_depths)
        .bind(&guard_ids)
        .bind(&confidences)
        .bind(&lines)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(sids.len() as u64)
}

/// One sync op enriched with its owning symbol + file, for the analysis layer.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct SyncOpRow {
    pub symbol_id: i64,
    pub file_id: i64,
    pub relative_path: String,
    pub symbol_name: String,
    pub scope_path: Option<String>,
    pub seq: i32,
    pub op_kind: String,
    pub resource_key: Option<String>,
    pub resource_kind: String,
    pub paradigm: String,
    pub nesting_depth: i32,
    pub guard_id: Option<i32>,
    pub resource_confidence: f32,
    pub line: i32,
}

/// Fetch the full ordered synchronization skeleton for a project, ordered by
/// `(symbol_id, seq)` so the caller can group consecutively into per-function
/// streams. Optional `paradigm` filter (`'lock'` / `'message'`) lets the
/// lock-order builder and the Petri-net builder each pull only their half.
pub async fn sync_skeleton_for_project(
    pool: &PgPool,
    project_id: i32,
    paradigm: Option<&str>,
) -> Result<Vec<SyncOpRow>, sqlx::Error> {
    sqlx::query_as::<_, SyncOpRow>(
        "SELECT so.symbol_id, fs.file_id, f.relative_path, fs.name AS symbol_name,
                fs.scope_path, so.seq, so.op_kind, so.resource_key, so.resource_kind,
                so.paradigm, so.nesting_depth, so.guard_id, so.resource_confidence, so.line
         FROM sync_ops so
         JOIN file_symbols fs ON fs.id = so.symbol_id
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE f.project_id = $1
           AND ($2::text IS NULL OR so.paradigm = $2)
         ORDER BY so.symbol_id, so.seq",
    )
    .bind(project_id)
    .bind(paradigm)
    .fetch_all(pool)
    .await
}

/// Resolved interprocedural call edges for a project: `(source_symbol_id,
/// target_symbol_id, source_line)`, restricted to confidently-resolved edges
/// (the `calls` edge). Drives the lock-order analyzer's interprocedural
/// inlining and witness-path reconstruction.
pub async fn resolved_call_edges_for_project(
    pool: &PgPool,
    project_id: i32,
    min_confidence: f32,
) -> Result<Vec<(i64, i64, i32)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, i64, i32)>(
        "SELECT sr.source_symbol_id, sr.target_symbol_id, sr.source_line
         FROM symbol_references sr
         JOIN indexed_files f ON f.id = sr.source_file_id
         WHERE f.project_id = $1
           AND sr.source_symbol_id IS NOT NULL
           AND sr.target_symbol_id IS NOT NULL
           AND sr.resolution_confidence >= $2",
    )
    .bind(project_id)
    .bind(min_confidence)
    .fetch_all(pool)
    .await
}

/// Symbol metadata for witness rendering (name / scope / kind / file /
/// visibility), keyed by `file_symbols.id`.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct SymbolMeta {
    pub id: i64,
    pub name: String,
    pub scope_path: Option<String>,
    pub kind: String,
    pub relative_path: String,
    pub visibility: Option<String>,
}

/// Fetch [`SymbolMeta`] for a set of symbol ids (witness reconstruction).
pub async fn symbol_meta_for_ids(
    pool: &PgPool,
    ids: &[i64],
) -> Result<Vec<SymbolMeta>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, SymbolMeta>(
        "SELECT fs.id, fs.name, fs.scope_path, fs.kind, f.relative_path, fs.visibility
         FROM file_symbols fs
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE fs.id = ANY($1::int8[])",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
}

/// True iff the project has sync-op-capable files (Rust/Rholang in v1) AND the
/// one-time `sync_ops` backfill has not yet run for it. Gated by a
/// `pgmcp_metadata` flag (not a row count) so concurrency-free projects — which
/// legitimately have zero `sync_ops` — do not force a re-scan every cron cycle.
pub async fn sync_ops_backfill_pending(
    pool: &PgPool,
    project_id: i32,
) -> Result<bool, sqlx::Error> {
    let key = format!("sync_ops_backfill:{project_id}");
    let pending: Option<bool> = sqlx::query_scalar(
        "SELECT EXISTS(
                 SELECT 1 FROM indexed_files
                 WHERE project_id = $1 AND language IN ('rust', 'rholang'))
             AND NOT EXISTS(SELECT 1 FROM pgmcp_metadata WHERE key = $2)",
    )
    .bind(project_id)
    .bind(&key)
    .fetch_optional(pool)
    .await?;
    Ok(pending.unwrap_or(false))
}

/// Mark the one-time `sync_ops` backfill done for a project (idempotent).
pub async fn mark_sync_ops_backfill_done(
    pool: &PgPool,
    project_id: i32,
) -> Result<(), sqlx::Error> {
    let key = format!("sync_ops_backfill:{project_id}");
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, 'done')
         ON CONFLICT (key) DO NOTHING",
    )
    .bind(&key)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch a single symbol's ordered skeleton (drill-down for the `sync_skeleton`
/// inspection tool and per-cycle witness reconstruction).
pub async fn sync_ops_for_symbol(
    pool: &PgPool,
    symbol_id: i64,
) -> Result<Vec<SyncOpRow>, sqlx::Error> {
    sqlx::query_as::<_, SyncOpRow>(
        "SELECT so.symbol_id, fs.file_id, f.relative_path, fs.name AS symbol_name,
                fs.scope_path, so.seq, so.op_kind, so.resource_key, so.resource_kind,
                so.paradigm, so.nesting_depth, so.guard_id, so.resource_confidence, so.line
         FROM sync_ops so
         JOIN file_symbols fs ON fs.id = so.symbol_id
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE so.symbol_id = $1
         ORDER BY so.seq",
    )
    .bind(symbol_id)
    .fetch_all(pool)
    .await
}
