//! Chunk ↔ symbol overlay helpers.
//!
//! Topic / clustering tools operate on `file_chunks`. This module joins
//! chunks against `file_symbols` so each chunk can be annotated with its
//! enclosing scope_path + symbol effects + symbol kind. Enables
//! symbol-level overlays on top of chunk-level retrieval (Pattern G in
//! the unified-semantic-representation plan).

use sqlx::PgPool;

/// Per-chunk annotation: the enclosing symbol (if any) and its effects.
#[derive(Debug, Clone)]
pub struct ChunkSymbolOverlay {
    pub chunk_id: i64,
    pub file_id: i64,
    pub start_line: i32,
    pub end_line: i32,
    pub enclosing_symbol_id: Option<i64>,
    pub enclosing_symbol_name: Option<String>,
    pub enclosing_symbol_kind: Option<String>,
    pub enclosing_scope_path: Option<String>,
    pub effects: Vec<String>,
}

/// Annotate a list of chunks with their enclosing symbol + effects.
/// The "enclosing symbol" is the smallest `file_symbols` row whose line
/// range covers the chunk's start_line. When no symbol covers the
/// chunk (e.g. top-level comments, module-prelude code), the symbol
/// fields are `None` and `effects` is empty.
pub async fn overlay_chunks(
    pool: &PgPool,
    chunk_ids: &[i64],
) -> Result<Vec<ChunkSymbolOverlay>, sqlx::Error> {
    if chunk_ids.is_empty() {
        return Ok(Vec::new());
    }
    type OverlayRow = (
        i64,
        i64,
        i32,
        i32,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<OverlayRow> = sqlx::query_as(
        "WITH ranked AS (
             SELECT c.id AS chunk_id,
                    c.file_id,
                    c.start_line,
                    c.end_line,
                    fs.id AS sid,
                    fs.name AS sname,
                    fs.kind AS skind,
                    fs.scope_path AS scope,
                    (fs.end_line - fs.start_line) AS span,
                    ROW_NUMBER() OVER (
                        PARTITION BY c.id
                        ORDER BY (fs.end_line - fs.start_line) ASC
                    ) AS rn
             FROM file_chunks c
             LEFT JOIN file_symbols fs
               ON fs.file_id = c.file_id
              AND fs.start_line <= c.start_line
              AND fs.end_line >= c.start_line
             WHERE c.id = ANY($1::int8[])
         )
         SELECT chunk_id, file_id, start_line, end_line, sid, sname, skind, scope
         FROM ranked
         WHERE rn = 1
         ORDER BY chunk_id",
    )
    .bind(chunk_ids)
    .fetch_all(pool)
    .await?;

    let mut overlays: Vec<ChunkSymbolOverlay> = Vec::with_capacity(rows.len());
    for (chunk_id, file_id, start_line, end_line, sid, sname, skind, scope) in rows {
        let effects = if let Some(symbol_id) = sid {
            sqlx::query_scalar::<_, String>(
                "SELECT effect FROM symbol_effects WHERE symbol_id = $1 ORDER BY effect",
            )
            .bind(symbol_id)
            .fetch_all(pool)
            .await
            .unwrap_or_default()
        } else {
            Vec::new()
        };
        overlays.push(ChunkSymbolOverlay {
            chunk_id,
            file_id,
            start_line,
            end_line,
            enclosing_symbol_id: sid,
            enclosing_symbol_name: sname,
            enclosing_symbol_kind: skind,
            enclosing_scope_path: scope,
            effects,
        });
    }
    Ok(overlays)
}

/// Aggregate effects for a set of chunks into a single effect-count map.
/// Used by topic tools to label a topic with its dominant effect set.
pub async fn topic_effect_distribution(
    pool: &PgPool,
    chunk_ids: &[i64],
) -> Result<std::collections::HashMap<String, i64>, sqlx::Error> {
    if chunk_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT se.effect, COUNT(*)::int8
         FROM file_chunks c
         JOIN file_symbols fs
           ON fs.file_id = c.file_id
          AND fs.start_line <= c.start_line
          AND fs.end_line >= c.start_line
         JOIN symbol_effects se ON se.symbol_id = fs.id
         WHERE c.id = ANY($1::int8[])
         GROUP BY se.effect
         ORDER BY se.effect",
    )
    .bind(chunk_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}
