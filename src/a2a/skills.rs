//! Skills surfaced in pgmcp's AgentCard.
//!
//! Comprises three curated umbrella skills + per-tool auto-enumeration. The
//! per-tool list is built by walking the `#[tool]` registry surfaced via
//! `McpServer::tool_router()` — but since rmcp's tool list is dynamic, we
//! also maintain a fixed authoritative list here so the AgentCard renders
//! without a live MCP handle.

#![allow(dead_code)]

use super::types::AgentSkill;

/// Three high-level umbrella skills surfaced on every AgentCard.
pub fn umbrella_skills() -> Vec<AgentSkill> {
    vec![
        AgentSkill {
            id: "code-analysis".into(),
            name: "Code analysis".into(),
            description: "155+ MCP tools for semantic search, graph theory, architecture review, ML, security, and evolution analytics over indexed code workspaces.".into(),
            tags: vec!["analysis".into(), "search".into(), "graph".into(), "security".into()],
            examples: vec![
                "Find all panic paths in project X".into(),
                "Compute K-core decomposition for project Y".into(),
                "Surface PII exposure in logging code".into(),
            ],
            specialty: vec!["analysis".into(), "code-health".into()],
            recommended_role: Some("Code Health Analyst".into()),
        },
        AgentSkill {
            id: "documented-tech-debt".into(),
            name: "Documented technical debt".into(),
            description: "Surface TODO/FIXME/HACK/etc. comments, stub macros (todo!(), NotImplementedError, etc.), and deprecation annotations with severity tiers, git-blame attribution, and GitHub-issue refs.".into(),
            tags: vec!["debt".into(), "quality".into(), "comments".into()],
            examples: vec![
                "List every high-severity debt marker in pgmcp".into(),
                "Show me TODOs older than 90 days".into(),
            ],
            specialty: vec!["code-health".into(), "quality".into()],
            recommended_role: Some("Quality Reviewer".into()),
        },
        AgentSkill {
            id: "cross-project-search".into(),
            name: "Cross-project semantic search".into(),
            description: "Vector + BM25 hybrid search across every indexed workspace project, with optional project / language / topic filtering.".into(),
            tags: vec!["search".into(), "semantic".into(), "vector".into()],
            examples: vec![
                "Find error-handling patterns across all my projects".into(),
                "Where do we use Postgres LISTEN/NOTIFY?".into(),
            ],
            specialty: vec!["search".into(), "retrieval".into()],
            recommended_role: Some("Search Specialist".into()),
        },
    ]
}

/// Build the full skills list for the AgentCard.
///
/// Combines umbrella skills + a sampled subset of per-tool skills. Each
/// per-tool skill has id = the tool's `call_tool_cli` name, name = a
/// human-readable rendition, description = a one-line summary from the
/// tool's `#[tool(description=...)]` (truncated).
pub fn agent_skills() -> Vec<AgentSkill> {
    let mut skills = umbrella_skills();
    skills.extend(per_tool_skills());
    skills
}

/// Per-tool skills — one entry per dispatched MCP tool. The id matches
/// `call_tool_cli("<id>", ...)` so A2A clients can request the exact tool.
///
/// Kept in lock-step with `src/mcp/server.rs::call_tool_cli` dispatch entries.
pub fn per_tool_skills() -> Vec<AgentSkill> {
    let names = dispatched_tool_names();
    names
        .into_iter()
        .map(|n| {
            let (tags, specialty, recommended_role) = tag_and_specialty_for(n);
            AgentSkill {
                id: n.to_string(),
                name: human_name(n),
                description: format!(
                    "Dispatched MCP tool. Invoke via A2A by passing `\"skillId\": \"{}\"`.",
                    n
                ),
                tags,
                examples: vec![],
                specialty,
                recommended_role,
            }
        })
        .collect()
}

/// The fixed dispatch table for skills — must stay in sync with the
/// `call_tool_cli` entries in `src/mcp/server.rs`. We don't auto-extract
/// because the rmcp macro doesn't expose its registry at compile time
/// outside the macro scope.
fn dispatched_tool_names() -> Vec<&'static str> {
    vec![
        // Search & retrieval
        "semantic_search",
        "text_search",
        "grep",
        "hybrid_search",
        "search_commits",
        // Patterns
        "software_pattern_search",
        "recommend_design_patterns",
        "review_design_patterns",
        "pattern_search",
        "list_software_patterns",
        "get_software_pattern",
        "pattern_catalog_stats",
        "refresh_pattern_catalog",
        "upsert_pattern_source",
        "pattern_abstraction_candidates",
        // Orientation
        "orient",
        "mandate_context",
        "session_mandates",
        "promote_session_mandate",
        "search_mandates",
        "recall_prompts",
        // Inventory
        "list_projects",
        "project_tree",
        "file_info",
        "index_stats",
        "read_file",
        "reindex",
        // Similarity
        "compare_files",
        "find_similar_modules",
        "find_duplicates",
        "refactoring_report",
        // Topics
        "discover_topics",
        "find_orphans",
        "find_misplaced_code",
        "find_coupled_files",
        "test_coverage_gaps",
        "complexity_hotspots",
        "topic_hierarchy",
        "topic_hierarchy_fcm",
        "suggest_merges",
        "suggest_splits",
        "doc_coverage_gaps",
        // Graph & architecture
        "dependency_graph",
        "centrality_analysis",
        "community_detection",
        "circular_dependencies",
        "change_impact_analysis",
        "coupling_cohesion_report",
        "architecture_violations",
        "design_smell_detection",
        "architecture_quality",
        "design_metrics",
        "boilerplate_clusters",
        "chunk_clusters",
        // Prediction & analysis
        "bug_prediction",
        "technical_debt_analysis",
        "anomaly_detection",
        "code_on_fire",
        "documented_tech_debt",
        "code_summarize",
        "engineering_scorecard",
        "quality_report",
        "mcp_tool_telemetry",
        "internal_dry",
        // Recommendation
        "dependency_health",
        "shotgun_surgery_fix",
        "pr_scope_recommender",
        "naming_consistency",
        "adoption_lag",
        "merge_conflict_risk",
        "hot_path_audit",
        "bus_factor_map",
        "module_growth_trajectory",
        "stale_zombie_detector",
        "tech_debt_burn_down",
        "extraction_candidates",
        "fix_circular_dependency",
        "recommend_module_split",
        "recommend_layering",
        "reviewer_recommender",
        "pgmcp_client_profile",
        // Memory
        "memory_create_entities",
        "memory_add_observations",
        "memory_create_relations",
        "memory_delete_entities",
        "memory_delete_observations",
        "memory_delete_relations",
        "memory_open_nodes",
        "memory_search_nodes",
        "memory_read_graph",
        "memory_neighbors",
        "memory_semantic_search",
        "memory_hybrid_search",
        "memory_path_search",
        "memory_ppr_search",
        "memory_raptor_search",
        "memory_unified_search",
        "memory_anchor_entity",
        "memory_unanchor_entity",
        "memory_find_code_for_entity",
        "memory_find_entities_for_code",
        "memory_relations_traverse",
        "memory_facts_at",
        "memory_reflect",
        "memory_purge_expired",
        "memory_forget",
        // SOTA Phase 2 — graph algorithms
        "kcore_analysis",
        "ktruss_analysis",
        "personalized_pagerank",
        "edge_betweenness",
        "structural_holes",
        "motif_census",
        "attack_vulnerability",
        // SOTA Phase 3 — information theory
        "compression_distance",
        "cochange_mutual_information",
        "import_entropy",
        "identifier_entropy",
        // SOTA Phase 4 — evolution + quality
        "bus_factor",
        "knowledge_silos",
        "ownership_coupling_mismatch",
        "doc_code_drift",
        "test_smells",
        "mutation_score_surrogate",
        "flaky_test_candidates",
        // SOTA Phase 5 — concurrency / safety / performance
        "lockset_races",
        "unsafe_clusters",
        "panic_paths",
        "deadlock_candidates",
        "send_sync_violations",
        "quadratic_loops",
        "missing_preallocation",
        "blocking_in_async",
        "clone_density",
        "io_hotpath",
        // SOTA Phase 6 — security
        "taint_analysis",
        "secret_detection",
        "crypto_misuse",
        "unsafe_deserialization",
        "injection_candidates",
        "unprotected_routes",
        "cve_supply_chain",
        // SOTA Phase 7 — API / contract
        "public_api_surface",
        "semver_break_audit",
        "deprecated_but_used",
        "api_stability",
        // SOTA Phase 8 — ML / embedding-based
        "lsh_clone_detection",
        "semantic_drift",
        "embedding_outliers",
        "multi_resolution_pagerank",
        // SOTA Phase 9 — data engineering
        "migration_safety",
        "dead_columns",
        "pii_spread",
        // SOTA Phase 10 — call-graph downstream
        "dead_code_reachability",
        "feature_envy",
        "shotgun_surgery",
        "lcom4",
        // SOTA Phase 11 — evolution analytics
        "refactor_pressure",
        "commit_changepoint",
        "commit_topic_drift",
        "release_api_stability",
        // A2A (added in this phase)
        "a2a_send_task",
        "a2a_get_task",
        "a2a_subscribe_task",
        "a2a_cancel_task",
        "a2a_register_agent",
        "a2a_list_agents",
        // A2A RecursiveMAS-inspired extensions
        "a2a_find_agents_by_specialty",
        "a2a_pattern_sequential",
        "a2a_pattern_mixture",
        "a2a_pattern_distillation",
        "a2a_pattern_deliberation",
        // RLM recursive decomposition (Part B)
        "a2a_pattern_recursive",
        "trajectory_similarity",
        // A2A best-practice exchange (Part A)
        "a2a_report_outcome",
    ]
}

fn human_name(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    let mut cap_next = true;
    for ch in id.chars() {
        if ch == '_' {
            out.push(' ');
            cap_next = true;
        } else if cap_next {
            out.extend(ch.to_uppercase());
            cap_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn tag_for(id: &str) -> Vec<String> {
    tag_and_specialty_for(id).0
}

/// Map an MCP tool id to `(tags, specialty, recommended_role)`. Specialty
/// is the machine-readable role-routing key; recommended_role is the
/// human-facing collaboration role per RecursiveMAS Table 1.
///
/// Returns empty Vec / None where no role association is meaningful so
/// the resulting AgentSkill omits the field via `skip_serializing_if`.
fn tag_and_specialty_for(id: &str) -> (Vec<String>, Vec<String>, Option<String>) {
    let mut tags: Vec<String> = vec!["mcp-tool".into()];
    let mut specialty: Vec<String> = Vec::new();
    let mut role: Option<String> = None;

    // Memory tools.
    if id.starts_with("memory_") {
        tags.push("memory".into());
        specialty.push("knowledge-graph".into());
        role = Some("Knowledge Curator".into());
    }

    // Search & retrieval.
    if id == "semantic_search"
        || id == "text_search"
        || id == "grep"
        || id == "hybrid_search"
        || id == "search_commits"
    {
        tags.push("search".into());
        specialty.push("search".into());
        specialty.push("retrieval".into());
        role = Some("Search Specialist".into());
    }

    // Topics / clustering.
    if id.contains("topic") || id.contains("cluster") {
        tags.push("topics".into());
        specialty.push("clustering".into());
        specialty.push("topics".into());
        role.get_or_insert_with(|| "Topic Analyst".into());
    }

    // Similarity / comparison.
    if id == "compare_files"
        || id == "find_similar_modules"
        || id == "find_duplicates"
        || id == "find_coupled_files"
    {
        tags.push("similarity".into());
        specialty.push("similarity".into());
        specialty.push("analysis".into());
        role.get_or_insert_with(|| "Similarity Analyst".into());
    }

    // Graph algorithms.
    if id.contains("graph")
        || id.contains("centrality")
        || id.contains("community")
        || id.contains("circular")
        || id.contains("dependency")
    {
        tags.push("graph".into());
        specialty.push("graph".into());
        specialty.push("architecture".into());
        role.get_or_insert_with(|| "Graph Analyst".into());
    }

    // Code health / metrics.
    if id.contains("complexity")
        || id.contains("hotspot")
        || id.contains("technical_debt")
        || id == "documented_tech_debt"
        || id == "code_on_fire"
    {
        tags.push("code-health".into());
        specialty.push("code-health".into());
        specialty.push("metrics".into());
        role.get_or_insert_with(|| "Code Health Analyst".into());
    }

    // Recommendations / refactoring.
    if id.starts_with("recommend_") || id.starts_with("suggest_") {
        tags.push("recommendation".into());
        specialty.push("recommendation".into());
        role.get_or_insert_with(|| "Refactoring Advisor".into());
    }

    // Quality / architecture review.
    if id == "engineering_scorecard"
        || id == "quality_report"
        || id == "architecture_quality"
        || id == "architecture_violations"
        || id == "design_metrics"
        || id == "design_smell_detection"
    {
        tags.push("quality".into());
        specialty.push("quality".into());
        role.get_or_insert_with(|| "Quality Reviewer".into());
    }

    // A2A orchestration / patterns.
    if id.starts_with("a2a_pattern_") {
        tags.push("a2a".into());
        tags.push("orchestration".into());
        specialty.push("orchestration".into());
        role = Some("Orchestrator".into());
    } else if id.starts_with("a2a_") {
        tags.push("a2a".into());
    }

    // Security.
    if id.contains("security")
        || id.contains("taint")
        || id.contains("secret")
        || id.contains("crypto")
        || id.contains("injection")
    {
        tags.push("security".into());
        specialty.push("security".into());
        role.get_or_insert_with(|| "Security Reviewer".into());
    }

    // Deduplicate specialty (each role tag appears at most once).
    specialty.sort();
    specialty.dedup();

    (tags, specialty, role)
}

/// Build the AgentCard for this pgmcp instance.
pub fn build_agent_card(base_url: &str) -> super::types::AgentCard {
    super::types::AgentCard {
        name: "pgmcp".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: "PostgreSQL+pgvector code-indexing agent. 155+ MCP tools spanning search, graph theory, architecture, ML, and security. Speaks both MCP (for tools) and A2A (for agent-to-agent collaboration).".into(),
        url: format!("{}/a2a/jsonrpc", base_url.trim_end_matches('/')),
        provider: super::types::AgentProvider {
            organization: "f1r3fly.io".into(),
        },
        capabilities: super::types::AgentCapabilities {
            streaming: true,
            push_notifications: true,
            state_transition_history: true,
        },
        authentication: super::types::AgentAuthentication {
            schemes: vec!["none".into()],
        },
        default_input_modes: vec!["text".into(), "data".into()],
        default_output_modes: vec!["text".into(), "data".into(), "file".into()],
        skills: agent_skills(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn umbrella_skills_present() {
        let s = umbrella_skills();
        assert_eq!(s.len(), 3);
        let ids: Vec<&str> = s.iter().map(|x| x.id.as_str()).collect();
        assert!(ids.contains(&"code-analysis"));
        assert!(ids.contains(&"documented-tech-debt"));
        assert!(ids.contains(&"cross-project-search"));
    }

    #[test]
    fn agent_card_has_skills() {
        let card = build_agent_card("http://localhost:3100");
        assert!(card.skills.len() > 100);
        assert!(card.skills.iter().any(|s| s.id == "code-analysis"));
        assert!(card.skills.iter().any(|s| s.id == "documented_tech_debt"));
        assert!(card.skills.iter().any(|s| s.id == "a2a_send_task"));
    }

    #[test]
    fn human_name_capitalizes_words() {
        assert_eq!(human_name("documented_tech_debt"), "Documented Tech Debt");
        assert_eq!(human_name("a2a_send_task"), "A2a Send Task");
    }

    #[test]
    fn agent_skill_ids_are_unique() {
        use std::collections::HashSet;
        let s = agent_skills();
        let mut seen: HashSet<&str> = HashSet::new();
        for sk in &s {
            assert!(seen.insert(sk.id.as_str()), "duplicate skill id: {}", sk.id);
        }
    }
}
