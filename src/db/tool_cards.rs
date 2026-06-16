//! Queries and ingestion helpers for the developer-tool ("toolbox") catalog.
//!
//! Single-table, 1024d-direct (the `durable_mandates` / `data_tables` model):
//! the `tool_cards.embedding` column is filled by the embedding-migration cron,
//! not on the write path. `upsert_tool_card` stamps a `content_hash` over the
//! embedded prose (prefixed by [`DEV_TOOL_EMBEDDING_SIGNATURE`]) and NULLs the
//! existing vector when that hash changes, so a card edit transparently
//! re-embeds on the next cron pass.

use pgvector::Vector;
use serde::Serialize;
use sqlx::PgPool;

use crate::db::patterns::content_hash;
use crate::tools_catalog::{ToolCategorySeed, ToolSeed, card_content};

/// Content-staleness key folded into `content_hash`. Bump the version suffix
/// whenever the embedded-prose composition (`tools_catalog::card_content`)
/// changes in a way that should force a global re-embed.
pub const DEV_TOOL_EMBEDDING_SIGNATURE: &str = "pgmcp-tool-embedding-v1";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ToolCardRow {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub domain: String,
    pub category: String,
    pub summary: String,
    pub what_it_does: String,
    pub when_to_use: String,
    pub inputs_outputs: String,
    pub invocation: String,
    pub strengths: String,
    pub limitations: String,
    pub alternatives: Vec<String>,
    pub availability: String,
    pub docs_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ToolCardListRow {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub domain: String,
    pub category: String,
    pub summary: String,
    pub availability: String,
    pub docs_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ToolCardSearchRow {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub domain: String,
    pub category: String,
    pub summary: String,
    pub when_to_use: String,
    pub invocation: String,
    pub availability: String,
    pub docs_url: Option<String>,
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DomainCount {
    pub domain: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CategoryCount {
    pub domain: String,
    pub category: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCatalogStats {
    pub categories: i64,
    pub tools: i64,
    pub by_domain: Vec<DomainCount>,
    pub by_category: Vec<CategoryCount>,
    pub missing_embeddings: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ToolSearchOptions {
    pub domain: Option<String>,
    pub category: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolListOptions {
    pub domain: Option<String>,
    pub category: Option<String>,
    pub limit: i32,
    pub offset: i32,
}

pub async fn upsert_tool_category(
    pool: &PgPool,
    seed: &ToolCategorySeed,
) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO tool_categories (slug, name, description, domain)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (slug) DO UPDATE SET
            name = EXCLUDED.name,
            description = EXCLUDED.description,
            domain = EXCLUDED.domain
         RETURNING id",
    )
    .bind(seed.slug)
    .bind(seed.name)
    .bind(seed.description)
    .bind(seed.domain)
    .fetch_one(pool)
    .await
}

pub async fn upsert_tool_card(pool: &PgPool, seed: &ToolSeed) -> Result<i64, sqlx::Error> {
    let hash = content_hash(&format!(
        "{DEV_TOOL_EMBEDDING_SIGNATURE}{}",
        card_content(seed)
    ));
    let alternatives: Vec<String> = seed.alternatives.iter().map(|s| s.to_string()).collect();
    sqlx::query_scalar(
        "INSERT INTO tool_cards (
            slug, name, domain, category, summary, what_it_does, when_to_use,
            inputs_outputs, invocation, strengths, limitations, alternatives,
            availability, docs_url, content_hash, updated_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, NOW())
         ON CONFLICT (slug) DO UPDATE SET
            name = EXCLUDED.name,
            domain = EXCLUDED.domain,
            category = EXCLUDED.category,
            summary = EXCLUDED.summary,
            what_it_does = EXCLUDED.what_it_does,
            when_to_use = EXCLUDED.when_to_use,
            inputs_outputs = EXCLUDED.inputs_outputs,
            invocation = EXCLUDED.invocation,
            strengths = EXCLUDED.strengths,
            limitations = EXCLUDED.limitations,
            alternatives = EXCLUDED.alternatives,
            availability = EXCLUDED.availability,
            docs_url = EXCLUDED.docs_url,
            content_hash = EXCLUDED.content_hash,
            -- drop the stale vector so the cron re-embeds when the prose changed
            embedding = CASE
                WHEN tool_cards.content_hash IS DISTINCT FROM EXCLUDED.content_hash
                THEN NULL ELSE tool_cards.embedding END,
            embedding_signature = CASE
                WHEN tool_cards.content_hash IS DISTINCT FROM EXCLUDED.content_hash
                THEN NULL ELSE tool_cards.embedding_signature END,
            updated_at = NOW()
         RETURNING id",
    )
    .bind(seed.slug)
    .bind(seed.name)
    .bind(seed.domain)
    .bind(seed.category)
    .bind(seed.summary)
    .bind(seed.what_it_does)
    .bind(seed.when_to_use)
    .bind(seed.inputs_outputs)
    .bind(seed.invocation)
    .bind(seed.strengths)
    .bind(seed.limitations)
    .bind(alternatives)
    .bind(seed.availability)
    .bind(seed.docs_url)
    .bind(hash)
    .fetch_one(pool)
    .await
}

pub async fn count_tool_cards(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM tool_cards")
        .fetch_one(pool)
        .await
}

pub async fn get_tool_card(
    pool: &PgPool,
    slug_or_id: &str,
) -> Result<Option<ToolCardRow>, sqlx::Error> {
    let by_id = slug_or_id.parse::<i64>().ok();
    sqlx::query_as::<_, ToolCardRow>(
        "SELECT id, slug, name, domain, category, summary, what_it_does, when_to_use,
                inputs_outputs, invocation, strengths, limitations,
                COALESCE(alternatives, ARRAY[]::text[]) AS alternatives,
                availability, docs_url
         FROM tool_cards
         WHERE (($1::bigint IS NOT NULL AND id = $1) OR ($2::text IS NOT NULL AND slug = $2))
         LIMIT 1",
    )
    .bind(by_id)
    .bind(if by_id.is_none() {
        Some(slug_or_id)
    } else {
        None
    })
    .fetch_optional(pool)
    .await
}

pub async fn list_tool_cards(
    pool: &PgPool,
    options: ToolListOptions,
) -> Result<Vec<ToolCardListRow>, sqlx::Error> {
    sqlx::query_as::<_, ToolCardListRow>(
        "SELECT id, slug, name, domain, category, summary, availability, docs_url
         FROM tool_cards
         WHERE ($1::text IS NULL OR domain = $1)
           AND ($2::text IS NULL OR category = $2)
         ORDER BY domain, category, name
         LIMIT $3 OFFSET $4",
    )
    .bind(options.domain)
    .bind(options.category)
    .bind(options.limit)
    .bind(options.offset)
    .fetch_all(pool)
    .await
}

pub async fn semantic_search_tool_cards(
    pool: &PgPool,
    embedding: &[f32],
    limit: i32,
    ef_search: i32,
    options: ToolSearchOptions,
) -> Result<Vec<ToolCardSearchRow>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "semantic_search_tool_cards: expected a 1024-dimension BGE-M3 query embedding, got {}",
            embedding.len()
        )));
    }
    let embedding_vec = Vector::from(embedding.to_vec());

    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL hnsw.ef_search = {}",
        ef_search
    )))
    .execute(&mut *tx)
    .await?;

    let rows = sqlx::query_as::<_, ToolCardSearchRow>(
        "SELECT id, slug, name, domain, category, summary, when_to_use, invocation,
                availability, docs_url,
                1 - (embedding <=> $1) AS score
         FROM tool_cards
         WHERE ($3::text IS NULL OR domain = $3)
           AND ($4::text IS NULL OR category = $4)
           AND embedding IS NOT NULL
         ORDER BY embedding <=> $1
         LIMIT $2",
    )
    .bind(&embedding_vec)
    .bind(limit)
    .bind(options.domain)
    .bind(options.category)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(rows)
}

/// Synchronously set a card's embedding (used by `toolbox_refresh`'s force-embed
/// path for immediate availability; the cron is the normal backfill route).
pub async fn update_tool_card_embedding(
    pool: &PgPool,
    id: i64,
    embedding: &[f32],
) -> Result<(), sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "update_tool_card_embedding: expected a 1024-dimension BGE-M3 embedding, got {}",
            embedding.len()
        )));
    }
    let embedding_vec = Vector::from(embedding.to_vec());
    sqlx::query(
        "UPDATE tool_cards SET embedding = $1, embedding_signature = 'bge-m3-v1' WHERE id = $2",
    )
    .bind(embedding_vec)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Cards whose embedding is still NULL (cron/refresh backfill targets).
pub async fn ids_missing_embeddings(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    // Mirror the embedding-migration cron's `text_select` for `tool_cards` so an
    // in-process reembed produces the same vector the cron would.
    sqlx::query_as(
        "SELECT id, concat_ws(' ', name, summary, what_it_does, when_to_use, inputs_outputs,
                              invocation, strengths, limitations, availability) AS t
         FROM tool_cards
         WHERE embedding IS NULL
         ORDER BY id
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn catalog_stats(pool: &PgPool) -> Result<ToolCatalogStats, sqlx::Error> {
    let categories = sqlx::query_scalar("SELECT COUNT(*) FROM tool_categories")
        .fetch_one(pool)
        .await?;
    let tools = sqlx::query_scalar("SELECT COUNT(*) FROM tool_cards")
        .fetch_one(pool)
        .await?;
    let missing_embeddings =
        sqlx::query_scalar("SELECT COUNT(*) FROM tool_cards WHERE embedding IS NULL")
            .fetch_one(pool)
            .await?;
    let by_domain = sqlx::query_as::<_, DomainCount>(
        "SELECT domain, COUNT(*)::bigint AS count
         FROM tool_cards GROUP BY domain ORDER BY domain",
    )
    .fetch_all(pool)
    .await?;
    let by_category = sqlx::query_as::<_, CategoryCount>(
        "SELECT domain, category, COUNT(*)::bigint AS count
         FROM tool_cards GROUP BY domain, category ORDER BY domain, category",
    )
    .fetch_all(pool)
    .await?;

    Ok(ToolCatalogStats {
        categories,
        tools,
        by_domain,
        by_category,
        missing_embeddings,
    })
}
