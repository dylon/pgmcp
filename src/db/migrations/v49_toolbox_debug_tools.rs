//! Migration step 49: re-seed the toolbox catalog after the Dbg-2 debugging
//! additions (the `rr` time-travel debugger card, the BCC `wakeuptime` waker-
//! attribution card, GDB's reverse-execution enrichment) and the
//! `DEV_TOOL_EMBEDDING_SIGNATURE` bump (`…-v1` → `…-v2`).
//!
//! ## Why a migration is needed
//!
//! `tool_cards` is seeded lazily by the `toolbox` MCP tool, but only when the
//! table is EMPTY (`ensure_toolbox_seeded_if_empty`). On an already-provisioned
//! install the table is non-empty, so newly-added bundled cards (`rr`,
//! `wakeuptime`) would never be inserted, and the GDB prose edit would never be
//! re-hashed, until an operator manually ran `toolbox_refresh`. This step
//! re-runs the idempotent upsert over the FULL bundled seed set so existing
//! installs converge:
//!   - new cards are inserted;
//!   - edited cards re-hash (the signature bump changes every card's
//!     `content_hash`), which NULLs the stale `embedding` so the
//!     embedding-migration cron re-embeds them at the new signature;
//!   - unchanged-prose cards still re-hash once (signature-driven) and re-embed
//!     to the same vector — a one-time cost that keeps the signature consistent.
//!
//! It does NOT embed inline (the established 1024d-direct pattern keeps GPU work
//! off the migration path; the cron backfills NULL embeddings). Idempotent and
//! version-gated by `apply_step`: re-running it upserts to the same hashes and
//! is a no-op after the first application.

use sqlx::PgPool;

use crate::db::tool_cards;
use crate::tools_catalog;

pub(super) const TOOLBOX_DEBUG_TOOLS: i32 = 49;
pub(super) const TOOLBOX_DEBUG_TOOLS_NAME: &str = "toolbox_debug_tools";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Upsert categories first (FK-safe), then every bundled tool card. The
    // upsert is content-hash-driven and NULLs the vector on a hash change, so
    // this is the same convergence path `toolbox_refresh` takes — just run
    // automatically on migration so existing installs pick up the new cards.
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
        assert_eq!(TOOLBOX_DEBUG_TOOLS, 49);
        assert_eq!(TOOLBOX_DEBUG_TOOLS_NAME, "toolbox_debug_tools");
    }

    /// The new debugging cards must be present in the bundled seed set this
    /// migration upserts (guards against a card being dropped from the catalog).
    #[test]
    fn seed_set_includes_new_debug_cards() {
        let slugs: Vec<&str> = tools_catalog::tool_seeds()
            .into_iter()
            .map(|t| t.slug)
            .collect();
        assert!(slugs.contains(&"rr"), "rr card missing from seed set");
        assert!(
            slugs.contains(&"wakeuptime"),
            "wakeuptime card missing from seed set"
        );
        assert!(slugs.contains(&"gdb"), "gdb card missing from seed set");
    }

    /// The catalog embedding signature must be the bumped v2 value — this is
    /// what forces existing rows to re-hash + re-embed through the upsert.
    #[test]
    fn embedding_signature_is_v2() {
        assert_eq!(
            tool_cards::DEV_TOOL_EMBEDDING_SIGNATURE,
            "pgmcp-tool-embedding-v2"
        );
    }
}
