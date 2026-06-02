//! Symbol-extraction queries (file enumeration/content fetch, bulk
//! symbol/parameter/effect/reference inserts, reference resolution, watermarks,
//! import + naming distribution). Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use crate::parsing::resolution_kind::ResolutionKind;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

// ============================================================================
// Tier-0e — Symbol extraction (file_symbols + symbol_references)
// ============================================================================

/// One row backing the symbol-extraction Phase A scan.
#[derive(Debug, sqlx::FromRow)]
pub struct SymbolExtractionFileMeta {
    pub file_id: i64,
    pub relative_path: String,
    pub language: String,
}

/// Phase-A metadata fetch — per-project list of files routed to a backend that exists,
/// optionally filtered by `since` watermark.
pub async fn list_files_for_symbol_extraction(
    pool: &PgPool,
    project_id: i32,
    backend_languages: &[&str],
    since: Option<DateTime<Utc>>,
) -> Result<Vec<SymbolExtractionFileMeta>, sqlx::Error> {
    let langs: Vec<String> = backend_languages.iter().map(|s| s.to_string()).collect();
    sqlx::query_as::<_, SymbolExtractionFileMeta>(
        // No `content IS NOT NULL` filter: files stored under the
        // asymmetric-storage policy (content NULL, recoverable from disk) MUST
        // be listed — Phase B recovers their text from disk. Filtering them out
        // here is what historically left ~90% of an actively-reindexed project
        // unextracted (RC2).
        "SELECT id as file_id, relative_path, language
         FROM indexed_files
         WHERE project_id = $1
           AND language = ANY($2::text[])
           AND ($3::timestamptz IS NULL OR modified_at > $3)
         ORDER BY id",
    )
    .bind(project_id)
    .bind(&langs)
    .bind(since)
    .fetch_all(pool)
    .await
}

/// Per-batch content fetch for the symbol-extraction cron's Phase B.
///
/// `content` is `NULL` for files stored under the asymmetric-storage policy
/// (`content_recoverable_from_disk = true`); the cron recovers their text from
/// disk via `db::disk_read::read_disk_verified`, keyed off `path` +
/// `content_hash`. `extracted_content_hash` records the `content_hash` at the
/// last successful extraction so an unchanged file can be skipped on a full
/// re-scan (RC2 incremental-skip).
#[derive(Debug, sqlx::FromRow)]
pub struct SymbolExtractionFileContent {
    pub file_id: i64,
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub content: Option<String>,
    pub content_recoverable_from_disk: bool,
    pub content_hash: Option<i64>,
    pub extracted_content_hash: Option<i64>,
    pub modified_at: DateTime<Utc>,
}

/// Fetch content (+ disk-fallback / incremental-skip metadata) for a batch of
/// file IDs. No `content IS NOT NULL` filter — content-NULL files are recovered
/// from disk in Phase B.
pub async fn fetch_file_content_batch(
    pool: &PgPool,
    project_id: i32,
    file_ids: &[i64],
) -> Result<Vec<SymbolExtractionFileContent>, sqlx::Error> {
    sqlx::query_as::<_, SymbolExtractionFileContent>(
        "SELECT id as file_id, path, relative_path, language, content,
                content_recoverable_from_disk, content_hash,
                extracted_content_hash, modified_at
         FROM indexed_files
         WHERE project_id = $1 AND id = ANY($2::bigint[])",
    )
    .bind(project_id)
    .bind(file_ids)
    .fetch_all(pool)
    .await
}

/// Record the `content_hash` that was just successfully extracted, so a later
/// full re-scan can skip the file while its content is unchanged (RC2
/// incremental-skip). Idempotent.
pub async fn set_extracted_content_hash(
    pool: &PgPool,
    file_id: i64,
    content_hash: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE indexed_files SET extracted_content_hash = $1 WHERE id = $2")
        .bind(content_hash)
        .bind(file_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete all `file_symbols` rows for a file (CASCADE wipes children + dependent
/// `symbol_references` via the FK on `source_symbol_id`/`target_symbol_id`).
pub async fn delete_symbols_for_file(pool: &PgPool, file_id: i64) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("DELETE FROM file_symbols WHERE file_id = $1")
        .bind(file_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Delete all `symbol_references` rows whose source is the given file.
pub async fn delete_symbol_references_for_file(
    pool: &PgPool,
    source_file_id: i64,
) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("DELETE FROM symbol_references WHERE source_file_id = $1")
        .bind(source_file_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Bulk-insert symbols for a file via UNNEST. Caller must dedupe by
/// `(file_id, kind, name, start_line)` before invoking. Returns the inserted
/// row IDs **in input order**, so the cron can resolve `parent_id` (impl-method
/// → struct) by joining names within the same file.
///
/// On UNIQUE conflict (which should not happen if the caller deletes existing
/// rows first), `DO UPDATE` updates the metadata fields and returns the existing
/// id — preserving the input-order invariant.
pub async fn bulk_insert_file_symbols(
    pool: &PgPool,
    file_id: i64,
    symbols: &[crate::parsing::symbols::Symbol],
) -> Result<Vec<i64>, sqlx::Error> {
    if symbols.is_empty() {
        return Ok(Vec::new());
    }

    let names: Vec<String> = symbols.iter().map(|s| s.name.clone()).collect();
    let kinds: Vec<String> = symbols
        .iter()
        .map(|s| s.kind.as_db_str().to_string())
        .collect();
    let start_lines: Vec<i32> = symbols.iter().map(|s| s.start_line as i32).collect();
    let end_lines: Vec<i32> = symbols.iter().map(|s| s.end_line as i32).collect();
    let visibilities: Vec<Option<String>> = symbols.iter().map(|s| s.visibility.clone()).collect();
    let signatures: Vec<Option<String>> = symbols.iter().map(|s| s.signature.clone()).collect();

    // Shadow-ASR fields. Defaulted to None / empty arrays so backends that
    // haven't been upgraded yet still produce well-typed inputs.
    let return_type_raws: Vec<Option<String>> = symbols
        .iter()
        .map(|s| s.return_type.as_ref().and_then(|rt| rt.type_raw.clone()))
        .collect();
    // Per-symbol return_type_tags as JSON arrays. Postgres `text[][]` would
    // require ragged-array support that sqlx doesn't ship, so we wrap each
    // symbol's tag list in a JSONB scalar and expand it server-side.
    let return_type_tags_json: Vec<serde_json::Value> = symbols
        .iter()
        .map(|s| {
            let tags = s
                .return_type
                .as_ref()
                .map(|rt| rt.type_tags.clone())
                .unwrap_or_default();
            serde_json::Value::Array(tags.into_iter().map(serde_json::Value::String).collect())
        })
        .collect();
    let return_type_shapes: Vec<Option<serde_json::Value>> = symbols
        .iter()
        .map(|s| {
            s.return_type
                .as_ref()
                .and_then(|rt| rt.type_shape.as_ref())
                .and_then(|sh| serde_json::to_value(sh).ok())
        })
        .collect();
    let generic_params: Vec<Option<serde_json::Value>> = symbols
        .iter()
        .map(|s| {
            if s.generic_params.is_empty() {
                None
            } else {
                serde_json::to_value(&s.generic_params).ok()
            }
        })
        .collect();
    let scope_paths: Vec<Option<String>> = symbols.iter().map(|s| s.scope_path.clone()).collect();
    let scope_depths: Vec<Option<i32>> = symbols
        .iter()
        .map(|s| s.scope_depth.map(|d| d as i32))
        .collect();

    // Generate a per-batch ordinal so RETURNING comes back in input order
    // even when ON CONFLICT DO UPDATE fires.
    let ordinals: Vec<i32> = (0..symbols.len() as i32).collect();

    let rows: Vec<(i32, i64)> = sqlx::query_as::<_, (i32, i64)>(
        "WITH input AS (
             SELECT u.*,
                    COALESCE(
                        ARRAY(SELECT jsonb_array_elements_text(u.return_type_tags_json)),
                        '{}'::text[]
                    ) AS return_type_tags
             FROM UNNEST(
                 $1::int4[], $2::int8[], $3::text[], $4::text[],
                 $5::int4[], $6::int4[], $7::text[], $8::text[],
                 $9::text[], $10::jsonb[], $11::jsonb[], $12::jsonb[],
                 $13::text[], $14::int4[]
             ) AS u(
                 ord, file_id, name, kind, start_line, end_line, visibility, signature,
                 return_type_raw, return_type_tags_json, return_type_shape, generic_params,
                 scope_path, scope_depth
             )
         ),
         inserted AS (
             INSERT INTO file_symbols (
                 file_id, name, kind, start_line, end_line, visibility, signature,
                 return_type_raw, return_type_tags, return_type_shape, generic_params,
                 scope_path, scope_depth
             )
             SELECT file_id, name, kind, start_line, end_line, visibility, signature,
                    return_type_raw, return_type_tags, return_type_shape, generic_params,
                    scope_path, scope_depth
             FROM input
             ON CONFLICT (file_id, kind, name, start_line) DO UPDATE SET
                 end_line = EXCLUDED.end_line,
                 visibility = EXCLUDED.visibility,
                 signature = EXCLUDED.signature,
                 return_type_raw = EXCLUDED.return_type_raw,
                 return_type_tags = EXCLUDED.return_type_tags,
                 return_type_shape = EXCLUDED.return_type_shape,
                 generic_params = EXCLUDED.generic_params,
                 scope_path = EXCLUDED.scope_path,
                 scope_depth = EXCLUDED.scope_depth
             RETURNING id, file_id, kind, name, start_line
         )
         SELECT input.ord, inserted.id
         FROM input
         JOIN inserted USING (file_id, kind, name, start_line)
         ORDER BY input.ord",
    )
    .bind(&ordinals)
    .bind(vec![file_id; symbols.len()])
    .bind(&names)
    .bind(&kinds)
    .bind(&start_lines)
    .bind(&end_lines)
    .bind(&visibilities)
    .bind(&signatures)
    .bind(&return_type_raws)
    .bind(&return_type_tags_json)
    .bind(&return_type_shapes)
    .bind(&generic_params)
    .bind(&scope_paths)
    .bind(&scope_depths)
    .fetch_all(pool)
    .await?;

    let mut ids: Vec<i64> = vec![0i64; symbols.len()];
    for (ord, id) in rows {
        if let Some(slot) = ids.get_mut(ord as usize) {
            *slot = id;
        }
    }
    Ok(ids)
}

/// Bulk-insert the structured parameter rows that go with each symbol.
/// `symbol_ids` must align 1:1 with `symbols` (typically what
/// `bulk_insert_file_symbols` returned). Existing rows for a given
/// `symbol_id` are deleted first so re-runs replace the parameter set
/// rather than accumulating duplicates.
pub async fn bulk_insert_symbol_parameters(
    pool: &PgPool,
    symbol_ids: &[i64],
    symbols: &[crate::parsing::symbols::Symbol],
) -> Result<u64, sqlx::Error> {
    debug_assert_eq!(symbol_ids.len(), symbols.len());
    if symbols.is_empty() {
        return Ok(0);
    }

    // Flatten (symbol_id, parameter) pairs into column-oriented vecs.
    // type_tags is the only ragged column — encode as JSONB and expand
    // server-side via `jsonb_array_elements_text`.
    let mut sids: Vec<i64> = Vec::new();
    let mut positions: Vec<i32> = Vec::new();
    let mut names: Vec<Option<String>> = Vec::new();
    let mut type_raws: Vec<Option<String>> = Vec::new();
    let mut type_tags_json: Vec<serde_json::Value> = Vec::new();
    let mut type_shapes: Vec<Option<serde_json::Value>> = Vec::new();
    let mut default_values: Vec<Option<String>> = Vec::new();
    let mut modifiers: Vec<Option<String>> = Vec::new();
    let mut is_variadics: Vec<bool> = Vec::new();
    let mut is_selfs: Vec<bool> = Vec::new();
    let mut affected_sids: Vec<i64> = Vec::new();

    for (sid, sym) in symbol_ids.iter().zip(symbols.iter()) {
        if !sym.parameters.is_empty() {
            affected_sids.push(*sid);
        }
        for p in &sym.parameters {
            sids.push(*sid);
            positions.push(p.position as i32);
            names.push(p.name.clone());
            type_raws.push(p.type_raw.clone());
            type_tags_json.push(serde_json::Value::Array(
                p.type_tags
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ));
            type_shapes.push(
                p.type_shape
                    .as_ref()
                    .and_then(|sh| serde_json::to_value(sh).ok()),
            );
            default_values.push(p.default_value.clone());
            modifiers.push(p.modifier.map(|m| m.as_db_str().to_string()));
            is_variadics.push(p.is_variadic);
            is_selfs.push(p.is_self);
        }
    }

    let mut tx = pool.begin().await?;

    // Replace semantics: clear out the existing parameters for the symbols
    // we're about to write, so a backend re-run that produces a different
    // signature shape doesn't leave orphan rows from the previous run.
    if !affected_sids.is_empty() {
        sqlx::query("DELETE FROM symbol_parameters WHERE symbol_id = ANY($1::int8[])")
            .bind(&affected_sids)
            .execute(&mut *tx)
            .await?;
    }

    if !sids.is_empty() {
        sqlx::query(
            "INSERT INTO symbol_parameters (
                 symbol_id, position, name, type_raw, type_tags, type_shape,
                 default_value, modifier, is_variadic, is_self
             )
             SELECT
                 symbol_id, position, name, type_raw,
                 COALESCE(
                     ARRAY(SELECT jsonb_array_elements_text(type_tags_json)),
                     '{}'::text[]
                 ) AS type_tags,
                 type_shape,
                 default_value, modifier, is_variadic, is_self
             FROM UNNEST(
                 $1::int8[], $2::int4[], $3::text[], $4::text[],
                 $5::jsonb[], $6::jsonb[],
                 $7::text[], $8::text[], $9::bool[], $10::bool[]
             ) AS u(
                 symbol_id, position, name, type_raw,
                 type_tags_json, type_shape,
                 default_value, modifier, is_variadic, is_self
             )",
        )
        .bind(&sids)
        .bind(&positions)
        .bind(&names)
        .bind(&type_raws)
        .bind(&type_tags_json)
        .bind(&type_shapes)
        .bind(&default_values)
        .bind(&modifiers)
        .bind(&is_variadics)
        .bind(&is_selfs)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(sids.len() as u64)
}

/// Bulk-insert the effect membership rows for each symbol. Replace
/// semantics, same as `bulk_insert_symbol_parameters`: existing rows for
/// each `symbol_id` are deleted before insert. The effect names must
/// exist in `effect_catalog` (enforced by the FK).
pub async fn bulk_insert_symbol_effects(
    pool: &PgPool,
    symbol_ids: &[i64],
    symbols: &[crate::parsing::symbols::Symbol],
) -> Result<u64, sqlx::Error> {
    debug_assert_eq!(symbol_ids.len(), symbols.len());
    if symbols.is_empty() {
        return Ok(0);
    }

    let mut sids: Vec<i64> = Vec::new();
    let mut effects: Vec<String> = Vec::new();
    let mut affected_sids: Vec<i64> = Vec::new();

    for (sid, sym) in symbol_ids.iter().zip(symbols.iter()) {
        if !sym.effects.is_empty() {
            affected_sids.push(*sid);
        }
        for eff in &sym.effects {
            sids.push(*sid);
            effects.push(eff.clone());
        }
    }

    let mut tx = pool.begin().await?;

    if !affected_sids.is_empty() {
        sqlx::query("DELETE FROM symbol_effects WHERE symbol_id = ANY($1::int8[])")
            .bind(&affected_sids)
            .execute(&mut *tx)
            .await?;
    }

    if !sids.is_empty() {
        sqlx::query(
            "INSERT INTO symbol_effects (symbol_id, effect)
             SELECT * FROM UNNEST($1::int8[], $2::text[])
             ON CONFLICT (symbol_id, effect) DO NOTHING",
        )
        .bind(&sids)
        .bind(&effects)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(sids.len() as u64)
}

// ============================================================================
// Temporal effect-drift ledger (v15 symbol_effect_history)
// ============================================================================

/// Current effect sets for a file, keyed by the stable `(symbol_kind,
/// symbol_name)` identity the drift ledger uses (line numbers move; kind+name
/// is what a human means by "this function"). Read BEFORE the symbol-extraction
/// cron rewrites a file's `symbol_effects`, so the cron can diff it against the
/// freshly-extracted set and record `gained` / `lost` transitions.
pub async fn effect_sets_for_file(
    pool: &PgPool,
    file_id: i64,
) -> Result<
    std::collections::HashMap<(String, String), std::collections::HashSet<String>>,
    sqlx::Error,
> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT fs.kind, fs.name, se.effect
         FROM file_symbols fs
         JOIN symbol_effects se ON se.symbol_id = fs.id
         WHERE fs.file_id = $1",
    )
    .bind(file_id)
    .fetch_all(pool)
    .await?;
    let mut map: std::collections::HashMap<(String, String), std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for (kind, name, effect) in rows {
        map.entry((kind, name)).or_default().insert(effect);
    }
    Ok(map)
}

/// Append `gained` / `lost` effect-drift rows for a file. Each tuple is
/// `(symbol_kind, symbol_name, effect, change)` where `change` is `"gained"` or
/// `"lost"`. Append-only — never updates or deletes existing history.
pub async fn record_effect_drift(
    pool: &PgPool,
    file_id: i64,
    rows: &[(String, String, String, &'static str)],
) -> Result<u64, sqlx::Error> {
    if rows.is_empty() {
        return Ok(0);
    }
    let kinds: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
    let names: Vec<String> = rows.iter().map(|r| r.1.clone()).collect();
    let effects: Vec<String> = rows.iter().map(|r| r.2.clone()).collect();
    let changes: Vec<String> = rows.iter().map(|r| r.3.to_string()).collect();
    let res = sqlx::query(
        "INSERT INTO symbol_effect_history
             (file_id, symbol_kind, symbol_name, effect, change)
         SELECT $1, u.kind, u.name, u.effect, u.change
         FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[])
              AS u(kind, name, effect, change)",
    )
    .bind(file_id)
    .bind(&kinds)
    .bind(&names)
    .bind(&effects)
    .bind(&changes)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// One row of the effect-drift ledger, enriched with project + path. Returned
/// by [`query_effect_drift`] for the `effect_drift` MCP tool.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct EffectDriftRow {
    pub relative_path: String,
    pub project_name: String,
    pub symbol_kind: String,
    pub symbol_name: String,
    pub effect: String,
    pub change: String,
    pub observed_at: DateTime<Utc>,
}

/// Query the effect-drift ledger newest-first, with optional project / effect /
/// change (`gained`|`lost`) / recency filters.
pub async fn query_effect_drift(
    pool: &PgPool,
    project: Option<&str>,
    effect: Option<&str>,
    change: Option<&str>,
    since: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<EffectDriftRow>, sqlx::Error> {
    sqlx::query_as::<_, EffectDriftRow>(
        "SELECT f.relative_path, p.name AS project_name,
                h.symbol_kind, h.symbol_name, h.effect, h.change, h.observed_at
         FROM symbol_effect_history h
         JOIN indexed_files f ON f.id = h.file_id
         JOIN projects p ON p.id = f.project_id
         WHERE ($1::text IS NULL OR p.name = $1)
           AND ($2::text IS NULL OR h.effect = $2)
           AND ($3::text IS NULL OR h.change = $3)
           AND ($4::timestamptz IS NULL OR h.observed_at >= $4)
         ORDER BY h.observed_at DESC, h.id DESC
         LIMIT $5",
    )
    .bind(project)
    .bind(effect)
    .bind(change)
    .bind(since)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Apply resolution metadata to existing `symbol_references` rows. Pairs
/// align with rows by `(source_file_id, source_line, target_raw,
/// ref_kind)` — the same composite key the cron uses to identify them.
///
/// Each entry is `(source_file_id, source_line, target_raw, ref_kind,
/// target_path, resolution_kind, resolution_confidence)`. Rows that don't
/// match silently no-op (typical when reindex deleted them mid-run).
#[allow(clippy::type_complexity)]
pub async fn update_symbol_reference_resolutions(
    pool: &PgPool,
    rows: &[(i64, u32, String, String, Option<String>, String, f32)],
) -> Result<u64, sqlx::Error> {
    if rows.is_empty() {
        return Ok(0);
    }
    let source_files: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let source_lines: Vec<i32> = rows.iter().map(|r| r.1 as i32).collect();
    let target_raws: Vec<String> = rows.iter().map(|r| r.2.clone()).collect();
    let ref_kinds: Vec<String> = rows.iter().map(|r| r.3.clone()).collect();
    let target_paths: Vec<Option<String>> = rows.iter().map(|r| r.4.clone()).collect();
    let resolution_kinds: Vec<String> = rows.iter().map(|r| r.5.clone()).collect();
    let confidences: Vec<f32> = rows.iter().map(|r| r.6).collect();
    let res = sqlx::query(
        "UPDATE symbol_references sr
         SET target_path = u.target_path,
             resolution_kind = u.resolution_kind,
             resolution_confidence = u.resolution_confidence
         FROM UNNEST(
             $1::int8[], $2::int4[], $3::text[], $4::text[],
             $5::text[], $6::text[], $7::real[]
         ) AS u(source_file_id, source_line, target_raw, ref_kind,
                target_path, resolution_kind, resolution_confidence)
         WHERE sr.source_file_id = u.source_file_id
           AND sr.source_line = u.source_line
           AND sr.target_raw = u.target_raw
           AND sr.ref_kind = u.ref_kind",
    )
    .bind(&source_files)
    .bind(&source_lines)
    .bind(&target_raws)
    .bind(&ref_kinds)
    .bind(&target_paths)
    .bind(&resolution_kinds)
    .bind(&confidences)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Apply resolved `parent_id` values for a file's symbols. The cron computes
/// `parent_id` by name+line-range matching in Rust; this helper writes them
/// back in one round-trip.
pub async fn update_symbol_parent_ids(
    pool: &PgPool,
    pairs: &[(i64, i64)], // (child_id, parent_id)
) -> Result<u64, sqlx::Error> {
    if pairs.is_empty() {
        return Ok(0);
    }
    let child_ids: Vec<i64> = pairs.iter().map(|(c, _)| *c).collect();
    let parent_ids: Vec<i64> = pairs.iter().map(|(_, p)| *p).collect();
    let res = sqlx::query(
        "UPDATE file_symbols
         SET parent_id = u.parent_id
         FROM UNNEST($1::int8[], $2::int8[]) AS u(child_id, parent_id)
         WHERE file_symbols.id = u.child_id",
    )
    .bind(&child_ids)
    .bind(&parent_ids)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Bulk-insert symbol references for a file via UNNEST. Caller must dedupe by
/// `(source_line, target_raw, ref_kind)` before invoking. ON CONFLICT DO NOTHING
/// — duplicate rows from re-runs are silently dropped.
pub async fn bulk_insert_symbol_references(
    pool: &PgPool,
    source_file_id: i64,
    refs: &[crate::parsing::symbols::SymbolReference],
) -> Result<u64, sqlx::Error> {
    if refs.is_empty() {
        return Ok(0);
    }

    let source_files: Vec<i64> = vec![source_file_id; refs.len()];
    let source_symbols: Vec<Option<i64>> = refs.iter().map(|r| r.source_symbol_id).collect();
    let target_files: Vec<Option<i64>> = refs.iter().map(|r| r.target_file_id).collect();
    let target_symbols: Vec<Option<i64>> = refs.iter().map(|r| r.target_symbol_id).collect();
    let target_raws: Vec<String> = refs.iter().map(|r| r.target_raw.clone()).collect();
    let ref_kinds: Vec<String> = refs
        .iter()
        .map(|r| r.ref_kind.as_db_str().to_string())
        .collect();
    let source_lines: Vec<i32> = refs.iter().map(|r| r.source_line as i32).collect();

    let res = sqlx::query(
        "INSERT INTO symbol_references (
             source_file_id, source_symbol_id, target_file_id, target_symbol_id,
             target_raw, ref_kind, source_line
         )
         SELECT * FROM UNNEST(
             $1::int8[], $2::int8[], $3::int8[], $4::int8[],
             $5::text[], $6::text[], $7::int4[]
         )
         ON CONFLICT (source_file_id, source_line, target_raw, ref_kind) DO NOTHING",
    )
    .bind(&source_files)
    .bind(&source_symbols)
    .bind(&target_files)
    .bind(&target_symbols)
    .bind(&target_raws)
    .bind(&ref_kinds)
    .bind(&source_lines)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Run one resolution phase in its own short transaction.
///
/// Splitting the four-phase walk into one transaction *per phase* (rather than a
/// single transaction spanning all four) makes partial progress durable: if a
/// later phase still hits the per-statement timeout, the phases that already
/// committed are not rolled back, so the project converges over successive cron
/// runs instead of looping forever. This is safe because every phase only
/// updates rows with `resolution_kind IS NULL` and marks them non-NULL, so a
/// committed phase is never re-touched by a subsequent one. The `'300s'`
/// `SET LOCAL` lifts the 30s pool default (`DatabaseConfig::statement_timeout_ms`)
/// for this heavy, project-wide UPDATE — it reverts when the connection returns
/// to the pool — and the `application_name` labels the backend for the
/// graceful-shutdown sweep (`db::admin::terminate_heavy_backends`). See
/// `~/.claude/plans/pgmcp-has-not-logged-structured-sprout.md`.
async fn run_resolution_phase(
    pool: &PgPool,
    project_id: i32,
    sql: &str,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '300s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL application_name = 'pgmcp:heavy:symbol-extraction'")
        .execute(&mut *tx)
        .await?;
    let res = sqlx::query(sql).bind(project_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(res.rows_affected())
}

/// Per-project second pass — resolve `target_symbol_id` and `target_file_id`
/// for any unresolved `symbol_references` rows by joining `target_raw` against
/// `file_symbols.name` within the project. Multi-match by name picks one
/// arbitrarily; the confidence score in downstream tools accounts for this.
pub async fn resolve_symbol_reference_targets(
    pool: &PgPool,
    project_id: i32,
) -> Result<u64, sqlx::Error> {
    // Resolution pass v2: a four-phase walk that populates not only
    // `target_symbol_id` (legacy) but also `target_path`,
    // `resolution_kind`, and `resolution_confidence`. The phases are
    // ordered by precision so each phase only touches rows the earlier
    // ones couldn't resolve. Every tier string + confidence is sourced from
    // the closed `ResolutionKind` enum (see `src/parsing/resolution_kind.rs`)
    // so the writer and the `chk_symbol_refs_resolution_kind` CHECK
    // (`v14_resolution_kind_vocab`) cannot drift — the exact failure that
    // previously rolled this whole transaction back.
    //
    //   1. exact_in_file        — name matches a symbol in the same file
    //                            (confidence 1.0).
    //   2. exact_via_import     — name matches a symbol reachable through an
    //                            `import` edge within the project (0.95).
    //   3a. bare_name_unique    — exactly one project-wide same-name candidate
    //                            (0.7); 3b. bare_name_ambiguous — multiple
    //                            candidates, the DB picks one but it's an
    //                            unreliable guess (0.3).
    //   4. unresolved           — final mark for everything else
    //                            (confidence 0.0, target_symbol_id NULL).
    //
    // Each UPDATE is gated by `resolution_kind IS NULL` so the earlier-tier
    // assignments stick even when a later phase would also match.

    // Cheap backlog guard: if the project has no unresolved references, skip the
    // whole pass — including phase 3's project-wide aggregation. This makes it
    // safe (and near-free, via the partial `idx_symbol_refs_unresolved` index
    // from v20) to call resolution unconditionally — e.g. on a symbol-extraction
    // cron cycle that found no new files but must still DRAIN a backlog stranded
    // by an earlier interrupted pass. Without the guard, every such call would
    // rebuild phase 3's `proj_counts` CTE for nothing.
    let has_unresolved: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM symbol_references sr
             JOIN indexed_files f ON f.id = sr.source_file_id
             WHERE f.project_id = $1 AND sr.resolution_kind IS NULL
         )",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await?;
    if !has_unresolved {
        return Ok(0);
    }

    // Each phase below runs in its OWN short transaction via
    // `run_resolution_phase`, which lifts the per-statement timeout to 300s and
    // commits independently. Per-phase commits make partial progress durable: a
    // later phase that still hits the timeout no longer rolls back the phases
    // that already succeeded — the permanent-failure loop that 300s-cancelled
    // the bare-name phase on large projects ("Symbol extraction failed for
    // project") and `?`-propagated out of `extract_project_symbols`.

    // Phase 1: exact_in_file. Same source file, same name. Tier string +
    // confidence come from the closed `ResolutionKind` enum so the values
    // written here cannot drift from the `chk_symbol_refs_resolution_kind`
    // CHECK (built from the same enum in `v14_resolution_kind_vocab`).
    let phase1_sql = format!(
        "UPDATE symbol_references sr
         SET target_symbol_id = fs.id,
             target_file_id = fs.file_id,
             target_path = fs.scope_path,
             resolution_kind = '{kind}',
             resolution_confidence = {conf}
         FROM file_symbols fs
         WHERE fs.file_id = sr.source_file_id
           AND sr.target_raw = fs.name
           AND sr.resolution_kind IS NULL
           AND EXISTS (
               SELECT 1 FROM indexed_files f
                WHERE f.id = sr.source_file_id AND f.project_id = $1
           )",
        kind = ResolutionKind::ExactInFile.as_db_str(),
        conf = ResolutionKind::ExactInFile.confidence(),
    );
    let phase1 = run_resolution_phase(pool, project_id, &phase1_sql).await?;

    // Phase 2: exact_via_import. The reference's source file imports a
    // module/symbol whose `target_raw` ends with `::<name>` (or `.<name>`
    // for languages using dot-notation). Match against `scope_path` so the
    // resolution is namespace-aware.
    //
    // The UPDATE target alias `sr` is in scope ONLY for SET/WHERE/RETURNING
    // — Postgres rejects references to `sr` inside `JOIN ... ON` predicates
    // between FROM-list members with `invalid reference to FROM-clause
    // entry for table "sr"`. The `e.source_file_id = sr.source_file_id`
    // correlation belongs in WHERE, not in the JOIN ON. See plan
    // ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md F2.
    let phase2_sql = format!(
        "UPDATE symbol_references sr
         SET target_symbol_id = fs.id,
             target_file_id = fs.file_id,
             target_path = fs.scope_path,
             resolution_kind = '{kind}',
             resolution_confidence = {conf}
         FROM file_symbols fs
         JOIN indexed_files tgt_f ON tgt_f.id = fs.file_id
         JOIN code_graph_edges e
           ON e.target_file_id = fs.file_id
          AND e.edge_type = 'import'
         WHERE sr.target_raw = fs.name
           AND sr.resolution_kind IS NULL
           AND tgt_f.project_id = $1
           AND e.source_file_id = sr.source_file_id
           AND EXISTS (
               SELECT 1 FROM indexed_files f
                WHERE f.id = sr.source_file_id AND f.project_id = $1
           )",
        kind = ResolutionKind::ExactViaImport.as_db_str(),
        conf = ResolutionKind::ExactViaImport.confidence(),
    );
    let phase2 = run_resolution_phase(pool, project_id, &phase2_sql).await?;

    // Phase 3: bare-name match within the project, now CONFIDENCE-GRADED by
    // ambiguity (graph-roadmap Phase 4.1). The legacy single 0.5 tier matched
    // ANY same-name symbol and picked one arbitrarily, fabricating edges that
    // distort centrality/communities. We now split by candidate count:
    //   - `bare_name_unique`    (exactly one project-wide candidate) — the
    //                            match is almost certainly right → confidence 0.7.
    //   - `bare_name_ambiguous` (multiple candidates) — the DB still picks one,
    //                            but it's an unreliable guess → confidence 0.3,
    //                            so downstream call-graph weighting and tool
    //                            `min_confidence` filters can discount it.
    // (Full receiver-type resolution — resolving `recv.method()` against the
    // receiver's inferred type — is the per-language extractor follow-up; it
    // needs a `receiver_type` the symbol extractors don't yet emit.)
    // De-correlated: a single `proj_counts` pre-aggregation replaces the original
    // per-row correlated `COUNT(*)` (one project-wide count per *unresolved ref*),
    // which on large projects (`default`, `Documents`) blew the 300s
    // statement_timeout and rolled the whole pass back every run. The grouped CTE
    // is one pass over the project's symbols (O(symbols)); the UPDATE then
    // hash-joins refs → candidates (O(refs)). Semantics are unchanged:
    // `n_cand = 1` → `bare_name_unique`, else `bare_name_ambiguous`, and a
    // multi-candidate name still resolves to an arbitrary same-name symbol.
    //
    // `proj_counts pc` is a FROM-list member correlated to the UPDATE target in
    // WHERE (`pc.nm = sr.target_raw`), NOT in a JOIN ... ON — Postgres rejects
    // `sr` references inside FROM-list JOIN predicates (the same trap noted in
    // phase 2). The source-in-project guard uses the EXISTS idiom from phase 1.
    let phase3_sql = format!(
        "WITH proj_counts AS (
             SELECT fs2.name AS nm, COUNT(*) AS n_cand
             FROM file_symbols fs2
             JOIN indexed_files tf2 ON tf2.id = fs2.file_id
             WHERE tf2.project_id = $1
             GROUP BY fs2.name
         )
         UPDATE symbol_references sr
         SET target_symbol_id = fs.id,
             target_file_id = fs.file_id,
             target_path = fs.scope_path,
             resolution_kind = CASE WHEN pc.n_cand = 1
                                    THEN '{uniq}'
                                    ELSE '{ambig}' END,
             resolution_confidence = CASE WHEN pc.n_cand = 1 THEN {uniq_conf} ELSE {ambig_conf} END
         FROM file_symbols fs
         JOIN indexed_files tgt_f ON tgt_f.id = fs.file_id,
              proj_counts pc
         WHERE tgt_f.project_id = $1
           AND pc.nm = sr.target_raw
           AND sr.target_raw = fs.name
           AND sr.resolution_kind IS NULL
           AND EXISTS (
               SELECT 1 FROM indexed_files f
                WHERE f.id = sr.source_file_id AND f.project_id = $1
           )",
        uniq = ResolutionKind::BareNameUnique.as_db_str(),
        ambig = ResolutionKind::BareNameAmbiguous.as_db_str(),
        uniq_conf = ResolutionKind::BareNameUnique.confidence(),
        ambig_conf = ResolutionKind::BareNameAmbiguous.confidence(),
    );
    let phase3 = run_resolution_phase(pool, project_id, &phase3_sql).await?;

    // Phase 4: anything still unresolved within the project's references is
    // marked `unresolved` so tools can distinguish "we tried" from "not yet
    // processed".
    let phase4_sql = format!(
        "UPDATE symbol_references sr
         SET resolution_kind = '{kind}',
             resolution_confidence = {conf}
         FROM indexed_files f
         WHERE sr.source_file_id = f.id
           AND f.project_id = $1
           AND sr.resolution_kind IS NULL",
        kind = ResolutionKind::Unresolved.as_db_str(),
        conf = ResolutionKind::Unresolved.confidence(),
    );
    let phase4 = run_resolution_phase(pool, project_id, &phase4_sql).await?;

    Ok(phase1 + phase2 + phase3 + phase4)
}

/// Read the symbol-extraction watermark for a project.
pub async fn get_symbol_extraction_watermark(
    pool: &PgPool,
    project_id: i32,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let key = format!("symbol_extraction_last_run:{}", project_id);
    let val: Option<String> = sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = $1")
        .bind(&key)
        .fetch_optional(pool)
        .await?;
    Ok(val.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }))
}

/// True when this project has source files in a backend language yet zero
/// `import_use` rows in `symbol_references` — the signature of the
/// "advance-on-empty watermark" trap (or a pre-fix extraction that dropped
/// imports). A normal incremental run would skip the project forever because
/// the watermark is already set, so callers force a full re-scan
/// (watermark = None) when this returns true and the import graph self-heals
/// without a manual `pgmcp_metadata` reset. Two short-circuiting EXISTS scans.
pub async fn project_missing_import_refs(
    pool: &PgPool,
    project_id: i32,
    languages: &[&str],
) -> Result<bool, sqlx::Error> {
    let langs: Vec<String> = languages.iter().map(|s| s.to_string()).collect();
    let (needs,): (bool,) = sqlx::query_as::<_, (bool,)>(
        "SELECT EXISTS(
                 SELECT 1 FROM indexed_files
                 WHERE project_id = $1 AND language = ANY($2::text[])
             ) AND NOT EXISTS(
                 SELECT 1 FROM symbol_references sr
                 JOIN indexed_files f ON f.id = sr.source_file_id
                 WHERE f.project_id = $1 AND sr.ref_kind = 'import_use'
             )",
    )
    .bind(project_id)
    .bind(&langs)
    .fetch_one(pool)
    .await?;
    Ok(needs)
}

/// Set the symbol-extraction watermark for a project.
pub async fn set_symbol_extraction_watermark(
    pool: &PgPool,
    project_id: i32,
    ts: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let key = format!("symbol_extraction_last_run:{}", project_id);
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(&key)
    .bind(ts.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

/// One symbol-derived import edge for the graph-analysis migration.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ImportFromSymbols {
    pub source_file_id: i64,
    pub target_raw: String,
    pub target_file_id: Option<i64>,
    pub source_line: i32,
}

/// Fetch all `import_use` symbol-references for a project's files. Used by
/// `graph_analysis::analyze_project` to materialize import edges without
/// re-parsing file content (the symbol-extraction cron has already run).
pub async fn get_imports_from_symbols(
    pool: &PgPool,
    project_id: i32,
    file_ids: &[i64],
) -> Result<Vec<ImportFromSymbols>, sqlx::Error> {
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, ImportFromSymbols>(
        "SELECT sr.source_file_id,
                sr.target_raw,
                sr.target_file_id,
                sr.source_line
         FROM symbol_references sr
         JOIN indexed_files f ON f.id = sr.source_file_id
         WHERE f.project_id = $1
           AND sr.source_file_id = ANY($2::bigint[])
           AND sr.ref_kind = 'import_use'",
    )
    .bind(project_id)
    .bind(file_ids)
    .fetch_all(pool)
    .await
}

/// Return the subset of `file_ids` that have at least one row in
/// `symbol_references`. Used by graph_analysis to decide which files take
/// the symbol-aware path vs the regex fallback.
pub async fn file_ids_with_symbol_refs(
    pool: &PgPool,
    project_id: i32,
    file_ids: &[i64],
) -> Result<std::collections::HashSet<i64>, sqlx::Error> {
    if file_ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let rows: Vec<(i64,)> = sqlx::query_as::<_, (i64,)>(
        "SELECT DISTINCT sr.source_file_id
         FROM symbol_references sr
         JOIN indexed_files f ON f.id = sr.source_file_id
         WHERE f.project_id = $1
           AND sr.source_file_id = ANY($2::bigint[])",
    )
    .bind(project_id)
    .bind(file_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// One row of the per-project naming distribution: a symbol's name + kind +
/// containing file path. Consumed by `tool_naming_consistency` for in-Rust
/// per-(directory, kind) convention dominance analysis.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NamingDistributionRow {
    pub symbol_name: String,
    pub kind: String,
    pub file_id: i64,
    pub relative_path: String,
    pub start_line: i32,
    pub language: String,
}

/// Fetch all symbol names + kinds for a project (optionally filtered by language).
/// Sorted by `(relative_path, start_line)` so the consumer's directory-grouping
/// stays stable across runs.
pub async fn get_naming_distribution(
    pool: &PgPool,
    project_id: i32,
    language: Option<&str>,
) -> Result<Vec<NamingDistributionRow>, sqlx::Error> {
    sqlx::query_as::<_, NamingDistributionRow>(
        "SELECT fs.name as symbol_name,
                fs.kind,
                fs.file_id,
                f.relative_path,
                fs.start_line,
                f.language
         FROM file_symbols fs
         JOIN indexed_files f ON fs.file_id = f.id
         WHERE f.project_id = $1
           AND ($2::text IS NULL OR f.language = $2)
         ORDER BY f.relative_path, fs.start_line",
    )
    .bind(project_id)
    .bind(language)
    .fetch_all(pool)
    .await
}

// ============================================================================
// SOTA Phase 1 — function_metrics + call-graph queries
// ============================================================================

/// One row identifying a function symbol in a file. Returned by
/// `lookup_function_symbol_ids` so the function-metrics cron can map
/// (name, start_line) → file_symbols.id.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FunctionSymbolRow {
    pub symbol_id: i64,
    pub name: String,
    pub start_line: i32,
}

/// Look up `file_symbols.id` for every function in a file. Returned ordered
/// by `(name, start_line)` for deterministic matching by callers.
pub async fn lookup_function_symbol_ids(
    pool: &PgPool,
    file_id: i64,
) -> Result<Vec<FunctionSymbolRow>, sqlx::Error> {
    sqlx::query_as::<_, FunctionSymbolRow>(
        "SELECT id as symbol_id, name, start_line
         FROM file_symbols
         WHERE file_id = $1 AND kind = 'function'",
    )
    .bind(file_id)
    .fetch_all(pool)
    .await
}
