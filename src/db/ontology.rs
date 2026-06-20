//! Single source of truth for the **unified knowledge-graph vocabulary** that
//! `super::migrations::{MEMORY_UNIFIED_NODES_SQL, MEMORY_UNIFIED_EDGES_SQL}`
//! materialize and the `memory_neighbors` / `memory_path_search` traversals (in
//! `super::queries`) walk.
//!
//! - **node_type is CLOSED** — every node arm in the nodes matview has an entry
//!   in [`NODE_TYPES`]; the golden test below enforces parity in both
//!   directions (registry⇄SQL) and asserts arm-count == registry-len.
//! - **edge_type has a CLOSED structural core** ([`EDGE_TYPES_CORE`]) — the
//!   edge_type *literals* the edges matview emits (`in_file`, `parent_of`, …) —
//!   **plus a documented free-form passthrough channel** ([`FREEFORM_EDGE_SOURCES`])
//!   for relation strings sourced from columns (`memory_relations.relation_type`,
//!   `item_relations.relation_type`, `code_graph_edges.edge_type`, the
//!   `*_code_anchor.anchor_type` columns, `work_item_claims.action`). Those are
//!   intentionally open because they belong to other subsystems' vocabularies.

/// Metadata for one node type in `memory_unified_nodes`.
// Registry metadata: `display`/`source_table`/`has_embedding` are consumed by
// the Stage-3 `graph_neighbors` tool (parameter docs + filter validation) and
// the golden test below; not all are read in non-test builds yet.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct NodeTypeMeta {
    /// The `node_type` value and the `node_id` prefix (e.g. `"work_item"` ⇒
    /// node ids `work_item:<pk>`).
    pub key: &'static str,
    /// Human-readable display name.
    pub display: &'static str,
    /// A source table the node arm SELECTs `FROM` (used by the golden test to
    /// confirm the arm exists; the `agent` arm UNIONs several sources and lists
    /// its primary one here).
    pub source_table: &'static str,
    /// Whether the arm carries a non-NULL embedding ⇒ participates in
    /// `memory_unified_search` vector seeding (NULL-embedding nodes are
    /// graph-traversable but not vector-seeded).
    pub has_embedding: bool,
}

/// Metadata for one structural (literal) edge type in `memory_unified_edges`.
// Registry metadata: `display`/`directed` are consumed by the Stage-3
// `graph_neighbors` tool docs; not all are read in non-test builds yet.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct EdgeTypeMeta {
    /// The literal `edge_type` value (e.g. `"in_file"`).
    pub key: &'static str,
    /// Human-readable display name.
    pub display: &'static str,
    /// Whether the edge is conceptually directed. Documents intent; the
    /// traversal CTEs currently walk all edges undirected.
    pub directed: bool,
}

/// CLOSED node-type vocabulary. Adding a node arm to `MEMORY_UNIFIED_NODES_SQL`
/// requires adding an entry here (and vice-versa) — the golden test fails on drift.
pub const NODE_TYPES: &[NodeTypeMeta] = &[
    NodeTypeMeta {
        key: "memory_entity",
        display: "Memory Entity",
        source_table: "memory_entities",
        // v31: entities gained a 1024-d embedding (name + entity_type) so the KB
        // entity hubs are vector-seeded in the unified graph, not graph-only.
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "observation",
        display: "Observation",
        source_table: "memory_observations",
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "chunk",
        display: "Code Chunk",
        source_table: "file_chunks",
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "topic",
        display: "Topic",
        source_table: "code_topics",
        // ADR-029 (e4526ea): topics are vector-seeded by their representative
        // chunk's embedding (`file_chunks.embedding_v2` via `representative_chunk_id`)
        // in `MEMORY_UNIFIED_NODES_SQL`, so topic hubs participate in unified vector
        // search, not graph-only. (The arm was NULL pre-ADR-029; the flag now matches.)
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "durable_mandate",
        display: "Durable Mandate",
        source_table: "durable_mandates",
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "session_mandate",
        display: "Session Mandate",
        source_table: "session_mandates",
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "commit",
        display: "Git Commit",
        source_table: "git_commits",
        has_embedding: false,
    },
    NodeTypeMeta {
        key: "commit_chunk",
        display: "Commit Chunk",
        source_table: "git_commit_chunks",
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "pattern_chunk",
        display: "Pattern Chunk",
        source_table: "software_pattern_chunks",
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "file",
        display: "File",
        source_table: "indexed_files",
        has_embedding: false,
    },
    NodeTypeMeta {
        key: "project",
        display: "Project",
        source_table: "projects",
        has_embedding: false,
    },
    NodeTypeMeta {
        key: "symbol",
        display: "Symbol",
        source_table: "file_symbols",
        has_embedding: false,
    },
    NodeTypeMeta {
        key: "work_item",
        display: "Work Item",
        source_table: "work_items",
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "experiment",
        display: "Experiment",
        source_table: "experiments",
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "agent",
        display: "Agent",
        source_table: "agent_presence",
        has_embedding: false,
    },
    // ADR-009 — CSM/MPST coordination protocols + their per-role projections.
    NodeTypeMeta {
        key: "protocol",
        display: "CSM Protocol",
        source_table: "csm_protocols",
        has_embedding: false,
    },
    NodeTypeMeta {
        key: "protocol_role",
        display: "CSM Protocol Role (G ↾ r)",
        source_table: "csm_projections",
        has_embedding: false,
    },
    // Shadow-ASR semantic layer (v2_shadow_asr): the effect + type-tag
    // vocabularies as graph nodes, reached via `has_effect` / `has_type` edges.
    // No embedding — categorical hubs, graph-traversable but not vector-seeded.
    NodeTypeMeta {
        key: "effect",
        display: "Effect (shadow-ASR)",
        source_table: "effect_catalog",
        has_embedding: false,
    },
    NodeTypeMeta {
        key: "type_tag",
        display: "Type Tag (shadow-ASR)",
        source_table: "type_tag_catalog",
        has_embedding: false,
    },
    // ADR-011 — concurrency resources as graph nodes (derived from `sync_ops`),
    // reached via `acquires` / `sends_on` / `lock_order` edges. No embedding —
    // categorical hubs, graph-traversable but not vector-seeded.
    NodeTypeMeta {
        key: "lock_resource",
        display: "Lock Resource (concurrency)",
        source_table: "sync_ops",
        has_embedding: false,
    },
    NodeTypeMeta {
        key: "channel",
        display: "Channel (concurrency)",
        source_table: "sync_ops",
        has_embedding: false,
    },
    // v31 graph-RAG coverage: A2A conversations, mandate-source prompts, JSON
    // data tables, and worktree-coordination negotiations as first-class nodes.
    // `a2a_task` is a non-embedded HUB (like `commit`), reached via `in_task` /
    // `evidenced_by` edges; the rest carry 1024-d embeddings (cron-backfilled).
    NodeTypeMeta {
        key: "agent_message",
        display: "Agent Message (social mailbox)",
        source_table: "agent_messages",
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "a2a_message",
        display: "A2A Message (task transcript)",
        source_table: "a2a_messages",
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "a2a_task",
        display: "A2A Task (hub)",
        source_table: "a2a_tasks",
        has_embedding: false,
    },
    NodeTypeMeta {
        key: "prompt",
        display: "Session Prompt",
        source_table: "session_prompts",
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "data_table",
        display: "Data Table (JSON)",
        source_table: "data_tables",
        has_embedding: true,
    },
    NodeTypeMeta {
        key: "coordination_request",
        display: "Coordination Request (worktree negotiation)",
        source_table: "coordination_requests",
        has_embedding: true,
    },
];

/// CLOSED structural edge-type core: the edge_type *literals* emitted by
/// `MEMORY_UNIFIED_EDGES_SQL`. Relation strings piped through from columns are
/// NOT here — see [`FREEFORM_EDGE_SOURCES`].
pub const EDGE_TYPES_CORE: &[EdgeTypeMeta] = &[
    EdgeTypeMeta {
        key: "belongs_to",
        display: "Belongs To (chunk→topic)",
        directed: true,
    },
    // Phase 4 — project → its dependency (cross-project, bitemporal). Source:
    // `project_dependencies`; makes the dependency graph a first-class,
    // `as_of`-queryable unified-graph citizen.
    EdgeTypeMeta {
        key: "project_depends_on",
        display: "Depends On (project→project)",
        directed: true,
    },
    // ADR-009 — protocol → its per-role projection.
    EdgeTypeMeta {
        key: "projects_to",
        display: "Projects To (protocol→role)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "in_file",
        display: "In File (chunk→file)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "in_project",
        display: "In Project",
        directed: true,
    },
    EdgeTypeMeta {
        key: "defined_in",
        display: "Defined In (symbol→file)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "parent_of",
        display: "Parent Of (tree)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "similar_to",
        display: "Similar To (cross-project)",
        directed: false,
    },
    EdgeTypeMeta {
        key: "in_commit",
        display: "In Commit (commit_chunk→commit)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "touches",
        display: "Touches (commit→file)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "validated_by",
        display: "Validated By (work_item→experiment)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "evidenced_by",
        display: "Evidenced By (→observation)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "supersedes",
        display: "Supersedes (experiment)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "claimed_by",
        display: "Claimed By (work_item→agent)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "working_on",
        display: "Working On (agent→work_item)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "handoff",
        display: "Handoff (agent→agent)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "evolves_like",
        display: "Evolves Like (MSM trajectory)",
        directed: false,
    },
    EdgeTypeMeta {
        key: "workflow_like",
        display: "Workflow Like (event-sequence)",
        directed: false,
    },
    // Shadow-ASR semantic edges (v2_shadow_asr). `calls` is weighted by
    // resolution_confidence; `has_effect` / `has_type` attach a symbol to its
    // effect / type-tag vocabulary nodes. All gated to the symbol-node set.
    EdgeTypeMeta {
        key: "calls",
        display: "Calls (symbol→symbol, resolved)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "has_effect",
        display: "Has Effect (symbol→effect)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "has_type",
        display: "Has Type (symbol→type_tag)",
        directed: true,
    },
    // ADR-011 — concurrency structure. `acquires` / `sends_on` are timeless
    // (static); `lock_order` is bitemporal (valid_from/valid_to from the
    // cron-materialized lock_order_edges) so it is `as_of`-queryable.
    EdgeTypeMeta {
        key: "acquires",
        display: "Acquires (symbol→lock_resource)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "sends_on",
        display: "Sends On (symbol→channel)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "lock_order",
        display: "Lock Order (lock_resource→lock_resource, bitemporal)",
        directed: true,
    },
    // v31 graph-RAG edges. `in_task` binds a transcript message to its A2A task;
    // `reply_to` threads the social mailbox; `extracted_from` links a mandate to
    // the prompt it was distilled from; `sent` attributes a mailbox message to its
    // author agent; `concerns` connects a coordination negotiation to the work
    // item it blocks and the mailbox message that carries it; `requested`
    // attributes a negotiation to the requesting agent. (a2a_task→observation
    // reuses `evidenced_by`; data_table/coordination→project reuse `in_project`.)
    EdgeTypeMeta {
        key: "in_task",
        display: "In Task (a2a_message→a2a_task)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "reply_to",
        display: "Reply To (agent_message→agent_message)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "extracted_from",
        display: "Extracted From (mandate→prompt)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "sent",
        display: "Sent (agent→agent_message)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "concerns",
        display: "Concerns (coordination_request→work_item / agent_message)",
        directed: true,
    },
    EdgeTypeMeta {
        key: "requested",
        display: "Requested (agent→coordination_request)",
        directed: true,
    },
];

/// Columns whose runtime *values* become `edge_type` and are intentionally
/// free-form (an open passthrough channel, not enumerated in [`EDGE_TYPES_CORE`]).
#[allow(dead_code)] // consumed by the Stage-3 graph_neighbors tool documentation.
pub const FREEFORM_EDGE_SOURCES: &[&str] = &[
    "memory_relations.relation_type",
    "item_relations.relation_type",
    "experiment_relations.relation_type",
    "code_graph_edges.edge_type",
    "memory_code_anchor.anchor_type",
    "work_item_code_anchor.anchor_type",
    "experiment_code_anchor.anchor_type",
    "work_item_claims.action",
];

/// Whether `s` is a registered unified-graph node type. Used by the Stage-3
/// `graph_neighbors` tool to validate caller-supplied `node_type` filters and
/// to populate tool-parameter documentation.
#[allow(dead_code)]
pub fn is_registered_node_type(s: &str) -> bool {
    NODE_TYPES.iter().any(|n| n.key == s)
}

/// Whether `s` is a registered structural (core) edge type.
#[allow(dead_code)]
pub fn is_core_edge_type(s: &str) -> bool {
    EDGE_TYPES_CORE.iter().any(|e| e.key == s)
}

/// Look up a node type's metadata by key.
#[allow(dead_code)]
pub fn node_type(key: &str) -> Option<&'static NodeTypeMeta> {
    NODE_TYPES.iter().find(|n| n.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations::{MEMORY_UNIFIED_EDGES_SQL, MEMORY_UNIFIED_NODES_SQL};
    use regex::Regex;
    use std::collections::HashSet;

    /// Node-id literals look like `'work_item:'` — a lowercase word + colon.
    fn node_id_prefixes(sql: &str) -> HashSet<String> {
        let re = Regex::new(r"'([a-z_]+):'").expect("valid regex");
        re.captures_iter(sql).map(|c| c[1].to_string()).collect()
    }

    #[test]
    fn every_registered_node_type_has_an_arm() {
        for n in NODE_TYPES {
            assert!(
                MEMORY_UNIFIED_NODES_SQL.contains(&format!("'{}:'", n.key)),
                "node type '{}' registered but has no '{}:' node-id prefix in NODES SQL",
                n.key,
                n.key
            );
            assert!(
                MEMORY_UNIFIED_NODES_SQL.contains(&format!("FROM {}", n.source_table)),
                "node type '{}' source_table '{}' not found as a FROM in NODES SQL",
                n.key,
                n.source_table
            );
        }
    }

    #[test]
    fn node_arm_count_matches_registry() {
        // Top-level arms are separated by `UNION ALL` (the agent arm's inner
        // `UNION` de-dup does not count). N arms ⇒ N-1 `UNION ALL`.
        let arms = MEMORY_UNIFIED_NODES_SQL.matches("UNION ALL").count() + 1;
        assert_eq!(
            arms,
            NODE_TYPES.len(),
            "NODES SQL has {} arms but NODE_TYPES has {} entries — registry drift",
            arms,
            NODE_TYPES.len()
        );
    }

    #[test]
    fn nodes_sql_emits_only_registered_node_types() {
        for p in node_id_prefixes(MEMORY_UNIFIED_NODES_SQL) {
            assert!(
                is_registered_node_type(&p),
                "NODES SQL emits unregistered node type '{p}'"
            );
        }
    }

    #[test]
    fn edges_connect_only_registered_node_types() {
        for p in node_id_prefixes(MEMORY_UNIFIED_EDGES_SQL) {
            assert!(
                is_registered_node_type(&p),
                "EDGES SQL references unregistered node type '{p}'"
            );
        }
    }

    #[test]
    fn every_core_edge_type_is_emitted() {
        for e in EDGE_TYPES_CORE {
            assert!(
                MEMORY_UNIFIED_EDGES_SQL.contains(&format!("'{}'", e.key)),
                "structural edge type '{}' registered but not emitted in EDGES SQL",
                e.key
            );
        }
    }

    #[test]
    fn has_embedding_flags_match_sql() {
        // Non-embedding node arms select `NULL::VECTOR(1024)`; embedding arms
        // select a real embedding column. The count of NULL-embedding arms must
        // equal the count of `has_embedding == false` registry entries.
        let null_arms = MEMORY_UNIFIED_NODES_SQL
            .matches("NULL::VECTOR(1024)")
            .count();
        let non_embedding = NODE_TYPES.iter().filter(|n| !n.has_embedding).count();
        assert_eq!(
            null_arms, non_embedding,
            "NODES SQL has {null_arms} NULL-embedding arms but {non_embedding} node types \
             are marked has_embedding=false — registry/SQL drift"
        );
    }

    #[test]
    fn freeform_edge_sources_are_used_in_edges_sql() {
        // Each documented passthrough column must actually feed an edge_type in
        // the EDGES SQL (as a bare identifier in the edge_type position).
        for src in FREEFORM_EDGE_SOURCES {
            let col = src.split('.').next_back().expect("table.column");
            assert!(
                MEMORY_UNIFIED_EDGES_SQL.contains(col),
                "FREEFORM_EDGE_SOURCES lists '{src}' but column '{col}' is not used in EDGES SQL"
            );
        }
    }

    #[test]
    fn registry_keys_are_unique() {
        let nset: HashSet<&str> = NODE_TYPES.iter().map(|n| n.key).collect();
        assert_eq!(nset.len(), NODE_TYPES.len(), "duplicate node-type key");
        let eset: HashSet<&str> = EDGE_TYPES_CORE.iter().map(|e| e.key).collect();
        assert_eq!(eset.len(), EDGE_TYPES_CORE.len(), "duplicate edge-type key");
    }
}
