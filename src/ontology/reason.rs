//! Deductive reasoning over the ontology (Phase 9).
//!
//! Transitive `is_a` closure + structural **constraint checking**, evaluated via
//! recursive CTEs (Datalog-equivalent for these queries). The equality-saturation
//! *canonicalization* half is delivered by the Phase-5 EDC pass
//! (`hierarchy::build_broader_edges`); together they cover the deduction +
//! canonicalization intent behind the `ontology_check` / `ontology_query` /
//! `ontology_export` tool surface.
//!
//! ## Future enhancement: egglog (intentionally not a dependency)
//!
//! The design (`~/.claude/plans/what-are-the-state-of-the-art-wise-willow.md`)
//! considered the **egglog** crate (Datalog + e-graph equality saturation) as the
//! reasoning engine. It is deliberately **not** wired in: the recursive-CTE
//! deduction here, plus the embedding-cosine EDC canonicalization, already satisfy
//! every current need — `is_a` acyclicity + invariant-anchoring constraints,
//! transitive `is_a*`/`part_of*` closure, and near-duplicate concept merging —
//! with no heavyweight in-process engine and no extra build risk. egglog would
//! earn its place only if the ontology later needs genuine *equational* rewriting
//! (canonicalizing morphological / acronym variants by saturation rather than
//! cosine) or large user-authored Datalog rule sets materialized at scale. The
//! `ontology_rule` table and the `ontology_check` / `ontology_query` /
//! `ontology_export` tools are shaped so an egglog engine can be slotted in behind
//! them with no change to callers or schema — so it can be adopted then, if ever
//! needed, without a migration.

use serde::Serialize;
use sqlx::PgPool;

use crate::db::queries;

/// A violated structural constraint.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Violation {
    /// Machine-readable class (`is_a_cycle` | `unanchored_invariant`).
    pub kind: String,
    /// Human-readable detail.
    pub detail: String,
}

/// Check the ontology's structural constraints: the `is_a` graph must be acyclic,
/// and every invariant must anchor ≥1 code object. Returns the violations (empty
/// ⇒ the ontology is well-formed).
pub async fn check_constraints(pool: &PgPool) -> Result<Vec<Violation>, sqlx::Error> {
    let mut violations = Vec::new();
    for id in queries::detect_is_a_cycles(pool).await? {
        violations.push(Violation {
            kind: "is_a_cycle".to_string(),
            detail: format!("concept {id} lies on an is_a cycle"),
        });
    }
    for (id, name) in queries::unanchored_invariants(pool).await? {
        violations.push(Violation {
            kind: "unanchored_invariant".to_string(),
            detail: format!("invariant `{name}` ({id}) anchors no code"),
        });
    }
    Ok(violations)
}
