//! Database query functions.
//!
//! This module is a thin facade: every query lives in a per-domain
//! submodule under `queries/`, declared below via `#[path]` and
//! re-exported with `pub use` so that `crate::db::queries::*` resolves
//! exactly as it did before the god-file split (2026-05-29). Adding a
//! new query means appending it to the owning submodule, not here.

#[path = "queries/stats.rs"]
mod queries_stats;
pub use queries_stats::*;

#[path = "queries/experiments.rs"]
mod queries_experiments;
pub use queries_experiments::*;

#[path = "queries/work_items.rs"]
mod queries_work_items;
pub use queries_work_items::*;

#[path = "queries/data_tables.rs"]
mod queries_data_tables;
pub use queries_data_tables::*;

#[path = "queries/dedup.rs"]
mod queries_dedup;
pub(crate) use queries_dedup::*;

#[path = "queries/projects.rs"]
mod queries_projects;
pub use queries_projects::*;

#[path = "queries/clients.rs"]
mod queries_clients;
pub use queries_clients::*;

#[path = "queries/files.rs"]
mod queries_files;
pub use queries_files::*;

#[path = "queries/feedback.rs"]
mod queries_feedback;
pub use queries_feedback::*;

#[path = "queries/votes.rs"]
mod queries_votes;
pub use queries_votes::*;

#[path = "queries/data_table_links.rs"]
mod queries_data_table_links;
pub use queries_data_table_links::*;

#[path = "queries/chunks.rs"]
mod queries_chunks;
pub use queries_chunks::*;

#[path = "queries/search.rs"]
mod queries_search;
pub use queries_search::*;

#[path = "queries/memory_crud.rs"]
mod queries_memory_crud;
pub use queries_memory_crud::*;

#[path = "queries/memory_search.rs"]
mod queries_memory_search;
pub use queries_memory_search::*;

#[path = "queries/git.rs"]
mod queries_git;
pub use queries_git::*;

#[path = "queries/topics.rs"]
mod queries_topics;
pub use queries_topics::*;

#[path = "queries/similarity.rs"]
mod queries_similarity;
pub use queries_similarity::*;

#[path = "queries/metrics.rs"]
mod queries_metrics;
pub use queries_metrics::*;

#[path = "queries/symbols.rs"]
mod queries_symbols;
pub use queries_symbols::*;

#[path = "queries/sync_ops.rs"]
mod queries_sync_ops;
pub use queries_sync_ops::*;

#[path = "queries/concurrency.rs"]
mod queries_concurrency;
pub use queries_concurrency::*;

#[path = "queries/graph.rs"]
mod queries_graph;
pub use queries_graph::*;

#[path = "queries/status.rs"]
mod queries_status;
pub use queries_status::*;

#[path = "queries/advisories.rs"]
mod queries_advisories;
pub use queries_advisories::*;

#[path = "queries/ontology.rs"]
mod queries_ontology;
pub use queries_ontology::*;

#[path = "queries/external_scanner.rs"]
mod queries_external_scanner;
pub use queries_external_scanner::*;

#[path = "queries/cron_history.rs"]
mod queries_cron_history;
pub use queries_cron_history::*;

#[path = "queries/index_failures.rs"]
mod queries_index_failures;
pub use queries_index_failures::*;

// The digest rate-limit ledger keeps its own namespace (`queries::digest::*`)
// rather than being flattened with `pub use`, so the digest's sole write
// surface stays obviously distinct from the read queries. See `src/digest/`.
#[path = "queries/digest.rs"]
pub mod digest;
