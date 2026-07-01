//! Resilience subsystem — keeps pgmcp serviceable through an external database
//! outage and stops it from contributing to the disk fills that cause one.
//!
//! Three cooperating parts, all reading one shared state bundle hung off
//! [`crate::stats::tracker::StatsTracker`] (already `Arc`-threaded into every
//! DB-using subsystem):
//!
//! - **DB-availability circuit breaker** ([`db_health`] + [`prober`]) — one
//!   background prober flips a lock-free [`DbHealth`]; consumers short-circuit
//!   instead of each eating a 10 s `acquire_timeout`, collapsing a per-operation
//!   error flood (1447 lines for the 2026-06-11 outage) to one line per edge.
//! - **Disk-space watchdog** ([`disk_pressure`] + [`watchdog`] + [`fs`]) —
//!   monitors free **bytes and inodes** and, under pressure, pauses pgmcp's own
//!   disk-growing work and triggers the `target-cleanup` cron out-of-band.
//! - **Ephemeral-event outbox** ([`outbox`]) — store-and-forward for the
//!   fire-and-forget hook ingress (session-observe / client-file-event) that has
//!   no other durable source while the DB is down; replayed on recovery.
//!
//! Full design + incident record: `docs/decisions/015-db-resilience.md`.

pub mod db_health;
pub mod disk_pressure;
pub mod disk_report;
pub mod fs;
pub mod outbox;
pub mod prober;
pub mod watchdog;

pub use db_health::DbHealth;
pub use disk_pressure::DiskPressure;
pub use outbox::{OnFull, Outbox, OutboxReplayer};
