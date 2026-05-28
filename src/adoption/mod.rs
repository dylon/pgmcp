//! Adoption telemetry — measure whether pgmcp's under-used tool families
//! (A2A collaboration, CSM coordination-conformance, the memory server, the
//! recursive language model, and the work-item tracker) are actually being
//! called by connecting agents.
//!
//! This is an *independent collector* (per the project's
//! aggregator-independent-collectors rule): it reads the durable
//! `mcp_tool_calls` telemetry table directly and never touches the working
//! `mcp_tool_telemetry` tool or any analysis tool. Signals that are not present
//! in queryable tables are omitted and documented rather than fabricated.
//!
//! The headline metric is per-family *call share* and *session adoption* broken
//! down by client, restricted to the real-client allowlist so pgmcp's own CLI
//! self-calls (`client_name = "cli"`) and smoke/test rows cannot inflate the
//! numbers. Per-session adoption only becomes meaningful for calls recorded
//! after the `mcp_session_id` telemetry fix (the column was historically empty),
//! so a baseline run shows call counts immediately but session rates that ramp
//! from zero.
//!
//! Phase 3 extends this module with lift (baseline-vs-now) and nudge→adoption
//! conversion; see [`collectors::conversion`].

pub mod collectors;
pub mod report;

pub use collectors::collect;
