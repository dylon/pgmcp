//! Tool bodies for the `ontology_*` MCP family (Phase 6).
//!
//! Thin wrappers over the (oracle-tested) `crate::db::queries::ontology` layer.
//! All return JSON. The agent-authoring tool (`assert_invariant`) routes through
//! `agent_assert_invariant`, which can only ever produce `status='candidate'` —
//! the structural trust boundary (an agent cannot self-canonicalize).

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::{
    OntologyAssertInvariantParams, OntologyCheckParams, OntologyConceptParams,
    OntologyCreateConceptParams, OntologyExportParams, OntologyInvariantsForFileParams,
    OntologyLinkParams, OntologyQueryParams, OntologySearchParams, OntologySuggestEdgesParams,
    OntologyTreeParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::ontology::edge::OntologyRelation;
use crate::ontology::facet::Facet;
use crate::tracker::transition::Actor;

fn parse_facet(s: &str) -> Result<Facet, McpError> {
    Facet::parse(s).ok_or_else(|| {
        McpError::invalid_params(format!("unknown facet `{s}` (see Facet::ALL)"), None)
    })
}

async fn resolve_file_id(pool: &sqlx::PgPool, path: &str) -> Result<Option<i64>, McpError> {
    sqlx::query_scalar(
        "SELECT id FROM indexed_files \
         WHERE relative_path = $1 OR path = $1 OR path LIKE '%/' || $1 \
         ORDER BY id LIMIT 1",
    )
    .bind(path)
    .fetch_optional(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("file lookup failed: {e}"), None))
}

fn db_err(e: sqlx::Error) -> McpError {
    McpError::internal_error(format!("ontology query failed: {e}"), None)
}

/// `ontology_tree` — the per-facet `is_a`/`part_of`/`broader` hierarchy (nodes + edges).
pub async fn tool_ontology_tree(
    ctx: &SystemContext,
    params: OntologyTreeParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let facets: Vec<Facet> = match params.facet.as_deref() {
        Some(s) => vec![parse_facet(s)?],
        None => Facet::ALL.to_vec(),
    };
    let mut out = Vec::with_capacity(facets.len());
    for facet in facets {
        let nodes = queries::list_concepts_by_facet(pool, facet, None, 500)
            .await
            .map_err(db_err)?;
        let edges = queries::concept_hierarchy_edges(pool, facet)
            .await
            .map_err(db_err)?;
        if nodes.is_empty() && edges.is_empty() {
            continue;
        }
        out.push(json!({ "facet": facet.as_str(), "concepts": nodes, "edges": edges }));
    }
    json_result(&json!({ "facets": out }))
}

/// `ontology_concept` — one concept's metadata + evidence.
pub async fn tool_ontology_concept(
    ctx: &SystemContext,
    params: OntologyConceptParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let entity_id = queries::resolve_concept(pool, &params.concept)
        .await
        .map_err(db_err)?
        .ok_or_else(|| McpError::invalid_params(format!("no concept `{}`", params.concept), None))?;
    let meta = queries::get_concept_meta(pool, entity_id).await.map_err(db_err)?;
    let evidence = queries::list_concept_evidence(pool, entity_id)
        .await
        .map_err(db_err)?;
    json_result(&json!({ "entity_id": entity_id, "meta": meta, "evidence": evidence }))
}

/// `ontology_search` — substring search over concept names (optional facet filter).
pub async fn tool_ontology_search(
    ctx: &SystemContext,
    params: OntologySearchParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let facet = match params.facet.as_deref() {
        Some(s) => Some(parse_facet(s)?),
        None => None,
    };
    let limit = params.limit.unwrap_or(30).clamp(1, 200);
    let hits = queries::search_concepts_by_name(pool, &params.query, facet, limit)
        .await
        .map_err(db_err)?;
    json_result(&json!({ "query": params.query, "results": hits }))
}

/// `ontology_invariants_for_file` — the anti-mistake query: invariants governing a file.
pub async fn tool_ontology_invariants_for_file(
    ctx: &SystemContext,
    params: OntologyInvariantsForFileParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let Some(file_id) = resolve_file_id(pool, &params.file).await? else {
        return json_result(&json!({ "file": params.file, "invariants": [], "note": "file not indexed" }));
    };
    let invariants = queries::invariants_for_file(pool, file_id)
        .await
        .map_err(db_err)?;
    json_result(&json!({ "file": params.file, "file_id": file_id, "invariants": invariants }))
}

/// `ontology_assert_invariant` — agent-authored invariant (always `candidate`).
pub async fn tool_ontology_assert_invariant(
    ctx: &SystemContext,
    params: OntologyAssertInvariantParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let file_id = match params.file.as_deref() {
        Some(p) => resolve_file_id(pool, p).await?,
        None => None,
    };
    let rationale = params.rationale.as_deref().unwrap_or("asserted by agent");
    let entity_id = queries::agent_assert_invariant(
        pool,
        &params.name,
        &params.constraint_text,
        rationale,
        file_id,
    )
    .await
    .map_err(db_err)?;
    json_result(&json!({
        "entity_id": entity_id,
        "name": params.name,
        "facet": "invariant",
        "status": "candidate",
        "note": "agent-authored invariants are candidate-only; a human curator promotes to canonical",
    }))
}

/// `ontology_create_concept` — author a concept (agent-sourced, candidate).
pub async fn tool_ontology_create_concept(
    ctx: &SystemContext,
    params: OntologyCreateConceptParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let facet = parse_facet(&params.facet)?;
    let (entity_id, created) = queries::create_concept(pool, &params.name, facet, Actor::Agent)
        .await
        .map_err(db_err)?;
    json_result(&json!({
        "entity_id": entity_id,
        "created": created,
        "name": params.name,
        "facet": facet.as_str(),
        "status": "candidate",
    }))
}

/// `ontology_link` — relate two concepts (is_a/part_of/broader/narrower/member_of).
pub async fn tool_ontology_link(
    ctx: &SystemContext,
    params: OntologyLinkParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let relation = OntologyRelation::parse(&params.relation).ok_or_else(|| {
        McpError::invalid_params(
            format!("unknown relation `{}` (is_a/part_of/broader/narrower/member_of)", params.relation),
            None,
        )
    })?;
    let from = queries::resolve_concept(pool, &params.from)
        .await
        .map_err(db_err)?
        .ok_or_else(|| McpError::invalid_params(format!("no concept `{}`", params.from), None))?;
    let to = queries::resolve_concept(pool, &params.to)
        .await
        .map_err(db_err)?
        .ok_or_else(|| McpError::invalid_params(format!("no concept `{}`", params.to), None))?;
    let inserted = queries::insert_ontology_edge(pool, from, to, relation, 1.0)
        .await
        .map_err(db_err)?;
    json_result(&json!({
        "from": from, "to": to, "relation": relation.as_str(), "inserted": inserted,
    }))
}

/// `ontology_suggest_edges` — Poincaré-predicted candidate hierarchy links
/// (`broader`) touching a concept, for curator review.
pub async fn tool_ontology_suggest_edges(
    ctx: &SystemContext,
    params: OntologySuggestEdgesParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let id = queries::resolve_concept(pool, &params.concept)
        .await
        .map_err(db_err)?
        .ok_or_else(|| McpError::invalid_params(format!("no concept `{}`", params.concept), None))?;
    let limit = params.limit.unwrap_or(20).clamp(1, 100);
    let rows = queries::concept_broader_links(pool, id, limit)
        .await
        .map_err(db_err)?;
    let suggestions: Vec<_> = rows
        .into_iter()
        .map(|(from_id, from, to_id, to, confidence)| {
            json!({
                "from_id": from_id, "from": from, "to_id": to_id, "to": to,
                "relation": "broader", "confidence": confidence,
            })
        })
        .collect();
    json_result(&json!({ "concept": params.concept, "suggestions": suggestions }))
}

/// `ontology_check` — run the structural constraint checks (is_a acyclicity +
/// invariants-must-anchor) and return the violation report.
pub async fn tool_ontology_check(
    ctx: &SystemContext,
    _params: OntologyCheckParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let violations = crate::ontology::reason::check_constraints(pool)
        .await
        .map_err(db_err)?;
    let count = violations.len();
    json_result(&json!({
        "violations": violations,
        "count": count,
        "well_formed": count == 0,
    }))
}

/// `ontology_export` — emit the ontology as Prolog/Datalog facts or EDN datoms
/// for an external reasoner / a local Datomic.
pub async fn tool_ontology_export(
    ctx: &SystemContext,
    params: OntologyExportParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let concepts = queries::export_concepts(pool).await.map_err(db_err)?;
    let edges = queries::export_edges(pool).await.map_err(db_err)?;
    let text = match params.format.as_deref().unwrap_or("prolog") {
        "edn" | "datoms" => crate::ontology::export::to_edn(&concepts, &edges),
        "prolog" | "datalog" => crate::ontology::export::to_prolog(&concepts, &edges),
        other => {
            return Err(McpError::invalid_params(
                format!("unknown export format `{other}` (prolog|edn)"),
                None,
            ));
        }
    };
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

/// `ontology_query` — deductive query: the transitive `is_a` ancestors of a concept.
pub async fn tool_ontology_query(
    ctx: &SystemContext,
    params: OntologyQueryParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let id = queries::resolve_concept(pool, &params.concept)
        .await
        .map_err(db_err)?
        .ok_or_else(|| McpError::invalid_params(format!("no concept `{}`", params.concept), None))?;
    let ancestors = queries::concept_ancestors(pool, id).await.map_err(db_err)?;
    let anc: Vec<_> = ancestors
        .into_iter()
        .map(|(id, name)| json!({ "id": id, "name": name }))
        .collect();
    json_result(&json!({ "concept": params.concept, "entity_id": id, "is_a_ancestors": anc }))
}
