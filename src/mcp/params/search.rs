//! Search, software-pattern, similarity & recommendation tool parameter types.
//!
//! Extracted verbatim from `server.rs` (B.2 god-file split). All structs
//! re-exported by `params/mod.rs` and, transitively, by `server.rs` so
//! `crate::mcp::server::<Name>Params` resolves for every tool body file.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SemanticSearchParams {
    #[schemars(description = "Search query text")]
    pub query: String,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Filter by project name")]
    pub project: Option<String>,
    #[schemars(
        description = "If true, collapse cross-worktree duplicates (same file appearing \
                       in multiple worktrees / sibling clones of the same upstream repo) \
                       to a single canonical hit per (repo, relative_path). Default false: \
                       all hits are returned, including the same code on different branches."
    )]
    pub dedupe_worktrees: Option<bool>,
    // Shadow-ASR filter params (Pattern D): restrict to chunks whose
    // enclosing symbol carries the given return_type_tags / effects /
    // scope_kind. Optional; omitting them preserves legacy behavior.
    #[schemars(
        description = "Restrict hits to chunks whose enclosing symbol's return_type_tags contains \
                       ALL of these tags (subset semantics). Optional."
    )]
    pub return_type_tags: Option<Vec<String>>,
    #[schemars(
        description = "Restrict hits to chunks whose enclosing symbol carries at least one of \
                       these effects. Optional."
    )]
    pub effects: Option<Vec<String>>,
    #[schemars(
        description = "Restrict hits to chunks whose enclosing symbol kind matches (e.g. \
                       \"function\", \"trait\", \"class\"). Optional."
    )]
    pub scope_kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TextSearchParams {
    #[schemars(description = "Full-text search query")]
    pub query: String,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(
        description = "If true, collapse cross-worktree duplicates (see semantic_search). \
                       Default false."
    )]
    pub dedupe_worktrees: Option<bool>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol's \
                       return_type_tags contains ALL of these tags. Optional."
    )]
    pub return_type_tags: Option<Vec<String>>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol carries \
                       at least one of these effects. Optional."
    )]
    pub effects: Option<Vec<String>>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol kind \
                       matches. Optional."
    )]
    pub scope_kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GrepParams {
    #[schemars(
        description = "Regex pattern to search for (or, when fuzzy=true, a TokenGrep query)"
    )]
    pub pattern: String,
    #[schemars(
        description = "If true, match `pattern` APPROXIMATELY (liblevenshtein TokenGrep) across \
                       indexed file_chunks instead of exact regex — finds typo'd / near-miss \
                       identifiers. Strongly recommend setting `project` to bound the scan. \
                       Default false."
    )]
    pub fuzzy: Option<bool>,
    #[schemars(description = "Max edit distance per token when fuzzy=true (default 2).")]
    pub fuzzy_max_distance: Option<u32>,
    #[schemars(description = "Glob pattern to filter files (e.g. '*.rs')")]
    pub glob: Option<String>,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "If true, collapse cross-worktree duplicates (see semantic_search). \
                       Default false."
    )]
    pub dedupe_worktrees: Option<bool>,
    #[schemars(description = "Filter matches to a specific project (by name)")]
    pub project: Option<String>,
    #[schemars(description = "Filter matches to a specific language string \
                       (e.g. \"rust\", \"pdf\", \"latex\")")]
    pub language: Option<String>,
    #[schemars(
        description = "Lines of context to show BEFORE each match (default: 0). Returns at most \
                       this many extra lines from the matching chunk to anchor the hit; \
                       cross-chunk context-line stitching is not performed."
    )]
    pub before_context: Option<i32>,
    #[schemars(
        description = "Lines of context to show AFTER each match (default: 0). See \
                       `before_context` for caveats."
    )]
    pub after_context: Option<i32>,
    #[schemars(description = "If true, ignore case (`~*` regex op). Default false.")]
    pub case_insensitive: Option<bool>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol's \
                       return_type_tags contains ALL of these tags. Optional."
    )]
    pub return_type_tags: Option<Vec<String>>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol carries \
                       at least one of these effects. Optional."
    )]
    pub effects: Option<Vec<String>>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol kind \
                       matches. Optional."
    )]
    pub scope_kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchCommitsParams {
    #[schemars(
        description = "Search query text (matched by semantic similarity against commit messages and diffs)"
    )]
    pub query: String,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter by project name")]
    pub project: Option<String>,
    #[schemars(
        description = "Shadow-ASR filter: restrict to commits that touched files containing \
                       at least one symbol carrying any of these effects (e.g. ['unsafe', \
                       'crypto'] surfaces commits that introduced unsafe-or-crypto code). \
                       Optional."
    )]
    pub touched_effects: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SoftwarePatternSearchParams {
    #[schemars(description = "Design/problem query to match against the software pattern index")]
    pub query: String,
    #[schemars(description = "Maximum number of pattern matches to return (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter to pattern or anti_pattern")]
    pub kind: Option<String>,
    #[schemars(
        description = "Programming paradigms to target, e.g. object_oriented_programming, functional_programming, logic_programming, event_driven_programming, concurrent_programming, parallel_programming, aspect_oriented_programming"
    )]
    pub paradigms: Option<Vec<String>>,
    #[schemars(
        description = "Filter by pattern category, e.g. creational, behavioral, resilience"
    )]
    pub category: Option<String>,
    #[schemars(description = "Filter by source family, e.g. wikipedia, oodesign, aws, aspectj")]
    pub source_family: Option<String>,
    #[schemars(
        description = "Filter by source type, e.g. curated_card, article, manual, repository"
    )]
    pub source_type: Option<String>,
    #[schemars(description = "Include source metadata and bounded excerpts (default: true)")]
    pub include_sources: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecommendDesignPatternsParams {
    #[schemars(description = "Feature or refactor task to design")]
    pub task: String,
    #[schemars(
        description = "Target programming paradigms. If omitted, inferred from language/project."
    )]
    pub paradigms: Option<Vec<String>>,
    #[schemars(description = "Implementation language, used for paradigm inference")]
    pub language: Option<String>,
    #[schemars(description = "Project name, used for dominant-language inference")]
    pub project: Option<String>,
    #[schemars(description = "Design constraints, risks, or preferences")]
    pub constraints: Option<Vec<String>>,
    #[schemars(description = "Maximum number of recommended patterns (default: 8)")]
    pub limit: Option<i32>,
    #[schemars(description = "Include anti-patterns to avoid (default: true)")]
    pub include_antipatterns: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReviewDesignPatternsParams {
    #[schemars(description = "Proposed design to review")]
    pub design: String,
    #[schemars(
        description = "Target programming paradigms. If omitted, inferred from language/project."
    )]
    pub paradigms: Option<Vec<String>>,
    #[schemars(description = "Implementation language, used for paradigm inference")]
    pub language: Option<String>,
    #[schemars(description = "Project name, used for dominant-language inference")]
    pub project: Option<String>,
    #[schemars(description = "Maximum number of findings/matches (default: 8)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetSoftwarePatternParams {
    #[schemars(description = "Pattern slug or numeric id")]
    pub slug_or_id: String,
    #[schemars(description = "Include source metadata (default: true)")]
    pub include_sources: Option<bool>,
    #[schemars(description = "Include bounded source excerpts (default: false)")]
    pub include_excerpts: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListSoftwarePatternsParams {
    #[schemars(description = "Filter to pattern or anti_pattern")]
    pub kind: Option<String>,
    #[schemars(description = "Filter by paradigm slug or name")]
    pub paradigm: Option<String>,
    #[schemars(description = "Filter by category")]
    pub category: Option<String>,
    #[schemars(description = "Filter by source family")]
    pub source_family: Option<String>,
    #[schemars(description = "Maximum number of rows (default: 50)")]
    pub limit: Option<i32>,
    #[schemars(description = "Offset for pagination (default: 0)")]
    pub offset: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefreshPatternCatalogParams {
    #[schemars(
        description = "Refresh mode: seed_only, source_family, or all. seed_only embeds bundled cards; source_family/all fetch opted-in source URLs."
    )]
    pub mode: Option<String>,
    #[schemars(description = "Source family to import when mode=source_family, e.g. oodesign")]
    pub source_family: Option<String>,
    #[schemars(description = "If true, report what would be imported without changing the DB")]
    pub dry_run: Option<bool>,
    #[schemars(description = "Maximum sources to import for this run")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpsertPatternSourceParams {
    #[schemars(description = "Existing pattern slug to attach this source to")]
    pub pattern_slug: String,
    #[schemars(description = "Source family label, e.g. local, team_wiki, oodesign")]
    pub source_family: String,
    #[schemars(description = "Source type, e.g. article, manual, snippet, repository")]
    pub source_type: String,
    #[schemars(description = "Source title")]
    pub title: String,
    #[schemars(description = "Optional source URL")]
    pub url: Option<String>,
    #[schemars(description = "Optional license/provenance label")]
    pub license_label: Option<String>,
    #[schemars(description = "Full text content to chunk and embed")]
    pub content: String,
    #[schemars(description = "Rebuild chunks/embeddings for this source (default: true)")]
    pub reembed: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CompareFilesParams {
    #[schemars(description = "First file reference (project:relative_path or absolute path)")]
    pub file_a: String,
    #[schemars(description = "Second file reference (project:relative_path or absolute path)")]
    pub file_b: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindSimilarModulesParams {
    #[schemars(description = "Project name containing the module")]
    pub project: String,
    #[schemars(
        description = "Module path pattern (glob/substring match, e.g. 'work_pool' or 'src/cron')"
    )]
    pub module_path: String,
    #[schemars(description = "Minimum similarity threshold (default: 0.80)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Maximum number of results (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter results to a specific target project")]
    pub target_project: Option<String>,
    #[schemars(
        description = "If true, also return matches in worktrees / sibling clones \
                       of the seed file's repo (same git_common_dir or \
                       git_root_commits). Default false — same-repo matches are \
                       excluded so cross-repo refactor candidates aren't drowned \
                       out by the same code on different branches."
    )]
    pub include_same_repo: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindDuplicatesParams {
    #[schemars(description = "Minimum similarity threshold (default: 0.90)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Minimum number of projects a cluster must span (default: 2)")]
    pub min_projects: Option<usize>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Maximum number of clusters to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(description = "If true, include duplicates whose two projects are \
                       worktrees / sibling clones of the same upstream repo \
                       (same git_common_dir or git_root_commits). Default false. \
                       Most operators want false: same-code-different-branch is \
                       not a refactor candidate.")]
    pub include_same_repo: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactoringReportParams {
    #[schemars(description = "Minimum similarity threshold (default: 0.85)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Minimum number of projects a cluster must span (default: 2)")]
    pub min_projects: Option<usize>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Maximum number of candidates to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "If true, include refactor candidates whose two projects \
                       are worktrees / sibling clones of the same upstream repo. \
                       Default false."
    )]
    pub include_same_repo: Option<bool>,
}

/// Tier 2 — `chunk_clusters` (chunk-level cross-project DRY).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChunkClustersParams {
    #[schemars(
        description = "Minimum chunk-pair similarity. Threshold for clustering decisions (default: 0.88)."
    )]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Minimum chunks per cluster (default: 3)")]
    pub min_cluster_size: Option<usize>,
    #[schemars(
        description = "Minimum distinct projects a cluster must span (default: 2). Set 1 for intra-project."
    )]
    pub min_projects: Option<usize>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(
        description = "Filter pairs to those touching this project. Use to focus the audit on a single project's DRY violations."
    )]
    pub project: Option<String>,
    #[schemars(description = "Maximum clusters to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Worktree filter: \"main\" (default) restricts both endpoints to canonical \
                       main projects (e.g. f1r3node/, not f1r3node-reified-rspaces/). \"all\" \
                       allows feature-branch worktrees."
    )]
    pub worktree_filter: Option<String>,
    #[schemars(
        description = "If true, include pairs whose two projects are worktrees / sibling clones \
                       of the same upstream repo. Default false (cross-repo refactor candidates only)."
    )]
    pub include_same_repo: Option<bool>,
}

// ============================================================================
// Tier 5 — Audit & trend params
// ============================================================================

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DependencyHealthParams {
    #[schemars(description = "Filter to a single project (optional)")]
    pub project: Option<String>,
    #[schemars(
        description = "Worktree filter: \"main\" (default) restricts to canonical main projects."
    )]
    pub worktree_filter: Option<String>,
    #[schemars(description = "Maximum dependency entries to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PatternSearchParams {
    #[schemars(description = "Code snippet to find similar implementations for")]
    pub snippet: String,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Minimum cosine similarity (default: 0.78)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Maximum matches to return (default: 15)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Exclude this project from results (typically the caller's own project)"
    )]
    pub exclude_project: Option<String>,
    #[schemars(description = "Worktree filter: \"main\" (default) or \"all\"")]
    pub worktree_filter: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MergeConflictRiskParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Files in the in-flight branch (relative paths, required)")]
    pub branch_files: Vec<String>,
    #[schemars(description = "Lookback window in days (default: 14)")]
    pub window_days: Option<i32>,
    #[schemars(
        description = "Exclude this author email from the risk count (typically the PR author)"
    )]
    pub exclude_email: Option<String>,
    #[schemars(description = "Maximum files to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NamingConsistencyParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Filter by programming language (e.g. \"rust\", \"python\"). Omit to scan every language with a registered backend."
    )]
    pub language: Option<String>,
    #[schemars(
        description = "Minimum dominance for the per-(directory, kind) convention (default: 0.7). Below this threshold the directory is considered too mixed to flag divergences."
    )]
    pub min_dominance: Option<f64>,
    #[schemars(description = "Maximum divergences to return (default: 50)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Whether to embed `recommended_fix` per divergence (default: true). Set false to reproduce the diagnostic-only shape."
    )]
    pub include_fixes: Option<bool>,
}
