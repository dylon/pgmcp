//! The evaluation-query vocabulary + the hand-authored known-item set.
//!
//! An [`EvalQuery`] pairs a natural-language query string with its labeled
//! [`GoldTarget`]s (the file(s) that genuinely answer it). The campaign runs
//! each query through every search mode and scores the returned ranking against
//! these gold labels with [`pgmcp::quality::retrieval_metrics`].

use pgmcp::quality::retrieval_metrics::GoldItem;
use serde::{Deserialize, Serialize};

/// Which ground-truth strategy produced a query. Recorded so the campaign can
/// stratify results (objective vs. leakage-controlled vs. realism) and so the
/// experiment ledger can attribute every number to its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryStrategy {
    /// **A** — hand-authored intent query against the live corpus; gold is a
    /// human-assigned file. Objective, small N, the human-validated anchor.
    KnownItem,
    /// **B-primary (M1)** — a doc-comment used as the query; gold is its code
    /// chunk, re-embedded with the doc-comment removed (exact leakage control).
    Docstring,
    /// **B M3** — like [`QueryStrategy::Docstring`] but with identifier tokens
    /// redacted from both query and chunk; isolates real semantics from
    /// identifier echo.
    DocstringRedacted,
    /// **B-realism (M2)** — a sentence drawn from beyond token 512 of a long
    /// prose chunk; the live embedding never saw it (leak-free by
    /// construction), full-corpus distractors.
    DocstringHoldout,
}

impl QueryStrategy {
    /// Stable lowercase tag for ledger arm labels and fixture grouping.
    pub fn tag(self) -> &'static str {
        match self {
            QueryStrategy::KnownItem => "known_item",
            QueryStrategy::Docstring => "docstring",
            QueryStrategy::DocstringRedacted => "docstring_redacted",
            QueryStrategy::DocstringHoldout => "docstring_holdout",
        }
    }
}

/// One labeled relevant target: a file (and optional line span) that answers a
/// query, with a graded relevance. `path` is the project-relative path, matched
/// against `SearchResult.relative_path`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldTarget {
    /// Project-relative path (matches `SearchResult.relative_path`).
    pub path: String,
    /// The project the path lives in (the search scope filter).
    pub project: String,
    /// Inclusive 1-indexed start line, or `None` for file-level gold.
    pub start_line: Option<i64>,
    /// Inclusive 1-indexed end line, or `None` for file-level gold.
    pub end_line: Option<i64>,
    /// Graded relevance (`1.0` binary, `{1,2,3}` graded).
    pub relevance: f64,
}

impl GoldTarget {
    /// A binary-relevant (gain 1.0) file-level gold target.
    pub fn file(project: &str, path: &str) -> Self {
        Self {
            path: path.to_string(),
            project: project.to_string(),
            start_line: None,
            end_line: None,
            relevance: 1.0,
        }
    }

    /// Convert to the metric crate's [`GoldItem`] (drops the project; matching
    /// is done after the search is already scoped to the project).
    pub fn to_gold_item(&self) -> GoldItem {
        GoldItem {
            path: self.path.clone(),
            start_line: self.start_line,
            end_line: self.end_line,
            relevance: self.relevance,
        }
    }
}

/// One evaluation query with its gold labels and provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalQuery {
    /// Stable identifier — the **unit key** that aligns this query across modes
    /// for the paired significance tests. Must be unique within a query set.
    pub id: String,
    /// How this query's ground truth was produced.
    pub strategy: QueryStrategy,
    /// The natural-language query text sent to the search tools.
    pub query: String,
    /// Project scope filter (e.g. `Some("pgmcp")`), or `None` for cross-project.
    pub project: Option<String>,
    /// The relevant target(s). At least one; the campaign skips empty-gold
    /// queries (their recall would be undefined).
    pub gold: Vec<GoldTarget>,
    /// Authoring rationale / paraphrase note (documentation; never scored).
    pub notes: Option<String>,
}

impl EvalQuery {
    /// The gold targets as metric-crate [`GoldItem`]s.
    pub fn gold_items(&self) -> Vec<GoldItem> {
        self.gold.iter().map(GoldTarget::to_gold_item).collect()
    }
}

/// A frozen collection of evaluation queries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuerySet {
    pub queries: Vec<EvalQuery>,
}

impl QuerySet {
    pub fn new(queries: Vec<EvalQuery>) -> Self {
        Self { queries }
    }

    pub fn len(&self) -> usize {
        self.queries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queries.is_empty()
    }

    /// Panic if any id is duplicated or any query has empty gold — invariants
    /// the campaign and the paired statistics rely on.
    pub fn validate(&self) {
        let mut seen = std::collections::HashSet::new();
        for q in &self.queries {
            assert!(
                seen.insert(q.id.as_str()),
                "duplicate EvalQuery id: {}",
                q.id
            );
            assert!(!q.gold.is_empty(), "EvalQuery {} has empty gold", q.id);
            assert!(
                !q.query.trim().is_empty(),
                "EvalQuery {} has empty text",
                q.id
            );
        }
    }
}

/// The hand-authored known-item query set (strategy **A**) over the `pgmcp`
/// project. Each query is phrased in *intent* language (what a developer would
/// ask), deliberately avoiding the literal identifiers in the target file, so
/// the query exercises semantic recall rather than keyword echo. Gold paths are
/// verified to exist by `known_item_gold_paths_exist` (a DB-backed campaign
/// check) and by a static non-empty/unique invariant test here.
///
/// Adversarial entries (`notes` mention a distractor) are phrased so a
/// `src/patterns/*` catalog card is the tempting wrong answer — these probe the
/// observed top-k crowding by the ~810-entry pattern catalog.
pub fn known_item_queries() -> QuerySet {
    let q = |id: &str, query: &str, gold_path: &str, notes: &str| EvalQuery {
        id: id.to_string(),
        strategy: QueryStrategy::KnownItem,
        query: query.to_string(),
        project: Some("pgmcp".to_string()),
        gold: vec![GoldTarget::file("pgmcp", gold_path)],
        notes: Some(notes.to_string()),
    };

    QuerySet::new(vec![
        q(
            "ki_hnsw_index",
            "where is the approximate nearest-neighbor index for embeddings created",
            "src/db/migrations.rs",
            "intent phrasing avoids the literal 'HNSW'/'ef_construction' tokens",
        ),
        q(
            "ki_cosine_rank",
            "how are search results ranked by vector distance to the query",
            "src/db/queries/search.rs",
            "the cosine k-NN SQL; avoids 'embedding_v2'/'<=>' literals",
        ),
        q(
            "ki_circuit_breaker",
            "how does the system stop hammering the database during an outage",
            "src/health/prober.rs",
            "adversarial: src/patterns/architecture.rs has a 'circuit_breaker' card distractor",
        ),
        q(
            "ki_chunking",
            "where is file content split into overlapping windows before embedding",
            "src/indexer/chunker.rs",
            "line-window chunker; avoids 'chunk_size_lines'",
        ),
        q(
            "ki_embed_model_load",
            "how is the text embedding model loaded onto the GPU",
            "src/embed/model.rs",
            "BGE-M3 candle load; avoids 'CandleBackend'/'cuda'",
        ),
        q(
            "ki_wilcoxon",
            "paired non-parametric significance test for two related samples",
            "src/stats/inference.rs",
            "adversarial: src/patterns/testing.rs may discuss tests generically",
        ),
        q(
            "ki_topic_collapse_gate",
            "detect when a clustering model has degenerated into uniform memberships",
            "src/quality/topic_metrics.rs",
            "degeneracy gate; avoids 'mean_max_membership'",
        ),
        q(
            "ki_rrf_fusion",
            "combine keyword and vector result lists into one ranked list",
            "src/mcp/tools/tool_hybrid_search.rs",
            "reciprocal rank fusion; avoids 'RRF'/'rrf_score'",
        ),
        q(
            "ki_tracker_trust_boundary",
            "rules preventing an agent from marking its own work as verified",
            "src/tracker/transition.rs",
            "state-transition trust matrix; avoids 'Actor::Agent'",
        ),
        q(
            "ki_session_mandates",
            "extracting imperative instructions from a user's prompt to remember them",
            "src/sessions.rs",
            "mandate extraction pipeline",
        ),
        q(
            "ki_glibc_arena",
            "limiting memory fragmentation from many allocator arenas at startup",
            "src/main.rs",
            "mallopt(M_ARENA_MAX, 2); avoids 'mallopt'",
        ),
        q(
            "ki_disk_watchdog",
            "watching free disk space and inodes to avoid filling the volume",
            "src/health/watchdog.rs",
            "disk watchdog",
        ),
        q(
            "ki_outbox",
            "deferring HTTP posts when the database is unavailable and replaying later",
            "src/health/outbox.rs",
            "deferred-POST outbox",
        ),
        q(
            "ki_dedup_content_hash",
            "avoiding re-embedding files whose contents have not changed",
            "src/embed/pool.rs",
            "content_hash dedup; avoids 'xxh3'",
        ),
        q(
            "ki_ppr_search",
            "expanding search hits to their callers and callees through the code graph",
            "src/db/queries/graph.rs",
            "personalized PageRank / HippoRAG over code_graph_edges",
        ),
        q(
            "ki_raptor",
            "answering module-level questions from precomputed cluster summaries",
            "src/cron/code_raptor.rs",
            "RAPTOR-over-code cron",
        ),
        q(
            "ki_louvain",
            "grouping the import graph into communities of related modules",
            "src/mcp/tools/tool_community_detection.rs",
            "Louvain community detection",
        ),
        q(
            "ki_work_item_migration",
            "the database schema that introduced the hierarchical work-item tracker",
            "src/db/migrations/v12_bug_tracker.rs",
            "v12 bug-tracker migration",
        ),
        q(
            "ki_experiment_protocol",
            "choosing which statistical test to run from an acceptance criterion",
            "src/experiment/protocol.rs",
            "experiment protocol → test mapping",
        ),
        q(
            "ki_bh_fdr",
            "correcting p-values for multiple comparisons to control false discoveries",
            "src/stats/inference.rs",
            "Benjamini-Hochberg; same file as Wilcoxon (multi-gold in same file ok)",
        ),
        q(
            "ki_pattern_catalog_seed",
            "the curated registry of software design patterns and their sources",
            "src/patterns/mod.rs",
            "non-adversarial: here the pattern catalog IS the right answer",
        ),
        q(
            "ki_adaptive_tools",
            "shrinking the per-client tool list based on how the client uses tools",
            "src/mcp/tool_policy.rs",
            "usage-adaptive tool surface (ADR-016)",
        ),
        q(
            "ki_digest",
            "a read-only proactive summary injected at session start",
            "src/digest/mod.rs",
            "proactive digest; trust-boundary read-only",
        ),
        q(
            "ki_ontology_trie",
            "typo-tolerant prefix search over ontology concept names",
            "src/ontology/mod.rs",
            "concept-trie accelerator",
        ),
        q(
            "ki_deadlock_lockorder",
            "building a lock-acquisition-order graph to find potential deadlocks",
            "src/graph/lock_order.rs",
            "static deadlock detection",
        ),
        q(
            "ki_quality_forecast",
            "projecting when a code-quality metric will cross a threshold",
            "src/quality/forecast.rs",
            "OLS slope / weeks-to-threshold",
        ),
        q(
            "ki_cron_phase_gate",
            "preventing background jobs from running before the index is ready",
            "src/cron/mod.rs",
            "phase gate on crons",
        ),
        q(
            "ki_git_autolink",
            "linking commits to work items by parsing the commit message",
            "src/tracker/git_link.rs",
            "git/PR close-the-loop",
        ),
        q(
            "ki_ci_evidence",
            "only continuous-integration evidence may mark a fix as verified",
            "src/tracker/auto_transition.rs",
            "CI-evidence gatekeeper",
        ),
        q(
            "ki_embedding_signature",
            "tracking which embedding model version produced each stored vector",
            "src/embed/signature.rs",
            "embedding signature bge-m3-v1",
        ),
        q(
            "ki_data_tables",
            "storing user-defined tabular data as JSON rows without dynamic DDL",
            "src/db/queries/data_tables.rs",
            "JSON data tables (ADR-010)",
        ),
        q(
            "ki_reranker",
            "a cross-encoder that re-scores candidate passages for the memory server",
            "src/reranker/bge_v2_m3.rs",
            "BGE-reranker-v2-m3",
        ),
        q(
            "ki_msm_trajectory",
            "matching a project's evolution against known trajectory templates",
            "src/mcp/tools/tool_trajectory_similarity.rs",
            "MSM trajectory / evolves_like",
        ),
        q(
            "ki_secret_detection",
            "finding hardcoded credentials and API keys in source files",
            "src/mcp/tools/tool_secret_detection.rs",
            "secret detection security scan",
        ),
        q(
            "ki_taint",
            "tracing untrusted input to dangerous sinks across functions",
            "src/mcp/tools/tool_taint_analysis.rs",
            "taint analysis",
        ),
        q(
            "ki_centrality",
            "ranking the most important files by their position in the dependency graph",
            "src/mcp/tools/tool_centrality_analysis.rs",
            "PageRank / betweenness centrality",
        ),
        q(
            "ki_blame_busfactor",
            "identifying knowledge concentration where one author owns a module",
            "src/mcp/tools/tool_bus_factor.rs",
            "bus factor via git blame",
        ),
        q(
            "ki_prompt_dedup",
            "persisting user prompts deduplicated by content hash for later recall",
            "src/sessions.rs",
            "session_prompts sha256 dedup (multi-target with ki_session_mandates ok)",
        ),
        q(
            "ki_render_views",
            "rendering an analysis report into multiple output formats",
            "src/render/mod.rs",
            "multi-format View renderer",
        ),
        q(
            "ki_gpu_admission",
            "limiting how many embedding models are resident on the GPU at once",
            "src/embed/admission.rs",
            "gpu_max_resident_embedders semaphore",
        ),
        q(
            "ki_migration_runner",
            "applying schema migrations in order at startup",
            "src/db/migrations.rs",
            "migration runner (multi-target with ki_hnsw_index ok)",
        ),
        q(
            "ki_tsvector_fts",
            "the stored full-text search column and its trigger or generated value",
            "src/db/migrations/v13_fts_stored_tsv.rs",
            "stored content_tsv",
        ),
        q(
            "ki_a2a_patterns",
            "coordinating multiple agents in sequential and deliberation patterns",
            "src/mcp/tools/tool_a2a_pattern_sequential.rs",
            "a2a coordination patterns",
        ),
        q(
            "ki_qwen_local",
            "calling a locally hosted language model for code summaries",
            "src/llm/qwen3.rs",
            "local qwen3-4b extractor",
        ),
        q(
            "ki_effect_breakdown",
            "summarizing the side effects touched by a set of functions",
            "src/mcp/tools/sema_helpers/effects.rs",
            "effect_breakdown helper",
        ),
        q(
            "ki_orient_tool",
            "a single call that bundles project overview, entry points and health",
            "src/mcp/tools/tool_orient.rs",
            "orient composite first-step tool",
        ),
        q(
            "ki_bus_disk_outbox_breaker",
            "the design record explaining resilience to database outages",
            "docs/decisions/015-db-resilience.md",
            "ADR-015 doc target (markdown gold)",
        ),
        q(
            "ki_recency_decay",
            "the theory behind decaying tool-usage counts over time",
            "docs/design/tool-policy-recency-decay.md",
            "recency-decay design doc (markdown gold)",
        ),
        q(
            "ki_verify_gate",
            "the script that must pass before any code change is complete",
            "scripts/verify.sh",
            "verify.sh (shell gold)",
        ),
        q(
            "ki_index_freshness_adr",
            "the decision record for detecting stale index entries and reconciling them",
            "docs/decisions/019-index-freshness-reconcile.md",
            "ADR-019 doc target (markdown gold)",
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_item_set_is_valid_and_sizeable() {
        let qs = known_item_queries();
        qs.validate();
        assert!(
            qs.len() >= 50,
            "want >= 50 known-item queries, got {}",
            qs.len()
        );
    }

    #[test]
    fn gold_target_converts_to_gold_item() {
        let g = GoldTarget::file("pgmcp", "src/x.rs");
        let gi = g.to_gold_item();
        assert_eq!(gi.path, "src/x.rs");
        assert_eq!(gi.relevance, 1.0);
        assert_eq!(gi.start_line, None);
    }

    #[test]
    fn known_item_queries_avoid_filename_identifier_echo() {
        // Guard against the *strong* leakage signal: a query literally
        // containing the gold file's compound identifier name (e.g.
        // `tool_hybrid_search`, `lock_order`, `v12_bug_tracker`). Generic
        // single-word stems (`search`, `model`, `graph`) are legitimate domain
        // vocabulary and intentionally allowed — a fair known-item set mixes
        // lexical-friendly queries (where text/hybrid should win) with purely
        // conceptual ones (where semantic should win); forbidding domain words
        // would bias the set toward the conceptual case.
        for q in known_item_queries().queries {
            let stem = q.gold[0]
                .path
                .rsplit('/')
                .next()
                .and_then(|f| f.split('.').next())
                .unwrap_or("")
                .to_lowercase();
            let compound = stem.contains('_') || stem.contains('-');
            if compound && stem.len() >= 7 {
                assert!(
                    !q.query.to_lowercase().contains(&stem),
                    "query `{}` echoes its gold file identifier `{}`",
                    q.query,
                    stem
                );
            }
        }
    }
}
