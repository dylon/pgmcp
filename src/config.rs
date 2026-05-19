use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::Value as TomlValue;

use crate::error::{PgmcpError, Result};

/// Top-level configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub indexer: IndexerConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
    #[serde(default)]
    pub vector: VectorConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub work_pool: WorkPoolConfig,
    #[serde(default)]
    pub cron: CronConfig,
    #[serde(default)]
    pub system: SystemConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
}

/// Memory-server configuration. Holds Phase 4+ knobs grouped under
/// `[memory.*]` in the TOML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MemoryConfig {
    #[serde(default)]
    pub extractor: MemoryExtractorConfig,
    #[serde(default)]
    pub reflection: MemoryReflectionConfig,
    #[serde(default)]
    pub retention: MemoryRetentionConfig,
    #[serde(default)]
    pub graph_rag: MemoryGraphRagConfig,
    #[serde(default)]
    pub eval: MemoryEvalConfig,
    #[serde(default)]
    pub latent_pipeline: MemoryLatentPipelineConfig,
}

/// `[memory.latent_pipeline]` — Phase 11 RecursiveLink hand-off
/// between same-backbone pipeline stages. Default `Disabled` per the
/// plan §11.3: the pipeline is opt-in once the operator has (a) the
/// hardware budget and (b) a trained RecursiveLink weights file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryLatentPipelineConfig {
    /// `"qwen3-rlv1"` or `"disabled"`. Default disabled.
    #[serde(default = "default_latent_backend")]
    pub backend: String,
    /// Path to the trained RecursiveLink safetensors file.
    /// Recommended: `models/recursive_link_qwen3_8b.safetensors`.
    #[serde(default = "default_latent_link_path")]
    pub link_weights_path: std::path::PathBuf,
    /// Signature stamped on the weights — bump when retraining with a
    /// new prompt template or backbone variant. Stored alongside
    /// `latent_pipeline_active` in `pgmcp_metadata`.
    #[serde(default = "default_latent_link_signature")]
    pub link_signature: String,
    /// Auto-downgrade threshold: when the daily quality validator
    /// detects `(text_score − latent_score) > quality_regression_threshold`
    /// over a `regression_window` days, the dispatcher demotes back
    /// to the text path. Default 0.05 — a 5-pp absolute quality
    /// regression triggers the downgrade.
    #[serde(default = "default_latent_quality_threshold")]
    pub quality_regression_threshold: f32,
    /// Days of A/B comparison data the validator looks at when deciding.
    #[serde(default = "default_latent_regression_window")]
    pub regression_window_days: i64,
    /// When `true`, the dispatcher attempts a short forward-pass on
    /// startup to confirm VRAM headroom; failure → demote to text.
    #[serde(default = "default_true")]
    pub vram_probe_at_startup: bool,
    #[serde(default)]
    pub train: MemoryLatentTrainConfig,
}

impl Default for MemoryLatentPipelineConfig {
    fn default() -> Self {
        Self {
            backend: default_latent_backend(),
            link_weights_path: default_latent_link_path(),
            link_signature: default_latent_link_signature(),
            quality_regression_threshold: default_latent_quality_threshold(),
            regression_window_days: default_latent_regression_window(),
            vram_probe_at_startup: true,
            train: MemoryLatentTrainConfig::default(),
        }
    }
}

fn default_latent_backend() -> String {
    "disabled".into()
}
fn default_latent_link_path() -> std::path::PathBuf {
    std::path::PathBuf::from("models/recursive_link_qwen3_8b.safetensors")
}
fn default_latent_link_signature() -> String {
    "rlv1".into()
}
fn default_latent_quality_threshold() -> f32 {
    0.05
}
fn default_latent_regression_window() -> i64 {
    7
}

/// `[memory.latent_pipeline.train]` — one-shot trainer settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryLatentTrainConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_latent_train_samples")]
    pub samples_from_session_prompts: usize,
    #[serde(default = "default_latent_train_epochs")]
    pub epochs: usize,
    #[serde(default = "default_latent_train_batch")]
    pub batch_size: usize,
    #[serde(default = "default_latent_train_lr")]
    pub learning_rate: f64,
    #[serde(default = "default_latent_train_seqcap")]
    pub seq_len_cap: usize,
    /// Output path for the trained safetensors file.
    #[serde(default = "default_latent_link_path")]
    pub output_path: std::path::PathBuf,
}

impl Default for MemoryLatentTrainConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            samples_from_session_prompts: default_latent_train_samples(),
            epochs: default_latent_train_epochs(),
            batch_size: default_latent_train_batch(),
            learning_rate: default_latent_train_lr(),
            seq_len_cap: default_latent_train_seqcap(),
            output_path: default_latent_link_path(),
        }
    }
}

fn default_latent_train_samples() -> usize {
    10_000
}
fn default_latent_train_epochs() -> usize {
    3
}
fn default_latent_train_batch() -> usize {
    1
}
fn default_latent_train_lr() -> f64 {
    5e-4
}
fn default_latent_train_seqcap() -> usize {
    1024
}

/// `[memory.eval]` — Phase 9 internal eval harness. The MCP-visible
/// scenarios live in `pgmcp-testing/tests/memory_eval.rs` and run as
/// part of `cargo test` / `scripts/verify.sh`. The cron variant
/// additionally records bi-temporal + provenance invariants into
/// `pgmcp_metadata` on a schedule, so a long-running daemon can detect
/// drift between deploys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEvalConfig {
    /// When `false` (default) the periodic invariant scan is skipped
    /// entirely. The integration test suite is always built.
    #[serde(default)]
    pub cron_enabled: bool,
    #[serde(default = "default_memory_eval_interval_secs")]
    pub cron_interval_secs: u64,
    /// Hard cap on rows examined per invariant pass. Keeps the scan
    /// O(N) bounded even on a million-row memory graph.
    #[serde(default = "default_memory_eval_row_cap")]
    pub row_cap: i64,
}

impl Default for MemoryEvalConfig {
    fn default() -> Self {
        Self {
            cron_enabled: false,
            cron_interval_secs: default_memory_eval_interval_secs(),
            row_cap: default_memory_eval_row_cap(),
        }
    }
}

fn default_memory_eval_interval_secs() -> u64 {
    86400
}

fn default_memory_eval_row_cap() -> i64 {
    50_000
}

/// `[memory.extractor]` — LLM-driven salience extraction (Phase 4).
/// Default backend is `disabled` so a stock pgmcp install does not
/// touch the LLM path until the operator opts in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryExtractorConfig {
    /// One of: `qwen3-8b`, `qwen3-4b`, `cloud`, `disabled`.
    #[serde(default = "default_extractor_backend")]
    pub backend: String,
    /// Stage-B debounce per session, in seconds. Stops a flurry of
    /// quick prompts from spamming the GPU.
    #[serde(default = "default_extractor_debounce_secs")]
    pub inline_debounce_secs: u64,
    /// LLM-judged importance threshold for auto-promotion into
    /// `memory_*`. Facts below the threshold are emitted but stamped
    /// with a lower importance (the entity row's `importance` column
    /// reflects the LLM's score directly).
    #[serde(default = "default_extractor_auto_promote_threshold")]
    pub auto_promote_threshold: f32,
    /// Schema-validation strictness: `"strict"` rejects any parse
    /// failure (default); `"lenient"` keeps best-effort parses.
    #[serde(default = "default_extractor_schema_validation")]
    pub schema_validation: String,
}

impl Default for MemoryExtractorConfig {
    fn default() -> Self {
        Self {
            backend: default_extractor_backend(),
            inline_debounce_secs: default_extractor_debounce_secs(),
            auto_promote_threshold: default_extractor_auto_promote_threshold(),
            schema_validation: default_extractor_schema_validation(),
        }
    }
}

fn default_extractor_backend() -> String {
    "disabled".into()
}
fn default_extractor_debounce_secs() -> u64 {
    30
}
fn default_extractor_auto_promote_threshold() -> f32 {
    0.6
}
fn default_extractor_schema_validation() -> String {
    "strict".into()
}

/// `[memory.reflection]` — agent-driven + cron reflection (Phase 5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryReflectionConfig {
    /// The MCP tool `memory_reflect` is always wired; this flag controls
    /// whether the daemon refuses agent calls (off when the operator
    /// wants to avoid LLM spend even with the tool present).
    #[serde(default = "default_true")]
    pub agent_enabled: bool,
    /// Whether the periodic `memory-reflect` cron runs.
    #[serde(default)]
    pub cron_enabled: bool,
    #[serde(default = "default_reflection_cron_interval")]
    pub cron_interval_secs: u64,
    /// Don't reflect on a scope that has fewer than this many new
    /// observations since the last reflection — avoid wasting calls.
    #[serde(default = "default_reflection_min_new")]
    pub min_new_observations: i64,
    /// Max observations included as grounding context for one
    /// reflection call. Bounded by the prompt size budget.
    #[serde(default = "default_reflection_window")]
    pub max_observations: i64,
}

impl Default for MemoryReflectionConfig {
    fn default() -> Self {
        Self {
            agent_enabled: true,
            cron_enabled: false,
            cron_interval_secs: default_reflection_cron_interval(),
            min_new_observations: default_reflection_min_new(),
            max_observations: default_reflection_window(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_reflection_cron_interval() -> u64 {
    86400
}
fn default_reflection_min_new() -> i64 {
    50
}
fn default_reflection_window() -> i64 {
    200
}

/// `[memory.retention]` — Phase 8 eviction policy. Stub config now so
/// the TOML accepts the section even though the cron lands in Phase 8.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRetentionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_retention_window_days")]
    pub window_days: i64,
    #[serde(default = "default_retention_importance")]
    pub importance_threshold: f32,
}

impl Default for MemoryRetentionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_days: default_retention_window_days(),
            importance_threshold: default_retention_importance(),
        }
    }
}

fn default_retention_window_days() -> i64 {
    90
}
fn default_retention_importance() -> f32 {
    0.3
}

/// `[memory.graph_rag]` — Phase 6.3–6.5 graph retrieval gating.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryGraphRagConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_graph_rag_max_latency")]
    pub max_latency_ms: i64,
    #[serde(default = "default_graph_rag_path_max_hops")]
    pub path_search_default_max_hops: i32,
    #[serde(default = "default_graph_rag_prune_jaccard")]
    pub path_search_prune_jaccard: f32,
}

impl Default for MemoryGraphRagConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_latency_ms: default_graph_rag_max_latency(),
            path_search_default_max_hops: default_graph_rag_path_max_hops(),
            path_search_prune_jaccard: default_graph_rag_prune_jaccard(),
        }
    }
}

fn default_graph_rag_max_latency() -> i64 {
    500
}
fn default_graph_rag_path_max_hops() -> i32 {
    3
}
fn default_graph_rag_prune_jaccard() -> f32 {
    0.7
}

/// Process-level resource budgets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SystemConfig {
    /// Aggregate RSS budget in MiB. The pool monitors use this as the
    /// `rss_pressure_score` denominator; when the daemon's RSS climbs
    /// past 50% of the budget, the hill-climber's RSS term starts
    /// discouraging unparking, and past 100% it actively parks workers.
    ///
    /// `0` (the default) disables RSS sensing — the climber falls back
    /// to the original two-term throughput-vs-queue-depth behavior.
    /// In daemon startup we resolve `0` to 80% of `MemAvailable` at
    /// boot time.
    #[serde(default)]
    pub rss_limit_mib: u64,
}

impl SystemConfig {
    /// Resolve `rss_limit_mib` to bytes. `0` (auto) returns 80% of
    /// `MemAvailable` at the time of the call, or `0` if /proc/meminfo
    /// is unreadable (in which case RSS sensing stays off — safe
    /// default rather than a wrong limit).
    pub fn resolved_rss_limit_bytes(&self) -> u64 {
        if self.rss_limit_mib > 0 {
            return self.rss_limit_mib * 1024 * 1024;
        }
        crate::stats::rss::mem_available_bytes()
            .map(|avail| avail * 4 / 5)
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(default = "default_workspace_paths")]
    pub paths: Vec<String>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            paths: default_workspace_paths(),
        }
    }
}

fn default_workspace_paths() -> Vec<String> {
    vec![]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileTypeMapping {
    pub extension: String,
    pub language: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexerConfig {
    #[serde(default = "default_file_types")]
    pub file_types: Vec<FileTypeMapping>,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default = "default_max_file_size")]
    pub max_file_size_bytes: u64,
    #[serde(default = "default_exclude_patterns")]
    pub exclude_patterns: Vec<String>,
    /// Source-form preference for the per-directory dedup pass. When a
    /// directory contains multiple files sharing the same stem (e.g.
    /// `invoice.org` + `invoice.tex` + `invoice.pdf`), the scanner enqueues
    /// only the entry whose extension appears earliest in this list.
    /// Extensions not listed are kept unconditionally.
    #[serde(default = "default_source_priority")]
    pub source_priority: Vec<String>,
    /// Per-file source-byte cap for binary document formats (PDF, DOCX,
    /// EPUB, etc.). The default 1 MiB `max_file_size_bytes` is too small
    /// for typical academic PDFs and would Level-1-skip them; document
    /// languages use this separate cap instead.
    #[serde(default = "default_max_document_source_bytes")]
    pub max_document_source_bytes: u64,
    /// Cap on the extracted-text size held in memory per document. The
    /// subprocess extractors stop reading child stdout at this byte count
    /// and set `truncated = true` rather than fail outright.
    #[serde(default = "default_max_extracted_text_bytes")]
    pub max_extracted_text_bytes: usize,
    /// Per-file timeout for the document extraction subprocess
    /// (`pdftotext`, `ps2ascii`, `pandoc`). Past this, the child is
    /// killed and the file is counted as `documents_extraction_timeout`.
    #[serde(default = "default_document_extraction_timeout_secs")]
    pub document_extraction_timeout_secs: u64,
    /// Hard cap on the address-space size (RLIMIT_AS) of any document
    /// extraction subprocess. Default 4 GiB. Guards against runaway
    /// allocators in `pandoc` / `pdftotext` / `ps2ascii` — a 2026-05-13
    /// pandoc invocation grew to 68 GiB RSS on a single input and got
    /// OOM-killed, taking the daemon's logging task with it. Setting to
    /// `0` disables the limit.
    #[serde(default = "default_max_extraction_subprocess_rss_bytes")]
    pub max_extraction_subprocess_rss_bytes: u64,
    /// Master switch for OCR fallback when `pdftotext` produces sparse
    /// text. When `true` (default), scanned/image-only PDFs are rasterized
    /// with `pdftoppm` and passed through `tesseract` per page; cached by
    /// content_hash so re-runs reuse the OCR output.
    #[serde(default = "default_ocr_enabled")]
    pub ocr_enabled: bool,
    /// Per-page character threshold below which OCR is triggered.
    /// Trigger formula: `pdftotext_chars < ocr_min_text_chars_per_page * page_count`.
    /// 200 chars/page admits sparse but real text (cover pages, single-paragraph
    /// figures) while catching mostly-empty pdftotext output from scans.
    #[serde(default = "default_ocr_min_text_chars_per_page")]
    pub ocr_min_text_chars_per_page: usize,
    /// Hard cap on pages OCRed per document. Protects against a 1000-page
    /// scanned PDF burning hours of CPU. Output beyond this is omitted and
    /// `truncated = true` is set on the result.
    #[serde(default = "default_ocr_max_pages")]
    pub ocr_max_pages: usize,
    /// Rasterization DPI passed to `pdftoppm -r`. 300 is the OCR
    /// industry-standard balance between accuracy and tempdir footprint.
    #[serde(default = "default_ocr_dpi")]
    pub ocr_dpi: u32,
    /// Tesseract language traineddata identifiers. `["eng"]` is the
    /// default; `["eng", "fra"]` joins with `+` for multi-language pages.
    #[serde(default = "default_ocr_languages")]
    pub ocr_languages: Vec<String>,
    /// Per-document wall-clock budget for the full OCR run (rasterize +
    /// all pages). When exceeded, the run is cut short, partial text is
    /// returned, and `truncated = true` is set.
    #[serde(default = "default_ocr_total_timeout_secs")]
    pub ocr_total_timeout_secs: u64,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            file_types: default_file_types(),
            debounce_ms: default_debounce_ms(),
            max_file_size_bytes: default_max_file_size(),
            exclude_patterns: default_exclude_patterns(),
            source_priority: default_source_priority(),
            max_document_source_bytes: default_max_document_source_bytes(),
            max_extracted_text_bytes: default_max_extracted_text_bytes(),
            document_extraction_timeout_secs: default_document_extraction_timeout_secs(),
            max_extraction_subprocess_rss_bytes: default_max_extraction_subprocess_rss_bytes(),
            ocr_enabled: default_ocr_enabled(),
            ocr_min_text_chars_per_page: default_ocr_min_text_chars_per_page(),
            ocr_max_pages: default_ocr_max_pages(),
            ocr_dpi: default_ocr_dpi(),
            ocr_languages: default_ocr_languages(),
            ocr_total_timeout_secs: default_ocr_total_timeout_secs(),
        }
    }
}

impl IndexerConfig {
    /// Build extension → language lookup map.
    #[allow(dead_code)]
    pub fn extension_map(&self) -> HashMap<String, String> {
        self.file_types
            .iter()
            .map(|ft| (ft.extension.clone(), ft.language.clone()))
            .collect()
    }

    /// Check if an extension is configured for indexing.
    pub fn is_configured_extension(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|ext| self.file_types.iter().any(|ft| ft.extension == ext))
            .unwrap_or(false)
    }

    /// Get the language for a file path, if configured.
    pub fn language_for_path(&self, path: &Path) -> Option<String> {
        path.extension().and_then(|e| e.to_str()).and_then(|ext| {
            self.file_types
                .iter()
                .find(|ft| ft.extension == ext)
                .map(|ft| ft.language.clone())
        })
    }
}

fn default_file_types() -> Vec<FileTypeMapping> {
    vec![
        FileTypeMapping {
            extension: "rs".into(),
            language: "rust".into(),
        },
        FileTypeMapping {
            extension: "md".into(),
            language: "markdown".into(),
        },
        FileTypeMapping {
            extension: "metta".into(),
            language: "metta".into(),
        },
        FileTypeMapping {
            extension: "rho".into(),
            language: "rholang".into(),
        },
        FileTypeMapping {
            extension: "js".into(),
            language: "javascript".into(),
        },
        FileTypeMapping {
            extension: "jsx".into(),
            language: "javascript".into(),
        },
        FileTypeMapping {
            extension: "py".into(),
            language: "python".into(),
        },
        FileTypeMapping {
            extension: "pl".into(),
            language: "prolog".into(),
        },
        FileTypeMapping {
            extension: "pro".into(),
            language: "prolog".into(),
        },
        FileTypeMapping {
            extension: "ts".into(),
            language: "typescript".into(),
        },
        // `.tsx` routes to the dedicated TSX backend
        // (`LanguageRegistry::for_language("tsx")` → `TSX_BACKEND`), not the
        // plain TS backend. Existing `.tsx` rows whose `indexed_files.language`
        // is `"typescript"` will keep that value until the next reindex.
        FileTypeMapping {
            extension: "tsx".into(),
            language: "tsx".into(),
        },
        FileTypeMapping {
            extension: "toml".into(),
            language: "toml".into(),
        },
        FileTypeMapping {
            extension: "json".into(),
            language: "json".into(),
        },
        FileTypeMapping {
            extension: "yaml".into(),
            language: "yaml".into(),
        },
        FileTypeMapping {
            extension: "yml".into(),
            language: "yaml".into(),
        },
        FileTypeMapping {
            extension: "sh".into(),
            language: "shell".into(),
        },
        FileTypeMapping {
            extension: "jsonl".into(),
            language: "jsonl".into(),
        },
        // Tier-0e tree-sitter backends — extensions added 2026-05-01 alongside
        // the symbol-extraction cron. Every language string here must
        // correspond to a `Some(...)` from `LanguageRegistry::for_language`.
        FileTypeMapping {
            extension: "java".into(),
            language: "java".into(),
        },
        FileTypeMapping {
            extension: "scala".into(),
            language: "scala".into(),
        },
        FileTypeMapping {
            extension: "c".into(),
            language: "c".into(),
        },
        FileTypeMapping {
            extension: "h".into(),
            language: "c".into(),
        },
        FileTypeMapping {
            extension: "cpp".into(),
            language: "cpp".into(),
        },
        FileTypeMapping {
            extension: "cc".into(),
            language: "cpp".into(),
        },
        FileTypeMapping {
            extension: "cxx".into(),
            language: "cpp".into(),
        },
        FileTypeMapping {
            extension: "hpp".into(),
            language: "cpp".into(),
        },
        FileTypeMapping {
            extension: "hxx".into(),
            language: "cpp".into(),
        },
        FileTypeMapping {
            extension: "clj".into(),
            language: "clojure".into(),
        },
        FileTypeMapping {
            extension: "cljs".into(),
            language: "clojurescript".into(),
        },
        // Document indexing extensions — extraction is routed through
        // `src/indexer/extract/` to system tools (`pdftotext`,
        // `ps2ascii`, `pandoc`). The `language` strings here are
        // deliberately unique from tree-sitter backend names so that
        // `parsing::LanguageRegistry::for_language` returns `None` for
        // them and the symbol-extraction / graph / import crons skip
        // these languages automatically.
        FileTypeMapping {
            extension: "pdf".into(),
            language: "pdf".into(),
        },
        FileTypeMapping {
            extension: "ps".into(),
            language: "postscript".into(),
        },
        FileTypeMapping {
            extension: "eps".into(),
            language: "postscript".into(),
        },
        FileTypeMapping {
            extension: "tex".into(),
            language: "latex".into(),
        },
        FileTypeMapping {
            extension: "latex".into(),
            language: "latex".into(),
        },
        FileTypeMapping {
            extension: "bib".into(),
            language: "bibtex".into(),
        },
        FileTypeMapping {
            extension: "org".into(),
            language: "org".into(),
        },
        FileTypeMapping {
            extension: "rst".into(),
            language: "rst".into(),
        },
        FileTypeMapping {
            extension: "docx".into(),
            language: "docx".into(),
        },
        FileTypeMapping {
            extension: "doc".into(),
            language: "doc".into(),
        },
        FileTypeMapping {
            extension: "rtf".into(),
            language: "rtf".into(),
        },
        FileTypeMapping {
            extension: "odt".into(),
            language: "odt".into(),
        },
        FileTypeMapping {
            extension: "epub".into(),
            language: "epub".into(),
        },
        FileTypeMapping {
            extension: "txt".into(),
            language: "text".into(),
        },
    ]
}

fn default_debounce_ms() -> u64 {
    300
}

fn default_max_file_size() -> u64 {
    1_048_576 // 1 MB
}

fn default_exclude_patterns() -> Vec<String> {
    vec![
        "node_modules".into(),
        "target".into(),
        ".git".into(),
        "__pycache__".into(),
        "*.lock".into(),
    ]
}

/// Hardcoded fallback priority for choosing one form when multiple sibling
/// files share the same `(parent_dir, file_stem)`. Earlier entries are
/// preferred. Overridable via `[indexer] source_priority = [...]` in the
/// global config and via `[indexer] source_priority = [...]` in a
/// per-project `.pgmcp.toml`.
pub const DEFAULT_SOURCE_PRIORITY: &[&str] = &[
    "org", "rst", "md", "tex", "latex", "docx", "epub", "odt", "rtf", "pdf", "ps", "eps", "doc",
];

fn default_source_priority() -> Vec<String> {
    DEFAULT_SOURCE_PRIORITY
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

fn default_max_document_source_bytes() -> u64 {
    100 * 1024 * 1024 // 100 MiB — covers virtually all academic PDFs and ebooks
}

fn default_max_extracted_text_bytes() -> usize {
    50 * 1024 * 1024 // 50 MiB of post-extraction text
}

fn default_max_extraction_subprocess_rss_bytes() -> u64 {
    4 * 1024 * 1024 * 1024 // 4 GiB
}

fn default_document_extraction_timeout_secs() -> u64 {
    30
}

fn default_ocr_enabled() -> bool {
    true
}

fn default_ocr_min_text_chars_per_page() -> usize {
    200
}

fn default_ocr_max_pages() -> usize {
    50
}

fn default_ocr_dpi() -> u32 {
    300
}

fn default_ocr_languages() -> Vec<String> {
    vec!["eng".to_string()]
}

fn default_ocr_total_timeout_secs() -> u64 {
    1800 // 30 minutes
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_host")]
    pub host: String,
    #[serde(default = "default_db_port")]
    pub port: u16,
    #[serde(default = "default_db_name")]
    pub name: String,
    #[serde(default = "default_db_user")]
    pub user: String,
    pub password: Option<String>,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            host: default_db_host(),
            port: default_db_port(),
            name: default_db_name(),
            user: default_db_user(),
            password: None,
            max_connections: default_max_connections(),
        }
    }
}

impl DatabaseConfig {
    /// Build the database connection URL.
    pub fn connection_url(&self) -> String {
        let password = self
            .password
            .clone()
            .or_else(|| std::env::var("PGMCP_DB_PASSWORD").ok())
            .unwrap_or_default();

        if password.is_empty() {
            format!(
                "postgres://{}@{}:{}/{}",
                self.user, self.host, self.port, self.name
            )
        } else {
            format!(
                "postgres://{}:{}@{}:{}/{}",
                self.user, password, self.host, self.port, self.name
            )
        }
    }

    /// Build the database connection URL with the password component
    /// replaced by `****`. Safe to log and to surface via the
    /// `pgmcp status` CLI / `/api/status` endpoint. Always returns the
    /// `:****@` form so a redacted URL is visually distinguishable
    /// from a passwordless one.
    pub fn connection_url_redacted(&self) -> String {
        format!(
            "postgres://{}:****@{}:{}/{}",
            self.user, self.host, self.port, self.name
        )
    }
}

fn default_db_host() -> String {
    "localhost".into()
}
fn default_db_port() -> u16 {
    5432
}
fn default_db_name() -> String {
    "pgmcp".into()
}
fn default_db_user() -> String {
    "pgmcp".into()
}
fn default_max_connections() -> u32 {
    20
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_dimensions")]
    pub dimensions: usize,
    #[serde(default = "default_chunk_size")]
    pub chunk_size_lines: usize,
    #[serde(default = "default_chunk_overlap")]
    pub chunk_overlap_lines: usize,
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_embed_pool_size")]
    pub pool_size: usize,
    /// Enable GPU acceleration for embeddings (requires `cuda` feature).
    #[serde(default)]
    pub use_gpu: bool,
    /// Maximum input token sequence length. Inputs that tokenize to more
    /// than this are truncated. all-MiniLM-L6-v2 was trained with
    /// `max_position_embeddings = 512`; matching that gives full fidelity.
    /// Lowering trades long-input accuracy for transient memory.
    #[serde(default = "default_max_length")]
    pub max_length: usize,
    /// Cap on input texts per single forward pass inside `Embedder::embed`.
    /// BERT self-attention is `O(batch * seq²)`, so unbounded batches OOM
    /// the GPU on files with many chunks. Default 8 keeps peak VRAM well
    /// under 1 GiB per worker at `max_length = 512`.
    #[serde(default = "default_inference_batch_size")]
    pub inference_batch_size: usize,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            dimensions: default_dimensions(),
            chunk_size_lines: default_chunk_size(),
            chunk_overlap_lines: default_chunk_overlap(),
            batch_size: default_batch_size(),
            pool_size: default_embed_pool_size(),
            use_gpu: false,
            max_length: default_max_length(),
            inference_batch_size: default_inference_batch_size(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorConfig {
    /// HNSW index `m` parameter: max number of bi-directional links per node.
    /// Higher values improve recall at the cost of memory and index build time.
    #[serde(default = "default_hnsw_m")]
    pub hnsw_m: i32,
    /// HNSW index `ef_construction` parameter: size of the dynamic candidate list
    /// during index construction. Higher values improve recall at the cost of build time.
    #[serde(default = "default_hnsw_ef_construction")]
    pub hnsw_ef_construction: i32,
    /// `ef_search` parameter set at query time: size of the dynamic candidate list
    /// during search. Higher values improve recall at the cost of query latency.
    #[serde(default = "default_ef_search")]
    pub ef_search: i32,
}

impl Default for VectorConfig {
    fn default() -> Self {
        Self {
            hnsw_m: default_hnsw_m(),
            hnsw_ef_construction: default_hnsw_ef_construction(),
            ef_search: default_ef_search(),
        }
    }
}

fn default_hnsw_m() -> i32 {
    24
}
fn default_hnsw_ef_construction() -> i32 {
    200
}
fn default_ef_search() -> i32 {
    100
}

fn default_model() -> String {
    "all-MiniLM-L6-v2".into()
}
fn default_dimensions() -> usize {
    384
}
fn default_chunk_size() -> usize {
    50
}
fn default_chunk_overlap() -> usize {
    10
}
fn default_batch_size() -> usize {
    32
}
fn default_embed_pool_size() -> usize {
    2
}
fn default_max_length() -> usize {
    512
}
fn default_inference_batch_size() -> usize {
    8
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default = "default_transport")]
    pub transport: String,
    /// Bind address for the Streamable HTTP transport (daemon mode).
    #[serde(default = "default_mcp_host")]
    pub host: String,
    /// Port for the Streamable HTTP transport (daemon mode).
    #[serde(default = "default_mcp_port")]
    pub port: u16,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            transport: default_transport(),
            host: default_mcp_host(),
            port: default_mcp_port(),
        }
    }
}

fn default_transport() -> String {
    "stdio".into()
}
fn default_mcp_host() -> String {
    "127.0.0.1".into()
}
fn default_mcp_port() -> u16 {
    3100
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsConfig {
    #[serde(default = "default_http_enabled")]
    pub http_enabled: bool,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_http_bind")]
    pub http_bind: String,
    /// Master switch for the `mcp_tool_calls` durable telemetry pipeline.
    /// When false, no rows are written to the DB; in-memory counters
    /// (`tool_invocations`, `tool_telemetry_by_client`) and Prometheus
    /// exposition continue to work, but historical aggregation queries
    /// via the `mcp_tool_telemetry` MCP tool will see only the
    /// in-memory window.
    #[serde(default = "default_telemetry_db_write_enabled")]
    pub telemetry_db_write_enabled: bool,
    /// Retain `mcp_tool_calls` rows for this many days; older rows are
    /// purged by the daily `telemetry-retention` cron job.
    #[serde(default = "default_telemetry_retention_days")]
    pub telemetry_retention_days: u32,
    /// Fraction of tool calls (0.0 – 1.0) that get a DB row written.
    /// In-memory counters always update regardless; sampling only reduces
    /// durable-storage volume.
    #[serde(default = "default_telemetry_sample_rate")]
    pub telemetry_sample_rate: f64,
    /// Telemetry-writer batches up to this many rows before issuing
    /// the bulk INSERT.
    #[serde(default = "default_telemetry_batch_size")]
    pub telemetry_batch_size: usize,
    /// Telemetry-writer flushes a partial batch after this many
    /// milliseconds even if `telemetry_batch_size` hasn't been reached.
    #[serde(default = "default_telemetry_batch_interval_ms")]
    pub telemetry_batch_interval_ms: u64,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            http_enabled: default_http_enabled(),
            http_port: default_http_port(),
            http_bind: default_http_bind(),
            telemetry_db_write_enabled: default_telemetry_db_write_enabled(),
            telemetry_retention_days: default_telemetry_retention_days(),
            telemetry_sample_rate: default_telemetry_sample_rate(),
            telemetry_batch_size: default_telemetry_batch_size(),
            telemetry_batch_interval_ms: default_telemetry_batch_interval_ms(),
        }
    }
}

fn default_http_enabled() -> bool {
    true
}
fn default_http_port() -> u16 {
    9464
}
fn default_http_bind() -> String {
    "127.0.0.1".into()
}
fn default_telemetry_db_write_enabled() -> bool {
    true
}
fn default_telemetry_retention_days() -> u32 {
    30
}
fn default_telemetry_sample_rate() -> f64 {
    1.0
}
fn default_telemetry_batch_size() -> usize {
    256
}
fn default_telemetry_batch_interval_ms() -> u64 {
    500
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_file")]
    pub file: String,
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_rotation")]
    pub rotation: String,
    #[serde(default = "default_max_log_files")]
    pub max_log_files: u32,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            file: default_log_file(),
            level: default_log_level(),
            rotation: default_rotation(),
            max_log_files: default_max_log_files(),
        }
    }
}

fn default_log_file() -> String {
    "~/.local/share/pgmcp/pgmcp.log".into()
}
fn default_log_level() -> String {
    "info".into()
}
fn default_rotation() -> String {
    "daily".into()
}
fn default_max_log_files() -> u32 {
    7
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkPoolConfig {
    #[serde(default = "default_min_threads")]
    pub min_threads: usize,
    #[serde(default)]
    pub max_threads: usize,
    #[serde(default)]
    pub initial_threads: usize,
}

impl Default for WorkPoolConfig {
    fn default() -> Self {
        Self {
            min_threads: default_min_threads(),
            max_threads: 0,
            initial_threads: 0,
        }
    }
}

impl WorkPoolConfig {
    /// Resolve 0 values to actual thread counts.
    pub fn resolved_max_threads(&self) -> usize {
        if self.max_threads == 0 {
            num_cpus::get()
        } else {
            self.max_threads
        }
    }

    pub fn resolved_initial_threads(&self) -> usize {
        if self.initial_threads == 0 {
            self.min_threads
        } else {
            self.initial_threads
        }
    }
}

fn default_min_threads() -> usize {
    2
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronConfig {
    #[serde(default = "default_stale_cleanup")]
    pub stale_cleanup_interval_secs: u64,
    #[serde(default = "default_integrity_check")]
    pub integrity_check_interval_secs: u64,
    #[serde(default = "default_stats_aggregation")]
    pub stats_aggregation_interval_secs: u64,
    #[serde(default = "default_db_maintenance")]
    pub db_maintenance_interval_secs: u64,
    #[serde(default = "default_git_history_index")]
    pub git_history_index_interval_secs: u64,
    #[serde(default = "default_similarity_scan_interval")]
    pub similarity_scan_interval_secs: u64,
    #[serde(default = "default_similarity_threshold")]
    pub similarity_threshold: f64,
    #[serde(default = "default_similarity_top_k")]
    pub similarity_top_k: i32,
    /// Interval between global topic scans (default: 43200 = 12 hours)
    #[serde(default = "default_topic_scan_interval")]
    pub topic_scan_interval_secs: u64,
    /// FCM min_cluster_size used for K estimation heuristic (default: 5)
    #[serde(default = "default_topic_min_cluster_size")]
    pub topic_min_cluster_size: usize,
    /// Explicit number of clusters (None = auto-estimate via sqrt(n / min_cluster_size))
    #[serde(default)]
    pub topic_num_clusters: Option<usize>,
    /// FCM fuzziness exponent m (default: 2.0; m > 1 controls overlap degree)
    #[serde(default = "default_topic_fuzziness")]
    pub topic_fuzziness: f64,
    /// Maximum FCM iterations (default: 100)
    #[serde(default = "default_topic_fcm_max_iters")]
    pub topic_fcm_max_iters: usize,
    /// FCM convergence tolerance on membership matrix (default: 1e-5)
    #[serde(default = "default_topic_fcm_tolerance")]
    pub topic_fcm_tolerance: f64,
    /// Minimum membership degree to store in DB (default: 0.05)
    #[serde(default = "default_topic_membership_threshold")]
    pub topic_membership_threshold: f64,
    /// Number of top keywords per topic from c-TF-IDF (default: 5)
    #[serde(default = "default_topic_label_top_k")]
    pub topic_label_top_k: usize,
    /// Interval between graph analysis runs in seconds (default: 7200 = 2 hours)
    #[serde(default = "default_graph_analysis_interval")]
    pub graph_analysis_interval_secs: u64,

    /// Interval between symbol-extraction (Tier-0e) runs in seconds (default: 7200 = 2 hours).
    /// The cron runs the per-language `LanguageBackend` impls across the indexed corpus and
    /// persists into `file_symbols` + `symbol_references`. Steady-state cost is bounded by the
    /// per-project `symbol_extraction_last_run:<id>` watermark — only files modified since the
    /// last run are re-extracted.
    #[serde(default = "default_symbol_extraction_interval")]
    pub symbol_extraction_interval_secs: u64,

    // -----------------------------------------------------------------------
    // OOM-fix additions (Phase 1)
    // -----------------------------------------------------------------------
    /// Maximum fraction of /proc/meminfo:MemAvailable that global topic clustering
    /// is allowed to predict using. If prediction exceeds this, fall back to the
    /// per-project emergency path. Default: 0.4 (use at most 40% of available memory).
    #[serde(default = "default_topic_max_mem_fraction")]
    pub topic_max_mem_fraction: f64,

    /// Scratch directory for the mmap-backed data matrix. Default: $XDG_CACHE_HOME/pgmcp
    /// (falls back to /tmp/pgmcp if XDG is unset). Files named `fcm-scratch-<pid>-<ts>.dat`
    /// are created and unlinked automatically.
    #[serde(default)]
    pub topic_scratch_dir: Option<std::path::PathBuf>,

    /// Ready-relative initial delay for git-history-index cron (seconds).
    /// Default 300 = wait 5 minutes after the daemon reaches Ready.
    #[serde(default = "default_ready_delay_git_secs")]
    pub ready_delay_git_secs: u64,

    /// Ready-relative initial delay for similarity-scan cron (seconds).
    /// Default 900 = 15 minutes.
    #[serde(default = "default_ready_delay_similarity_secs")]
    pub ready_delay_similarity_secs: u64,

    /// Ready-relative initial delay for graph-analysis cron (seconds).
    /// Default 1800 = 30 minutes.
    #[serde(default = "default_ready_delay_graph_secs")]
    pub ready_delay_graph_secs: u64,

    /// Ready-relative initial delay for topic-clustering cron (seconds).
    /// Default 3600 = 60 minutes.
    #[serde(default = "default_ready_delay_topic_secs")]
    pub ready_delay_topic_secs: u64,

    /// Ready-relative initial delay for symbol-extraction cron (seconds).
    /// Default 1800 = 30 minutes (matches `ready_delay_graph_secs`).
    #[serde(default = "default_ready_delay_symbol_extraction_secs")]
    pub ready_delay_symbol_extraction_secs: u64,

    /// GPU FCM precision selector (cuda feature only). Valid values: "fp32",
    /// "fp16", "bf16". Default: "fp16" — mixed precision with fp32 accumulator,
    /// Tensor Cores enabled on Ada Lovelace / Hopper GPUs. Falls back to fp32
    /// cuBLAS SGEMM if the GPU doesn't support the requested precision.
    #[serde(default = "default_gpu_fcm_precision")]
    pub gpu_fcm_precision: String,

    /// Adaptive K selector index (Phase 12). Valid values: "xie_beni"
    /// (default, cheapest), "silhouette" (fuzzy silhouette), "gap"
    /// (Gap statistic, most expensive).
    #[serde(default = "default_topic_k_selector")]
    pub topic_k_selector: String,

    /// Candidate K values for the sweep. Empty = use geometric sweep
    /// around `estimate_k` (K_base · 2^{-2..+2}, clamped [10, 500]).
    #[serde(default)]
    pub topic_k_candidates: Vec<usize>,

    /// Max iterations per short-FCM during the K sweep (default 20).
    #[serde(default = "default_topic_k_sweep_max_iters")]
    pub topic_k_sweep_max_iters: usize,

    /// Subsample size for the K sweep — pass only this many rows of `data`
    /// to the short FCM runs (default 50 000). 0 disables subsampling.
    #[serde(default = "default_topic_k_sweep_subsample")]
    pub topic_k_sweep_subsample: usize,

    /// LMDB path for persistent topic state (Phase 7). None = XDG default
    /// (`$XDG_DATA_HOME/pgmcp/topics.lmdb`).
    #[serde(default)]
    pub topic_lmdb_path: Option<std::path::PathBuf>,

    /// Enable LMDB-backed warm-start. Default true. Set false to always
    /// cold-start via k-means++.
    #[serde(default = "default_topic_lmdb_enabled")]
    pub topic_lmdb_enabled: bool,

    /// n threshold above which `run_global_topic_scan` dispatches to the
    /// online mini-batch FCM (Phase 8). Default 1_000_000.
    #[serde(default = "default_topic_online_n_threshold")]
    pub topic_online_n_threshold: usize,

    /// Mini-batch size for the online FCM (Phase 8). Default 10_000.
    #[serde(default = "default_topic_online_batch_size")]
    pub topic_online_batch_size: usize,

    /// n threshold above which `run_global_topic_scan` uses the mmap-backed
    /// data matrix + streaming c-TF-IDF (Phase 1.2-1.3) instead of loading all
    /// ChunkEmbeddingRow records in one `fetch_all`. Default 50_000. Must be
    /// <= topic_online_n_threshold; above that threshold the online FCM
    /// (Phase 8) takes over.
    #[serde(default = "default_topic_mmap_n_threshold")]
    pub topic_mmap_n_threshold: usize,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            stale_cleanup_interval_secs: default_stale_cleanup(),
            integrity_check_interval_secs: default_integrity_check(),
            stats_aggregation_interval_secs: default_stats_aggregation(),
            db_maintenance_interval_secs: default_db_maintenance(),
            git_history_index_interval_secs: default_git_history_index(),
            similarity_scan_interval_secs: default_similarity_scan_interval(),
            similarity_threshold: default_similarity_threshold(),
            similarity_top_k: default_similarity_top_k(),
            topic_scan_interval_secs: default_topic_scan_interval(),
            topic_min_cluster_size: default_topic_min_cluster_size(),
            topic_num_clusters: None,
            topic_fuzziness: default_topic_fuzziness(),
            topic_fcm_max_iters: default_topic_fcm_max_iters(),
            topic_fcm_tolerance: default_topic_fcm_tolerance(),
            topic_membership_threshold: default_topic_membership_threshold(),
            topic_label_top_k: default_topic_label_top_k(),
            graph_analysis_interval_secs: default_graph_analysis_interval(),
            symbol_extraction_interval_secs: default_symbol_extraction_interval(),
            topic_max_mem_fraction: default_topic_max_mem_fraction(),
            topic_scratch_dir: None,
            ready_delay_git_secs: default_ready_delay_git_secs(),
            ready_delay_similarity_secs: default_ready_delay_similarity_secs(),
            ready_delay_graph_secs: default_ready_delay_graph_secs(),
            ready_delay_topic_secs: default_ready_delay_topic_secs(),
            ready_delay_symbol_extraction_secs: default_ready_delay_symbol_extraction_secs(),
            gpu_fcm_precision: default_gpu_fcm_precision(),
            topic_k_selector: default_topic_k_selector(),
            topic_k_candidates: Vec::new(),
            topic_k_sweep_max_iters: default_topic_k_sweep_max_iters(),
            topic_k_sweep_subsample: default_topic_k_sweep_subsample(),
            topic_lmdb_path: None,
            topic_lmdb_enabled: default_topic_lmdb_enabled(),
            topic_online_n_threshold: default_topic_online_n_threshold(),
            topic_online_batch_size: default_topic_online_batch_size(),
            topic_mmap_n_threshold: default_topic_mmap_n_threshold(),
        }
    }
}

fn default_topic_max_mem_fraction() -> f64 {
    0.4
}
fn default_ready_delay_git_secs() -> u64 {
    300
}
fn default_ready_delay_similarity_secs() -> u64 {
    900
}
fn default_ready_delay_graph_secs() -> u64 {
    1800
}
fn default_ready_delay_topic_secs() -> u64 {
    3600
}
fn default_ready_delay_symbol_extraction_secs() -> u64 {
    1800
}
fn default_gpu_fcm_precision() -> String {
    "fp16".into()
}
fn default_topic_k_selector() -> String {
    "xie_beni".into()
}
fn default_topic_k_sweep_max_iters() -> usize {
    20
}
fn default_topic_k_sweep_subsample() -> usize {
    50_000
}
fn default_topic_lmdb_enabled() -> bool {
    true
}
fn default_topic_online_n_threshold() -> usize {
    1_000_000
}
fn default_topic_online_batch_size() -> usize {
    10_000
}
fn default_topic_mmap_n_threshold() -> usize {
    50_000
}

fn default_stale_cleanup() -> u64 {
    3600
}
fn default_integrity_check() -> u64 {
    86400
}
fn default_stats_aggregation() -> u64 {
    60
}
fn default_db_maintenance() -> u64 {
    604_800
}
fn default_git_history_index() -> u64 {
    3600
}
fn default_similarity_scan_interval() -> u64 {
    21600
} // 6 hours
fn default_similarity_threshold() -> f64 {
    0.85
}
fn default_similarity_top_k() -> i32 {
    10
}
fn default_topic_scan_interval() -> u64 {
    43200
} // 12 hours
fn default_topic_min_cluster_size() -> usize {
    5
}
fn default_topic_fuzziness() -> f64 {
    2.0
}
fn default_topic_fcm_max_iters() -> usize {
    100
}
fn default_topic_fcm_tolerance() -> f64 {
    1e-5
}
fn default_topic_membership_threshold() -> f64 {
    0.05
}
fn default_topic_label_top_k() -> usize {
    5
}
fn default_graph_analysis_interval() -> u64 {
    7200
} // 2 hours
fn default_symbol_extraction_interval() -> u64 {
    7200
} // 2 hours — matches graph-analysis cadence

impl Config {
    /// Load configuration from the default path or the specified path.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => Self::default_config_path(),
        };

        if !config_path.exists() {
            return Ok(Config::default());
        }

        let content = std::fs::read_to_string(&config_path)
            .map_err(|e| PgmcpError::file_io(&config_path, e))?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Resolve the config file path from an optional user-provided path or the default.
    pub fn resolve_path(custom: Option<&Path>) -> PathBuf {
        match custom {
            Some(p) => p.to_path_buf(),
            None => Self::default_config_path(),
        }
    }

    /// Default config file path: ~/.config/pgmcp/config.toml
    pub fn default_config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("pgmcp")
            .join("config.toml")
    }

    /// Generate default config content as TOML string.
    pub fn default_toml() -> String {
        let config = Config::default();
        toml::to_string_pretty(&config).expect("Failed to serialize default config")
    }

    /// Write the default config to the default path.
    pub fn write_default() -> Result<PathBuf> {
        let path = Self::default_config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| PgmcpError::file_io(parent, e))?;
        }
        std::fs::write(&path, Self::default_toml()).map_err(|e| PgmcpError::file_io(&path, e))?;
        Ok(path)
    }

    /// Return the `~/.claude/` directory if it exists.
    pub fn claude_dir() -> Option<PathBuf> {
        dirs::home_dir()
            .map(|h| h.join(".claude"))
            .filter(|p| p.is_dir())
    }

    /// Return the `~/.codex/` directory if it exists.
    pub fn codex_dir() -> Option<PathBuf> {
        dirs::home_dir()
            .map(|h| h.join(".codex"))
            .filter(|p| p.is_dir())
    }

    /// Return the `~/Papers/` directory if it exists. When present, the
    /// scanner auto-discovers it as a synthetic project named `Papers`
    /// (mirroring the `~/.claude/` and `~/.codex/` precedent — no `.git/`
    /// required). Returns `None` if the directory is absent so users
    /// without an academic-papers folder pay no cost.
    pub fn papers_dir() -> Option<PathBuf> {
        dirs::home_dir()
            .map(|h| h.join("Papers"))
            .filter(|p| p.is_dir())
    }

    /// Return the `~/Documents/` directory if it exists. Auto-discovered
    /// as a synthetic project named `Documents`. See `papers_dir` for the
    /// design rationale; same `is_dir()` guard pattern.
    pub fn documents_dir() -> Option<PathBuf> {
        dirs::home_dir()
            .map(|h| h.join("Documents"))
            .filter(|p| p.is_dir())
    }

    /// Upgrade an existing config file by merging new defaults while preserving
    /// user customizations. Returns the path that was written.
    pub fn upgrade(path: Option<&Path>) -> Result<PathBuf> {
        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => Self::default_config_path(),
        };

        let defaults_toml: TomlValue =
            toml::from_str(&Self::default_toml()).expect("Default config must be valid TOML");

        if config_path.exists() {
            let user_content = std::fs::read_to_string(&config_path)
                .map_err(|e| PgmcpError::file_io(&config_path, e))?;
            let user_toml: TomlValue = toml::from_str(&user_content)?;
            let merged = merge_toml_values(defaults_toml, user_toml);
            let output = toml::to_string_pretty(&merged).expect("Merged TOML must serialize");
            std::fs::write(&config_path, output)
                .map_err(|e| PgmcpError::file_io(&config_path, e))?;
        } else {
            // No existing config — just write defaults
            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| PgmcpError::file_io(parent, e))?;
            }
            std::fs::write(&config_path, Self::default_toml())
                .map_err(|e| PgmcpError::file_io(&config_path, e))?;
        }

        Ok(config_path)
    }
}

/// Recursively merge two TOML values. `user` values take precedence over `defaults`.
/// - Tables: recursively merged; new default keys are added; user keys preserved.
/// - Arrays: user entries kept, new default entries (not already present) appended.
/// - Scalars: user value wins.
pub fn merge_toml_values(defaults: TomlValue, user: TomlValue) -> TomlValue {
    match (defaults, user) {
        (TomlValue::Table(mut def_table), TomlValue::Table(user_table)) => {
            for (key, user_val) in user_table {
                let merged = if let Some(def_val) = def_table.remove(&key) {
                    merge_toml_values(def_val, user_val)
                } else {
                    user_val
                };
                def_table.insert(key, merged);
            }
            TomlValue::Table(def_table)
        }
        (TomlValue::Array(def_arr), TomlValue::Array(user_arr)) => {
            let mut merged = user_arr;
            for def_item in def_arr {
                if !merged.contains(&def_item) {
                    merged.push(def_item);
                }
            }
            TomlValue::Array(merged)
        }
        // User scalar wins over default scalar
        (_defaults, user) => user,
    }
}

/// Per-project override config (.pgmcp.toml in project root).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProjectOverride {
    #[serde(default)]
    pub indexer: Option<ProjectIndexerOverride>,
    #[serde(default)]
    pub git: Option<GitConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectIndexerOverride {
    pub exclude_patterns: Option<Vec<String>>,
    pub file_types: Option<Vec<FileTypeMapping>>,
    pub max_file_size_bytes: Option<u64>,
    /// Per-project source-form priority (replaces the global list rather
    /// than merging — for an ordered list, replace semantics are clearer
    /// than OR).
    pub source_priority: Option<Vec<String>>,
    /// Per-project cap on binary document source bytes; overrides the
    /// global `[indexer] max_document_source_bytes`.
    pub max_document_source_bytes: Option<u64>,
    /// Per-project cap on extracted text size.
    pub max_extracted_text_bytes: Option<usize>,
    /// Per-project extraction subprocess timeout in seconds.
    pub document_extraction_timeout_secs: Option<u64>,
}

/// Git history indexing configuration for a project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GitConfig {
    /// Enable git history indexing (commit messages + diffs) for this project.
    #[serde(default)]
    pub index_history: bool,
}

impl ProjectOverride {
    pub fn load(project_root: &Path) -> Option<Self> {
        let path = project_root.join(".pgmcp.toml");
        if !path.exists() {
            return None;
        }
        let content = std::fs::read_to_string(&path).ok()?;
        toml::from_str(&content).ok()
    }

    /// Default per-project config TOML content.
    pub fn default_toml() -> String {
        let default = ProjectOverride {
            indexer: None,
            git: Some(GitConfig::default()),
        };
        toml::to_string_pretty(&default).expect("Failed to serialize default project override")
    }

    /// Write the default .pgmcp.toml to a project root.
    pub fn write_default(project_root: &Path) -> Result<PathBuf> {
        let path = project_root.join(".pgmcp.toml");
        std::fs::write(&path, Self::default_toml()).map_err(|e| PgmcpError::file_io(&path, e))?;
        Ok(path)
    }

    /// Upgrade an existing .pgmcp.toml by merging new defaults while preserving
    /// user customizations.
    pub fn upgrade(project_root: &Path) -> Result<PathBuf> {
        let path = project_root.join(".pgmcp.toml");

        let defaults_toml: TomlValue = toml::from_str(&Self::default_toml())
            .expect("Default project override must be valid TOML");

        if path.exists() {
            let user_content =
                std::fs::read_to_string(&path).map_err(|e| PgmcpError::file_io(&path, e))?;
            let user_toml: TomlValue = toml::from_str(&user_content)?;
            let merged = merge_toml_values(defaults_toml, user_toml);
            let output = toml::to_string_pretty(&merged).expect("Merged TOML must serialize");
            std::fs::write(&path, output).map_err(|e| PgmcpError::file_io(&path, e))?;
        } else {
            std::fs::write(&path, Self::default_toml())
                .map_err(|e| PgmcpError::file_io(&path, e))?;
        }

        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_parses() {
        let toml_str = Config::default_toml();
        let _config: Config = toml::from_str(&toml_str).expect("Default config should parse");
    }

    #[test]
    fn test_extension_map() {
        let config = IndexerConfig::default();
        let map = config.extension_map();
        assert_eq!(map.get("rs"), Some(&"rust".to_string()));
        assert_eq!(map.get("py"), Some(&"python".to_string()));
    }

    #[test]
    fn test_is_configured_extension() {
        let config = IndexerConfig::default();
        assert!(config.is_configured_extension(Path::new("foo.rs")));
        assert!(config.is_configured_extension(Path::new("bar.py")));
        assert!(!config.is_configured_extension(Path::new("baz.exe")));
    }

    #[test]
    fn test_language_for_path() {
        let config = IndexerConfig::default();
        assert_eq!(
            config.language_for_path(Path::new("foo.rs")),
            Some("rust".into())
        );
        assert_eq!(config.language_for_path(Path::new("foo.xyz")), None);
    }

    #[test]
    fn test_database_url() {
        let db = DatabaseConfig::default();
        let url = db.connection_url();
        assert!(url.starts_with("postgres://pgmcp@localhost:5432/pgmcp"));
    }

    #[test]
    fn test_work_pool_config_defaults() {
        let wpc = WorkPoolConfig::default();
        assert_eq!(wpc.min_threads, 2);
        assert!(wpc.resolved_max_threads() >= 1);
        assert_eq!(wpc.resolved_initial_threads(), 2);
    }

    #[test]
    fn test_new_file_types() {
        let config = IndexerConfig::default();
        assert!(config.is_configured_extension(Path::new("script.sh")));
        assert!(config.is_configured_extension(Path::new("data.jsonl")));
        assert_eq!(
            config.language_for_path(Path::new("script.sh")),
            Some("shell".into())
        );
        assert_eq!(
            config.language_for_path(Path::new("data.jsonl")),
            Some("jsonl".into())
        );
    }

    /// Regression test for the Tier-0e extensions added 2026-05-01. Every
    /// extension here must round-trip to a language whose `LanguageRegistry`
    /// returns `Some` — otherwise the symbol-extraction cron would skip files
    /// of that type.
    #[test]
    fn test_default_file_types_includes_tier_0e_languages() {
        let config = IndexerConfig::default();
        for (ext, expected_lang) in [
            ("java", "java"),
            ("scala", "scala"),
            ("c", "c"),
            ("h", "c"),
            ("cpp", "cpp"),
            ("cc", "cpp"),
            ("cxx", "cpp"),
            ("hpp", "cpp"),
            ("hxx", "cpp"),
            ("clj", "clojure"),
            ("cljs", "clojurescript"),
            ("tsx", "tsx"),
        ] {
            let path_str = format!("file.{}", ext);
            let path = Path::new(&path_str);
            assert!(
                config.is_configured_extension(path),
                "missing default mapping for .{}",
                ext
            );
            assert_eq!(
                config.language_for_path(path),
                Some(expected_lang.to_string()),
                "wrong language for .{}",
                ext
            );
            // Cross-check: the language must be one that `LanguageRegistry`
            // routes to a backend.
            assert!(
                crate::parsing::LanguageRegistry::for_language(expected_lang).is_some(),
                "no backend registered for language `{}` (mapped from .{})",
                expected_lang,
                ext
            );
        }
    }

    #[test]
    fn test_merge_toml_scalars_user_wins() {
        let defaults: TomlValue = toml::from_str(r#"key = "default""#).expect("parse");
        let user: TomlValue = toml::from_str(r#"key = "custom""#).expect("parse");
        let merged = merge_toml_values(defaults, user);
        assert_eq!(merged["key"].as_str(), Some("custom"));
    }

    #[test]
    fn test_merge_toml_tables_add_new_keys() {
        let defaults: TomlValue = toml::from_str(
            r#"
            [section]
            existing = "default"
            new_key = "added"
        "#,
        )
        .expect("parse");
        let user: TomlValue = toml::from_str(
            r#"
            [section]
            existing = "custom"
        "#,
        )
        .expect("parse");
        let merged = merge_toml_values(defaults, user);
        assert_eq!(merged["section"]["existing"].as_str(), Some("custom"));
        assert_eq!(merged["section"]["new_key"].as_str(), Some("added"));
    }

    #[test]
    fn test_merge_toml_arrays_union() {
        let defaults: TomlValue = toml::from_str(
            r#"
            items = ["a", "b", "c"]
        "#,
        )
        .expect("parse");
        let user: TomlValue = toml::from_str(
            r#"
            items = ["b", "d"]
        "#,
        )
        .expect("parse");
        let merged = merge_toml_values(defaults, user);
        let arr = merged["items"].as_array().expect("should be array");
        assert!(arr.contains(&TomlValue::String("b".into())));
        assert!(arr.contains(&TomlValue::String("d".into())));
        assert!(arr.contains(&TomlValue::String("a".into())));
        assert!(arr.contains(&TomlValue::String("c".into())));
    }

    #[test]
    fn test_merge_toml_preserves_user_only_keys() {
        let defaults: TomlValue = toml::from_str(r#"a = 1"#).expect("parse");
        let user: TomlValue = toml::from_str(
            r#"
            a = 2
            user_only = 42
        "#,
        )
        .expect("parse");
        let merged = merge_toml_values(defaults, user);
        assert_eq!(merged["a"].as_integer(), Some(2));
        assert_eq!(merged["user_only"].as_integer(), Some(42));
    }

    /// Regression: every document extension added in Phase 5 must be
    /// configured and map to its expected language. The language strings
    /// MUST NOT collide with any tree-sitter backend name in
    /// `LanguageRegistry`, since that's how the symbol-extraction cron
    /// decides to skip these files (return `None` from `for_language`).
    #[test]
    fn test_default_file_types_includes_document_languages() {
        let config = IndexerConfig::default();
        for (ext, expected_lang) in [
            ("pdf", "pdf"),
            ("ps", "postscript"),
            ("eps", "postscript"),
            ("tex", "latex"),
            ("latex", "latex"),
            ("bib", "bibtex"),
            ("org", "org"),
            ("rst", "rst"),
            ("docx", "docx"),
            ("doc", "doc"),
            ("rtf", "rtf"),
            ("odt", "odt"),
            ("epub", "epub"),
            ("txt", "text"),
        ] {
            let path_str = format!("file.{}", ext);
            let path = Path::new(&path_str);
            assert!(
                config.is_configured_extension(path),
                "missing document mapping for .{}",
                ext
            );
            assert_eq!(
                config.language_for_path(path),
                Some(expected_lang.to_string()),
                "wrong language for .{}",
                ext
            );
            // None of these languages should resolve to a tree-sitter backend.
            assert!(
                crate::parsing::LanguageRegistry::for_language(expected_lang).is_none(),
                "document language `{}` (.{}) collides with tree-sitter backend",
                expected_lang,
                ext
            );
        }
    }

    #[test]
    fn test_indexer_config_document_defaults() {
        let cfg = IndexerConfig::default();
        assert_eq!(cfg.max_document_source_bytes, 100 * 1024 * 1024);
        assert_eq!(cfg.max_extracted_text_bytes, 50 * 1024 * 1024);
        assert_eq!(cfg.document_extraction_timeout_secs, 30);
        // Priority list contains source forms first, output forms last.
        let prio = &cfg.source_priority;
        let pos_org = prio.iter().position(|e| e == "org").expect("org present");
        let pos_tex = prio.iter().position(|e| e == "tex").expect("tex present");
        let pos_pdf = prio.iter().position(|e| e == "pdf").expect("pdf present");
        assert!(
            pos_org < pos_tex && pos_tex < pos_pdf,
            "expected org < tex < pdf in source priority"
        );
    }

    #[test]
    fn test_project_override_with_document_fields() {
        let toml_str = r#"
            [indexer]
            source_priority = ["org", "pdf"]
            max_document_source_bytes = 209715200
            max_extracted_text_bytes = 104857600
            document_extraction_timeout_secs = 60
        "#;
        let parsed: ProjectOverride = toml::from_str(toml_str).expect("parse");
        let idx = parsed.indexer.expect("indexer section present");
        assert_eq!(
            idx.source_priority.as_deref(),
            Some(&["org".to_string(), "pdf".to_string()][..])
        );
        assert_eq!(idx.max_document_source_bytes, Some(209715200));
        assert_eq!(idx.max_extracted_text_bytes, Some(104857600));
        assert_eq!(idx.document_extraction_timeout_secs, Some(60));
    }

    #[test]
    fn test_synthetic_dir_helpers_optional() {
        // These helpers return Option<PathBuf>; the contract is "Some when
        // the directory exists, None otherwise" — we only assert the type
        // contract here since the directories' existence depends on the
        // host filesystem.
        let _: Option<PathBuf> = Config::papers_dir();
        let _: Option<PathBuf> = Config::documents_dir();
    }

    #[test]
    fn test_project_override_default_toml_parses() {
        let toml_str = ProjectOverride::default_toml();
        let _parsed: ProjectOverride =
            toml::from_str(&toml_str).expect("Default project override TOML should parse");
    }

    #[test]
    fn test_project_override_with_git_config() {
        let toml_str = r#"
            [git]
            index_history = true
        "#;
        let parsed: ProjectOverride = toml::from_str(toml_str).expect("parse");
        assert!(
            parsed
                .git
                .expect("git section should be present")
                .index_history
        );
    }

    #[test]
    fn test_git_history_cron_default() {
        let config = CronConfig::default();
        assert_eq!(config.git_history_index_interval_secs, 3600);
    }

    #[test]
    fn test_similarity_cron_defaults() {
        let config = CronConfig::default();
        assert_eq!(config.similarity_scan_interval_secs, 21600);
        assert!((config.similarity_threshold - 0.85).abs() < f64::EPSILON);
        assert_eq!(config.similarity_top_k, 10);
    }

    #[test]
    fn test_topic_clustering_cron_defaults() {
        let config = CronConfig::default();
        assert_eq!(config.topic_scan_interval_secs, 43200);
        assert_eq!(config.topic_min_cluster_size, 5);
        assert!(config.topic_num_clusters.is_none());
        assert!((config.topic_fuzziness - 2.0).abs() < f64::EPSILON);
        assert_eq!(config.topic_fcm_max_iters, 100);
        assert!((config.topic_fcm_tolerance - 1e-5).abs() < 1e-12);
        assert!((config.topic_membership_threshold - 0.05).abs() < f64::EPSILON);
        assert_eq!(config.topic_label_top_k, 5);
    }

    #[test]
    fn test_symbol_extraction_cron_defaults() {
        let config = CronConfig::default();
        assert_eq!(config.symbol_extraction_interval_secs, 7200);
        assert_eq!(config.ready_delay_symbol_extraction_secs, 1800);
    }

    // ========================================================================
    // Property tests for merge_toml_values
    // ========================================================================

    use proptest::prelude::*;

    proptest! {
        /// Scalar merge: user value always wins.
        #[test]
        fn prop_merge_user_scalar_wins(def in -100i64..100, user in -100i64..100) {
            let d = TomlValue::Integer(def);
            let u = TomlValue::Integer(user);
            let merged = merge_toml_values(d, u);
            prop_assert_eq!(merged.as_integer(), Some(user));
        }

        /// Array merge: result starts with user verbatim (including any
        /// duplicates the user wrote), then appends default items not
        /// already in user. User items always appear first.
        #[test]
        fn prop_merge_array_appends_missing_defaults(
            def in prop::collection::vec(0i64..20, 0..10),
            user in prop::collection::vec(0i64..20, 0..10),
        ) {
            let d = TomlValue::Array(def.iter().map(|&x| TomlValue::Integer(x)).collect());
            let u = TomlValue::Array(user.iter().map(|&x| TomlValue::Integer(x)).collect());
            let merged = merge_toml_values(d, u);
            let arr = merged.as_array().expect("array");
            // User portion is a verbatim prefix of the merged output.
            for (i, &v) in user.iter().enumerate() {
                prop_assert_eq!(arr[i].as_integer(), Some(v),
                    "user item {} at pos {} changed", v, i);
            }
            // Every default value appears somewhere (either because it was
            // already in user or because it was appended).
            for &v in &def {
                prop_assert!(arr.iter().any(|x| x.as_integer() == Some(v)),
                    "default value {} missing from merged array", v);
            }
            // Length is bounded above by |user| + |def| — no accidental
            // multiplication.
            prop_assert!(arr.len() <= user.len() + def.len());
        }

        /// Table merge: keys only in user end up in result, keys only in
        /// default end up in result, keys in both are recursively merged.
        #[test]
        fn prop_merge_tables_preserve_both_sides(
            def_key in "[a-z]{1,6}",
            user_key in "[a-z]{1,6}",
            def_val in 0i64..100,
            user_val in 0i64..100,
        ) {
            prop_assume!(def_key != user_key);
            let mut d_table = toml::map::Map::new();
            d_table.insert(def_key.clone(), TomlValue::Integer(def_val));
            let d = TomlValue::Table(d_table);

            let mut u_table = toml::map::Map::new();
            u_table.insert(user_key.clone(), TomlValue::Integer(user_val));
            let u = TomlValue::Table(u_table);

            let merged = merge_toml_values(d, u);
            let t = merged.as_table().expect("table");
            prop_assert_eq!(t.get(&def_key).and_then(|v| v.as_integer()), Some(def_val));
            prop_assert_eq!(t.get(&user_key).and_then(|v| v.as_integer()), Some(user_val));
        }

        /// Idempotence: merge(merge(d, u), u) == merge(d, u) for scalars.
        #[test]
        fn prop_merge_idempotent_for_scalars(def in 0i64..100, user in 0i64..100) {
            let d = TomlValue::Integer(def);
            let u = TomlValue::Integer(user);
            let first = merge_toml_values(d.clone(), u.clone());
            let again = merge_toml_values(first.clone(), u);
            prop_assert_eq!(first.as_integer(), again.as_integer());
        }
    }
}
