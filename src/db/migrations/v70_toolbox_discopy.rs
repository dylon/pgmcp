//! Migration step 70: re-seed the toolbox catalog after adding the two DisCoPy
//! cards — `discopy` (string-diagram drawing, the `diagramming` domain, beside
//! `tikz`/`pikchr`/`metapost`) and `discopy-categorical` (categorical semantics
//! and ZX-calculus rewriting, the `formal_verification` domain, beside
//! `maude`/`k-framework`/`ott`).
//!
//! ## Why a migration is needed
//!
//! `tool_cards` is seeded lazily by the `toolbox` MCP tool, but only when the
//! table is EMPTY (`ensure_toolbox_seeded_if_empty`). On an already-provisioned
//! install the table is non-empty, so the newly-added DisCoPy cards would never
//! be inserted until an operator manually ran `toolbox_refresh`. This step
//! re-runs the idempotent upsert over the FULL bundled seed set so existing
//! installs converge:
//!   - the two new DisCoPy cards are inserted (NULL embedding → the embedding
//!     cron, or `toolbox_refresh{mode:reembed}`, backfills their vectors);
//!   - every other card upserts to the same `content_hash` and is left as-is.
//!
//! It does NOT embed inline (the established pattern keeps GPU work off the
//! migration path; the cron backfills NULL embeddings). Idempotent and
//! version-gated by `apply_step`: re-running it upserts to the same hashes and
//! is a no-op after the first application.
//!
//! Structurally identical to `v63_toolbox_wolfram.rs`: one installed package
//! (`python-discopy`) surfacing in two domains under two distinct slugs, since
//! `tool_cards.slug` is globally unique.

use sqlx::PgPool;

use crate::db::tool_cards;
use crate::tools_catalog;

pub(super) const TOOLBOX_DISCOPY: i32 = 70;
pub(super) const TOOLBOX_DISCOPY_NAME: &str = "toolbox_discopy";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Upsert categories first (FK-safe), then every bundled tool card. The
    // upsert is content-hash-driven and NULLs the vector on a hash change, so
    // this is the same convergence path `toolbox_refresh` takes — just run
    // automatically on migration so existing installs pick up the DisCoPy cards.
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
        assert_eq!(TOOLBOX_DISCOPY, 70);
        assert_eq!(TOOLBOX_DISCOPY_NAME, "toolbox_discopy");
    }

    /// The two DisCoPy cards must be present in the bundled seed set this
    /// migration upserts (guards against a card being dropped from the catalog).
    #[test]
    fn seed_set_includes_discopy_cards() {
        let slugs: Vec<&str> = tools_catalog::tool_seeds()
            .into_iter()
            .map(|t| t.slug)
            .collect();
        assert!(
            slugs.contains(&"discopy"),
            "discopy (diagram_language) card missing from seed set"
        );
        assert!(
            slugs.contains(&"discopy-categorical"),
            "discopy-categorical (rewriting_semantics) card missing from seed set"
        );
    }

    /// The diagramming `discopy` card and the FV `discopy-categorical` card ship
    /// in one pacman package but MUST be distinct slugs (the catalog enforces
    /// globally unique slugs), each landing in its own domain/category.
    #[test]
    fn discopy_cards_are_distinct_and_correctly_placed() {
        let seeds = tools_catalog::tool_seeds();
        let draw = seeds
            .iter()
            .find(|t| t.slug == "discopy")
            .expect("discopy card present");
        let cat = seeds
            .iter()
            .find(|t| t.slug == "discopy-categorical")
            .expect("discopy-categorical card present");
        assert_eq!(draw.domain, tools_catalog::ToolDomain::Diagramming.as_str());
        assert_eq!(draw.category, "diagram_language");
        assert_eq!(
            cat.domain,
            tools_catalog::ToolDomain::FormalVerification.as_str()
        );
        assert_eq!(cat.category, "rewriting_semantics");
        assert_ne!(draw.slug, cat.slug);
    }

    /// The two cards cross-link each other, so an agent that finds one is told
    /// the other exists (the drawing surface and the compute surface of the
    /// same library).
    #[test]
    fn discopy_cards_cross_link() {
        let seeds = tools_catalog::tool_seeds();
        let draw = seeds
            .iter()
            .find(|t| t.slug == "discopy")
            .expect("discopy card present");
        let cat = seeds
            .iter()
            .find(|t| t.slug == "discopy-categorical")
            .expect("discopy-categorical card present");
        assert!(draw.alternatives.contains(&"discopy-categorical"));
        assert!(cat.alternatives.contains(&"discopy"));
    }
}
