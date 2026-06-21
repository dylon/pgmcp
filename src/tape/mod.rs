//! The Crucible **context-tape paging control plane** (Phase 5).
//!
//! This module turns a model's fixed context window into a *paged* address
//! space over the indexed corpus. The orchestrator (pi) owns the actual prompt;
//! pgmcp owns the *mechanical residency decision*: given a token budget and an
//! eviction policy, which pages are resident at each trace position. pgmcp never
//! runs a shell or writes the user's files — it reads its own tables and calls
//! the data-plane [`TapeDataPlane`](data_plane::TapeDataPlane) seam.
//!
//! ## The pieces
//!
//! | Module | Role |
//! |--------|------|
//! | [`vocab`] | Closed ADR-003 vocabularies (`PageState`, `EvictionPolicy`, `EvictReason`, `PageKind`). |
//! | [`working_set`] | In-memory resident set ([`WorkingSet`](working_set::WorkingSet)) + logical-clock metadata. |
//! | [`store`] | Pure persistence of the working set to `working_set_pages` / `working_set_config`. |
//! | [`data_plane`] | The fetch/resolve/put/summarize seam + `MockTapeDataPlane`. |
//! | [`engine`] | The [`PagingEngine`](engine::PagingEngine): `page_in` + `evict_to_fit` + the demotion ladder. |
//! | [`prefetch`] | Speculative page-ins with budget headroom only (never evicts a demand page). |
//!
//! ## Phase 3 — the hydration bridge (the REAL data plane)
//!
//! Phase 5 above is the control plane over the `MockTapeDataPlane`. Phase 3 wires
//! the completed [`context_tape`] data-plane crate (its `PageAddress` / `Page` /
//! `TapeStore`) behind the same [`data_plane::TapeDataPlane`] seam:
//!
//! | Module | Role |
//! |--------|------|
//! | [`address_resolve`] | Bridges `PageAddr(String)` ↔ [`context_tape::PageAddress`] (the string IS `PageAddress::to_path()`) + the `node_id` axis. |
//! | [`registry`] | Per-[`TreeId`](context_tape::TreeId) [`context_tape::TapeStore`] registry (held on `SystemContext`), lazily created, with a `drop_tree` finaliser + a TTL-reaper seam. |
//! | [`hydrate`] | The ONLY corpus reader (strictly READ-ONLY): [`context_tape::PageAddress`] → [`context_tape::Page`] from `file_chunks` / `memory_observations`. |
//! | [`real_data_plane`] | [`RealTapeDataPlane`](real_data_plane::RealTapeDataPlane) over the registry + corpus + the embedding path `resolve` deferred. |
//!
//! ## The trust boundary
//!
//! Residency is a deterministic function of the replayed trace — the token
//! budget, the eviction policy, and the monotonic logical clock — and **never**
//! an agent judgment, mirroring the absence of an `Agent` arm in
//! [`crate::tracker::transition`]. The single most important constraint is that
//! `last_access_ord` is a *logical* clock value, so a paused + resumed session
//! reconstructs a bit-identical working set.

pub mod address_resolve;
pub mod data_plane;
pub mod engine;
pub mod hydrate;
pub mod prefetch;
pub mod real_data_plane;
pub mod registry;
pub mod repl_host;
pub mod store;
pub mod vocab;
pub mod working_set;

/// Re-export of the P0-P2 data-plane crate so downstream consumers (and the
/// integration tests) can name [`context_tape::PageAddress`] / [`context_tape::Page`]
/// / [`context_tape::TreeId`] through the bridge module without a separate
/// dependency edge. The bridge ([`address_resolve`], [`hydrate`],
/// [`real_data_plane`]) is the supported surface; this re-export is the type
/// vocabulary it speaks.
///
/// `allow(unused_imports)`: this is a deliberate *facade* re-export — a public
/// API surface for downstream / cross-crate-test consumers. The bin/lib crate's
/// own modules reach `context_tape` directly (via its crate name), so the
/// re-export has no *internal* user and would otherwise trip `unused_imports`
/// under `-D warnings`; the `pub` visibility is the point.
#[allow(unused_imports)]
pub use ::context_tape;
