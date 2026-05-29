//! Pure helpers and small support types shared by the MCP tool handlers and
//! the CLI dispatch path. Extracted from `server.rs` as part of the god-file
//! split (B.1). These items are *not* per-call instrumentation — that lives in
//! `server.rs` (`instrumented_tool_wrap` / `instrumented_tool_run`). Everything
//! here is `pub(crate)` and re-exported by `server.rs` via
//! `pub(crate) use support::*;` so the handler modules and CLI dispatch reach
//! them unchanged.
#![allow(unused_imports)]

use rmcp::model::CallToolResult;
use rmcp::{ErrorData as McpError, RoleServer};

use super::error_classify::classify_error_kind;

/// Identifying metadata about the MCP peer that issued the current call,
/// derived from the rmcp `RequestContext`. `client_name` is normalized to
/// lowercase so per-(tool, client) breakdowns are stable across capitalization
/// variants in `clientInfo.name`.
///
/// `client_version` and `protocol_version` are captured for Tier 3's DB-row
/// telemetry but unused by the Tier 1 in-memory counters. The
/// `#[allow(dead_code)]` is removed once those fields land in the
/// `mcp_tool_calls` row builder.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct CallerInfo {
    pub client_name: String,
    pub client_version: String,
    pub protocol_version: String,
}

impl CallerInfo {
    pub fn unknown() -> Self {
        Self {
            client_name: "unknown".to_string(),
            client_version: "unknown".to_string(),
            protocol_version: "unknown".to_string(),
        }
    }
}

/// Compact one-line summary of a tool's typed parameters for logging.
/// Uses `Debug` (every `*Params` struct in this file derives it). Truncates
/// to 200 bytes on a valid UTF-8 char boundary with a `…(+NB)` suffix
/// indicating how many bytes were elided.
pub(crate) fn summarize_debug<P: std::fmt::Debug + ?Sized>(p: &P) -> String {
    truncate_for_log(&format!("{:?}", p))
}

/// Compact one-line summary of a raw JSON params value (used by the CLI
/// dispatch path which receives `serde_json::Value` rather than a typed
/// struct). Uses `Value::to_string` for readable JSON shape.
pub(crate) fn summarize_json(v: &serde_json::Value) -> String {
    truncate_for_log(&v.to_string())
}

pub(crate) fn truncate_for_log(s: &str) -> String {
    if s.len() <= 200 {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(200);
        format!("{}…(+{}B)", &s[..end], s.len() - end)
    }
}

/// Classify a tool result into the `outcome` + `error_class` columns of
/// `mcp_tool_calls`. The `timeout` outcome is detected when the duration
/// is at-or-above the budget AND the error message mentions "timed out".
pub(crate) fn classify_result(
    result: &Result<CallToolResult, McpError>,
    timeout_secs: u64,
    elapsed: std::time::Duration,
) -> (&'static str, Option<String>) {
    match result {
        Ok(_) => ("ok", None),
        Err(e) => {
            let msg = e.to_string();
            let is_timeout = elapsed.as_secs() >= timeout_secs && msg.contains("timed out");
            if is_timeout {
                ("timeout", Some("timeout".to_string()))
            } else {
                ("error", Some(classify_error_kind(&msg)))
            }
        }
    }
}

/// Truncate a string to at most `max_len` bytes on a valid char boundary.
pub(crate) fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..s.floor_char_boundary(max_len)]
    }
}

// ============================================================================
// Union-Find for duplicate clustering
// ============================================================================

pub(crate) struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    pub(crate) fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    pub(crate) fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }

    pub(crate) fn union(&mut self, x: usize, y: usize) {
        let rx = self.find(x);
        let ry = self.find(y);
        if rx == ry {
            return;
        }
        if self.rank[rx] < self.rank[ry] {
            self.parent[rx] = ry;
        } else if self.rank[rx] > self.rank[ry] {
            self.parent[ry] = rx;
        } else {
            self.parent[ry] = rx;
            self.rank[rx] += 1;
        }
    }
}

/// Cluster duplicate file pairs using union-find.
/// Returns clusters that span at least `min_projects` distinct projects.
pub(crate) fn cluster_file_pairs(
    pairs: &[crate::db::queries::DuplicateFilePair],
    min_projects: usize,
) -> Vec<serde_json::Value> {
    use std::collections::{HashMap, HashSet};

    if pairs.is_empty() {
        return Vec::new();
    }

    // Assign each unique file_id an index
    let mut file_ids: Vec<i64> = Vec::new();
    let mut id_to_idx: HashMap<i64, usize> = HashMap::new();

    for pair in pairs {
        if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(pair.file_id_a) {
            e.insert(file_ids.len());
            file_ids.push(pair.file_id_a);
        }
        if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(pair.file_id_b) {
            e.insert(file_ids.len());
            file_ids.push(pair.file_id_b);
        }
    }

    // Build file metadata map
    struct FileMeta {
        path: String,
        project_name: String,
        project_id: i32,
        language: String,
        line_count: Option<i64>,
    }

    let mut meta: HashMap<i64, FileMeta> = HashMap::new();
    for pair in pairs {
        meta.entry(pair.file_id_a).or_insert_with(|| FileMeta {
            path: pair.path_a.clone(),
            project_name: pair.project_name_a.clone(),
            project_id: pair.project_id_a,
            language: pair.language.clone(),
            line_count: None,
        });
        meta.entry(pair.file_id_b).or_insert_with(|| FileMeta {
            path: pair.path_b.clone(),
            project_name: pair.project_name_b.clone(),
            project_id: pair.project_id_b,
            language: pair.language.clone(),
            line_count: None,
        });
    }

    // Union-find clustering
    let mut uf = UnionFind::new(file_ids.len());
    let mut pair_sims: HashMap<(usize, usize), f64> = HashMap::new();
    for pair in pairs {
        let ia = id_to_idx[&pair.file_id_a];
        let ib = id_to_idx[&pair.file_id_b];
        uf.union(ia, ib);
        pair_sims.insert((ia.min(ib), ia.max(ib)), pair.avg_similarity);
    }

    // Collect clusters
    let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..file_ids.len() {
        let root = uf.find(i);
        clusters.entry(root).or_default().push(i);
    }

    // Filter to clusters spanning min_projects and format output
    let mut result: Vec<serde_json::Value> = Vec::new();
    for members in clusters.values() {
        let mut projects: HashSet<i32> = HashSet::new();
        let mut project_names: HashSet<String> = HashSet::new();
        let mut files = Vec::new();
        let mut language = String::new();
        let mut sim_sum = 0.0f64;
        let mut sim_count = 0u64;

        for &idx in members {
            let fid = file_ids[idx];
            if let Some(m) = meta.get(&fid) {
                projects.insert(m.project_id);
                project_names.insert(m.project_name.clone());
                language = m.language.clone();

                // Extract relative_path from absolute path (last path components after project root)
                let rel_path = m
                    .path
                    .rsplit('/')
                    .take(4)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("/");
                files.push(serde_json::json!({
                    "file_id": fid,
                    "path": m.path,
                    "relative_path": rel_path,
                    "project": m.project_name,
                    "line_count": m.line_count,
                }));
            }
        }

        // Calculate average similarity across all pairs in this cluster
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                let key = (members[i].min(members[j]), members[i].max(members[j]));
                if let Some(&sim) = pair_sims.get(&key) {
                    sim_sum += sim;
                    sim_count += 1;
                }
            }
        }

        if projects.len() < min_projects {
            continue;
        }

        let avg_sim = if sim_count > 0 {
            sim_sum / sim_count as f64
        } else {
            0.0
        };

        result.push(serde_json::json!({
            "cluster_size": members.len(),
            "projects": project_names.into_iter().collect::<Vec<_>>(),
            "project_count": projects.len(),
            "language": language,
            "avg_similarity": format!("{:.4}", avg_sim),
            "files": files,
            "representative_file": files.first(),
        }));
    }

    // Sort by project_count * avg_similarity descending
    result.sort_by(|a, b| {
        let score_a = a["project_count"].as_u64().unwrap_or(0) as f64
            * a["avg_similarity"]
                .as_str()
                .unwrap_or("0")
                .parse::<f64>()
                .unwrap_or(0.0);
        let score_b = b["project_count"].as_u64().unwrap_or(0) as f64
            * b["avg_similarity"]
                .as_str()
                .unwrap_or("0")
                .parse::<f64>()
                .unwrap_or(0.0);
        score_b
            .partial_cmp(&score_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    result
}

/// Infer a suggested crate name from common path segments across files.
pub(crate) fn infer_crate_name(paths: &[&str]) -> String {
    if paths.is_empty() {
        return "shared-lib".to_string();
    }

    // Find common path segments (ignoring project root differences)
    // Take the last meaningful segment that appears in most paths
    let mut segment_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for path in paths {
        let segments: std::collections::HashSet<&str> = path
            .split('/')
            .filter(|s| !s.is_empty() && *s != "src" && *s != "mod.rs" && !s.contains('.'))
            .collect();
        for seg in segments {
            *segment_counts.entry(seg).or_insert(0) += 1;
        }
    }

    // Find the segment that appears in the most paths (excluding very generic ones)
    let generic = ["lib", "main", "index", "utils", "helpers", "common"];
    segment_counts
        .into_iter()
        .filter(|(seg, count)| *count > 1 && !generic.contains(seg))
        .max_by_key(|(_, count)| *count)
        .map(|(seg, _)| seg.replace('_', "-"))
        .unwrap_or_else(|| "shared-lib".to_string())
}

// ============================================================================
// Agglomerative clustering for topic hierarchy (ndarray-accelerated)
// ============================================================================

/// Agglomerative clustering with average linkage on topic centroids.
///
/// Pairwise cosine similarities are computed as a single matrix multiplication
/// `sim = C × Cᵀ` using ndarray, which is orders of magnitude faster than
/// element-wise loops (exploits SIMD and cache-friendly memory access).
///
/// Returns (groups, dendrogram).
pub(crate) fn agglomerative_cluster(
    centroids: &[&[f32]],
    labels: &[String],
    sizes: &[i64],
    topic_ids: &[i32],
    num_groups: usize,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    use ndarray::Array2;

    let n = centroids.len();
    let dim = centroids[0].len();

    // Build centroid matrix (n × dim) as f64 for precision
    let mut centroid_matrix = Array2::<f64>::zeros((n, dim));
    for (i, centroid) in centroids.iter().enumerate() {
        for (j, &val) in centroid.iter().enumerate() {
            centroid_matrix[[i, j]] = val as f64;
        }
    }

    // Compute full pairwise cosine similarity matrix via matmul: sim = C × Cᵀ
    // Since centroids are L2-normalized, dot product = cosine similarity.
    let sim_matrix = centroid_matrix.dot(&centroid_matrix.t());

    // Initialize cluster-level similarity matrix from point similarity matrix.
    // UPGMA update formula maintains this incrementally: O(k) per merge instead
    // of O(|Ci|×|Cj|) member-pair recomputation.
    let mut cluster_sim: Vec<Vec<f64>> = (0..n)
        .map(|i| (0..n).map(|j| sim_matrix[[i, j]]).collect())
        .collect();
    let mut cluster_sizes: Vec<usize> = vec![1; n];

    let mut cluster_members: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
    let mut dendrogram: Vec<serde_json::Value> = Vec::new();

    // Active index list: avoids scanning deactivated indices every iteration
    let mut active_indices: Vec<usize> = (0..n).collect();
    let mut step = 0;

    while active_indices.len() > num_groups {
        // Find the most similar pair among active clusters
        let mut best_sim = f64::NEG_INFINITY;
        let mut best_i = 0;
        let mut best_j = 0;

        for (ai, &i) in active_indices.iter().enumerate() {
            for &j in &active_indices[ai + 1..] {
                if cluster_sim[i][j] > best_sim {
                    best_sim = cluster_sim[i][j];
                    best_i = i;
                    best_j = j;
                }
            }
        }

        // Record dendrogram step
        step += 1;
        let all_merged: Vec<&str> = cluster_members[best_i]
            .iter()
            .chain(cluster_members[best_j].iter())
            .map(|&idx| labels[idx].as_str())
            .collect();

        dendrogram.push(serde_json::json!({
            "step": step,
            "merged": all_merged,
            "distance": format!("{:.4}", 1.0 - best_sim),
        }));

        // UPGMA update: recompute cluster_sim[best_i][k] for all active k
        let size_a = cluster_sizes[best_i];
        let size_b = cluster_sizes[best_j];
        let total = size_a + size_b;
        for &k in &active_indices {
            if k == best_i || k == best_j {
                continue;
            }
            let new_sim = (size_a as f64 * cluster_sim[best_i][k]
                + size_b as f64 * cluster_sim[best_j][k])
                / total as f64;
            cluster_sim[best_i][k] = new_sim;
            cluster_sim[k][best_i] = new_sim;
        }
        cluster_sizes[best_i] = total;

        // Merge cluster best_j into best_i
        let members_j = cluster_members[best_j].clone();
        cluster_members[best_i].extend(members_j);

        // Remove best_j from active indices
        active_indices.retain(|&x| x != best_j);
    }

    // Build output groups from remaining active clusters
    let mut groups: Vec<serde_json::Value> = Vec::new();
    for &ci in &active_indices {
        let members = &cluster_members[ci];

        let group_topics: Vec<serde_json::Value> = members
            .iter()
            .map(|&idx| {
                serde_json::json!({
                    "id": topic_ids[idx],
                    "label": labels[idx],
                    "size": sizes[idx],
                })
            })
            .collect();

        // Group label: join topic labels with " + "
        let group_label = members
            .iter()
            .map(|&idx| labels[idx].as_str())
            .collect::<Vec<_>>()
            .join(" + ");

        // Average internal distance from precomputed point-level sim_matrix
        let mut internal_sum = 0.0f64;
        let mut internal_count = 0usize;
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                internal_sum += 1.0 - sim_matrix[[members[i], members[j]]];
                internal_count += 1;
            }
        }
        let avg_distance = if internal_count > 0 {
            internal_sum / internal_count as f64
        } else {
            0.0
        };

        groups.push(serde_json::json!({
            "group_label": group_label,
            "merge_distance": format!("{:.4}", avg_distance),
            "topic_count": members.len(),
            "topics": group_topics,
        }));
    }

    // Sort groups by size descending
    groups.sort_by(|a, b| {
        let sa = a["topic_count"].as_u64().unwrap_or(0);
        let sb = b["topic_count"].as_u64().unwrap_or(0);
        sb.cmp(&sa)
    });

    (groups, dendrogram)
}

/// Format a ClusteringSummary into the JSON response structure.
pub(crate) fn format_clustering_summary(
    summary: &crate::cron::topic_clustering::ClusteringSummary,
    limit: i32,
) -> serde_json::Value {
    let noise_pct = if summary.chunks_analyzed > 0 {
        summary.noise_chunks as f64 / summary.chunks_analyzed as f64 * 100.0
    } else {
        0.0
    };

    let topics: Vec<serde_json::Value> = summary.topics.iter().take(limit as usize).map(|t| {
        serde_json::json!({
            "id": t.cluster_index,
            "label": t.label,
            "keywords": t.keywords,
            "keyword_scores": t.keyword_scores.iter().map(|s| format!("{:.4}", s)).collect::<Vec<_>>(),
            "size": t.chunk_ids.len(),
            "files": t.file_ids.len(),
            "projects": t.project_names,
            "project_count": t.project_names.len(),
            "avg_internal_similarity": format!("{:.4}", t.avg_internal_similarity),
            "representative_files": t.top_files.iter().take(10).map(|f| serde_json::json!({
                "path": f.path,
                "project": f.project,
                "chunks": f.chunks_in_topic,
            })).collect::<Vec<_>>(),
            "representative_snippet": truncate(&t.representative_snippet, 500),
        })
    }).collect();

    serde_json::json!({
        "scope": summary.scope,
        "algorithm": "Fuzzy C-Means + c-TF-IDF",
        "params": {
            "num_clusters": summary.num_clusters,
            "fuzziness": summary.fuzziness,
            "converged": summary.converged,
            "iterations": summary.iterations,
        },
        "chunks_analyzed": summary.chunks_analyzed,
        "topics_found": summary.topics_found,
        "noise_chunks": summary.noise_chunks,
        "noise_pct": format!("{:.1}", noise_pct),
        "topics": topics,
        "guidance": "Use compare_files to examine specific file pairs within a topic. \
                     Topics with high avg_internal_similarity and multiple files indicate \
                     DRY candidates. Keywords show c-TF-IDF extracted semantic labels.",
    })
}
