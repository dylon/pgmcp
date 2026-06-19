//! Topic-clustering queries (store/load cached topics + assignments,
//! centroids, coverage, orphans, co-change coupling) plus the derived-health
//! signature/staleness helpers. Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// Read the algorithm signature the stored code-topics were computed with
/// (`pgmcp_metadata['topics_algo_signature']`). `None` means the topics predate
/// the signature mechanism (older tokenizer/label code) and must be treated as
/// stale. See `crate::cron::topic_clustering::TOPICS_ALGO_SIGNATURE`.
pub async fn topics_algo_signature(pool: &PgPool) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT value FROM pgmcp_metadata WHERE key = 'topics_algo_signature'",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Record the algorithm signature used for the topics just written. Called at
/// the end of a successful global topic scan.
pub async fn set_topics_algo_signature(pool: &PgPool, sig: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ('topics_algo_signature', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(sig)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist the latest topic-quality metrics for `scope` into
/// `pgmcp_metadata['topics_quality']` — a JSON object keyed by scope — and
/// append a snapshot to the bounded `topics_quality_history` array.
///
/// No schema change: `pgmcp_metadata` is a `(key text, value text)` table. This
/// is the authoritative quality store consulted by `orient` / the digest and is
/// what makes a topic-model regression *visible* (the breakage on 2026-06-13 was
/// invisible for ~3 weeks precisely because no such signal was ever stored).
pub async fn set_topic_quality(
    pool: &PgPool,
    scope: &str,
    metrics: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    let now = Utc::now().to_rfc3339();

    // Stamp scope + timestamp onto the snapshot.
    let mut entry = metrics.clone();
    if let Some(m) = entry.as_object_mut() {
        m.insert("scope".to_string(), serde_json::json!(scope));
        m.insert("computed_at".to_string(), serde_json::json!(now));
    }

    // Read-modify-write the per-scope object.
    let existing: Option<String> =
        sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = 'topics_quality'")
            .fetch_optional(pool)
            .await?;
    let mut obj = existing
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    obj.as_object_mut()
        .expect("obj is an object")
        .insert(scope.to_string(), entry.clone());
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ('topics_quality', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(obj.to_string())
    .execute(pool)
    .await?;

    // Append to the bounded history (most-recent-last, cap 500 snapshots).
    let hist_existing: Option<String> =
        sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = 'topics_quality_history'")
            .fetch_optional(pool)
            .await?;
    let mut hist = hist_existing
        .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(&s).ok())
        .unwrap_or_default();
    hist.push(entry);
    let len = hist.len();
    if len > 500 {
        hist.drain(0..len - 500);
    }
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ('topics_quality_history', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(serde_json::Value::Array(hist).to_string())
    .execute(pool)
    .await?;

    Ok(())
}

/// Read the per-scope topic-quality object (`pgmcp_metadata['topics_quality']`).
/// Returns `None` if never computed or unparseable — callers (orient/digest)
/// treat that as "topic quality unknown".
pub async fn get_topic_quality(pool: &PgPool) -> Option<serde_json::Value> {
    sqlx::query_scalar::<_, String>("SELECT value FROM pgmcp_metadata WHERE key = 'topics_quality'")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// Append a per-topic size snapshot to the bounded `topics_size_history` array
/// in `pgmcp_metadata` (most-recent-last, cap 500) — the longitudinal series
/// `topic_trends` reads to compute per-topic growth/decline. Each snapshot
/// records every `code_topics` row's current `chunk_count` at `now`. No schema
/// change: `pgmcp_metadata` is a `(key, value)` table. Returns the topic count.
pub async fn set_topics_size_snapshot(pool: &PgPool) -> Result<usize, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct SizeRow {
        scope: String,
        id: i32,
        label: String,
        chunk_count: i32,
    }
    let rows = sqlx::query_as::<_, SizeRow>(
        "SELECT scope, id, label, chunk_count FROM code_topics ORDER BY scope, id",
    )
    .fetch_all(pool)
    .await?;
    let now = Utc::now().to_rfc3339();
    let topics: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "scope": r.scope,
                "topic_id": r.id,
                "label": r.label,
                "chunk_count": r.chunk_count,
            })
        })
        .collect();
    let n = topics.len();
    let snapshot = serde_json::json!({ "at": now, "topics": topics });

    let existing: Option<String> =
        sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = 'topics_size_history'")
            .fetch_optional(pool)
            .await?;
    let mut hist = existing
        .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(&s).ok())
        .unwrap_or_default();
    hist.push(snapshot);
    let len = hist.len();
    if len > 500 {
        hist.drain(0..len - 500);
    }
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ('topics_size_history', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(serde_json::Value::Array(hist).to_string())
    .execute(pool)
    .await?;
    Ok(n)
}

/// Read the bounded `topics_size_history` snapshots (oldest-first). Empty when
/// the `topics-size-history` cron has not run yet.
pub async fn get_topics_size_history(pool: &PgPool) -> Vec<serde_json::Value> {
    sqlx::query_scalar::<_, String>(
        "SELECT value FROM pgmcp_metadata WHERE key = 'topics_size_history'",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(&s).ok())
    .unwrap_or_default()
}

/// Read the bounded `topics_quality_history` snapshots (oldest-first; each entry
/// carries `scope`, `computed_at`, and the 8 quality metrics). Empty when no
/// topic scan has run. Used by `topic_trends` (mode=quality).
pub async fn get_topics_quality_history(pool: &PgPool) -> Vec<serde_json::Value> {
    sqlx::query_scalar::<_, String>(
        "SELECT value FROM pgmcp_metadata WHERE key = 'topics_quality_history'",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(&s).ok())
    .unwrap_or_default()
}

/// Whether the global code-topic model is stale relative to the current
/// algorithm signature and the indexed corpus.
///
/// Stale (returns `true`) when: (a) no topics exist; (b) the stored algorithm
/// signature differs from `expected_sig` (topics computed by an older
/// tokenizer/label pipeline — this is what flags the pre-stopword degenerate
/// topics); or (c) some file was indexed after the newest topic was computed.
/// Best-effort: a query error is treated as "not enough signal to claim fresh"
/// only for the freshness leg — the signature leg is authoritative.
pub async fn topics_global_stale(pool: &PgPool, expected_sig: &str) -> bool {
    if topics_algo_signature(pool).await.as_deref() != Some(expected_sig) {
        return true;
    }
    let newest_topic: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(computed_at) FROM code_topics")
            .fetch_one(pool)
            .await
            .unwrap_or(None);
    let Some(newest_topic) = newest_topic else {
        return true; // signature matched but no rows — treat as stale
    };
    let newest_file: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(indexed_at) FROM indexed_files")
            .fetch_one(pool)
            .await
            .unwrap_or(None);
    matches!(newest_file, Some(nf) if nf > newest_topic)
}

/// Whether a project's import/metric graph is stale relative to its files.
/// Stale when no file has a PageRank yet (graph-analysis never ran), or a file
/// was indexed after the newest `file_metrics` row was computed.
pub async fn graph_stale(pool: &PgPool, project_id: i32) -> bool {
    let with_pagerank: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM file_metrics WHERE project_id = $1 AND pagerank IS NOT NULL",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    if with_pagerank == 0 {
        return true;
    }
    let newest_metric: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(computed_at) FROM file_metrics WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(pool)
            .await
            .unwrap_or(None);
    let newest_file: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(indexed_at) FROM indexed_files WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(pool)
            .await
            .unwrap_or(None);
    match (newest_file, newest_metric) {
        (Some(nf), Some(nm)) => nf > nm,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Delete all topics and their assignments for a given scope.
pub async fn clear_topics_for_scope(pool: &PgPool, scope: &str) -> Result<(), sqlx::Error> {
    // Delete assignments first (FK constraint)
    sqlx::query(
        "DELETE FROM chunk_topic_assignments WHERE topic_id IN (
            SELECT id FROM code_topics WHERE scope = $1
        )",
    )
    .bind(scope)
    .execute(pool)
    .await?;

    sqlx::query("DELETE FROM code_topics WHERE scope = $1")
        .bind(scope)
        .execute(pool)
        .await?;

    Ok(())
}

/// A `scope='global'` roll-up meta-topic: an aggregation of similar per-project
/// topics across the workspace. Carries an EXPLICIT aggregated `chunk_count`
/// (summed from its member per-project topics) and `parent_topic_ids` linking to
/// those members. Stored WITHOUT `chunk_topic_assignments` rows — the per-chunk
/// links live on the per-project topics; duplicating them under 'global' would
/// double the multi-million-row assignment table.
#[derive(Debug, Clone)]
pub struct GlobalRollupRow {
    pub cluster_index: i32,
    pub label: String,
    pub keywords: Vec<String>,
    pub keyword_scores: Vec<f32>,
    pub centroid: Vec<f32>,
    pub chunk_count: i32,
    pub file_count: i32,
    pub project_names: Vec<String>,
    pub representative_chunk_id: i64,
    pub representative_snippet: String,
    pub parent_topic_ids: Vec<i64>,
}

/// Store the global roll-up meta-topics under `scope='global'`. Each row carries
/// its explicit aggregated `chunk_count` and `parent_topic_ids`; no
/// `chunk_topic_assignments` are written (see [`GlobalRollupRow`]). The caller
/// is responsible for `clear_topics_for_scope("global")` first.
pub async fn store_global_rollup(
    pool: &PgPool,
    rows: &[GlobalRollupRow],
) -> Result<(), sqlx::Error> {
    for r in rows {
        let centroid_opt: Option<&[f32]> = if r.centroid.is_empty() {
            None
        } else {
            Some(&r.centroid)
        };
        let parent_ids_opt: Option<&[i64]> = if r.parent_topic_ids.is_empty() {
            None
        } else {
            Some(&r.parent_topic_ids)
        };
        sqlx::query(
            "INSERT INTO code_topics
                (scope, cluster_index, label, chunk_count, file_count, project_count,
                 project_names, avg_internal_similarity, representative_chunk_id,
                 representative_snippet, top_files, keywords, keyword_scores,
                 centroid, parent_topic_ids)
             VALUES (
                'global', $1, $2, $3, $4, $5, $6, 0.0,
                (SELECT id FROM file_chunks WHERE id = $7 FOR KEY SHARE),
                $8, '[]'::jsonb, $9, $10, $11, $12
             )
             ON CONFLICT (scope, cluster_index) DO UPDATE SET
                label = EXCLUDED.label,
                chunk_count = EXCLUDED.chunk_count,
                file_count = EXCLUDED.file_count,
                project_count = EXCLUDED.project_count,
                project_names = EXCLUDED.project_names,
                representative_chunk_id = EXCLUDED.representative_chunk_id,
                representative_snippet = EXCLUDED.representative_snippet,
                keywords = EXCLUDED.keywords,
                keyword_scores = EXCLUDED.keyword_scores,
                centroid = COALESCE(EXCLUDED.centroid, code_topics.centroid),
                parent_topic_ids = EXCLUDED.parent_topic_ids,
                computed_at = NOW()",
        )
        .bind(r.cluster_index)
        .bind(&r.label)
        .bind(r.chunk_count)
        .bind(r.file_count)
        .bind(r.project_names.len() as i32)
        .bind(&r.project_names)
        .bind(r.representative_chunk_id)
        .bind(&r.representative_snippet)
        .bind(&r.keywords)
        .bind(&r.keyword_scores)
        .bind(centroid_opt)
        .bind(parent_ids_opt)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Store discovered topics and their chunk assignments in the DB.
pub async fn store_topics(
    pool: &PgPool,
    scope: &str,
    topics: &[crate::cron::topic_clustering::TopicResult],
) -> Result<(), sqlx::Error> {
    // Per-topic transaction: each topic's `code_topics` row + its chunk
    // assignments commit together, or roll back together. A failed topic
    // does NOT abort the whole `store_topics` call — we log and continue
    // so a single transient FK conflict doesn't lose the rest of a
    // 200-topic clustering run.
    //
    // FK-resilience: between when topic clustering starts (12+ minutes
    // ago for a 178k-chunk corpus) and when assignments are inserted
    // here, the daemon's reindex/file watcher may have deleted some
    // `file_chunks` rows. The `chunk_topic_assignments.chunk_id ->
    // file_chunks.id` FK rejects those inserts. We sidestep this with a
    // bulk `INSERT ... SELECT ... WHERE EXISTS` that silently skips
    // orphaned chunk_ids — preserving the topics that *can* still be
    // recorded while dropping the rows that no longer have a valid
    // parent. (Bug 1 in
    // ~/.claude/plans/thoroughly-examine-home-dylon-workspace-melodic-cake.md.)
    let mut errors: Vec<(i32, sqlx::Error)> = Vec::new();
    for topic in topics {
        let top_files_json =
            serde_json::to_value(&topic.top_files).unwrap_or(serde_json::Value::Null);

        // Convert keyword_scores from f64 to f32 for REAL[] column
        let keyword_scores_f32: Vec<f32> = topic.keyword_scores.iter().map(|&s| s as f32).collect();

        // Phase 7: persist centroid as REAL[] for warm-start (if non-empty).
        let centroid_opt: Option<&[f32]> = if topic.centroid.is_empty() {
            None
        } else {
            Some(&topic.centroid)
        };
        // Phase 9: persist parent_topic_ids for hierarchy rows (BIGINT[]).
        let parent_ids_opt: Option<&[i64]> = if topic.parent_topic_ids.is_empty() {
            None
        } else {
            Some(&topic.parent_topic_ids)
        };

        let mut tx = pool.begin().await?;

        // Validate `representative_chunk_id` exists at INSERT time. The
        // `code_topics.representative_chunk_id` FK rejects nonexistent IDs
        // even though the column is nullable with `ON DELETE SET NULL` —
        // the `ON DELETE` only fires when the parent is deleted *after* a
        // valid INSERT. We use a sub-SELECT that returns NULL when the
        // chunk doesn't exist; that NULL satisfies the FK trivially. The
        // `FOR KEY SHARE` prevents the row from being deleted between
        // the validation SELECT and the INSERT's FK trigger fire.
        let topic_id_res = sqlx::query_scalar::<_, i32>(
            "INSERT INTO code_topics
                (scope, cluster_index, label, chunk_count, file_count, project_count,
                 project_names, avg_internal_similarity, representative_chunk_id,
                 representative_snippet, top_files, keywords, keyword_scores,
                 centroid, parent_topic_ids)
             VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8,
                (SELECT id FROM file_chunks WHERE id = $9 FOR KEY SHARE),
                $10, $11, $12, $13, $14, $15
             )
             ON CONFLICT (scope, cluster_index) DO UPDATE SET
                label = EXCLUDED.label,
                chunk_count = EXCLUDED.chunk_count,
                file_count = EXCLUDED.file_count,
                project_count = EXCLUDED.project_count,
                project_names = EXCLUDED.project_names,
                avg_internal_similarity = EXCLUDED.avg_internal_similarity,
                representative_chunk_id = EXCLUDED.representative_chunk_id,
                representative_snippet = EXCLUDED.representative_snippet,
                top_files = EXCLUDED.top_files,
                keywords = EXCLUDED.keywords,
                keyword_scores = EXCLUDED.keyword_scores,
                centroid = COALESCE(EXCLUDED.centroid, code_topics.centroid),
                parent_topic_ids = COALESCE(EXCLUDED.parent_topic_ids, code_topics.parent_topic_ids),
                computed_at = NOW()
             RETURNING id"
        )
        .bind(scope)
        .bind(topic.cluster_index)
        .bind(&topic.label)
        .bind(topic.chunk_ids.len() as i32)
        .bind(topic.file_ids.len() as i32)
        .bind(topic.project_names.len() as i32)
        .bind(&topic.project_names)
        .bind(topic.avg_internal_similarity)
        .bind(topic.representative_chunk_id)
        .bind(&topic.representative_snippet)
        .bind(&top_files_json)
        .bind(&topic.keywords)
        .bind(&keyword_scores_f32)
        .bind(centroid_opt)
        .bind(parent_ids_opt)
        .fetch_one(&mut *tx)
        .await;

        let topic_id = match topic_id_res {
            Ok(id) => id,
            Err(e) => {
                let _ = tx.rollback().await;
                errors.push((topic.cluster_index, e));
                continue;
            }
        };

        // Bulk-insert assignments. Use a CTE that locks the parent
        // `file_chunks` rows with `FOR KEY SHARE` *before* the INSERT
        // runs — this fixes the TOCTOU race the previous `WHERE EXISTS`
        // version had, where a concurrent DELETE between the EXISTS
        // check and the FK trigger fire would still cause the FK to
        // fail. `FOR KEY SHARE` is the weakest lock that blocks DELETE
        // (and key-changing UPDATEs); concurrent reads + non-key UPDATEs
        // still proceed. The lock is released when this transaction
        // commits or rolls back.
        if !topic.chunk_ids.is_empty() {
            // Pad memberships with 1.0 if the FCM result didn't supply one
            // for every chunk (defensive — should never happen).
            let n = topic.chunk_ids.len();
            let mut memberships: Vec<f64> = topic
                .memberships
                .iter()
                .copied()
                .chain(std::iter::repeat(1.0))
                .take(n)
                .collect();
            memberships.truncate(n);

            let assign_res = sqlx::query(
                "WITH locked AS (
                     SELECT fc.id AS chunk_id
                     FROM file_chunks fc
                     WHERE fc.id = ANY($1::bigint[])
                     FOR KEY SHARE
                 )
                 INSERT INTO chunk_topic_assignments (chunk_id, topic_id, membership_score)
                 SELECT v.chunk_id, $2, v.membership
                 FROM unnest($1::bigint[], $3::double precision[]) AS v(chunk_id, membership)
                 JOIN locked l ON l.chunk_id = v.chunk_id
                 ON CONFLICT (chunk_id, topic_id) DO UPDATE SET
                    membership_score = EXCLUDED.membership_score",
            )
            .bind(&topic.chunk_ids)
            .bind(topic_id)
            .bind(&memberships)
            .execute(&mut *tx)
            .await;

            if let Err(e) = assign_res {
                let _ = tx.rollback().await;
                errors.push((topic.cluster_index, e));
                continue;
            }
        }

        if let Err(e) = tx.commit().await {
            errors.push((topic.cluster_index, e));
        }
    }

    if !errors.is_empty() {
        // Log per-topic failures via tracing; return Ok unless ALL topics
        // failed (in which case the most-recent error is propagated).
        for (cluster_index, err) in &errors {
            tracing::error!(
                cluster_index,
                error = %err,
                "store_topics: per-topic transaction failed (continuing)"
            );
        }
        if errors.len() == topics.len() && !topics.is_empty() {
            // All topics failed — surface the last error.
            return Err(errors.into_iter().last().expect("non-empty").1);
        }
    }

    // NOTE: `store_topics` is a pure storage primitive — it does NOT stamp the
    // algorithm signature. Stamping is owned by the cron orchestration layer
    // (`topic_clustering::stamp_topics_signature`), called once per global-refresh
    // strategy after the canonical degeneracy gate (`topic_gate_rejects`) passes
    // and a global store succeeds. Keeping the model-quality policy out of the DB
    // query layer avoids a second, divergent degeneracy definition (the prior
    // inline `with_kw`/`distinct_lead` heuristic disagreed with the canonical gate
    // and silently left the keyword-less online-FCM path permanently "stale").
    Ok(())
}

/// Load cached topics for a given scope from the DB.
pub async fn load_cached_topics(
    pool: &PgPool,
    scope: &str,
    limit: i32,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let rows = sqlx::query_as::<_, CachedTopicRow>(
        "SELECT id, scope, cluster_index, label, chunk_count, file_count,
                project_count, project_names, avg_internal_similarity,
                representative_snippet, top_files, keywords, keyword_scores, computed_at
         FROM code_topics
         WHERE scope = $1
         ORDER BY chunk_count DESC
         LIMIT $2",
    )
    .bind(scope)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let results: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.cluster_index,
                "label": r.label,
                "keywords": r.keywords,
                "keyword_scores": r.keyword_scores,
                "size": r.chunk_count,
                "files": r.file_count,
                "projects": r.project_names,
                "project_count": r.project_count,
                "avg_internal_similarity": r.avg_internal_similarity,
                "representative_snippet": r.representative_snippet,
                "representative_files": r.top_files,
                "computed_at": r.computed_at.map(|t| t.to_rfc3339()),
            })
        })
        .collect();

    Ok(results)
}

#[derive(Debug, sqlx::FromRow)]
struct CachedTopicRow {
    #[allow(dead_code)]
    id: i32,
    #[allow(dead_code)]
    scope: String,
    cluster_index: i32,
    label: String,
    chunk_count: i32,
    file_count: i32,
    project_count: i32,
    project_names: Vec<String>,
    avg_internal_similarity: Option<f64>,
    representative_snippet: Option<String>,
    top_files: Option<serde_json::Value>,
    keywords: Option<Vec<String>>,
    keyword_scores: Option<Vec<f32>>,
    computed_at: Option<DateTime<Utc>>,
}

// ============================================================================
// Analysis tool queries
// ============================================================================

/// Orphan chunk: a chunk not assigned to any topic.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct OrphanChunkRow {
    pub chunk_id: i64,
    pub content: String,
    pub path: String,
    pub language: String,
    pub project_name: String,
    pub chunk_index: i32,
}

/// Find chunks not assigned to any topic (HDBSCAN noise).
pub async fn find_orphan_chunks(
    pool: &PgPool,
    project: Option<&str>,
    language: Option<&str>,
    limit: i32,
) -> Result<Vec<OrphanChunkRow>, sqlx::Error> {
    match (project, language) {
        (Some(proj), Some(lang)) => {
            sqlx::query_as::<_, OrphanChunkRow>(
                "SELECT c.id as chunk_id, c.content, f.path, f.language,
                        p.name as project_name, c.chunk_index
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE NOT EXISTS (
                     SELECT 1 FROM chunk_topic_assignments cta WHERE cta.chunk_id = c.id
                 )
                 AND p.name = $1 AND f.language = $2
                 ORDER BY f.path, c.chunk_index
                 LIMIT $3",
            )
            .bind(proj)
            .bind(lang)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (Some(proj), None) => {
            sqlx::query_as::<_, OrphanChunkRow>(
                "SELECT c.id as chunk_id, c.content, f.path, f.language,
                        p.name as project_name, c.chunk_index
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE NOT EXISTS (
                     SELECT 1 FROM chunk_topic_assignments cta WHERE cta.chunk_id = c.id
                 )
                 AND p.name = $1
                 ORDER BY f.path, c.chunk_index
                 LIMIT $2",
            )
            .bind(proj)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (None, Some(lang)) => {
            sqlx::query_as::<_, OrphanChunkRow>(
                "SELECT c.id as chunk_id, c.content, f.path, f.language,
                        p.name as project_name, c.chunk_index
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE NOT EXISTS (
                     SELECT 1 FROM chunk_topic_assignments cta WHERE cta.chunk_id = c.id
                 )
                 AND f.language = $1
                 ORDER BY f.path, c.chunk_index
                 LIMIT $2",
            )
            .bind(lang)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (None, None) => {
            sqlx::query_as::<_, OrphanChunkRow>(
                "SELECT c.id as chunk_id, c.content, f.path, f.language,
                        p.name as project_name, c.chunk_index
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE NOT EXISTS (
                     SELECT 1 FROM chunk_topic_assignments cta WHERE cta.chunk_id = c.id
                 )
                 ORDER BY f.path, c.chunk_index
                 LIMIT $1",
            )
            .bind(limit)
            .fetch_all(pool)
            .await
        }
    }
}

/// Find chunks not assigned to any topic, scoped by resolved project id.
pub async fn find_orphan_chunks_by_project_id(
    pool: &PgPool,
    project_id: Option<i32>,
    language: Option<&str>,
    limit: i32,
) -> Result<Vec<OrphanChunkRow>, sqlx::Error> {
    match (project_id, language) {
        (Some(pid), Some(lang)) => {
            sqlx::query_as::<_, OrphanChunkRow>(
                "SELECT c.id as chunk_id, c.content, f.path, f.language,
                        p.name as project_name, c.chunk_index
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE NOT EXISTS (
                     SELECT 1 FROM chunk_topic_assignments cta WHERE cta.chunk_id = c.id
                 )
                 AND f.project_id = $1 AND f.language = $2
                 ORDER BY f.path, c.chunk_index
                 LIMIT $3",
            )
            .bind(pid)
            .bind(lang)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (Some(pid), None) => {
            sqlx::query_as::<_, OrphanChunkRow>(
                "SELECT c.id as chunk_id, c.content, f.path, f.language,
                        p.name as project_name, c.chunk_index
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE NOT EXISTS (
                     SELECT 1 FROM chunk_topic_assignments cta WHERE cta.chunk_id = c.id
                 )
                 AND f.project_id = $1
                 ORDER BY f.path, c.chunk_index
                 LIMIT $2",
            )
            .bind(pid)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (None, Some(lang)) => {
            sqlx::query_as::<_, OrphanChunkRow>(
                "SELECT c.id as chunk_id, c.content, f.path, f.language,
                        p.name as project_name, c.chunk_index
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE NOT EXISTS (
                     SELECT 1 FROM chunk_topic_assignments cta WHERE cta.chunk_id = c.id
                 )
                 AND f.language = $1
                 ORDER BY f.path, c.chunk_index
                 LIMIT $2",
            )
            .bind(lang)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (None, None) => {
            sqlx::query_as::<_, OrphanChunkRow>(
                "SELECT c.id as chunk_id, c.content, f.path, f.language,
                        p.name as project_name, c.chunk_index
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE NOT EXISTS (
                     SELECT 1 FROM chunk_topic_assignments cta WHERE cta.chunk_id = c.id
                 )
                 ORDER BY f.path, c.chunk_index
                 LIMIT $1",
            )
            .bind(limit)
            .fetch_all(pool)
            .await
        }
    }
}

/// File-level orphan summary.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct OrphanFileSummary {
    pub path: String,
    pub project_name: String,
    pub language: String,
    pub orphan_chunks: i64,
    pub total_chunks: i64,
    pub orphan_pct: f64,
}

/// Get file-level summary of orphan chunks (files with highest orphan %).
pub async fn find_orphan_file_summary(
    pool: &PgPool,
    project: Option<&str>,
) -> Result<Vec<OrphanFileSummary>, sqlx::Error> {
    if let Some(proj) = project {
        sqlx::query_as::<_, OrphanFileSummary>(
            "SELECT f.path, p.name as project_name, f.language,
                    COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) as orphan_chunks,
                    COUNT(*) as total_chunks,
                    ROUND(100.0 * COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) / COUNT(*), 1)::float8 as orphan_pct
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             LEFT JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
             WHERE p.name = $1
             GROUP BY f.id, f.path, p.name, f.language
             HAVING COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) > 0
             ORDER BY orphan_pct DESC, orphan_chunks DESC"
        )
        .bind(proj)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as::<_, OrphanFileSummary>(
            "SELECT f.path, p.name as project_name, f.language,
                    COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) as orphan_chunks,
                    COUNT(*) as total_chunks,
                    ROUND(100.0 * COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) / COUNT(*), 1)::float8 as orphan_pct
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             LEFT JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
             GROUP BY f.id, f.path, p.name, f.language
             HAVING COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) > 0
             ORDER BY orphan_pct DESC, orphan_chunks DESC"
        )
        .fetch_all(pool)
        .await
    }
}

/// Get file-level summary of orphan chunks, scoped by resolved project id and
/// bounded for MCP response safety.
pub async fn find_orphan_file_summary_by_project_id(
    pool: &PgPool,
    project_id: Option<i32>,
    language: Option<&str>,
    limit: i32,
) -> Result<Vec<OrphanFileSummary>, sqlx::Error> {
    match (project_id, language) {
        (Some(pid), Some(lang)) => {
            sqlx::query_as::<_, OrphanFileSummary>(
                "SELECT f.path, p.name as project_name, f.language,
                        COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) as orphan_chunks,
                        COUNT(*) as total_chunks,
                        ROUND(100.0 * COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) / COUNT(*), 1)::float8 as orphan_pct
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 LEFT JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
                 WHERE f.project_id = $1 AND f.language = $2
                 GROUP BY f.id, f.path, p.name, f.language
                 HAVING COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) > 0
                 ORDER BY orphan_pct DESC, orphan_chunks DESC
                 LIMIT $3",
            )
            .bind(pid)
            .bind(lang)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (Some(pid), None) => {
            sqlx::query_as::<_, OrphanFileSummary>(
                "SELECT f.path, p.name as project_name, f.language,
                        COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) as orphan_chunks,
                        COUNT(*) as total_chunks,
                        ROUND(100.0 * COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) / COUNT(*), 1)::float8 as orphan_pct
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 LEFT JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
                 WHERE f.project_id = $1
                 GROUP BY f.id, f.path, p.name, f.language
                 HAVING COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) > 0
                 ORDER BY orphan_pct DESC, orphan_chunks DESC
                 LIMIT $2",
            )
            .bind(pid)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (None, Some(lang)) => {
            sqlx::query_as::<_, OrphanFileSummary>(
                "SELECT f.path, p.name as project_name, f.language,
                        COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) as orphan_chunks,
                        COUNT(*) as total_chunks,
                        ROUND(100.0 * COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) / COUNT(*), 1)::float8 as orphan_pct
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 LEFT JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
                 WHERE f.language = $1
                 GROUP BY f.id, f.path, p.name, f.language
                 HAVING COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) > 0
                 ORDER BY orphan_pct DESC, orphan_chunks DESC
                 LIMIT $2",
            )
            .bind(lang)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (None, None) => {
            sqlx::query_as::<_, OrphanFileSummary>(
                "SELECT f.path, p.name as project_name, f.language,
                        COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) as orphan_chunks,
                        COUNT(*) as total_chunks,
                        ROUND(100.0 * COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) / COUNT(*), 1)::float8 as orphan_pct
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 LEFT JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
                 GROUP BY f.id, f.path, p.name, f.language
                 HAVING COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) > 0
                 ORDER BY orphan_pct DESC, orphan_chunks DESC
                 LIMIT $1",
            )
            .bind(limit)
            .fetch_all(pool)
            .await
        }
    }
}

/// File-to-topic assignment for misplaced code analysis.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FileTopicRow {
    pub path: String,
    pub project_name: String,
    pub topic_label: String,
    pub topic_id: i32,
    pub chunks_in_topic: i64,
}

/// Load chunk-to-topic assignments aggregated to file level.
pub async fn load_chunk_topic_assignments_for_files(
    pool: &PgPool,
    project: Option<&str>,
) -> Result<Vec<FileTopicRow>, sqlx::Error> {
    // The five-way join + GROUP BY can scan the full chunk_topic_assignments
    // table; raise the per-transaction ceiling so the daemon-wide
    // statement_timeout doesn't fire mid-aggregation on large projects.
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '2min'")
        .execute(&mut *tx)
        .await?;
    // Label this heavy transaction for the graceful-shutdown sweep
    // (db::admin::terminate_heavy_backends).
    sqlx::query("SET LOCAL application_name = 'pgmcp:heavy:topic-clustering'")
        .execute(&mut *tx)
        .await?;
    let results = if let Some(proj) = project {
        sqlx::query_as::<_, FileTopicRow>(
            "SELECT f.path, p.name as project_name, ct.label as topic_label,
                    ct.id as topic_id, COUNT(*) as chunks_in_topic
             FROM chunk_topic_assignments cta
             JOIN file_chunks c ON c.id = cta.chunk_id
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             JOIN code_topics ct ON ct.id = cta.topic_id
             WHERE p.name = $1
             GROUP BY f.path, p.name, ct.label, ct.id
             ORDER BY f.path, chunks_in_topic DESC",
        )
        .bind(proj)
        .fetch_all(&mut *tx)
        .await?
    } else {
        sqlx::query_as::<_, FileTopicRow>(
            "SELECT f.path, p.name as project_name, ct.label as topic_label,
                    ct.id as topic_id, COUNT(*) as chunks_in_topic
             FROM chunk_topic_assignments cta
             JOIN file_chunks c ON c.id = cta.chunk_id
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             JOIN code_topics ct ON ct.id = cta.topic_id
             GROUP BY f.path, p.name, ct.label, ct.id
             ORDER BY f.path, chunks_in_topic DESC",
        )
        .fetch_all(&mut *tx)
        .await?
    };
    tx.commit().await?;
    Ok(results)
}

/// Load chunk-to-topic assignments aggregated to file level for one resolved
/// project id. This is the production path for name-scoped MCP tools because
/// project display names are not globally unique.
pub async fn load_chunk_topic_assignments_for_files_by_project_id(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<FileTopicRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '2min'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL application_name = 'pgmcp:heavy:topic-clustering'")
        .execute(&mut *tx)
        .await?;
    let results = sqlx::query_as::<_, FileTopicRow>(
        "SELECT f.path, p.name as project_name, ct.label as topic_label,
                ct.id as topic_id, COUNT(*) as chunks_in_topic
         FROM chunk_topic_assignments cta
         JOIN file_chunks c ON c.id = cta.chunk_id
         JOIN indexed_files f ON f.id = c.file_id
         JOIN projects p ON p.id = f.project_id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE f.project_id = $1
         GROUP BY f.path, p.name, ct.label, ct.id
         ORDER BY f.path, chunks_in_topic DESC",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(results)
}

/// Co-change coupled file pair from git history.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct CoupledFilePair {
    pub file_a: String,
    pub file_b: String,
    pub co_commits: i64,
    pub commits_a: i64,
    pub commits_b: i64,
    pub jaccard: f64,
}

/// Maximum number of files a single commit may touch before it is excluded from
/// co-change coupling. A commit that touches hundreds of files (a vendored-dep
/// import, a tree-wide `cargo fmt`, a license-header sweep, or pgmcp's own
/// 282-file "add all tools" commit) makes every pair of those files look
/// co-changed, manufacturing phantom Jaccard coupling and "shotgun surgery"
/// findings that reflect git mechanics, not logical coupling. 50 comfortably
/// admits normal feature commits while dropping the bulk-commit tail. Excluded
/// commits are removed from BOTH the pair counts and the per-file totals so the
/// Jaccard denominator stays consistent.
const COCHANGE_MAX_FILES_PER_COMMIT: i64 = 50;

/// Find files that frequently change together in git commits (Jaccard co-change coupling).
///
/// Commits touching more than [`COCHANGE_MAX_FILES_PER_COMMIT`] files are
/// ignored — see that constant for the rationale (bulk commits are not evidence
/// of logical coupling).
pub async fn find_coupled_files(
    pool: &PgPool,
    project: &str,
    min_coupling: f64,
    min_commits: i32,
) -> Result<Vec<CoupledFilePair>, sqlx::Error> {
    sqlx::query_as::<_, CoupledFilePair>(
        "WITH commit_sizes AS (
            SELECT gcf.commit_id, COUNT(*) AS files_in_commit
            FROM git_commit_files gcf
            JOIN git_commits gc ON gc.id = gcf.commit_id
            JOIN projects p ON p.id = gc.project_id
            WHERE p.name = $1
            GROUP BY gcf.commit_id
        ),
        file_commits AS (
            SELECT gcf.file_path, gcf.commit_id
            FROM git_commit_files gcf
            JOIN git_commits gc ON gc.id = gcf.commit_id
            JOIN projects p ON p.id = gc.project_id
            JOIN commit_sizes cs ON cs.commit_id = gcf.commit_id
            WHERE p.name = $1
              AND cs.files_in_commit <= $4
        ),
        pair_counts AS (
            SELECT a.file_path AS file_a, b.file_path AS file_b,
                   COUNT(*) AS co_commits
            FROM file_commits a
            JOIN file_commits b ON a.commit_id = b.commit_id AND a.file_path < b.file_path
            GROUP BY a.file_path, b.file_path
        ),
        file_totals AS (
            SELECT file_path, COUNT(DISTINCT commit_id) AS total_commits
            FROM file_commits
            GROUP BY file_path
        )
        SELECT pc.file_a, pc.file_b, pc.co_commits,
               ta.total_commits AS commits_a, tb.total_commits AS commits_b,
               pc.co_commits::float8 / (ta.total_commits + tb.total_commits - pc.co_commits) AS jaccard
        FROM pair_counts pc
        JOIN file_totals ta ON ta.file_path = pc.file_a
        JOIN file_totals tb ON tb.file_path = pc.file_b
        WHERE pc.co_commits::float8 / (ta.total_commits + tb.total_commits - pc.co_commits) >= $2
          AND pc.co_commits >= $3
        ORDER BY jaccard DESC"
    )
    .bind(project)
    .bind(min_coupling)
    .bind(min_commits)
    .bind(COCHANGE_MAX_FILES_PER_COMMIT)
    .fetch_all(pool)
    .await
}

/// Project-id-scoped variant used by MCP tools after duplicate project-name
/// resolution. This is the production-safe path: duplicate display names cannot
/// merge unrelated git histories.
pub async fn find_coupled_files_by_project_id(
    pool: &PgPool,
    project_id: i32,
    min_coupling: f64,
    min_commits: i32,
) -> Result<Vec<CoupledFilePair>, sqlx::Error> {
    sqlx::query_as::<_, CoupledFilePair>(
        "WITH commit_sizes AS (
            SELECT gcf.commit_id, COUNT(*) AS files_in_commit
            FROM git_commit_files gcf
            JOIN git_commits gc ON gc.id = gcf.commit_id
            WHERE gc.project_id = $1
            GROUP BY gcf.commit_id
        ),
        file_commits AS (
            SELECT gcf.file_path, gcf.commit_id
            FROM git_commit_files gcf
            JOIN git_commits gc ON gc.id = gcf.commit_id
            JOIN commit_sizes cs ON cs.commit_id = gcf.commit_id
            WHERE gc.project_id = $1
              AND cs.files_in_commit <= $4
        ),
        pair_counts AS (
            SELECT a.file_path AS file_a, b.file_path AS file_b,
                   COUNT(*) AS co_commits
            FROM file_commits a
            JOIN file_commits b ON a.commit_id = b.commit_id AND a.file_path < b.file_path
            GROUP BY a.file_path, b.file_path
        ),
        file_totals AS (
            SELECT file_path, COUNT(DISTINCT commit_id) AS total_commits
            FROM file_commits
            GROUP BY file_path
        )
        SELECT pc.file_a, pc.file_b, pc.co_commits,
               ta.total_commits AS commits_a, tb.total_commits AS commits_b,
               pc.co_commits::float8 / (ta.total_commits + tb.total_commits - pc.co_commits) AS jaccard
        FROM pair_counts pc
        JOIN file_totals ta ON ta.file_path = pc.file_a
        JOIN file_totals tb ON tb.file_path = pc.file_b
        WHERE pc.co_commits::float8 / (ta.total_commits + tb.total_commits - pc.co_commits) >= $2
          AND pc.co_commits >= $3
        ORDER BY jaccard DESC"
    )
    .bind(project_id)
    .bind(min_coupling)
    .bind(min_commits)
    .bind(COCHANGE_MAX_FILES_PER_COMMIT)
    .fetch_all(pool)
    .await
}

/// Topic coverage row for test gap analysis.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct TopicCoverageRow {
    pub topic_id: i32,
    pub label: String,
    pub test_chunks: i64,
    pub impl_chunks: i64,
}

/// Get per-topic test vs implementation chunk counts for a project.
pub async fn get_test_topic_coverage(
    pool: &PgPool,
    project: &str,
) -> Result<Vec<TopicCoverageRow>, sqlx::Error> {
    sqlx::query_as::<_, TopicCoverageRow>(
        "SELECT ct.id as topic_id, ct.label,
                COUNT(*) FILTER (WHERE f.path ~ '(^|/)(tests?|specs?)(/|$)|_test\\.|\\btest_|\\.test\\.|_spec\\.|\\bspec_|\\.spec\\.') as test_chunks,
                COUNT(*) FILTER (WHERE f.path !~ '(^|/)(tests?|specs?)(/|$)|_test\\.|\\btest_|\\.test\\.|_spec\\.|\\bspec_|\\.spec\\.') as impl_chunks
         FROM chunk_topic_assignments cta
         JOIN file_chunks c ON c.id = cta.chunk_id
         JOIN indexed_files f ON f.id = c.file_id
         JOIN projects p ON p.id = f.project_id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE p.name = $1
         GROUP BY ct.id, ct.label
         ORDER BY impl_chunks DESC"
    )
    .bind(project)
    .fetch_all(pool)
    .await
}

/// Get per-topic test vs implementation chunk counts for a resolved project id.
pub async fn get_test_topic_coverage_by_project_id(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<TopicCoverageRow>, sqlx::Error> {
    sqlx::query_as::<_, TopicCoverageRow>(
        "SELECT ct.id as topic_id, ct.label,
                COUNT(*) FILTER (WHERE f.path ~ '(^|/)(tests?|specs?)(/|$)|_test\\.|\\btest_|\\.test\\.|_spec\\.|\\bspec_|\\.spec\\.') as test_chunks,
                COUNT(*) FILTER (WHERE f.path !~ '(^|/)(tests?|specs?)(/|$)|_test\\.|\\btest_|\\.test\\.|_spec\\.|\\bspec_|\\.spec\\.') as impl_chunks
         FROM chunk_topic_assignments cta
         JOIN file_chunks c ON c.id = cta.chunk_id
         JOIN indexed_files f ON f.id = c.file_id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE f.project_id = $1
         GROUP BY ct.id, ct.label
         ORDER BY impl_chunks DESC"
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// Topic centroid row for hierarchy analysis.
#[derive(Debug, Clone)]
pub struct TopicCentroidRow {
    pub topic_id: i32,
    pub label: String,
    pub chunk_count: i64,
    pub centroid: Vec<f32>,
}

/// Load topic centroids by averaging chunk embeddings per topic.
/// Since pgvector may not support AVG on vector, we compute centroids in Rust.
pub async fn load_topic_centroids(
    pool: &PgPool,
    scope: &str,
) -> Result<Vec<TopicCentroidRow>, sqlx::Error> {
    // First get the topic metadata
    let topics = sqlx::query_as::<_, TopicMetaRow>(
        "SELECT id as topic_id, label, chunk_count
         FROM code_topics
         WHERE scope = $1
         ORDER BY chunk_count DESC",
    )
    .bind(scope)
    .fetch_all(pool)
    .await?;

    let mut results = Vec::with_capacity(topics.len());

    // Post-cutover the legacy `embedding` column is gone; read the active
    // signature's column (embedding_v2 under BGE-M3).
    let col = crate::embed::signature::read_active_signature(pool)
        .await?
        .read_column();

    for topic in &topics {
        // Get all chunk embeddings for this topic
        let embeddings: Vec<Vec<f32>> =
            sqlx::query_scalar::<_, Vec<f32>>(sqlx::AssertSqlSafe(format!(
                "SELECT c.{col}::real[] as embedding
             FROM chunk_topic_assignments cta
             JOIN file_chunks c ON c.id = cta.chunk_id
             WHERE cta.topic_id = $1",
            )))
            .bind(topic.topic_id)
            .fetch_all(pool)
            .await?;

        if embeddings.is_empty() {
            continue;
        }

        // Compute centroid as mean of all embeddings
        let dim = embeddings[0].len();
        let mut centroid = vec![0.0f32; dim];
        for emb in &embeddings {
            for (i, val) in emb.iter().enumerate() {
                if i < dim {
                    centroid[i] += val;
                }
            }
        }
        let n = embeddings.len() as f32;
        for val in &mut centroid {
            *val /= n;
        }

        // L2-normalize
        let norm: f32 = centroid.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for val in &mut centroid {
                *val /= norm;
            }
        }

        results.push(TopicCentroidRow {
            topic_id: topic.topic_id,
            label: topic.label.clone(),
            chunk_count: topic.chunk_count as i64,
            centroid,
        });
    }

    Ok(results)
}

#[derive(Debug, sqlx::FromRow)]
struct TopicMetaRow {
    topic_id: i32,
    label: String,
    chunk_count: i32,
}

/// Check whether any chunk_topic_assignments exist (to detect if topics have been computed).
pub async fn has_topic_assignments(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM chunk_topic_assignments LIMIT 1")
            .fetch_one(pool)
            .await?;
    Ok(count > 0)
}

// ============================================================================
// Document analysis queries (suggest_merges, suggest_splits, doc_coverage_gaps)
// ============================================================================

/// Per-file topic distribution row for merge analysis.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FileTopicDistributionRow {
    pub file_id: i64,
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub line_count: i32,
    pub size_bytes: i64,
    pub topic_id: i32,
    pub topic_label: String,
    pub keywords: Option<Vec<String>>,
    pub total_membership: f64,
    pub chunks_in_topic: i64,
}

/// Get per-file topic distributions for merge analysis.
/// Returns one row per (file, topic) pair with aggregated membership scores.
pub async fn get_file_topic_distributions(
    pool: &PgPool,
    project: &str,
    language: Option<&str>,
) -> Result<Vec<FileTopicDistributionRow>, sqlx::Error> {
    sqlx::query_as::<_, FileTopicDistributionRow>(
        "SELECT f.id as file_id, f.path, f.relative_path, f.language,
                f.line_count, f.size_bytes,
                cta.topic_id, ct.label as topic_label, ct.keywords,
                SUM(cta.membership_score) as total_membership,
                COUNT(*) as chunks_in_topic
         FROM indexed_files f
         JOIN projects p ON p.id = f.project_id
         JOIN file_chunks c ON c.file_id = f.id
         JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE p.name = $1
           AND ($2::text IS NULL OR f.language = $2)
         GROUP BY f.id, f.path, f.relative_path, f.language,
                  f.line_count, f.size_bytes,
                  cta.topic_id, ct.label, ct.keywords
         ORDER BY f.path, total_membership DESC",
    )
    .bind(project)
    .bind(language)
    .fetch_all(pool)
    .await
}

/// Chunk-level topic detail row for split analysis.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ChunkTopicDetailRow {
    pub file_id: i64,
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub line_count: i32,
    pub size_bytes: i64,
    pub chunk_id: i64,
    pub chunk_index: i32,
    pub start_line: i32,
    pub end_line: i32,
    pub chunk_content: String,
    pub topic_id: i32,
    pub topic_label: String,
    pub membership_score: f64,
}

/// Get chunk-level topic assignments with position info for split analysis.
pub async fn get_chunk_topic_details(
    pool: &PgPool,
    project: &str,
    language: Option<&str>,
) -> Result<Vec<ChunkTopicDetailRow>, sqlx::Error> {
    sqlx::query_as::<_, ChunkTopicDetailRow>(
        "SELECT f.id as file_id, f.path, f.relative_path, f.language,
                f.line_count, f.size_bytes,
                c.id as chunk_id, c.chunk_index, c.start_line, c.end_line,
                c.content as chunk_content,
                cta.topic_id, ct.label as topic_label,
                cta.membership_score
         FROM indexed_files f
         JOIN projects p ON p.id = f.project_id
         JOIN file_chunks c ON c.file_id = f.id
         JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE p.name = $1
           AND ($2::text IS NULL OR f.language = $2)
         ORDER BY f.path, c.chunk_index, cta.membership_score DESC",
    )
    .bind(project)
    .bind(language)
    .fetch_all(pool)
    .await
}

/// Documentation coverage row for doc_coverage_gaps analysis.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct DocCoverageRow {
    pub topic_id: i32,
    pub label: String,
    pub keywords: Option<Vec<String>>,
    pub doc_chunks: i64,
    pub code_chunks: i64,
}

/// Get per-topic documentation vs code chunk counts for a project.
pub async fn get_doc_topic_coverage(
    pool: &PgPool,
    project: &str,
) -> Result<Vec<DocCoverageRow>, sqlx::Error> {
    sqlx::query_as::<_, DocCoverageRow>(
        "SELECT ct.id as topic_id, ct.label, ct.keywords,
                COUNT(*) FILTER (WHERE f.language = 'markdown') as doc_chunks,
                COUNT(*) FILTER (WHERE f.language != 'markdown') as code_chunks
         FROM chunk_topic_assignments cta
         JOIN file_chunks c ON c.id = cta.chunk_id
         JOIN indexed_files f ON f.id = c.file_id
         JOIN projects p ON p.id = f.project_id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE p.name = $1
         GROUP BY ct.id, ct.label, ct.keywords
         ORDER BY code_chunks DESC",
    )
    .bind(project)
    .fetch_all(pool)
    .await
}

/// Get per-topic documentation vs code chunk counts for one resolved project.
pub async fn get_doc_topic_coverage_by_project_id(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<DocCoverageRow>, sqlx::Error> {
    sqlx::query_as::<_, DocCoverageRow>(
        "SELECT ct.id as topic_id, ct.label, ct.keywords,
                COUNT(*) FILTER (WHERE f.language = 'markdown') as doc_chunks,
                COUNT(*) FILTER (WHERE f.language != 'markdown') as code_chunks
         FROM chunk_topic_assignments cta
         JOIN file_chunks c ON c.id = cta.chunk_id
         JOIN indexed_files f ON f.id = c.file_id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE f.project_id = $1
         GROUP BY ct.id, ct.label, ct.keywords
         ORDER BY code_chunks DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}
