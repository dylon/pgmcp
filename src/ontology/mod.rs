//! Hierarchical, multi-faceted **ontology** layer.
//!
//! Design: `~/.claude/plans/what-are-the-state-of-the-art-wise-willow.md`.
//!
//! Concepts are `memory_entities` rows (`entity_type='concept'`); the
//! hierarchy/membership edges (`is_a`/`part_of`/`broader`/`narrower`/`member_of`)
//! ride the freeform `memory_relations.relation_type` passthrough already in the
//! unified-graph edge matview — so the ontology inherits bitemporal validity,
//! scoping, RAPTOR, and graph-RAG with **zero** node/edge-matview change. The
//! only new schema is the v23 sidecar set (facet/status/invariant/evidence/attr/
//! data-link/rule), built from this module's closed vocabularies.
//!
//! This `mod.rs` owns those vocabularies; later phases add `classify`, `cluster`,
//! `fca`, `hierarchy`, `mine`, `canonicalize`, `egglog_engine`, `export`, and the
//! trie-accelerator submodules.

pub mod classify;
pub mod cluster;
pub mod edge;
pub mod embed_hyperbolic;
pub mod facet;
pub mod fca;
pub mod hierarchy;
pub mod mine;

#[allow(unused_imports)] // re-exports consumed by queries/tools/crons in later phases
pub use edge::{EvidenceKind, OntologyRelation};
#[allow(unused_imports)]
pub use facet::{ConceptStatus, Facet};
