//! Lock-free atomic statistics tracker.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use arc_swap::ArcSwapOption;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use tokio::sync::mpsc;

use crate::stats::telemetry_writer::TelemetryRow;

/// Outcome of the most recent run of a named cron job. `Ok` covers both
/// requeue-true and stop-false return values of the task closure;
/// `Panicked` covers anything `catch_unwind` caught.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronJobOutcome {
    Ok,
    Panicked,
}

impl CronJobOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            CronJobOutcome::Ok => "ok",
            CronJobOutcome::Panicked => "panicked",
        }
    }
}

/// Last-known status of one named cron job. Kept in the `last_cron_outcomes`
/// DashMap on `StatsTracker`; exposed via the JSON snapshot so dashboards
/// can distinguish "running cleanly", "panicked recently", and "never run"
/// per job rather than only seeing global `cron_panics`.
#[derive(Debug, Clone)]
pub struct CronJobStatus {
    pub outcome: CronJobOutcome,
    pub at: DateTime<Utc>,
    pub duration_ms: u64,
}

/// Per-tool telemetry: call count, error count, cumulative duration, and
/// bucketed duration histogram. Used both keyed by `tool_name`
/// (the `tool_invocations` map, replacing the old plain `AtomicU64`) and
/// keyed by `(tool_name, client_name)` (the `tool_telemetry_by_client`
/// map). All increments are `Relaxed`: this is observability-only data
/// with no happens-before requirement against other state.
///
/// Bucket spacing is exponential at 3× per step from 100 µs to 1000 s
/// plus an overflow bucket — yielding p50/p95/p99 estimates accurate
/// to within a 3× factor without any per-call allocation.
pub struct PerToolStats {
    pub count: AtomicU64,
    pub error_count: AtomicU64,
    pub duration_ns_sum: AtomicU64,
    pub duration_buckets: [AtomicU64; 16],
}

impl PerToolStats {
    /// Inclusive upper bounds (ns) for each duration bucket. Index 15
    /// is the overflow bucket (`u64::MAX`).
    pub const BUCKET_UPPER_NS: [u64; 16] = [
        100_000,
        300_000,
        1_000_000,
        3_000_000,
        10_000_000,
        30_000_000,
        100_000_000,
        300_000_000,
        1_000_000_000,
        3_000_000_000,
        10_000_000_000,
        30_000_000_000,
        100_000_000_000,
        300_000_000_000,
        1_000_000_000_000,
        u64::MAX,
    ];

    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            duration_ns_sum: AtomicU64::new(0),
            duration_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Record a single tool invocation. `duration_ns` is the wall-clock
    /// time the future took (including timeout if any). `ok = false`
    /// also increments `error_count`.
    pub fn record(&self, duration_ns: u64, ok: bool) {
        self.count.fetch_add(1, Ordering::Relaxed);
        if !ok {
            self.error_count.fetch_add(1, Ordering::Relaxed);
        }
        self.duration_ns_sum
            .fetch_add(duration_ns, Ordering::Relaxed);
        let bucket = Self::BUCKET_UPPER_NS
            .iter()
            .position(|&upper| duration_ns <= upper)
            .unwrap_or(15);
        self.duration_buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }

    /// Estimate the p-th percentile (p ∈ [0.0, 1.0]) duration in ns by
    /// finding the bucket whose cumulative count crosses `total * p`.
    /// Returns the bucket's upper bound; precision is therefore 3× the
    /// underlying bucket spacing.
    pub fn percentile_ns(&self, p: f64) -> u64 {
        let counts: [u64; 16] =
            std::array::from_fn(|i| self.duration_buckets[i].load(Ordering::Relaxed));
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return 0;
        }
        let target = ((total as f64) * p) as u64;
        let mut running = 0u64;
        for (i, &c) in counts.iter().enumerate() {
            running = running.saturating_add(c);
            if running >= target {
                return Self::BUCKET_UPPER_NS[i];
            }
        }
        Self::BUCKET_UPPER_NS[15]
    }

    /// JSON snapshot suitable for inclusion in `/api/status` and the
    /// `pgmcp://stats` MCP resource.
    pub fn snapshot(&self) -> serde_json::Value {
        let count = self.count.load(Ordering::Relaxed);
        let errors = self.error_count.load(Ordering::Relaxed);
        let sum_ns = self.duration_ns_sum.load(Ordering::Relaxed);
        let mean_ms = if count > 0 {
            (sum_ns as f64 / count as f64) / 1_000_000.0
        } else {
            0.0
        };
        let to_ms = |ns: u64| ns as f64 / 1_000_000.0;
        serde_json::json!({
            "count": count,
            "errors": errors,
            "mean_ms": mean_ms,
            "p50_ms": to_ms(self.percentile_ns(0.50)),
            "p95_ms": to_ms(self.percentile_ns(0.95)),
            "p99_ms": to_ms(self.percentile_ns(0.99)),
        })
    }
}

impl Default for PerToolStats {
    fn default() -> Self {
        Self::new()
    }
}

/// All statistics counters — fully lock-free.
pub struct StatsTracker {
    // Indexing counters
    pub files_indexed: AtomicU64,
    pub files_failed: AtomicU64,
    /// Files the scanner/watcher has submitted to the embed-pool's
    /// index channel — i.e. handed off but not yet processed. Together
    /// with `files_indexed` and `files_failed` this gives in-flight
    /// count: `submitted - indexed - failed`. Without it, the metrics
    /// surface only "what's done"; this counter exposes "what's still
    /// coming" so back-pressure on the bounded(batch*2) embed channel
    /// is observable.
    pub files_submitted: AtomicU64,
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

    // Memory-server Phase 0 counters
    pub memory_recall_prompts: AtomicU64,
    pub memory_search_mandates: AtomicU64,
    pub memory_mandate_supersessions: AtomicU64,

    // Memory-server Phase 1 (BGE-M3 migration) counters
    pub embeddings_migration_runs: AtomicU64,
    pub embeddings_migrated_file_chunks: AtomicU64,
    pub embeddings_migrated_session_prompts: AtomicU64,
    pub embeddings_migration_errors: AtomicU64,

    // Memory-server Phase 3 (official-compat CRUD) counters
    pub memory_entities_created: AtomicU64,
    pub memory_relations_created: AtomicU64,
    pub memory_observations_added: AtomicU64,
    pub memory_entities_deleted: AtomicU64,
    pub memory_observations_deleted: AtomicU64,
    pub memory_relations_deleted: AtomicU64,
    pub memory_read_graph_calls: AtomicU64,
    pub memory_search_nodes_calls: AtomicU64,
    pub memory_open_nodes_calls: AtomicU64,

    // Memory-server Phase 4 (LLM extractor / Stage B) counters
    pub memory_extractor_runs: AtomicU64,
    pub memory_extractor_errors: AtomicU64,
    pub memory_extractor_entities_written: AtomicU64,
    pub memory_extractor_relations_written: AtomicU64,
    pub memory_extractor_observations_written: AtomicU64,
    pub memory_extractor_contradictions_resolved: AtomicU64,

    // Memory-server Phase 5 (reflection) counters
    pub memory_reflection_runs_agent: AtomicU64,
    pub memory_reflection_runs_cron: AtomicU64,
    pub memory_reflection_facts_emitted: AtomicU64,
    pub memory_reflection_errors: AtomicU64,

    // Memory-server Phase 6 (graph-enhanced retrieval) counters
    pub graph_retrieval_latency_violations: AtomicU64,
    pub graph_retrieval_underperformance: AtomicU64,
    pub memory_raptor_build_runs: AtomicU64,
    pub memory_raptor_build_errors: AtomicU64,
    pub memory_raptor_summaries_written: AtomicU64,

    // Memory-server Phase 7 (reranker) counters
    pub memory_reranker_calls: AtomicU64,
    pub memory_reranker_errors: AtomicU64,

    // Memory-server Phase 8 (forget / retention) counters
    pub memory_forget_soft: AtomicU64,
    pub memory_forget_cascade: AtomicU64,
    pub memory_retention_entities_purged: AtomicU64,
    pub memory_retention_observations_purged: AtomicU64,
    pub memory_retention_relations_purged: AtomicU64,

    // Memory-server Phase 9 (eval harness) counters
    pub memory_eval_runs: AtomicU64,
    pub memory_eval_scenarios_passed: AtomicU64,
    pub memory_eval_scenarios_failed: AtomicU64,
    /// Sum of invariant violations across `memory_eval_invariants` rows
    /// (bi-temporal, supersession cycles, orphans, dangling forget log).
    pub memory_eval_invariant_violations: AtomicU64,

    // Memory-server Phase 11 (latent pipeline) counters.
    pub memory_latent_pipeline_runs: AtomicU64,
    pub memory_latent_pipeline_errors: AtomicU64,
    pub memory_latent_pipeline_fallbacks: AtomicU64,
    /// Approximate output tokens skipped vs the text-mediated pipeline
    /// (estimate: stage_A_output_tokens × 1 per fused call).
    pub memory_latent_tokens_saved: AtomicU64,
    /// Quality validator: total A/B comparisons recorded so far.
    pub memory_latent_quality_samples: AtomicU64,
    /// Quality validator: count of samples where the latent path
    /// scored strictly worse than the text path.
    pub memory_latent_quality_regressions: AtomicU64,
    /// Trainer: total backward steps completed so far across runs.
    pub memory_latent_train_steps: AtomicU64,

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
    /// Number of embedding workers that currently hold a live `Embedder`.
    /// Decrements when a worker exits (shutdown or permanent supervisor
    /// failure). A long-running value below the configured `pool_size`
    /// signals degraded throughput before user-visible latency rises.
    pub embed_workers_alive: AtomicU64,
    /// Total restart attempts the supervisor has performed across all
    /// workers (i.e., `Embedder::new` failures that were retried). Rises
    /// monotonically; never resets.
    pub embed_worker_restarts: AtomicU64,
    /// Number of worker slots the supervisor abandoned permanently after
    /// exceeding the consecutive-failure threshold. Once non-zero, the
    /// daemon needs operator attention (usually a CUDA reset or weights
    /// re-download).
    pub embed_worker_permanent_failures: AtomicU64,

    // File watcher counters
    pub watcher_events_received: AtomicU64,
    pub watcher_events_filtered: AtomicU64,
    pub watcher_events_debounced: AtomicU64,
    /// Total watcher errors of any kind delivered to the notify callback.
    /// Rises on inotify queue overflow, unmounted-path errors, transient
    /// filesystem hiccups. Monotonic.
    pub watcher_errors_total: AtomicU64,
    /// Subset of `watcher_errors_total` classified as inotify queue
    /// overflow. Each increment means events were dropped and the
    /// workspace index is now divergent from disk until the daemon
    /// restarts (full automatic re-arm is a follow-up).
    pub inotify_overflows_total: AtomicU64,
    /// `/api/search` 5xx responses returned to the Claude Code RAG
    /// hook. Distinct from `mcp_errors` so dashboards can attribute
    /// "RAG context injection quality" failures separately from
    /// general MCP tool errors.
    pub rag_search_failures_total: AtomicU64,
    /// Most recent outcome per named cron job. Keyed by task name (the
    /// `TaskMetadata::Named.name` field); un-named (`OneShot` /
    /// `Recurring`) tasks aren't tracked here because their identity is
    /// fungible. Updated synchronously inside `execute_inline` so a
    /// `/api/status` poll always sees the actual last run, not a
    /// race-window-old value.
    pub last_cron_outcomes: DashMap<String, CronJobStatus>,
    /// Cron jobs whose body classified a permanent fault
    /// (`CronAction::Disable`) and which the scheduler must skip on
    /// subsequent ticks. Cleared on daemon restart. The map value is a
    /// short human-readable reason (e.g. "git binary missing") that
    /// also surfaces in `/api/status` so operators can see why a job
    /// stopped without grepping logs.
    pub disabled_cron_jobs: DashMap<String, String>,
    /// Cumulative milliseconds the embed-pool workers spent inside
    /// `pool.begin()` for the chunk-insert transaction. Pairs with
    /// `embed_chunk_batches_total` (rises by 1 per file with non-zero
    /// chunks) to give an average per-batch transaction hold time —
    /// the operator's signal for whether transaction coalescing is
    /// trading too much pool contention for the reduced acquisition
    /// count. `Relaxed` everywhere; observability-only data.
    pub pool_pressure_ms_total: AtomicU64,
    /// Number of chunk-insert batches the embed-pool committed. One
    /// increment per file that produced at least one chunk.
    pub embed_chunk_batches_total: AtomicU64,

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

    // Document indexing counters — populated by `embed::pool` when
    // processing PDF/DOCX/EPUB/etc. files. `_skipped_no_tool` indicates a
    // missing CLI tool (poppler/ghostscript/pandoc) was needed; the
    // daemon's startup preflight names which tool to install.
    pub documents_skipped_no_tool: AtomicU64,
    pub documents_extraction_timeout: AtomicU64,
    /// Document extraction subprocess (`pandoc`/`pdftotext`/`ps2ascii`)
    /// died from a signal — typically because it exceeded the
    /// `max_extraction_subprocess_rss_bytes` rlimit or was OOM-killed.
    /// One increment per affected file.
    pub documents_extraction_oom: AtomicU64,
    pub documents_truncated: AtomicU64,
    /// Documents where `pdftotext` returned below the per-page text
    /// threshold and OCR (pdftoppm + tesseract) was triggered. One
    /// increment per affected file, whether OCR succeeded or fell back.
    pub documents_ocr_triggered: AtomicU64,
    /// OCR run was skipped because a cached result keyed on the PDF
    /// byte-hash was already present in `ocr_extractions`. One increment
    /// per cache hit.
    pub documents_ocr_cache_hits: AtomicU64,
    /// OCR run failed (pdftoppm/tesseract subprocess error, timeout, or
    /// empty output). The caller falls back to the sparse pdftotext
    /// output rather than raising. One increment per affected file.
    pub documents_ocr_failed: AtomicU64,
    /// Cumulative count of PDF pages successfully OCR'd across the
    /// daemon's lifetime. Tracks OCR throughput at coarse granularity.
    pub documents_ocr_pages_processed: AtomicU64,
    /// Cross-path duplicate detected at index time — second copy of the
    /// same content stored as a metadata-only `duplicate_of_file_id`
    /// pointer to the canonical row.
    pub documents_deduplicated: AtomicU64,
    /// Rename/relocation detected — content found at a previously
    /// known path that no longer exists on disk; the canonical row's
    /// path was updated in place without re-extracting or re-embedding.
    pub documents_renamed: AtomicU64,
    /// Canonical deleted while duplicates pointed at it — one duplicate
    /// was promoted to canonical and chunks were re-parented to it.
    pub documents_canonical_promoted: AtomicU64,
    /// Files where one or more NUL bytes (`\0`) were stripped from
    /// content or chunk text before SQL insertion. Postgres `TEXT`
    /// columns reject NUL bytes even though Rust `String` allows them;
    /// stripping is lossless because NUL carries no semantic information
    /// in any indexed text format. One increment per affected file.
    pub files_with_null_bytes_stripped: AtomicU64,
    /// Files where `indexed_files.content` was deliberately stored as
    /// `NULL` because the file is a plain-text language whose source
    /// lives on disk and `read_file` can recreate the bytes via
    /// `read_to_string` after content_hash verification. Distinct from
    /// `documents_truncated` (size-gated). One increment per affected
    /// indexer run per file (not cumulative across runs).
    pub files_with_content_omitted: AtomicU64,
    /// `read_file` MCP tool served content from disk after verifying
    /// `xxh3_64(disk_bytes) == content_hash`. The expected fast-path
    /// for plain-text files indexed under the asymmetric-storage
    /// policy.
    pub read_file_disk_hits: AtomicU64,
    /// `read_file` MCP tool detected a content_hash mismatch between
    /// the indexed row and the on-disk file. Means the file has
    /// changed since the last indexer pass; falls back to chunk
    /// stitching (which is also stale relative to disk).
    pub read_file_disk_hash_mismatches: AtomicU64,
    /// `read_file` MCP tool failed to `fs::read_to_string(path)` —
    /// file missing, permission error, encoding error. Falls back to
    /// chunk stitching.
    pub read_file_disk_io_errors: AtomicU64,
    /// `read_file` MCP tool stitched chunks because there was no
    /// inline content and either no recoverable-from-disk flag or
    /// the disk attempt failed/mismatched.
    pub read_file_chunk_stitches: AtomicU64,

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

    /// Per-tool telemetry, keyed by MCP tool name (e.g. `"semantic_search"`,
    /// `"grep"`, `"orient"`). Populated by `record_tool_call()` from the
    /// `instrumented_tool_wrap` helper in `src/mcp/server.rs` once per
    /// tool invocation, after the future has resolved. Carries call count,
    /// error count, cumulative duration, and a 16-bucket duration
    /// histogram so p50/p95/p99 can be derived without per-call allocation.
    /// Surfaced in `/api/status` as the `tool_invocations` map (count
    /// only, for back-compat) and as the new `tool_telemetry` map (full
    /// per-tool detail).
    pub tool_invocations: DashMap<String, PerToolStats>,
    /// Per-(tool, client_name) telemetry, keyed by
    /// `(tool_name, client_name_lowercased)` where client_name is sourced
    /// from `rmcp::Peer::peer_info().client_info.name` (e.g. `"claude-code"`,
    /// `"cursor"`). Defaults to `"unknown"` when the MCP `initialize`
    /// handshake has not been observed yet. Surfaced in `/api/status` as
    /// `tool_telemetry_by_client`.
    pub tool_telemetry_by_client: DashMap<(String, String), PerToolStats>,

    // ---- Telemetry writer (Tier 3 durable storage) ----
    /// Rows successfully INSERTed into `mcp_tool_calls`.
    pub telemetry_rows_written: AtomicU64,
    /// Rows dropped because the writer channel was full (back-pressure).
    pub telemetry_writes_dropped: AtomicU64,
    /// Rows discarded because the bulk-INSERT failed (DB error / connection loss).
    pub telemetry_writes_failed: AtomicU64,
    /// Rows purged by the daily `telemetry-retention` cron job.
    pub telemetry_rows_purged: AtomicU64,
    /// Channel sender to the telemetry writer task; `None` in CLI mode
    /// or before `start_telemetry_writer` has registered the writer.
    /// Wrapped in `ArcSwapOption` so the hot `instrumented_tool_wrap`
    /// path is lock-free.
    telemetry_tx: ArcSwapOption<mpsc::Sender<TelemetryRow>>,

    // Uptime
    pub uptime_start: Instant,
}

impl StatsTracker {
    pub fn new() -> Self {
        Self {
            files_indexed: AtomicU64::new(0),
            files_failed: AtomicU64::new(0),
            files_submitted: AtomicU64::new(0),
            files_aborted_fk: AtomicU64::new(0),
            chunks_embedded: AtomicU64::new(0),
            bytes_processed: AtomicU64::new(0),
            mcp_requests: AtomicU64::new(0),
            mcp_errors: AtomicU64::new(0),
            semantic_searches: AtomicU64::new(0),
            text_searches: AtomicU64::new(0),
            grep_searches: AtomicU64::new(0),
            commit_searches: AtomicU64::new(0),
            memory_recall_prompts: AtomicU64::new(0),
            memory_search_mandates: AtomicU64::new(0),
            memory_mandate_supersessions: AtomicU64::new(0),
            embeddings_migration_runs: AtomicU64::new(0),
            embeddings_migrated_file_chunks: AtomicU64::new(0),
            embeddings_migrated_session_prompts: AtomicU64::new(0),
            embeddings_migration_errors: AtomicU64::new(0),
            memory_entities_created: AtomicU64::new(0),
            memory_relations_created: AtomicU64::new(0),
            memory_observations_added: AtomicU64::new(0),
            memory_entities_deleted: AtomicU64::new(0),
            memory_observations_deleted: AtomicU64::new(0),
            memory_relations_deleted: AtomicU64::new(0),
            memory_read_graph_calls: AtomicU64::new(0),
            memory_search_nodes_calls: AtomicU64::new(0),
            memory_open_nodes_calls: AtomicU64::new(0),
            memory_extractor_runs: AtomicU64::new(0),
            memory_extractor_errors: AtomicU64::new(0),
            memory_extractor_entities_written: AtomicU64::new(0),
            memory_extractor_relations_written: AtomicU64::new(0),
            memory_extractor_observations_written: AtomicU64::new(0),
            memory_extractor_contradictions_resolved: AtomicU64::new(0),
            memory_reflection_runs_agent: AtomicU64::new(0),
            memory_reflection_runs_cron: AtomicU64::new(0),
            memory_reflection_facts_emitted: AtomicU64::new(0),
            memory_reflection_errors: AtomicU64::new(0),
            graph_retrieval_latency_violations: AtomicU64::new(0),
            graph_retrieval_underperformance: AtomicU64::new(0),
            memory_raptor_build_runs: AtomicU64::new(0),
            memory_raptor_build_errors: AtomicU64::new(0),
            memory_raptor_summaries_written: AtomicU64::new(0),
            memory_reranker_calls: AtomicU64::new(0),
            memory_reranker_errors: AtomicU64::new(0),
            memory_forget_soft: AtomicU64::new(0),
            memory_forget_cascade: AtomicU64::new(0),
            memory_retention_entities_purged: AtomicU64::new(0),
            memory_retention_observations_purged: AtomicU64::new(0),
            memory_retention_relations_purged: AtomicU64::new(0),
            memory_eval_runs: AtomicU64::new(0),
            memory_eval_scenarios_passed: AtomicU64::new(0),
            memory_eval_scenarios_failed: AtomicU64::new(0),
            memory_eval_invariant_violations: AtomicU64::new(0),
            memory_latent_pipeline_runs: AtomicU64::new(0),
            memory_latent_pipeline_errors: AtomicU64::new(0),
            memory_latent_pipeline_fallbacks: AtomicU64::new(0),
            memory_latent_tokens_saved: AtomicU64::new(0),
            memory_latent_quality_samples: AtomicU64::new(0),
            memory_latent_quality_regressions: AtomicU64::new(0),
            memory_latent_train_steps: AtomicU64::new(0),
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
            embed_workers_alive: AtomicU64::new(0),
            embed_worker_restarts: AtomicU64::new(0),
            embed_worker_permanent_failures: AtomicU64::new(0),
            watcher_errors_total: AtomicU64::new(0),
            inotify_overflows_total: AtomicU64::new(0),
            rag_search_failures_total: AtomicU64::new(0),
            last_cron_outcomes: DashMap::new(),
            disabled_cron_jobs: DashMap::new(),
            pool_pressure_ms_total: AtomicU64::new(0),
            embed_chunk_batches_total: AtomicU64::new(0),
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
            documents_skipped_no_tool: AtomicU64::new(0),
            documents_extraction_timeout: AtomicU64::new(0),
            documents_extraction_oom: AtomicU64::new(0),
            documents_truncated: AtomicU64::new(0),
            documents_ocr_triggered: AtomicU64::new(0),
            documents_ocr_cache_hits: AtomicU64::new(0),
            documents_ocr_failed: AtomicU64::new(0),
            documents_ocr_pages_processed: AtomicU64::new(0),
            documents_deduplicated: AtomicU64::new(0),
            documents_renamed: AtomicU64::new(0),
            documents_canonical_promoted: AtomicU64::new(0),
            files_with_null_bytes_stripped: AtomicU64::new(0),
            files_with_content_omitted: AtomicU64::new(0),
            read_file_disk_hits: AtomicU64::new(0),
            read_file_disk_hash_mismatches: AtomicU64::new(0),
            read_file_disk_io_errors: AtomicU64::new(0),
            read_file_chunk_stitches: AtomicU64::new(0),
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
            tool_telemetry_by_client: DashMap::new(),
            telemetry_rows_written: AtomicU64::new(0),
            telemetry_writes_dropped: AtomicU64::new(0),
            telemetry_writes_failed: AtomicU64::new(0),
            telemetry_rows_purged: AtomicU64::new(0),
            telemetry_tx: ArcSwapOption::empty(),
            uptime_start: Instant::now(),
        }
    }

    /// Register the telemetry writer's sender on this tracker so
    /// `instrumented_tool_wrap` can enqueue rows. Called once at daemon
    /// startup from `start_telemetry_writer`.
    pub fn set_telemetry_sender(&self, tx: mpsc::Sender<TelemetryRow>) {
        self.telemetry_tx.store(Some(std::sync::Arc::new(tx)));
    }

    /// Snapshot the current sender, or `None` if no writer is registered
    /// (CLI mode or `[metrics] telemetry_db_write_enabled = false`).
    pub fn telemetry_sender(&self) -> Option<std::sync::Arc<mpsc::Sender<TelemetryRow>>> {
        self.telemetry_tx.load_full()
    }

    /// Record a completed tool invocation. Called once per tool call
    /// from `instrumented_tool_wrap` in `src/mcp/server.rs` after the
    /// future resolves. Increments both the per-tool and the
    /// per-(tool, client) telemetry. `Relaxed` ordering throughout:
    /// observability-only data with no happens-before requirement.
    pub fn record_tool_call(&self, name: &str, client: &str, duration_ns: u64, ok: bool) {
        self.tool_invocations
            .entry(name.to_string())
            .or_default()
            .record(duration_ns, ok);
        self.tool_telemetry_by_client
            .entry((name.to_string(), client.to_string()))
            .or_default()
            .record(duration_ns, ok);
    }

    /// Update the last-known status of a named cron job. Called by
    /// `CronStateMachine::execute_inline` once per task completion or
    /// panic. Un-named tasks (OneShot / Recurring without a name) are
    /// skipped — their identity is fungible and aggregating them under
    /// one key would just be noise.
    pub fn record_cron_outcome(&self, name: &str, outcome: CronJobOutcome, duration_ms: u64) {
        if name == "one-shot" || name == "recurring" {
            return;
        }
        self.last_cron_outcomes.insert(
            name.to_string(),
            CronJobStatus {
                outcome,
                at: Utc::now(),
                duration_ms,
            },
        );
    }

    /// Mark a named cron job as permanently disabled until daemon
    /// restart. Idempotent — later calls overwrite the reason string.
    /// Un-named tasks are skipped (same reasoning as
    /// `record_cron_outcome`).
    pub fn disable_cron_job(&self, name: &str, reason: impl Into<String>) {
        if name == "one-shot" || name == "recurring" {
            return;
        }
        self.disabled_cron_jobs
            .insert(name.to_string(), reason.into());
    }

    /// True if this cron job has been disabled. Checked by the scheduler
    /// before running each task body.
    pub fn is_cron_job_disabled(&self, name: &str) -> bool {
        self.disabled_cron_jobs.contains_key(name)
    }

    /// Serialize `last_cron_outcomes` as a JSON object suitable for the
    /// top-level snapshot.
    fn cron_outcomes_snapshot(&self) -> serde_json::Value {
        let mut map = serde_json::Map::with_capacity(self.last_cron_outcomes.len());
        for entry in self.last_cron_outcomes.iter() {
            let v = entry.value();
            map.insert(
                entry.key().clone(),
                serde_json::json!({
                    "outcome": v.outcome.as_str(),
                    "at": v.at.to_rfc3339(),
                    "duration_ms": v.duration_ms,
                }),
            );
        }
        serde_json::Value::Object(map)
    }

    /// Serialize `disabled_cron_jobs` as a JSON object `{ name: reason }`.
    fn disabled_cron_jobs_snapshot(&self) -> serde_json::Value {
        let mut map = serde_json::Map::with_capacity(self.disabled_cron_jobs.len());
        for entry in self.disabled_cron_jobs.iter() {
            map.insert(
                entry.key().clone(),
                serde_json::Value::String(entry.value().clone()),
            );
        }
        serde_json::Value::Object(map)
    }

    /// Get a JSON snapshot of all counters.
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "files_indexed": self.files_indexed.load(Ordering::Acquire),
            "files_failed": self.files_failed.load(Ordering::Acquire),
            "files_submitted": self.files_submitted.load(Ordering::Acquire),
            "files_aborted_fk": self.files_aborted_fk.load(Ordering::Acquire),
            "chunks_embedded": self.chunks_embedded.load(Ordering::Acquire),
            "bytes_processed": self.bytes_processed.load(Ordering::Acquire),
            "mcp_requests": self.mcp_requests.load(Ordering::Acquire),
            "mcp_errors": self.mcp_errors.load(Ordering::Acquire),
            "semantic_searches": self.semantic_searches.load(Ordering::Acquire),
            "text_searches": self.text_searches.load(Ordering::Acquire),
            "grep_searches": self.grep_searches.load(Ordering::Acquire),
            "commit_searches": self.commit_searches.load(Ordering::Acquire),
            "memory_recall_prompts": self.memory_recall_prompts.load(Ordering::Acquire),
            "memory_search_mandates": self.memory_search_mandates.load(Ordering::Acquire),
            "memory_mandate_supersessions": self.memory_mandate_supersessions.load(Ordering::Acquire),
            "embeddings_migration_runs": self.embeddings_migration_runs.load(Ordering::Acquire),
            "embeddings_migrated_file_chunks": self.embeddings_migrated_file_chunks.load(Ordering::Acquire),
            "embeddings_migrated_session_prompts": self.embeddings_migrated_session_prompts.load(Ordering::Acquire),
            "embeddings_migration_errors": self.embeddings_migration_errors.load(Ordering::Acquire),
            "memory_entities_created": self.memory_entities_created.load(Ordering::Acquire),
            "memory_relations_created": self.memory_relations_created.load(Ordering::Acquire),
            "memory_observations_added": self.memory_observations_added.load(Ordering::Acquire),
            "memory_entities_deleted": self.memory_entities_deleted.load(Ordering::Acquire),
            "memory_observations_deleted": self.memory_observations_deleted.load(Ordering::Acquire),
            "memory_relations_deleted": self.memory_relations_deleted.load(Ordering::Acquire),
            "memory_read_graph_calls": self.memory_read_graph_calls.load(Ordering::Acquire),
            "memory_search_nodes_calls": self.memory_search_nodes_calls.load(Ordering::Acquire),
            "memory_open_nodes_calls": self.memory_open_nodes_calls.load(Ordering::Acquire),
            "memory_extractor_runs": self.memory_extractor_runs.load(Ordering::Acquire),
            "memory_extractor_errors": self.memory_extractor_errors.load(Ordering::Acquire),
            "memory_extractor_entities_written": self.memory_extractor_entities_written.load(Ordering::Acquire),
            "memory_extractor_relations_written": self.memory_extractor_relations_written.load(Ordering::Acquire),
            "memory_extractor_observations_written": self.memory_extractor_observations_written.load(Ordering::Acquire),
            "memory_extractor_contradictions_resolved": self.memory_extractor_contradictions_resolved.load(Ordering::Acquire),
            "memory_reflection_runs_agent": self.memory_reflection_runs_agent.load(Ordering::Acquire),
            "memory_reflection_runs_cron": self.memory_reflection_runs_cron.load(Ordering::Acquire),
            "memory_reflection_facts_emitted": self.memory_reflection_facts_emitted.load(Ordering::Acquire),
            "memory_reflection_errors": self.memory_reflection_errors.load(Ordering::Acquire),
            "graph_retrieval_latency_violations": self.graph_retrieval_latency_violations.load(Ordering::Acquire),
            "graph_retrieval_underperformance": self.graph_retrieval_underperformance.load(Ordering::Acquire),
            "memory_raptor_build_runs": self.memory_raptor_build_runs.load(Ordering::Acquire),
            "memory_raptor_build_errors": self.memory_raptor_build_errors.load(Ordering::Acquire),
            "memory_raptor_summaries_written": self.memory_raptor_summaries_written.load(Ordering::Acquire),
            "memory_reranker_calls": self.memory_reranker_calls.load(Ordering::Acquire),
            "memory_reranker_errors": self.memory_reranker_errors.load(Ordering::Acquire),
            "memory_forget_soft": self.memory_forget_soft.load(Ordering::Acquire),
            "memory_forget_cascade": self.memory_forget_cascade.load(Ordering::Acquire),
            "memory_retention_entities_purged": self.memory_retention_entities_purged.load(Ordering::Acquire),
            "memory_retention_observations_purged": self.memory_retention_observations_purged.load(Ordering::Acquire),
            "memory_retention_relations_purged": self.memory_retention_relations_purged.load(Ordering::Acquire),
            "memory_eval_runs": self.memory_eval_runs.load(Ordering::Acquire),
            "memory_eval_scenarios_passed": self.memory_eval_scenarios_passed.load(Ordering::Acquire),
            "memory_eval_scenarios_failed": self.memory_eval_scenarios_failed.load(Ordering::Acquire),
            "memory_eval_invariant_violations": self.memory_eval_invariant_violations.load(Ordering::Acquire),
            "memory_latent_pipeline_runs": self.memory_latent_pipeline_runs.load(Ordering::Acquire),
            "memory_latent_pipeline_errors": self.memory_latent_pipeline_errors.load(Ordering::Acquire),
            "memory_latent_pipeline_fallbacks": self.memory_latent_pipeline_fallbacks.load(Ordering::Acquire),
            "memory_latent_tokens_saved": self.memory_latent_tokens_saved.load(Ordering::Acquire),
            "memory_latent_quality_samples": self.memory_latent_quality_samples.load(Ordering::Acquire),
            "memory_latent_quality_regressions": self.memory_latent_quality_regressions.load(Ordering::Acquire),
            "memory_latent_train_steps": self.memory_latent_train_steps.load(Ordering::Acquire),
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
            "embed_workers_alive": self.embed_workers_alive.load(Ordering::Acquire),
            "embed_worker_restarts": self.embed_worker_restarts.load(Ordering::Acquire),
            "embed_worker_permanent_failures": self.embed_worker_permanent_failures.load(Ordering::Acquire),
            "watcher_errors_total": self.watcher_errors_total.load(Ordering::Acquire),
            "inotify_overflows_total": self.inotify_overflows_total.load(Ordering::Acquire),
            "rag_search_failures_total": self.rag_search_failures_total.load(Ordering::Acquire),
            "last_cron_outcomes": self.cron_outcomes_snapshot(),
            "disabled_cron_jobs": self.disabled_cron_jobs_snapshot(),
            "pool_pressure_ms_total": self.pool_pressure_ms_total.load(Ordering::Acquire),
            "embed_chunk_batches_total": self.embed_chunk_batches_total.load(Ordering::Acquire),
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
            "documents_skipped_no_tool": self.documents_skipped_no_tool.load(Ordering::Acquire),
            "documents_extraction_timeout": self.documents_extraction_timeout.load(Ordering::Acquire),
            "documents_extraction_oom": self.documents_extraction_oom.load(Ordering::Acquire),
            "documents_truncated": self.documents_truncated.load(Ordering::Acquire),
            "documents_ocr_triggered": self.documents_ocr_triggered.load(Ordering::Acquire),
            "documents_ocr_cache_hits": self.documents_ocr_cache_hits.load(Ordering::Acquire),
            "documents_ocr_failed": self.documents_ocr_failed.load(Ordering::Acquire),
            "documents_ocr_pages_processed": self.documents_ocr_pages_processed.load(Ordering::Acquire),
            "documents_deduplicated": self.documents_deduplicated.load(Ordering::Acquire),
            "documents_renamed": self.documents_renamed.load(Ordering::Acquire),
            "documents_canonical_promoted": self.documents_canonical_promoted.load(Ordering::Acquire),
            "files_with_null_bytes_stripped": self.files_with_null_bytes_stripped.load(Ordering::Acquire),
            "files_with_content_omitted": self.files_with_content_omitted.load(Ordering::Acquire),
            "read_file_disk_hits": self.read_file_disk_hits.load(Ordering::Acquire),
            "read_file_disk_hash_mismatches": self.read_file_disk_hash_mismatches.load(Ordering::Acquire),
            "read_file_disk_io_errors": self.read_file_disk_io_errors.load(Ordering::Acquire),
            "read_file_chunk_stitches": self.read_file_chunk_stitches.load(Ordering::Acquire),
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
            "telemetry_rows_written": self.telemetry_rows_written.load(Ordering::Acquire),
            "telemetry_writes_dropped": self.telemetry_writes_dropped.load(Ordering::Acquire),
            "telemetry_writes_failed": self.telemetry_writes_failed.load(Ordering::Acquire),
            "telemetry_rows_purged": self.telemetry_rows_purged.load(Ordering::Acquire),
            "tool_invocations": serde_json::Value::Object(
                self.tool_invocations.iter()
                    .map(|e| (e.key().clone(), serde_json::Value::from(e.value().count.load(Ordering::Relaxed))))
                    .collect()
            ),
            "tool_telemetry": serde_json::Value::Object(
                self.tool_invocations.iter()
                    .map(|e| (e.key().clone(), e.value().snapshot()))
                    .collect()
            ),
            "tool_telemetry_by_client": serde_json::Value::Array(
                self.tool_telemetry_by_client.iter()
                    .map(|e| {
                        let (tool, client) = e.key();
                        let mut obj = e.value().snapshot();
                        if let Some(map) = obj.as_object_mut() {
                            map.insert("tool".to_string(), serde_json::Value::String(tool.clone()));
                            map.insert("client".to_string(), serde_json::Value::String(client.clone()));
                        }
                        obj
                    })
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
    fn record_cron_outcome_skips_unnamed_tasks() {
        let stats = StatsTracker::new();
        stats.record_cron_outcome("one-shot", CronJobOutcome::Ok, 5);
        stats.record_cron_outcome("recurring", CronJobOutcome::Panicked, 5);
        assert_eq!(stats.last_cron_outcomes.len(), 0);
    }

    #[test]
    fn record_cron_outcome_overwrites_previous_status_per_name() {
        let stats = StatsTracker::new();
        stats.record_cron_outcome("graph-analysis", CronJobOutcome::Ok, 100);
        stats.record_cron_outcome("graph-analysis", CronJobOutcome::Panicked, 50);
        assert_eq!(stats.last_cron_outcomes.len(), 1);
        let entry = stats
            .last_cron_outcomes
            .get("graph-analysis")
            .expect("entry present");
        assert_eq!(entry.value().outcome, CronJobOutcome::Panicked);
        assert_eq!(entry.value().duration_ms, 50);
    }

    #[test]
    fn disable_cron_job_records_reason_and_is_observable() {
        let stats = StatsTracker::new();
        assert!(!stats.is_cron_job_disabled("git-history-index"));
        stats.disable_cron_job("git-history-index", "git binary missing");
        assert!(stats.is_cron_job_disabled("git-history-index"));
        let reason = stats
            .disabled_cron_jobs
            .get("git-history-index")
            .expect("entry present")
            .value()
            .clone();
        assert_eq!(reason, "git binary missing");
    }

    #[test]
    fn disable_cron_job_skips_unnamed_tasks() {
        let stats = StatsTracker::new();
        stats.disable_cron_job("one-shot", "should be ignored");
        stats.disable_cron_job("recurring", "should be ignored");
        assert!(!stats.is_cron_job_disabled("one-shot"));
        assert!(!stats.is_cron_job_disabled("recurring"));
        assert_eq!(stats.disabled_cron_jobs.len(), 0);
    }

    #[test]
    fn disabled_cron_jobs_appear_in_snapshot_json() {
        let stats = StatsTracker::new();
        stats.disable_cron_job("similarity-scan", "ENOSPC");
        let snap = stats.snapshot();
        let map = snap
            .get("disabled_cron_jobs")
            .and_then(|v| v.as_object())
            .expect("snapshot has disabled_cron_jobs object");
        assert_eq!(
            map.get("similarity-scan").and_then(|v| v.as_str()),
            Some("ENOSPC"),
        );
    }

    #[test]
    fn cron_outcomes_appear_in_snapshot_json() {
        let stats = StatsTracker::new();
        stats.record_cron_outcome("topic-clustering", CronJobOutcome::Ok, 42);
        let snap = stats.snapshot();
        let outcomes = snap
            .get("last_cron_outcomes")
            .and_then(|v| v.as_object())
            .expect("snapshot has last_cron_outcomes object");
        let entry = outcomes
            .get("topic-clustering")
            .and_then(|v| v.as_object())
            .expect("topic-clustering entry");
        assert_eq!(entry.get("outcome").and_then(|v| v.as_str()), Some("ok"));
        assert_eq!(entry.get("duration_ms").and_then(|v| v.as_u64()), Some(42));
        assert!(entry.contains_key("at"));
    }

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
