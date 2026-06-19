//! Cron job: Graph analysis — import extraction, graph construction, metric computation.
//!
//! Extracts import dependencies from indexed file content, builds a petgraph DiGraph,
//! computes PageRank, betweenness centrality, degree metrics, coupling/instability,
//! and stores results in `code_graph_edges` and `file_metrics` tables.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{error, info};

use crate::db::DbClient;
use crate::graph::algorithms;
use crate::graph::builder::{FileMetaRow, GraphEdgeRow};
use crate::graph::import_extractor;
use crate::graph::metrics;
use crate::stats::tracker::StatsTracker;

/// Metadata-only row (no content) — cheap to fetch in bulk.
#[derive(Debug, sqlx::FromRow)]
struct FileMetaLite {
    file_id: i64,
    relative_path: String,
    language: String,
}

/// Content-carrying row for streaming imports extraction.
#[derive(Debug, sqlx::FromRow)]
struct FileContentLite {
    file_id: i64,
    relative_path: String,
    language: String,
    content: Option<String>,
}

/// Size of each content-fetch batch. Peak content RAM per project ≈
/// batch_size × avg_file_size (typically ~10-50 KB per file ⇒ 2.5-12 MB
/// per batch, fully released between batches).
const CONTENT_BATCH_SIZE: usize = 256;

/// Run the full graph analysis pipeline for all projects.
///
/// Accepts an optional `Arc<WorkPool>`. When provided, Brandes betweenness
/// centrality is parallelized across the pool (Phase 6). When `None`, falls
/// back to the sequential single-threaded implementation.
pub async fn run_graph_analysis(
    db: &dyn DbClient,
    stats: &Arc<StatsTracker>,
    work_pool: Option<Arc<crate::work_pool::pool::WorkPool>>,
) {
    // Graph analysis is built on inline SQL (see this file's many
    // `sqlx::query(...).fetch_all(pool)` sites). The DbClient trait does
    // not yet model these queries; until it does, we unwrap a real PgPool
    // here. With the production `impl DbClient for PgPool`, this is the
    // backing pool. With a mock backend (no inline SQL support), this
    // panics — graph analysis is not unit-testable through the trait.
    let pool = db
        .pool()
        .expect("graph_analysis requires a real &PgPool — DbClient backend must be PgPool-backed");

    info!("Starting graph analysis cron job");
    let start = std::time::Instant::now();

    // Promoted to top-of-body: this counter means "the body reached its
    // work-eligible state" — pairs with `graph_build_noop_returns` to
    // distinguish "ran, no work" from "never ran".
    stats.graph_build_runs.fetch_add(1, Ordering::Relaxed);

    // Get all projects
    let projects: Vec<(i32, String)> =
        match sqlx::query_as::<_, (i32, String)>("SELECT id, name FROM projects ORDER BY id")
            .fetch_all(pool)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to list projects for graph analysis: {}", e);
                return;
            }
        };

    if projects.is_empty() {
        stats
            .graph_build_noop_returns
            .fetch_add(1, Ordering::Relaxed);
        info!("Graph analysis cron job: no projects to analyze");
        return;
    }

    for (project_id, project_name) in &projects {
        if let Err(e) = analyze_project(pool, *project_id, project_name, work_pool.as_ref()).await {
            error!(
                project = %project_name,
                error = %e,
                "Graph analysis failed for project"
            );
        }
    }

    // Hierarchical rollup (ADR-027): now that every project's project_metrics row
    // exists, aggregate up to groups + the workspace. Non-fatal.
    if let Err(e) = crate::hierarchy::rollup::persist_group_workspace_rollup(pool).await {
        error!(error = %e, "group/workspace rollup failed");
    }

    info!(
        elapsed_ms = start.elapsed().as_millis() as u64,
        projects = projects.len(),
        "Graph analysis cron job complete"
    );
}

/// Analyze a single project: extract imports, build graph, compute metrics.
///
/// Two-phase content fetch (Phase 3 of OOM fix):
///   Phase A: metadata only — id, path, language — small (~50 bytes/file).
///   Phase B: content in batches of 256 file_ids — extract imports, drop batch.
/// Peak content in RAM per project ≈ 256 × avg_file_size (~12 MB for 50KB avg).
/// Previously: `fetch_all` of all file content simultaneously (~300 MB for a
/// 7000-file project with 50 KB avg content).
async fn analyze_project(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
    work_pool: Option<&Arc<crate::work_pool::pool::WorkPool>>,
) -> Result<(), sqlx::Error> {
    // Phase A: fetch metadata only (no content).
    let metas: Vec<FileMetaLite> = sqlx::query_as::<_, FileMetaLite>(
        "SELECT id as file_id, relative_path, language
         FROM indexed_files
         WHERE project_id = $1 AND content IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;

    if metas.is_empty() {
        return Ok(());
    }

    let file_paths: HashMap<String, i64> = metas
        .iter()
        .map(|m| (m.relative_path.clone(), m.file_id))
        .collect();

    // Phase B: stream content in 256-file batches, extract imports, drop each
    // batch before loading the next. `all_edges` stays live but is bounded by
    // the actual number of import statements in the project (typically
    // 5-15× file_count).
    //
    // Tier-0e dispatch: files with rows in `symbol_references` skip the regex
    // path and pull import_use rows directly. Files NOT in `symbol_set` (no
    // backend or symbol-extraction cron hasn't run yet) take the regex path.
    let mut all_edges: Vec<ImportEdge> = Vec::new();
    let file_ids: Vec<i64> = metas.iter().map(|m| m.file_id).collect();

    let symbol_set =
        crate::db::queries::file_ids_with_symbol_refs(pool, project_id, &file_ids).await?;

    // Build a per-file language map so the symbol-dispatch path can call
    // `resolve_import_candidates` without re-fetching content.
    let file_languages: HashMap<i64, String> = metas
        .iter()
        .map(|m| (m.file_id, m.language.clone()))
        .collect();
    let file_paths_by_id: HashMap<i64, String> = metas
        .iter()
        .map(|m| (m.file_id, m.relative_path.clone()))
        .collect();

    if !symbol_set.is_empty() {
        let symbol_imports =
            crate::db::queries::get_imports_from_symbols(pool, project_id, &file_ids).await?;
        for imp in &symbol_imports {
            let target_file_id = imp.target_file_id.or_else(|| {
                let language = file_languages.get(&imp.source_file_id)?;
                let source_path = file_paths_by_id.get(&imp.source_file_id)?;
                let raw = synthesize_raw_import(&imp.target_raw, language);
                let candidates =
                    import_extractor::resolve_import_candidates(&raw, source_path, language);
                candidates
                    .iter()
                    .find_map(|c| file_paths.get(c.as_str()))
                    .copied()
            });
            all_edges.push(ImportEdge {
                source_file_id: imp.source_file_id,
                target_file_id,
                target_raw: imp.target_raw.clone(),
                edge_type: "import".to_string(),
                weight: 1.0,
            });
        }
    }

    for batch_ids in file_ids.chunks(CONTENT_BATCH_SIZE) {
        // Skip files that took the symbol-aware path above.
        let regex_batch_ids: Vec<i64> = batch_ids
            .iter()
            .copied()
            .filter(|id| !symbol_set.contains(id))
            .collect();
        if regex_batch_ids.is_empty() {
            continue;
        }
        let batch: Vec<FileContentLite> = sqlx::query_as::<_, FileContentLite>(
            "SELECT id as file_id, relative_path, language, content
             FROM indexed_files
             WHERE project_id = $1 AND id = ANY($2::bigint[]) AND content IS NOT NULL",
        )
        .bind(project_id)
        .bind(&regex_batch_ids)
        .fetch_all(pool)
        .await?;

        for file in &batch {
            let content = match &file.content {
                Some(c) => c,
                None => continue,
            };

            let imports = import_extractor::extract_imports(content, &file.language);

            for import in &imports {
                let candidates = import_extractor::resolve_import_candidates(
                    import,
                    &file.relative_path,
                    &file.language,
                );

                let target_file_id = candidates
                    .iter()
                    .find_map(|candidate| file_paths.get(candidate.as_str()))
                    .copied();

                all_edges.push(ImportEdge {
                    source_file_id: file.file_id,
                    target_file_id,
                    target_raw: import.raw_path.clone(),
                    edge_type: "import".to_string(),
                    weight: 1.0,
                });
            }
        }
        // `batch` dropped here — content strings freed before next fetch.
    }

    // Step 3: Clear old import edges and insert new ones via UNNEST batch.
    sqlx::query("DELETE FROM code_graph_edges WHERE project_id = $1 AND edge_type = 'import'")
        .bind(project_id)
        .execute(pool)
        .await?;

    for batch in all_edges.chunks(500) {
        insert_edges_batch(pool, project_id, batch).await?;
    }

    info!(
        project = %project_name,
        import_edges = all_edges.len(),
        "Import extraction complete"
    );

    // Step 4: Add co-change edges from git history (reuses file_paths — no
    // per-pair SELECT lookups after Phase 3).
    let co_change_count =
        compute_and_store_cochange_edges(pool, project_id, 0.2, &file_paths).await?;

    // Step 5: Build graph and compute metrics
    let edge_rows = load_graph_edges(pool, project_id).await?;
    let file_metas: Vec<FileMetaRow> = metas
        .iter()
        .map(|m| FileMetaRow {
            file_id: m.file_id,
            relative_path: m.relative_path.clone(),
            language: m.language.clone(),
        })
        .collect();

    let code_graph = crate::graph::builder::build_graph(&edge_rows, &file_metas);

    if code_graph.node_count() == 0 {
        return Ok(());
    }

    // Compute PageRank
    let pr = algorithms::pagerank(&code_graph.graph, 0.85, 100, 1e-8);

    // Compute betweenness centrality (parallel when a WorkPool is provided).
    let betweenness = match work_pool {
        Some(wp) => algorithms::betweenness_centrality_parallel(&code_graph.graph, wp, None),
        None => algorithms::betweenness_centrality(&code_graph.graph),
    };

    // Compute degrees
    let degrees = algorithms::compute_degrees(&code_graph.graph);

    // Compute module metrics for coupling
    let module_metrics = metrics::compute_module_metrics(&code_graph, 2);

    // Build per-file Ca/Ce from module metrics
    let mut file_coupling: HashMap<i64, (i32, i32)> = HashMap::new();
    for mm in &module_metrics {
        for file_path in &mm.files {
            if let Some(&file_id) = file_paths.get(file_path) {
                file_coupling.insert(
                    file_id,
                    (mm.afferent_coupling as i32, mm.efferent_coupling as i32),
                );
            }
        }
    }

    // Hierarchical rollup (ADR-027): persist module_metrics + project_metrics now
    // that module_metrics are computed (cheap add at the point the data exists).
    // Non-fatal — a rollup failure must not abort the graph-analysis pass.
    if let Err(e) =
        crate::hierarchy::rollup::persist_project_rollup(pool, project_id, &module_metrics).await
    {
        tracing::error!(project_id, error = %e, "hierarchical rollup persist failed");
    }

    // Compute churn metrics from git history
    let churn_data = load_churn_data(pool, project_id).await?;

    // Step 6: Upsert file_metrics
    let mut metrics_rows: Vec<FileMetricsRow> = Vec::new();

    for &node_idx in code_graph.graph.node_indices().collect::<Vec<_>>().iter() {
        let file_id = match code_graph.node_to_file_id.get(&node_idx) {
            Some(&id) => id,
            None => continue,
        };

        let (in_deg, out_deg) = degrees.get(&node_idx).copied().unwrap_or((0, 0));
        let pr_score = pr.scores.get(&node_idx).copied().unwrap_or(0.0);
        let between = betweenness.get(&node_idx).copied().unwrap_or(0.0);
        let (ca, ce) = file_coupling.get(&file_id).copied().unwrap_or((0, 0));
        let instability = if ca + ce > 0 {
            ce as f64 / (ca + ce) as f64
        } else {
            0.0
        };

        let churn = churn_data.get(&file_id);

        metrics_rows.push(FileMetricsRow {
            file_id,
            project_id,
            pagerank: pr_score,
            betweenness: between,
            in_degree: in_deg as i32,
            out_degree: out_deg as i32,
            afferent_coupling: ca,
            efferent_coupling: ce,
            instability,
            commit_count: churn.map(|c| c.commit_count).unwrap_or(0),
            author_count: churn.map(|c| c.author_count).unwrap_or(0),
            fix_commit_ratio: churn.map(|c| c.fix_commit_ratio).unwrap_or(0.0),
            churn_rate: churn.map(|c| c.churn_rate).unwrap_or(0.0),
            days_since_last_change: churn.and_then(|c| c.days_since_last_change),
        });
    }

    // Batch upsert
    for batch in metrics_rows.chunks(500) {
        upsert_file_metrics_batch(pool, batch).await?;
    }

    info!(
        project = %project_name,
        nodes = code_graph.node_count(),
        edges = code_graph.edge_count(),
        metrics = metrics_rows.len(),
        co_change = co_change_count,
        "Graph metrics computation complete"
    );

    Ok(())
}

// ============================================================================
// Helper types and functions
// ============================================================================

struct ImportEdge {
    source_file_id: i64,
    target_file_id: Option<i64>,
    target_raw: String,
    edge_type: String,
    weight: f64,
}

/// Synthesize a `RawImport` from a `symbol_references.target_raw` string so
/// the existing per-language `resolve_import_candidates` resolver can be
/// reused without re-parsing source content. `kind` is heuristically derived
/// because the parsing layer collapses Rust's `use`/`mod`/`extern_crate` to
/// a single `Import` shape.
fn synthesize_raw_import(target_raw: &str, language: &str) -> import_extractor::RawImport {
    let kind = match language {
        "rust" => {
            if target_raw.contains("::") {
                "use"
            } else {
                "mod"
            }
        }
        _ => "import",
    };
    import_extractor::RawImport {
        raw_path: target_raw.to_string(),
        kind: kind.to_string(),
    }
}

struct FileMetricsRow {
    file_id: i64,
    project_id: i32,
    pagerank: f64,
    betweenness: f64,
    in_degree: i32,
    out_degree: i32,
    afferent_coupling: i32,
    efferent_coupling: i32,
    instability: f64,
    commit_count: i32,
    author_count: i32,
    fix_commit_ratio: f64,
    churn_rate: f64,
    days_since_last_change: Option<i32>,
}

struct ChurnData {
    commit_count: i32,
    author_count: i32,
    fix_commit_ratio: f64,
    churn_rate: f64,
    days_since_last_change: Option<i32>,
}

/// Insert a batch of import edges via a single UNNEST-based INSERT.
/// Previously this did N sequential `INSERT ... VALUES` queries; the new
/// implementation is one round-trip per batch (500 edges), on the order of
/// 100× faster for a typical project.
///
/// Deduplicates by conflict key before binding. PG rejects bulk INSERT
/// with ON CONFLICT when the same conflict-key tuple appears more than
/// once in the input ("ON CONFLICT DO UPDATE command cannot affect row
/// a second time"). Import extraction can produce duplicates when a
/// file has multiple imports that resolve to the same `target_file_id`
/// AND share the same `target_raw` (e.g., the same raw `use foo;`
/// appearing twice, or a regex extracting an overlapping pattern). The
/// edge_type is constant per call site so we exclude it from the
/// per-batch key to save string clones.
async fn insert_edges_batch(
    pool: &PgPool,
    project_id: i32,
    edges: &[ImportEdge],
) -> Result<(), sqlx::Error> {
    if edges.is_empty() {
        return Ok(());
    }

    use std::collections::HashSet;
    let mut seen: HashSet<(i64, i64, &str)> = HashSet::with_capacity(edges.len());
    let dedup: Vec<&ImportEdge> = edges
        .iter()
        .filter(|e| {
            seen.insert((
                e.source_file_id,
                e.target_file_id.unwrap_or(-1),
                e.target_raw.as_str(),
            ))
        })
        .collect();

    let project_ids: Vec<i32> = vec![project_id; dedup.len()];
    let source_ids: Vec<i64> = dedup.iter().map(|e| e.source_file_id).collect();
    let target_ids: Vec<Option<i64>> = dedup.iter().map(|e| e.target_file_id).collect();
    let edge_types: Vec<String> = dedup.iter().map(|e| e.edge_type.clone()).collect();
    let target_raws: Vec<String> = dedup.iter().map(|e| e.target_raw.clone()).collect();
    let weights: Vec<f64> = dedup.iter().map(|e| e.weight).collect();

    sqlx::query(
        "INSERT INTO code_graph_edges (project_id, source_file_id, target_file_id, edge_type, target_raw, weight)
         SELECT * FROM UNNEST(
             $1::int4[], $2::int8[], $3::int8[], $4::text[], $5::text[], $6::float8[]
         )
         ON CONFLICT (source_file_id, COALESCE(target_file_id, -1::BIGINT), edge_type, COALESCE(target_raw, ''))
         DO UPDATE SET weight = EXCLUDED.weight, computed_at = NOW()"
    )
    .bind(&project_ids)
    .bind(&source_ids)
    .bind(&target_ids)
    .bind(&edge_types)
    .bind(&target_raws)
    .bind(&weights)
    .execute(pool)
    .await?;

    Ok(())
}

async fn load_graph_edges(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<GraphEdgeRow>, sqlx::Error> {
    let rows = sqlx::query_as::<_, GraphEdgeRowDb>(
        "SELECT
            e.source_file_id,
            sf.relative_path as source_relative_path,
            sf.language as source_language,
            e.target_file_id,
            tf.relative_path as target_relative_path,
            tf.language as target_language,
            e.edge_type,
            e.weight
         FROM code_graph_edges e
         JOIN indexed_files sf ON e.source_file_id = sf.id
         LEFT JOIN indexed_files tf ON e.target_file_id = tf.id
         WHERE e.project_id = $1",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| GraphEdgeRow {
            source_file_id: r.source_file_id,
            source_relative_path: r.source_relative_path,
            source_language: r.source_language,
            target_file_id: r.target_file_id,
            target_relative_path: r.target_relative_path,
            target_language: r.target_language,
            edge_type: r.edge_type,
            weight: r.weight,
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct GraphEdgeRowDb {
    source_file_id: i64,
    source_relative_path: String,
    source_language: String,
    target_file_id: Option<i64>,
    target_relative_path: Option<String>,
    target_language: Option<String>,
    edge_type: String,
    weight: f64,
}

async fn compute_and_store_cochange_edges(
    pool: &PgPool,
    project_id: i32,
    min_jaccard: f64,
    file_paths: &HashMap<String, i64>,
) -> Result<usize, sqlx::Error> {
    // Clear old co-change edges
    sqlx::query("DELETE FROM code_graph_edges WHERE project_id = $1 AND edge_type = 'co_change'")
        .bind(project_id)
        .execute(pool)
        .await?;

    // The Jaccard aggregation is O(commits × files²) on the PG side and
    // can pin a connection for several minutes on large histories. Raise
    // the per-transaction statement_timeout so the daemon-wide ceiling
    // doesn't fire mid-compute.
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '10min'")
        .execute(&mut *tx)
        .await?;
    // Label this heavy transaction for the graceful-shutdown sweep
    // (db::admin::terminate_heavy_backends).
    sqlx::query("SET LOCAL application_name = 'pgmcp:heavy:graph-analysis'")
        .execute(&mut *tx)
        .await?;

    // Compute Jaccard similarity from git_commit_files
    let pairs = sqlx::query_as::<_, CoChangePairDb>(
        "WITH file_commits AS (
            SELECT gcf.file_path, gc.id as commit_id
            FROM git_commit_files gcf
            JOIN git_commits gc ON gcf.commit_id = gc.id
            WHERE gc.project_id = $1
        ),
        file_pairs AS (
            SELECT
                a.file_path as path_a,
                b.file_path as path_b,
                COUNT(DISTINCT a.commit_id) FILTER (WHERE a.commit_id = b.commit_id) as co_commits,
                COUNT(DISTINCT a.commit_id) as commits_a,
                COUNT(DISTINCT b.commit_id) as commits_b
            FROM file_commits a
            JOIN file_commits b ON a.commit_id = b.commit_id AND a.file_path < b.file_path
            GROUP BY a.file_path, b.file_path
            HAVING COUNT(DISTINCT a.commit_id) FILTER (WHERE a.commit_id = b.commit_id) >= 3
        )
        SELECT
            path_a, path_b,
            co_commits::DOUBLE PRECISION / (commits_a + commits_b - co_commits)::DOUBLE PRECISION as jaccard
        FROM file_pairs
        WHERE co_commits::DOUBLE PRECISION / (commits_a + commits_b - co_commits)::DOUBLE PRECISION >= $2"
    )
    .bind(project_id)
    .bind(min_jaccard)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    // Resolve file paths to IDs using the in-memory map (no per-pair SELECTs).
    // Build parallel Vecs for a single UNNEST-based batch INSERT.
    //
    // Deduplicate by `(source_file_id, target_file_id)` — the conflict key
    // PG sees is `(source_file_id, COALESCE(target_file_id, -1), edge_type,
    // COALESCE(target_raw, ''))`, but for co-change edges edge_type and
    // target_raw are constant within this batch ("co_change", ""), so the
    // file-id pair fully determines uniqueness. The upstream SQL
    // (`GROUP BY a.file_path, b.file_path` with `a < b`) already produces
    // unique path-pairs, but if `file_paths` has multiple aliases mapping
    // to the same file_id, the same (sid, tid) pair could appear twice —
    // which PG rejects as "ON CONFLICT DO UPDATE command cannot affect
    // row a second time".
    use std::collections::HashSet;
    let mut seen: HashSet<(i64, i64)> = HashSet::with_capacity(pairs.len());
    let mut project_ids = Vec::<i32>::with_capacity(pairs.len());
    let mut source_ids = Vec::<i64>::with_capacity(pairs.len());
    let mut target_ids = Vec::<Option<i64>>::with_capacity(pairs.len());
    let mut edge_types = Vec::<String>::with_capacity(pairs.len());
    let mut target_raws = Vec::<String>::with_capacity(pairs.len());
    let mut weights = Vec::<f64>::with_capacity(pairs.len());

    for pair in &pairs {
        let (sid, tid) = match (
            file_paths.get(&pair.path_a).copied(),
            file_paths.get(&pair.path_b).copied(),
        ) {
            (Some(s), Some(t)) => (s, t),
            _ => continue,
        };
        if !seen.insert((sid, tid)) {
            continue;
        }
        project_ids.push(project_id);
        source_ids.push(sid);
        target_ids.push(Some(tid));
        edge_types.push("co_change".to_string());
        target_raws.push(String::new()); // empty; not used for co_change
        weights.push(pair.jaccard);
    }

    let count = project_ids.len();
    if count == 0 {
        return Ok(0);
    }

    // Single UNNEST-based INSERT for all co-change edges.
    sqlx::query(
        "INSERT INTO code_graph_edges (project_id, source_file_id, target_file_id, edge_type, target_raw, weight)
         SELECT * FROM UNNEST(
             $1::int4[], $2::int8[], $3::int8[], $4::text[], $5::text[], $6::float8[]
         )
         ON CONFLICT (source_file_id, COALESCE(target_file_id, -1::BIGINT), edge_type, COALESCE(target_raw, ''))
         DO UPDATE SET weight = EXCLUDED.weight, computed_at = NOW()"
    )
    .bind(&project_ids)
    .bind(&source_ids)
    .bind(&target_ids)
    .bind(&edge_types)
    .bind(&target_raws)
    .bind(&weights)
    .execute(pool)
    .await?;

    Ok(count)
}

#[derive(sqlx::FromRow)]
struct CoChangePairDb {
    path_a: String,
    path_b: String,
    jaccard: f64,
}

async fn load_churn_data(
    pool: &PgPool,
    project_id: i32,
) -> Result<HashMap<i64, ChurnData>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ChurnRowDb>(
        "WITH file_churn AS (
            SELECT
                f.id as file_id,
                COUNT(DISTINCT gc.id) as commit_count,
                COUNT(DISTINCT gc.author) as author_count,
                COUNT(DISTINCT gc.id) FILTER (
                    WHERE gc.subject ~* '(fix|bug|patch|hotfix|resolve|closes?|fixes)'
                ) as fix_commits,
                MAX(gc.author_date) as last_commit_date
            FROM indexed_files f
            JOIN git_commit_files gcf ON gcf.file_path = f.relative_path
            JOIN git_commits gc ON gcf.commit_id = gc.id AND gc.project_id = f.project_id
            WHERE f.project_id = $1
            GROUP BY f.id
        )
        SELECT
            file_id,
            commit_count::INTEGER,
            author_count::INTEGER,
            CASE WHEN commit_count > 0
                THEN fix_commits::DOUBLE PRECISION / commit_count::DOUBLE PRECISION
                ELSE 0.0
            END as fix_commit_ratio,
            commit_count::DOUBLE PRECISION / GREATEST(
                EXTRACT(EPOCH FROM (NOW() - COALESCE(last_commit_date, NOW()))) / 86400.0 / 30.0,
                1.0
            ) as churn_rate,
            EXTRACT(EPOCH FROM (NOW() - last_commit_date))::INTEGER / 86400 as days_since_last_change
        FROM file_churn"
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;

    let mut map = HashMap::new();
    for row in rows {
        map.insert(
            row.file_id,
            ChurnData {
                commit_count: row.commit_count,
                author_count: row.author_count,
                fix_commit_ratio: row.fix_commit_ratio,
                churn_rate: row.churn_rate,
                days_since_last_change: row.days_since_last_change,
            },
        );
    }
    Ok(map)
}

#[derive(sqlx::FromRow)]
struct ChurnRowDb {
    file_id: i64,
    commit_count: i32,
    author_count: i32,
    fix_commit_ratio: f64,
    churn_rate: f64,
    days_since_last_change: Option<i32>,
}

/// Upsert a batch of file_metrics rows via a single UNNEST-based INSERT.
/// One round-trip per batch of 500 rows instead of 500 sequential queries.
async fn upsert_file_metrics_batch(
    pool: &PgPool,
    rows: &[FileMetricsRow],
) -> Result<(), sqlx::Error> {
    if rows.is_empty() {
        return Ok(());
    }

    let file_ids: Vec<i64> = rows.iter().map(|r| r.file_id).collect();
    let project_ids: Vec<i32> = rows.iter().map(|r| r.project_id).collect();
    let pageranks: Vec<f64> = rows.iter().map(|r| r.pagerank).collect();
    let betweennesses: Vec<f64> = rows.iter().map(|r| r.betweenness).collect();
    let in_degrees: Vec<i32> = rows.iter().map(|r| r.in_degree).collect();
    let out_degrees: Vec<i32> = rows.iter().map(|r| r.out_degree).collect();
    let aff: Vec<i32> = rows.iter().map(|r| r.afferent_coupling).collect();
    let eff: Vec<i32> = rows.iter().map(|r| r.efferent_coupling).collect();
    let insts: Vec<f64> = rows.iter().map(|r| r.instability).collect();
    let commits: Vec<i32> = rows.iter().map(|r| r.commit_count).collect();
    let authors: Vec<i32> = rows.iter().map(|r| r.author_count).collect();
    let fix_ratios: Vec<f64> = rows.iter().map(|r| r.fix_commit_ratio).collect();
    let churn_rates: Vec<f64> = rows.iter().map(|r| r.churn_rate).collect();
    let days: Vec<Option<i32>> = rows.iter().map(|r| r.days_since_last_change).collect();

    sqlx::query(
        "INSERT INTO file_metrics (
            file_id, project_id,
            pagerank, betweenness, in_degree, out_degree,
            afferent_coupling, efferent_coupling, instability,
            commit_count, author_count, fix_commit_ratio, churn_rate,
            days_since_last_change, computed_at
        )
        SELECT file_id, project_id,
               pagerank, betweenness, in_degree, out_degree,
               aff, eff, inst,
               commit_count, author_count, fix_ratio, churn_rate,
               days_since, NOW()
        FROM UNNEST(
            $1::int8[], $2::int4[],
            $3::float8[], $4::float8[], $5::int4[], $6::int4[],
            $7::int4[], $8::int4[], $9::float8[],
            $10::int4[], $11::int4[], $12::float8[], $13::float8[],
            $14::int4[]
        ) AS u(file_id, project_id, pagerank, betweenness, in_degree, out_degree,
               aff, eff, inst, commit_count, author_count, fix_ratio, churn_rate,
               days_since)
        ON CONFLICT (file_id) DO UPDATE SET
            project_id = EXCLUDED.project_id,
            pagerank = EXCLUDED.pagerank,
            betweenness = EXCLUDED.betweenness,
            in_degree = EXCLUDED.in_degree,
            out_degree = EXCLUDED.out_degree,
            afferent_coupling = EXCLUDED.afferent_coupling,
            efferent_coupling = EXCLUDED.efferent_coupling,
            instability = EXCLUDED.instability,
            commit_count = EXCLUDED.commit_count,
            author_count = EXCLUDED.author_count,
            fix_commit_ratio = EXCLUDED.fix_commit_ratio,
            churn_rate = EXCLUDED.churn_rate,
            days_since_last_change = EXCLUDED.days_since_last_change,
            computed_at = NOW()",
    )
    .bind(&file_ids)
    .bind(&project_ids)
    .bind(&pageranks)
    .bind(&betweennesses)
    .bind(&in_degrees)
    .bind(&out_degrees)
    .bind(&aff)
    .bind(&eff)
    .bind(&insts)
    .bind(&commits)
    .bind(&authors)
    .bind(&fix_ratios)
    .bind(&churn_rates)
    .bind(&days)
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesize_rust_use_kind() {
        let r = synthesize_raw_import("crate::foo::bar", "rust");
        assert_eq!(r.raw_path, "crate::foo::bar");
        assert_eq!(r.kind, "use");
    }

    #[test]
    fn synthesize_rust_mod_kind_for_bare_name() {
        // `mod foo;` and `extern crate foo;` both produce bare names.
        // We pick "mod" so resolve_rust_import returns sibling-file candidates.
        let r = synthesize_raw_import("foo", "rust");
        assert_eq!(r.kind, "mod");
    }

    #[test]
    fn synthesize_python_kind() {
        let r = synthesize_raw_import("django.http", "python");
        assert_eq!(r.kind, "import");
    }

    #[test]
    fn synthesize_javascript_kind() {
        let r = synthesize_raw_import("./helpers", "javascript");
        assert_eq!(r.kind, "import");
    }

    #[test]
    fn synthesize_unknown_language_falls_back_to_import() {
        let r = synthesize_raw_import("anything", "unknown_lang");
        assert_eq!(r.kind, "import");
    }

    #[test]
    fn synthesize_preserves_raw_path_verbatim() {
        // The raw_path must be byte-identical to the input target_raw — the
        // resolver depends on the exact form for path-prefix matching
        // ("crate::foo::bar" must not become "foo::bar" or similar).
        let input = "std::collections::HashMap::new";
        let r = synthesize_raw_import(input, "rust");
        assert_eq!(r.raw_path, input);
    }

    #[test]
    fn synthesize_handles_empty_target_raw() {
        // Edge case: an empty target_raw shouldn't panic; falls to the
        // bare-name (kind=mod) path for rust, kind=import for others.
        let r = synthesize_raw_import("", "rust");
        assert_eq!(r.raw_path, "");
        assert_eq!(r.kind, "mod");
        let r = synthesize_raw_import("", "python");
        assert_eq!(r.kind, "import");
    }

    use proptest::prelude::*;

    proptest! {
        /// Any rust path containing `::` produces kind=use (so the resolver
        /// hits the use-arm and walks crate/super/self prefixes correctly).
        #[test]
        fn prop_rust_path_with_double_colon_is_use(
            head in "[a-z][a-z0-9_]{0,8}",
            tail in prop::collection::vec("[a-z][a-z0-9_]{0,8}", 1..4usize),
        ) {
            let path = format!("{}::{}", head, tail.join("::"));
            let r = synthesize_raw_import(&path, "rust");
            prop_assert_eq!(r.kind, "use");
            prop_assert_eq!(r.raw_path, path);
        }

        /// Any bare rust name (no `::`) produces kind=mod (so the resolver
        /// hits the mod-arm and looks for sibling-file candidates).
        #[test]
        fn prop_rust_bare_name_is_mod(
            name in "[a-z][a-z0-9_]{0,15}",
        ) {
            let r = synthesize_raw_import(&name, "rust");
            prop_assert_eq!(r.kind, "mod");
        }

        /// Non-rust languages always produce kind=import regardless of
        /// target_raw shape.
        #[test]
        fn prop_non_rust_always_import(
            target in "[a-zA-Z][a-zA-Z0-9_./:-]*",
            language in prop_oneof![
                Just("python"),
                Just("javascript"),
                Just("typescript"),
                Just("java"),
                Just("clojure"),
                Just("scala"),
                Just("go"),
                Just("unknown"),
            ],
        ) {
            let r = synthesize_raw_import(&target, language);
            prop_assert_eq!(r.kind, "import");
        }
    }
}
