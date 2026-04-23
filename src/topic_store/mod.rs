//! LMDB-backed persistent topic state for FCM warm-start across daemon restarts.
//!
//! Stores topic centroids and per-chunk assignments so subsequent FCM runs can
//! skip k-means++ cold initialisation and instead seed from the previous run's
//! centroids — typically 3–5× faster convergence.
//!
//! Schema:
//!   `centroids`   : topic_id (u32) → `StoredCentroid { scope, centroid[384], created_at, d, k_total }`
//!   `assignments` : chunk_id (i64) → `Vec<AssignmentEntry { topic_id, membership }>`
//!
//! Both sub-databases serialize via `bincode` for compactness + speed.
//!
//! This module is used by the global topic cron (Phase 7 of the OOM fix plan).
//! A missing or unreadable store is non-fatal — the caller simply falls back
//! to k-means++ cold init and logs a WARN.

#[allow(dead_code)]
pub mod lmdb;

#[allow(unused_imports)]
pub use lmdb::{AssignmentEntry, CentroidStore, StoredCentroid};
