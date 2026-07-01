//! Migration step 63: re-seed the toolbox catalog after adding the two Wolfram
//! Language cards — `wolfram` (computer-algebra / mathematical modeling, the
//! `formal_verification` domain, beside `sagemath`/`msolve`) and
//! `wolfram-graphics` (scientific plotting / diagramming, the `diagramming`
//! domain, beside `matplotlib`/`gnuplot`/`pgfplots`).
//!
//! ## Why a migration is needed
//!
//! `tool_cards` is seeded lazily by the `toolbox` MCP tool, but only when the
//! table is EMPTY (`ensure_toolbox_seeded_if_empty`). On an already-provisioned
//! install the table is non-empty, so the newly-added `wolfram` /
//! `wolfram-graphics` cards would never be inserted (and the `sagemath` card's
//! widened `alternatives` cross-link — now including `wolfram` — would never be
//! re-hashed) until an operator manually ran `toolbox_refresh`. This step
//! re-runs the idempotent upsert over the FULL bundled seed set so existing
//! installs converge:
//!   - the two new Wolfram cards are inserted (NULL embedding → the embedding
//!     cron, or `toolbox_refresh{mode:reembed}`, backfills their vectors);
//!   - the `sagemath` card re-hashes (its `alternatives` changed), which NULLs
//!     its stale `embedding` so it is re-embedded at the same signature;
//!   - every other card upserts to the same `content_hash` and is left as-is.
//!
//! It does NOT embed inline (the established pattern keeps GPU work off the
//! migration path; the cron backfills NULL embeddings). Idempotent and
//! version-gated by `apply_step`: re-running it upserts to the same hashes and
//! is a no-op after the first application.

use sqlx::PgPool;

use crate::db::tool_cards;
use crate::tools_catalog;

pub(super) const TOOLBOX_WOLFRAM: i32 = 63;
pub(super) const TOOLBOX_WOLFRAM_NAME: &str = "toolbox_wolfram";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Upsert categories first (FK-safe), then every bundled tool card. The
    // upsert is content-hash-driven and NULLs the vector on a hash change, so
    // this is the same convergence path `toolbox_refresh` takes — just run
    // automatically on migration so existing installs pick up the Wolfram cards.
    for category in tools_catalog::tool_category_seeds() {
        tool_cards::upsert_tool_category(pool, &category).await?;
    }
    for seed in tools_catalog::tool_seeds() {
        tool_cards::upsert_tool_card(pool, &seed).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(TOOLBOX_WOLFRAM, 63);
        assert_eq!(TOOLBOX_WOLFRAM_NAME, "toolbox_wolfram");
    }

    /// The two Wolfram cards must be present in the bundled seed set this
    /// migration upserts (guards against a card being dropped from the catalog).
    #[test]
    fn seed_set_includes_wolfram_cards() {
        let slugs: Vec<&str> = tools_catalog::tool_seeds()
            .into_iter()
            .map(|t| t.slug)
            .collect();
        assert!(
            slugs.contains(&"wolfram"),
            "wolfram (computer_algebra) card missing from seed set"
        );
        assert!(
            slugs.contains(&"wolfram-graphics"),
            "wolfram-graphics (scientific_plotting) card missing from seed set"
        );
    }

    /// The FV `wolfram` card and the diagramming `wolfram-graphics` card share
    /// one binary but MUST be distinct slugs (the catalog enforces globally
    /// unique slugs), each landing in its own domain/category.
    #[test]
    fn wolfram_cards_are_distinct_and_correctly_placed() {
        let seeds = tools_catalog::tool_seeds();
        let cas = seeds
            .iter()
            .find(|t| t.slug == "wolfram")
            .expect("wolfram card present");
        let gfx = seeds
            .iter()
            .find(|t| t.slug == "wolfram-graphics")
            .expect("wolfram-graphics card present");
        assert_eq!(cas.category, "computer_algebra");
        assert_eq!(gfx.category, "scientific_plotting");
        assert_ne!(cas.slug, gfx.slug);
    }
}
