//! Lock-free atomic statistics tracker.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use dashmap::DashMap;

/// All statistics counters — fully lock-free.
pub struct StatsTracker {
    // Indexing counters
    pub files_indexed: AtomicU64,
    pub files_failed: AtomicU64,
    /// Files where the inference worker observed a foreign-key violation
    /// during chunk insert — i.e. the parent `indexed_files` row was
    /// deleted while the worker was mid-pipeline (typical cause:
    /// `pgmcp reindex --force` while daemon alive, or external
    /// `TRUNCATE indexed_files`). One increment per affected file, not
    /// per chunk.
    pub files_aborted_fk: AtomicU64,
    pub chunks_embedded: AtomicU64,
    pub bytes_processed: AtomicU64,

    // MCP counters
    pub mcp_requests: AtomicU64,
    pub mcp_errors: AtomicU64,
    pub semantic_searches: AtomicU64,
    pub text_searches: AtomicU64,
    pub grep_searches: AtomicU64,
    pub commit_searches: AtomicU64,

    // Timing (cumulative)
    pub index_duration_ms: AtomicU64,
    pub embedding_duration_ms: AtomicU64,
    pub last_index_timestamp: AtomicU64,

    // Scan counters
    pub files_scanned: AtomicU64,
    pub files_skipped: AtomicU64,
    pub files_stale_removed: AtomicU64,

    // Pool state
    pub active_work_pool_threads: AtomicU64,
    pub work_pool_queue_depth: AtomicU64,

    // Cron counters
    pub cron_executions: AtomicU64,
    pub cron_panics: AtomicU64,

    // Git history counters
    pub git_commits_indexed: AtomicU64,
    pub git_commits_failed: AtomicU64,

    // Config watcher counters
    pub config_reloads: AtomicU64,
    pub config_reload_errors: AtomicU64,

    // Similarity analysis counters
    pub similarity_scans: AtomicU64,
    pub similarity_pairs_found: AtomicU64,

    // Topic clustering counters
    pub topic_scans: AtomicU64,
    pub topics_discovered: AtomicU64,
    pub topic_noise_chunks: AtomicU64,

    // Embedding pool counters
    pub embed_file_batches: AtomicU64,
    pub embed_commit_batches: AtomicU64,
    pub embed_query_count: AtomicU64,
    pub embed_errors: AtomicU64,

    // File watcher counters
    pub watcher_events_received: AtomicU64,
    pub watcher_events_filtered: AtomicU64,
    pub watcher_events_debounced: AtomicU64,

    // Work pool lifetime counters
    pub work_pool_tasks_completed: AtomicU64,
    pub work_pool_scale_ups: AtomicU64,
    pub work_pool_scale_downs: AtomicU64,

    // Analysis tool counters
    pub orphan_scans: AtomicU64,
    pub misplaced_scans: AtomicU64,
    pub coupling_scans: AtomicU64,
    pub coverage_scans: AtomicU64,
    pub complexity_scans: AtomicU64,
    pub hierarchy_scans: AtomicU64,

    // Document analysis tool counters
    pub merge_scans: AtomicU64,
    pub split_scans: AtomicU64,
    pub doc_coverage_scans: AtomicU64,

    // Graph analysis counters
    pub graph_build_runs: AtomicU64,
    pub dependency_graph_scans: AtomicU64,
    pub centrality_scans: AtomicU64,
    pub community_scans: AtomicU64,
    pub cycle_scans: AtomicU64,
    pub impact_scans: AtomicU64,

    // Architecture & design quality counters
    pub coupling_reports: AtomicU64,
    pub violation_scans: AtomicU64,
    pub design_smell_scans: AtomicU64,
    pub architecture_quality_scans: AtomicU64,
    pub design_metric_scans: AtomicU64,

    // ML prediction counters
    pub bug_predictions: AtomicU64,
    pub debt_analyses: AtomicU64,
    pub anomaly_scans: AtomicU64,

    // NLP & IR counters
    pub hybrid_searches: AtomicU64,
    pub summarize_scans: AtomicU64,

    // Scorecard counters
    pub scorecard_scans: AtomicU64,

    // DRY tier (Tier 2) counters — recommendation-shaped tools producing
    // typed `RecommendedFix` actions; see `src/mcp/tools/fix_actions.rs`.
    pub chunk_cluster_scans: AtomicU64,
    pub extraction_candidate_reports: AtomicU64,
    pub boilerplate_scans: AtomicU64,
    pub internal_dry_scans: AtomicU64,
    pub pattern_abstraction_scans: AtomicU64,

    // Antipattern-fix tier (Tier 3) counters — typed `RecommendedFix`
    // actions for architecture violations and design smells.
    pub cycle_fix_scans: AtomicU64,
    pub split_recommendations: AtomicU64,
    pub consolidation_scans: AtomicU64,
    pub zombie_scans: AtomicU64,
    pub layering_scans: AtomicU64,

    // Engineer/architect workflow tier (Tier 4) counters.
    pub pr_scope_recommendations: AtomicU64,
    pub hot_path_audits: AtomicU64,
    pub bus_factor_scans: AtomicU64,
    pub reviewer_recommendations: AtomicU64,

    // Audit & trend tier (Tier 5) counters.
    pub dependency_health_scans: AtomicU64,
    pub pattern_searches: AtomicU64,
    pub merge_risk_scans: AtomicU64,
    pub naming_consistency_scans: AtomicU64,
    pub growth_trajectory_scans: AtomicU64,
    pub adoption_lag_scans: AtomicU64,
    pub burn_down_plans: AtomicU64,

    // Tree-sitter symbol extraction (Tier 0e infrastructure).
    pub symbol_extraction_runs: AtomicU64,
    pub symbols_extracted: AtomicU64,
    pub symbol_references_inserted: AtomicU64,

    /// Currently-connected Streamable HTTP MCP sessions. Incremented when
    /// a peer issues an `initialize` and a session is created in the
    /// `LocalSessionManager` wrapper; decremented on session close /
    /// expiration. The stdio transport is always 1 peer per process and
    /// is reported separately by the `pgmcp status` CLI.
    pub http_mcp_sessions: AtomicU64,

    // Memory observability (Phase 4 of OOM fix)
    /// Peak RSS in bytes observed since daemon start. Updated every 500 ms
    /// by the peak-RSS sampler thread (`src/stats/rss.rs::spawn_peak_sampler`).
    pub peak_rss_bytes: AtomicU64,
    /// Currently-observed RSS in bytes (same sampler updates this). Exposed
    /// as a Prometheus gauge to cheaply see live memory without scraping
    /// `/proc/self/statm` from the HTTP handler.
    pub current_rss_bytes: AtomicU64,
    /// Set while a heavy cron body is executing (exclusive via heavy_cron_lock).
    /// Read by observability tooling to correlate RSS spikes with cron phase.
    pub heavy_cron_running: AtomicBool,

    /// Per-tool invocation counter, keyed by MCP tool name (e.g.
    /// `"semantic_search"`, `"grep"`, `"orient"`). Populated by
    /// `record_tool_call()` at the top of each `#[tool]` body in
    /// `src/mcp/server.rs`. Surfaced in `/api/status`'s `counters` block as
    /// the `tool_invocations` map. Used to A/B-test pgmcp utilization across
    /// rollouts of the description rewrites, hooks, and agent-override work.
    pub tool_invocations: DashMap<String, AtomicU64>,

    // Uptime
    pub uptime_start: Instant,
}

impl StatsTracker {
    pub fn new() -> Self {
        Self {
            files_indexed: AtomicU64::new(0),
            files_failed: AtomicU64::new(0),
            files_aborted_fk: AtomicU64::new(0),
            chunks_embedded: AtomicU64::new(0),
            bytes_processed: AtomicU64::new(0),
            mcp_requests: AtomicU64::new(0),
            mcp_errors: AtomicU64::new(0),
            semantic_searches: AtomicU64::new(0),
            text_searches: AtomicU64::new(0),
            grep_searches: AtomicU64::new(0),
            commit_searches: AtomicU64::new(0),
            index_duration_ms: AtomicU64::new(0),
            embedding_duration_ms: AtomicU64::new(0),
            last_index_timestamp: AtomicU64::new(0),
            files_scanned: AtomicU64::new(0),
            files_skipped: AtomicU64::new(0),
            files_stale_removed: AtomicU64::new(0),
            active_work_pool_threads: AtomicU64::new(0),
            work_pool_queue_depth: AtomicU64::new(0),
            cron_executions: AtomicU64::new(0),
            cron_panics: AtomicU64::new(0),
            git_commits_indexed: AtomicU64::new(0),
            git_commits_failed: AtomicU64::new(0),
            config_reloads: AtomicU64::new(0),
            config_reload_errors: AtomicU64::new(0),
            similarity_scans: AtomicU64::new(0),
            similarity_pairs_found: AtomicU64::new(0),
            topic_scans: AtomicU64::new(0),
            topics_discovered: AtomicU64::new(0),
            topic_noise_chunks: AtomicU64::new(0),
            embed_file_batches: AtomicU64::new(0),
            embed_commit_batches: AtomicU64::new(0),
            embed_query_count: AtomicU64::new(0),
            embed_errors: AtomicU64::new(0),
            watcher_events_received: AtomicU64::new(0),
            watcher_events_filtered: AtomicU64::new(0),
            watcher_events_debounced: AtomicU64::new(0),
            work_pool_tasks_completed: AtomicU64::new(0),
            work_pool_scale_ups: AtomicU64::new(0),
            work_pool_scale_downs: AtomicU64::new(0),
            orphan_scans: AtomicU64::new(0),
            misplaced_scans: AtomicU64::new(0),
            coupling_scans: AtomicU64::new(0),
            coverage_scans: AtomicU64::new(0),
            complexity_scans: AtomicU64::new(0),
            hierarchy_scans: AtomicU64::new(0),
            merge_scans: AtomicU64::new(0),
            split_scans: AtomicU64::new(0),
            doc_coverage_scans: AtomicU64::new(0),
            graph_build_runs: AtomicU64::new(0),
            dependency_graph_scans: AtomicU64::new(0),
            centrality_scans: AtomicU64::new(0),
            community_scans: AtomicU64::new(0),
            cycle_scans: AtomicU64::new(0),
            impact_scans: AtomicU64::new(0),
            coupling_reports: AtomicU64::new(0),
            violation_scans: AtomicU64::new(0),
            design_smell_scans: AtomicU64::new(0),
            architecture_quality_scans: AtomicU64::new(0),
            design_metric_scans: AtomicU64::new(0),
            bug_predictions: AtomicU64::new(0),
            debt_analyses: AtomicU64::new(0),
            anomaly_scans: AtomicU64::new(0),
            hybrid_searches: AtomicU64::new(0),
            summarize_scans: AtomicU64::new(0),
            scorecard_scans: AtomicU64::new(0),
            chunk_cluster_scans: AtomicU64::new(0),
            extraction_candidate_reports: AtomicU64::new(0),
            boilerplate_scans: AtomicU64::new(0),
            internal_dry_scans: AtomicU64::new(0),
            pattern_abstraction_scans: AtomicU64::new(0),
            cycle_fix_scans: AtomicU64::new(0),
            split_recommendations: AtomicU64::new(0),
            consolidation_scans: AtomicU64::new(0),
            zombie_scans: AtomicU64::new(0),
            layering_scans: AtomicU64::new(0),
            pr_scope_recommendations: AtomicU64::new(0),
            hot_path_audits: AtomicU64::new(0),
            bus_factor_scans: AtomicU64::new(0),
            reviewer_recommendations: AtomicU64::new(0),
            dependency_health_scans: AtomicU64::new(0),
            pattern_searches: AtomicU64::new(0),
            merge_risk_scans: AtomicU64::new(0),
            naming_consistency_scans: AtomicU64::new(0),
            growth_trajectory_scans: AtomicU64::new(0),
            adoption_lag_scans: AtomicU64::new(0),
            burn_down_plans: AtomicU64::new(0),
            symbol_extraction_runs: AtomicU64::new(0),
            symbols_extracted: AtomicU64::new(0),
            symbol_references_inserted: AtomicU64::new(0),
            http_mcp_sessions: AtomicU64::new(0),
            peak_rss_bytes: AtomicU64::new(0),
            current_rss_bytes: AtomicU64::new(0),
            heavy_cron_running: AtomicBool::new(false),
            tool_invocations: DashMap::new(),
            uptime_start: Instant::now(),
        }
    }

    /// Increment the per-tool invocation counter for the named MCP tool.
    /// Called once at the top of each `#[tool]` body in `src/mcp/server.rs`.
    /// `Relaxed` ordering: the counter is observability-only, no
    /// happens-before relation needed with anything else.
    pub fn record_tool_call(&self, name: &str) {
        self.tool_invocations
            .entry(name.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Get a JSON snapshot of all counters.
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "files_indexed": self.files_indexed.load(Ordering::Acquire),
            "files_failed": self.files_failed.load(Ordering::Acquire),
            "files_aborted_fk": self.files_aborted_fk.load(Ordering::Acquire),
            "chunks_embedded": self.chunks_embedded.load(Ordering::Acquire),
            "bytes_processed": self.bytes_processed.load(Ordering::Acquire),
            "mcp_requests": self.mcp_requests.load(Ordering::Acquire),
            "mcp_errors": self.mcp_errors.load(Ordering::Acquire),
            "semantic_searches": self.semantic_searches.load(Ordering::Acquire),
            "text_searches": self.text_searches.load(Ordering::Acquire),
            "grep_searches": self.grep_searches.load(Ordering::Acquire),
            "commit_searches": self.commit_searches.load(Ordering::Acquire),
            "index_duration_ms": self.index_duration_ms.load(Ordering::Acquire),
            "embedding_duration_ms": self.embedding_duration_ms.load(Ordering::Acquire),
            "files_scanned": self.files_scanned.load(Ordering::Acquire),
            "files_skipped": self.files_skipped.load(Ordering::Acquire),
            "files_stale_removed": self.files_stale_removed.load(Ordering::Acquire),
            "active_work_pool_threads": self.active_work_pool_threads.load(Ordering::Acquire),
            "work_pool_queue_depth": self.work_pool_queue_depth.load(Ordering::Acquire),
            "cron_executions": self.cron_executions.load(Ordering::Acquire),
            "cron_panics": self.cron_panics.load(Ordering::Acquire),
            "git_commits_indexed": self.git_commits_indexed.load(Ordering::Acquire),
            "git_commits_failed": self.git_commits_failed.load(Ordering::Acquire),
            "config_reloads": self.config_reloads.load(Ordering::Acquire),
            "config_reload_errors": self.config_reload_errors.load(Ordering::Acquire),
            "similarity_scans": self.similarity_scans.load(Ordering::Acquire),
            "similarity_pairs_found": self.similarity_pairs_found.load(Ordering::Acquire),
            "topic_scans": self.topic_scans.load(Ordering::Acquire),
            "topics_discovered": self.topics_discovered.load(Ordering::Acquire),
            "topic_noise_chunks": self.topic_noise_chunks.load(Ordering::Acquire),
            "embed_file_batches": self.embed_file_batches.load(Ordering::Acquire),
            "embed_commit_batches": self.embed_commit_batches.load(Ordering::Acquire),
            "embed_query_count": self.embed_query_count.load(Ordering::Acquire),
            "embed_errors": self.embed_errors.load(Ordering::Acquire),
            "watcher_events_received": self.watcher_events_received.load(Ordering::Acquire),
            "watcher_events_filtered": self.watcher_events_filtered.load(Ordering::Acquire),
            "watcher_events_debounced": self.watcher_events_debounced.load(Ordering::Acquire),
            "work_pool_tasks_completed": self.work_pool_tasks_completed.load(Ordering::Acquire),
            "work_pool_scale_ups": self.work_pool_scale_ups.load(Ordering::Acquire),
            "work_pool_scale_downs": self.work_pool_scale_downs.load(Ordering::Acquire),
            "orphan_scans": self.orphan_scans.load(Ordering::Acquire),
            "misplaced_scans": self.misplaced_scans.load(Ordering::Acquire),
            "coupling_scans": self.coupling_scans.load(Ordering::Acquire),
            "coverage_scans": self.coverage_scans.load(Ordering::Acquire),
            "complexity_scans": self.complexity_scans.load(Ordering::Acquire),
            "hierarchy_scans": self.hierarchy_scans.load(Ordering::Acquire),
            "merge_scans": self.merge_scans.load(Ordering::Acquire),
            "split_scans": self.split_scans.load(Ordering::Acquire),
            "doc_coverage_scans": self.doc_coverage_scans.load(Ordering::Acquire),
            "graph_build_runs": self.graph_build_runs.load(Ordering::Acquire),
            "dependency_graph_scans": self.dependency_graph_scans.load(Ordering::Acquire),
            "centrality_scans": self.centrality_scans.load(Ordering::Acquire),
            "community_scans": self.community_scans.load(Ordering::Acquire),
            "cycle_scans": self.cycle_scans.load(Ordering::Acquire),
            "impact_scans": self.impact_scans.load(Ordering::Acquire),
            "coupling_reports": self.coupling_reports.load(Ordering::Acquire),
            "violation_scans": self.violation_scans.load(Ordering::Acquire),
            "design_smell_scans": self.design_smell_scans.load(Ordering::Acquire),
            "architecture_quality_scans": self.architecture_quality_scans.load(Ordering::Acquire),
            "design_metric_scans": self.design_metric_scans.load(Ordering::Acquire),
            "bug_predictions": self.bug_predictions.load(Ordering::Acquire),
            "debt_analyses": self.debt_analyses.load(Ordering::Acquire),
            "anomaly_scans": self.anomaly_scans.load(Ordering::Acquire),
            "hybrid_searches": self.hybrid_searches.load(Ordering::Acquire),
            "summarize_scans": self.summarize_scans.load(Ordering::Acquire),
            "scorecard_scans": self.scorecard_scans.load(Ordering::Acquire),
            "chunk_cluster_scans": self.chunk_cluster_scans.load(Ordering::Acquire),
            "extraction_candidate_reports": self.extraction_candidate_reports.load(Ordering::Acquire),
            "boilerplate_scans": self.boilerplate_scans.load(Ordering::Acquire),
            "internal_dry_scans": self.internal_dry_scans.load(Ordering::Acquire),
            "pattern_abstraction_scans": self.pattern_abstraction_scans.load(Ordering::Acquire),
            "cycle_fix_scans": self.cycle_fix_scans.load(Ordering::Acquire),
            "split_recommendations": self.split_recommendations.load(Ordering::Acquire),
            "consolidation_scans": self.consolidation_scans.load(Ordering::Acquire),
            "zombie_scans": self.zombie_scans.load(Ordering::Acquire),
            "layering_scans": self.layering_scans.load(Ordering::Acquire),
            "pr_scope_recommendations": self.pr_scope_recommendations.load(Ordering::Acquire),
            "hot_path_audits": self.hot_path_audits.load(Ordering::Acquire),
            "bus_factor_scans": self.bus_factor_scans.load(Ordering::Acquire),
            "reviewer_recommendations": self.reviewer_recommendations.load(Ordering::Acquire),
            "dependency_health_scans": self.dependency_health_scans.load(Ordering::Acquire),
            "pattern_searches": self.pattern_searches.load(Ordering::Acquire),
            "merge_risk_scans": self.merge_risk_scans.load(Ordering::Acquire),
            "naming_consistency_scans": self.naming_consistency_scans.load(Ordering::Acquire),
            "growth_trajectory_scans": self.growth_trajectory_scans.load(Ordering::Acquire),
            "adoption_lag_scans": self.adoption_lag_scans.load(Ordering::Acquire),
            "burn_down_plans": self.burn_down_plans.load(Ordering::Acquire),
            "symbol_extraction_runs": self.symbol_extraction_runs.load(Ordering::Acquire),
            "symbols_extracted": self.symbols_extracted.load(Ordering::Acquire),
            "symbol_references_inserted": self.symbol_references_inserted.load(Ordering::Acquire),
            "http_mcp_sessions": self.http_mcp_sessions.load(Ordering::Acquire),
            "tool_invocations": serde_json::Value::Object(
                self.tool_invocations.iter()
                    .map(|e| (e.key().clone(), serde_json::Value::from(e.value().load(Ordering::Relaxed))))
                    .collect()
            ),
            "uptime_secs": self.uptime_start.elapsed().as_secs(),
        })
    }
}

impl Default for StatsTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_contains_all_new_counters() {
        let stats = StatsTracker::new();

        // Set all new fields to distinct non-zero values
        stats.cron_executions.store(1, Ordering::Relaxed);
        stats.cron_panics.store(2, Ordering::Relaxed);
        stats.git_commits_indexed.store(3, Ordering::Relaxed);
        stats.git_commits_failed.store(4, Ordering::Relaxed);
        stats.config_reloads.store(5, Ordering::Relaxed);
        stats.config_reload_errors.store(6, Ordering::Relaxed);
        stats.similarity_scans.store(17, Ordering::Relaxed);
        stats.similarity_pairs_found.store(18, Ordering::Relaxed);
        stats.embed_file_batches.store(7, Ordering::Relaxed);
        stats.embed_commit_batches.store(8, Ordering::Relaxed);
        stats.embed_query_count.store(48, Ordering::Relaxed);
        stats.embed_errors.store(9, Ordering::Relaxed);
        stats.watcher_events_received.store(10, Ordering::Relaxed);
        stats.watcher_events_filtered.store(11, Ordering::Relaxed);
        stats.watcher_events_debounced.store(12, Ordering::Relaxed);
        stats.work_pool_tasks_completed.store(13, Ordering::Relaxed);
        stats.work_pool_scale_ups.store(14, Ordering::Relaxed);
        stats.work_pool_scale_downs.store(15, Ordering::Relaxed);
        stats.commit_searches.store(16, Ordering::Relaxed);
        stats.topic_scans.store(19, Ordering::Relaxed);
        stats.topics_discovered.store(20, Ordering::Relaxed);
        stats.topic_noise_chunks.store(21, Ordering::Relaxed);
        stats.orphan_scans.store(22, Ordering::Relaxed);
        stats.misplaced_scans.store(23, Ordering::Relaxed);
        stats.coupling_scans.store(24, Ordering::Relaxed);
        stats.coverage_scans.store(25, Ordering::Relaxed);
        stats.complexity_scans.store(26, Ordering::Relaxed);
        stats.hierarchy_scans.store(27, Ordering::Relaxed);
        stats.merge_scans.store(28, Ordering::Relaxed);
        stats.split_scans.store(29, Ordering::Relaxed);
        stats.doc_coverage_scans.store(30, Ordering::Relaxed);
        stats.graph_build_runs.store(31, Ordering::Relaxed);
        stats.dependency_graph_scans.store(32, Ordering::Relaxed);
        stats.centrality_scans.store(33, Ordering::Relaxed);
        stats.community_scans.store(34, Ordering::Relaxed);
        stats.cycle_scans.store(35, Ordering::Relaxed);
        stats.impact_scans.store(36, Ordering::Relaxed);
        stats.coupling_reports.store(37, Ordering::Relaxed);
        stats.violation_scans.store(38, Ordering::Relaxed);
        stats.design_smell_scans.store(39, Ordering::Relaxed);
        stats
            .architecture_quality_scans
            .store(40, Ordering::Relaxed);
        stats.design_metric_scans.store(41, Ordering::Relaxed);
        stats.bug_predictions.store(42, Ordering::Relaxed);
        stats.debt_analyses.store(43, Ordering::Relaxed);
        stats.anomaly_scans.store(44, Ordering::Relaxed);
        stats.hybrid_searches.store(45, Ordering::Relaxed);
        stats.summarize_scans.store(46, Ordering::Relaxed);
        stats.scorecard_scans.store(47, Ordering::Relaxed);
        stats.chunk_cluster_scans.store(101, Ordering::Relaxed);
        stats
            .extraction_candidate_reports
            .store(102, Ordering::Relaxed);
        stats.boilerplate_scans.store(103, Ordering::Relaxed);
        stats.internal_dry_scans.store(104, Ordering::Relaxed);
        stats
            .pattern_abstraction_scans
            .store(105, Ordering::Relaxed);
        stats.cycle_fix_scans.store(106, Ordering::Relaxed);
        stats.split_recommendations.store(107, Ordering::Relaxed);
        stats.consolidation_scans.store(108, Ordering::Relaxed);
        stats.zombie_scans.store(109, Ordering::Relaxed);
        stats.layering_scans.store(110, Ordering::Relaxed);
        stats.pr_scope_recommendations.store(111, Ordering::Relaxed);
        stats.hot_path_audits.store(112, Ordering::Relaxed);
        stats.bus_factor_scans.store(113, Ordering::Relaxed);
        stats.reviewer_recommendations.store(114, Ordering::Relaxed);
        stats.dependency_health_scans.store(115, Ordering::Relaxed);
        stats.pattern_searches.store(116, Ordering::Relaxed);
        stats.merge_risk_scans.store(117, Ordering::Relaxed);
        stats.naming_consistency_scans.store(118, Ordering::Relaxed);
        stats.growth_trajectory_scans.store(119, Ordering::Relaxed);
        stats.adoption_lag_scans.store(120, Ordering::Relaxed);
        stats.burn_down_plans.store(121, Ordering::Relaxed);
        stats.symbol_extraction_runs.store(122, Ordering::Relaxed);
        stats.symbols_extracted.store(123, Ordering::Relaxed);
        stats
            .symbol_references_inserted
            .store(124, Ordering::Relaxed);

        let snap = stats.snapshot();

        assert_eq!(snap["cron_executions"], 1);
        assert_eq!(snap["cron_panics"], 2);
        assert_eq!(snap["git_commits_indexed"], 3);
        assert_eq!(snap["git_commits_failed"], 4);
        assert_eq!(snap["config_reloads"], 5);
        assert_eq!(snap["config_reload_errors"], 6);
        assert_eq!(snap["similarity_scans"], 17);
        assert_eq!(snap["similarity_pairs_found"], 18);
        assert_eq!(snap["topic_scans"], 19);
        assert_eq!(snap["topics_discovered"], 20);
        assert_eq!(snap["topic_noise_chunks"], 21);
        assert_eq!(snap["embed_file_batches"], 7);
        assert_eq!(snap["embed_commit_batches"], 8);
        assert_eq!(snap["embed_query_count"], 48);
        assert_eq!(snap["embed_errors"], 9);
        assert_eq!(snap["watcher_events_received"], 10);
        assert_eq!(snap["watcher_events_filtered"], 11);
        assert_eq!(snap["watcher_events_debounced"], 12);
        assert_eq!(snap["work_pool_tasks_completed"], 13);
        assert_eq!(snap["work_pool_scale_ups"], 14);
        assert_eq!(snap["work_pool_scale_downs"], 15);
        assert_eq!(snap["commit_searches"], 16);
        assert_eq!(snap["orphan_scans"], 22);
        assert_eq!(snap["misplaced_scans"], 23);
        assert_eq!(snap["coupling_scans"], 24);
        assert_eq!(snap["coverage_scans"], 25);
        assert_eq!(snap["complexity_scans"], 26);
        assert_eq!(snap["hierarchy_scans"], 27);
        assert_eq!(snap["merge_scans"], 28);
        assert_eq!(snap["split_scans"], 29);
        assert_eq!(snap["doc_coverage_scans"], 30);
        assert_eq!(snap["graph_build_runs"], 31);
        assert_eq!(snap["dependency_graph_scans"], 32);
        assert_eq!(snap["centrality_scans"], 33);
        assert_eq!(snap["community_scans"], 34);
        assert_eq!(snap["cycle_scans"], 35);
        assert_eq!(snap["impact_scans"], 36);
        assert_eq!(snap["coupling_reports"], 37);
        assert_eq!(snap["violation_scans"], 38);
        assert_eq!(snap["design_smell_scans"], 39);
        assert_eq!(snap["architecture_quality_scans"], 40);
        assert_eq!(snap["design_metric_scans"], 41);
        assert_eq!(snap["bug_predictions"], 42);
        assert_eq!(snap["debt_analyses"], 43);
        assert_eq!(snap["anomaly_scans"], 44);
        assert_eq!(snap["hybrid_searches"], 45);
        assert_eq!(snap["summarize_scans"], 46);
        assert_eq!(snap["scorecard_scans"], 47);
        assert_eq!(snap["chunk_cluster_scans"], 101);
        assert_eq!(snap["extraction_candidate_reports"], 102);
        assert_eq!(snap["boilerplate_scans"], 103);
        assert_eq!(snap["internal_dry_scans"], 104);
        assert_eq!(snap["pattern_abstraction_scans"], 105);
        assert_eq!(snap["cycle_fix_scans"], 106);
        assert_eq!(snap["split_recommendations"], 107);
        assert_eq!(snap["consolidation_scans"], 108);
        assert_eq!(snap["zombie_scans"], 109);
        assert_eq!(snap["layering_scans"], 110);
        assert_eq!(snap["pr_scope_recommendations"], 111);
        assert_eq!(snap["hot_path_audits"], 112);
        assert_eq!(snap["bus_factor_scans"], 113);
        assert_eq!(snap["reviewer_recommendations"], 114);
        assert_eq!(snap["dependency_health_scans"], 115);
        assert_eq!(snap["pattern_searches"], 116);
        assert_eq!(snap["merge_risk_scans"], 117);
        assert_eq!(snap["naming_consistency_scans"], 118);
        assert_eq!(snap["growth_trajectory_scans"], 119);
        assert_eq!(snap["adoption_lag_scans"], 120);
        assert_eq!(snap["burn_down_plans"], 121);
        assert_eq!(snap["symbol_extraction_runs"], 122);
        assert_eq!(snap["symbols_extracted"], 123);
        assert_eq!(snap["symbol_references_inserted"], 124);

        // Verify existing fields still present
        assert_eq!(snap["files_indexed"], 0);
        assert_eq!(snap["mcp_requests"], 0);
        assert!(snap["uptime_secs"].as_u64().is_some());
    }

    // ========================================================================
    // Concurrency property tests
    // ========================================================================

    use proptest::prelude::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 8, ..ProptestConfig::default() })]

        /// N producer threads × M increments each = counter is exactly N*M.
        /// Proves fetch_add(1, Relaxed) loses no updates under contention.
        #[test]
        fn prop_concurrent_increments_are_not_lost(
            producers in 2usize..8,
            per_producer in 100usize..500,
        ) {
            let tracker = Arc::new(StatsTracker::new());
            let mut handles = Vec::new();
            for _ in 0..producers {
                let t = Arc::clone(&tracker);
                handles.push(std::thread::spawn(move || {
                    for _ in 0..per_producer {
                        t.files_indexed.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
            for h in handles {
                h.join().expect("thread join");
            }
            let total = tracker.files_indexed.load(Ordering::Relaxed);
            prop_assert_eq!(total as usize, producers * per_producer);
        }

        /// Read-after-write monotonicity: every read value ≥ previously read.
        #[test]
        fn prop_counter_monotonically_non_decreasing(
            increments in prop::collection::vec(1u64..100, 5..20usize),
        ) {
            let tracker = StatsTracker::new();
            let mut last = 0u64;
            for inc in &increments {
                tracker.files_indexed.fetch_add(*inc, Ordering::Relaxed);
                let now = tracker.files_indexed.load(Ordering::Relaxed);
                prop_assert!(now >= last,
                    "counter decreased: {} -> {}", last, now);
                last = now;
            }
        }
    }
}
