//! `fca_concept_lattice` (ADR-028, CT-4) — **Formal Concept Analysis** over a
//! real incidence relation drawn from pgmcp's own tables.
//!
//! Two grounded formal contexts `(G, M, I)`, selected by params:
//!
//! | object_kind | attribute_kind | objects G        | attributes M        | incidence I |
//! |-------------|----------------|------------------|---------------------|-------------|
//! | `symbol`    | `effect`       | `file_symbols`   | `effect_catalog`    | `symbol_effects` (shadow-ASR `has_effect`) |
//! | `file`      | `type_tag`     | `indexed_files`  | `type_tag_catalog`  | `file_symbols.return_type_tags` ∪ `symbol_parameters.type_tags` (shadow-ASR `has_type`) |
//!
//! The derivation operators `A↦A'` / `B↦B'` (the Galois connection), the
//! enumeration of **all** formal concepts (Ganter's NextClosure), the Hasse
//! covering lattice on extent inclusion, and the attribute implications all live
//! in the pure [`crate::category::fca`] core; this tool only loads the incidence
//! and serializes the result. The lattice is COMPUTED from real incidence —
//! distinct from the declared ontology `is_a` Hasse cover.
//!
//! Read-only over pgmcp's own tables.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::category::fca::FormalContext;
use crate::context::SystemContext;
use crate::mcp::server::FcaConceptLatticeParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

/// The two supported `(object_kind, attribute_kind)` contexts (ADR-028 CT-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextKind {
    /// objects = functions (`file_symbols`), attributes = effects.
    SymbolEffect,
    /// objects = files (`indexed_files`), attributes = type tags.
    FileTypeTag,
}

pub async fn tool_fca_concept_lattice(
    ctx: &SystemContext,
    params: FcaConceptLatticeParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let object_kind = params.object_kind.as_deref().unwrap_or("symbol");
    let attribute_kind = params.attribute_kind.as_deref().unwrap_or("effect");
    let kind = match (object_kind, attribute_kind) {
        ("symbol", "effect") => ContextKind::SymbolEffect,
        ("file", "type_tag") => ContextKind::FileTypeTag,
        (o, a) => {
            return Err(McpError::invalid_params(
                format!(
                    "unsupported context (object_kind='{o}', attribute_kind='{a}'); supported: \
                     (symbol, effect) and (file, type_tag)"
                ),
                None,
            ));
        }
    };

    let max_concepts = params.max_concepts.unwrap_or(200).clamp(1, 100_000) as usize;
    let extent_sample = params.extent_sample.unwrap_or(8).clamp(0, 1000) as usize;

    // Optional project scope (resolved to an id, fails closed on ambiguity).
    let project_id: Option<i32> = match params.project.as_deref() {
        Some(p) if !p.trim().is_empty() => Some(project_id_or_err(ctx, p).await?),
        _ => None,
    };

    // Load the incidence as (object_label, attribute_label) pairs from the real
    // tables. Only objects that carry ≥1 attribute enter the context — an
    // attribute-less object would only inflate the universal (top) extent and can
    // never create a new concept (standard object reduction). This is reported in
    // the output note so the count is unambiguous.
    let pairs: Vec<(String, String)> = match kind {
        ContextKind::SymbolEffect => load_symbol_effect(pool, project_id).await?,
        ContextKind::FileTypeTag => load_file_type_tag(pool, project_id).await?,
    };

    // Dense-index objects and attributes (sorted for determinism).
    let mut obj_index: BTreeMap<String, usize> = BTreeMap::new();
    let mut attr_index: BTreeMap<String, usize> = BTreeMap::new();
    for (o, a) in &pairs {
        let next_o = obj_index.len();
        obj_index.entry(o.clone()).or_insert(next_o);
        let next_a = attr_index.len();
        attr_index.entry(a.clone()).or_insert(next_a);
    }
    // BTreeMap insertion order ≠ sorted index order; rebuild as sorted label
    // vectors with a label→index map so indices are stable & sorted.
    let objects: Vec<String> = obj_index.keys().cloned().collect();
    let attributes: Vec<String> = attr_index.keys().cloned().collect();
    let obj_of: BTreeMap<&str, usize> = objects
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();
    let attr_of: BTreeMap<&str, usize> = attributes
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();
    let incidence: Vec<(usize, usize)> = pairs
        .iter()
        .filter_map(|(o, a)| Some((*obj_of.get(o.as_str())?, *attr_of.get(a.as_str())?)))
        .collect();

    let n_objects = objects.len();
    let n_attributes = attributes.len();
    let attributes_out = attributes.clone();
    let context = FormalContext::new(objects, attributes, incidence);

    // Enumerate all concepts (NextClosure), bounded by max_concepts.
    let (concepts, truncated) = context.concepts(max_concepts);
    if truncated {
        // ADR-022: no silent caps — name the cap and the context.
        tracing::error!(
            object_kind,
            attribute_kind,
            max_concepts,
            n_objects,
            n_attributes,
            enumerated = concepts.len(),
            "fca_concept_lattice: concept enumeration TRUNCATED at max_concepts; \
             the lattice is incomplete — raise max_concepts for the full set"
        );
    }

    let lattice_edges = FormalContext::covers(&concepts);
    let implications = context.implications(&concepts, &lattice_edges);

    // Shape the concepts for output (id = index in the enumerated order).
    let concepts_out: Vec<serde_json::Value> = concepts
        .iter()
        .enumerate()
        .map(|(id, c)| {
            json!({
                "id": id,
                "extent_size": c.extent.count(),
                "intent": context.label_attrs(&c.intent),
                "extent_sample": context.sample_objects(&c.extent, extent_sample),
            })
        })
        .collect();
    let edges_out: Vec<[usize; 2]> = lattice_edges.iter().map(|&(c, p)| [c, p]).collect();

    json_result(&json!({
        "context": {
            "object_kind": object_kind,
            "attribute_kind": attribute_kind,
            "n_objects": n_objects,
            "n_attributes": n_attributes,
            "attributes": attributes_out,
        },
        "concepts": concepts_out,
        "n_concepts": concepts.len(),
        "lattice_edges": edges_out,
        "implications": implications,
        "truncated": truncated,
        "note": "Formal Concept Analysis over a REAL incidence relation (CT-4). A concept (extent, \
    intent) satisfies extent'=intent and intent'=extent under the Galois connection A↦A' (attributes \
    common to all of A) / B↦B' (objects sharing all of B). All concepts are enumerated by Ganter's \
    NextClosure; lattice_edges is the Hasse cover on extent inclusion; implications are extent-drop \
    attribute dependencies (premise⟹conclusion, support=|premise'|). Objects with no attributes are \
    excluded (they only inflate the top extent). This lattice is computed from incidence — distinct \
    from the declared ontology is_a cover.",
        "guidance": if n_objects == 0 {
            Some("empty context — no incidence found; check the project scope or run the \
                  symbol-extraction cron (shadow-ASR effects / type tags)")
        } else { None },
    }))
}

/// Incidence for the `(symbol, effect)` context: each function-bearing symbol
/// that carries ≥1 effect, paired with its effects. Object label = `name @ path`
/// (disambiguates same-named symbols across files). Scoped to a project when
/// `project_id` is set.
async fn load_symbol_effect(
    pool: &sqlx::PgPool,
    project_id: Option<i32>,
) -> Result<Vec<(String, String)>, McpError> {
    // file_symbols.kind covers the function-like symbols; restrict to those so
    // the objects are "functions" per the ADR. Join effects via symbol_effects.
    let sql = "SELECT s.name || ' @ ' || f.relative_path AS obj, se.effect AS attr
                 FROM symbol_effects se
                 JOIN file_symbols s ON s.id = se.symbol_id
                 JOIN indexed_files f ON f.id = s.file_id
                WHERE s.kind IN ('function','method','fn','func','constructor','closure')
                  AND ($1::int IS NULL OR f.project_id = $1)
                ORDER BY obj, attr";
    sqlx::query_as::<_, (String, String)>(sql)
        .bind(project_id)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("symbol/effect incidence failed: {e}"), None))
}

/// Incidence for the `(file, type_tag)` context: each file paired with the
/// distinct type tags appearing on its symbols' return types and parameters
/// (the shadow-ASR `has_type` relation, lifted to file granularity). Object
/// label = `relative_path`. Scoped to a project when `project_id` is set.
async fn load_file_type_tag(
    pool: &sqlx::PgPool,
    project_id: Option<i32>,
) -> Result<Vec<(String, String)>, McpError> {
    // Union return-type tags and parameter tags, lifted to the owning file.
    let sql = "SELECT DISTINCT f.relative_path AS obj, tag AS attr
                 FROM (
                     SELECT s.file_id, t.tag
                       FROM file_symbols s
                       CROSS JOIN LATERAL unnest(s.return_type_tags) AS t(tag)
                     UNION
                     SELECT s.file_id, t.tag
                       FROM symbol_parameters sp
                       JOIN file_symbols s ON s.id = sp.symbol_id
                       CROSS JOIN LATERAL unnest(sp.type_tags) AS t(tag)
                 ) tagged
                 JOIN indexed_files f ON f.id = tagged.file_id
                WHERE ($1::int IS NULL OR f.project_id = $1)
                ORDER BY obj, attr";
    sqlx::query_as::<_, (String, String)>(sql)
        .bind(project_id)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("file/type_tag incidence failed: {e}"), None))
}
