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
use crate::fuzzy::values::ConceptValue;
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
    let s = s.trim();
    if s.is_empty() {
        return Err(McpError::invalid_params("facet must not be blank", None));
    }
    Facet::parse(s).ok_or_else(|| {
        McpError::invalid_params(format!("unknown facet `{s}` (see Facet::ALL)"), None)
    })
}

async fn resolve_file_id(pool: &sqlx::PgPool, path: &str) -> Result<Option<i64>, McpError> {
    let path = path.trim();
    if path.is_empty() {
        return Err(McpError::invalid_params("file must be non-empty", None));
    }
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id
         FROM indexed_files
         WHERE relative_path = $1
            OR path = $1
            OR right(path, char_length($1) + 1) = '/' || $1
         ORDER BY
            CASE
              WHEN path = $1 THEN 0
              WHEN relative_path = $1 THEN 1
              ELSE 2
            END,
            id
         LIMIT 2",
    )
    .bind(path)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("file lookup failed: {e}"), None))?;
    match ids.as_slice() {
        [] => Ok(None),
        [id] => Ok(Some(*id)),
        _ => Err(McpError::invalid_params(
            format!(
                "file path '{path}' is ambiguous across indexed files; use an absolute indexed path"
            ),
            None,
        )),
    }
}

fn db_err(e: sqlx::Error) -> McpError {
    McpError::internal_error(format!("ontology query failed: {e}"), None)
}

/// `ontology_tree` — the per-facet `is_a`/`part_of`/`broader` hierarchy (nodes +
/// edges), or — when `root_concept` is given — the bounded **subtree** of
/// descendants under that concept.
pub async fn tool_ontology_tree(
    ctx: &SystemContext,
    params: OntologyTreeParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;

    // Subtree mode: descendants of one concept (bounded depth). Correct over the
    // DAG via a recursive closure (see `queries::concept_descendants`).
    if let Some(root) = params.root_concept.as_deref() {
        let root_id = queries::resolve_concept(pool, root)
            .await
            .map_err(db_err)?
            .ok_or_else(|| McpError::invalid_params(format!("no concept `{root}`"), None))?;
        let depth = params.depth.unwrap_or(5).clamp(1, 50) as i32;
        let edges = queries::concept_descendants(pool, root_id, depth)
            .await
            .map_err(db_err)?;
        return json_result(&json!({
            "root_concept": root,
            "root_id": root_id,
            "depth": depth,
            "edges": edges,
        }));
    }

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
        .ok_or_else(|| {
            McpError::invalid_params(format!("no concept `{}`", params.concept), None)
        })?;
    let meta = queries::get_concept_meta(pool, entity_id)
        .await
        .map_err(db_err)?;
    let evidence = queries::list_concept_evidence(pool, entity_id)
        .await
        .map_err(db_err)?;
    json_result(&json!({ "entity_id": entity_id, "meta": meta, "evidence": evidence }))
}

/// `ontology_search` — typo-tolerant + substring concept-name search.
///
/// Unions and dedups (by `entity_id`) the persistent **concept trie** — a
/// Damerau-Levenshtein fuzzy leg + a prefix leg (the typo/autocomplete
/// accelerator, linear in query length over the mmap'd trie) — with the SQL
/// `ILIKE` substring scan (the always-correct fallback). Trie hits propose only
/// concept *names*; PG resolves them to live rows (`valid_to IS NULL`), so stale
/// or cross-project duplicate trie entries can never produce an incorrect
/// result. Degrades cleanly to ILIKE-only when the trie is cold/unavailable
/// (fresh install before the first `fuzzy-sync`).
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

    // Leg 1 — SQL ILIKE substring (always-correct; the cold-trie fallback).
    let mut rows = queries::search_concepts_by_name(pool, &params.query, facet, limit)
        .await
        .map_err(db_err)?;
    let mut seen: std::collections::HashSet<i64> = rows.iter().map(|r| r.entity_id).collect();

    // Legs 2+3 — the persistent concept trie: fuzzy (typo-tolerant) + prefix.
    // Best-effort: a cold/absent trie simply leaves the ILIKE results in place.
    let mut fuzzy_used = false;
    if let Ok(idx) = crate::fuzzy::sync::open_concept_trie(ctx).await {
        fuzzy_used = true;
        let facet_str = facet.map(|f| f.as_str());
        let pred = |v: &ConceptValue| facet_str.is_none_or(|f| v.facet == f);
        // Tighter edit budget for short queries so fuzzy doesn't over-match.
        let max_distance = if params.query.chars().count() <= 4 {
            1
        } else {
            2
        };

        let mut names: Vec<String> = Vec::new();
        let mut name_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (name, _dist, v) in idx.query(&params.query, max_distance) {
            if pred(&v) && name_seen.insert(name.clone()) {
                names.push(name);
            }
        }
        for (name, v) in idx.prefix(&params.query, 50) {
            if pred(&v) && name_seen.insert(name.clone()) {
                names.push(name);
            }
        }
        // Resolve the trie's candidate names → authoritative live rows; merge
        // whatever the ILIKE leg didn't already surface.
        if !names.is_empty() {
            let resolved = queries::resolve_concepts_by_names(pool, &names, facet, limit)
                .await
                .map_err(db_err)?;
            for r in resolved {
                if seen.insert(r.entity_id) {
                    rows.push(r);
                }
            }
        }
    }

    // Re-rank the merged union (canonical first, then confidence, then id) and cap.
    rows.sort_by(|a, b| {
        (b.status == "canonical")
            .cmp(&(a.status == "canonical"))
            .then(
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.entity_id.cmp(&b.entity_id))
    });
    rows.truncate(limit as usize);

    json_result(&json!({ "query": params.query, "fuzzy": fuzzy_used, "results": rows }))
}

/// `ontology_invariants_for_file` — the anti-mistake query: invariants governing a file.
pub async fn tool_ontology_invariants_for_file(
    ctx: &SystemContext,
    params: OntologyInvariantsForFileParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let file = params.file.trim();
    let Some(file_id) = resolve_file_id(pool, file).await? else {
        return json_result(&json!({ "file": file, "invariants": [], "note": "file not indexed" }));
    };
    let invariants = queries::invariants_for_file(pool, file_id)
        .await
        .map_err(db_err)?;
    json_result(&json!({ "file": file, "file_id": file_id, "invariants": invariants }))
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
    let name = queries::normalize_concept_name(&params.name)
        .map_err(|msg| McpError::invalid_params(format!("invalid concept name: {msg}"), None))?;
    let facet = parse_facet(&params.facet)?;
    let (entity_id, created) = queries::create_concept(pool, &name, facet, Actor::Agent)
        .await
        .map_err(db_err)?;
    let meta = queries::get_concept_meta(pool, entity_id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| {
            McpError::internal_error(
                format!("ontology concept {entity_id} has no metadata after create"),
                None,
            )
        })?;
    json_result(&json!({
        "entity_id": entity_id,
        "created": created,
        "name": name,
        "facet": meta.facet,
        "status": meta.status,
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
            format!(
                "unknown relation `{}` (is_a/part_of/broader/narrower/member_of)",
                params.relation
            ),
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
        .ok_or_else(|| {
            McpError::invalid_params(format!("no concept `{}`", params.concept), None)
        })?;
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
        .ok_or_else(|| {
            McpError::invalid_params(format!("no concept `{}`", params.concept), None)
        })?;
    let ancestors = queries::concept_ancestors(pool, id).await.map_err(db_err)?;
    let anc: Vec<_> = ancestors
        .into_iter()
        .map(|(id, name)| json!({ "id": id, "name": name }))
        .collect();
    json_result(&json!({ "concept": params.concept, "entity_id": id, "is_a_ancestors": anc }))
}
