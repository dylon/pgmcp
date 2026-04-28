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

pub mod tool_anomaly_detection;
pub mod tool_architecture_quality;
pub mod tool_architecture_violations;
pub mod tool_bug_prediction;
pub mod tool_centrality_analysis;
pub mod tool_change_impact_analysis;
pub mod tool_circular_dependencies;
pub mod tool_code_summarize;
pub mod tool_community_detection;
pub mod tool_compare_files;
pub mod tool_complexity_hotspots;
pub mod tool_coupling_cohesion_report;
pub mod tool_dependency_graph;
pub mod tool_design_metrics;
pub mod tool_design_smell_detection;
pub mod tool_discover_topics;
pub mod tool_doc_coverage_gaps;
pub mod tool_engineering_scorecard;
pub mod tool_file_info;
pub mod tool_find_coupled_files;
pub mod tool_find_duplicates;
pub mod tool_find_misplaced_code;
pub mod tool_find_orphans;
pub mod tool_find_similar_modules;
pub mod tool_grep;
pub mod tool_hybrid_search;
pub mod tool_index_stats;
pub mod tool_list_projects;
pub mod tool_orient;
pub mod tool_project_tree;
pub mod tool_read_file;
pub mod tool_refactoring_report;
pub mod tool_reindex;
pub mod tool_search_commits;
pub mod tool_semantic_search;
pub mod tool_suggest_merges;
pub mod tool_suggest_splits;
pub mod tool_technical_debt_analysis;
pub mod tool_test_coverage_gaps;
pub mod tool_text_search;
pub mod tool_topic_hierarchy;
pub mod tool_topic_hierarchy_fcm;

pub use tool_anomaly_detection::tool_anomaly_detection;
pub use tool_architecture_quality::tool_architecture_quality;
pub use tool_architecture_violations::tool_architecture_violations;
pub use tool_bug_prediction::tool_bug_prediction;
pub use tool_centrality_analysis::tool_centrality_analysis;
pub use tool_change_impact_analysis::tool_change_impact_analysis;
pub use tool_circular_dependencies::tool_circular_dependencies;
pub use tool_code_summarize::tool_code_summarize;
pub use tool_community_detection::tool_community_detection;
pub use tool_compare_files::tool_compare_files;
pub use tool_complexity_hotspots::tool_complexity_hotspots;
pub use tool_coupling_cohesion_report::tool_coupling_cohesion_report;
pub use tool_dependency_graph::tool_dependency_graph;
pub use tool_design_metrics::tool_design_metrics;
pub use tool_design_smell_detection::tool_design_smell_detection;
pub use tool_discover_topics::tool_discover_topics;
pub use tool_doc_coverage_gaps::tool_doc_coverage_gaps;
pub use tool_engineering_scorecard::tool_engineering_scorecard;
pub use tool_file_info::tool_file_info;
pub use tool_find_coupled_files::tool_find_coupled_files;
pub use tool_find_duplicates::tool_find_duplicates;
pub use tool_find_misplaced_code::tool_find_misplaced_code;
pub use tool_find_orphans::tool_find_orphans;
pub use tool_find_similar_modules::tool_find_similar_modules;
pub use tool_grep::tool_grep;
pub use tool_hybrid_search::tool_hybrid_search;
pub use tool_index_stats::tool_index_stats;
pub use tool_list_projects::tool_list_projects;
pub use tool_orient::tool_orient;
pub use tool_project_tree::tool_project_tree;
pub use tool_read_file::tool_read_file;
pub use tool_refactoring_report::tool_refactoring_report;
pub use tool_reindex::tool_reindex;
pub use tool_search_commits::tool_search_commits;
pub use tool_semantic_search::tool_semantic_search;
pub use tool_suggest_merges::tool_suggest_merges;
pub use tool_suggest_splits::tool_suggest_splits;
pub use tool_technical_debt_analysis::tool_technical_debt_analysis;
pub use tool_test_coverage_gaps::tool_test_coverage_gaps;
pub use tool_text_search::tool_text_search;
pub use tool_topic_hierarchy::tool_topic_hierarchy;
pub use tool_topic_hierarchy_fcm::tool_topic_hierarchy_fcm;
