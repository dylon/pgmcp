//! Multiparty Session Types: the global/local type ASTs, well-formedness, and
//! projection. See ADR-009.

pub mod global;
pub mod local;
pub mod project;
pub mod wellformed;

// Convenience re-exports are added in Phase 2 as the conformance observer and
// the pattern registry consume them; full paths (`mpst::project::project`, …)
// are used until then to keep the surface free of unused-import warnings.
