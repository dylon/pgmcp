//! Per-tool free-function bodies for MCP tools.
//!
//! Each tool exposes `pub async fn tool_<name>(ctx: &SystemContext,
//! params: <Name>Params) -> Result<CallToolResult, McpError>`. The
//! `#[tool]`-annotated method on `McpServer` is a one-line forward into
//! the corresponding `tool_<name>` here. rmcp's `#[tool_router]` macro
//! still owns schema + dispatch; this module owns the bodies.
//!
//! The `pub use` re-exports below let external callers (incl. tests in
//! `pgmcp-testing/tests/`) access tools without the `tool_<name>::`
//! qualifier. Internal forwards from `super::server` use the qualified
//! path explicitly, so the re-exports are dead from the bin's perspective
//! — the `#[allow]` is intentional.

#![allow(unused_imports)]

// Shared infrastructure for recommendation-shaped tools.
// `fix_actions` defines the typed `RecommendedFix` action enum; `fix_helpers`
// extracts the import-graph loader / callsite scanner shared across multiple
// tool bodies (was previously inlined and duplicated in
// tool_architecture_violations.rs and tool_circular_dependencies.rs).
pub mod fix_actions;
pub mod fix_helpers;

pub mod tool_adoption_lag;
pub mod tool_anomaly_detection;
pub mod tool_architecture_quality;
pub mod tool_architecture_violations;
pub mod tool_boilerplate_clusters;
pub mod tool_bug_prediction;
pub mod tool_bus_factor_map;
pub mod tool_centrality_analysis;
pub mod tool_change_impact_analysis;
pub mod tool_chunk_clusters;
pub mod tool_circular_dependencies;
pub mod tool_code_summarize;
pub mod tool_community_detection;
pub mod tool_compare_files;
pub mod tool_complexity_hotspots;
pub mod tool_coupling_cohesion_report;
pub mod tool_dependency_graph;
pub mod tool_dependency_health;
pub mod tool_design_metrics;
pub mod tool_design_smell_detection;
pub mod tool_discover_topics;
pub mod tool_doc_coverage_gaps;
pub mod tool_engineering_scorecard;
pub mod tool_extraction_candidates;
pub mod tool_file_info;
pub mod tool_find_coupled_files;
pub mod tool_find_duplicates;
pub mod tool_find_misplaced_code;
pub mod tool_find_orphans;
pub mod tool_find_similar_modules;
pub mod tool_fix_circular_dependency;
pub mod tool_grep;
pub mod tool_hot_path_audit;
pub mod tool_hybrid_search;
pub mod tool_index_stats;
pub mod tool_internal_dry;
pub mod tool_list_projects;
pub mod tool_mandate_context;
pub mod tool_mcp_tool_telemetry;
pub mod tool_memory_crud;
pub mod tool_merge_conflict_risk;
pub mod tool_module_growth;
pub mod tool_naming_consistency;
pub mod tool_orient;
pub mod tool_pattern_abstraction;
pub mod tool_pattern_search;
pub mod tool_pr_scope;
pub mod tool_project_tree;
pub mod tool_read_file;
pub mod tool_recall_prompts;
pub mod tool_recommend_layering;
pub mod tool_recommend_module_split;
pub mod tool_refactoring_report;
pub mod tool_reindex;
pub mod tool_reviewer_recommender;
pub mod tool_search_commits;
pub mod tool_search_mandates;
pub mod tool_semantic_search;
pub mod tool_session_mandates;
pub mod tool_shotgun_surgery_fix;
pub mod tool_software_patterns;
pub mod tool_stale_zombie;
pub mod tool_suggest_merges;
pub mod tool_suggest_splits;
pub mod tool_tech_debt_burn_down;
pub mod tool_technical_debt_analysis;
pub mod tool_test_coverage_gaps;
pub mod tool_text_search;
pub mod tool_topic_hierarchy;
pub mod tool_topic_hierarchy_fcm;

pub use tool_adoption_lag::tool_adoption_lag;
pub use tool_anomaly_detection::tool_anomaly_detection;
pub use tool_architecture_quality::tool_architecture_quality;
pub use tool_architecture_violations::tool_architecture_violations;
pub use tool_boilerplate_clusters::tool_boilerplate_clusters;
pub use tool_bug_prediction::tool_bug_prediction;
pub use tool_bus_factor_map::tool_bus_factor_map;
pub use tool_centrality_analysis::tool_centrality_analysis;
pub use tool_change_impact_analysis::tool_change_impact_analysis;
pub use tool_chunk_clusters::tool_chunk_clusters;
pub use tool_circular_dependencies::tool_circular_dependencies;
pub use tool_code_summarize::tool_code_summarize;
pub use tool_community_detection::tool_community_detection;
pub use tool_compare_files::tool_compare_files;
pub use tool_complexity_hotspots::tool_complexity_hotspots;
pub use tool_coupling_cohesion_report::tool_coupling_cohesion_report;
pub use tool_dependency_graph::tool_dependency_graph;
pub use tool_dependency_health::tool_dependency_health;
pub use tool_design_metrics::tool_design_metrics;
pub use tool_design_smell_detection::tool_design_smell_detection;
pub use tool_discover_topics::tool_discover_topics;
pub use tool_doc_coverage_gaps::tool_doc_coverage_gaps;
pub use tool_engineering_scorecard::tool_engineering_scorecard;
pub use tool_extraction_candidates::tool_extraction_candidates;
pub use tool_file_info::tool_file_info;
pub use tool_find_coupled_files::tool_find_coupled_files;
pub use tool_find_duplicates::tool_find_duplicates;
pub use tool_find_misplaced_code::tool_find_misplaced_code;
pub use tool_find_orphans::tool_find_orphans;
pub use tool_find_similar_modules::tool_find_similar_modules;
pub use tool_fix_circular_dependency::tool_fix_circular_dependency;
pub use tool_grep::tool_grep;
pub use tool_hot_path_audit::tool_hot_path_audit;
pub use tool_hybrid_search::tool_hybrid_search;
pub use tool_index_stats::tool_index_stats;
pub use tool_internal_dry::tool_internal_dry;
pub use tool_list_projects::tool_list_projects;
pub use tool_mandate_context::tool_mandate_context;
pub use tool_merge_conflict_risk::tool_merge_conflict_risk;
pub use tool_module_growth::tool_module_growth;
pub use tool_naming_consistency::tool_naming_consistency;
pub use tool_orient::tool_orient;
pub use tool_pattern_abstraction::tool_pattern_abstraction;
pub use tool_pattern_search::tool_pattern_search;
pub use tool_pr_scope::tool_pr_scope;
pub use tool_project_tree::tool_project_tree;
pub use tool_read_file::tool_read_file;
pub use tool_recall_prompts::tool_recall_prompts;
pub use tool_recommend_layering::tool_recommend_layering;
pub use tool_recommend_module_split::tool_recommend_module_split;
pub use tool_refactoring_report::tool_refactoring_report;
pub use tool_reindex::tool_reindex;
pub use tool_reviewer_recommender::tool_reviewer_recommender;
pub use tool_search_commits::tool_search_commits;
pub use tool_search_mandates::tool_search_mandates;
pub use tool_semantic_search::tool_semantic_search;
pub use tool_shotgun_surgery_fix::tool_shotgun_surgery_fix;
pub use tool_software_patterns::{
    tool_get_software_pattern, tool_list_software_patterns, tool_pattern_catalog_stats,
    tool_recommend_design_patterns, tool_refresh_pattern_catalog, tool_review_design_patterns,
    tool_software_pattern_search, tool_upsert_pattern_source,
};
pub use tool_stale_zombie::tool_stale_zombie;
pub use tool_suggest_merges::tool_suggest_merges;
pub use tool_suggest_splits::tool_suggest_splits;
pub use tool_tech_debt_burn_down::tool_tech_debt_burn_down;
pub use tool_technical_debt_analysis::tool_technical_debt_analysis;
pub use tool_test_coverage_gaps::tool_test_coverage_gaps;
pub use tool_text_search::tool_text_search;
pub use tool_topic_hierarchy::tool_topic_hierarchy;
pub use tool_topic_hierarchy_fcm::tool_topic_hierarchy_fcm;
