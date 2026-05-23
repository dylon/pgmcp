//! Queries and small ingestion helpers for the software-pattern knowledge index.

use chrono::{DateTime, Utc};
use pgvector::Vector;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use xxhash_rust::xxh3::xxh3_64;

use crate::patterns::{ParadigmSeed, PatternSeed};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PatternSearchRow {
    pub pattern_id: i64,
    pub slug: String,
    pub name: String,
    pub kind: String,
    pub category: String,
    pub summary: String,
    pub intent: String,
    pub canonical_url: Option<String>,
    pub source_id: i64,
    pub source_family: String,
    pub source_title: String,
    pub source_url: Option<String>,
    pub license_label: Option<String>,
    pub chunk_content: String,
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PatternListRow {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub kind: String,
    pub category: String,
    pub summary: String,
    pub canonical_url: Option<String>,
    pub paradigms: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PatternDetailRow {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub kind: String,
    pub category: String,
    pub summary: String,
    pub intent: String,
    pub problem: String,
    pub solution: String,
    pub consequences: String,
    pub canonical_url: Option<String>,
    pub paradigms: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PatternSourceRow {
    pub id: i64,
    pub source_family: String,
    pub title: String,
    pub url: Option<String>,
    pub license_label: Option<String>,
    pub source_type: String,
    pub ingest_policy: String,
    pub status: String,
    pub fetched_at: Option<DateTime<Utc>>,
    pub imported_at: DateTime<Utc>,
    pub content_hash: Option<i64>,
    pub chunk_count: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SourceStateRow {
    pub id: i64,
    pub content: Option<String>,
    pub content_hash: Option<i64>,
    pub metadata: serde_json::Value,
    pub chunk_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PatternCatalogStats {
    pub paradigms: i64,
    pub patterns: i64,
    pub anti_patterns: i64,
    pub principles: i64,
    pub code_smells: i64,
    pub sources: i64,
    pub chunks: i64,
    pub chunks_missing_embeddings: i64,
    pub source_families: Vec<SourceFamilyStats>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SourceFamilyStats {
    pub source_family: String,
    pub source_count: i64,
    pub chunk_count: i64,
}

#[derive(Debug, Clone)]
pub struct SourceUpsert<'a> {
    pub source_family: &'a str,
    pub title: &'a str,
    pub url: Option<&'a str>,
    pub license_label: Option<&'a str>,
    pub source_type: &'a str,
    pub ingest_policy: &'a str,
    pub content: Option<&'a str>,
    pub status: &'a str,
    pub error: Option<&'a str>,
    pub metadata: serde_json::Value,
    pub fetched_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct PatternSearchOptions {
    pub kind: Option<String>,
    pub paradigms: Option<Vec<String>>,
    pub category: Option<String>,
    pub source_family: Option<String>,
    pub source_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PatternListOptions {
    pub kind: Option<String>,
    pub paradigm: Option<String>,
    pub category: Option<String>,
    pub source_family: Option<String>,
    pub limit: i32,
    pub offset: i32,
}

pub fn content_hash(content: &str) -> i64 {
    xxh3_64(content.as_bytes()) as i64
}

pub fn content_sha256(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    format!("{:x}", digest)
}

pub async fn upsert_paradigm(pool: &PgPool, seed: &ParadigmSeed) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO programming_paradigms (slug, name, description, wikipedia_url)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (slug) DO UPDATE SET
            name = EXCLUDED.name,
            description = EXCLUDED.description,
            wikipedia_url = EXCLUDED.wikipedia_url
         RETURNING id",
    )
    .bind(seed.slug)
    .bind(seed.name)
    .bind(seed.description)
    .bind(seed.wikipedia_url)
    .fetch_one(pool)
    .await
}

pub async fn upsert_pattern(pool: &PgPool, seed: &PatternSeed) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO software_patterns (
            slug, name, kind, category, summary, intent, problem, solution,
            consequences, tags, canonical_url, updated_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NOW())
         ON CONFLICT (slug) DO UPDATE SET
            name = EXCLUDED.name,
            kind = EXCLUDED.kind,
            category = EXCLUDED.category,
            summary = EXCLUDED.summary,
            intent = EXCLUDED.intent,
            problem = EXCLUDED.problem,
            solution = EXCLUDED.solution,
            consequences = EXCLUDED.consequences,
            tags = EXCLUDED.tags,
            canonical_url = EXCLUDED.canonical_url,
            updated_at = NOW()
         RETURNING id",
    )
    .bind(seed.slug)
    .bind(seed.name)
    .bind(seed.kind)
    .bind(seed.category)
    .bind(seed.summary)
    .bind(seed.intent)
    .bind(seed.problem)
    .bind(seed.solution)
    .bind(seed.consequences)
    .bind(seed.tags.to_vec())
    .bind(seed.canonical_url)
    .fetch_one(pool)
    .await
}

pub async fn link_pattern_paradigm(
    pool: &PgPool,
    pattern_id: i64,
    paradigm_slug: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO software_pattern_paradigms (pattern_id, paradigm_id)
         SELECT $1, id FROM programming_paradigms WHERE slug = $2
         ON CONFLICT (pattern_id, paradigm_id) DO NOTHING",
    )
    .bind(pattern_id)
    .bind(paradigm_slug)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn find_pattern_id_by_slug(
    pool: &PgPool,
    slug: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM software_patterns WHERE slug = $1")
        .bind(slug)
        .fetch_optional(pool)
        .await
}

pub async fn upsert_source(pool: &PgPool, source: SourceUpsert<'_>) -> Result<i64, sqlx::Error> {
    let hash = source.content.map(content_hash);
    let existing_id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM software_pattern_sources
         WHERE source_family = $1
           AND title = $2
           AND COALESCE(url, '') = COALESCE($3, '')
         LIMIT 1",
    )
    .bind(source.source_family)
    .bind(source.title)
    .bind(source.url)
    .fetch_optional(pool)
    .await?;

    if let Some(id) = existing_id {
        sqlx::query(
            "UPDATE software_pattern_sources SET
                license_label = $2,
                source_type = $3,
                ingest_policy = $4,
                content = COALESCE($5, content),
                content_hash = COALESCE($6, content_hash),
                fetched_at = COALESCE($7, fetched_at),
                imported_at = NOW(),
                status = $8,
                error = $9,
                metadata = $10
             WHERE id = $1",
        )
        .bind(id)
        .bind(source.license_label)
        .bind(source.source_type)
        .bind(source.ingest_policy)
        .bind(source.content)
        .bind(hash)
        .bind(source.fetched_at)
        .bind(source.status)
        .bind(source.error)
        .bind(source.metadata)
        .execute(pool)
        .await?;
        Ok(id)
    } else {
        sqlx::query_scalar(
            "INSERT INTO software_pattern_sources (
                source_family, title, url, license_label, source_type, ingest_policy,
                content, content_hash, fetched_at, status, error, metadata
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             RETURNING id",
        )
        .bind(source.source_family)
        .bind(source.title)
        .bind(source.url)
        .bind(source.license_label)
        .bind(source.source_type)
        .bind(source.ingest_policy)
        .bind(source.content)
        .bind(hash)
        .bind(source.fetched_at)
        .bind(source.status)
        .bind(source.error)
        .bind(source.metadata)
        .fetch_one(pool)
        .await
    }
}

pub async fn find_source_state(
    pool: &PgPool,
    source_family: &str,
    title: &str,
    url: Option<&str>,
) -> Result<Option<SourceStateRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT
            s.id,
            s.content,
            s.content_hash,
            s.metadata,
            COUNT(c.id)::BIGINT AS chunk_count
         FROM software_pattern_sources s
         LEFT JOIN software_pattern_chunks c ON c.source_id = s.id
         WHERE s.source_family = $1
           AND s.title = $2
           AND COALESCE(s.url, '') = COALESCE($3, '')
         GROUP BY s.id
         LIMIT 1",
    )
    .bind(source_family)
    .bind(title)
    .bind(url)
    .fetch_optional(pool)
    .await
}

pub async fn update_source_status(
    pool: &PgPool,
    source_id: i64,
    status: &str,
    error: Option<&str>,
    metadata: serde_json::Value,
    fetched_at: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE software_pattern_sources SET
            fetched_at = COALESCE($2, fetched_at),
            imported_at = NOW(),
            status = $3,
            error = $4,
            metadata = $5
         WHERE id = $1",
    )
    .bind(source_id)
    .bind(fetched_at)
    .bind(status)
    .bind(error)
    .bind(metadata)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn link_source_pattern(
    pool: &PgPool,
    source_id: i64,
    pattern_id: i64,
    relation: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO software_pattern_source_patterns (source_id, pattern_id, relation)
         VALUES ($1, $2, $3)
         ON CONFLICT (source_id, pattern_id) DO UPDATE SET relation = EXCLUDED.relation",
    )
    .bind(source_id)
    .bind(pattern_id)
    .bind(relation)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_source_chunks(pool: &PgPool, source_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM software_pattern_chunks WHERE source_id = $1")
        .bind(source_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn insert_source_chunk(
    pool: &PgPool,
    source_id: i64,
    chunk_index: i32,
    content: &str,
    start_line: i32,
    end_line: i32,
    embedding: &[f32],
) -> Result<(), sqlx::Error> {
    // Phase 5 C3: dispatch on embedding dim. Same shape as
    // queries::insert_chunk. Plan reference:
    // ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md
    // Phase 5 C3.
    let embedding_vec = Vector::from(embedding.to_vec());
    match embedding.len() {
        384 => {
            sqlx::query(
                "INSERT INTO software_pattern_chunks
                    (source_id, chunk_index, content, start_line, end_line,
                     embedding, embedding_signature)
                 VALUES ($1, $2, $3, $4, $5, $6, 'minilm-l6-v2')
                 ON CONFLICT (source_id, chunk_index) DO UPDATE SET
                    content = EXCLUDED.content,
                    start_line = EXCLUDED.start_line,
                    end_line = EXCLUDED.end_line,
                    embedding = EXCLUDED.embedding,
                    embedding_signature = EXCLUDED.embedding_signature",
            )
            .bind(source_id)
            .bind(chunk_index)
            .bind(content)
            .bind(start_line)
            .bind(end_line)
            .bind(embedding_vec)
            .execute(pool)
            .await?;
        }
        1024 => {
            sqlx::query(
                "INSERT INTO software_pattern_chunks
                    (source_id, chunk_index, content, start_line, end_line,
                     embedding_v2, embedding_signature)
                 VALUES ($1, $2, $3, $4, $5, $6, 'bge-m3-v1')
                 ON CONFLICT (source_id, chunk_index) DO UPDATE SET
                    content = EXCLUDED.content,
                    start_line = EXCLUDED.start_line,
                    end_line = EXCLUDED.end_line,
                    embedding_v2 = EXCLUDED.embedding_v2,
                    embedding_signature = EXCLUDED.embedding_signature",
            )
            .bind(source_id)
            .bind(chunk_index)
            .bind(content)
            .bind(start_line)
            .bind(end_line)
            .bind(embedding_vec)
            .execute(pool)
            .await?;
        }
        other => {
            return Err(sqlx::Error::Protocol(format!(
                "insert_source_chunk: unsupported embedding dim {other} \
                 (expected 384 for MiniLM-L6-v2 or 1024 for BGE-M3); \
                 run `pgmcp embed-cutover --check`"
            )));
        }
    }
    Ok(())
}

pub async fn semantic_search_patterns(
    pool: &PgPool,
    embedding: &[f32],
    limit: i32,
    ef_search: i32,
    options: PatternSearchOptions,
) -> Result<Vec<PatternSearchRow>, sqlx::Error> {
    let embedding_vec = Vector::from(embedding.to_vec());
    let paradigms = options.paradigms;

    // Phase 5 C8: signature-aware column dispatch. The
    // software_pattern_chunks table gained an `embedding_v2` column in
    // C1; pick the right one based on the incoming query's dim.
    let col = match embedding.len() {
        384 => "embedding",
        1024 => "embedding_v2",
        other => {
            return Err(sqlx::Error::Protocol(format!(
                "semantic_search_patterns: unsupported query-embedding dim {other} \
                 (expected 384 for MiniLM or 1024 for BGE-M3). \
                 Run `pgmcp embed-cutover --check` to inspect."
            )));
        }
    };

    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;

    let rows = sqlx::query_as::<_, PatternSearchRow>(&format!(
        "SELECT p.id AS pattern_id,
                p.slug, p.name, p.kind, p.category, p.summary, p.intent, p.canonical_url,
                s.id AS source_id, s.source_family, s.title AS source_title,
                s.url AS source_url, s.license_label,
                c.content AS chunk_content,
                1 - (c.{col} <=> $1) AS score
         FROM software_pattern_chunks c
         JOIN software_pattern_sources s ON s.id = c.source_id
         JOIN software_pattern_source_patterns sp ON sp.source_id = s.id
         JOIN software_patterns p ON p.id = sp.pattern_id
         WHERE ($3::text IS NULL OR p.kind = $3)
           AND ($4::text[] IS NULL OR EXISTS (
                SELECT 1
                FROM software_pattern_paradigms pp
                JOIN programming_paradigms pg ON pg.id = pp.paradigm_id
                WHERE pp.pattern_id = p.id
                  AND (pg.slug = ANY($4) OR pg.name = ANY($4))
           ))
           AND ($5::text IS NULL OR p.category = $5)
           AND ($6::text IS NULL OR s.source_family = $6)
           AND ($7::text IS NULL OR s.source_type = $7)
           AND c.{col} IS NOT NULL
         ORDER BY c.{col} <=> $1
         LIMIT $2"
    ))
    .bind(&embedding_vec)
    .bind(limit)
    .bind(options.kind)
    .bind(paradigms)
    .bind(options.category)
    .bind(options.source_family)
    .bind(options.source_type)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(rows)
}

pub async fn list_patterns(
    pool: &PgPool,
    options: PatternListOptions,
) -> Result<Vec<PatternListRow>, sqlx::Error> {
    sqlx::query_as::<_, PatternListRow>(
        "SELECT p.id, p.slug, p.name, p.kind, p.category, p.summary, p.canonical_url,
                COALESCE(array_remove(array_agg(DISTINCT pg.slug), NULL), ARRAY[]::text[]) AS paradigms,
                p.tags
         FROM software_patterns p
         LEFT JOIN software_pattern_paradigms pp ON pp.pattern_id = p.id
         LEFT JOIN programming_paradigms pg ON pg.id = pp.paradigm_id
         WHERE ($1::text IS NULL OR p.kind = $1)
           AND ($2::text IS NULL OR p.category = $2)
           AND ($3::text IS NULL OR EXISTS (
                SELECT 1
                FROM software_pattern_paradigms pp2
                JOIN programming_paradigms pg2 ON pg2.id = pp2.paradigm_id
                WHERE pp2.pattern_id = p.id AND (pg2.slug = $3 OR pg2.name = $3)
           ))
           AND ($4::text IS NULL OR EXISTS (
                SELECT 1
                FROM software_pattern_source_patterns sp
                JOIN software_pattern_sources s ON s.id = sp.source_id
                WHERE sp.pattern_id = p.id AND s.source_family = $4
           ))
         GROUP BY p.id
         ORDER BY p.kind, p.category, p.name
         LIMIT $5 OFFSET $6",
    )
    .bind(options.kind)
    .bind(options.category)
    .bind(options.paradigm)
    .bind(options.source_family)
    .bind(options.limit)
    .bind(options.offset)
    .fetch_all(pool)
    .await
}

pub async fn get_pattern(
    pool: &PgPool,
    slug_or_id: &str,
) -> Result<Option<PatternDetailRow>, sqlx::Error> {
    let by_id = slug_or_id.parse::<i64>().ok();
    sqlx::query_as::<_, PatternDetailRow>(
        "SELECT p.id, p.slug, p.name, p.kind, p.category, p.summary, p.intent,
                p.problem, p.solution, p.consequences, p.canonical_url, p.tags,
                COALESCE(array_remove(array_agg(DISTINCT pg.slug), NULL), ARRAY[]::text[]) AS paradigms
         FROM software_patterns p
         LEFT JOIN software_pattern_paradigms pp ON pp.pattern_id = p.id
         LEFT JOIN programming_paradigms pg ON pg.id = pp.paradigm_id
         WHERE (($1::bigint IS NOT NULL AND p.id = $1) OR ($2::text IS NOT NULL AND p.slug = $2))
         GROUP BY p.id",
    )
    .bind(by_id)
    .bind(if by_id.is_none() { Some(slug_or_id) } else { None })
    .fetch_optional(pool)
    .await
}

pub async fn get_pattern_sources(
    pool: &PgPool,
    pattern_id: i64,
) -> Result<Vec<PatternSourceRow>, sqlx::Error> {
    sqlx::query_as::<_, PatternSourceRow>(
        "SELECT s.id, s.source_family, s.title, s.url, s.license_label,
                s.source_type, s.ingest_policy, s.status, s.fetched_at, s.imported_at,
                s.content_hash,
                COUNT(c.id)::bigint AS chunk_count
         FROM software_pattern_sources s
         JOIN software_pattern_source_patterns sp ON sp.source_id = s.id
         LEFT JOIN software_pattern_chunks c ON c.source_id = s.id
         WHERE sp.pattern_id = $1
         GROUP BY s.id
         ORDER BY s.source_family, s.title",
    )
    .bind(pattern_id)
    .fetch_all(pool)
    .await
}

pub async fn get_source_excerpts(
    pool: &PgPool,
    source_id: i64,
    limit: i32,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT content FROM software_pattern_chunks
         WHERE source_id = $1
         ORDER BY chunk_index
         LIMIT $2",
    )
    .bind(source_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn catalog_stats(pool: &PgPool) -> Result<PatternCatalogStats, sqlx::Error> {
    let paradigms = sqlx::query_scalar("SELECT COUNT(*) FROM programming_paradigms")
        .fetch_one(pool)
        .await?;
    let patterns =
        sqlx::query_scalar("SELECT COUNT(*) FROM software_patterns WHERE kind = 'pattern'")
            .fetch_one(pool)
            .await?;
    let anti_patterns =
        sqlx::query_scalar("SELECT COUNT(*) FROM software_patterns WHERE kind = 'anti_pattern'")
            .fetch_one(pool)
            .await?;
    let principles =
        sqlx::query_scalar("SELECT COUNT(*) FROM software_patterns WHERE kind = 'principle'")
            .fetch_one(pool)
            .await?;
    let code_smells =
        sqlx::query_scalar("SELECT COUNT(*) FROM software_patterns WHERE kind = 'code_smell'")
            .fetch_one(pool)
            .await?;
    let sources = sqlx::query_scalar("SELECT COUNT(*) FROM software_pattern_sources")
        .fetch_one(pool)
        .await?;
    let chunks = sqlx::query_scalar("SELECT COUNT(*) FROM software_pattern_chunks")
        .fetch_one(pool)
        .await?;
    let chunks_missing_embeddings =
        sqlx::query_scalar("SELECT COUNT(*) FROM software_pattern_chunks WHERE embedding IS NULL")
            .fetch_one(pool)
            .await?;
    let source_families = sqlx::query_as::<_, SourceFamilyStats>(
        "SELECT s.source_family,
                COUNT(DISTINCT s.id)::bigint AS source_count,
                COUNT(c.id)::bigint AS chunk_count
         FROM software_pattern_sources s
         LEFT JOIN software_pattern_chunks c ON c.source_id = s.id
         GROUP BY s.source_family
         ORDER BY s.source_family",
    )
    .fetch_all(pool)
    .await?;

    Ok(PatternCatalogStats {
        paradigms,
        patterns,
        anti_patterns,
        principles,
        code_smells,
        sources,
        chunks,
        chunks_missing_embeddings,
        source_families,
    })
}

pub async fn count_patterns(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM software_patterns")
        .fetch_one(pool)
        .await
}

pub async fn start_import_run(
    pool: &PgPool,
    mode: &str,
    source_family: Option<&str>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO software_pattern_import_runs (mode, source_family, status)
         VALUES ($1, $2, 'running')
         RETURNING id",
    )
    .bind(mode)
    .bind(source_family)
    .fetch_one(pool)
    .await
}

pub async fn finish_import_run(
    pool: &PgPool,
    run_id: i64,
    status: &str,
    sources_seen: i32,
    sources_imported: i32,
    chunks_embedded: i32,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE software_pattern_import_runs SET
            finished_at = NOW(),
            status = $2,
            sources_seen = $3,
            sources_imported = $4,
            chunks_embedded = $5,
            error = $6
         WHERE id = $1",
    )
    .bind(run_id)
    .bind(status)
    .bind(sources_seen)
    .bind(sources_imported)
    .bind(chunks_embedded)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

pub fn chunk_text(
    content: &str,
    chunk_size_lines: usize,
    overlap_lines: usize,
) -> Vec<(i32, i32, String)> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let chunk_size = chunk_size_lines.max(1);
    let overlap = overlap_lines.min(chunk_size.saturating_sub(1));
    let step = chunk_size.saturating_sub(overlap).max(1);
    let mut chunks = Vec::new();
    let mut start = 0usize;

    while start < lines.len() {
        let end = (start + chunk_size).min(lines.len());
        let text = lines[start..end].join("\n");
        if !text.trim().is_empty() {
            chunks.push((start as i32 + 1, end as i32, text));
        }
        if end == lines.len() {
            break;
        }
        start += step;
    }

    chunks
}
