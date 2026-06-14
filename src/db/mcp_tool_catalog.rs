//! Queries + ingestion for the server's OWN MCP-tool catalog (`mcp_tool_catalog`,
//! v38). Distinct from [`crate::db::tool_cards`], which catalogs *external*
//! developer tools installed on the machine; this one indexes pgmcp's own
//! `tools/list` so the `tool_catalog` meta-tool can rank tools by a
//! natural-language query and a `Learned` client can `enable_tools` whatever it
//! needs beyond its default surface.
//!
//! 1024d-direct (the `tool_cards` / `durable_mandates` model): the `embedding`
//! column is filled by the embedding-migration cron, not on the seed path.
//! `upsert_tool` stamps a `content_hash` over the embedded prose (name +
//! description, signature-prefixed) and NULLs the existing vector when that hash
//! changes, so a tool description edit transparently re-embeds on the next cron
//! pass.

use pgvector::Vector;
use serde::Serialize;
use sqlx::PgPool;

use crate::db::patterns::content_hash;

/// Content-staleness key folded into `content_hash`. Bump the version suffix
/// whenever the embedded-prose composition changes in a way that should force a
/// global re-embed.
pub const MCP_TOOL_CATALOG_EMBEDDING_SIGNATURE: &str = "pgmcp-mcp-tool-embedding-v1";

/// One ranked tool from a `tool_catalog` / `enable_tools(query=…)` search.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ToolCatalogSearchRow {
    pub name: String,
    pub domain: String,
    pub description: String,
    pub score: Option<f64>,
}

/// The prose embedded for semantic discovery (mirrored by the embedding-migration
/// cron's `text_select` for `mcp_tool_catalog`).
fn embed_text(name: &str, description: &str) -> String {
    format!("{name} {description}")
}

/// Upsert one tool row. Re-embeds (NULLs the vector) only when the embedded prose
/// (name + description) actually changed, so re-seeding an unchanged catalog is a
/// no-op for the embedding cron.
pub async fn upsert_tool(
    pool: &PgPool,
    name: &str,
    domain: &str,
    description: &str,
    input_schema: &str,
) -> Result<(), sqlx::Error> {
    let hash = content_hash(&format!(
        "{MCP_TOOL_CATALOG_EMBEDDING_SIGNATURE}\n{}",
        embed_text(name, description)
    ));
    sqlx::query(
        "INSERT INTO mcp_tool_catalog (name, domain, description, input_schema, content_hash, updated_at)
         VALUES ($1, $2, $3, $4, $5, now())
         ON CONFLICT (name) DO UPDATE SET
            domain       = EXCLUDED.domain,
            description  = EXCLUDED.description,
            input_schema = EXCLUDED.input_schema,
            content_hash = EXCLUDED.content_hash,
            updated_at   = now(),
            embedding    = CASE
                WHEN mcp_tool_catalog.content_hash IS DISTINCT FROM EXCLUDED.content_hash
                THEN NULL ELSE mcp_tool_catalog.embedding END",
    )
    .bind(name)
    .bind(domain)
    .bind(description)
    .bind(input_schema)
    .bind(hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove catalog rows whose tool no longer exists (e.g. a tool was deleted
/// between releases). `keep` is the live tool-name set. Returns rows deleted.
pub async fn prune_missing(pool: &PgPool, keep: &[String]) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("DELETE FROM mcp_tool_catalog WHERE name <> ALL($1)")
        .bind(keep)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Semantic search over embedded tool prose. Returns name + description + cosine
/// score, optionally filtered to one domain.
pub async fn semantic_search(
    pool: &PgPool,
    embedding: &[f32],
    limit: i64,
    ef_search: i32,
    domain: Option<String>,
) -> Result<Vec<ToolCatalogSearchRow>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "mcp_tool_catalog::semantic_search: expected a 1024-dim BGE-M3 query embedding, got {}",
            embedding.len()
        )));
    }
    let embedding_vec = Vector::from(embedding.to_vec());
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;
    let rows = sqlx::query_as::<_, ToolCatalogSearchRow>(
        "SELECT name, domain, description, 1 - (embedding <=> $1) AS score
         FROM mcp_tool_catalog
         WHERE ($3::text IS NULL OR domain = $3)
           AND embedding IS NOT NULL
         ORDER BY embedding <=> $1
         LIMIT $2",
    )
    .bind(&embedding_vec)
    .bind(limit)
    .bind(domain)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Keyword fallback used before embeddings backfill (or when the embedder is
/// unavailable). Tokenizes the query on whitespace and matches **any** token as a
/// substring of `name`/`description`, ranking by the number of distinct tokens
/// that hit — so a multi-word natural-language query (e.g. "centrality pagerank
/// graph") still finds tools, unlike a single contiguous-substring `ILIKE` which
/// only matched a verbatim phrase. An empty query lists the catalog (optionally
/// domain-filtered), name-ordered.
pub async fn keyword_search(
    pool: &PgPool,
    query: &str,
    limit: i64,
    domain: Option<String>,
) -> Result<Vec<ToolCatalogSearchRow>, sqlx::Error> {
    // `%token%` ILIKE patterns from the whitespace-split query. An empty set (no
    // query) falls through to a plain name-ordered catalog browse.
    let patterns: Vec<String> = query
        .split_whitespace()
        .map(|tok| format!("%{tok}%"))
        .collect();
    sqlx::query_as::<_, ToolCatalogSearchRow>(
        "SELECT name, domain, description, NULL::float8 AS score
         FROM mcp_tool_catalog
         WHERE ($3::text IS NULL OR domain = $3)
           AND (
             cardinality($1::text[]) = 0
             OR EXISTS (
               SELECT 1 FROM unnest($1::text[]) AS pat
               WHERE name ILIKE pat OR description ILIKE pat
             )
           )
         ORDER BY (
             SELECT count(*) FROM unnest($1::text[]) AS pat
             WHERE name ILIKE pat OR description ILIKE pat
           ) DESC, name
         LIMIT $2",
    )
    .bind(patterns)
    .bind(limit)
    .bind(domain)
    .fetch_all(pool)
    .await
}

/// `(total, missing_embeddings)` counts for diagnostics.
pub async fn counts(pool: &PgPool) -> Result<(i64, i64), sqlx::Error> {
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcp_tool_catalog")
        .fetch_one(pool)
        .await?;
    let missing: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mcp_tool_catalog WHERE embedding IS NULL")
            .fetch_one(pool)
            .await?;
    Ok((total, missing))
}

/// Row ids (with their embed prose) whose `embedding` is still NULL, up to
/// `limit`. The text mirrors the embedding-migration cron's `text_select` for
/// `mcp_tool_catalog` (`concat_ws(' ', name, description)`) — and the seed-path
/// [`embed_text`] — so an in-process warm-up reembed produces the same vector the
/// cron would. Drives [`crate::mcp::tools::tool_meta::warm_mcp_tool_catalog`].
pub async fn ids_missing_embeddings(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, concat_ws(' ', name, description) AS t
         FROM mcp_tool_catalog
         WHERE embedding IS NULL
         ORDER BY id
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Write one row's 1024-dim BGE-M3 embedding, stamping the `bge-m3-v1` signature
/// the cron uses (the v38 column default). Used by the warm-up reembed so
/// `tool_catalog` semantic ranking works without the (default-off) cron.
pub async fn update_embedding(
    pool: &PgPool,
    id: i64,
    embedding: &[f32],
) -> Result<(), sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "mcp_tool_catalog::update_embedding: expected a 1024-dim BGE-M3 embedding, got {}",
            embedding.len()
        )));
    }
    let embedding_vec = Vector::from(embedding.to_vec());
    sqlx::query(
        "UPDATE mcp_tool_catalog SET embedding = $1, embedding_signature = 'bge-m3-v1' WHERE id = $2",
    )
    .bind(&embedding_vec)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}
