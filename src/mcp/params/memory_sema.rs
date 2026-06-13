//! Memory tail, semantic-shape, fuzzy & phonetic parameter types.
//!
//! Extracted verbatim from `server.rs` (B.2 god-file split). All structs
//! re-exported by `params/mod.rs` and, transitively, by `server.rs` so
//! `crate::mcp::server::<Name>Params` resolves for every tool body file.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryFindEntitiesForCodeParams {
    pub file_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub topic_id: Option<i64>,
    #[schemars(description = "Find entities anchored to this file_symbols.id.")]
    pub symbol_id: Option<i64>,
    #[schemars(description = "Find entities anchored to this projects.id.")]
    pub project_id: Option<i32>,
}

// ----------------------------------------------------------------------------
// Phase 6 graph-enhanced retrieval Params
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryUnifiedSearchParams {
    pub query: String,
    #[schemars(
        description = "Optional whitelist of node_types to include (e.g. ['memory_entity','observation','chunk','topic','durable_mandate','commit','work_item','experiment','prompt','data_table','agent_message','a2a_message','coordination_request'])."
    )]
    pub node_types: Option<Vec<String>>,
    pub k: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConversationSearchParams {
    #[schemars(
        description = "Free-text query. Vector-searches the A2A / coordination conversation \
                       nodes in the unified graph (a2a_message, agent_message, a2a_task, \
                       coordination_request). A convenience wrapper over memory_unified_search \
                       with node_types pinned to the conversation family."
    )]
    pub query: String,
    #[schemars(description = "Max rows (default 10).")]
    pub k: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryNeighborsParams {
    #[schemars(description = "Composite node_id of the seed (e.g. 'memory_entity:42').")]
    pub node_id: String,
    pub depth: Option<i32>,
    pub edge_filter: Option<String>,
    pub max_nodes: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GraphNeighborsParams {
    #[schemars(
        description = "Friendly node reference '<type>:<key>' — key is a natural id (file path, \
project/topic name, work_item public_id, experiment slug, commit sha, symbol name, agent id) or \
a numeric pk. E.g. 'work_item:WI-12', 'file:src/foo.rs', 'project:pgmcp', 'agent:codex', 'chunk:42'."
    )]
    pub node_ref: String,
    #[schemars(description = "Traversal depth (default 1, max 4).")]
    pub depth: Option<i32>,
    #[schemars(description = "Optional edge_type filter (e.g. 'validated_by', 'in_project').")]
    pub edge_filter: Option<String>,
    #[schemars(description = "Hard cap on total nodes returned (default 200, max 500).")]
    pub max_nodes: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryPathSearchParams {
    pub query: String,
    pub seed_node_types: Option<Vec<String>>,
    pub target_node_types: Option<Vec<String>>,
    pub max_hops: Option<i32>,
    pub k: Option<i32>,
    #[schemars(description = "PathRAG prune threshold; paths with Jaccard ≥ this are pruned.")]
    pub prune_jaccard: Option<f32>,
    #[schemars(
        description = "Stage 5b: as-of point-in-time (RFC3339, e.g. '2026-01-01T00:00:00Z') — \
restrict traversal to edges valid at that time (the graph as it was). Omit for the current graph."
    )]
    pub as_of: Option<String>,
    #[schemars(
        description = "Stage 5b: recency half-life in days (default 90) — recent edges are \
up-weighted in path scoring; timeless structural edges are never decayed."
    )]
    pub half_life_days: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryPprSearchParams {
    pub query: String,
    pub k: Option<i32>,
    #[schemars(description = "PageRank teleport probability (default 0.85).")]
    pub alpha: Option<f64>,
    pub max_seeds: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryRaptorSearchParams {
    pub query: String,
    pub scope_id: Option<i64>,
    #[schemars(
        description = "Optional tree-level filter. Level 0 = leaves; level k = k-th summary tier."
    )]
    pub levels: Option<Vec<i32>>,
    pub k: Option<i32>,
}

// ----------------------------------------------------------------------------
// Phase 10 client-profile introspection Params
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PgmcpClientProfileParams {
    #[schemars(
        description = "Client name to look up (case-insensitive). Defaults to 'generic' when omitted. Match against MCP `clientInfo.name`."
    )]
    pub client_name: Option<String>,
    #[schemars(
        description = "When true, return every registered profile instead of resolving one client name. Default false."
    )]
    pub list_all: Option<bool>,
}

// ----------------------------------------------------------------------------
// Phase 8 forget Params
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryForgetParams {
    #[schemars(description = "Target row type: 'entity' | 'observation' | 'relation'.")]
    pub target_type: String,
    pub target_id: i64,
    #[schemars(
        description = "When true, hard-delete the row + every dependent FK row and write an audit manifest. \
                       Default false (soft-delete via valid_to)."
    )]
    pub cascade: Option<bool>,
    #[schemars(description = "Actor label written to memory_forget_log (default 'agent').")]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryPurgeExpiredParams {
    pub window_days: Option<i64>,
    pub importance_threshold: Option<f32>,
    #[schemars(description = "When true (default), report counts only — do not delete.")]
    pub dry_run: Option<bool>,
    #[serde(default)]
    #[schemars(
        description = "Optional project name (list_projects) to scope the effect_breakdown channel; omit for an empty breakdown."
    )]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryReflectParams {
    pub scope: Option<MemoryScopeParam>,
    #[schemars(
        description = "Optional session UUID — stamps the source on reflection-emitted observations."
    )]
    pub session_id: Option<String>,
    #[schemars(description = "RFC3339 lower-bound on observation creation time. Optional.")]
    pub since: Option<String>,
    #[schemars(
        description = "Max observations to consider in the reflection window. Default 200."
    )]
    pub max_observations: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchMandatesParams {
    #[schemars(description = "Free-text search query — full-text matched against \
                       `imperative || target` in `durable_mandates`.")]
    pub query: String,
    #[schemars(
        description = "Optional polarity filter (one of: always, never, prefer, avoid, \
                       remember, from_now_on, correction, permission, constraint, mandate, \
                       process_rule, project_rule)."
    )]
    pub polarity: Option<String>,
    #[schemars(description = "Optional scope filter ('project' or 'workspace').")]
    pub scope: Option<String>,
    #[schemars(
        description = "Optional project_id filter. Workspace-scoped mandates are always \
                       returned regardless of this filter."
    )]
    pub project_id: Option<i32>,
    #[schemars(description = "Max rows (1..=200, default 20).")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Ranking mode: 'fts' (Postgres full-text, default), 'semantic' (BGE-M3 \
                       vector similarity over the mandate embedding), or 'hybrid' (RRF-fused \
                       FTS + semantic). Semantic/hybrid embed the query."
    )]
    pub mode: Option<String>,
}

// ============================================================================
// Phase D2b — new tool params (6 new MCP tools)
// ============================================================================

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrossLanguageApiEquivalentsParams {
    #[schemars(description = "Minimum similarity (0.0..=1.0, default 0.7).")]
    pub min_similarity: Option<f32>,
    #[schemars(description = "Maximum number of pairs to return (default 50).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TypeShapeSearchParams {
    #[schemars(description = "Project name.")]
    pub project: String,
    #[schemars(description = "Required tags in return_type_tags (subset semantics).")]
    pub return_type_tags: Option<Vec<String>>,
    #[schemars(description = "Required tags in any parameter's type_tags (subset semantics).")]
    pub parameter_type_tags: Option<Vec<String>>,
    #[schemars(description = "Required effects (any of).")]
    pub effects: Option<Vec<String>>,
    #[schemars(description = "Maximum matches to return (default 50).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindCallersBySignatureParams {
    #[schemars(description = "Project name.")]
    pub project: String,
    #[schemars(description = "Resolved target path (e.g. crate::auth::validate).")]
    pub target_path: String,
    #[schemars(description = "Filter callers by parameter type-tag intersection.")]
    pub parameter_type_tags: Option<Vec<String>>,
    #[schemars(description = "Restrict the type-tag filter to a specific parameter position.")]
    pub parameter_position: Option<i32>,
    #[schemars(description = "Filter callers by their own effects (any of).")]
    pub caller_effects: Option<Vec<String>>,
    #[schemars(description = "Maximum callers to return (default 50).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EffectPropagationParams {
    #[schemars(description = "Project name.")]
    pub project: String,
    #[schemars(description = "Forward mode: BFS reachability from this seed symbol_id.")]
    pub seed_symbol_id: Option<i64>,
    #[schemars(description = "Reverse mode: find symbols that reach any of these effects.")]
    pub target_effects: Vec<String>,
    #[schemars(description = "Maximum BFS depth (1..=32, default 8).")]
    pub max_depth: Option<u32>,
    #[schemars(description = "Maximum results to return (default 50).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EffectDriftParams {
    #[schemars(
        description = "Filter to a single project by name. Optional (omit = all projects)."
    )]
    pub project: Option<String>,
    #[schemars(
        description = "Filter to a single effect (e.g. 'unsafe', 'async', 'blocking_io', \
                       'may_panic'). Optional."
    )]
    pub effect: Option<String>,
    #[schemars(
        description = "Filter to 'gained' or 'lost' transitions only. Optional (omit = both)."
    )]
    pub change: Option<String>,
    #[schemars(
        description = "Only transitions observed within the last N days. Optional (omit = all time)."
    )]
    pub since_days: Option<i64>,
    #[schemars(description = "Maximum rows to return, newest first (default 50, max 500).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TypeTagDictionaryParams {
    // No filter parameters — this tool is a self-documenting introspection
    // surface for the vocabulary catalogs. The empty struct keeps the
    // JSON-schema shape uniform across tool params.
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SignatureLintParams {
    #[schemars(description = "Project name.")]
    pub project: String,
    #[schemars(description = "Maximum results per finding category (default 50).")]
    pub limit: Option<i32>,
}

// Phase 8 — index-backed fuzzy / phonetic / correction tool params.
// The caller-supplied "haystack" param family (substring/token/fuzzy/phonetic
// grep, time-series, mandate-dedup, rename-oracle, articulatory & phonetic
// distance/naming, code_property_graph/subtree_mining/paradigm_profile/
// gnn_semantic_issues) was removed 2026-06-13 — it described data the agent
// supplied inline, with no linkage to the indexed corpus.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DendrogramTopicHierarchyParams {
    #[schemars(description = "Project name.")]
    pub project: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FuzzySymbolSearchParams {
    #[schemars(description = "Query symbol (approximate match).")]
    pub query: String,
    /// Project name (REQUIRED). The persistent symbol trie is
    /// per-project — there is no global view. Callers wanting a
    /// global search should iterate `list_projects` client-side
    /// and merge results.
    #[schemars(
        description = "Project name (required — the persistent symbol trie is per-project)."
    )]
    pub project: String,
    #[schemars(description = "Max edit distance (default 2).")]
    pub max_distance: Option<u32>,
    #[schemars(description = "Result limit (default 20).")]
    pub limit: Option<u32>,
    #[schemars(
        description = "If true, match in phonetic-normalized space (composed phonetic∘edit) instead of raw edit distance. Default false. For a richer phonetic result with kind/visibility, use phonetic_symbol_search."
    )]
    pub phonetic: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FuzzyPathSearchParams {
    #[schemars(description = "Query path fragment (approximate match).")]
    pub query: String,
    /// Project name (REQUIRED). The persistent path trie is
    /// per-project — there is no global view. Callers wanting a
    /// global search should iterate `list_projects` client-side
    /// and merge results.
    #[schemars(description = "Project name (required — the persistent path trie is per-project).")]
    pub project: String,
    #[schemars(description = "Max edit distance (default 2).")]
    pub max_distance: Option<u32>,
    #[schemars(description = "Result limit (default 20).")]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CorrectQueryParams {
    #[schemars(description = "User query to correct.")]
    pub query: String,
    #[schemars(
        description = "Project whose persistent symbol vocabulary + n-gram LM drive the correction."
    )]
    pub project: String,
    #[schemars(description = "Max per-token edit distance for candidate generation (default 2).")]
    pub max_distance: Option<u32>,
    #[schemars(
        description = "Language-model interpolation weight 0.0–1.0 (default 0.5; 0 disables LM rescoring)."
    )]
    pub lm_weight: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PhoneticSymbolSearchParams {
    #[schemars(description = "Query symbol (composed phonetic∘edit match in normalized space).")]
    pub query: String,
    #[schemars(description = "Project to search — its persistent symbol trie is consulted.")]
    pub project: String,
    #[schemars(description = "Max edit distance in phonetic-normalized space (default 2).")]
    pub max_distance: Option<u32>,
    #[schemars(description = "Maximum number of results (default 20).")]
    pub limit: Option<u32>,
}
