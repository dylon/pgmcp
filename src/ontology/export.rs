//! Export the ontology as **Prolog/Datalog facts** or **EDN datoms** so an
//! external reasoner (a local Datomic, a Prolog/Datalog engine) can ingest and
//! query it read-only — interop without coupling pgmcp to those tools (Phase 9).
//!
//! Pure string generation over `(concepts, edges)` loaded by the query layer.

/// A concept tuple: `(entity_id, name, facet, status)`.
pub type ConceptTuple = (i64, String, String, String);
/// An edge tuple: `(from_id, to_id, relation)`.
pub type EdgeTuple = (i64, i64, String);

/// Single-quote-escape a Prolog atom body (`'` → `\'`).
fn pl_atom(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Double-quote-escape an EDN string.
fn edn_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Render the ontology as Prolog/Datalog facts:
/// `concept(Id, Name, Facet, Status).` and `Relation(From, To).` per edge.
pub fn to_prolog(concepts: &[ConceptTuple], edges: &[EdgeTuple]) -> String {
    let mut out = String::with_capacity(concepts.len() * 48 + edges.len() * 24);
    out.push_str("% pgmcp ontology export (Prolog/Datalog facts)\n");
    for (id, name, facet, status) in concepts {
        out.push_str(&format!(
            "concept({id}, {}, {}, {}).\n",
            pl_atom(name),
            pl_atom(facet),
            pl_atom(status)
        ));
    }
    for (from, to, relation) in edges {
        // relation is a closed vocab (is_a/part_of/broader/narrower/member_of) ⇒
        // a safe Prolog predicate name as-is.
        out.push_str(&format!("{relation}({from}, {to}).\n"));
    }
    out
}

/// Render the ontology as EDN (Datomic-style): a map with `:ontology/concepts`
/// (vectors of attribute maps) and `:ontology/edges` (`[from :relation to]`).
pub fn to_edn(concepts: &[ConceptTuple], edges: &[EdgeTuple]) -> String {
    let mut out = String::with_capacity(concepts.len() * 80 + edges.len() * 32);
    out.push_str("{:ontology/concepts [");
    for (id, name, facet, status) in concepts {
        out.push_str(&format!(
            "{{:concept/id {id} :concept/name {} :concept/facet {} :concept/status {}}} ",
            edn_str(name),
            edn_str(facet),
            edn_str(status)
        ));
    }
    out.push_str("]\n :ontology/edges [");
    for (from, to, relation) in edges {
        out.push_str(&format!("[{from} :{relation} {to}] "));
    }
    out.push_str("]}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Vec<ConceptTuple>, Vec<EdgeTuple>) {
        let concepts = vec![
            (1, "Parser".to_string(), "component".to_string(), "canonical".to_string()),
            (2, "Don't Panic".to_string(), "invariant".to_string(), "candidate".to_string()),
        ];
        let edges = vec![(2, 1, "is_a".to_string())];
        (concepts, edges)
    }

    #[test]
    fn prolog_emits_facts_and_escapes_quotes() {
        let (c, e) = fixture();
        let pl = to_prolog(&c, &e);
        assert!(pl.contains("concept(1, 'Parser', 'component', 'canonical')."));
        assert!(pl.contains("is_a(2, 1)."));
        assert!(pl.contains("\\'"), "single quote in an atom must be escaped");
    }

    #[test]
    fn edn_emits_concepts_and_edges() {
        let (c, e) = fixture();
        let edn = to_edn(&c, &e);
        assert!(edn.contains(":concept/id 1"));
        assert!(edn.contains(":concept/name \"Parser\""));
        assert!(edn.contains("[2 :is_a 1]"));
        assert!(edn.starts_with("{:ontology/concepts"));
    }
}
