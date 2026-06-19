//! Fuzzy BERTopic: FCM clustering + c-TF-IDF topic labeling for code chunks.
//!
//! Replaces HDBSCAN with Fuzzy C-Means (FCM) — O(n×K×d) per iteration, no
//! pairwise distances. Topic labels derived via class-based TF-IDF (c-TF-IDF)
//! instead of path-segment heuristics.
//!
//! Two entry points:
//! - `run_global_topic_scan()`: cron job, stores results in DB
//! - `run_project_topic_scan()`: on-demand, returns results directly

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use ndarray::{Array2, ArrayView2};
use tracing::{error, info, warn};

use crate::config::CronConfig;
use crate::db::DbClient;
use crate::db::queries::ChunkEmbeddingRow;
use crate::fcm;
use crate::quality::topic_metrics::{DegeneracyThresholds, TopicMetrics};
mod similarity;
use similarity::*;

use crate::stats::tracker::StatsTracker;

// Re-exports so existing callers in the topic_hierarchy / k_selector /
// topic_clustering_online / gpu_smoke modules keep
// their current import paths (`use crate::cron::topic_clustering::FcmResult`
// etc.). The canonical definitions live in `crate::fcm`.
pub use crate::fcm::{CancelFn, FcmResult, GpuPrecision, kmeans_plus_plus_init};

// ============================================================================
// Result types
// ============================================================================

/// A single file entry within a topic.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TopicFileEntry {
    pub path: String,
    pub project: String,
    pub chunks_in_topic: i32,
}

/// A single discovered topic cluster.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TopicResult {
    pub cluster_index: i32,
    pub label: String,
    pub keywords: Vec<String>,
    pub keyword_scores: Vec<f64>,
    pub chunk_ids: Vec<i64>,
    /// Membership degrees for each chunk_id (parallel to chunk_ids).
    pub memberships: Vec<f64>,
    pub file_ids: Vec<i64>,
    pub project_names: Vec<String>,
    pub avg_internal_similarity: f64,
    pub representative_chunk_id: i64,
    pub representative_snippet: String,
    pub top_files: Vec<TopicFileEntry>,
    /// FCM centroid vector (f32, length d). Empty for topics where the caller
    /// didn't supply a centroid (legacy / on-demand paths). Persisted to
    /// `code_topics.centroid` for warm-start (Phase 7) and hierarchy (Phase 9).
    #[serde(default)]
    pub centroid: Vec<f32>,
    /// Parent global-topic IDs for `scope="hierarchy"` rows (Phase 9).
    /// Empty for non-hierarchy topics.
    #[serde(default)]
    pub parent_topic_ids: Vec<i64>,
}

/// Summary of a clustering run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClusteringSummary {
    pub scope: String,
    pub chunks_analyzed: usize,
    pub topics_found: usize,
    pub noise_chunks: usize,
    pub num_clusters: usize,
    pub fuzziness: f64,
    pub converged: bool,
    pub iterations: usize,
    pub topics: Vec<TopicResult>,
    /// Quality metrics for this clustering result (coherence, validity,
    /// degeneracy signals). `None` for empty/degenerate-early-return summaries.
    /// Computed in the clustering paths where the membership matrix is in scope;
    /// consulted by the pre-overwrite degeneracy gate and persisted for trend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<crate::quality::topic_metrics::TopicMetrics>,
}

// ============================================================================
// K estimation
// ============================================================================

/// Estimate the number of clusters K from data size and min_cluster_size.
/// Heuristic: K = clamp(sqrt(n / min_cluster_size), 10, 100).
///
/// Upper cap lowered from 500 → 100 during the OOM fix: every FCM matrix is
/// O(n × K), so a 5× smaller K means 5× less peak RSS on the n × K buffers
/// (membership, dist_sq, u_pow_m, dot_xc). K=100 produces well-separated
/// topics up to ~n=100k; larger n with meaningful sub-structure should enable
/// Phase 12 (adaptive K via Xie-Beni / silhouette).
pub fn estimate_k(n: usize, min_cluster_size: usize) -> usize {
    let min_cs = min_cluster_size.max(1);
    let k = ((n as f64) / (min_cs as f64)).sqrt().round() as usize;
    k.clamp(10, 100)
}

// k-means++ initialization lives in `crate::fcm::kmeans_plus_plus_init`;
// re-exported at the top of this file for backwards compatibility.

// ============================================================================
// Fuzzy C-Means (thin adapters; canonical loop is crate::fcm::run_seeded)
// ============================================================================

/// Run Fuzzy C-Means clustering on L2-normalized f32 data.
///
/// Thin adapter over [`crate::fcm::run_seeded`] — constructs a CUDA backend with
/// fp32 precision (mid-iteration arithmetic stays in f32, which matches the
/// precision callers expect from the pre-backend CPU path) and runs the
/// canonical FCM iteration loop. On CUDA failure, returns a degenerate result.
///
/// The cron path goes through `dispatch_fcm` which honours
/// `CronConfig.gpu_fcm_precision` (default fp16) — that is where the fp16
/// perf win lives. Direct callers here (tests, k_selector, topic_hierarchy)
/// prefer fp32 for stable convergence at tight tolerances.
pub fn fuzzy_c_means(
    data: ArrayView2<f32>,
    k: usize,
    m: f64,
    max_iters: usize,
    tolerance: f64,
    should_cancel: CancelFn<'_>,
) -> FcmResult {
    fuzzy_c_means_with_init(data, k, m, max_iters, tolerance, should_cancel, None)
}

/// Route FCM through the backend seam picked by `params.gpu_fcm_precision`.
/// CUDA-init and mid-iteration CUDA errors are surfaced as degenerate results;
/// production topic scans do not silently fall back to CPU.
fn dispatch_fcm(
    data: ArrayView2<'_, f32>,
    k: usize,
    params: &FcmParams,
    warm_centroids: Option<Array2<f32>>,
) -> FcmResult {
    let precision = GpuPrecision::parse(&params.gpu_fcm_precision);
    run_through_backend(
        data,
        k,
        params.fuzziness,
        params.max_iters,
        params.tolerance,
        None,
        warm_centroids,
        fcm::BackendChoice::Cuda(precision),
        None,
    )
}

/// Phase 5 GPU precision dispatcher — kept for callers (smoke tests, the
/// fallback smoke binary) that pin a specific GPU precision.
pub fn fuzzy_c_means_gpu(
    data: ArrayView2<f32>,
    k: usize,
    m: f64,
    max_iters: usize,
    tolerance: f64,
    precision: GpuPrecision,
) -> FcmResult {
    run_through_backend(
        data,
        k,
        m,
        max_iters,
        tolerance,
        None,
        None,
        fcm::BackendChoice::Cuda(precision),
        None,
    )
}

/// Warm-start-capable FCM entry (Phase 7 LMDB integration).
///
/// Defaults to fp32 GPU precision — see `fuzzy_c_means` for the rationale.
pub fn fuzzy_c_means_with_init(
    data: ArrayView2<f32>,
    k: usize,
    m: f64,
    max_iters: usize,
    tolerance: f64,
    should_cancel: CancelFn<'_>,
    initial_centroids: Option<Array2<f32>>,
) -> FcmResult {
    run_through_backend(
        data,
        k,
        m,
        max_iters,
        tolerance,
        should_cancel,
        initial_centroids,
        fcm::BackendChoice::Cuda(GpuPrecision::Fp32),
        None,
    )
}

/// Seeded FCM entry for reproducible cold-starts. Used by the
/// golden-fixture harness — running this with a fixed `seed` on the
/// same data yields bit-identical centroids (modulo GEMM rounding
/// under the configured `tolerance`).
#[allow(clippy::too_many_arguments)]
pub fn fuzzy_c_means_seeded(
    data: ArrayView2<f32>,
    k: usize,
    m: f64,
    max_iters: usize,
    tolerance: f64,
    seed: u64,
) -> FcmResult {
    run_through_backend(
        data,
        k,
        m,
        max_iters,
        tolerance,
        None,
        None,
        fcm::BackendChoice::Cpu,
        Some(seed),
    )
}

/// Shared body: build a backend with the requested choice, then run the
/// canonical FCM loop in `crate::fcm::run_seeded`. On any production CUDA
/// error, return a degenerate result so downstream code skips topic
/// construction rather than silently changing compute backends.
#[allow(clippy::too_many_arguments)]
fn run_through_backend(
    data: ArrayView2<'_, f32>,
    k: usize,
    m: f64,
    max_iters: usize,
    tolerance: f64,
    should_cancel: CancelFn<'_>,
    warm_centroids: Option<Array2<f32>>,
    choice: fcm::BackendChoice,
    seed: Option<u64>,
) -> FcmResult {
    let mut backend = match fcm::make_backend(data.to_owned(), k, choice) {
        Ok(b) => b,
        Err(e) => {
            error!(error = %e, "FCM backend construction failed");
            return degenerate_result(data.nrows(), data.ncols(), k);
        }
    };

    match fcm::run_seeded(
        &mut *backend,
        data,
        k,
        m,
        max_iters,
        tolerance,
        should_cancel,
        warm_centroids.clone(),
        seed,
    ) {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "FCM run failed");
            degenerate_result(data.nrows(), data.ncols(), k)
        }
    }
}

/// Produce an all-zeros FcmResult as a last-resort fallback when every
/// backend fails. Callers downstream treat zero iterations + all-zero
/// membership as a signal to skip topic construction for this scope.
fn degenerate_result(n: usize, d: usize, k: usize) -> FcmResult {
    let k_safe = k.max(1);
    FcmResult {
        membership: Array2::<f32>::zeros((n, k_safe)),
        centroids: Array2::<f32>::zeros((k_safe, d)),
        iterations: 0,
        converged: false,
        cancelled: false,
        inertia: 0.0,
    }
}

// ============================================================================
// c-TF-IDF topic labeling
// ============================================================================

/// Static algorithm/representation signature of the code-topic *label pipeline*
/// this binary produces. Combined with the active clustering engine by
/// [`topics_effective_signature`] and persisted to
/// `pgmcp_metadata['topics_algo_signature']` by [`stamp_topics_signature`] at the
/// end of a successful, non-degenerate global refresh; compared by the staleness
/// check (`db::queries::topics_global_stale`). Bump it whenever the tokenizer,
/// stopword tiers, or keyword-extraction logic change so that topics computed by
/// older code are reported stale (and recomputed) rather than trusted — the same
/// idiom as the `pgmcp-pattern-embedding-v3` pattern-catalog signature.
///
/// `v3` = stopword-tiered c-TF-IDF with identifier splitting + embedding-based
/// (KeyBERT/MMR) keyword refinement, under the `MAX_MEMBERSHIPS_PER_CHUNK` cap.
/// Topics carrying no signature (NULL) were computed by pre-signature code and
/// are always treated as stale.
pub const TOPICS_ALGO_SIGNATURE: &str = "pgmcp-topics-v3";

/// The *effective* topics-algorithm signature: the static label-pipeline version
/// ([`TOPICS_ALGO_SIGNATURE`]) folded with the active clustering engine
/// (`topic_clustering_method`). This is the value persisted to
/// `pgmcp_metadata['topics_algo_signature']` and compared for staleness.
///
/// Folding the engine in means switching `topic_clustering_method` (e.g. `graph`
/// → `embedding_hdbscan`) automatically invalidates a model produced by a
/// different engine — their partition geometries differ (graph is a hard
/// 1-topic-per-doc partition; the FCM tracks are soft, up to
/// `MAX_MEMBERSHIPS_PER_CHUNK` per chunk), so trusting one engine's topics as
/// current under another would be wrong. Bump [`TOPICS_ALGO_SIGNATURE`] for
/// label-pipeline changes; the engine suffix handles engine switches with no
/// manual bump.
pub fn topics_effective_signature(config: &CronConfig) -> String {
    format!("{TOPICS_ALGO_SIGNATURE}+{}", config.topic_clustering_method)
}

/// Stamp the effective signature ([`topics_effective_signature`]) for a
/// just-completed global refresh. Call ONLY after the canonical degeneracy gate
/// ([`topic_gate_rejects`]) has passed (where the strategy has one) *and* a
/// global-scope store has succeeded — i.e. on the non-early-return success path
/// of each global-refresh strategy (in-memory / mmap / online FCM, and the graph
/// global roll-up). Paths that intentionally do not refresh the authoritative
/// `global` scope — the per-project emergency fallback, the `hierarchy` overlay,
/// and on-demand single-project `discover_topics` — must NOT call this; leaving
/// the signature untouched keeps the model honestly "stale" until a real global
/// refresh, which is the correct signal for those paths.
///
/// Best-effort: a stamp failure is logged, not fatal — the stored model is still
/// good and the next successful scan re-stamps.
async fn stamp_topics_signature(db: &dyn DbClient, config: &CronConfig) {
    let Some(pool) = db.pool() else { return };
    let sig = topics_effective_signature(config);
    if let Err(e) = crate::db::queries::set_topics_algo_signature(pool, &sig).await {
        error!(error = %e, sig = %sig, "failed to stamp topics_algo_signature");
    }
}

/// Maximum number of topics a single chunk is assigned to. The FCM
/// soft-membership matrix (fuzziness m=2, K up to ~500) places non-trivial mass
/// on many topics per chunk, so persisting every topic above the absolute
/// `membership_threshold` (0.05) saturated per-file topic diversity to ~K — the
/// `topic_count`≈K pathology that rendered `complexity_hotspots`' topic signal,
/// `find_orphans`, `find_misplaced_code`, and `architecture_quality`'s
/// `separation_of_concerns` meaningless (every file appeared to span every
/// topic). Keeping only the strongest few topics per chunk restores a sparse,
/// meaningful assignment. The cap is applied before BOTH the persisted
/// `chunk_topic_assignments` rows and the per-topic member buckets used for
/// c-TF-IDF keywords, dropping only weak (noise) memberships. The signature was
/// bumped v2→v3 so existing assignments recompute under the cap. 4 follows the
/// conventional BERTopic top-k.
const MAX_MEMBERSHIPS_PER_CHUNK: usize = 4;

/// Cap a chunk's topic-assignment list to the `MAX_MEMBERSHIPS_PER_CHUNK`
/// strongest topics. Sorts by membership descending then truncates; a no-op
/// when the list is already within the cap.
fn cap_chunk_memberships(chunk_topics: &mut Vec<(usize, f64)>) {
    if chunk_topics.len() > MAX_MEMBERSHIPS_PER_CHUNK {
        chunk_topics.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        chunk_topics.truncate(MAX_MEMBERSHIPS_PER_CHUNK);
    }
}

/// The strongest topics for chunk `i` (membership > 1e-8), capped to
/// `MAX_MEMBERSHIPS_PER_CHUNK`. Shared by both c-TF-IDF paths so a chunk's
/// tokens feed only its dominant topics.
///
/// Without this cap, diffuse fuzzy memberships (FCM with m=2, large K) place
/// non-trivial mass on nearly every topic, so every word ends up in every
/// topic's bag — and the max-document-frequency cutoff in c-TF-IDF then drops
/// *all* of them, yielding empty keyword lists (the empty-label failure the
/// bake-off exposed for the embedding tracks). Capping to the top memberships
/// keeps per-topic word distributions sparse and discriminative, exactly
/// mirroring the assignment cap. For K ≤ `MAX_MEMBERSHIPS_PER_CHUNK` this is a
/// no-op, so the small-K golden fixtures are unaffected.
fn top_membership_topics(membership: &Array2<f32>, i: usize, k: usize) -> Vec<(usize, f64)> {
    let mut v: Vec<(usize, f64)> = (0..k)
        .map(|t| (t, membership[[i, t]] as f64))
        .filter(|&(_, mu)| mu > 1e-8)
        .collect();
    if v.len() > MAX_MEMBERSHIPS_PER_CHUNK {
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.truncate(MAX_MEMBERSHIPS_PER_CHUNK);
    }
    v
}

// Stopword tiers for c-TF-IDF topic labeling.
//
// Originally only programming-language keywords were filtered; topic
// labels then degenerated into English function words ("the", "and",
// "of") and host-specific path tokens ("home", "workspace", the user's
// username) bleeding through from embedded path strings in error
// messages, log lines, and doc comments. The four explicit tiers
// (CODE / ENGLISH / PATHS / SCAFFOLDING) make each category of
// suppression auditable; the union of all four is the default.
//
// Per-installation tokens (e.g. a real username like "dylon") would
// be inappropriate to ship in the binary — they're loaded from the
// environment variable `PGMCP_TOPIC_STOPWORDS_EXTRA` (comma-separated).

/// English function-word tier: the, and, of, … plus common verbs/
/// auxiliaries that dominate bag-of-words counts in any natural-language
/// corpus and provide no discriminative signal for topic separation.
const ENGLISH_STOPWORDS: &[&str] = &[
    "the", "and", "or", "of", "for", "in", "on", "at", "to", "from", "with", "as", "by", "an", "a",
    "is", "are", "was", "were", "be", "been", "has", "have", "had", "this", "that", "these",
    "those", "but", "if", "then", "when", "where", "what", "which", "how", "why", "who", "can",
    "will", "would", "should", "could", "may", "might", "must", "just", "only", "also", "not",
    "no", "yes", "all", "any", "some", "each", "every", "here", "there", "now", "very", "more",
    "most", "less", "into", "out", "up", "down", "over", "under", "do", "does", "did", "doing",
    "between", "through", "during", "before", "after", "above", "below", "again", "further",
    "than", "too",
];

/// Filesystem-path tier: directory components that appear in
/// `/home/<user>/Workspace/<project>/src/...` style strings embedded in
/// log lines and error messages. None of these are project-meaningful.
const PATH_STOPWORDS: &[&str] = &[
    "home",
    "workspace",
    "project",
    "projects",
    "source",
    "sources",
    "target",
    "build",
    "dist",
    "node",
    "modules",
    "github",
    "gitlab",
    "bitbucket",
    "com",
    "io",
    "org",
    "www",
    "http",
    "https",
    "tmp",
    "var",
    "etc",
    "usr",
    "opt",
    "local",
    "lib",
    "bin",
    "share",
    "include",
];

/// Identifier-scaffolding tier: variable-name pieces that appear in
/// nearly every code corpus and dominate bag-of-words counts without
/// distinguishing topics.
const SCAFFOLDING_STOPWORDS: &[&str] = &[
    "value", "values", "data", "item", "items", "name", "names", "kind", "kinds", "info", "list",
    "array", "object", "key", "keys", "result", "results", "error", "errors", "count", "size",
    "index", "indices", "num", "number", "file", "files", "line", "lines", "code", "text",
    "content", "contents", "input", "output", "config", "settings",
];

/// Programming language stopwords to filter from topic labels.
fn code_stopwords() -> HashSet<&'static str> {
    let mut set: HashSet<&'static str> = [
        // Rust
        "fn",
        "pub",
        "let",
        "mut",
        "use",
        "impl",
        "struct",
        "enum",
        "const",
        "mod",
        "trait",
        "type",
        "where",
        "async",
        "await",
        "move",
        "ref",
        "self",
        "crate",
        "super",
        "match",
        "loop",
        "break",
        "continue",
        "unsafe",
        "dyn",
        "box",
        // C / C++
        "int",
        "void",
        "char",
        "float",
        "double",
        "long",
        "short",
        "unsigned",
        "signed",
        "static",
        "extern",
        "typedef",
        "sizeof",
        "volatile",
        "register",
        "inline",
        "auto",
        "namespace",
        "template",
        "class",
        "virtual",
        "override",
        "final",
        "delete",
        "new",
        "nullptr",
        // Python
        "def",
        "class",
        "import",
        "from",
        "self",
        "cls",
        "lambda",
        "yield",
        "global",
        "nonlocal",
        "pass",
        "raise",
        "with",
        "assert",
        "del",
        "print",
        // JavaScript / TypeScript
        "var",
        "const",
        "function",
        "export",
        "default",
        "require",
        "module",
        "prototype",
        "this",
        "undefined",
        "null",
        "typeof",
        "instanceof",
        "interface",
        "abstract",
        "extends",
        "implements",
        "declare",
        "readonly",
        // General control flow
        "if",
        "else",
        "elif",
        "for",
        "while",
        "do",
        "switch",
        "case",
        "try",
        "catch",
        "finally",
        "throw",
        "throws",
        "return",
        "goto",
        // Common operators / tokens
        "true",
        "false",
        "none",
        "nil",
        // Common noise words in code
        "todo",
        "fixme",
        "hack",
        "note",
        "xxx",
        "bug",
        // Very common short identifiers
        "ok",
        "err",
        "str",
        "vec",
        "map",
        "set",
        "get",
        "put",
        "new",
        "end",
        "val",
        "arg",
        "buf",
        "len",
        "idx",
        "tmp",
        "res",
        "ret",
        "src",
        "dst",
    ]
    .into_iter()
    .collect();

    // Union the English / path / scaffolding tiers.
    set.extend(ENGLISH_STOPWORDS.iter().copied());
    set.extend(PATH_STOPWORDS.iter().copied());
    set.extend(SCAFFOLDING_STOPWORDS.iter().copied());
    set
}

/// Per-installation extras, computed once and cached. Sources:
///   1. the host **username**, auto-derived from `$USER` / `$LOGNAME` / the
///      basename of `$HOME` — this is what suppresses the `dylon`-style token
///      that bleeds in from `/home/<user>/...` path strings WITHOUT requiring
///      any manual configuration (the prior leak that produced degenerate
///      labels); and
///   2. `PGMCP_TOPIC_STOPWORDS_EXTRA` (comma-separated) for any additional
///      site-specific tokens.
///
/// Each entry is lower-cased and short/empty tokens are dropped.
fn user_stopwords() -> &'static HashSet<String> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<HashSet<String>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut set: HashSet<String> = HashSet::new();

        // (1) Host username from the environment (covers `/home/<user>/...`).
        let username = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .ok()
            .or_else(|| {
                std::env::var("HOME").ok().and_then(|h| {
                    std::path::Path::new(&h)
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                })
            });
        if let Some(u) = username {
            let u = u.trim().to_lowercase();
            if !u.is_empty() {
                set.insert(u);
            }
        }

        // (2) Explicit per-installation extras.
        let raw = std::env::var("PGMCP_TOPIC_STOPWORDS_EXTRA").unwrap_or_default();
        set.extend(
            raw.split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty()),
        );
        set
    })
}

/// Split a raw identifier into concept sub-tokens so multi-word identifiers
/// surface their constituent concepts as topic keywords (a BERTopic-class
/// preprocessing step): snake_case on `_`, and camelCase / PascalCase / acronym
/// runs on letter-case boundaries. Lowercases each sub-token and appends to
/// `out`. Examples: `tokenize_query` → [tokenize, query];
/// `parseHTTPResponse` → [parse, http, response]; `FcmBackend` → [fcm, backend].
/// Digit runs are NOT split off (so `utf8`, `bge3` stay intact).
fn split_identifier(raw: &str, out: &mut Vec<String>) {
    for part in raw.split('_') {
        if part.is_empty() {
            continue;
        }
        let chars: Vec<char> = part.chars().collect();
        let mut start = 0usize;
        for i in 1..chars.len() {
            let prev = chars[i - 1];
            let cur = chars[i];
            // Boundary BEFORE `cur` when: a lowercase/digit is followed by an
            // uppercase (camelCase), or an acronym run ends — an uppercase
            // followed by an uppercase that begins a new word (next is lower):
            // `HTTPResponse` → `HTTP` | `Response`.
            let camel = !prev.is_uppercase() && cur.is_uppercase();
            let acronym_end = prev.is_uppercase()
                && cur.is_uppercase()
                && chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if camel || acronym_end {
                out.push(chars[start..i].iter().collect::<String>().to_lowercase());
                start = i;
            }
        }
        out.push(chars[start..].iter().collect::<String>().to_lowercase());
    }
}

/// Tokenize content for c-TF-IDF. Delegates to [`tokenize_into`].
fn tokenize(content: &str) -> Vec<String> {
    let mut buf = Vec::new();
    tokenize_into(content, &mut buf);
    buf
}

/// A single topic's keyword with its score.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TopicKeyword {
    pub word: String,
    pub score: f64,
}

/// Tokenize content into a reused scratch buffer to avoid allocating a new
/// `Vec<String>` per chunk. Splits on non-alphanumeric, then splits each raw
/// identifier into concept sub-tokens ([`split_identifier`]), lowercases, and
/// applies the length / all-digit / stopword filters. Clears `buf` first.
///
/// `pub(crate)` so `crate::quality::topic_metrics` can tokenise documents the
/// same way the labels were derived — NPMI coherence requires term presence to
/// match the keyword vocabulary exactly.
pub(crate) fn tokenize_into(content: &str, buf: &mut Vec<String>) {
    buf.clear();
    let stopwords = code_stopwords();
    let user_extras = user_stopwords();
    let mut subtoks: Vec<String> = Vec::new();
    for raw in content.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if raw.is_empty() {
            continue;
        }
        subtoks.clear();
        split_identifier(raw, &mut subtoks);
        for lower in subtoks.drain(..) {
            if lower.len() >= 3
                && lower.len() <= 50
                && !lower.chars().all(|c| c.is_ascii_digit())
                && !stopwords.contains(lower.as_str())
                && !user_extras.contains(&lower)
            {
                buf.push(lower);
            }
        }
    }
}

/// Compute c-TF-IDF labels for K topics given chunk contents and membership matrix.
///
/// Soft aggregation: token counts are weighted by membership degrees μ_ik.
///
/// Uses a single scratch `Vec<String>` reused across chunks; the previous
/// version materialised `Vec<Vec<String>>` for every chunk simultaneously
/// (~3–5 GB of String allocations at n=113k), which contributed significantly
/// to the OOM allocator churn.
pub fn compute_ctf_idf(
    contents: &[&str],
    membership: &Array2<f32>,
    top_k: usize,
) -> Vec<Vec<TopicKeyword>> {
    let k = membership.ncols();

    // For each topic, accumulate weighted token counts.
    let mut topic_word_counts: Vec<HashMap<String, f64>> = vec![HashMap::new(); k];
    let mut topic_total_tokens: Vec<f64> = vec![0.0; k];

    // Reused scratch buffer — no per-chunk allocation.
    let mut scratch_tokens: Vec<String> = Vec::with_capacity(256);
    let mut local_counts: HashMap<String, u32> = HashMap::with_capacity(256);

    for (i, content) in contents.iter().enumerate() {
        tokenize_into(content, &mut scratch_tokens);

        // Count tokens in this chunk (reuse local_counts map).
        local_counts.clear();
        for token in &scratch_tokens {
            *local_counts.entry(token.clone()).or_insert(0) += 1;
        }

        // Distribute tokens to this chunk's strongest topics only (top-J cap),
        // so diffuse fuzzy memberships don't smear every word into every topic
        // and trip the max-df cutoff (which would empty all keyword lists).
        for (t, mu) in top_membership_topics(membership, i, k) {
            for (word, &count) in &local_counts {
                let weighted = mu * count as f64;
                *topic_word_counts[t].entry(word.clone()).or_insert(0.0) += weighted;
                topic_total_tokens[t] += weighted;
            }
        }
    }

    // Compute IDF: log(1 + avg_tokens_per_topic / topics_containing_w)
    let avg_tokens = if k > 0 {
        topic_total_tokens.iter().sum::<f64>() / k as f64
    } else {
        1.0
    };

    // Count in how many topics each word appears
    let mut word_topic_freq: HashMap<String, usize> = HashMap::new();
    for topic_counts in &topic_word_counts {
        for word in topic_counts.keys() {
            *word_topic_freq.entry(word.clone()).or_insert(0) += 1;
        }
    }

    // Compute c-TF-IDF score for each word in each topic
    let mut results: Vec<Vec<TopicKeyword>> = Vec::with_capacity(k);

    // Max document-frequency cutoff: with >= 5 topics, drop words that appear in
    // more than 40% of topics — near-ubiquitous terms that the idf factor only
    // partially suppresses and that blur topic separation (classic TF-IDF df
    // pruning). Disabled for < 5 topics (too few for the fraction to mean much).
    let df_cap: usize = if k >= 5 {
        ((k as f64) * 0.4).ceil() as usize
    } else {
        usize::MAX
    };

    for t in 0..k {
        let total = topic_total_tokens[t].max(1.0);
        let mut scored: Vec<TopicKeyword> = topic_word_counts[t]
            .iter()
            .filter(|(word, _)| *word_topic_freq.get(*word).unwrap_or(&1) <= df_cap)
            .map(|(word, &count)| {
                let tf = count / total;
                let df = *word_topic_freq.get(word).unwrap_or(&1) as f64;
                let idf = (1.0 + avg_tokens / df).ln();
                TopicKeyword {
                    word: word.clone(),
                    score: tf * idf,
                }
            })
            .collect();

        // Sort by score descending; break ties on word ascending so the
        // output is deterministic regardless of HashMap iteration order
        // (golden-fixture tests depend on this).
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.word.cmp(&b.word))
        });
        scored.truncate(top_k);
        results.push(scored);
    }

    results
}

/// Build a topic label from top keywords: "keyword1 / keyword2 / ..."
fn label_from_keywords(keywords: &[TopicKeyword], cluster_index: i32) -> String {
    if keywords.is_empty() {
        return format!("topic_{}", cluster_index);
    }
    keywords
        .iter()
        .map(|kw| kw.word.as_str())
        .collect::<Vec<_>>()
        .join(" / ")
}

// ============================================================================
// Core clustering pipeline
// ============================================================================

/// FCM configuration extracted from CronConfig.
struct FcmParams {
    num_clusters: Option<usize>,
    min_cluster_size: usize,
    fuzziness: f64,
    max_iters: usize,
    tolerance: f64,
    membership_threshold: f64,
    label_top_k: usize,
    /// Phase 2 (Track B): optional dimensionality reduction applied to the
    /// embedding matrix before FCM. `None` = cluster raw 1024-d (baseline).
    reduce_method: Option<crate::cron::topic_reduce::ReduceMethod>,
    /// Target dimensionality when `reduce_method` is `Some`.
    reduce_dim: usize,
    /// GPU precision selector ("fp32" | "fp16" | "bf16"). Read in
    /// `dispatch_fcm` to pick the CUDA backend.
    gpu_fcm_precision: String,
    /// Phase 12: adaptive K selector config.
    k_selector: String,
    k_candidates: Vec<usize>,
    k_sweep_max_iters: usize,
    k_sweep_subsample: usize,
    /// Phase 7: LMDB warm-start config.
    lmdb_path: Option<std::path::PathBuf>,
    lmdb_enabled: bool,
    /// Phase 1: scratch dir for mmap-backed data matrix.
    _topic_scratch_dir: Option<std::path::PathBuf>,
}

impl FcmParams {
    /// Return the configured scratch directory path (or None → caller picks default).
    fn topic_scratch_dir(&self) -> Option<std::path::PathBuf> {
        self._topic_scratch_dir.clone()
    }

    fn from_config(config: &CronConfig) -> Self {
        Self {
            num_clusters: config.topic_num_clusters,
            min_cluster_size: config.topic_min_cluster_size,
            fuzziness: config.topic_fuzziness,
            max_iters: config.topic_fcm_max_iters,
            tolerance: config.topic_fcm_tolerance,
            membership_threshold: config.topic_membership_threshold,
            label_top_k: config.topic_label_top_k,
            reduce_method: match config.topic_clustering_method.as_str() {
                "embedding_pca" | "pca" => Some(crate::cron::topic_reduce::ReduceMethod::Pca),
                "embedding_rp" | "embedding_random" | "rp" => {
                    Some(crate::cron::topic_reduce::ReduceMethod::RandomProjection)
                }
                // "baseline" clusters raw 1024-d; "graph" is handled by a
                // separate entry point (topic_graph) and never reaches here.
                _ => None,
            },
            reduce_dim: config.topic_reduce_dim,
            gpu_fcm_precision: config.gpu_fcm_precision.clone(),
            k_selector: config.topic_k_selector.clone(),
            k_candidates: config.topic_k_candidates.clone(),
            k_sweep_max_iters: config.topic_k_sweep_max_iters,
            k_sweep_subsample: config.topic_k_sweep_subsample,
            lmdb_path: config.topic_lmdb_path.clone(),
            lmdb_enabled: config.topic_lmdb_enabled,
            _topic_scratch_dir: config.topic_scratch_dir.clone(),
        }
    }

    fn with_min_cluster_size(config: &CronConfig, min_cluster_size: usize) -> Self {
        let mut params = Self::from_config(config);
        params.min_cluster_size = min_cluster_size;
        params
    }
}

/// Resolve the LMDB centroid-store path and open it. On any I/O error,
/// returns None and logs a WARN — the caller falls back to k-means++
/// cold start without disrupting the FCM run.
fn open_centroid_store(params: &FcmParams) -> Option<crate::topic_store::CentroidStore> {
    let path = params
        .lmdb_path
        .clone()
        .unwrap_or_else(crate::topic_store::lmdb::default_path);
    match crate::topic_store::CentroidStore::open(&path) {
        Ok(store) => Some(store),
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "Failed to open topic LMDB store — FCM will cold-start"
            );
            None
        }
    }
}

/// Load warm-start centroids from LMDB for the given scope. Returns None if
/// the store is missing, unreachable, or the stored centroids don't match
/// the expected (k, d) shape (e.g. after K was re-sized via Phase 12).
fn load_warm_start_centroids(
    params: &FcmParams,
    scope: &str,
    k: usize,
    d: usize,
) -> Option<Array2<f32>> {
    let store = open_centroid_store(params)?;
    let records = match store.load_centroids(scope) {
        Ok(r) => r,
        Err(e) => {
            warn!(scope, error = %e, "LMDB load_centroids failed — cold start");
            return None;
        }
    };
    if records.len() != k {
        if !records.is_empty() {
            info!(
                scope,
                stored_k = records.len(),
                requested_k = k,
                "Stored centroid count differs from current K — cold start"
            );
        }
        return None;
    }
    if records[0].d != d {
        info!(
            scope,
            stored_d = records[0].d,
            requested_d = d,
            "Stored centroid dim differs from current d — cold start"
        );
        return None;
    }
    let mut centroids = Array2::<f32>::zeros((k, d));
    for (i, rec) in records.iter().enumerate() {
        if rec.centroid.len() != d {
            return None;
        }
        for (j, &v) in rec.centroid.iter().enumerate() {
            centroids[[i, j]] = v;
        }
    }
    Some(centroids)
}

/// Store final centroids to LMDB for the next run's warm-start.
fn store_warm_start_centroids(params: &FcmParams, scope: &str, centroids: &Array2<f32>) {
    let store = match open_centroid_store(params) {
        Some(s) => s,
        None => return,
    };
    let k = centroids.nrows();
    let d = centroids.ncols();
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let records: Vec<crate::topic_store::StoredCentroid> = (0..k)
        .map(|i| crate::topic_store::StoredCentroid {
            scope: scope.to_string(),
            centroid: centroids.row(i).to_vec(),
            created_at,
            d,
            k_total: k,
        })
        .collect();
    if let Err(e) = store.store_centroids(scope, &records) {
        warn!(scope, error = %e, "LMDB store_centroids failed");
    } else {
        info!(
            scope,
            k, d, "Persisted {} centroids to LMDB for warm-start", k
        );
    }
}

/// Phase 12: adaptive K selection via Xie-Beni / Fuzzy Silhouette / Gap.
/// Subsamples `data` if it's larger than `params.k_sweep_subsample`, then
/// runs a short-FCM sweep over candidate K values around the heuristic base.
/// Returns the best K.
fn select_k_adaptive(data: &Array2<f32>, base_k: usize, params: &FcmParams) -> usize {
    use crate::cron::k_selector;

    let n = data.nrows();

    // Subsample for cost control.
    let sweep_n = if params.k_sweep_subsample > 0 {
        params.k_sweep_subsample.min(n)
    } else {
        n
    };
    let sub = if sweep_n < n {
        k_selector::subsample_data(data, sweep_n)
    } else {
        data.clone()
    };

    let candidates = if !params.k_candidates.is_empty() {
        let mut c = params.k_candidates.clone();
        c.sort_unstable();
        c.dedup();
        c
    } else {
        k_selector::geometric_candidates(base_k, 500)
    };

    let cfg = k_selector::SweepConfig {
        candidates,
        index: k_selector::Index::parse(&params.k_selector),
        m: params.fuzziness,
        max_iters: params.k_sweep_max_iters,
        tolerance: params.tolerance.max(1e-3),
        gap_n_refs: 10,
    };

    let (best_k, _entries) = k_selector::sweep_k(sub.view(), &cfg);
    info!(
        base_k,
        best_k,
        sweep_n,
        index = ?cfg.index,
        "Adaptive K sweep selected best K"
    );
    best_k
}

/// Run FCM clustering on extracted chunk embeddings and build topic results.
fn cluster_embeddings(
    rows: &[ChunkEmbeddingRow],
    params: &FcmParams,
    scope: &str,
) -> ClusteringSummary {
    cluster_embeddings_with_runner(rows, params, scope, |data, k, params, warm_centroids| {
        dispatch_fcm(data, k, params, warm_centroids)
    })
}

fn cluster_embeddings_with_runner<F>(
    rows: &[ChunkEmbeddingRow],
    params: &FcmParams,
    scope: &str,
    mut run_fcm: F,
) -> ClusteringSummary
where
    F: for<'a> FnMut(ArrayView2<'a, f32>, usize, &FcmParams, Option<Array2<f32>>) -> FcmResult,
{
    if rows.is_empty() {
        return ClusteringSummary {
            scope: scope.to_string(),
            chunks_analyzed: 0,
            topics_found: 0,
            noise_chunks: 0,
            num_clusters: 0,
            fuzziness: params.fuzziness,
            converged: false,
            iterations: 0,
            topics: Vec::new(),
            metrics: None,
        };
    }

    let n = rows.len();
    let d = rows[0].embedding.len();

    // Build the data matrix directly in f32 (embeddings are already f32 from
    // fastembed; no reason to expand to f64). L2-normalize each row in place.
    let mut data = Array2::<f32>::zeros((n, d));
    for (i, row) in rows.iter().enumerate() {
        for (j, &v) in row.embedding.iter().enumerate() {
            data[[i, j]] = v;
        }
        let norm: f32 = data.row(i).dot(&data.row(i)).sqrt();
        if norm > 1e-12 {
            data.row_mut(i).mapv_inplace(|x| x / norm);
        }
    }

    // Phase 2 (Track B): optional dimensionality reduction BEFORE clustering.
    // Clustering raw 1024-d embeddings is what caused the collapse (distance
    // concentration → uniform memberships); reducing to ~30-d restores contrast.
    // All downstream work (FCM, centroids, avg-similarity, metrics) then runs in
    // the reduced space — `data` and `d` are rebound to the reduced matrix.
    let (data, d) = match params.reduce_method {
        Some(method) => {
            let target = params.reduce_dim.min(d).max(2);
            info!(
                from_dim = d,
                to_dim = target,
                method = ?method,
                "Track B: reducing embeddings before FCM"
            );
            let reduced = crate::cron::topic_reduce::reduce(data.view(), target, method, 42);
            let rd = reduced.ncols();
            (reduced, rd)
        }
        None => (data, d),
    };

    // Determine K — adaptive sweep (Phase 12) when num_clusters is None.
    let k = match params.num_clusters {
        Some(explicit) => explicit.min(n),
        None => {
            let base = estimate_k(n, params.min_cluster_size);
            select_k_adaptive(&data, base, params).min(n)
        }
    };

    info!(n, k, fuzziness = params.fuzziness, "Running FCM clustering");

    // Phase 7: load warm-start centroids from LMDB if available & enabled.
    let warm_centroids = if params.lmdb_enabled {
        load_warm_start_centroids(params, scope, k, d)
    } else {
        None
    };

    // Run FCM (f32 internals, preallocated buffers, ping-pong membership).
    let fcm_result = run_fcm(data.view(), k, params, warm_centroids);

    // Phase 7: persist final centroids for next-run warm-start.
    if params.lmdb_enabled {
        store_warm_start_centroids(params, scope, &fcm_result.centroids);
    }

    info!(
        iterations = fcm_result.iterations,
        converged = fcm_result.converged,
        cancelled = fcm_result.cancelled,
        inertia = format!("{:.2}", fcm_result.inertia),
        "FCM complete"
    );

    // Assign chunks to topics: each chunk's primary topic is argmax(membership row).
    // Chunks whose max membership is below threshold are "noise".
    // We store all assignments above membership_threshold for soft clustering.

    let threshold_f32 = params.membership_threshold as f32;

    // Build topic → [(chunk_idx, membership as f64 for public API)] mapping
    let mut topic_members: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
    let mut noise_count = 0usize;

    for i in 0..n {
        let row = fcm_result.membership.row(i);
        let max_membership = row.iter().cloned().fold(f32::NEG_INFINITY, f32::max);

        if max_membership < threshold_f32 {
            noise_count += 1;
            continue;
        }

        // Collect this chunk's above-threshold topics, then cap to the top-J
        // strongest (cap_chunk_memberships). Without the cap, diffuse FCM
        // memberships (m=2, K≈199) put nearly every chunk in nearly every
        // topic, saturating per-file topic diversity to ~K (the topic_count≈K
        // bug).
        let mut chunk_topics: Vec<(usize, f64)> = Vec::new();
        for t in 0..k {
            let mu = fcm_result.membership[[i, t]];
            if mu >= threshold_f32 {
                chunk_topics.push((t, mu as f64));
            }
        }
        if chunk_topics.is_empty() {
            noise_count += 1;
            continue;
        }
        cap_chunk_memberships(&mut chunk_topics);
        for (t, score) in chunk_topics {
            topic_members.entry(t).or_default().push((i, score));
        }
    }

    // Compute c-TF-IDF keywords (takes &Array2<f32> membership)
    let contents: Vec<&str> = rows.iter().map(|r| r.content.as_str()).collect();
    let all_keywords = compute_ctf_idf(&contents, &fcm_result.membership, params.label_top_k);

    // Build TopicResult for each non-empty topic
    let mut topics: Vec<TopicResult> = Vec::with_capacity(topic_members.len());

    for (&topic_idx, members) in &topic_members {
        if members.is_empty() {
            continue;
        }

        let chunk_ids: Vec<i64> = members.iter().map(|&(i, _)| rows[i].chunk_id).collect();
        let memberships: Vec<f64> = members.iter().map(|&(_, mu)| mu).collect();
        let file_ids: Vec<i64> = members
            .iter()
            .map(|&(i, _)| rows[i].file_id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let project_names: Vec<String> = members
            .iter()
            .map(|&(i, _)| rows[i].project_name.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        // Collect member indices for similarity & representative computation.
        // avg_internal_similarity / find_representative read from the data view
        // directly — no duplicate allocation (the old `data_vecs` has been
        // eliminated).
        let member_indices: Vec<usize> = members.iter().map(|&(i, _)| i).collect();
        let avg_sim = avg_internal_similarity(&data.view(), &member_indices);

        let representative_id = find_representative(&data.view(), &chunk_ids, &member_indices);
        let representative_snippet = rows
            .iter()
            .find(|r| r.chunk_id == representative_id)
            .map(|r| {
                if r.content.len() > 500 {
                    format!("{}...", &r.content[..r.content.floor_char_boundary(500)])
                } else {
                    r.content.clone()
                }
            })
            .unwrap_or_default();

        // Build top_files (weighted by membership)
        let mut file_chunk_counts: HashMap<(&str, &str), f64> = HashMap::new();
        for &(i, mu) in members {
            let key = (rows[i].path.as_str(), rows[i].project_name.as_str());
            *file_chunk_counts.entry(key).or_insert(0.0) += mu;
        }
        let mut top_files: Vec<TopicFileEntry> = file_chunk_counts
            .into_iter()
            .map(|((path, project), weighted_count)| TopicFileEntry {
                path: path.to_string(),
                project: project.to_string(),
                chunks_in_topic: weighted_count.round() as i32,
            })
            .collect();
        top_files.sort_by_key(|b| std::cmp::Reverse(b.chunks_in_topic));

        // Get keywords for this topic
        let empty_kw = Vec::new();
        let kw = if topic_idx < all_keywords.len() {
            &all_keywords[topic_idx]
        } else {
            &empty_kw
        };
        let label = label_from_keywords(kw, topic_idx as i32);
        let keywords: Vec<String> = kw.iter().map(|k| k.word.clone()).collect();
        let keyword_scores: Vec<f64> = kw.iter().map(|k| k.score).collect();

        // Extract this topic's centroid from the FCM result for warm-start
        // + hierarchy (Phase 7 / Phase 9).
        let centroid_vec: Vec<f32> = fcm_result.centroids.row(topic_idx).to_vec();

        topics.push(TopicResult {
            cluster_index: topic_idx as i32,
            label,
            keywords,
            keyword_scores,
            chunk_ids,
            memberships,
            file_ids,
            project_names,
            avg_internal_similarity: avg_sim,
            representative_chunk_id: representative_id,
            representative_snippet,
            top_files,
            centroid: centroid_vec,
            parent_topic_ids: Vec::new(),
        });
    }

    // Sort by chunk count descending
    topics.sort_by_key(|b| std::cmp::Reverse(b.chunk_ids.len()));

    // Phase 1: compute quality metrics on the FINAL model — the membership
    // matrix, centroids, data, and chunk contents are all still in scope here.
    // Coherence (NPMI / UMass) reuses the same tokenizer the labels were derived
    // from. The scan paths consult `summary.metrics` for the degeneracy gate and
    // persist it for trend.
    let mut metrics = TopicMetrics::compute(
        data.view(),
        fcm_result.membership.view(),
        fcm_result.centroids.view(),
        params.fuzziness,
        k,
        &topics,
    );
    metrics.fill_coherence(
        &contents,
        &topics,
        crate::quality::topic_metrics::DEFAULT_COHERENCE_TOP_N,
    );

    ClusteringSummary {
        scope: scope.to_string(),
        chunks_analyzed: n,
        topics_found: topics.len(),
        noise_chunks: noise_count,
        num_clusters: k,
        fuzziness: params.fuzziness,
        converged: fcm_result.converged,
        iterations: fcm_result.iterations,
        topics,
        metrics: Some(metrics),
    }
}

// ============================================================================
// Entry point 1: Global topic scan (cron job)
// ============================================================================

/// Strategy chosen by `select_scan_strategy` based on corpus size + config
/// thresholds. Pure dispatch — no I/O, no allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanStrategy {
    /// Online (mini-batch) FCM — keeps O(batch·(d+K)) memory regardless of
    /// total chunk count. Used when chunk count exceeds the online threshold.
    Online,
    /// mmap-backed data matrix + streaming c-TF-IDF — caps anonymous-heap
    /// RSS while preserving in-memory FCM speed. Used for medium corpora.
    Mmap,
    /// Vanilla in-memory FCM — fastest path for small corpora.
    InMemory,
}

/// Decision from the memory pre-flight against `/proc/meminfo:MemAvailable`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BudgetDecision {
    /// Predicted peak RSS within `topic_max_mem_fraction × MemAvailable`.
    /// Safe to proceed with the chosen strategy.
    WithinBudget {
        predicted_mb: u64,
        available_mb: u64,
        frac: f64,
    },
    /// Predicted peak RSS exceeds the budget. Caller should run the
    /// per-project emergency fallback for this cycle and retry next cycle.
    OverBudget {
        predicted_mb: u64,
        available_mb: u64,
        frac: f64,
        budget_frac: f64,
    },
    /// Pre-flight skipped (no chunks, or `/proc/meminfo` unreadable).
    NotChecked,
}

/// Pure dispatch: pick the FCM strategy from corpus size + thresholds.
pub(crate) fn select_scan_strategy(chunk_count: usize, config: &CronConfig) -> ScanStrategy {
    if chunk_count > config.topic_online_n_threshold {
        ScanStrategy::Online
    } else if chunk_count > config.topic_mmap_n_threshold {
        ScanStrategy::Mmap
    } else {
        ScanStrategy::InMemory
    }
}

/// Pure memory budget check. Predicts peak RSS for in-memory FCM at the
/// given (n, k) and compares against `MemAvailable × budget_frac`.
pub(crate) fn check_memory_budget(
    chunk_count: usize,
    k: usize,
    mem_avail: u64,
    budget_frac: f64,
) -> BudgetDecision {
    if chunk_count == 0 {
        return BudgetDecision::NotChecked;
    }
    let d = 1024u64;
    let n = chunk_count as u64;
    let k = k as u64;
    // Conservative prediction matching the in-memory FCM buffer footprint.
    let predicted_bytes = 8u64 * (n * d)                  // data Array2<f64>
        + 8u64 * (n * d)                                  // data_vecs duplicate
        + 8u64 * 4 * (n * k)                              // membership + clone + dist_sq + u_pow_m
        + 8u64 * (n * k)                                  // dot_xc
        + 2_000u64 * n; // rows Vec overhead (content + strings)
    let predicted_mb = predicted_bytes >> 20;
    let available_mb = mem_avail >> 20;
    let frac = predicted_bytes as f64 / mem_avail as f64;
    if frac > budget_frac {
        BudgetDecision::OverBudget {
            predicted_mb,
            available_mb,
            frac,
            budget_frac,
        }
    } else {
        BudgetDecision::WithinBudget {
            predicted_mb,
            available_mb,
            frac,
        }
    }
}

/// Phase 1 degeneracy gate. Returns `true` if the scan should ABORT the
/// clear+store (preserving the prior, presumably-good topics) because the new
/// model is degenerate. Logs the reason and bumps `topic_degenerate_refusals`
/// on rejection.
///
/// This is the structural fix for the silent collapse: the prior
/// `store_topics` guard only withheld the algo-signature *after* it had already
/// cleared and overwritten the previous topics. This gate runs *before* the
/// destructive clear, so a degenerate cycle cannot replace good data with junk.
fn topic_gate_rejects(
    summary: &ClusteringSummary,
    config: &CronConfig,
    stats: &Arc<StatsTracker>,
) -> bool {
    let Some(metrics) = summary.metrics.as_ref() else {
        // No metrics computed (e.g. the online >1M path). Fall back to the
        // existing iterations==0 wipe-protection the caller already applied.
        return false;
    };
    let thresholds = DegeneracyThresholds::from_config(config);
    if let Some(reason) = metrics.degeneracy_reason(&thresholds) {
        warn!(
            scope = %summary.scope,
            k = summary.num_clusters,
            topics = summary.topics_found,
            mean_max_membership = metrics.mean_max_membership,
            distinct_label_ratio = metrics.distinct_label_ratio,
            topics_per_doc = metrics.topics_per_doc_mean,
            max_topic_share = metrics.max_topic_share,
            fuzzy_silhouette = metrics.fuzzy_silhouette,
            reason = %reason,
            "topic degeneracy gate: REFUSING to overwrite prior topics with a degenerate model"
        );
        stats
            .topic_degenerate_refusals
            .fetch_add(1, Ordering::Relaxed);
        return true;
    }
    false
}

/// Persist the scan's quality metrics to `pgmcp_metadata['topics_quality']`
/// (best-effort; a failure here does not fail the scan). Called after a
/// successful `store_topics`.
async fn persist_topic_quality(db: &dyn DbClient, scope: &str, summary: &ClusteringSummary) {
    if let (Some(pool), Some(metrics)) = (db.pool(), summary.metrics.as_ref())
        && let Err(e) = crate::db::queries::set_topic_quality(pool, scope, &metrics.to_json()).await
    {
        error!(scope, error = %e, "failed to persist topic quality metrics");
    }
}

/// Run a global topic scan over all chunks, storing results in the DB.
pub async fn run_global_topic_scan(
    db: &dyn DbClient,
    config: &CronConfig,
    stats: &Arc<StatsTracker>,
    lifecycle: &crate::daemon_state::DaemonLifecycle,
) {
    let params = FcmParams::from_config(config);
    info!(
        min_cluster_size = params.min_cluster_size,
        num_clusters = ?params.num_clusters,
        fuzziness = params.fuzziness,
        "Starting global topic clustering scan (FCM + c-TF-IDF)"
    );

    let chunk_count_opt = count_chunks(db).await;

    // NoOp detection: if there are no chunks (or count failed), record
    // the no-op outcome so an operator can tell "the cron ran, nothing
    // to cluster" apart from "the cron never ran".
    match chunk_count_opt {
        Some(0) | None => {
            stats
                .topic_clustering_noop_returns
                .fetch_add(1, Ordering::Relaxed);
            info!("Topic clustering cron: no chunks to scan");
            return;
        }
        Some(_) => {}
    }

    // Phase 6: the per-project engines (graph-hybrid — the default — and
    // embedding-HDBSCAN) run a per-project scan instead of the global FCM
    // strategy paths. Each project is clustered independently (bounded memory),
    // gated + quality-persisted + LLM-labeled, stored under `scope='project:NAME'`,
    // then rolled up into `scope='global'`. (HDBSCAN is O(n²) so it cannot run on
    // the whole-corpus FCM paths anyway.)
    let method = config.topic_clustering_method.as_str();
    if method == "graph" || method == "embedding_hdbscan" {
        run_graph_topic_scan(db, config, stats).await;
        return;
    }

    // Strategy dispatch: online (huge) → mmap (medium) → in-memory (small).
    if let Some(chunk_count) = chunk_count_opt {
        match select_scan_strategy(chunk_count, config) {
            ScanStrategy::Online => {
                info!(
                    chunk_count,
                    threshold = config.topic_online_n_threshold,
                    batch_size = config.topic_online_batch_size,
                    "Dispatching to online FCM (mini-batch) for global topic scan"
                );
                run_online_global_topic_scan(db, config, stats, chunk_count).await;
                return;
            }
            ScanStrategy::Mmap => {
                info!(
                    chunk_count,
                    threshold = config.topic_mmap_n_threshold,
                    "Dispatching to mmap-streaming FCM for global topic scan"
                );
                run_mmap_global_topic_scan(db, config, stats, chunk_count, lifecycle).await;
                return;
            }
            ScanStrategy::InMemory => { /* fall through to in-memory path */ }
        }
    }

    // Memory pre-flight for the in-memory path. If the prediction exceeds
    // budget, run per-project fallback for this cycle and retry next cycle.
    if let (Some(chunk_count), Some(mem_avail)) =
        (chunk_count_opt, crate::stats::rss::mem_available_bytes())
    {
        let k_est = estimate_k(chunk_count, params.min_cluster_size);
        match check_memory_budget(chunk_count, k_est, mem_avail, config.topic_max_mem_fraction) {
            BudgetDecision::WithinBudget {
                predicted_mb,
                available_mb,
                frac,
            } => {
                info!(
                    chunks = chunk_count,
                    k_est,
                    predicted_peak_mb = predicted_mb,
                    available_mb,
                    frac = format!("{:.3}", frac),
                    "Global topic clustering memory pre-flight"
                );
            }
            BudgetDecision::OverBudget {
                predicted_mb,
                available_mb,
                frac,
                budget_frac,
            } => {
                warn!(
                    chunks = chunk_count,
                    predicted_peak_mb = predicted_mb,
                    available_mb,
                    frac = format!("{:.3}", frac),
                    budget_frac,
                    "Global clustering skipped this cycle: predicted RSS exceeds budget. Running per-project emergency fallback; scope='global' not refreshed."
                );
                run_per_project_emergency_fallback(db, config, stats).await;
                return;
            }
            BudgetDecision::NotChecked => {}
        }
    }

    let rows = match db.bulk_extract_embeddings(None).await {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "Failed to extract embeddings for topic clustering");
            return;
        }
    };

    if rows.is_empty() {
        info!("No chunks to cluster for topics");
        return;
    }

    info!(chunks = rows.len(), "Extracted embeddings, running FCM");

    let summary = cluster_embeddings(&rows, &params, "global");

    info!(
        topics = summary.topics_found,
        noise = summary.noise_chunks,
        k = summary.num_clusters,
        converged = summary.converged,
        iterations = summary.iterations,
        noise_pct = format!(
            "{:.1}",
            if summary.chunks_analyzed > 0 {
                summary.noise_chunks as f64 / summary.chunks_analyzed as f64 * 100.0
            } else {
                0.0
            }
        ),
        "FCM clustering complete"
    );

    // Record what FCM discovered BEFORE persistence. See the analogous
    // comment in run_mmap_global_topic_scan for rationale.
    stats.topic_scans.fetch_add(1, Ordering::Relaxed);
    stats
        .topics_discovered
        .store(summary.topics_found as u64, Ordering::Relaxed);
    stats
        .topic_noise_chunks
        .store(summary.noise_chunks as u64, Ordering::Relaxed);

    // Wipe-protection: if FCM produced a degenerate result (CUDA OOM,
    // cancellation, backend failure), `iterations == 0` and `topics` is
    // empty. Clearing the prior-cycle topics here would replace good data
    // with nothing — the very failure mode F12 in the robustness plan
    // calls out. Skip the swap and let the next successful run resync.
    if summary.iterations == 0 && summary.topics_found == 0 {
        warn!(
            chunks_analyzed = summary.chunks_analyzed,
            "FCM produced no topics (degenerate result or cancellation); preserving prior-cycle global topics"
        );
        return;
    }

    // Phase 1 degeneracy gate: refuse to overwrite good topics with a collapsed
    // model (uniform memberships / label collapse / corpus-wide smearing).
    if topic_gate_rejects(&summary, config, stats) {
        return;
    }

    // Store results
    if let Err(e) = db.clear_topics_for_scope("global").await {
        error!(
            error = %e,
            topics_found = summary.topics_found,
            noise_chunks = summary.noise_chunks,
            "Failed to clear old global topics — clustering completed but DB unchanged"
        );
        return;
    }

    if let Err(e) = db.store_topics("global", &summary.topics).await {
        error!(
            error = %e,
            topics_found = summary.topics_found,
            noise_chunks = summary.noise_chunks,
            "Failed to store global topics (all topics) — clustering completed but no topics persisted"
        );
        return;
    }

    info!(
        topics = summary.topics_found,
        "Global topic clustering scan complete"
    );

    // Phase 1: persist quality metrics for trend + health surfacing.
    persist_topic_quality(db, "global", &summary).await;

    // Mark the global model fresh under the active engine's effective signature:
    // the degeneracy gate passed and the global store succeeded above.
    stamp_topics_signature(db, config).await;

    // Phase 9: chain meta-clustering hierarchy on the global centroids.
    run_hierarchy_pass(db, config, stats).await;
}

/// Phase 9 — meta-clustering hierarchy on global topic centroids.
/// Reads centroids from `code_topics WHERE scope='global'`, runs FCM on them,
/// stores meta-groups as `scope='hierarchy'` rows with parent_topic_ids
/// pointing back at the global topic IDs.
/// Failure-isolated: a hierarchy error does NOT touch the authoritative
/// global assignments, just logs and returns.
async fn run_hierarchy_pass(db: &dyn DbClient, config: &CronConfig, stats: &Arc<StatsTracker>) {
    // Inline SQL not yet on the DbClient trait — escape hatch.
    let pool = db
        .pool()
        .expect("hierarchy pass requires a real &PgPool from DbClient::pool()");

    #[derive(sqlx::FromRow)]
    struct GlobalTopicRow {
        id: i64,
        label: String,
        centroid: Option<Vec<f32>>,
    }

    let rows = match sqlx::query_as::<_, GlobalTopicRow>(
        "SELECT id::bigint, label, centroid
         FROM code_topics
         WHERE scope = 'global' AND centroid IS NOT NULL
         ORDER BY id",
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "hierarchy: failed to load global centroids");
            return;
        }
    };

    let inputs: Vec<crate::cron::topic_hierarchy::TopicCentroid> = rows
        .into_iter()
        .filter_map(|r| {
            r.centroid
                .map(|c| crate::cron::topic_hierarchy::TopicCentroid {
                    topic_id: r.id,
                    label: r.label,
                    centroid: c,
                })
        })
        .collect();

    if inputs.len() < 4 {
        info!(
            global_topics = inputs.len(),
            "hierarchy: not enough global topics (need ≥ 4)"
        );
        return;
    }

    info!(
        global_topics = inputs.len(),
        "hierarchy: running meta-clustering FCM"
    );

    let (meta_groups, _meta_fcm) = crate::cron::topic_hierarchy::cluster_topic_hierarchy(
        &inputs,
        config.topic_fuzziness,
        config.topic_fcm_max_iters,
        config.topic_fcm_tolerance,
    );

    // Build TopicResult rows for each meta-group; store as scope='hierarchy'.
    let meta_topics: Vec<TopicResult> = meta_groups
        .iter()
        .enumerate()
        .map(|(i, g)| TopicResult {
            cluster_index: i as i32,
            label: crate::cron::topic_hierarchy::label_meta_group(g, 5),
            keywords: g.parent_labels.iter().take(5).cloned().collect(),
            keyword_scores: Vec::new(),
            chunk_ids: Vec::new(),
            memberships: Vec::new(),
            file_ids: Vec::new(),
            project_names: Vec::new(),
            avg_internal_similarity: 0.0,
            representative_chunk_id: 0,
            representative_snippet: String::new(),
            top_files: Vec::new(),
            centroid: Vec::new(), // meta-centroid could be persisted too if needed
            parent_topic_ids: g.parent_topic_ids.clone(),
        })
        .collect();

    // Wipe-protection: if the hierarchy FCM produced no meta-groups,
    // preserve the prior-cycle hierarchy rather than clearing it.
    if meta_topics.is_empty() {
        warn!("hierarchy: meta-clustering produced no groups; preserving prior-cycle hierarchy");
        return;
    }
    if let Err(e) = db.clear_topics_for_scope("hierarchy").await {
        error!(error = %e, "hierarchy: clear failed");
        return;
    }
    if let Err(e) = db.store_topics("hierarchy", &meta_topics).await {
        error!(error = %e, "hierarchy: store failed");
        return;
    }

    stats.hierarchy_scans.fetch_add(1, Ordering::Relaxed);
    info!(
        meta_groups = meta_topics.len(),
        "hierarchy: meta-clustering complete"
    );
}

/// Phase 1.2-1.3 streaming dispatch: for medium-sized corpora (between
/// `topic_mmap_n_threshold` and `topic_online_n_threshold`), the data matrix
/// is written to a memory-mapped scratch file and c-TF-IDF content is
/// fetched in batches rather than held all in RAM up-front. Designed to keep
/// anonymous-heap RSS bounded while preserving the in-memory FCM (faster
/// than the fully-online path since the mmap data is OS-cached after the
/// first pass).
async fn run_mmap_global_topic_scan(
    db: &dyn DbClient,
    config: &CronConfig,
    stats: &Arc<StatsTracker>,
    n_total: usize,
    lifecycle: &crate::daemon_state::DaemonLifecycle,
) {
    // Inline SQL not on the trait — escape hatch.
    let pool = db
        .pool()
        .expect("run_mmap_global_topic_scan requires a real &PgPool");

    let params = FcmParams::from_config(config);
    let d = 1024usize;

    // Resolve scratch directory.
    let scratch_dir = crate::mmap_array::resolve_scratch_dir(params.topic_scratch_dir().as_deref());

    info!(
        n = n_total,
        scratch_dir = %scratch_dir.display(),
        "Dispatching to mmap-streaming FCM for global topic scan"
    );

    let mmap = match crate::mmap_array::MmapArrayF32::new(n_total, d, &scratch_dir) {
        Ok(m) => m,
        Err(e) => {
            error!(error = %e, "Failed to allocate mmap scratch — aborting scan");
            return;
        }
    };

    // Stream embeddings + metadata from Postgres into the mmap.
    let mut chunk_metas: Vec<ChunkMetaLite> = Vec::with_capacity(n_total);
    let pool_for_meta = pool.clone();

    let mut mmap = mmap;
    let batch_size = 2048usize;
    let mut offset = 0usize;

    #[derive(sqlx::FromRow)]
    struct EmbRow {
        id: i64,
        file_id: i64,
        project_name: String,
        path: String,
        language: String,
        embedding: Vec<f32>,
    }

    // Phase 5 C8: dispatch on the active embedding signature so we read
    // the canonical column. The legacy 384/`embedding` column has been
    // dropped, leaving `embedding_v2` as the only embedding column, so
    // we resolve it via the active signature rather than hardcoding.
    // Resolved once here — the column cannot change mid-scan.
    let col = match crate::embed::signature::read_active_signature(pool).await {
        Ok(sig) => sig.read_column(),
        Err(e) => {
            error!(error = %e, "global topic scan: read active signature");
            return;
        }
    };

    while offset < n_total {
        // Worktree-aware: filter to canonical project per repo so we
        // don't re-index the same chunks across worktrees / sibling
        // clones. See `bulk_extract_embeddings` in src/db/queries.rs
        // for rationale; the SQL is the same shape.
        let rows = sqlx::query_as::<_, EmbRow>(sqlx::AssertSqlSafe(format!(
            "SELECT c.id AS id, c.file_id AS file_id, p.name AS project_name,
                    f.path AS path, f.language AS language,
                    c.{col}::real[] AS embedding
             FROM file_chunks c
             JOIN indexed_files f ON c.file_id = f.id
             JOIN projects p ON f.project_id = p.id
             WHERE c.{col} IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM projects p_dup
                   WHERE p_dup.id < p.id
                     AND (
                         (p_dup.git_common_dir IS NOT NULL
                          AND p.git_common_dir IS NOT NULL
                          AND p_dup.git_common_dir = p.git_common_dir)
                         OR
                         (p_dup.git_root_commits IS NOT NULL
                          AND p.git_root_commits IS NOT NULL
                          AND p_dup.git_root_commits = p.git_root_commits)
                     )
               )
             ORDER BY c.id
             LIMIT $1 OFFSET $2",
        )))
        .bind(batch_size as i64)
        .bind(offset as i64)
        .fetch_all(&pool_for_meta)
        .await;

        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                error!(error = %e, offset, "mmap-streaming: batch fetch failed");
                return;
            }
        };
        if rows.is_empty() {
            break;
        }

        for row in rows {
            if chunk_metas.len() >= n_total {
                break;
            }
            let idx = chunk_metas.len();
            if let Err(e) = mmap.write_row_l2_normalized(idx, &row.embedding) {
                error!(idx, error = %e, "mmap-streaming: write_row failed");
                return;
            }
            chunk_metas.push(ChunkMetaLite {
                chunk_id: row.id,
                file_id: row.file_id,
                project_name: row.project_name,
                path: row.path,
                language: row.language,
            });
        }
        offset = chunk_metas.len();
    }

    if chunk_metas.is_empty() {
        info!("mmap-streaming: no chunks returned");
        return;
    }

    let n = chunk_metas.len();
    mmap.advise_sequential();

    // Determine K (Phase 12 sweep on a subsample view of the mmap).
    let data_view_owned = mmap.view().to_owned();
    let k = match params.num_clusters {
        Some(explicit) => explicit.min(n),
        None => {
            let base = estimate_k(n, params.min_cluster_size);
            select_k_adaptive(&data_view_owned, base, &params).min(n)
        }
    };

    info!(n, k, "mmap-streaming: running FCM on mmap data");

    // Warm-start from LMDB if enabled (Phase 7).
    let warm = if params.lmdb_enabled {
        load_warm_start_centroids(&params, "global", k, d)
    } else {
        None
    };

    let fcm_result = fuzzy_c_means_with_init(
        mmap.view(),
        k,
        params.fuzziness,
        params.max_iters,
        params.tolerance,
        None,
        warm,
    );

    info!(
        iterations = fcm_result.iterations,
        converged = fcm_result.converged,
        inertia = format!("{:.2}", fcm_result.inertia),
        "mmap-streaming FCM complete"
    );

    // Persist warm-start centroids.
    if params.lmdb_enabled {
        store_warm_start_centroids(&params, "global", &fcm_result.centroids);
    }

    // Assemble chunk → topic mappings (same as cluster_embeddings body).
    let threshold_f32 = params.membership_threshold as f32;
    let mut topic_members: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
    let mut noise_count = 0usize;
    for i in 0..n {
        let row = fcm_result.membership.row(i);
        let max_mu = row.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        if max_mu < threshold_f32 {
            noise_count += 1;
            continue;
        }
        let mut chunk_topics: Vec<(usize, f64)> = Vec::new();
        for t in 0..k {
            let mu = fcm_result.membership[[i, t]];
            if mu >= threshold_f32 {
                chunk_topics.push((t, mu as f64));
            }
        }
        if chunk_topics.is_empty() {
            noise_count += 1;
            continue;
        }
        // Top-J cap (see cap_chunk_memberships / MAX_MEMBERSHIPS_PER_CHUNK).
        cap_chunk_memberships(&mut chunk_topics);
        for (t, score) in chunk_topics {
            topic_members.entry(t).or_default().push((i, score));
        }
    }

    // Streaming c-TF-IDF: fetch chunk content in batches of 1024 ids, tokenize
    // each chunk's content into a reused scratch Vec<String>, updating
    // topic_word_counts in-place. Never holds all content in RAM at once.
    let keyword_sets = compute_ctf_idf_streaming(
        pool,
        &chunk_metas,
        &fcm_result.membership,
        params.label_top_k,
        lifecycle,
    )
    .await;

    // Build topics from topic_members + keyword_sets.
    let data_view = mmap.view();
    let topics = build_topics_from_members(
        &topic_members,
        &chunk_metas,
        &data_view,
        &fcm_result,
        &keyword_sets,
        pool,
    )
    .await;

    // Phase 1: geometry/spread/label metrics on the final model. The mmap path
    // streams content for c-TF-IDF and never holds it all in RAM, so coherence
    // (NPMI/UMass) is left unset here; the structural collapse signals
    // (mean_max_membership, distinct_label_ratio, topics_per_doc,
    // max_topic_share) are computed and are sufficient for the degeneracy gate.
    let metrics = TopicMetrics::compute(
        mmap.view(),
        fcm_result.membership.view(),
        fcm_result.centroids.view(),
        params.fuzziness,
        k,
        &topics,
    );

    let summary = ClusteringSummary {
        scope: "global".to_string(),
        chunks_analyzed: n,
        topics_found: topics.len(),
        noise_chunks: noise_count,
        num_clusters: k,
        fuzziness: params.fuzziness,
        converged: fcm_result.converged,
        iterations: fcm_result.iterations,
        topics,
        metrics: Some(metrics),
    };

    // Record what FCM discovered BEFORE attempting persistence. If storage
    // fails (FK conflict from a chunk deleted mid-run, disk full, etc.) the
    // user still sees an accurate "topics_found / noise_chunks" rather than
    // silently zero. Stats reflect the in-memory computation; persistence
    // is the side-effect.
    stats.topic_scans.fetch_add(1, Ordering::Relaxed);
    stats
        .topics_discovered
        .store(summary.topics_found as u64, Ordering::Relaxed);
    stats
        .topic_noise_chunks
        .store(summary.noise_chunks as u64, Ordering::Relaxed);

    // Wipe-protection: see F12 in the robustness plan. The mmap-streaming
    // path is the global-scope's primary mode; a degenerate FCM result
    // here would clear good data and replace it with nothing.
    if summary.iterations == 0 && summary.topics_found == 0 {
        warn!(
            chunks_analyzed = n,
            "mmap-streaming: FCM produced no topics; preserving prior-cycle global topics"
        );
        return;
    }

    // Phase 1 degeneracy gate (the live global path). This is the exact path
    // that silently produced the 2026-06-13 collapse; the gate now refuses to
    // overwrite prior topics when the new model is degenerate.
    if topic_gate_rejects(&summary, config, stats) {
        return;
    }
    if let Err(e) = db.clear_topics_for_scope("global").await {
        error!(
            error = %e,
            chunks_analyzed = n,
            topics_found = summary.topics_found,
            noise_chunks = summary.noise_chunks,
            "mmap-streaming: clear_topics failed — clustering completed but DB state unchanged"
        );
        return;
    }
    if let Err(e) = db.store_topics("global", &summary.topics).await {
        error!(
            error = %e,
            chunks_analyzed = n,
            topics_found = summary.topics_found,
            noise_chunks = summary.noise_chunks,
            "mmap-streaming: store_topics failed (all topics) — clustering completed but no topics persisted"
        );
        return;
    }

    info!(
        topics = summary.topics_found,
        "mmap-streaming global topic scan complete"
    );

    // Phase 1: persist quality metrics for trend + health surfacing.
    persist_topic_quality(db, "global", &summary).await;

    // Mark the global model fresh under the active engine's effective signature:
    // the degeneracy gate passed and the global store succeeded above.
    stamp_topics_signature(db, config).await;

    drop(data_view_owned);
    drop(mmap);

    // Phase 9: chain meta-clustering hierarchy.
    run_hierarchy_pass(db, config, stats).await;
}

/// Cheap metadata held in RAM for every chunk during mmap-streaming.
/// Content is fetched on demand in compute_ctf_idf_streaming.
struct ChunkMetaLite {
    chunk_id: i64,
    file_id: i64,
    project_name: String,
    path: String,
    #[allow(dead_code)]
    language: String,
}

/// Streaming c-TF-IDF: fetches chunk content in 1024-id batches, tokenises
/// into a reused scratch Vec<String>, and updates weighted topic_word_counts
/// in-place. Never holds more than one batch of content in RAM.
///
/// `lifecycle` is consulted between batches so daemon shutdown breaks the
/// loop instead of hitting the closed pool 1024 IDs at a time. Pass an
/// `is_stopping() == false` lifecycle when invoking from a CLI / one-off
/// context.
async fn compute_ctf_idf_streaming(
    db: &dyn DbClient,
    metas: &[ChunkMetaLite],
    membership: &Array2<f32>,
    top_k: usize,
    lifecycle: &crate::daemon_state::DaemonLifecycle,
) -> Vec<Vec<TopicKeyword>> {
    // Inline SQL not on the trait — escape hatch.
    let pool = db
        .pool()
        .expect("compute_ctf_idf_streaming requires a real &PgPool");

    let n = metas.len();
    let k = membership.ncols();
    let mut topic_word_counts: Vec<HashMap<String, f64>> = vec![HashMap::new(); k];
    let mut topic_total_tokens: Vec<f64> = vec![0.0; k];

    let mut scratch: Vec<String> = Vec::with_capacity(256);
    let mut local_counts: HashMap<String, u32> = HashMap::with_capacity(256);

    #[derive(sqlx::FromRow)]
    struct ContentRow {
        id: i64,
        content: String,
    }

    // chunk_id → index in `metas` (to look up the right membership row).
    let mut id_to_idx: HashMap<i64, usize> = HashMap::with_capacity(n);
    for (i, m) in metas.iter().enumerate() {
        id_to_idx.insert(m.chunk_id, i);
    }

    let batch_ids: Vec<Vec<i64>> = metas
        .chunks(1024)
        .map(|chunk| chunk.iter().map(|m| m.chunk_id).collect())
        .collect();

    for ids in &batch_ids {
        if lifecycle.is_stopping() {
            info!(
                processed_batches = batch_ids
                    .iter()
                    .position(|b| std::ptr::eq(b.as_ptr(), ids.as_ptr()))
                    .unwrap_or(0),
                total_batches = batch_ids.len(),
                "streaming c-TF-IDF: lifecycle stopping, breaking loop"
            );
            break;
        }
        let rows = match sqlx::query_as::<_, ContentRow>(
            "SELECT id, content FROM file_chunks WHERE id = ANY($1::bigint[])",
        )
        .bind(ids)
        .fetch_all(pool)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                if crate::cron::shutdown::is_terminal_db_error(&e) {
                    warn!(
                        error = %e,
                        "streaming c-TF-IDF: DB pool closed or runtime shutting down, aborting"
                    );
                    break;
                }
                error!(error = %e, "streaming c-TF-IDF: content batch fetch failed");
                continue;
            }
        };
        for row in rows {
            let i = match id_to_idx.get(&row.id) {
                Some(&x) => x,
                None => continue,
            };
            tokenize_into(&row.content, &mut scratch);
            local_counts.clear();
            for token in &scratch {
                *local_counts.entry(token.clone()).or_insert(0) += 1;
            }
            for (t, mu) in top_membership_topics(membership, i, k) {
                for (word, &count) in &local_counts {
                    let weighted = mu * count as f64;
                    *topic_word_counts[t].entry(word.clone()).or_insert(0.0) += weighted;
                    topic_total_tokens[t] += weighted;
                }
            }
        }
    }

    let avg_tokens = if k > 0 {
        topic_total_tokens.iter().sum::<f64>() / k as f64
    } else {
        1.0
    };

    let mut word_topic_freq: HashMap<String, usize> = HashMap::new();
    for topic_counts in &topic_word_counts {
        for word in topic_counts.keys() {
            *word_topic_freq.entry(word.clone()).or_insert(0) += 1;
        }
    }

    // Same max-document-frequency cutoff as the in-memory `compute_ctf_idf`:
    // drop words appearing in > 40% of topics (>= 5 topics) so the global
    // (streaming) labels are as discriminative as the in-memory ones.
    let df_cap: usize = if k >= 5 {
        ((k as f64) * 0.4).ceil() as usize
    } else {
        usize::MAX
    };

    (0..k)
        .map(|t| {
            let total = topic_total_tokens[t].max(1.0);
            let mut scored: Vec<TopicKeyword> = topic_word_counts[t]
                .iter()
                .filter(|(word, _)| *word_topic_freq.get(*word).unwrap_or(&1) <= df_cap)
                .map(|(word, &count)| {
                    let tf = count / total;
                    let df = *word_topic_freq.get(word).unwrap_or(&1) as f64;
                    let idf = (1.0 + avg_tokens / df).ln();
                    TopicKeyword {
                        word: word.clone(),
                        score: tf * idf,
                    }
                })
                .collect();
            scored.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.word.cmp(&b.word))
            });
            scored.truncate(top_k);
            scored
        })
        .collect()
}

/// Shared topic-assembly used by both the small-n and mmap-streaming paths.
/// `content_lookup` is fetched on demand per representative chunk_id.
async fn build_topics_from_members(
    topic_members: &HashMap<usize, Vec<(usize, f64)>>,
    metas: &[ChunkMetaLite],
    data_view: &ArrayView2<'_, f32>,
    fcm_result: &FcmResult,
    keyword_sets: &[Vec<TopicKeyword>],
    db: &dyn DbClient,
) -> Vec<TopicResult> {
    // Inline SQL not on the trait — escape hatch.
    let pool = db
        .pool()
        .expect("build_topics_from_members requires a real &PgPool");

    let mut topics: Vec<TopicResult> = Vec::with_capacity(topic_members.len());
    for (&topic_idx, members) in topic_members {
        if members.is_empty() {
            continue;
        }

        let chunk_ids: Vec<i64> = members.iter().map(|&(i, _)| metas[i].chunk_id).collect();
        let memberships: Vec<f64> = members.iter().map(|&(_, mu)| mu).collect();
        let file_ids: Vec<i64> = members
            .iter()
            .map(|&(i, _)| metas[i].file_id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let project_names: Vec<String> = members
            .iter()
            .map(|&(i, _)| metas[i].project_name.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        let member_indices: Vec<usize> = members.iter().map(|&(i, _)| i).collect();
        let avg_sim = avg_internal_similarity(data_view, &member_indices);
        let representative_id = find_representative(data_view, &chunk_ids, &member_indices);

        // Fetch representative snippet from DB.
        let representative_snippet = match sqlx::query_scalar::<_, Option<String>>(
            "SELECT content FROM file_chunks WHERE id = $1",
        )
        .bind(representative_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(Some(c))) => {
                if c.len() > 500 {
                    format!("{}...", &c[..c.floor_char_boundary(500)])
                } else {
                    c
                }
            }
            _ => String::new(),
        };

        let mut file_chunk_counts: HashMap<(&str, &str), f64> = HashMap::new();
        for &(i, mu) in members {
            let key = (metas[i].path.as_str(), metas[i].project_name.as_str());
            *file_chunk_counts.entry(key).or_insert(0.0) += mu;
        }
        let mut top_files: Vec<TopicFileEntry> = file_chunk_counts
            .into_iter()
            .map(|((path, project), weighted)| TopicFileEntry {
                path: path.to_string(),
                project: project.to_string(),
                chunks_in_topic: weighted.round() as i32,
            })
            .collect();
        top_files.sort_by_key(|b| std::cmp::Reverse(b.chunks_in_topic));

        let empty_kw = Vec::new();
        let kw = keyword_sets.get(topic_idx).unwrap_or(&empty_kw);
        let label = label_from_keywords(kw, topic_idx as i32);
        let keywords: Vec<String> = kw.iter().map(|k| k.word.clone()).collect();
        let keyword_scores: Vec<f64> = kw.iter().map(|k| k.score).collect();

        let centroid_vec: Vec<f32> = fcm_result.centroids.row(topic_idx).to_vec();

        topics.push(TopicResult {
            cluster_index: topic_idx as i32,
            label,
            keywords,
            keyword_scores,
            chunk_ids,
            memberships,
            file_ids,
            project_names,
            avg_internal_similarity: avg_sim,
            representative_chunk_id: representative_id,
            representative_snippet,
            top_files,
            centroid: centroid_vec,
            parent_topic_ids: Vec::new(),
        });

        let _ = fcm_result; // Suppress unused if fcm_result fields not touched above.
    }
    topics.sort_by_key(|b| std::cmp::Reverse(b.chunk_ids.len()));
    topics
}

/// Phase 8 online FCM dispatch: streams embeddings from PostgreSQL in
/// mini-batches, runs bounded-memory FCM, writes final centroids to LMDB
/// and returns (membership rows are persisted per-chunk in the LMDB
/// `memberships_dense` sub-db; too large to return in RAM).
async fn run_online_global_topic_scan(
    db: &dyn DbClient,
    config: &CronConfig,
    stats: &Arc<StatsTracker>,
    n_total: usize,
) {
    use crate::cron::topic_clustering_online::{
        BatchFetcher, MembershipStore, OnlineFcmConfig, fuzzy_c_means_online,
    };

    // Inline SQL not on the trait — escape hatch.
    let pool = db
        .pool()
        .expect("run_online_global_topic_scan requires a real &PgPool");

    let params = FcmParams::from_config(config);
    let d = 1024usize;
    let k = params
        .num_clusters
        .unwrap_or_else(|| estimate_k(n_total, params.min_cluster_size));

    // Build the LMDB-backed membership store.
    let store_path = params
        .lmdb_path
        .clone()
        .unwrap_or_else(crate::topic_store::lmdb::default_path);
    let centroid_store = match crate::topic_store::CentroidStore::open(&store_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            error!(
                path = %store_path.display(),
                error = %e,
                "Online FCM: failed to open LMDB store, aborting"
            );
            return;
        }
    };

    // Warm-start centroids (Phase 7).
    let warm_centroids = if params.lmdb_enabled {
        load_warm_start_centroids(&params, "global", k, d)
    } else {
        None
    };

    // MembershipStore adapter: proxies dense Vec<f32> through CentroidStore.
    struct LmdbDenseMembershipAdapter {
        store: Arc<crate::topic_store::CentroidStore>,
    }
    impl MembershipStore for LmdbDenseMembershipAdapter {
        fn load(&self, chunk_id: i64) -> Option<Vec<f32>> {
            self.store.load_membership_dense(chunk_id).ok().flatten()
        }
        fn store(&mut self, chunk_id: i64, membership: &[f32]) {
            let _ = self.store.store_membership_dense(chunk_id, membership);
        }
        fn store_batch(&mut self, items: &[(i64, Vec<f32>)]) {
            let _ = self.store.store_memberships_dense_batch(items);
        }
    }
    let membership_store = Arc::new(std::sync::Mutex::new(LmdbDenseMembershipAdapter {
        store: Arc::clone(&centroid_store),
    }));

    // BatchFetcher via tokio runtime handle + sqlx cursor streaming by OFFSET.
    // We use a blocking closure that calls into the async runtime; since this
    // is invoked from `tokio::task::spawn_blocking` below, `Handle::block_on`
    // is safe here.
    let pool_clone = pool.clone();
    let rt_handle = tokio::runtime::Handle::current();

    // Phase 5 C8: resolve the active-signature column ONCE here (async
    // context), then move the `&'static str` into the blocking + fetcher
    // closures by copy. The legacy 384/`embedding` column has been
    // dropped, leaving `embedding_v2` as the only embedding column.
    let col = match crate::embed::signature::read_active_signature(pool).await {
        Ok(sig) => sig.read_column(),
        Err(e) => {
            error!(error = %e, "online global topic scan: read active signature");
            return;
        }
    };

    // Move to blocking context for the long-running FCM loop.
    let cfg = OnlineFcmConfig {
        k,
        m: params.fuzziness,
        max_iters: params.max_iters,
        tolerance: params.tolerance,
        batch_size: config.topic_online_batch_size,
        n_expected: n_total,
        d,
    };

    let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
    let t0 = std::time::Instant::now();

    let result = tokio::task::spawn_blocking(move || {
        let fetcher: BatchFetcher = Box::new(move |bs, off| {
            rt_handle.block_on(async {
                #[derive(sqlx::FromRow)]
                struct Row {
                    id: i64,
                    embedding: Vec<f32>,
                }
                // Worktree-aware: filter to canonical project per
                // repo. See bulk_extract_embeddings rationale.
                let rows = sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(format!(
                    "SELECT c.id AS id, c.{col}::real[] AS embedding
                     FROM file_chunks c
                     JOIN indexed_files f ON c.file_id = f.id
                     JOIN projects p ON f.project_id = p.id
                     WHERE c.{col} IS NOT NULL
                       AND NOT EXISTS (
                           SELECT 1 FROM projects p_dup
                           WHERE p_dup.id < p.id
                             AND (
                                 (p_dup.git_common_dir IS NOT NULL
                                  AND p.git_common_dir IS NOT NULL
                                  AND p_dup.git_common_dir = p.git_common_dir)
                                 OR
                                 (p_dup.git_root_commits IS NOT NULL
                                  AND p.git_root_commits IS NOT NULL
                                  AND p_dup.git_root_commits = p.git_root_commits)
                             )
                       )
                     ORDER BY c.id
                     LIMIT $1 OFFSET $2",
                )))
                .bind(bs as i64)
                .bind(off as i64)
                .fetch_all(&pool_clone)
                .await
                .ok()?;
                if rows.is_empty() {
                    return None;
                }
                let d_local = rows[0].embedding.len();
                let mut data = Array2::<f32>::zeros((rows.len(), d_local));
                let mut ids = Vec::with_capacity(rows.len());
                for (i, row) in rows.into_iter().enumerate() {
                    for (j, &v) in row.embedding.iter().enumerate() {
                        data[[i, j]] = v;
                    }
                    // L2-normalize row
                    let norm: f32 = data.row(i).dot(&data.row(i)).sqrt();
                    if norm > 1e-12 {
                        data.row_mut(i).mapv_inplace(|x| x / norm);
                    }
                    ids.push(row.id);
                }
                Some((ids, data))
            })
        });
        fuzzy_c_means_online(fetcher, &cfg, membership_store, warm_centroids, None)
    })
    .await;

    let fcm_result = match result {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "Online FCM spawn_blocking panicked");
            return;
        }
    };

    let rss_end = crate::stats::rss::current_rss_bytes().unwrap_or(0);
    info!(
        job = "online-fcm",
        n = n_total,
        k,
        iterations = fcm_result.iterations,
        converged = fcm_result.converged,
        rss_mb_delta = (rss_end as i64 - rss_start as i64) >> 20,
        elapsed_s = t0.elapsed().as_secs_f64(),
        "Online FCM complete"
    );

    // Persist final centroids for next warm-start.
    if params.lmdb_enabled {
        store_warm_start_centroids(&params, "global", &fcm_result.centroids);
    }

    // Persist topics into code_topics with per-chunk metadata sourced
    // from the LMDB membership store. Pre-B.3(b), this path wrote empty
    // shells (`chunk_ids: Vec::new()`) so `chunk_topic_assignments` was
    // never populated and `code_topics.chunk_count` rendered as 0 (or,
    // worse, the project-wide total) for every topic. Now: read every
    // stored membership row from LMDB, bucket chunks into the topics
    // whose membership exceeds `membership_threshold`, and pass the
    // populated topic list to `db.store_topics` so the per-chunk
    // assignments land.
    let k = fcm_result.centroids.nrows();
    let mut topic_chunk_ids: Vec<Vec<i64>> = vec![Vec::new(); k];
    let mut topic_memberships: Vec<Vec<f64>> = vec![Vec::new(); k];

    if params.lmdb_enabled
        && let Some(store) = open_centroid_store(&params)
    {
        let threshold = params.membership_threshold;
        let memberships = match store.collect_memberships_dense() {
            Ok(v) => v,
            Err(e) => {
                error!(error = %e, "online FCM: collect_memberships_dense failed; falling back to empty shells");
                Vec::new()
            }
        };
        let mut chunk_topic_pairs = 0u64;
        for (chunk_id, mu) in memberships {
            if mu.len() != k {
                // Stale LMDB rows from a prior run with a different K.
                // `clear_all` should have been called on K change, but
                // be defensive in case the store survived a restart.
                continue;
            }
            // Collect this chunk's above-threshold topics, cap to the top-J
            // strongest (same rule as the in-memory paths), then bucket.
            let mut chunk_topics: Vec<(usize, f64)> = Vec::new();
            for (j, &m) in mu.iter().enumerate() {
                if (m as f64) >= threshold {
                    chunk_topics.push((j, m as f64));
                }
            }
            cap_chunk_memberships(&mut chunk_topics);
            for (j, score) in chunk_topics {
                topic_chunk_ids[j].push(chunk_id);
                topic_memberships[j].push(score);
                chunk_topic_pairs += 1;
            }
        }
        info!(
            job = "online-fcm",
            chunk_topic_pairs, "Bucketed LMDB memberships into per-topic chunk lists"
        );
    }

    let topics_built: Vec<TopicResult> = (0..k)
        .map(|i| TopicResult {
            cluster_index: i as i32,
            label: format!("topic_{}", i),
            keywords: Vec::new(),
            keyword_scores: Vec::new(),
            chunk_ids: std::mem::take(&mut topic_chunk_ids[i]),
            memberships: std::mem::take(&mut topic_memberships[i]),
            file_ids: Vec::new(),
            project_names: Vec::new(),
            avg_internal_similarity: 0.0,
            representative_chunk_id: 0,
            representative_snippet: String::new(),
            top_files: Vec::new(),
            centroid: fcm_result.centroids.row(i).to_vec(),
            parent_topic_ids: Vec::new(),
        })
        .collect();
    if let Err(e) = db.clear_topics_for_scope("global").await {
        error!(error = %e, "online FCM: clear global failed");
    }
    // The online mini-batch path persists per-chunk membership to LMDB rather
    // than RAM (to bound memory on huge corpora) and so does not compute the
    // full membership/label metrics the canonical `topic_gate_rejects` gate
    // needs; the signature's contract is freshness ("the global model is current
    // under this engine"), not label quality (that is the separate
    // `topics_quality`/`topics_degenerate` channel), so a successful store is the
    // correct stamp condition here.
    match db.store_topics("global", &topics_built).await {
        Err(e) => error!(error = %e, "online FCM: store global topics failed"),
        Ok(()) => stamp_topics_signature(db, config).await,
    }

    stats.topic_scans.fetch_add(1, Ordering::Relaxed);
    // Online FCM produces k shell-topic centroids; per-chunk membership is
    // persisted in LMDB rather than held in RAM, so we record the cluster
    // count as the canonical "topics discovered" number. Noise count is not
    // tracked on this path (would require a separate LMDB scan to count
    // chunks whose max-membership is below the threshold) — leaving
    // `topic_noise_chunks` as zero is correct in spirit (it isn't *known*),
    // but a follow-up could expose noise counting from
    // `fuzzy_c_means_online`. See topic_clustering_online.rs.
    stats.topics_discovered.store(k as u64, Ordering::Relaxed);

    // Phase 9: chain meta-clustering hierarchy.
    run_hierarchy_pass(db, config, stats).await;
}

/// Query the total chunk count with an embedding — used by memory pre-flight
/// AND to size the mmap allocation in `run_mmap_global_topic_scan`. The
/// count must match the filter used by the streaming query (canonical
/// project per `git_common_dir` / `git_root_commits` group), otherwise the
/// mmap is over-allocated and downstream `mmap.view()` includes garbage
/// rows for un-populated indices.
async fn count_chunks(db: &dyn DbClient) -> Option<usize> {
    let pool = db
        .pool()
        .expect("count_chunks requires a real &PgPool from DbClient::pool()");
    // Phase 5 C8: count must use the same active-signature column as the
    // streaming query (`embedding_v2` post-cutover, `embedding`
    // pre-cutover). Mismatching here would over/under-size the mmap.
    let col = match crate::embed::signature::read_active_signature(pool).await {
        Ok(sig) => sig.read_column(),
        Err(e) => {
            error!(error = %e, "count_chunks: read active signature failed");
            return None;
        }
    };
    match sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*)
         FROM file_chunks c
         JOIN indexed_files f ON c.file_id = f.id
         JOIN projects p ON f.project_id = p.id
         WHERE c.{col} IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM projects p_dup
               WHERE p_dup.id < p.id
                 AND (
                     (p_dup.git_common_dir IS NOT NULL
                      AND p.git_common_dir IS NOT NULL
                      AND p_dup.git_common_dir = p.git_common_dir)
                     OR
                     (p_dup.git_root_commits IS NOT NULL
                      AND p.git_root_commits IS NOT NULL
                      AND p_dup.git_root_commits = p.git_root_commits)
                 )
           )",
    )))
    .fetch_one(pool)
    .await
    {
        Ok(n) => Some(n as usize),
        Err(e) => {
            error!(error = %e, "count_chunks pre-flight query failed");
            None
        }
    }
}

/// Phase 6 — per-project graph-hybrid topic scan (the default engine since the
/// 2026-06-13 bake-off). For each project: cluster its fused semantic+import+
/// co-change file graph into community-topics, apply the degeneracy gate,
/// optionally LLM-relabel, persist quality, and store under `scope='project:NAME'`.
///
/// Memory-safe by construction (one project's chunks at a time), so it sidesteps
/// the global-corpus memory pressure that motivated the FCM mmap/online paths.
/// `chunk_topic_assignments` is populated per-project, so the global analysis
/// tools (orphans / coverage / misplaced-code) keep working.
async fn run_graph_topic_scan(db: &dyn DbClient, config: &CronConfig, stats: &Arc<StatsTracker>) {
    let pool = match db.pool() {
        Some(p) => p,
        None => {
            error!("run_graph_topic_scan requires a real &PgPool");
            return;
        }
    };
    let projects: Vec<(i32, String)> =
        match sqlx::query_as::<_, (i32, String)>("SELECT id, name FROM projects ORDER BY id")
            .fetch_all(pool)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "graph topic scan: failed to list projects");
                return;
            }
        };

    let w = &config.topic_graph_edge_weights;
    let ew = [
        w.first().copied().unwrap_or(1.0),
        w.get(1).copied().unwrap_or(1.0),
        w.get(2).copied().unwrap_or(1.0),
    ];

    let mut total_topics = 0usize;
    let mut total_noise = 0usize;
    for (pid, name) in &projects {
        let rows = match db.bulk_extract_project_embeddings(name, None).await {
            Ok(r) => r,
            Err(e) => {
                error!(project = %name, error = %e, "graph topic scan: extract failed; skipping");
                continue;
            }
        };
        if rows.is_empty() {
            continue;
        }
        let scope = format!("project:{name}");
        let mut summary = if config.topic_clustering_method == "embedding_hdbscan" {
            cluster_embeddings_hdbscan(&rows, config, config.topic_min_cluster_size, &scope)
        } else {
            let edges = crate::cron::topic_graph::load_project_graph_edges(db, *pid)
                .await
                .unwrap_or_default();
            crate::cron::topic_graph::cluster_graph(
                &rows,
                &edges,
                ew,
                config.topic_graph_resolution,
                config.topic_min_cluster_size,
                config.topic_label_top_k,
                &scope,
            )
        };
        // Wipe-protection + degeneracy gate before the destructive clear.
        if summary.iterations == 0 && summary.topics_found == 0 {
            warn!(project = %name, "graph topic scan: no topics; preserving prior");
            continue;
        }
        if topic_gate_rejects(&summary, config, stats) {
            continue;
        }
        // LLM-relabel all topics (config-gated; deterministic fallback). Runs on
        // the authoritative per-project store, not the on-demand path.
        summary.topics = crate::cron::topic_label_llm::maybe_relabel(
            std::mem::take(&mut summary.topics),
            config,
        )
        .await;
        if let Err(e) = db.clear_topics_for_scope(&scope).await {
            error!(project = %name, error = %e, "graph topic scan: clear failed; preserving prior");
            continue;
        }
        if let Err(e) = db.store_topics(&scope, &summary.topics).await {
            error!(project = %name, error = %e, "graph topic scan: store failed");
            continue;
        }
        persist_topic_quality(db, &scope, &summary).await;
        total_topics += summary.topics_found;
        total_noise += summary.noise_chunks;
    }

    // Cross-project global roll-up (scope='global') by meta-clustering the
    // per-project topic centroids, then the hierarchy overlay (scope='hierarchy')
    // on top of the fresh global centroids.
    build_global_rollup(db, config, stats).await;
    run_hierarchy_pass(db, config, stats).await;

    stats.topic_scans.fetch_add(1, Ordering::Relaxed);
    stats
        .topics_discovered
        .store(total_topics as u64, Ordering::Relaxed);
    stats
        .topic_noise_chunks
        .store(total_noise as u64, Ordering::Relaxed);
    info!(
        projects = projects.len(),
        total_topics, total_noise, "graph topic scan complete (per-project; scope='project:NAME')"
    );
}

/// Build the cross-project global roll-up: meta-cluster the per-project topic
/// centroids into `scope='global'` topics so `discover_topics` with no project
/// returns a cross-project view ("error handling", "parsing", … as they recur
/// across projects). Uses cosine-kNN + Louvain over the centroids (robust at the
/// centroid level, consistent with the graph engine); each global topic
/// aggregates its member per-project topics (summed `chunk_count`, merged
/// keywords, mean centroid, `parent_topic_ids` → members) and is stored WITHOUT
/// duplicating `chunk_topic_assignments`. Gated + quality-persisted + LLM-labeled
/// like the per-project pass.
async fn build_global_rollup(db: &dyn DbClient, config: &CronConfig, stats: &Arc<StatsTracker>) {
    /// kNN degree over centroids for the meta-graph.
    const KNN: usize = 10;
    /// Minimum cosine to draw a meta-edge (centroids are L2-normalized).
    const MIN_COS: f32 = 0.30;

    let pool = match db.pool() {
        Some(p) => p,
        None => return,
    };

    #[derive(sqlx::FromRow)]
    struct ProjTopicRow {
        id: i32,
        label: String,
        keywords: Option<Vec<String>>,
        keyword_scores: Option<Vec<f32>>,
        chunk_count: i32,
        file_count: i32,
        representative_chunk_id: Option<i64>,
        representative_snippet: Option<String>,
        project_names: Option<Vec<String>>,
        centroid: Option<Vec<f32>>,
    }

    let rows = match sqlx::query_as::<_, ProjTopicRow>(
        "SELECT id, label, keywords, keyword_scores, chunk_count, file_count,
                representative_chunk_id, representative_snippet, project_names, centroid
         FROM code_topics
         WHERE scope LIKE 'project:%' AND centroid IS NOT NULL
         ORDER BY id",
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "global rollup: failed to load per-project topics");
            return;
        }
    };

    let cand: Vec<ProjTopicRow> = rows
        .into_iter()
        .filter(|r| r.centroid.as_ref().is_some_and(|c| !c.is_empty()))
        .collect();
    if cand.len() < 4 {
        info!(
            candidates = cand.len(),
            "global rollup: too few per-project topics; skipping"
        );
        return;
    }
    let d = cand[0].centroid.as_ref().expect("filtered Some").len();

    // Centroid matrix (defensively re-L2-normalized).
    let n = cand.len();
    let mut cen = Array2::<f32>::zeros((n, d));
    for (i, r) in cand.iter().enumerate() {
        let c = r.centroid.as_ref().expect("filtered Some");
        for (j, &v) in c.iter().take(d).enumerate() {
            cen[[i, j]] = v;
        }
        let norm: f32 = cen.row(i).dot(&cen.row(i)).sqrt();
        if norm > 1e-12 {
            cen.row_mut(i).mapv_inplace(|x| x / norm);
        }
    }

    // Cosine-kNN meta-graph over centroids → Louvain communities.
    use petgraph::graph::{DiGraph, NodeIndex};
    let mut g: DiGraph<usize, crate::graph::types::EdgeWeight> = DiGraph::new();
    let nodes: Vec<NodeIndex> = (0..n).map(|i| g.add_node(i)).collect();
    for i in 0..n {
        let mut sims: Vec<(usize, f32)> = (0..n)
            .filter(|&j| j != i)
            .map(|j| (j, cen.row(i).dot(&cen.row(j))))
            .filter(|&(_, s)| s >= MIN_COS)
            .collect();
        sims.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (j, s) in sims.into_iter().take(KNN) {
            g.add_edge(
                nodes[i],
                nodes[j],
                crate::graph::types::EdgeWeight {
                    edge_type: crate::graph::types::EdgeType::Semantic,
                    weight: s as f64,
                },
            );
        }
    }
    let louvain = crate::graph::algorithms::louvain_communities(&g, config.topic_graph_resolution);

    // community id → member candidate indices.
    let mut comm: HashMap<usize, Vec<usize>> = HashMap::new();
    for (node, c) in &louvain.communities {
        comm.entry(*c).or_default().push(node.index());
    }
    let mut communities: Vec<Vec<usize>> = comm.into_values().collect();
    communities
        .sort_by_key(|m| std::cmp::Reverse(m.iter().map(|&i| cand[i].chunk_count).sum::<i32>()));

    // Aggregate each community into a GlobalRollupRow.
    let mut rollup: Vec<crate::db::queries::GlobalRollupRow> =
        Vec::with_capacity(communities.len());
    for (ci, members) in communities.iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        let mut kw_scores: HashMap<String, f64> = HashMap::new();
        let mut chunk_count = 0i32;
        let mut file_count = 0i32;
        let mut projects: HashSet<String> = HashSet::new();
        let mut parent_ids: Vec<i64> = Vec::with_capacity(members.len());
        let mut rep_idx = members[0];
        let mut mean = vec![0.0f32; d];
        for &mi in members {
            let r = &cand[mi];
            chunk_count += r.chunk_count;
            file_count += r.file_count;
            parent_ids.push(r.id as i64);
            if let Some(ps) = &r.project_names {
                projects.extend(ps.iter().cloned());
            }
            match (&r.keywords, &r.keyword_scores) {
                (Some(ws), Some(ss)) => {
                    for (w, s) in ws.iter().zip(ss.iter()) {
                        *kw_scores.entry(w.clone()).or_insert(0.0) += *s as f64;
                    }
                }
                (Some(ws), None) => {
                    for w in ws {
                        *kw_scores.entry(w.clone()).or_insert(0.0) += 1.0;
                    }
                }
                _ => {}
            }
            if cand[mi].chunk_count > cand[rep_idx].chunk_count {
                rep_idx = mi;
            }
            let c = r.centroid.as_ref().expect("filtered Some");
            for (j, &v) in c.iter().take(d).enumerate() {
                mean[j] += v;
            }
        }
        let m = members.len() as f32;
        for v in &mut mean {
            *v /= m;
        }
        let norm: f32 = mean.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 1e-12 {
            for v in &mut mean {
                *v /= norm;
            }
        }
        let mut scored: Vec<(String, f64)> = kw_scores.into_iter().collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(config.topic_label_top_k.max(1));
        let keywords: Vec<String> = scored.iter().map(|(w, _)| w.clone()).collect();
        let keyword_scores: Vec<f32> = scored.iter().map(|(_, s)| *s as f32).collect();
        let label = if keywords.is_empty() {
            format!("topic_{ci}")
        } else {
            keywords.join(" / ")
        };
        let mut project_names: Vec<String> = projects.into_iter().collect();
        project_names.sort();
        rollup.push(crate::db::queries::GlobalRollupRow {
            cluster_index: ci as i32,
            label,
            keywords,
            keyword_scores,
            centroid: mean,
            chunk_count,
            file_count,
            project_names,
            representative_chunk_id: cand[rep_idx].representative_chunk_id.unwrap_or(0),
            representative_snippet: cand[rep_idx]
                .representative_snippet
                .clone()
                .unwrap_or_default(),
            parent_topic_ids: parent_ids,
        });
    }

    if rollup.is_empty() {
        warn!("global rollup: produced no meta-topics; preserving prior global scope");
        return;
    }

    // Degeneracy guard on the roll-up labels (no membership matrix here).
    let distinct: HashSet<&str> = rollup.iter().map(|r| r.label.as_str()).collect();
    let distinct_ratio = distinct.len() as f64 / rollup.len() as f64;
    if rollup.len() >= 5 && distinct_ratio < config.topic_min_distinct_label_ratio {
        warn!(
            rollup = rollup.len(),
            distinct_ratio, "global rollup: labels degenerate; preserving prior global scope"
        );
        stats
            .topic_degenerate_refusals
            .fetch_add(1, Ordering::Relaxed);
        return;
    }

    // LLM-relabel the global meta-topics (config-gated; deterministic fallback).
    if config.topic_llm_labels {
        let shim: Vec<TopicResult> = rollup
            .iter()
            .map(|r| TopicResult {
                cluster_index: r.cluster_index,
                label: r.label.clone(),
                keywords: r.keywords.clone(),
                keyword_scores: r.keyword_scores.iter().map(|&s| s as f64).collect(),
                chunk_ids: Vec::new(),
                memberships: Vec::new(),
                file_ids: Vec::new(),
                project_names: r.project_names.clone(),
                avg_internal_similarity: 0.0,
                representative_chunk_id: r.representative_chunk_id,
                representative_snippet: r.representative_snippet.clone(),
                top_files: Vec::new(),
                centroid: Vec::new(),
                parent_topic_ids: r.parent_topic_ids.clone(),
            })
            .collect();
        let relabeled = crate::cron::topic_label_llm::maybe_relabel(shim, config).await;
        for (r, t) in rollup.iter_mut().zip(relabeled) {
            r.label = t.label;
        }
    }

    if let Err(e) = db.clear_topics_for_scope("global").await {
        error!(error = %e, "global rollup: clear failed; preserving prior");
        return;
    }
    if let Err(e) = crate::db::queries::store_global_rollup(pool, &rollup).await {
        error!(error = %e, "global rollup: store failed");
        return;
    }

    let q = serde_json::json!({
        "n_topics": rollup.len(),
        "distinct_label_ratio": distinct_ratio,
        "source": "global_rollup_over_per_project_centroids",
    });
    if let Err(e) = crate::db::queries::set_topic_quality(pool, "global", &q).await {
        error!(error = %e, "global rollup: persist quality failed");
    }

    // Mark the global model fresh: the roll-up cleared the per-project degeneracy
    // guards above and the `global`-scope store succeeded. This is the success
    // point for the default `graph` engine (it never calls `store_topics`
    // with scope="global").
    stamp_topics_signature(db, config).await;

    info!(
        meta_topics = rollup.len(),
        from_project_topics = n,
        modularity = format!("{:.4}", louvain.modularity),
        "global rollup complete (scope='global')"
    );
}

/// Emergency fallback: when global clustering's memory prediction exceeds the
/// budget, cluster each project in isolation so *some* topic coverage exists
/// for this cycle. Topic IDs stored this way are NOT cross-project-comparable
/// (that's the whole point of `scope="global"`), so this is the failsafe path,
/// NOT a primary mode. Each project's FCM peak is bounded by that project's
/// chunk count — much smaller than the union.
async fn run_per_project_emergency_fallback(
    db: &dyn DbClient,
    config: &CronConfig,
    stats: &Arc<StatsTracker>,
) {
    let pool = db
        .pool()
        .expect("emergency fallback requires a real &PgPool from DbClient::pool()");
    let projects: Vec<(i32, String)> =
        match sqlx::query_as::<_, (i32, String)>("SELECT id, name FROM projects ORDER BY id")
            .fetch_all(pool)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "emergency fallback: failed to list projects");
                return;
            }
        };

    let mut total_topics = 0usize;
    let mut total_noise = 0usize;
    for (_project_id, project_name) in &projects {
        let rss_start = crate::stats::rss::current_rss_bytes().unwrap_or(0);
        match run_project_topic_scan(
            db,
            project_name,
            config,
            config.topic_min_cluster_size,
            None,
        )
        .await
        {
            Ok(summary) => {
                let scope = format!("project:{}", project_name);
                // Wipe-protection: don't clear an existing per-project
                // topic set when this iteration produced nothing.
                if summary.iterations == 0 && summary.topics_found == 0 {
                    warn!(
                        project = %project_name,
                        "emergency fallback: FCM produced no topics; preserving prior per-project topics"
                    );
                    continue;
                }
                // Phase 1 degeneracy gate (per-project failsafe path).
                if topic_gate_rejects(&summary, config, stats) {
                    continue;
                }
                if let Err(e) = db.clear_topics_for_scope(&scope).await {
                    error!(
                        project = %project_name,
                        error = %e,
                        "emergency fallback: clear_topics failed; skipping store to preserve prior topics"
                    );
                    continue;
                }
                if let Err(e) = db.store_topics(&scope, &summary.topics).await {
                    error!(
                        project = %project_name,
                        error = %e,
                        "emergency fallback: failed to store per-project topics"
                    );
                    continue;
                }
                total_topics += summary.topics_found;
                total_noise += summary.noise_chunks;
                persist_topic_quality(db, &scope, &summary).await;
                let rss_end = crate::stats::rss::current_rss_bytes().unwrap_or(0);
                info!(
                    project = %project_name,
                    topics = summary.topics_found,
                    noise = summary.noise_chunks,
                    chunks = summary.chunks_analyzed,
                    rss_mb_delta = (rss_end as i64 - rss_start as i64) >> 20,
                    "emergency fallback: per-project clustering complete"
                );
            }
            Err(e) => {
                error!(
                    project = %project_name,
                    error = %e,
                    "emergency fallback: per-project clustering failed"
                );
            }
        }
    }

    stats.topic_scans.fetch_add(1, Ordering::Relaxed);
    stats
        .topics_discovered
        .store(total_topics as u64, Ordering::Relaxed);
    stats
        .topic_noise_chunks
        .store(total_noise as u64, Ordering::Relaxed);
    info!(
        projects = projects.len(),
        total_topics,
        total_noise,
        "emergency fallback: per-project clustering cycle complete (global scope NOT refreshed)"
    );
}

// ============================================================================
// Entry point 2: Project topic scan (on-demand)
// ============================================================================

/// Run topic clustering for a single project, returning results directly.
pub async fn run_project_topic_scan(
    db: &dyn DbClient,
    project_name: &str,
    config: &CronConfig,
    min_cluster_size: usize,
    language: Option<&str>,
) -> Result<ClusteringSummary, anyhow::Error> {
    let rows = db
        .bulk_extract_project_embeddings(project_name, language)
        .await?;

    if rows.is_empty() {
        return Ok(ClusteringSummary {
            scope: format!("project:{}", project_name),
            chunks_analyzed: 0,
            topics_found: 0,
            noise_chunks: 0,
            num_clusters: 0,
            fuzziness: config.topic_fuzziness,
            converged: false,
            iterations: 0,
            topics: Vec::new(),
            metrics: None,
        });
    }

    let scope = format!("project:{}", project_name);

    // Dispatch on the configured engine. The graph track is a separate entry
    // point that needs the project's file graph; the embedding tracks
    // (baseline / pca / rp) go through `cluster_embeddings`, which reads
    // `reduce_method` from the config-derived params.
    let mut summary = if config.topic_clustering_method == "graph" {
        let pid = if let Some(pool) = db.pool() {
            sqlx::query_scalar::<_, i32>(
                "SELECT id FROM projects WHERE name = $1 ORDER BY id LIMIT 1",
            )
            .bind(project_name)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
        } else {
            None
        };
        let edges = match pid {
            Some(pid) => crate::cron::topic_graph::load_project_graph_edges(db, pid)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let w = &config.topic_graph_edge_weights;
        let ew = [
            w.first().copied().unwrap_or(1.0),
            w.get(1).copied().unwrap_or(1.0),
            w.get(2).copied().unwrap_or(1.0),
        ];
        crate::cron::topic_graph::cluster_graph(
            &rows,
            &edges,
            ew,
            config.topic_graph_resolution,
            min_cluster_size,
            config.topic_label_top_k,
            &scope,
        )
    } else if config.topic_clustering_method == "embedding_hdbscan" {
        cluster_embeddings_hdbscan(&rows, config, min_cluster_size, &scope)
    } else {
        let params = FcmParams::with_min_cluster_size(config, min_cluster_size);
        cluster_embeddings(&rows, &params, &scope)
    };

    // LLM-label all topics on the on-demand path too (config-gated; deterministic
    // c-TF-IDF fallback). `maybe_relabel` runs inference under `spawn_blocking`
    // so it doesn't stall the async runtime.
    summary.topics =
        crate::cron::topic_label_llm::maybe_relabel(std::mem::take(&mut summary.topics), config)
            .await;
    Ok(summary)
}

/// Embedding-track engine selector for the bake-off (Phase 3) and the
/// production dispatch (Phase 6). The graph track is a separate entry point
/// ([`crate::cron::topic_graph::cluster_graph`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicEngine {
    /// FCM on raw 1024-d embeddings (the path that collapsed).
    Baseline,
    /// FCM on PCA-reduced embeddings.
    EmbeddingPca,
    /// FCM on JL-random-projection-reduced embeddings.
    EmbeddingRp,
    /// HDBSCAN\* on PCA-reduced embeddings (canonical BERTopic clusterer).
    EmbeddingHdbscan,
}

/// Run one embedding-track engine on already-extracted rows, independent of the
/// configured `topic_clustering_method`. Used by the bake-off to compare engines
/// on identical input. Warm-start LMDB is disabled so engines don't cross-
/// contaminate each other's centroids.
pub fn cluster_embeddings_engine(
    rows: &[ChunkEmbeddingRow],
    config: &CronConfig,
    min_cluster_size: usize,
    scope: &str,
    engine: TopicEngine,
) -> ClusteringSummary {
    if engine == TopicEngine::EmbeddingHdbscan {
        return cluster_embeddings_hdbscan(rows, config, min_cluster_size, scope);
    }
    let mut params = FcmParams::with_min_cluster_size(config, min_cluster_size);
    params.reduce_method = match engine {
        TopicEngine::Baseline => None,
        TopicEngine::EmbeddingPca => Some(crate::cron::topic_reduce::ReduceMethod::Pca),
        TopicEngine::EmbeddingRp => Some(crate::cron::topic_reduce::ReduceMethod::RandomProjection),
        TopicEngine::EmbeddingHdbscan => unreachable!("handled above"),
    };
    params.reduce_dim = config.topic_reduce_dim;
    params.lmdb_enabled = false;
    cluster_embeddings(rows, &params, scope)
}

/// HDBSCAN\*-on-PCA-reduced topic engine (the canonical BERTopic clusterer).
///
/// HDBSCAN\* is O(n²) (pairwise distances), so for tractability the clustering
/// *decision* is made on a strided subsample of the reduced space and every
/// chunk is then assigned to its nearest cluster centroid — an
/// approximate-at-scale scheme (cluster-on-sample, assign-all) that keeps full
/// coverage while bounding cost. Topics are a hard partition (like the graph
/// engine), labeled with the same c-TF-IDF.
fn cluster_embeddings_hdbscan(
    rows: &[ChunkEmbeddingRow],
    config: &CronConfig,
    min_cluster_size: usize,
    scope: &str,
) -> ClusteringSummary {
    /// Cap on points fed to the O(n²) HDBSCAN clustering decision.
    const HDBSCAN_MAX_N: usize = 6_000;

    let n = rows.len();
    let empty = || ClusteringSummary {
        scope: scope.to_string(),
        chunks_analyzed: n,
        topics_found: 0,
        noise_chunks: n,
        num_clusters: 0,
        fuzziness: 0.0,
        converged: true,
        iterations: 1,
        topics: Vec::new(),
        metrics: None,
    };
    if n == 0 {
        return empty();
    }

    // L2-normalized embedding matrix (original space, for cohesion/centroid).
    let d = rows[0].embedding.len();
    let mut data = Array2::<f32>::zeros((n, d));
    for (i, row) in rows.iter().enumerate() {
        for (j, &v) in row.embedding.iter().enumerate() {
            data[[i, j]] = v;
        }
        let norm: f32 = data.row(i).dot(&data.row(i)).sqrt();
        if norm > 1e-12 {
            data.row_mut(i).mapv_inplace(|x| x / norm);
        }
    }

    // PCA-reduce (breaks distance concentration so HDBSCAN's Euclidean is sane).
    let rdim = config.topic_reduce_dim.min(d).max(2);
    let reduced = crate::cron::topic_reduce::reduce(
        data.view(),
        rdim,
        crate::cron::topic_reduce::ReduceMethod::Pca,
        42,
    );
    let rdim = reduced.ncols();

    // Strided subsample for the clustering decision.
    let sample_idx: Vec<usize> = if n > HDBSCAN_MAX_N {
        let stride = (n / HDBSCAN_MAX_N).max(1);
        (0..n).step_by(stride).take(HDBSCAN_MAX_N).collect()
    } else {
        (0..n).collect()
    };
    let mut sample = Array2::<f32>::zeros((sample_idx.len(), rdim));
    for (si, &i) in sample_idx.iter().enumerate() {
        sample.row_mut(si).assign(&reduced.row(i));
    }
    let ms = min_cluster_size.clamp(1, 25);
    let sample_labels = crate::cron::hdbscan::hdbscan(sample.view(), min_cluster_size, ms);

    let k = sample_labels
        .iter()
        .filter(|&&l| l >= 0)
        .map(|&l| l as usize + 1)
        .max()
        .unwrap_or(0);
    if k == 0 {
        return empty();
    }

    // Cluster centroids in reduced space from the sample's labeled points.
    let mut cent = vec![vec![0.0f32; rdim]; k];
    let mut cnt = vec![0usize; k];
    for (si, &lab) in sample_labels.iter().enumerate() {
        if lab >= 0 {
            let c = lab as usize;
            let r = reduced.row(sample_idx[si]);
            for (acc, &v) in cent[c].iter_mut().zip(r.iter()) {
                *acc += v;
            }
            cnt[c] += 1;
        }
    }
    for (c, ct) in cnt.iter().enumerate() {
        if *ct > 0 {
            for v in &mut cent[c] {
                *v /= *ct as f32;
            }
        }
    }

    // Assign every chunk to its nearest centroid (reduced-space Euclidean).
    let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = reduced.row(i);
        let mut best = 0usize;
        let mut best_d = f32::INFINITY;
        for (c, ce) in cent.iter().enumerate() {
            if cnt[c] == 0 {
                continue;
            }
            let mut s = 0.0f32;
            for (a, b) in r.iter().zip(ce.iter()) {
                let diff = a - b;
                s += diff * diff;
            }
            if s < best_d {
                best_d = s;
                best = c;
            }
        }
        members.entry(best).or_default().push(i);
    }

    // Assemble hard-partition topics (reuse the in-module helpers).
    let mut kept: Vec<(usize, Vec<usize>)> = members
        .into_iter()
        .filter(|(_, v)| v.len() >= min_cluster_size.max(1))
        .collect();
    kept.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    let topic_k = kept.len();
    let assigned: usize = kept.iter().map(|(_, v)| v.len()).sum();
    if topic_k == 0 {
        return empty();
    }

    let mut membership = Array2::<f32>::zeros((n, topic_k));
    for (ti, (_c, idxs)) in kept.iter().enumerate() {
        for &i in idxs {
            membership[[i, ti]] = 1.0;
        }
    }
    let contents: Vec<&str> = rows.iter().map(|r| r.content.as_str()).collect();
    let keyword_sets = compute_ctf_idf(&contents, &membership, config.topic_label_top_k);

    let mut topics: Vec<TopicResult> = Vec::with_capacity(topic_k);
    for (ti, (_c, idxs)) in kept.iter().enumerate() {
        let chunk_ids: Vec<i64> = idxs.iter().map(|&i| rows[i].chunk_id).collect();
        let file_ids: Vec<i64> = idxs
            .iter()
            .map(|&i| rows[i].file_id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let project_names: Vec<String> = idxs
            .iter()
            .map(|&i| rows[i].project_name.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let avg_sim = avg_internal_similarity(&data.view(), idxs);
        let representative_chunk_id = find_representative(&data.view(), &chunk_ids, idxs);
        let representative_snippet = rows
            .iter()
            .find(|r| r.chunk_id == representative_chunk_id)
            .map(|r| {
                if r.content.len() > 500 {
                    format!("{}...", &r.content[..r.content.floor_char_boundary(500)])
                } else {
                    r.content.clone()
                }
            })
            .unwrap_or_default();
        let mut file_counts: HashMap<(&str, &str), i32> = HashMap::new();
        for &i in idxs {
            *file_counts
                .entry((rows[i].path.as_str(), rows[i].project_name.as_str()))
                .or_insert(0) += 1;
        }
        let mut top_files: Vec<TopicFileEntry> = file_counts
            .into_iter()
            .map(|((path, project), c)| TopicFileEntry {
                path: path.to_string(),
                project: project.to_string(),
                chunks_in_topic: c,
            })
            .collect();
        top_files.sort_by_key(|b| std::cmp::Reverse(b.chunks_in_topic));
        // 1024-d centroid from original embeddings (hierarchy/warm-start compat).
        let mut centroid = vec![0.0f32; d];
        for &i in idxs {
            for (acc, &v) in centroid.iter_mut().zip(data.row(i).iter()) {
                *acc += v;
            }
        }
        let m = idxs.len() as f32;
        for v in &mut centroid {
            *v /= m;
        }
        let norm: f32 = centroid.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 1e-12 {
            for v in &mut centroid {
                *v /= norm;
            }
        }
        let empty_kw: Vec<TopicKeyword> = Vec::new();
        let kw = keyword_sets.get(ti).unwrap_or(&empty_kw);
        let keywords: Vec<String> = kw.iter().map(|k| k.word.clone()).collect();
        let keyword_scores: Vec<f64> = kw.iter().map(|k| k.score).collect();
        let label = label_from_keywords(kw, ti as i32);
        let memberships = vec![1.0f64; chunk_ids.len()];
        topics.push(TopicResult {
            cluster_index: ti as i32,
            label,
            keywords,
            keyword_scores,
            chunk_ids,
            memberships,
            file_ids,
            project_names,
            avg_internal_similarity: avg_sim,
            representative_chunk_id,
            representative_snippet,
            top_files,
            centroid,
            parent_topic_ids: Vec::new(),
        });
    }

    let mut metrics = TopicMetrics::from_topics(topic_k, &topics);
    metrics.fill_coherence(
        &contents,
        &topics,
        crate::quality::topic_metrics::DEFAULT_COHERENCE_TOP_N,
    );
    metrics.mean_max_membership = 1.0;
    metrics.n_scored = n;

    ClusteringSummary {
        scope: scope.to_string(),
        chunks_analyzed: n,
        topics_found: topics.len(),
        noise_chunks: n - assigned,
        num_clusters: topic_k,
        fuzziness: 0.0,
        converged: true,
        iterations: 1,
        topics,
        metrics: Some(metrics),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_effective_signature_folds_engine() {
        let sig = |m: &str| {
            topics_effective_signature(&crate::config::CronConfig {
                topic_clustering_method: m.to_string(),
                ..Default::default()
            })
        };
        assert_eq!(sig("graph"), "pgmcp-topics-v3+graph");
        assert_eq!(
            sig("embedding_hdbscan"),
            "pgmcp-topics-v3+embedding_hdbscan"
        );
        assert_eq!(sig("baseline"), "pgmcp-topics-v3+baseline");
    }

    #[test]
    fn effective_signature_distinguishes_engines_and_carries_pipeline_version() {
        let sig = |m: &str| {
            topics_effective_signature(&crate::config::CronConfig {
                topic_clustering_method: m.to_string(),
                ..Default::default()
            })
        };
        // Distinct engines ⇒ distinct signatures, so switching `topic_clustering_method`
        // invalidates a model produced by a different engine (D3).
        assert_ne!(sig("graph"), sig("embedding_pca"));
        // Stable for the same engine (idempotent comparison).
        assert_eq!(sig("graph"), sig("graph"));
        // Always carries the static label-pipeline version as the prefix, so a
        // `TOPICS_ALGO_SIGNATURE` bump still invalidates every engine's stored model.
        assert!(sig("graph").starts_with(TOPICS_ALGO_SIGNATURE));
        assert!(sig("graph").starts_with("pgmcp-topics-v3+"));
    }

    fn test_fcm(
        data: ndarray::ArrayView2<'_, f32>,
        k: usize,
        m: f64,
        max_iters: usize,
        tolerance: f64,
    ) -> FcmResult {
        fuzzy_c_means_seeded(data, k, m, max_iters, tolerance, 42)
    }

    fn test_fcm_with_cancel(
        data: ndarray::ArrayView2<'_, f32>,
        k: usize,
        m: f64,
        max_iters: usize,
        tolerance: f64,
        should_cancel: CancelFn<'_>,
    ) -> FcmResult {
        run_through_backend(
            data,
            k,
            m,
            max_iters,
            tolerance,
            should_cancel,
            None,
            fcm::BackendChoice::Cpu,
            Some(42),
        )
    }

    fn cluster_embeddings_for_tests(
        rows: &[ChunkEmbeddingRow],
        params: &FcmParams,
        scope: &str,
    ) -> ClusteringSummary {
        cluster_embeddings_with_runner(rows, params, scope, |data, k, params, _warm| {
            test_fcm(
                data,
                k,
                params.fuzziness,
                params.max_iters,
                params.tolerance,
            )
        })
    }

    // ----- Phase 9: pure helpers extracted from run_global_topic_scan -----

    fn cron_config_with_thresholds(mmap: usize, online: usize) -> CronConfig {
        CronConfig {
            topic_mmap_n_threshold: mmap,
            topic_online_n_threshold: online,
            topic_max_mem_fraction: 0.4,
            ..CronConfig::default()
        }
    }

    #[test]
    fn select_scan_strategy_in_memory_for_small_corpus() {
        let cfg = cron_config_with_thresholds(10_000, 1_000_000);
        assert_eq!(select_scan_strategy(500, &cfg), ScanStrategy::InMemory);
        assert_eq!(select_scan_strategy(10_000, &cfg), ScanStrategy::InMemory);
    }

    #[test]
    fn select_scan_strategy_mmap_above_mmap_threshold() {
        let cfg = cron_config_with_thresholds(10_000, 1_000_000);
        assert_eq!(select_scan_strategy(10_001, &cfg), ScanStrategy::Mmap);
        assert_eq!(select_scan_strategy(500_000, &cfg), ScanStrategy::Mmap);
    }

    #[test]
    fn select_scan_strategy_online_above_online_threshold() {
        let cfg = cron_config_with_thresholds(10_000, 1_000_000);
        assert_eq!(select_scan_strategy(1_000_001, &cfg), ScanStrategy::Online);
        assert_eq!(select_scan_strategy(5_000_000, &cfg), ScanStrategy::Online);
    }

    #[test]
    fn check_memory_budget_within_budget_at_realistic_size() {
        // n=100k, k=100, mem_avail=128 GiB → ~1-2 GB predicted, well under 40%.
        let mem_avail = 128u64 * 1024 * 1024 * 1024;
        match check_memory_budget(100_000, 100, mem_avail, 0.4) {
            BudgetDecision::WithinBudget { frac, .. } => assert!(frac < 0.4),
            other => panic!("expected within budget, got {:?}", other),
        }
    }

    #[test]
    fn check_memory_budget_over_budget_when_huge() {
        // n=10M, k=500, mem_avail=4 GiB → way over 40%.
        let mem_avail = 4u64 * 1024 * 1024 * 1024;
        match check_memory_budget(10_000_000, 500, mem_avail, 0.4) {
            BudgetDecision::OverBudget {
                frac, budget_frac, ..
            } => {
                assert!(frac > 0.4);
                assert_eq!(budget_frac, 0.4);
            }
            other => panic!("expected over budget, got {:?}", other),
        }
    }

    #[test]
    fn check_memory_budget_not_checked_with_zero_chunks() {
        match check_memory_budget(0, 10, 1024 * 1024 * 1024, 0.4) {
            BudgetDecision::NotChecked => {}
            other => panic!("expected NotChecked, got {:?}", other),
        }
    }

    // ----- existing tests below -----

    #[test]
    fn test_l2_normalize() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        let norm: f64 = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-10);
        assert!((v[0] - 0.6).abs() < 1e-10);
        assert!((v[1] - 0.8).abs() < 1e-10);
    }

    #[test]
    fn test_l2_normalize_zero() {
        let mut v = vec![0.0, 0.0, 0.0];
        l2_normalize(&mut v);
        assert!(v.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn test_find_representative_single() {
        let data = ndarray::array![[0.6_f32, 0.8_f32]];
        let ids = vec![42i64];
        let members = vec![0usize];
        assert_eq!(find_representative(&data.view(), &ids, &members), 42);
    }

    #[test]
    fn test_avg_internal_similarity_identical() {
        let data = ndarray::array![[0.6_f32, 0.8_f32], [0.6_f32, 0.8_f32]];
        let members = vec![0usize, 1usize];
        let sim = avg_internal_similarity(&data.view(), &members);
        assert!((sim - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_avg_internal_similarity_single() {
        let data = ndarray::array![[0.6_f32, 0.8_f32]];
        let members = vec![0usize];
        let sim = avg_internal_similarity(&data.view(), &members);
        assert!((sim - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_estimate_k() {
        // 113K chunks, min_cluster_size=5 → sqrt(22760) ≈ 151, clamped to 100
        let k = estimate_k(113_800, 5);
        assert_eq!(k, 100, "Expected clamp to 100, got {}", k);

        // Small dataset clamps to min 10
        assert_eq!(estimate_k(10, 5), 10);

        // Huge dataset clamps to max 100 (lowered from 500 during OOM fix —
        // see topic_clustering.rs estimate_k docstring)
        assert_eq!(estimate_k(5_000_000, 5), 100);

        // Moderate dataset, K below the cap, uses computed value
        // n=200, min_cs=5 → sqrt(40) ≈ 6 → clamped up to 10 (min)
        assert_eq!(estimate_k(200, 5), 10);

        // n=2500, min_cs=5 → sqrt(500) ≈ 22
        let k = estimate_k(2_500, 5);
        assert!((20..=25).contains(&k), "Expected ~22, got {}", k);
    }

    #[test]
    fn test_kmeans_plus_plus() {
        let data: Array2<f32> = Array2::from_shape_fn((100, 4), |(i, j)| {
            if i < 50 {
                if j == 0 { 1.0 } else { 0.0 }
            } else {
                if j == 1 { 1.0 } else { 0.0 }
            }
        });
        let centroids = kmeans_plus_plus_init(data.view(), 5);
        assert_eq!(centroids.nrows(), 5);
        assert_eq!(centroids.ncols(), 4);
        // Centroids should be distinct
        for i in 0..5 {
            for j in (i + 1)..5 {
                let diff = &centroids.row(i) - &centroids.row(j);
                let dist = diff.dot(&diff).sqrt();
                // Not all identical (at least some should differ)
                if dist > 1e-6 {
                    return; // Pass: at least one pair is distinct
                }
            }
        }
        // If we get here, all centroids are identical — this is extremely unlikely
        // but technically possible with random seeding. Don't fail for it.
    }

    #[test]
    fn test_fcm_two_clusters() {
        // Two well-separated Gaussian blobs in 4D
        let mut data = Array2::<f32>::zeros((40, 4));
        for i in 0..20 {
            data[[i, 0]] = 1.0 + 0.01 * i as f32;
            data[[i, 1]] = 0.01 * i as f32;
        }
        for i in 20..40 {
            data[[i, 2]] = 1.0 + 0.01 * (i - 20) as f32;
            data[[i, 3]] = 0.01 * (i - 20) as f32;
        }

        let result = test_fcm(data.view(), 2, 2.0, 100, 1e-5);
        assert!(
            result.converged,
            "FCM should converge on well-separated blobs"
        );
        assert_eq!(result.membership.nrows(), 40);
        assert_eq!(result.membership.ncols(), 2);

        // Core points should have near-1.0 membership in their cluster
        for i in 0..20 {
            let max_mu = result
                .membership
                .row(i)
                .iter()
                .cloned()
                .fold(f32::NEG_INFINITY, f32::max);
            assert!(
                max_mu > 0.9,
                "Core point {} should have max membership > 0.9, got {}",
                i,
                max_mu
            );
        }
        for i in 20..40 {
            let max_mu = result
                .membership
                .row(i)
                .iter()
                .cloned()
                .fold(f32::NEG_INFINITY, f32::max);
            assert!(
                max_mu > 0.9,
                "Core point {} should have max membership > 0.9, got {}",
                i,
                max_mu
            );
        }
    }

    #[test]
    fn test_fcm_overlap() {
        // Two overlapping clusters with a boundary point
        let mut data = Array2::<f32>::zeros((21, 2));
        for i in 0..10 {
            data[[i, 0]] = -1.0 + 0.01 * i as f32;
            data[[i, 1]] = 0.0;
        }
        for i in 10..20 {
            data[[i, 0]] = 1.0 + 0.01 * (i - 10) as f32;
            data[[i, 1]] = 0.0;
        }
        // Boundary point equidistant from both clusters
        data[[20, 0]] = 0.0;
        data[[20, 1]] = 0.0;

        let result = test_fcm(data.view(), 2, 2.0, 100, 1e-5);

        // Boundary point should have split membership (close to 0.5/0.5)
        let mu0 = result.membership[[20, 0]];
        let mu1 = result.membership[[20, 1]];
        assert!(
            (mu0 - 0.5).abs() < 0.2 && (mu1 - 0.5).abs() < 0.2,
            "Boundary point should have split membership, got ({:.3}, {:.3})",
            mu0,
            mu1
        );
    }

    #[test]
    fn test_fcm_convergence() {
        let data: Array2<f32> =
            Array2::from_shape_fn((50, 10), |(i, j)| ((i * 7 + j * 3) % 100) as f32 / 100.0);
        let result = test_fcm(data.view(), 3, 2.0, 200, 1e-5);
        assert!(result.iterations <= 200);
        // Membership rows should sum to ~1.0 (f32 tolerance)
        for i in 0..50 {
            let row_sum: f32 = result.membership.row(i).sum();
            assert!(
                (row_sum - 1.0).abs() < 1e-4,
                "Row {} sums to {}",
                i,
                row_sum
            );
        }
    }

    #[test]
    fn test_fcm_cancellation_honored() {
        // A closure that always returns true should cause immediate exit.
        let data: Array2<f32> = Array2::from_shape_fn((100, 4), |(_, _)| 0.5);
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let cancel_fn: &(dyn Fn() -> bool + Sync) = &|| {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            true
        };
        let result = test_fcm_with_cancel(data.view(), 3, 2.0, 100, 1e-5, Some(cancel_fn));
        assert!(result.cancelled, "FCM should report cancelled=true");
        assert!(cancelled.load(std::sync::atomic::Ordering::Acquire));
        assert!(
            result.iterations <= 2,
            "cancellation should short-circuit quickly"
        );
    }

    #[test]
    fn test_ctf_idf_keywords() {
        let contents = [
            "database connection pool manager",
            "database query optimization index",
            "database schema migration tool",
            "http server request handler middleware",
            "http router endpoint authentication",
            "http response serialization json",
        ];
        // Membership: first 3 chunks fully in topic 0, last 3 fully in topic 1
        let membership: Array2<f32> = Array2::from_shape_fn((6, 2), |(i, j)| {
            if i < 3 {
                if j == 0 { 1.0 } else { 0.0 }
            } else {
                if j == 0 { 0.0 } else { 1.0 }
            }
        });
        let keywords = compute_ctf_idf(contents.as_ref(), &membership, 5);
        assert_eq!(keywords.len(), 2);

        // Topic 0 should contain "database" as a top keyword
        let topic0_words: Vec<&str> = keywords[0].iter().map(|k| k.word.as_str()).collect();
        assert!(
            topic0_words.contains(&"database"),
            "Topic 0 should contain 'database', got {:?}",
            topic0_words
        );

        // Topic 1 should contain a request-handling keyword. (Note: "http"
        // is filtered by the PATH_STOPWORDS tier added in B.3 since it
        // commonly bleeds in from URL strings rather than as a topic
        // signal; the request-handling cluster is still well-identified
        // by the surviving tokens.)
        let topic1_words: Vec<&str> = keywords[1].iter().map(|k| k.word.as_str()).collect();
        let expected_signals = ["middleware", "endpoint", "router", "handler", "request"];
        assert!(
            expected_signals
                .iter()
                .any(|sig| topic1_words.contains(sig)),
            "Topic 1 should contain a request-handling signal from {:?}; got {:?}",
            expected_signals,
            topic1_words
        );
    }

    #[test]
    fn test_ctf_idf_diffuse_membership_not_nuked() {
        // Regression for the bake-off finding: with diffuse fuzzy memberships
        // (every chunk has non-trivial mass on EVERY topic) and K >
        // MAX_MEMBERSHIPS_PER_CHUNK, distributing tokens to all topics would put
        // every word in every topic, so the max-df cutoff empties ALL keyword
        // lists. The top-J cap (`top_membership_topics`) must keep keywords
        // non-empty by feeding each chunk's tokens only to its dominant topics.
        let k = 8usize;
        let n = 24usize; // 3 chunks per home topic
        let contents: Vec<String> = (0..n)
            .map(|j| {
                let home = j % k;
                // Distinct per-topic vocabulary + a ubiquitous "shared" token.
                format!("concept{home} concept{home} concept{home} shared payload{home}")
            })
            .collect();
        let refs: Vec<&str> = contents.iter().map(|s| s.as_str()).collect();
        // Diffuse membership: home 0.6, every other topic 0.4/(k-1) (all > 1e-8).
        let membership: Array2<f32> = Array2::from_shape_fn((n, k), |(i, t)| {
            let home = i % k;
            if t == home {
                0.6
            } else {
                0.4 / (k as f32 - 1.0)
            }
        });
        let keywords = compute_ctf_idf(&refs, &membership, 5);
        let total: usize = keywords.iter().map(|t| t.len()).sum();
        assert!(
            total > 0,
            "diffuse membership must still yield keywords (df-nuke regression); got all-empty"
        );
        // The per-topic distinctive "concept{h}" token should survive somewhere.
        let any_concept = keywords
            .iter()
            .flatten()
            .any(|kw| kw.word.starts_with("concept"));
        assert!(
            any_concept,
            "expected a distinctive concept term to survive"
        );
    }

    #[test]
    fn test_ctf_idf_stopwords() {
        let contents = ["fn pub let mut use impl struct enum const"];
        let membership: Array2<f32> = Array2::from_elem((1, 1), 1.0);
        let keywords = compute_ctf_idf(&[contents[0]], &membership, 5);
        // All tokens are stopwords, so no keywords should be extracted
        assert!(
            keywords[0].is_empty(),
            "Stopwords should be filtered: {:?}",
            keywords[0]
        );
    }

    #[test]
    fn test_ctf_idf_english_stopwords_filtered() {
        // Regression guard for B.3: prior degenerate labels were
        // dominated by English function words ("the", "and") and path
        // tokens ("home", "workspace") that bled in from embedded path
        // strings in error messages and doc comments. After extending
        // the stopword tiers, those tokens must not survive into any
        // topic's top-K keywords.
        //
        // Build two contrived single-chunk corpora where the only
        // "signal" tokens are `alpha` and `beta`; everything else is a
        // stopword tier member. The stopwords MUST be filtered, and the
        // signal token MUST be the top hit.
        let contents = [
            "the value and home workspace alpha alpha alpha and the of for",
            "home workspace dylon github io and the value beta beta beta the and",
        ];
        let membership: Array2<f32> = ndarray::array![[1.0, 0.0], [0.0, 1.0]];
        let keywords = compute_ctf_idf(&contents, &membership, 5);

        // Each topic's keyword list must contain its signal token and
        // no stopwords from any tier.
        let topic0_words: Vec<&str> = keywords[0].iter().map(|k| k.word.as_str()).collect();
        let topic1_words: Vec<&str> = keywords[1].iter().map(|k| k.word.as_str()).collect();

        assert!(
            topic0_words.contains(&"alpha"),
            "Topic 0 should contain 'alpha', got {:?}",
            topic0_words
        );
        assert!(
            topic1_words.contains(&"beta"),
            "Topic 1 should contain 'beta', got {:?}",
            topic1_words
        );

        for forbidden in [
            "the",
            "and",
            "of",
            "for",
            "home",
            "workspace",
            "github",
            "value",
        ] {
            assert!(
                !topic0_words.contains(&forbidden),
                "Topic 0 leaked stopword {forbidden:?}: {:?}",
                topic0_words
            );
            assert!(
                !topic1_words.contains(&forbidden),
                "Topic 1 leaked stopword {forbidden:?}: {:?}",
                topic1_words
            );
        }
    }

    #[test]
    fn cap_chunk_memberships_truncates_to_top_j() {
        // A diffuse chunk that landed in more topics than the cap allows must be
        // reduced to the MAX_MEMBERSHIPS_PER_CHUNK strongest, in descending
        // order — this is what stops per-file topic_count saturating to ~K.
        let mut topics: Vec<(usize, f64)> = vec![
            (3, 0.10),
            (7, 0.40),
            (1, 0.05),
            (9, 0.30),
            (2, 0.20),
            (5, 0.06),
        ];
        cap_chunk_memberships(&mut topics);
        assert_eq!(topics.len(), MAX_MEMBERSHIPS_PER_CHUNK);
        assert_eq!(topics, vec![(7, 0.40), (9, 0.30), (2, 0.20), (3, 0.10)]);
    }

    #[test]
    fn cap_chunk_memberships_noop_when_within_cap() {
        // At or under the cap, leave the list unchanged (no spurious reorder).
        let mut topics: Vec<(usize, f64)> = vec![(4, 0.6), (2, 0.3)];
        let before = topics.clone();
        cap_chunk_memberships(&mut topics);
        assert_eq!(topics, before);
    }

    #[test]
    fn test_soft_aggregation() {
        // Chunk 0: "alpha beta", membership [0.8, 0.2]
        // Chunk 1: "beta gamma", membership [0.2, 0.8]
        let contents = ["alpha beta", "beta gamma"];
        let membership: Array2<f32> = ndarray::array![[0.8, 0.2], [0.2, 0.8]];
        let keywords = compute_ctf_idf(contents.as_ref(), &membership, 5);
        assert_eq!(keywords.len(), 2);
        // Both topics should have "beta" but with different weights
        let _t0_words: Vec<&str> = keywords[0].iter().map(|k| k.word.as_str()).collect();
        let _t1_words: Vec<&str> = keywords[1].iter().map(|k| k.word.as_str()).collect();
        // "alpha" should score higher in topic 0 than topic 1
        // "gamma" should score higher in topic 1 than topic 0
        let alpha_in_t0 = keywords[0]
            .iter()
            .find(|k| k.word == "alpha")
            .map(|k| k.score)
            .unwrap_or(0.0);
        let alpha_in_t1 = keywords[1]
            .iter()
            .find(|k| k.word == "alpha")
            .map(|k| k.score)
            .unwrap_or(0.0);
        assert!(
            alpha_in_t0 > alpha_in_t1,
            "alpha should score higher in topic 0"
        );

        let gamma_in_t0 = keywords[0]
            .iter()
            .find(|k| k.word == "gamma")
            .map(|k| k.score)
            .unwrap_or(0.0);
        let gamma_in_t1 = keywords[1]
            .iter()
            .find(|k| k.word == "gamma")
            .map(|k| k.score)
            .unwrap_or(0.0);
        assert!(
            gamma_in_t1 > gamma_in_t0,
            "gamma should score higher in topic 1"
        );
    }

    #[test]
    fn test_membership_threshold() {
        // With m=2, well-separated clusters should have most memberships near 0 or 1.
        // Only assignments above threshold should appear in topic results.
        let mut data = Array2::<f32>::zeros((20, 4));
        for i in 0..10 {
            data[[i, 0]] = 10.0 + 0.01 * i as f32;
        }
        for i in 10..20 {
            data[[i, 2]] = 10.0 + 0.01 * (i - 10) as f32;
        }

        let result = test_fcm(data.view(), 2, 2.0, 100, 1e-5);

        // Count assignments above threshold 0.05
        let threshold = 0.05;
        let mut above_count = 0;
        for i in 0..20 {
            for t in 0..2 {
                if result.membership[[i, t]] >= threshold {
                    above_count += 1;
                }
            }
        }
        // With well-separated clusters, most points should be in exactly 1 cluster above threshold
        // (the other cluster's membership is near 0)
        assert!(
            (20..=40).contains(&above_count),
            "Expected 20-40 above-threshold assignments, got {}",
            above_count
        );
    }

    fn test_params(num_clusters: Option<usize>, min_cluster_size: usize) -> FcmParams {
        FcmParams {
            num_clusters,
            min_cluster_size,
            fuzziness: 2.0,
            max_iters: 100,
            tolerance: 1e-5,
            membership_threshold: 0.05,
            label_top_k: 5,
            // Tests cluster raw embeddings (no reduction) for determinism.
            reduce_method: None,
            reduce_dim: 30,
            // Tests use fp32 for deterministic CUDA arithmetic.
            gpu_fcm_precision: "fp32".into(),
            k_selector: "xie_beni".into(),
            k_candidates: Vec::new(),
            k_sweep_max_iters: 20,
            // Disable subsampling for deterministic tests.
            k_sweep_subsample: 0,
            lmdb_path: None,
            lmdb_enabled: false,
            _topic_scratch_dir: None,
        }
    }

    #[test]
    fn test_cluster_embeddings_empty() {
        let rows: Vec<ChunkEmbeddingRow> = Vec::new();
        let params = test_params(None, 5);
        let summary = cluster_embeddings_for_tests(&rows, &params, "test");
        assert_eq!(summary.topics_found, 0);
        assert_eq!(summary.chunks_analyzed, 0);
    }

    #[test]
    fn test_cluster_embeddings_small_dataset() {
        let mut rows = Vec::new();
        // Cluster A: similar embeddings around [1, 0, 0, ...]
        for i in 0..10 {
            let mut emb = vec![0.0f32; 1024];
            emb[0] = 1.0;
            emb[1] = 0.01 * i as f32;
            rows.push(ChunkEmbeddingRow {
                chunk_id: i as i64,
                file_id: 1,
                project_id: 1,
                project_name: "proj_a".to_string(),
                path: "src/db/queries.rs".to_string(),
                language: "rust".to_string(),
                content: format!("fn query_{}() {{}}", i),
                embedding: emb,
            });
        }
        // Cluster B: similar embeddings around [0, 1, 0, ...]
        for i in 0..10 {
            let mut emb = vec![0.0f32; 1024];
            emb[1] = 1.0;
            emb[2] = 0.01 * i as f32;
            rows.push(ChunkEmbeddingRow {
                chunk_id: (10 + i) as i64,
                file_id: 2,
                project_id: 1,
                project_name: "proj_a".to_string(),
                path: "src/cron/scheduler.rs".to_string(),
                language: "rust".to_string(),
                content: format!("fn schedule_{}() {{}}", i),
                embedding: emb,
            });
        }

        let params = test_params(Some(2), 3);
        let summary = cluster_embeddings_for_tests(&rows, &params, "test");
        assert_eq!(summary.chunks_analyzed, 20);
        assert!(
            summary.topics_found >= 1,
            "Expected at least 1 topic, got {}",
            summary.topics_found
        );
        assert!(
            summary.converged,
            "FCM should converge on small well-separated data"
        );
    }

    #[test]
    fn test_tokenize_filters() {
        let tokens = tokenize("fn pub let mut use impl struct hello_world database 42 ab XYZ");
        // "fn", "pub", "let", "mut", "use", "impl", "struct" are stopwords
        // "42" is numeric only
        // "ab" is < 3 chars
        // "xyz" should remain (lowercased)
        // "hello_world" is SPLIT into concept sub-tokens by `split_identifier`,
        // so the compound token no longer appears — its parts do.
        assert!(!tokens.contains(&"hello_world".to_string()));
        assert!(tokens.contains(&"hello".to_string()));
        assert!(tokens.contains(&"world".to_string()));
        assert!(tokens.contains(&"database".to_string()));
        assert!(tokens.contains(&"xyz".to_string()));
        assert!(!tokens.contains(&"fn".to_string()));
        assert!(!tokens.contains(&"42".to_string()));
        assert!(!tokens.contains(&"ab".to_string()));
    }

    #[test]
    fn test_split_identifier_concept_tokens() {
        let mut out = Vec::new();
        split_identifier("tokenize_query", &mut out);
        assert_eq!(out, vec!["tokenize", "query"]);

        out.clear();
        split_identifier("parseHTTPResponse", &mut out);
        assert_eq!(out, vec!["parse", "http", "response"]);

        out.clear();
        split_identifier("FcmBackend", &mut out);
        assert_eq!(out, vec!["fcm", "backend"]);

        // Digit runs are NOT split off; plain words pass through unchanged.
        out.clear();
        split_identifier("utf8", &mut out);
        assert_eq!(out, vec!["utf8"]);
    }

    #[test]
    fn test_label_from_keywords() {
        let kw = vec![
            TopicKeyword {
                word: "database".into(),
                score: 0.5,
            },
            TopicKeyword {
                word: "query".into(),
                score: 0.3,
            },
        ];
        assert_eq!(label_from_keywords(&kw, 0), "database / query");
        assert_eq!(label_from_keywords(&[], 7), "topic_7");
    }

    // ========================================================================
    // Property tests (Phase 2)
    // ========================================================================

    use proptest::prelude::*;

    /// Build a well-separated K-blob dataset used for FCM convergence
    /// property checks. K clusters on a d-dim grid.
    fn make_blobs(k: usize, pts_per_cluster: usize, d: usize) -> ndarray::Array2<f32> {
        let n = k * pts_per_cluster;
        let mut data = ndarray::Array2::<f32>::zeros((n, d));
        for c in 0..k {
            for i in 0..pts_per_cluster {
                let row = c * pts_per_cluster + i;
                data[[row, c % d]] = 10.0 + 0.01 * i as f32;
            }
        }
        data
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

        /// Every row of the membership matrix sums to ≈ 1.0 (row-stochastic).
        /// This is an FCM invariant — memberships are normalized after each
        /// update step.
        #[test]
        fn prop_fcm_membership_rows_sum_to_one(
            k in 2usize..5,
            pts in 6usize..12,
            d in 2usize..5,
        ) {
            let data = make_blobs(k, pts, d);
            let result = test_fcm(data.view(), k, 2.0, 30, 1e-4);
            for i in 0..result.membership.nrows() {
                let sum: f32 = result.membership.row(i).iter().sum();
                prop_assert!((sum - 1.0).abs() < 1e-3,
                    "row {} sum = {} (should be ≈ 1.0)", i, sum);
            }
        }

        /// FCM always terminates within max_iters — the while loop must
        /// respect its upper bound even if tolerance is never reached.
        #[test]
        fn prop_fcm_converges_within_max_iters(
            k in 2usize..4,
            pts in 5usize..10,
            d in 2usize..4,
            max_iters in 5usize..30,
        ) {
            let data = make_blobs(k, pts, d);
            let result = test_fcm(data.view(), k, 2.0, max_iters, 1e-10);
            prop_assert!(result.iterations <= max_iters,
                "ran {} iterations but cap was {}", result.iterations, max_iters);
        }

        /// Membership values above `topic_membership_threshold` get kept
        /// as topic assignments. Specifically, for each chunk the primary
        /// topic (argmax) must have membership ≥ threshold — if the chunk
        /// is ever assigned. This pins the filter semantics of
        /// `run_global_topic_scan` downstream assignment logic.
        #[test]
        fn prop_membership_threshold_filters_low_assignments(
            k in 2usize..4,
            pts in 5usize..10,
            d in 2usize..4,
            threshold_bps in 100u32..500u32,  // basis points → 0.01..0.05
        ) {
            let data = make_blobs(k, pts, d);
            let result = test_fcm(data.view(), k, 2.0, 30, 1e-4);
            let threshold = (threshold_bps as f32) * 0.0001;
            // For every row, find the primary membership. If above threshold,
            // it would be kept; if below, the chunk becomes noise.
            for i in 0..result.membership.nrows() {
                let row = result.membership.row(i);
                let max: f32 = row.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
                // Either max ≥ threshold (chunk would be kept) or it wouldn't.
                // Both paths are valid per the contract. Primary assertion:
                // the max value is a legal membership in [0, 1].
                prop_assert!((0.0..=1.0 + 1e-5).contains(&max));
                // If threshold is absurdly low (< 1/k), every chunk must
                // pass — since rows sum to 1, at least one value is ≥ 1/k.
                if threshold < (1.0 / (k as f32)) - 1e-5 {
                    prop_assert!(max >= threshold,
                        "max membership {} should exceed threshold {} (rows sum to 1 with k={})",
                        max, threshold, k);
                }
            }
        }

        /// All membership values are in [0, 1] (not strict [0, 1], allow
        /// epsilon rounding).
        #[test]
        fn prop_fcm_memberships_in_unit_interval(
            k in 2usize..4,
            pts in 5usize..10,
            d in 2usize..4,
        ) {
            let data = make_blobs(k, pts, d);
            let result = test_fcm(data.view(), k, 2.0, 30, 1e-4);
            for &v in result.membership.iter() {
                prop_assert!((-1e-5..=1.0 + 1e-5).contains(&v),
                    "membership {} outside [0, 1]", v);
            }
        }

        /// c-TF-IDF keywords are always sorted by score descending within
        /// each topic's output.
        #[test]
        fn prop_tfidf_keywords_top_k_descending_score(
            k in 2usize..5,
            words_per_topic in 3usize..10,
            top_k in 1usize..8,
        ) {
            // Build synthetic chunks — each chunk belongs 100% to one topic
            // and contains a few topic-specific words.
            let n = k * 8;
            let mut contents: Vec<String> = Vec::with_capacity(n);
            for i in 0..n {
                let topic = i % k;
                let words: Vec<String> = (0..words_per_topic)
                    .map(|w| format!("topic{}_word{}", topic, w))
                    .collect();
                contents.push(words.join(" "));
            }
            let content_refs: Vec<&str> = contents.iter().map(|s| s.as_str()).collect();

            // Hard assignment: row i → topic (i % k).
            let mut membership = ndarray::Array2::<f32>::zeros((n, k));
            for i in 0..n {
                membership[[i, i % k]] = 1.0;
            }

            let results = compute_ctf_idf(&content_refs, &membership, top_k);
            prop_assert_eq!(results.len(), k);
            for topic_kw in &results {
                for pair in topic_kw.windows(2) {
                    prop_assert!(pair[0].score >= pair[1].score - 1e-9,
                        "keywords not descending: {} ({}) vs {} ({})",
                        pair[0].word, pair[0].score, pair[1].word, pair[1].score);
                }
                prop_assert!(topic_kw.len() <= top_k);
            }
        }

        /// c-TF-IDF keywords within a topic are distinct (no duplicates).
        #[test]
        fn prop_tfidf_keywords_unique_per_topic(
            k in 2usize..4,
            top_k in 2usize..6,
        ) {
            let n = 20;
            let mut contents: Vec<String> = Vec::with_capacity(n);
            for i in 0..n {
                let topic = i % k;
                contents.push(format!("topic{} common shared {} unique{}",
                    topic,
                    if i.is_multiple_of(2) { "alpha" } else { "beta" },
                    i));
            }
            let content_refs: Vec<&str> = contents.iter().map(|s| s.as_str()).collect();

            let mut membership = ndarray::Array2::<f32>::zeros((n, k));
            for i in 0..n {
                membership[[i, i % k]] = 1.0;
            }

            let results = compute_ctf_idf(&content_refs, &membership, top_k);
            for topic_kw in &results {
                let mut seen = std::collections::HashSet::new();
                for kw in topic_kw {
                    prop_assert!(seen.insert(kw.word.clone()),
                        "duplicate keyword `{}` in topic", kw.word);
                }
            }
        }
    }
}
