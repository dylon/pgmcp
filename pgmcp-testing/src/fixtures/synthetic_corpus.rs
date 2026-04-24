//! Synthetic, hand-designed code corpus used by the topic-clustering
//! correctness oracles (Phase G).
//!
//! ## Layout
//!
//! Three projects, three planted topics, ~30 chunks total. Each chunk's
//! embedding is a small perturbation of one of three orthonormal basis
//! vectors (e0, e1, e2) — guaranteeing that any reasonable clusterer
//! will find three clearly-separated communities.
//!
//! Chunks are partitioned thus (the layout is referenced from every
//! Phase G oracle test that asserts an expected output):
//!
//!   indices 0–9   → topic "auth"      (project `proj-auth`)
//!   indices 10–19 → topic "database"  (project `proj-database`)
//!   indices 20–29 → topic "logging"   (project `proj-logging`)
//!
//! Plus three deliberate edge-case chunks:
//!
//!   chunk 30 → "auth" content but embedding ≈ e2 (orphan-like;
//!              membership in any cluster is low)
//!   chunk 31 → "database" content (planted as a misplaced file in
//!              `proj-database/auth/` directory — its semantic topic
//!              clashes with its directory context)
//!   chunk 32 → mixed-vocabulary content (split candidate)
//!
//! ## Helpers
//!
//! - [`SyntheticCorpus::seed_chunks_only`] — projects + files +
//!   chunks (with embeddings). Used by [`oracle_discover_topics`]
//!   so the FCM pipeline produces the topic assignments itself.
//! - [`SyntheticCorpus::seed_with_assignments`] — additionally
//!   inserts hand-pinned `code_topics` and
//!   `chunk_topic_assignments` rows so downstream oracle tests
//!   read deterministic data.
//!
//! Both helpers are idempotent in the sense that they assume a fresh
//! per-test database from `db_harness::TestDatabase` — they do not
//! upsert against existing rows.

use sqlx::PgPool;

/// Vector dimension matches the production fastembed model.
pub const DIM: usize = 384;

/// Topic IDs (database PK) assigned by [`SyntheticCorpus::seed_with_assignments`].
/// Captured for tests that need to reference a specific topic by id.
pub struct PlantedTopicIds {
    pub auth: i32,
    pub database: i32,
    pub logging: i32,
}

/// Returned by both seed helpers; lets oracle tests look up file_id /
/// chunk_id without re-querying.
pub struct SeededHandles {
    pub auth_project_id: i32,
    pub database_project_id: i32,
    pub logging_project_id: i32,
    pub auth_chunk_ids: Vec<i64>,
    pub database_chunk_ids: Vec<i64>,
    pub logging_chunk_ids: Vec<i64>,
    pub orphan_chunk_id: i64,
    pub misplaced_chunk_id: i64,
    pub split_candidate_chunk_ids: Vec<i64>,
    pub merge_candidate_file_ids: (i64, i64),
    pub split_candidate_file_id: i64,
    pub planted_topics: Option<PlantedTopicIds>,
}

/// L2-normalized basis vector with a single 1.0.
pub fn basis(idx: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[idx] = 1.0;
    v
}

/// Basis vector + small reproducible noise on adjacent dims, then L2-normalised.
/// The noise gives FCM something nontrivial to cluster while staying clearly
/// closer to its own basis than to any other.
pub fn basis_with_noise(idx: usize, jitter_index: usize) -> Vec<f32> {
    let mut v = basis(idx);
    let neighbour = (idx + 1) % 3;
    // Small reproducible noise: ≈ 0.03 in the neighbour direction,
    // ≈ 0.01 in the next-next direction. Keeps the cosine to its own
    // basis ≈ 0.999 and the cosine to the other bases ≈ 0.03 / 0.01.
    v[neighbour] = 0.03 * (1.0 + (jitter_index as f32) * 0.01);
    v[(idx + 2) % 3] = 0.01;
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in &mut v {
        *x /= n;
    }
    v
}

/// Token-rich content for one of the three planted topics. The
/// vocabulary overlap is what `compute_ctf_idf` would key on; we use
/// disjoint vocabularies so any keyword-driven test has a clean signal.
pub fn topic_content(topic: &str, idx: usize) -> String {
    match topic {
        "auth" => format!(
            "validate password and refresh access token {idx} for the user account; \
             reject expired credentials and re-issue session token {idx}"
        ),
        "database" => format!(
            "execute prepared sql query {idx} on postgres connection pool; \
             commit transaction {idx} after row insertion completes"
        ),
        "logging" => format!(
            "emit structured log message {idx} at info level with span context; \
             record latency metric {idx} for trace correlation"
        ),
        _ => format!("misc content {idx}"),
    }
}

pub struct SyntheticCorpus;

impl SyntheticCorpus {
    /// Insert projects, files and file_chunks (with embeddings) only.
    /// Use this when the test wants the production FCM pipeline to
    /// generate code_topics + chunk_topic_assignments itself.
    pub async fn seed_chunks_only(pool: &PgPool) -> SeededHandles {
        let auth_pid = insert_project(pool, "proj-auth", "/ws/auth").await;
        let database_pid = insert_project(pool, "proj-database", "/ws/database").await;
        let logging_pid = insert_project(pool, "proj-logging", "/ws/logging").await;

        // 10 chunks per topic, distributed across 2 files per project.
        let auth_chunk_ids = seed_topic_chunks(pool, auth_pid, "/ws/auth", "auth", 0).await;
        let database_chunk_ids =
            seed_topic_chunks(pool, database_pid, "/ws/database", "database", 1).await;
        let logging_chunk_ids =
            seed_topic_chunks(pool, logging_pid, "/ws/logging", "logging", 2).await;

        // Edge cases.
        let orphan_chunk_id = seed_orphan_chunk(pool, auth_pid).await;
        let misplaced_chunk_id = seed_misplaced_chunk(pool, database_pid).await;
        let (split_file_id, split_chunk_ids) = seed_split_candidate_file(pool, auth_pid).await;
        let merge_files = seed_merge_candidates(pool, database_pid).await;

        SeededHandles {
            auth_project_id: auth_pid,
            database_project_id: database_pid,
            logging_project_id: logging_pid,
            auth_chunk_ids,
            database_chunk_ids,
            logging_chunk_ids,
            orphan_chunk_id,
            misplaced_chunk_id,
            split_candidate_chunk_ids: split_chunk_ids,
            merge_candidate_file_ids: merge_files,
            split_candidate_file_id: split_file_id,
            planted_topics: None,
        }
    }

    /// Insert chunks AND a hand-pinned topic assignment table mirroring
    /// the planted partition: every auth-content chunk is in topic
    /// "auth", every database-content chunk is in topic "database", etc.
    /// The orphan chunk is intentionally NOT assigned to any topic.
    /// The misplaced chunk is assigned to "auth" so its semantic topic
    /// clashes with its `database/auth/` directory context.
    /// The split-candidate chunks are split across all three topics.
    pub async fn seed_with_assignments(pool: &PgPool) -> SeededHandles {
        let mut h = Self::seed_chunks_only(pool).await;

        let scope = "global"; // matches the default scope used by load_topic_centroids
        let auth_topic = insert_topic(
            pool,
            scope,
            0,
            "auth",
            h.auth_chunk_ids.len() as i32,
            &["password", "token", "credential"],
        )
        .await;
        let database_topic = insert_topic(
            pool,
            scope,
            1,
            "database",
            h.database_chunk_ids.len() as i32,
            &["query", "transaction", "postgres"],
        )
        .await;
        let logging_topic = insert_topic(
            pool,
            scope,
            2,
            "logging",
            h.logging_chunk_ids.len() as i32,
            &["log", "metric", "trace"],
        )
        .await;

        // Bulk assign every "core" chunk to its planted topic (membership 1.0).
        for &cid in &h.auth_chunk_ids {
            assign_chunk(pool, cid, auth_topic, 1.0).await;
        }
        for &cid in &h.database_chunk_ids {
            assign_chunk(pool, cid, database_topic, 1.0).await;
        }
        for &cid in &h.logging_chunk_ids {
            assign_chunk(pool, cid, logging_topic, 1.0).await;
        }

        // Misplaced chunk: lives in the "database" project's `auth/` dir
        // but is auth-flavoured semantically. So its topic is auth.
        assign_chunk(pool, h.misplaced_chunk_id, auth_topic, 1.0).await;

        // Split-candidate chunks: distribute one chunk to each topic so
        // the file's distribution has Shannon entropy log2(3) ≈ 1.585.
        if h.split_candidate_chunk_ids.len() >= 3 {
            assign_chunk(pool, h.split_candidate_chunk_ids[0], auth_topic, 1.0).await;
            assign_chunk(pool, h.split_candidate_chunk_ids[1], database_topic, 1.0).await;
            assign_chunk(pool, h.split_candidate_chunk_ids[2], logging_topic, 1.0).await;
        }

        // Merge candidate: both files in the pair share the database
        // topic to a high degree (already inherited from the database
        // chunks they own). Nothing further to insert.

        // Orphan chunk: deliberately not assigned to any topic.

        h.planted_topics = Some(PlantedTopicIds {
            auth: auth_topic,
            database: database_topic,
            logging: logging_topic,
        });
        h
    }
}

// ============================================================================
// Internal seeders
// ============================================================================

async fn insert_project(pool: &PgPool, name: &str, workspace: &str) -> i32 {
    sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(workspace)
    .bind(format!("{workspace}/{name}"))
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("project")
}

async fn insert_file(
    pool: &PgPool,
    project_id: i32,
    abs_path: &str,
    relative_path: &str,
    language: &str,
    line_count: i32,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO indexed_files \
         (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW()) RETURNING id",
    )
    .bind(project_id)
    .bind(abs_path)
    .bind(relative_path)
    .bind(language)
    .bind(64_i64)
    .bind("synthetic")
    .bind(line_count)
    .fetch_one(pool)
    .await
    .expect("file")
}

async fn insert_chunk_with_embedding(
    pool: &PgPool,
    file_id: i64,
    chunk_idx: i32,
    content: &str,
    embedding: &[f32],
    start_line: i32,
    end_line: i32,
) -> i64 {
    let v = pgvector::Vector::from(embedding.to_vec());
    sqlx::query_scalar(
        "INSERT INTO file_chunks \
         (file_id, chunk_index, content, start_line, end_line, embedding) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(file_id)
    .bind(chunk_idx)
    .bind(content)
    .bind(start_line)
    .bind(end_line)
    .bind(v)
    .fetch_one(pool)
    .await
    .expect("chunk")
}

/// Seed two files for one topic (5 chunks per file). `basis_idx`
/// chooses which orthonormal basis vector this topic clusters around.
async fn seed_topic_chunks(
    pool: &PgPool,
    project_id: i32,
    workspace: &str,
    topic_name: &str,
    basis_idx: usize,
) -> Vec<i64> {
    let mut chunk_ids = Vec::with_capacity(10);
    for file_n in 0..2 {
        let rel = format!("{topic_name}/file_{file_n}.rs");
        let abs = format!("{workspace}/{topic_name}/file_{file_n}.rs");
        let file_id = insert_file(pool, project_id, &abs, &rel, "rust", 50).await;
        for chunk_n in 0..5 {
            let global_idx = file_n * 5 + chunk_n;
            let cid = insert_chunk_with_embedding(
                pool,
                file_id,
                chunk_n as i32,
                &topic_content(topic_name, global_idx as usize),
                &basis_with_noise(basis_idx, global_idx as usize),
                (chunk_n as i32) * 10 + 1,
                (chunk_n as i32) * 10 + 9,
            )
            .await;
            chunk_ids.push(cid);
        }
    }
    chunk_ids
}

/// Single chunk in its own file in the auth project, but with an
/// embedding pointing at the LOGGING basis (e2). Far from every
/// topic centroid → low membership in all clusters → orphan.
async fn seed_orphan_chunk(pool: &PgPool, project_id: i32) -> i64 {
    let file_id = insert_file(
        pool,
        project_id,
        "/ws/auth/auth/orphan.rs",
        "auth/orphan.rs",
        "rust",
        5,
    )
    .await;
    // Embedding is a balanced superposition of all 3 bases — equidistant
    // from each cluster, so its max membership is well below threshold.
    let mut v = vec![0.0_f32; DIM];
    v[0] = 1.0;
    v[1] = 1.0;
    v[2] = 1.0;
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in &mut v {
        *x /= n;
    }
    insert_chunk_with_embedding(pool, file_id, 0, "neither auth nor db nor log", &v, 1, 5).await
}

/// Single chunk in `proj-database/auth/misplaced.rs`. Directory
/// context says "database" but content + embedding say "auth".
async fn seed_misplaced_chunk(pool: &PgPool, project_id: i32) -> i64 {
    let file_id = insert_file(
        pool,
        project_id,
        "/ws/database/auth/misplaced.rs",
        "auth/misplaced.rs",
        "rust",
        10,
    )
    .await;
    insert_chunk_with_embedding(
        pool,
        file_id,
        0,
        &topic_content("auth", 99),
        &basis_with_noise(0, 99),
        1,
        9,
    )
    .await
}

/// One file with 3 chunks, each pointing at a different topic basis.
/// File entropy = log2(3) ≈ 1.585, well above the 1.5 default threshold.
async fn seed_split_candidate_file(pool: &PgPool, project_id: i32) -> (i64, Vec<i64>) {
    let file_id = insert_file(
        pool,
        project_id,
        "/ws/auth/mixed.rs",
        "mixed.rs",
        "rust",
        30,
    )
    .await;
    let mut ids = Vec::with_capacity(3);
    for (i, basis_idx) in [0, 1, 2].iter().enumerate() {
        let cid = insert_chunk_with_embedding(
            pool,
            file_id,
            i as i32,
            &topic_content(["auth", "database", "logging"][i], 200 + i),
            &basis_with_noise(*basis_idx, 200 + i),
            (i as i32) * 10 + 1,
            (i as i32) * 10 + 9,
        )
        .await;
        ids.push(cid);
    }
    (file_id, ids)
}

/// Two files in the same project (database) whose chunks share the
/// "database" basis identically. These will appear as merge
/// candidates by suggest_merges (high weighted Jaccard on topics).
async fn seed_merge_candidates(pool: &PgPool, project_id: i32) -> (i64, i64) {
    let file_a = insert_file(
        pool,
        project_id,
        "/ws/database/database/twin_a.rs",
        "database/twin_a.rs",
        "rust",
        15,
    )
    .await;
    let file_b = insert_file(
        pool,
        project_id,
        "/ws/database/database/twin_b.rs",
        "database/twin_b.rs",
        "rust",
        15,
    )
    .await;
    for (file_id, jit) in [(file_a, 300), (file_b, 400)] {
        for chunk_n in 0..3 {
            insert_chunk_with_embedding(
                pool,
                file_id,
                chunk_n,
                &topic_content("database", jit + chunk_n as usize),
                &basis_with_noise(1, jit + chunk_n as usize),
                (chunk_n as i32) * 5 + 1,
                (chunk_n as i32) * 5 + 4,
            )
            .await;
        }
    }
    (file_a, file_b)
}

async fn insert_topic(
    pool: &PgPool,
    scope: &str,
    cluster_index: i32,
    label: &str,
    chunk_count: i32,
    keywords: &[&str],
) -> i32 {
    let kw_owned: Vec<String> = keywords.iter().map(|s| s.to_string()).collect();
    sqlx::query_scalar(
        "INSERT INTO code_topics \
         (scope, cluster_index, label, chunk_count, file_count, project_count, project_names, keywords) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(scope)
    .bind(cluster_index)
    .bind(label)
    .bind(chunk_count)
    .bind(2_i32)
    .bind(1_i32)
    .bind::<Vec<String>>(vec!["proj-synthetic".into()])
    .bind(kw_owned)
    .fetch_one(pool)
    .await
    .expect("topic")
}

async fn assign_chunk(pool: &PgPool, chunk_id: i64, topic_id: i32, score: f64) {
    sqlx::query(
        "INSERT INTO chunk_topic_assignments (chunk_id, topic_id, membership_score) \
         VALUES ($1, $2, $3)",
    )
    .bind(chunk_id)
    .bind(topic_id)
    .bind(score)
    .execute(pool)
    .await
    .expect("assignment");
}

// ============================================================================
// Graph corpus (Phase H — graph & architecture tools)
// ============================================================================

/// Handles returned by `seed_graph_corpus`. The graph is intentionally
/// small enough (5 files, planted edges, planted metrics) that every
/// downstream Phase H tool's expected output is computable on paper.
pub struct GraphHandles {
    pub project_id: i32,
    /// Files keyed by short label. Each label maps to (file_id, relative_path).
    pub files: std::collections::HashMap<&'static str, (i64, String)>,
}

/// Seed a small canonical CodeGraph in PostgreSQL. The structure is:
///
/// ```text
///   core/a.rs ──► core/b.rs ──► core/c.rs
///         ▲                          │
///         │                          ▼
///   util/util.rs ◄────────── api/api.rs
///         │                          ▲
///         └─── (3-cycle: a → b → c → a omitted; we add a 2-cycle
///              api ↔ util to give find_cycles something to find)
/// ```
///
/// Edges (all `import` type, weight 1.0):
///   - core/a.rs    → core/b.rs
///   - core/b.rs    → core/c.rs
///   - core/c.rs    → core/a.rs   (closes a 3-cycle in the core module)
///   - util/util.rs → core/a.rs   (cross-module dep)
///   - api/api.rs   → util/util.rs
///   - util/util.rs → api/api.rs  (closes a 2-cycle util ↔ api)
///
/// Plus pre-computed `file_metrics` rows so tools that read them
/// (architecture_quality, design_metrics, etc.) get deterministic
/// values without depending on the cron pipeline.
pub async fn seed_graph_corpus(pool: &PgPool) -> GraphHandles {
    let project_id = insert_project(pool, "graph-proj", "/ws/graph").await;

    let mut files = std::collections::HashMap::new();
    let labels: &[(&str, &str, &str, usize)] = &[
        // (label, rel_path, language, basis_idx for embedding)
        // basis_idx: 0/1/2 cluster the files into 3 different
        // semantic regions; the "outlier" file at basis 3 is the
        // intended anomaly_detection target.
        ("a", "core/a.rs", "rust", 0),
        ("b", "core/b.rs", "rust", 0),
        ("c", "core/c.rs", "rust", 0),
        ("util", "util/util.rs", "rust", 1),
        ("api", "api/api.rs", "rust", 2),
    ];
    for (label, rel, lang, basis_idx) in labels {
        let abs = format!("/ws/graph/{rel}");
        let fid = insert_file(pool, project_id, &abs, rel, lang, 100).await;
        // Seed one chunk per file with a deterministic embedding.
        // anomaly_detection uses these as input; the four files at
        // basis 0 form a cluster, util at basis 1, api at basis 2 —
        // both util and api should rank as moderately anomalous
        // relative to the core/ cluster.
        let _ = insert_chunk_with_embedding(
            pool,
            fid,
            0,
            &format!("synthetic content for {rel}"),
            &basis_with_noise(*basis_idx, 0),
            1,
            10,
        )
        .await;
        files.insert(*label, (fid, rel.to_string()));
    }

    let edges: &[(&str, &str)] = &[
        ("a", "b"),
        ("b", "c"),
        ("c", "a"),    // 3-cycle in core/
        ("util", "a"), // util → core
        ("api", "util"),
        ("util", "api"), // 2-cycle util ↔ api
    ];
    for (src, dst) in edges {
        let s = files[src].0;
        let t = files[dst].0;
        insert_graph_edge(pool, project_id, s, Some(t), "import", 1.0).await;
    }

    // Pre-computed metrics. Numbers chosen so each tool's output is
    // hand-traceable (e.g. instability = Ce/(Ca+Ce) for util is 1/2).
    insert_file_metric(
        pool,
        project_id,
        files["a"].0,
        FileMetricInput {
            pagerank: 0.30,
            betweenness: 0.20,
            in_degree: 2,
            out_degree: 1,
            afferent_coupling: 2,
            efferent_coupling: 1,
            instability: 1.0 / 3.0,
            commit_count: 10,
            author_count: 3,
            churn_rate: 5.0,
            bug_proneness: 0.2,
            tech_debt_score: 0.3,
            health_score: 0.7,
        },
    )
    .await;
    insert_file_metric(
        pool,
        project_id,
        files["b"].0,
        FileMetricInput {
            pagerank: 0.20,
            betweenness: 0.10,
            in_degree: 1,
            out_degree: 1,
            afferent_coupling: 1,
            efferent_coupling: 1,
            instability: 0.5,
            commit_count: 5,
            author_count: 2,
            churn_rate: 2.0,
            bug_proneness: 0.1,
            tech_debt_score: 0.2,
            health_score: 0.8,
        },
    )
    .await;
    insert_file_metric(
        pool,
        project_id,
        files["c"].0,
        FileMetricInput {
            pagerank: 0.15,
            betweenness: 0.05,
            in_degree: 1,
            out_degree: 1,
            afferent_coupling: 1,
            efferent_coupling: 1,
            instability: 0.5,
            commit_count: 3,
            author_count: 1,
            churn_rate: 1.0,
            bug_proneness: 0.05,
            tech_debt_score: 0.1,
            health_score: 0.9,
        },
    )
    .await;
    insert_file_metric(
        pool,
        project_id,
        files["util"].0,
        FileMetricInput {
            pagerank: 0.20,
            betweenness: 0.15,
            in_degree: 1,
            out_degree: 2,
            afferent_coupling: 1,
            efferent_coupling: 2,
            instability: 2.0 / 3.0,
            commit_count: 20,
            author_count: 5,
            churn_rate: 8.0,
            bug_proneness: 0.6,
            tech_debt_score: 0.7,
            health_score: 0.3,
        },
    )
    .await;
    insert_file_metric(
        pool,
        project_id,
        files["api"].0,
        FileMetricInput {
            pagerank: 0.15,
            betweenness: 0.10,
            in_degree: 1,
            out_degree: 1,
            afferent_coupling: 1,
            efferent_coupling: 1,
            instability: 0.5,
            commit_count: 8,
            author_count: 2,
            churn_rate: 3.0,
            bug_proneness: 0.3,
            tech_debt_score: 0.4,
            health_score: 0.6,
        },
    )
    .await;

    GraphHandles { project_id, files }
}

pub struct FileMetricInput {
    pub pagerank: f64,
    pub betweenness: f64,
    pub in_degree: i32,
    pub out_degree: i32,
    pub afferent_coupling: i32,
    pub efferent_coupling: i32,
    pub instability: f64,
    pub commit_count: i32,
    pub author_count: i32,
    pub churn_rate: f64,
    pub bug_proneness: f64,
    pub tech_debt_score: f64,
    pub health_score: f64,
}

async fn insert_graph_edge(
    pool: &PgPool,
    project_id: i32,
    source: i64,
    target: Option<i64>,
    edge_type: &str,
    weight: f64,
) {
    sqlx::query(
        "INSERT INTO code_graph_edges \
         (project_id, source_file_id, target_file_id, edge_type, weight) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT DO NOTHING",
    )
    .bind(project_id)
    .bind(source)
    .bind(target)
    .bind(edge_type)
    .bind(weight)
    .execute(pool)
    .await
    .expect("graph edge");
}

async fn insert_file_metric(pool: &PgPool, project_id: i32, file_id: i64, m: FileMetricInput) {
    sqlx::query(
        "INSERT INTO file_metrics \
         (file_id, project_id, pagerank, betweenness, in_degree, out_degree, \
          afferent_coupling, efferent_coupling, instability, commit_count, \
          author_count, churn_rate, bug_proneness, tech_debt_score, health_score) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
    )
    .bind(file_id)
    .bind(project_id)
    .bind(m.pagerank)
    .bind(m.betweenness)
    .bind(m.in_degree)
    .bind(m.out_degree)
    .bind(m.afferent_coupling)
    .bind(m.efferent_coupling)
    .bind(m.instability)
    .bind(m.commit_count)
    .bind(m.author_count)
    .bind(m.churn_rate)
    .bind(m.bug_proneness)
    .bind(m.tech_debt_score)
    .bind(m.health_score)
    .execute(pool)
    .await
    .expect("file_metric");
}

// ============================================================================
// Engineering scorecard corpus (Phase J)
// ============================================================================
//
// Each scenario seeds a project tuned so that every dimension of
// `engineering_scorecard` lands at a predetermined letter band. The
// 10 dimensions are computed from `indexed_files` (size, language,
// path-pattern, line_count) and `file_metrics` (churn, fix ratio,
// coupling, author count, days_since_last_change), plus
// `code_graph_edges` for the dependency-health (cycles) dimension.
//
// Scenarios:
//
//   PerfectInputs: every dimension scores ≥ 90 → A
//   FailingInputs: every dimension scores < 60 → F
//   MixedInputs:   each dimension lands in a distinct band so the
//                  test asserts the per-dimension grade table

#[derive(Debug, Clone, Copy)]
pub enum ScorecardScenario {
    Perfect,
    Failing,
    /// `(no_circular_deps, low_churn, low_fix_ratio, no_god_files,
    /// bus_factor_ok, recently_maintained, has_documentation,
    /// test_coverage)` switches let oracle tests pin specific ORR
    /// items individually.
    OrrFailures {
        cycles: bool,
        high_churn: bool,
        high_fix: bool,
        god_files: bool,
        single_author: bool,
        stale: bool,
        no_docs: bool,
        no_tests: bool,
    },
}

pub async fn seed_scorecard_corpus(
    pool: &PgPool,
    project_name: &str,
    scenario: ScorecardScenario,
) -> i32 {
    let project_id = insert_project(pool, project_name, &format!("/ws/{project_name}")).await;
    match scenario {
        ScorecardScenario::Perfect => seed_perfect(pool, project_id).await,
        ScorecardScenario::Failing => seed_failing(pool, project_id).await,
        ScorecardScenario::OrrFailures {
            cycles,
            high_churn,
            high_fix,
            god_files,
            single_author,
            stale,
            no_docs,
            no_tests,
        } => {
            seed_orr_tunable(
                pool,
                project_id,
                cycles,
                high_churn,
                high_fix,
                god_files,
                single_author,
                stale,
                no_docs,
                no_tests,
            )
            .await
        }
    }
    project_id
}

/// 10 source files (avg lc 200), 2 markdown files (≥10% docs),
/// 2 test files (≥20% test ratio after counting), zero churn /
/// fix / coupling, multiple authors, no cycles, no god files.
async fn seed_perfect(pool: &PgPool, project_id: i32) {
    // 8 prod files at exactly 200 lines.
    for i in 0..8 {
        let path = format!("src/m{i}.rs");
        let abs = format!("/ws/perfect/{path}");
        let fid = insert_file(pool, project_id, &abs, &path, "rust", 200).await;
        insert_perfect_metric(pool, project_id, fid).await;
    }
    // 2 test files (test_ratio = 2/12 ≈ 0.166 → score 83.3 → B).
    // Bump to 3 tests for safety: 3/13 ≈ 0.23 → 100 → A.
    for i in 0..3 {
        let path = format!("tests/it{i}.rs");
        let abs = format!("/ws/perfect/{path}");
        let fid = insert_file(pool, project_id, &abs, &path, "rust", 200).await;
        insert_perfect_metric(pool, project_id, fid).await;
    }
    // 2 markdown files (doc_ratio = 2/15 ≈ 0.133 → ×10 → 100 → A).
    for i in 0..2 {
        let path = format!("docs/d{i}.md");
        let abs = format!("/ws/perfect/{path}");
        let fid = insert_file(pool, project_id, &abs, &path, "markdown", 200).await;
        insert_perfect_metric(pool, project_id, fid).await;
    }
}

async fn insert_perfect_metric(pool: &PgPool, project_id: i32, file_id: i64) {
    insert_file_metric(
        pool,
        project_id,
        file_id,
        FileMetricInput {
            pagerank: 0.1,
            betweenness: 0.0,
            in_degree: 0,
            out_degree: 0,
            afferent_coupling: 0,
            efferent_coupling: 0,
            instability: 0.5,
            commit_count: 1,
            author_count: 4,
            churn_rate: 0.0,
            bug_proneness: 0.0,
            tech_debt_score: 0.0,
            health_score: 1.0,
        },
    )
    .await;
    sqlx::query("UPDATE file_metrics SET days_since_last_change = 0 WHERE file_id = $1")
        .bind(file_id)
        .execute(pool)
        .await
        .expect("days");
}

/// Worst-case inputs across every dimension.
async fn seed_failing(pool: &PgPool, project_id: i32) {
    // 10 god-class files (line_count = 1500 → avg 1500 → size_score 0).
    let mut file_ids = Vec::with_capacity(10);
    for i in 0..10 {
        let path = format!("src/giant{i}.rs");
        let abs = format!("/ws/failing/{path}");
        let fid = insert_file(pool, project_id, &abs, &path, "rust", 1500).await;
        file_ids.push(fid);
        insert_file_metric(
            pool,
            project_id,
            fid,
            FileMetricInput {
                pagerank: 0.1,
                betweenness: 0.0,
                in_degree: 5,
                out_degree: 5,
                afferent_coupling: 15,
                efferent_coupling: 15,
                instability: 0.5,
                commit_count: 100,
                author_count: 1,
                churn_rate: 10.0, // ≥ 5 → 0
                bug_proneness: 0.9,
                tech_debt_score: 0.9,
                health_score: 0.0,
            },
        )
        .await;
        sqlx::query("UPDATE file_metrics SET fix_commit_ratio = 0.5, days_since_last_change = 730 WHERE file_id = $1")
            .bind(fid)
            .execute(pool)
            .await
            .expect("fix+stale");
    }
    // Add a 3-cycle so dep_score drops.
    if file_ids.len() >= 3 {
        for win in &[(0_usize, 1_usize), (1, 2), (2, 0)] {
            sqlx::query(
                "INSERT INTO code_graph_edges (project_id, source_file_id, target_file_id, edge_type, weight) \
                 VALUES ($1, $2, $3, 'import', 1.0)",
            )
            .bind(project_id)
            .bind(file_ids[win.0])
            .bind(file_ids[win.1])
            .execute(pool)
            .await
            .expect("edge");
        }
    }
    // No tests, no docs.
}

/// Per-ORR-checklist toggles. Every off switch leaves the dimension
/// in the "passing" range so individual items can be tested in
/// isolation.
#[allow(clippy::too_many_arguments)]
async fn seed_orr_tunable(
    pool: &PgPool,
    project_id: i32,
    cycles: bool,
    high_churn: bool,
    high_fix: bool,
    god_files: bool,
    single_author: bool,
    stale: bool,
    no_docs: bool,
    no_tests: bool,
) {
    // Base: 10 healthy 200-line files.
    let mut file_ids = Vec::with_capacity(10);
    for i in 0..10 {
        let path = format!("src/m{i}.rs");
        let abs = format!("/ws/orr/{path}");
        let lc = if god_files && i < 6 { 1500 } else { 200 };
        let fid = insert_file(pool, project_id, &abs, &path, "rust", lc).await;
        file_ids.push(fid);
        insert_file_metric(
            pool,
            project_id,
            fid,
            FileMetricInput {
                pagerank: 0.1,
                betweenness: 0.0,
                in_degree: 0,
                out_degree: 0,
                afferent_coupling: 0,
                efferent_coupling: 0,
                instability: 0.5,
                commit_count: 1,
                author_count: if single_author { 1 } else { 4 },
                churn_rate: if high_churn { 8.0 } else { 0.0 },
                bug_proneness: 0.0,
                tech_debt_score: 0.0,
                health_score: 1.0,
            },
        )
        .await;
        let fix = if high_fix { 0.5 } else { 0.0 };
        let days = if stale { 400 } else { 0 };
        sqlx::query(
            "UPDATE file_metrics SET fix_commit_ratio = $1, days_since_last_change = $2 WHERE file_id = $3",
        )
        .bind(fix)
        .bind(days)
        .bind(fid)
        .execute(pool)
        .await
        .expect("fix+stale tunable");
    }
    if !no_tests {
        for i in 0..3 {
            let path = format!("tests/it{i}.rs");
            let abs = format!("/ws/orr/{path}");
            let fid = insert_file(pool, project_id, &abs, &path, "rust", 200).await;
            insert_perfect_metric(pool, project_id, fid).await;
        }
    }
    if !no_docs {
        for i in 0..2 {
            let path = format!("docs/d{i}.md");
            let abs = format!("/ws/orr/{path}");
            let fid = insert_file(pool, project_id, &abs, &path, "markdown", 200).await;
            insert_perfect_metric(pool, project_id, fid).await;
        }
    }
    if cycles && file_ids.len() >= 3 {
        for win in &[(0_usize, 1_usize), (1, 2), (2, 0)] {
            sqlx::query(
                "INSERT INTO code_graph_edges (project_id, source_file_id, target_file_id, edge_type, weight) \
                 VALUES ($1, $2, $3, 'import', 1.0)",
            )
            .bind(project_id)
            .bind(file_ids[win.0])
            .bind(file_ids[win.1])
            .execute(pool)
            .await
            .expect("edge");
        }
    }
}
