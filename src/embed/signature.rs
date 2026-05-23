// C2 lands the cache module ahead of the consumers in C3 / C6 / C8;
// dead-code allow scoped to this file until those commits land.
#![allow(dead_code)]

//! Active-embedding-signature lookup with a short in-process cache.
//!
//! Every read-side query that targets a dual-column table must consult
//! `pgmcp_metadata.active_embedding_signature` to pick the right
//! column (`embedding` for MiniLM-era data, `embedding_v2` for BGE-M3).
//! The cache here serves that lookup with a 30-second TTL plus an
//! explicit `force_refresh()` hook that the cutover CLI calls
//! immediately after flipping the metadata row, so the running daemon
//! observes the change in < 1 s instead of waiting for the TTL.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 5 C2.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use sqlx::PgPool;

use crate::embed::model::{BGE_M3_SIGNATURE, MINILM_SIGNATURE};

/// Canonical enum form of `pgmcp_metadata.active_embedding_signature`.
///
/// Used by C6+ for read-side dispatch and by C3 for the indexer
/// write-side; `#[allow(dead_code)]` on the C2 commit only because
/// the consumers land in subsequent commits within the same Phase 5
/// PR series.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmbeddingSignature {
    /// 384-dim `all-MiniLM-L6-v2` BERT-base. Reads/writes the legacy
    /// `embedding` column.
    MiniLmV1,
    /// 1024-dim BGE-M3 XLM-RoBERTa-Large. Reads/writes the
    /// `embedding_v2` column on dual-column tables and the canonical
    /// `embedding` column on tables that ship 1024d-direct (mandates,
    /// memory_observations, memory_summary_tree).
    BgeM3V1,
}

impl EmbeddingSignature {
    /// Stable string form, exactly matching the value persisted in
    /// `pgmcp_metadata.active_embedding_signature`.
    pub fn as_str(self) -> &'static str {
        match self {
            EmbeddingSignature::MiniLmV1 => MINILM_SIGNATURE,
            EmbeddingSignature::BgeM3V1 => BGE_M3_SIGNATURE,
        }
    }

    /// Output dimensionality of the corresponding embedder.
    pub fn dim(self) -> usize {
        match self {
            EmbeddingSignature::MiniLmV1 => 384,
            EmbeddingSignature::BgeM3V1 => 1024,
        }
    }

    /// SQL column name that should be read by dual-column tables
    /// (`file_chunks`, `session_prompts`, `git_commit_chunks`,
    /// `software_pattern_chunks`) under this signature.
    pub fn read_column(self) -> &'static str {
        match self {
            EmbeddingSignature::MiniLmV1 => "embedding",
            EmbeddingSignature::BgeM3V1 => "embedding_v2",
        }
    }

    /// Human-readable name of the model that produces this signature,
    /// suitable for error messages pointing operators at the CLI.
    pub fn model_name(self) -> &'static str {
        match self {
            EmbeddingSignature::MiniLmV1 => "all-MiniLM-L6-v2",
            EmbeddingSignature::BgeM3V1 => "bge-m3",
        }
    }

    /// Parse the persisted form back into the enum. Returns `None` on
    /// an unknown signature so callers can warn loudly rather than
    /// silently default.
    pub fn from_str_signature(s: &str) -> Option<Self> {
        match s {
            MINILM_SIGNATURE => Some(EmbeddingSignature::MiniLmV1),
            BGE_M3_SIGNATURE => Some(EmbeddingSignature::BgeM3V1),
            _ => None,
        }
    }
}

/// TTL-bounded snapshot of the active signature plus the timestamp at
/// which it was read.
#[derive(Debug, Clone, Copy)]
struct Snapshot {
    signature: EmbeddingSignature,
    read_at: Instant,
}

/// 30-second TTL on the cache. Long enough to avoid hammering
/// `pgmcp_metadata` on every search call; short enough that an
/// operator who forgot to run `pgmcp embed-cutover` and instead
/// `UPDATE`d the metadata row by hand will see the change within
/// half a minute. The explicit `force_refresh()` collapses the
/// window to zero for CLI-driven cutovers.
const TTL: Duration = Duration::from_secs(30);

/// Thread-safe lazy cache for the active embedding signature.
pub struct ActiveSignatureCache {
    snapshot: ArcSwap<Option<Snapshot>>,
}

impl ActiveSignatureCache {
    pub fn new() -> Self {
        Self {
            snapshot: ArcSwap::new(Arc::new(None)),
        }
    }

    /// Resolve the active signature, hitting `pgmcp_metadata` if the
    /// cache is empty or older than [`TTL`]. The DB row's text value
    /// is parsed via [`EmbeddingSignature::from_str_signature`]; an
    /// unknown signature is treated as `MiniLmV1` (the conservative
    /// pre-migration default) and logged at WARN level so the
    /// operator sees the inconsistency without losing service.
    pub async fn current(&self, pool: &PgPool) -> Result<EmbeddingSignature, sqlx::Error> {
        if let Some(snap) = self.snapshot.load_full().as_ref()
            && snap.read_at.elapsed() < TTL
        {
            return Ok(snap.signature);
        }
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT value FROM pgmcp_metadata WHERE key = 'active_embedding_signature'",
        )
        .fetch_optional(pool)
        .await?;
        let signature = raw
            .as_deref()
            .and_then(EmbeddingSignature::from_str_signature)
            .unwrap_or_else(|| {
                tracing::warn!(
                    raw = ?raw,
                    "active_embedding_signature in pgmcp_metadata is unrecognized; \
                     defaulting to MiniLM-L6-v2 for safety. Verify with \
                     `pgmcp embed-cutover --check`."
                );
                EmbeddingSignature::MiniLmV1
            });
        self.snapshot.store(Arc::new(Some(Snapshot {
            signature,
            read_at: Instant::now(),
        })));
        Ok(signature)
    }

    /// Drop the cached snapshot so the next [`current`] call hits the
    /// DB. Called by `pgmcp embed-cutover` (and by the in-process
    /// `promote_to_bge_m3` helper) so a flip is visible to running
    /// readers instantly rather than after the TTL.
    pub fn force_refresh(&self) {
        self.snapshot.store(Arc::new(None));
    }
}

impl Default for ActiveSignatureCache {
    fn default() -> Self {
        Self::new()
    }
}

/// One-shot resolver for callers that don't have access to a cached
/// `ActiveSignatureCache` (currently: the C7 inline-SQL MCP tools that
/// `format!` the column name into an `AVG(c.<col>)::vector(<dim>)`
/// expression). Reads `pgmcp_metadata.active_embedding_signature`
/// directly per call. The 30 s cache TTL is bypassed; this is fine for
/// occasional MCP tool calls (relative cost: <1 ms of metadata SELECT
/// vs the multi-second SQL aggregate that follows).
pub async fn read_active_signature(pool: &PgPool) -> Result<EmbeddingSignature, sqlx::Error> {
    let raw: Option<String> = sqlx::query_scalar(
        "SELECT value FROM pgmcp_metadata WHERE key = 'active_embedding_signature'",
    )
    .fetch_optional(pool)
    .await?;
    Ok(raw
        .as_deref()
        .and_then(EmbeddingSignature::from_str_signature)
        .unwrap_or(EmbeddingSignature::MiniLmV1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_round_trip_str() {
        for sig in [EmbeddingSignature::MiniLmV1, EmbeddingSignature::BgeM3V1] {
            assert_eq!(
                EmbeddingSignature::from_str_signature(sig.as_str()),
                Some(sig)
            );
        }
    }

    #[test]
    fn dim_matches_documented_model_output() {
        assert_eq!(EmbeddingSignature::MiniLmV1.dim(), 384);
        assert_eq!(EmbeddingSignature::BgeM3V1.dim(), 1024);
    }

    #[test]
    fn read_column_is_correct_per_signature() {
        assert_eq!(EmbeddingSignature::MiniLmV1.read_column(), "embedding");
        assert_eq!(EmbeddingSignature::BgeM3V1.read_column(), "embedding_v2");
    }

    #[test]
    fn force_refresh_clears_snapshot() {
        let cache = ActiveSignatureCache::new();
        // Seed manually to avoid needing a PgPool in unit tests.
        cache.snapshot.store(Arc::new(Some(Snapshot {
            signature: EmbeddingSignature::BgeM3V1,
            read_at: Instant::now(),
        })));
        assert!(cache.snapshot.load_full().is_some());
        cache.force_refresh();
        assert!(cache.snapshot.load_full().is_none());
    }

    #[test]
    fn unknown_signature_string_parses_to_none() {
        assert_eq!(EmbeddingSignature::from_str_signature("garbage-v0"), None);
    }
}
