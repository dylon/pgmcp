//! Phase 4 — `maybe_emit` dedup + rate-limit gate.
//!
//! Asserts the nudge-gate idiom applied to digests:
//!
//! 1. The first emission of a digest succeeds (`true`) and records a row.
//! 2. An identical digest (same `content_sha256`) to the same session within
//!    the TTL is suppressed (`false`).
//! 3. A *distinct* digest (different content ⇒ different sha) to the same
//!    session emits again (`true`).
//! 4. The per-session cap suppresses further emissions once reached.
//!
//! Self-skips (via `require_test_db!`) when no test DB is configured.

use pgmcp::config::DigestConfig;
use pgmcp::digest::{
    Digest, DigestCategory, DigestChannel, DigestItem, DigestSeverity, compose_digest, maybe_emit,
};
use pgmcp_testing::require_test_db;

fn digest_with(text: &str) -> Digest {
    Digest {
        items: vec![DigestItem {
            severity: DigestSeverity::High,
            category: DigestCategory::Health,
            text: text.to_string(),
        }],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn maybe_emit_dedups_within_ttl_then_admits_distinct() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    let cfg = DigestConfig {
        enabled: true,
        ttl_secs: 3600,       // wide window: a re-emit of the same sha must dedup
        max_per_session: 100, // not the constraint under test here
        ..DigestConfig::default()
    };
    let session = "rate-limit-session-A";

    let d1 = digest_with("backlog is large (12345 chunks unembedded)");

    // (1) first emission succeeds.
    let first = maybe_emit(&pool, session, DigestChannel::Prompt, None, &cfg, &d1).await;
    assert!(first, "first emission of a digest must succeed");

    // (2) identical content within TTL is suppressed.
    let dup = maybe_emit(&pool, session, DigestChannel::Prompt, None, &cfg, &d1).await;
    assert!(
        !dup,
        "identical digest within TTL must be deduped (suppressed)"
    );

    // sanity: the sha really is identical for the same content.
    assert_eq!(
        d1.content_sha256(),
        digest_with("backlog is large (12345 chunks unembedded)").content_sha256()
    );

    // (3) a distinct digest (different text ⇒ different sha) admits again.
    let d2 = digest_with("a different health signal");
    assert_ne!(
        d1.content_sha256(),
        d2.content_sha256(),
        "distinct content ⇒ distinct sha"
    );
    let distinct = maybe_emit(&pool, session, DigestChannel::Prompt, None, &cfg, &d2).await;
    assert!(
        distinct,
        "a distinct digest must emit even within the TTL of another"
    );

    // An empty digest never emits.
    let empty = maybe_emit(
        &pool,
        session,
        DigestChannel::Prompt,
        None,
        &cfg,
        &Digest::default(),
    )
    .await;
    assert!(!empty, "empty digest must not emit");
}

#[tokio::test(flavor = "multi_thread")]
async fn maybe_emit_respects_the_per_session_cap() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    let cfg = DigestConfig {
        enabled: true,
        ttl_secs: 1, // narrow window so dedup is not what stops us — the cap is
        max_per_session: 2,
        ..DigestConfig::default()
    };
    let session = "rate-limit-session-cap";

    // Two distinct digests fill the cap.
    let a = maybe_emit(
        &pool,
        session,
        DigestChannel::Prompt,
        None,
        &cfg,
        &digest_with("one"),
    )
    .await;
    let b = maybe_emit(
        &pool,
        session,
        DigestChannel::Prompt,
        None,
        &cfg,
        &digest_with("two"),
    )
    .await;
    assert!(
        a && b,
        "first two distinct emissions fit under the cap of 2"
    );

    // A third distinct digest is over the cap → suppressed.
    let c = maybe_emit(
        &pool,
        session,
        DigestChannel::Prompt,
        None,
        &cfg,
        &digest_with("three"),
    )
    .await;
    assert!(
        !c,
        "third emission exceeds max_per_session=2 and is suppressed"
    );

    // A different session is unaffected by the first session's cap.
    let other = maybe_emit(
        &pool,
        "other-session",
        DigestChannel::Prompt,
        None,
        &cfg,
        &digest_with("one"),
    )
    .await;
    assert!(other, "the per-session cap is scoped per session_id");
}

/// `compose_digest` over an empty DB yields an empty digest, and `maybe_emit`
/// suppresses it — the proactive surface stays silent when there's nothing to
/// say.
#[tokio::test(flavor = "multi_thread")]
async fn empty_db_composes_empty_and_emits_nothing() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let cfg = DigestConfig {
        enabled: true,
        ..DigestConfig::default()
    };
    let digest = compose_digest(&pool, None, None, &cfg).await;
    // No projects, no tracker rows, no quality history ⇒ nothing to surface.
    assert!(digest.is_empty(), "empty DB ⇒ empty digest: {digest:?}");
    let emitted = maybe_emit(
        &pool,
        "empty-db-session",
        DigestChannel::SessionStart,
        None,
        &cfg,
        &digest,
    )
    .await;
    assert!(!emitted, "an empty digest must never emit");
}
