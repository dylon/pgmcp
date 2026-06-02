//! Parameter structs for the `ontology_*` MCP tools (Phase 6).

use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OntologyTreeParams {
    /// Facet to show (e.g. "invariant", "concurrency"); omit for all facets.
    #[serde(default)]
    pub facet: Option<String>,
    /// Subtree mode: a concept (name or id) whose hierarchy *descendants* to
    /// return (more-specific concepts reachable via is_a/part_of/broader). When
    /// set, `facet` is ignored and the response is the bounded subtree.
    #[serde(default)]
    pub root_concept: Option<String>,
    /// Max hops for `root_concept` subtree traversal (default 5, clamped 1..=50).
    #[serde(default)]
    pub depth: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OntologyConceptParams {
    /// Concept name or numeric entity id.
    pub concept: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OntologySearchParams {
    /// Substring to match against concept names.
    pub query: String,
    /// Optional facet filter.
    #[serde(default)]
    pub facet: Option<String>,
    /// Max results (default 30).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OntologyInvariantsForFileParams {
    /// File path (relative or absolute) to surface governing invariants for.
    pub file: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OntologyAssertInvariantParams {
    /// Short name for the invariant concept.
    pub name: String,
    /// The constraint sentence (the rule).
    pub constraint_text: String,
    /// Why it holds (optional).
    #[serde(default)]
    pub rationale: Option<String>,
    /// Optional file the invariant governs (anchors it for surfacing).
    #[serde(default)]
    pub file: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OntologyCreateConceptParams {
    /// Concept name.
    pub name: String,
    /// Facet (e.g. "tool", "system", "collection", "algorithm", ...).
    pub facet: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OntologyLinkParams {
    /// Source concept (name or id).
    pub from: String,
    /// Target concept (name or id).
    pub to: String,
    /// Relation: is_a | part_of | broader | narrower | member_of.
    pub relation: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OntologySuggestEdgesParams {
    /// Concept (name or id) to list candidate (`broader`) hierarchy links for.
    pub concept: String,
    /// Max suggestions (default 20).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct OntologyCheckParams {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OntologyExportParams {
    /// Export format: "prolog" (Prolog/Datalog facts, default) or "edn" (Datomic-style datoms).
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OntologyQueryParams {
    /// Concept (name or id) to compute transitive is_a ancestors for.
    pub concept: String,
}
