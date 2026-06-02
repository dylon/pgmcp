//! Deductive reasoning over the ontology (Phase 9).
//!
//! Transitive `is_a` closure + structural **constraint checking**, evaluated via
//! recursive CTEs (Datalog-equivalent for these queries). The equality-saturation
//! *canonicalization* half of the chosen reasoning layer is delivered by the
//! Phase-5 EDC pass (`hierarchy::build_broader_edges`); these two together cover
//! the deduction + canonicalization intent, behind the `ontology_check` /
//! `ontology_query` / `ontology_export` tool surface (a future egglog engine can
//! be swapped in behind the same surface without changing callers).

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
