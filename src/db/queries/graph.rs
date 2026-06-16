//! Function-metrics + call-graph + semantic-edge + PPR/RAPTOR/CK queries
//! (metric upsert, node/edge listing, semantic edge compute, seed files,
//! fan-io/centralities, watermarks). Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// One row consumed by `upsert_function_metrics_batch`. Mirrors the table
/// columns 1:1.
#[derive(Debug, Clone)]
pub struct FunctionMetricsRow {
    pub function_id: i64,
    pub file_id: i64,
    pub project_id: i32,
    pub cyclomatic: i32,
    pub cognitive: i32,
    pub halstead_n1: i32,
    pub halstead_n2: i32,
    pub halstead_big_n1: i32,
    pub halstead_big_n2: i32,
    pub halstead_volume: f64,
    pub halstead_difficulty: f64,
    pub halstead_effort: f64,
    pub halstead_bugs: f64,
    pub npath: i64,
    pub npath_overflow: bool,
    pub loc: i32,
    pub comment_lines: i32,
    pub maintainability_index: f64,
    pub panic_paths: i32,
    pub unsafe_blocks: i32,
}

/// UNNEST-style bulk upsert into `function_metrics`. ON CONFLICT (function_id)
/// DO UPDATE refreshes every column except `fan_in`/`fan_out` (those are owned
/// by the call-graph cron).
pub async fn upsert_function_metrics_batch(
    pool: &PgPool,
    rows: &[FunctionMetricsRow],
) -> Result<u64, sqlx::Error> {
    if rows.is_empty() {
        return Ok(0);
    }
    let function_ids: Vec<i64> = rows.iter().map(|r| r.function_id).collect();
    let file_ids: Vec<i64> = rows.iter().map(|r| r.file_id).collect();
    let project_ids: Vec<i32> = rows.iter().map(|r| r.project_id).collect();
    let cyclo: Vec<i32> = rows.iter().map(|r| r.cyclomatic).collect();
    let cogn: Vec<i32> = rows.iter().map(|r| r.cognitive).collect();
    let h_n1: Vec<i32> = rows.iter().map(|r| r.halstead_n1).collect();
    let h_n2: Vec<i32> = rows.iter().map(|r| r.halstead_n2).collect();
    let h_bn1: Vec<i32> = rows.iter().map(|r| r.halstead_big_n1).collect();
    let h_bn2: Vec<i32> = rows.iter().map(|r| r.halstead_big_n2).collect();
    let h_v: Vec<f64> = rows.iter().map(|r| r.halstead_volume).collect();
    let h_d: Vec<f64> = rows.iter().map(|r| r.halstead_difficulty).collect();
    let h_e: Vec<f64> = rows.iter().map(|r| r.halstead_effort).collect();
    let h_b: Vec<f64> = rows.iter().map(|r| r.halstead_bugs).collect();
    let np: Vec<i64> = rows.iter().map(|r| r.npath).collect();
    let np_ovf: Vec<bool> = rows.iter().map(|r| r.npath_overflow).collect();
    let loc: Vec<i32> = rows.iter().map(|r| r.loc).collect();
    let cl: Vec<i32> = rows.iter().map(|r| r.comment_lines).collect();
    let mi: Vec<f64> = rows.iter().map(|r| r.maintainability_index).collect();
    let panic_p: Vec<i32> = rows.iter().map(|r| r.panic_paths).collect();
    let uns: Vec<i32> = rows.iter().map(|r| r.unsafe_blocks).collect();

    let res = sqlx::query(
        "INSERT INTO function_metrics (
            function_id, file_id, project_id,
            cyclomatic, cognitive,
            halstead_n1, halstead_n2, halstead_big_n1, halstead_big_n2,
            halstead_volume, halstead_difficulty, halstead_effort, halstead_bugs,
            npath, npath_overflow,
            loc, comment_lines,
            maintainability_index,
            panic_paths, unsafe_blocks,
            computed_at
        )
        SELECT * FROM UNNEST(
            $1::int8[], $2::int8[], $3::int4[],
            $4::int4[], $5::int4[],
            $6::int4[], $7::int4[], $8::int4[], $9::int4[],
            $10::float8[], $11::float8[], $12::float8[], $13::float8[],
            $14::int8[], $15::bool[],
            $16::int4[], $17::int4[],
            $18::float8[],
            $19::int4[], $20::int4[]
        ) AS u(
            function_id, file_id, project_id,
            cyclomatic, cognitive,
            halstead_n1, halstead_n2, halstead_big_n1, halstead_big_n2,
            halstead_volume, halstead_difficulty, halstead_effort, halstead_bugs,
            npath, npath_overflow,
            loc, comment_lines,
            maintainability_index,
            panic_paths, unsafe_blocks
        ), (SELECT NOW())
        ON CONFLICT (function_id) DO UPDATE SET
            file_id = EXCLUDED.file_id,
            project_id = EXCLUDED.project_id,
            cyclomatic = EXCLUDED.cyclomatic,
            cognitive = EXCLUDED.cognitive,
            halstead_n1 = EXCLUDED.halstead_n1,
            halstead_n2 = EXCLUDED.halstead_n2,
            halstead_big_n1 = EXCLUDED.halstead_big_n1,
            halstead_big_n2 = EXCLUDED.halstead_big_n2,
            halstead_volume = EXCLUDED.halstead_volume,
            halstead_difficulty = EXCLUDED.halstead_difficulty,
            halstead_effort = EXCLUDED.halstead_effort,
            halstead_bugs = EXCLUDED.halstead_bugs,
            npath = EXCLUDED.npath,
            npath_overflow = EXCLUDED.npath_overflow,
            loc = EXCLUDED.loc,
            comment_lines = EXCLUDED.comment_lines,
            maintainability_index = EXCLUDED.maintainability_index,
            panic_paths = EXCLUDED.panic_paths,
            unsafe_blocks = EXCLUDED.unsafe_blocks,
            computed_at = NOW()",
    )
    .bind(&function_ids)
    .bind(&file_ids)
    .bind(&project_ids)
    .bind(&cyclo)
    .bind(&cogn)
    .bind(&h_n1)
    .bind(&h_n2)
    .bind(&h_bn1)
    .bind(&h_bn2)
    .bind(&h_v)
    .bind(&h_d)
    .bind(&h_e)
    .bind(&h_b)
    .bind(&np)
    .bind(&np_ovf)
    .bind(&loc)
    .bind(&cl)
    .bind(&mi)
    .bind(&panic_p)
    .bind(&uns)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Read the function-metrics watermark for a project.
pub async fn get_function_metrics_watermark(
    pool: &PgPool,
    project_id: i32,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let key = format!("function_metrics_last_run:{}", project_id);
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

/// Set the function-metrics watermark for a project.
pub async fn set_function_metrics_watermark(
    pool: &PgPool,
    project_id: i32,
    ts: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let key = format!("function_metrics_last_run:{}", project_id);
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

/// True when this project has source files in a function-metrics backend
/// language yet zero rows in `function_metrics` — the function-metrics analogue
/// of `project_missing_import_refs`. Detects the advance-on-empty watermark trap
/// so the caller can force a full re-scan and self-heal the empty metrics table
/// (which otherwise leaves `complexity_hotspots` / scorecard complexity dark).
/// `function_metrics` carries `project_id`, so this is two short-circuiting
/// EXISTS scans.
pub async fn project_missing_function_metrics(
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
                 SELECT 1 FROM function_metrics WHERE project_id = $1
             )",
    )
    .bind(project_id)
    .bind(&langs)
    .fetch_one(pool)
    .await?;
    Ok(needs)
}

// ----------------------------------------------------------------------------
// Call-graph cron support
// ----------------------------------------------------------------------------

/// One node in the in-process call graph (one row per function symbol in a
/// project). Returned by `list_function_nodes_for_project`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FunctionNodeRow {
    pub symbol_id: i64,
    pub file_id: i64,
    pub name: String,
    pub relative_path: String,
    pub language: String,
    pub parent_id: Option<i64>,
}

/// Fetch every function symbol in a project, with the file path/language and
/// its parent_id (so the call-graph builder can decide `is_method`).
pub async fn list_function_nodes_for_project(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<FunctionNodeRow>, sqlx::Error> {
    sqlx::query_as::<_, FunctionNodeRow>(
        "SELECT fs.id as symbol_id,
                fs.file_id,
                fs.name,
                f.relative_path,
                f.language,
                fs.parent_id
         FROM file_symbols fs
         JOIN indexed_files f ON fs.file_id = f.id
         WHERE f.project_id = $1 AND fs.kind = 'function'",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// One raw call edge for the call-graph cron — sourced from `symbol_references`
/// rows where `ref_kind='call'` and the source is inside a known function.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RawCallEdgeRow {
    pub source_file_id: i64,
    pub source_symbol_id: Option<i64>,
    pub target_file_id: Option<i64>,
    pub target_symbol_id: Option<i64>,
    pub target_raw: String,
    /// Resolution confidence of the underlying `symbol_reference` (Phase 4.1):
    /// the call edge's weight, so probability-weighted graph algorithms discount
    /// low-confidence (ambiguous bare-name) edges.
    pub resolution_confidence: Option<f64>,
}

/// Read all call-kind symbol_references for a project.
pub async fn list_call_edges_for_project(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<RawCallEdgeRow>, sqlx::Error> {
    sqlx::query_as::<_, RawCallEdgeRow>(
        "SELECT sr.source_file_id,
                sr.source_symbol_id,
                sr.target_file_id,
                sr.target_symbol_id,
                sr.target_raw,
                sr.resolution_confidence
         FROM symbol_references sr
         JOIN indexed_files f ON sr.source_file_id = f.id
         WHERE f.project_id = $1
           AND sr.ref_kind = 'call'
           AND sr.source_symbol_id IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// Delete existing call edges for a project before re-populating.
pub async fn delete_call_edges_for_project(
    pool: &PgPool,
    project_id: i32,
) -> Result<u64, sqlx::Error> {
    let res =
        sqlx::query("DELETE FROM code_graph_edges WHERE project_id = $1 AND edge_type = 'call'")
            .bind(project_id)
            .execute(pool)
            .await?;
    Ok(res.rows_affected())
}

/// Bulk-insert call edges into `code_graph_edges` with `edge_type='call'`.
/// Skips rows whose source_symbol_id is NULL (would violate CHECK constraint).
pub async fn bulk_insert_call_edges(
    pool: &PgPool,
    project_id: i32,
    edges: &[RawCallEdgeRow],
) -> Result<u64, sqlx::Error> {
    if edges.is_empty() {
        return Ok(0);
    }
    let valid: Vec<&RawCallEdgeRow> = edges
        .iter()
        .filter(|e| e.source_symbol_id.is_some())
        .collect();
    if valid.is_empty() {
        return Ok(0);
    }
    let project_ids: Vec<i32> = vec![project_id; valid.len()];
    let source_files: Vec<i64> = valid.iter().map(|e| e.source_file_id).collect();
    let target_files: Vec<Option<i64>> = valid.iter().map(|e| e.target_file_id).collect();
    let source_symbols: Vec<i64> = valid
        .iter()
        .map(|e| e.source_symbol_id.expect("filtered above"))
        .collect();
    let target_symbols: Vec<Option<i64>> = valid.iter().map(|e| e.target_symbol_id).collect();
    let target_raws: Vec<String> = valid.iter().map(|e| e.target_raw.clone()).collect();

    let res = sqlx::query(
        "INSERT INTO code_graph_edges
            (project_id, source_file_id, target_file_id, source_symbol_id,
             target_symbol_id, edge_type, target_raw, weight, computed_at)
         SELECT u.project_id, u.source_file_id, u.target_file_id, u.source_symbol_id,
                u.target_symbol_id, 'call', u.target_raw, 1.0, NOW()
         FROM UNNEST(
             $1::int4[], $2::int8[], $3::int8[], $4::int8[],
             $5::int8[], $6::text[]
         ) AS u(project_id, source_file_id, target_file_id, source_symbol_id,
                target_symbol_id, target_raw)
         ON CONFLICT (source_file_id, COALESCE(target_file_id, -1::BIGINT), edge_type, COALESCE(target_raw, '')) DO NOTHING",
    )
    .bind(&project_ids)
    .bind(&source_files)
    .bind(&target_files)
    .bind(&source_symbols)
    .bind(&target_symbols)
    .bind(&target_raws)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// A directed file→file semantic-affinity edge (graph-roadmap Phase 3.1).
/// `weight` is the maximum chunk-level cosine similarity observed between any
/// chunk of the source file and any chunk of the target file (within the
/// per-chunk HNSW neighbor set, above threshold).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SemanticFileEdge {
    pub source_file_id: i64,
    pub target_file_id: i64,
    pub weight: f64,
}

/// Delete prior `edge_type='semantic'` edges for a project so a re-scan is
/// idempotent. Scoped to the semantic partition — never touches `'import'` /
/// `'co_change'` / `'call'` edges.
pub async fn delete_semantic_edges_for_project(
    pool: &PgPool,
    project_id: i32,
) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "DELETE FROM code_graph_edges WHERE project_id = $1 AND edge_type = 'semantic'",
    )
    .bind(project_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Compute within-project file→file semantic edges via the HNSW chunk index.
/// Each chunk probes its top `per_chunk_k` nearest neighbors in OTHER files of
/// the same project; chunk pairs at/above `threshold` cosine are aggregated to
/// file pairs (MAX cosine), and each source file keeps only its top `fanout_k`
/// targets (the fan-out cap keeps semantic hubs from forming near-cliques that
/// wash out community modularity). Uses the active BGE-M3 `embedding_v2`
/// column — the same column the cross-project similarity scanner probes.
pub async fn compute_semantic_file_edges(
    pool: &PgPool,
    project_id: i32,
    threshold: f64,
    per_chunk_k: i32,
    fanout_k: i32,
    ef_search: i32,
) -> Result<Vec<SemanticFileEdge>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL hnsw.ef_search = {}",
        ef_search
    )))
    .execute(&mut *tx)
    .await?;
    // The per-project HNSW LATERAL can exceed the daemon-wide statement_timeout
    // on large indexes; raise the ceiling for this transaction only (mirrors
    // `batch_find_cross_project_neighbors`).
    sqlx::query("SET LOCAL statement_timeout = '5min'")
        .execute(&mut *tx)
        .await?;
    // Label this heavy transaction so the graceful-shutdown sweep
    // (db::admin::terminate_heavy_backends) can terminate it and free its locks.
    sqlx::query("SET LOCAL application_name = 'pgmcp:heavy:semantic-edges'")
        .execute(&mut *tx)
        .await?;
    let col = crate::embed::signature::read_active_signature(pool)
        .await?
        .read_column();
    let rows = sqlx::query_as::<_, SemanticFileEdge>(sqlx::AssertSqlSafe(format!(
        "WITH chunk_nn AS (
            SELECT c.file_id AS source_file_id,
                   nn.file_id AS target_file_id,
                   nn.similarity
            FROM file_chunks c
            JOIN indexed_files f ON f.id = c.file_id
            CROSS JOIN LATERAL (
                SELECT c2.file_id,
                       1 - (c2.{col} <=> c.{col}) AS similarity
                FROM file_chunks c2
                JOIN indexed_files f2 ON f2.id = c2.file_id
                WHERE f2.project_id = $1
                  AND c2.file_id <> c.file_id
                  AND c2.{col} IS NOT NULL
                ORDER BY c2.{col} <=> c.{col}
                LIMIT $3
            ) nn
            WHERE f.project_id = $1
              AND c.{col} IS NOT NULL
        ),
        file_pairs AS (
            SELECT source_file_id, target_file_id, MAX(similarity) AS weight
            FROM chunk_nn
            WHERE similarity >= $2
            GROUP BY source_file_id, target_file_id
        ),
        ranked AS (
            SELECT source_file_id, target_file_id, weight,
                   ROW_NUMBER() OVER (
                       PARTITION BY source_file_id ORDER BY weight DESC
                   ) AS rn
            FROM file_pairs
        )
        SELECT source_file_id, target_file_id, weight
        FROM ranked
        WHERE rn <= $4",
    )))
    .bind(project_id)
    .bind(threshold)
    .bind(per_chunk_k)
    .bind(fanout_k)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Bulk-insert semantic file→file edges (`edge_type='semantic'`,
/// `target_raw=NULL`, both symbol endpoints NULL — the `cge_call_needs_source
/// _symbol` CHECK only constrains `'call'`). Mirrors `bulk_insert_call_edges`'
/// UNNEST shape. The caller is responsible for de-duplicating the input (the
/// `ON CONFLICT` target cannot be hit twice within one statement).
pub async fn bulk_insert_semantic_edges(
    pool: &PgPool,
    project_id: i32,
    edges: &[SemanticFileEdge],
) -> Result<u64, sqlx::Error> {
    if edges.is_empty() {
        return Ok(0);
    }
    let project_ids: Vec<i32> = vec![project_id; edges.len()];
    let source_files: Vec<i64> = edges.iter().map(|e| e.source_file_id).collect();
    let target_files: Vec<i64> = edges.iter().map(|e| e.target_file_id).collect();
    let weights: Vec<f64> = edges.iter().map(|e| e.weight).collect();
    let res = sqlx::query(
        "INSERT INTO code_graph_edges
            (project_id, source_file_id, target_file_id, edge_type, target_raw, weight, computed_at)
         SELECT u.project_id, u.source_file_id, u.target_file_id, 'semantic', NULL, u.weight, NOW()
         FROM UNNEST($1::int4[], $2::int8[], $3::int8[], $4::float8[])
             AS u(project_id, source_file_id, target_file_id, weight)
         ON CONFLICT (source_file_id, COALESCE(target_file_id, -1::BIGINT), edge_type, COALESCE(target_raw, '')) DO NOTHING",
    )
    .bind(&project_ids)
    .bind(&source_files)
    .bind(&target_files)
    .bind(&weights)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// A representative chunk for a file in a code-PPR result (graph-roadmap
/// Phase 3.3): the file's single best-matching chunk for the query.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PprFileChunk {
    pub file_id: i64,
    pub relative_path: String,
    pub language: String,
    pub content: String,
    pub start_line: i32,
    pub end_line: i32,
    pub similarity: f64,
}

/// Choose the active embedding column for a query vector by its dimensionality.
/// BGE-M3-only: the sole supported dim is 1024 → `embedding_v2`.
pub(crate) fn embedding_column_for_dim(dim: usize) -> Result<&'static str, sqlx::Error> {
    if dim != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "unsupported query-embedding dim {dim} (expected a 1024-dimension BGE-M3 embedding)"
        )));
    }
    Ok("embedding_v2")
}

/// Seed files for code-PPR retrieval (Phase 3.3): the distinct files of the top
/// `limit` chunks nearest to `embedding` within the project, each with its best
/// (max) chunk cosine similarity. HNSW-accelerated via the top-`limit` chunk
/// scan; aggregated to files in SQL. Returns `(file_id, similarity)` desc.
pub async fn ppr_seed_files(
    pool: &PgPool,
    embedding: &[f32],
    project_id: i32,
    limit: i32,
    ef_search: i32,
) -> Result<Vec<(i64, f64)>, sqlx::Error> {
    let col = embedding_column_for_dim(embedding.len())?;
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());
    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL hnsw.ef_search = {}",
        ef_search
    )))
    .execute(&mut *tx)
    .await?;
    let rows = sqlx::query_as::<_, (i64, f64)>(sqlx::AssertSqlSafe(format!(
        "SELECT file_id, MAX(sim)::float8 AS sim FROM (
            SELECT c.file_id, (1.0 - (c.{col} <=> $1)) AS sim
            FROM file_chunks c
            JOIN indexed_files f ON f.id = c.file_id
            WHERE f.project_id = $2 AND c.{col} IS NOT NULL
            ORDER BY c.{col} <=> $1
            LIMIT $3
         ) t
         GROUP BY file_id
         ORDER BY sim DESC"
    )))
    .bind(embedding_vec)
    .bind(project_id)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// A RAPTOR-over-code summary hit (graph-roadmap Phase 3.3): a cluster-level
/// "module gist" with its cosine similarity to the query.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CodeRaptorSummary {
    pub project_name: String,
    pub summary_text: String,
    pub member_count: i32,
    pub member_paths: Vec<String>,
    pub top_topics: Vec<String>,
    pub similarity: f64,
}

/// Query the RAPTOR-over-code summary tree: the level-1 cluster summaries whose
/// centroid embeddings are nearest the query. `project=None` searches across
/// all projects (the cross-project conceptual-query use case). Sequential
/// cosine scan — the table is small (k≈3-24 rows per project), so no HNSW.
pub async fn code_raptor_search(
    pool: &PgPool,
    embedding: &[f32],
    project: Option<&str>,
    k: i32,
) -> Result<Vec<CodeRaptorSummary>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "code_raptor_search: expected 1024-d (BGE-M3) embedding, got {}",
            embedding.len()
        )));
    }
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());
    sqlx::query_as::<_, CodeRaptorSummary>(
        "SELECT p.name AS project_name, t.summary_text, t.member_count,
                t.member_paths, t.top_topics,
                (1.0 - (t.summary_embedding <=> $1))::float8 AS similarity
         FROM code_summary_tree t
         JOIN projects p ON p.id = t.project_id
         WHERE ($2::text IS NULL OR p.name = $2)
         ORDER BY t.summary_embedding <=> $1
         LIMIT $3",
    )
    .bind(embedding_vec)
    .bind(project)
    .bind(k)
    .fetch_all(pool)
    .await
}

/// One class's arithmetic CK metrics (graph-roadmap Phase 4.3), aggregated in
/// SQL from `file_symbols` (methods via `parent_id`), `function_metrics`
/// (cyclomatic), and `symbol_references` (calls). DIT/NOC are computed
/// separately from the inheritance edges.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CkClassRow {
    pub symbol_id: i64,
    pub name: String,
    pub relative_path: String,
    pub method_count: i64,
    /// Weighted Methods per Class = Σ cyclomatic over the class's methods.
    pub wmc: i64,
    /// Distinct call targets from the class's methods (the "called" half of RFC).
    pub distinct_callees: i64,
    /// Coupling Between Objects proxy: distinct target files the class's methods
    /// reference.
    pub cbo: i64,
}

/// Load per-class CK arithmetic metrics for a project. "Classes" are
/// `file_symbols` of an OO kind (class/struct/interface/trait/enum); their
/// methods are rows with `parent_id = class.id`.
pub async fn ck_class_rows(pool: &PgPool, project_id: i32) -> Result<Vec<CkClassRow>, sqlx::Error> {
    sqlx::query_as::<_, CkClassRow>(
        "SELECT c.id AS symbol_id, c.name, f.relative_path,
                COUNT(DISTINCT m.id) AS method_count,
                COALESCE(SUM(fm.cyclomatic), 0)::int8 AS wmc,
                COUNT(DISTINCT sr.target_raw)
                    FILTER (WHERE sr.ref_kind = 'call') AS distinct_callees,
                COUNT(DISTINCT sr.target_file_id)
                    FILTER (WHERE sr.ref_kind = 'call' AND sr.target_file_id IS NOT NULL) AS cbo
         FROM file_symbols c
         JOIN indexed_files f ON f.id = c.file_id
         LEFT JOIN file_symbols m ON m.parent_id = c.id
         LEFT JOIN function_metrics fm ON fm.function_id = m.id
         LEFT JOIN symbol_references sr ON sr.source_symbol_id = m.id
         WHERE f.project_id = $1
           AND c.kind IN ('class', 'struct', 'interface', 'trait', 'enum')
         GROUP BY c.id, c.name, f.relative_path",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// Load resolved inheritance/impl edges for a project as `(child_symbol_id,
/// parent_symbol_id)` pairs, for DIT/NOC. (Phase 4.3)
pub async fn ck_inheritance_edges(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<(i64, i64)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT sr.source_symbol_id, sr.target_symbol_id
         FROM symbol_references sr
         JOIN indexed_files f ON f.id = sr.source_file_id
         WHERE f.project_id = $1
           AND sr.ref_kind IN ('inherit', 'impl')
           AND sr.source_symbol_id IS NOT NULL
           AND sr.target_symbol_id IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// Update `function_metrics.fan_in` / `fan_out` for a batch of function IDs.
/// Rows whose function_id has no row in `function_metrics` are silently
/// ignored (their metrics row hasn't been computed yet; the next
/// function-metrics cron pass will populate it).
pub async fn update_function_fan_io(
    pool: &PgPool,
    triples: &[(i64, i32, i32)], // (function_id, fan_in, fan_out)
) -> Result<u64, sqlx::Error> {
    if triples.is_empty() {
        return Ok(0);
    }
    let ids: Vec<i64> = triples.iter().map(|(i, _, _)| *i).collect();
    let fis: Vec<i32> = triples.iter().map(|(_, fi, _)| *fi).collect();
    let fos: Vec<i32> = triples.iter().map(|(_, _, fo)| *fo).collect();
    let res = sqlx::query(
        "UPDATE function_metrics
         SET fan_in = u.fan_in, fan_out = u.fan_out
         FROM UNNEST($1::int8[], $2::int4[], $3::int4[]) AS u(function_id, fan_in, fan_out)
         WHERE function_metrics.function_id = u.function_id",
    )
    .bind(&ids)
    .bind(&fis)
    .bind(&fos)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Bulk-update the call-graph-derived centrality columns on `function_metrics`,
/// keyed by `function_id` (= `file_symbols.id` = the call graph's `symbol_id`).
/// Owned by the call-graph cron; `upsert_function_metrics_batch` deliberately
/// omits these columns from its ON CONFLICT clause so a metrics pass never
/// resets them. Mirrors `update_function_fan_io`'s UNNEST shape.
#[allow(clippy::type_complexity)]
pub async fn update_function_centralities(
    pool: &PgPool,
    // (function_id, pagerank, betweenness, community_id, coreness, harmonic)
    rows: &[(i64, f64, f64, i32, i32, f64)],
) -> Result<u64, sqlx::Error> {
    if rows.is_empty() {
        return Ok(0);
    }
    let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let pr: Vec<f64> = rows.iter().map(|r| r.1).collect();
    let btw: Vec<f64> = rows.iter().map(|r| r.2).collect();
    let comm: Vec<i32> = rows.iter().map(|r| r.3).collect();
    let core: Vec<i32> = rows.iter().map(|r| r.4).collect();
    let harm: Vec<f64> = rows.iter().map(|r| r.5).collect();
    let res = sqlx::query(
        "UPDATE function_metrics
         SET pagerank = u.pagerank,
             betweenness = u.betweenness,
             community_id = u.community_id,
             coreness = u.coreness,
             harmonic = u.harmonic
         FROM UNNEST($1::int8[], $2::float8[], $3::float8[], $4::int4[], $5::int4[], $6::float8[])
              AS u(function_id, pagerank, betweenness, community_id, coreness, harmonic)
         WHERE function_metrics.function_id = u.function_id",
    )
    .bind(&ids)
    .bind(&pr)
    .bind(&btw)
    .bind(&comm)
    .bind(&core)
    .bind(&harm)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Read the call-graph watermark for a project.
pub async fn get_call_graph_watermark(
    pool: &PgPool,
    project_id: i32,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let key = format!("call_graph_last_run:{}", project_id);
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

/// Set the call-graph watermark for a project.
pub async fn set_call_graph_watermark(
    pool: &PgPool,
    project_id: i32,
    ts: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let key = format!("call_graph_last_run:{}", project_id);
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
