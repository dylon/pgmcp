//! Optional outbound digest webhook (roadmap Phase 4).
//!
//! A fire-and-forget POST of the composed [`Digest`](super::Digest) to an
//! operator-configured URL. This is the one place pgmcp's local-first posture
//! relaxes, so it is **off by default**: `[digest] webhook_url` is empty unless
//! the operator opts in, and even then the POST fires only on the daemon path
//! (the CLI never reaches it) and only when the digest's
//! [`max_severity`](super::Digest::max_severity) meets `webhook_min_severity`.
//!
//! "Fire-and-forget" means [`post_webhook`] spawns the HTTP request on the Tokio
//! runtime and returns immediately — a slow or unreachable webhook never blocks
//! the observe response. Failures are logged at `warn` and dropped.

use crate::config::DigestConfig;

use super::{Digest, DigestSeverity};

/// Should this digest be POSTed, given the config? True only when a non-empty
/// `webhook_url` is set AND the digest's max severity is at least
/// `webhook_min_severity` (an unpar+seable threshold is treated as `high`).
fn should_post(cfg: &DigestConfig, digest: &Digest) -> bool {
    if cfg.webhook_url.trim().is_empty() {
        return false;
    }
    let Some(max) = digest.max_severity() else {
        return false; // empty digest
    };
    let threshold =
        DigestSeverity::parse(&cfg.webhook_min_severity).unwrap_or(DigestSeverity::High);
    max >= threshold
}

/// Fire-and-forget the digest to `cfg.webhook_url` when [`should_post`] allows.
/// No-op (and no spawn) otherwise. Spawns the request on the current Tokio
/// runtime so the caller (the observe handler) is never blocked on network I/O.
///
/// The body is a JSON object: `{channel, max_severity, item_count,
/// content_sha256, markdown}` — `markdown` is the rendered block (bounded by
/// `cfg.max_bytes`), suitable for a chat/Slack-style incoming webhook.
pub fn post_webhook(cfg: &DigestConfig, channel: super::DigestChannel, digest: &Digest) {
    if !should_post(cfg, digest) {
        return;
    }
    let url = cfg.webhook_url.clone();
    let markdown = digest.render_markdown(cfg.max_bytes);
    let body = serde_json::json!({
        "channel": channel.as_str(),
        "max_severity": digest.max_severity().map(|s| s.as_str()),
        "item_count": digest.items.len(),
        "content_sha256": digest.content_sha256(),
        "markdown": markdown,
    });
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        match client
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!(%url, "digest webhook delivered");
            }
            Ok(resp) => {
                tracing::error!(%url, status = %resp.status(), "digest webhook non-2xx");
            }
            Err(e) => {
                tracing::error!(%url, error = %e, "digest webhook POST failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::{DigestCategory, DigestItem};

    fn cfg_with(url: &str, min: &str) -> DigestConfig {
        DigestConfig {
            webhook_url: url.to_string(),
            webhook_min_severity: min.to_string(),
            ..DigestConfig::default()
        }
    }

    fn digest_of(sev: DigestSeverity) -> Digest {
        Digest {
            items: vec![DigestItem::new(sev, DigestCategory::Health, "x")],
        }
    }

    #[test]
    fn empty_url_never_posts() {
        let cfg = cfg_with("", "info");
        assert!(!should_post(&cfg, &digest_of(DigestSeverity::Critical)));
    }

    #[test]
    fn empty_digest_never_posts() {
        let cfg = cfg_with("https://example.test/hook", "info");
        assert!(!should_post(&cfg, &Digest::default()));
    }

    #[test]
    fn severity_threshold_gates_posting() {
        let cfg = cfg_with("https://example.test/hook", "high");
        // Below threshold → no post.
        assert!(!should_post(&cfg, &digest_of(DigestSeverity::Notice)));
        assert!(!should_post(&cfg, &digest_of(DigestSeverity::Info)));
        // At/above threshold → post.
        assert!(should_post(&cfg, &digest_of(DigestSeverity::High)));
        assert!(should_post(&cfg, &digest_of(DigestSeverity::Critical)));
    }

    #[test]
    fn unparseable_threshold_defaults_to_high() {
        let cfg = cfg_with("https://example.test/hook", "garbage");
        assert!(!should_post(&cfg, &digest_of(DigestSeverity::Notice)));
        assert!(should_post(&cfg, &digest_of(DigestSeverity::High)));
    }
}
