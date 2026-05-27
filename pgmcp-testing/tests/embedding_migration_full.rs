//! Regression tests for the BGE-M3-only signature dispatch
//! (`pgmcp::embed::signature::EmbeddingSignature`).
//!
//! The legacy 384-d MiniLM path was removed: `EmbeddingSignature` now has a
//! single `BgeM3V1` variant. These pure-Rust unit tests pin the one supported
//! signature's invariants (dim 1024, `embedding_v2` column, `bge-m3` model,
//! `bge-m3-v1` string) plus the cache's construction/refresh contract.

use pgmcp::embed::signature::{ActiveSignatureCache, EmbeddingSignature};

#[test]
fn signature_string_round_trips() {
    let sig = EmbeddingSignature::BgeM3V1;
    let parsed = EmbeddingSignature::from_str_signature(sig.as_str());
    assert_eq!(
        parsed,
        Some(sig),
        "round-trip failed for `{}`",
        sig.as_str()
    );
    assert_eq!(
        EmbeddingSignature::BgeM3V1.as_str(),
        "bge-m3-v1",
        "the persisted signature string must be `bge-m3-v1`"
    );
}

#[test]
fn signature_dim_matches_documented_model_output() {
    assert_eq!(
        EmbeddingSignature::BgeM3V1.dim(),
        1024,
        "BGE-M3 is the only supported signature and emits 1024-d vectors"
    );
}

#[test]
fn signature_read_column_dispatches_to_embedding_v2() {
    assert_eq!(
        EmbeddingSignature::BgeM3V1.read_column(),
        "embedding_v2",
        "BGE-M3 reads the 1024-d `embedding_v2` column"
    );
}

#[test]
fn signature_model_name_is_operator_friendly() {
    assert_eq!(EmbeddingSignature::BgeM3V1.model_name(), "bge-m3");
}

#[test]
fn unknown_signature_string_returns_none_not_garbage() {
    assert_eq!(
        EmbeddingSignature::from_str_signature("future-model-v3"),
        None,
        "parser must not silently accept an unknown signature; \
         the daemon's startup probe (C4) relies on `None` to \
         emit a clear warning"
    );
    assert_eq!(
        EmbeddingSignature::from_str_signature(""),
        None,
        "empty string is not a valid signature"
    );
}

#[test]
fn cache_default_constructs_an_empty_cache() {
    let cache = ActiveSignatureCache::default();
    // The cache has no public introspection of "is the cell populated"
    // because that's an implementation detail. We can only assert that
    // force_refresh is a no-op on an empty cache and that the type is
    // Sized/Send/Sync as documented.
    cache.force_refresh();
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ActiveSignatureCache>();
}

#[test]
fn force_refresh_is_idempotent() {
    let cache = ActiveSignatureCache::new();
    cache.force_refresh();
    cache.force_refresh();
    cache.force_refresh();
    // Three consecutive force_refresh calls on an empty cache must
    // not panic. The cache's underlying ArcSwap handles repeated
    // stores cleanly.
}
