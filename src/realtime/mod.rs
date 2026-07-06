//! Realtime-event producer seam (the write side of `pgmcp_realtime_events`).
//!
//! The v64 table is an append-only, transactional event log the web UI (and any
//! local control-plane consumer) replays by `seq` to drive its live panes. This
//! module is the *only* place domain code should reach for to emit into it:
//!
//! ```text
//!   mutation site ──▶ RealtimeEvent::<topic>_<op>(…)  (a pure value; event.rs)
//!                          │
//!                          ├─ emit_in_tx(&mut tx, &ev)   ← tracker / mandate / trace(root) / task
//!                          │     (commits atomically with the mutation; ins_xid = mutation xid)
//!                          │
//!                          └─ emit(&pool, &ev)           ← cron / index / client / scanner / control / status
//!                                (own tx; best-effort, error!→swallow, never aborts the work)
//! ```
//!
//! Closed vocabularies ([`topic::Topic`], [`op::Op`]) follow the ADR-003 idiom:
//! a Rust enum whose `sql_in_list()` is the single source of truth for the v64
//! CHECK constraints, pinned by golden tests. Callers work through the typed
//! [`event::RealtimeEvent`] builders, so the enums are reached via their own
//! modules rather than re-exported here.

pub mod emit;
pub mod event;
pub mod op;
pub mod topic;

pub use emit::{RealtimeEmitter, emit, emit_in_tx};
pub use event::RealtimeEvent;
