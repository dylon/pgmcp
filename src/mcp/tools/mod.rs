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
// `sema_helpers` is the home for shadow-ASR-aware JOIN patterns shared
// across the Phase D2b tool upgrades (signatures, effects, resolved
// edges, type-tag filters, cross-language equivalence reads).
pub mod sema_helpers;
pub mod sota_helpers;
pub mod sota_regex_scan;
pub mod tool_ontology;

pub mod tool_active_clients;
pub mod tool_adoption_lag;
pub mod tool_adoption_report;
pub mod tool_anomaly_detection;
pub mod tool_architecture_dsm;
pub mod tool_architecture_quality;
pub mod tool_architecture_violations;
pub mod tool_boilerplate_clusters;
pub mod tool_bug_prediction;
pub mod tool_bus_factor_map;
pub mod tool_centrality_analysis;
pub mod tool_change_impact_analysis;
pub mod tool_chunk_clusters;
pub mod tool_circular_dependencies;
pub mod tool_ck_metrics;
pub mod tool_client_profile;
pub mod tool_client_project_matrix;
pub mod tool_code_on_fire;
pub mod tool_code_path_search;
pub mod tool_code_ppr_search;
pub mod tool_code_raptor_search;
pub mod tool_code_summarize;
pub mod tool_community_detection;
pub mod tool_compare_files;
pub mod tool_complexity_hotspots;
pub mod tool_conversation_search;
pub mod tool_coordinate_dependency_block;
pub mod tool_coordination_respond;
pub mod tool_coupling_cohesion_report;
pub mod tool_cross_language_api_equivalents;
pub mod tool_dependency_graph;
pub mod tool_dependency_health;
pub mod tool_design_metrics;
pub mod tool_design_smell_detection;
pub mod tool_discover_topics;
pub mod tool_doc_coverage_gaps;
pub mod tool_effect_drift;
pub mod tool_effect_propagation;
pub mod tool_engineering_scorecard;
pub mod tool_extraction_candidates;
pub mod tool_file_info;
pub mod tool_find_callers_by_signature;
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
pub mod tool_memory_ext;
pub mod tool_memory_forget;
pub mod tool_memory_graph_rag;
pub mod tool_memory_reflect;
pub mod tool_merge_conflict_risk;
pub mod tool_module_growth;
pub mod tool_naming_consistency;
pub mod tool_orient;
pub mod tool_pattern_abstraction;
pub mod tool_pattern_search;
pub mod tool_pr_scope;
pub mod tool_project_dependencies;
pub mod tool_project_dependents;
pub mod tool_project_tree;
pub mod tool_quality_forecast;
pub mod tool_quality_report;
pub mod tool_quality_trend;
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
pub mod tool_signature_lint;
pub mod tool_software_patterns;
pub mod tool_stale_zombie;
pub mod tool_suggest_merges;
pub mod tool_suggest_splits;
pub mod tool_suggest_worktree;
pub mod tool_tech_debt_burn_down;
pub mod tool_technical_debt_analysis;
pub mod tool_test_coverage_gaps;
pub mod tool_text_search;
pub mod tool_topic_hierarchy;
pub mod tool_topic_hierarchy_fcm;
pub mod tool_type_shape_search;
pub mod tool_type_tag_dictionary;
pub mod tool_work_summary;

// Phase 8 — code-analysis + fuzzy + phonetic MCP tools (full surface).
pub mod tool_articulatory_distance;
pub mod tool_articulatory_naming_consistency;
pub mod tool_code_property_graph;
pub mod tool_correct_query;
pub mod tool_dendrogram_topic_hierarchy;
pub mod tool_expand_query_to_phonetic_pattern;
pub mod tool_fuzzy_grep;
pub mod tool_fuzzy_path_search;
pub mod tool_fuzzy_symbol_search;
pub mod tool_gnn_semantic_issues;
pub mod tool_mandate_dedup_v2;
pub mod tool_paradigm_profile;
pub mod tool_phonetic_grep_comments;
pub mod tool_phonetic_naming_consistency;
pub mod tool_phonetic_normalize;
pub mod tool_phonetic_symbol_search;
pub mod tool_rename_oracle;
pub mod tool_substring_search;
pub mod tool_subtree_mining;
pub mod tool_time_series_fuzzy_match;
pub mod tool_token_grep;

// SOTA Phase 2 — graph algorithms
pub mod tool_attack_vulnerability;
pub mod tool_edge_betweenness;
pub mod tool_kcore_analysis;
pub mod tool_ktruss_analysis;
pub mod tool_motif_census;
pub mod tool_personalized_pagerank;
pub mod tool_structural_holes;

// SOTA Phase 3 — information theory
pub mod tool_cochange_mutual_information;
pub mod tool_compression_distance;
pub mod tool_identifier_entropy;
pub mod tool_import_entropy;
pub mod tool_import_hygiene;

// SOTA Phase 4 — evolution + quality
pub mod tool_bus_factor;
pub mod tool_doc_code_drift;
pub mod tool_flaky_test_candidates;
pub mod tool_knowledge_silos;
pub mod tool_mutation_score_surrogate;
pub mod tool_ownership_coupling_mismatch;
pub mod tool_test_smells;

// SOTA Phase 5 — concurrency / safety / performance
pub mod tool_blocking_in_async;
pub mod tool_channel_deadlock;
pub mod tool_clone_density;
pub mod tool_concurrency_bottlenecks;
pub mod tool_concurrency_forecast;
pub mod tool_deadlock_candidates;
pub mod tool_deadlock_cycles;
pub mod tool_io_hotpath;
pub mod tool_lock_order_graph;
pub mod tool_lockset_races;
pub mod tool_missing_preallocation;
pub mod tool_panic_paths;
pub mod tool_quadratic_loops;
pub mod tool_send_sync_violations;
pub mod tool_sync_skeleton;
pub mod tool_unsafe_clusters;

// SOTA Phase 6 — security
pub mod tool_crypto_misuse;
pub mod tool_cve_supply_chain;
pub mod tool_injection_candidates;
pub mod tool_secret_detection;
pub mod tool_taint_analysis;
pub mod tool_unprotected_routes;
pub mod tool_unsafe_deserialization;

// Developer-tool ("toolbox") catalog — installed FV + profiling/debug tools (v32).
pub mod tool_toolbox;

// SOTA Phase 7 — API / contract
pub mod tool_api_stability;
pub mod tool_deprecated_but_used;
pub mod tool_public_api_surface;
pub mod tool_semver_break_audit;

// SOTA Phase 8 — ML / embedding-based
pub mod tool_embedding_outliers;
pub mod tool_lsh_clone_detection;
pub mod tool_multi_resolution_pagerank;
pub mod tool_semantic_drift;

// SOTA Phase 9 — data engineering
pub mod tool_dead_columns;
pub mod tool_migration_safety;
pub mod tool_pii_spread;

// SOTA Phase 10 — call-graph downstream
pub mod tool_dead_code_reachability;
pub mod tool_feature_envy;
pub mod tool_lcom4;
pub mod tool_shotgun_surgery;

// Graph-roadmap Phase 1.1 — function-level graph analytics (the file-graph
// algorithm library, genericized to also run on the symbol call graph).
pub mod tool_central_functions;
pub mod tool_extended_centrality;
pub mod tool_function_communities;
pub mod tool_function_kcore;
pub mod tool_recursive_clusters;

// Graph-roadmap Phase 2.6 — Tier-A graph algorithms (file or call graph).
pub mod graph_scope;
pub mod tool_articulation_points;
pub mod tool_dominator_tree;
pub mod tool_hits;

// Graph-roadmap Phase 3.6 — connectivity, min-cut, Leiden refinement.
pub mod tool_graph_connectivity;

// Graph-roadmap Phase 4.6 — spectral connectivity + WL structural clones.
pub mod tool_spectral_analysis;

// SOTA Phase 11 — evolution analytics
pub mod tool_commit_changepoint;
pub mod tool_commit_topic_drift;
pub mod tool_refactor_pressure;
pub mod tool_release_api_stability;

// Documented tech debt (post-SOTA addition)
pub mod tool_documented_tech_debt;

// Operational: on-demand cron trigger (skips the Ready-delay + interval
// wait when the operator needs symbol/function-metric/call-graph data
// for `dead_code_reachability` / `naming_consistency` immediately).
pub mod tool_trajectory_similarity;
pub mod tool_trigger_cron;

// A2A inter-agent IPC bridge — outbound MCP-side tools
pub mod tool_a2a_ack_message;
pub mod tool_a2a_active_agents;
pub mod tool_a2a_cancel_task;
pub mod tool_a2a_find_agents_by_specialty;
pub mod tool_a2a_get_task;
pub mod tool_a2a_inbox;
pub mod tool_a2a_list_agents;
pub mod tool_a2a_pattern_deliberation;
pub mod tool_a2a_pattern_distillation;
pub mod tool_a2a_pattern_mixture;
pub mod tool_a2a_pattern_recursive;
pub mod tool_a2a_pattern_sequential;
pub mod tool_a2a_register_agent;
pub mod tool_a2a_reply_message;
pub mod tool_a2a_report_outcome;
pub mod tool_a2a_send_message;
pub mod tool_a2a_send_task;
pub mod tool_a2a_subscribe_task;
pub mod tool_csm_infer_peer_fsm;
pub mod tool_csm_list_protocols;
pub mod tool_csm_protocol_of_pattern;
pub mod tool_csm_protocol_plan;
pub mod tool_csm_show_projection;
pub mod tool_csm_validate_run;
pub mod tool_experiments;
pub use tool_experiments::{
    tool_experiment_decide, tool_experiment_get, tool_experiment_list,
    tool_experiment_log_artifact, tool_experiment_open, tool_experiment_protocol,
    tool_experiment_record_measurement, tool_experiment_render_ledger, tool_experiment_search,
    tool_experiment_timeline,
};

// Work-item / plan tracker tool surface (CRUD + lifecycle). Submodule layout
// (`crud` + `lifecycle`) mirrors the tracker's domain split.
pub mod work_items;

// JSON data tables (client-defined tables of observation rows). Submodule
// layout (`ddl` / `dml` / `analysis` / `search`) mirrors SQL DDL/DML + the
// analysis & discovery surface. Domain in `crate::datatable`, queries in
// `crate::db::queries::data_tables`, schema in the v19 migration.
pub mod data_tables;

pub use tool_active_clients::tool_active_clients;
pub use tool_adoption_lag::tool_adoption_lag;
pub use tool_anomaly_detection::tool_anomaly_detection;
pub use tool_architecture_dsm::tool_architecture_dsm;
pub use tool_architecture_quality::tool_architecture_quality;
pub use tool_architecture_violations::tool_architecture_violations;
pub use tool_boilerplate_clusters::tool_boilerplate_clusters;
pub use tool_bug_prediction::tool_bug_prediction;
pub use tool_bus_factor_map::tool_bus_factor_map;
pub use tool_centrality_analysis::tool_centrality_analysis;
pub use tool_change_impact_analysis::tool_change_impact_analysis;
pub use tool_chunk_clusters::tool_chunk_clusters;
pub use tool_circular_dependencies::tool_circular_dependencies;
pub use tool_ck_metrics::tool_ck_metrics;
pub use tool_client_project_matrix::tool_client_project_matrix;
pub use tool_code_path_search::tool_code_path_search;
pub use tool_code_ppr_search::tool_code_ppr_search;
pub use tool_code_raptor_search::tool_code_raptor_search;
pub use tool_code_summarize::tool_code_summarize;
pub use tool_community_detection::tool_community_detection;
pub use tool_compare_files::tool_compare_files;
pub use tool_complexity_hotspots::tool_complexity_hotspots;
pub use tool_conversation_search::tool_conversation_search;
pub use tool_coordinate_dependency_block::tool_coordinate_dependency_block;
pub use tool_coordination_respond::tool_coordination_respond;
pub use tool_coupling_cohesion_report::tool_coupling_cohesion_report;
pub use tool_cross_language_api_equivalents::tool_cross_language_api_equivalents;
pub use tool_dependency_graph::tool_dependency_graph;
pub use tool_dependency_health::tool_dependency_health;
pub use tool_design_metrics::tool_design_metrics;
pub use tool_design_smell_detection::tool_design_smell_detection;
pub use tool_discover_topics::tool_discover_topics;
pub use tool_doc_coverage_gaps::tool_doc_coverage_gaps;
pub use tool_effect_drift::tool_effect_drift;
pub use tool_effect_propagation::tool_effect_propagation;
pub use tool_engineering_scorecard::tool_engineering_scorecard;
pub use tool_extraction_candidates::tool_extraction_candidates;
pub use tool_file_info::tool_file_info;
pub use tool_find_callers_by_signature::tool_find_callers_by_signature;
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
pub use tool_project_dependencies::tool_project_dependencies;
pub use tool_project_dependents::tool_project_dependents;
pub use tool_project_tree::tool_project_tree;
pub use tool_quality_forecast::tool_quality_forecast;
pub use tool_quality_report::tool_quality_report;
pub use tool_quality_trend::tool_quality_trend;
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
pub use tool_signature_lint::tool_signature_lint;
pub use tool_software_patterns::{
    tool_get_software_pattern, tool_list_software_patterns, tool_pattern_catalog_stats,
    tool_recommend_design_patterns, tool_refresh_pattern_catalog, tool_review_design_patterns,
    tool_software_pattern_search, tool_upsert_pattern_source,
};
pub use tool_stale_zombie::tool_stale_zombie;
pub use tool_suggest_merges::tool_suggest_merges;
pub use tool_suggest_splits::tool_suggest_splits;
pub use tool_suggest_worktree::tool_suggest_worktree;
pub use tool_tech_debt_burn_down::tool_tech_debt_burn_down;
pub use tool_technical_debt_analysis::tool_technical_debt_analysis;
pub use tool_test_coverage_gaps::tool_test_coverage_gaps;
pub use tool_text_search::tool_text_search;
pub use tool_topic_hierarchy::tool_topic_hierarchy;
pub use tool_topic_hierarchy_fcm::tool_topic_hierarchy_fcm;
pub use tool_type_shape_search::tool_type_shape_search;
pub use tool_type_tag_dictionary::tool_type_tag_dictionary;
pub use tool_work_summary::tool_work_summary;
