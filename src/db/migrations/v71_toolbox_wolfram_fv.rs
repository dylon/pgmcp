//! Migration step 71: re-seed the toolbox catalog after broadening the Wolfram
//! coverage from one CAS card into its full agent-facing surface.
//!
//! Adds three `formal_verification` cards beside the existing `wolfram`
//! (computer algebra) and `wolfram-graphics` (diagramming) cards:
//!
//!   - `wolfram-prover`   (`first_order_atp`)  — `FindEquationalProof` → `ProofObject`
//!   - `wolfram-solver`   (`smt_solver`)       — `Resolve`/`Reduce` (CAD), `SatisfiabilityInstances`
//!   - `wolfram-modeling` (`system_modeling`)  — Modelica `SystemModelSimulate`, `StateSpaceModel`
//!
//! and seeds the new `system_modeling` tool category (FV domain).
//!
//! It also re-hashes two existing cards whose prose changed:
//!   - `wolfram` — its `limitations` claimed "not a machine-checked prover", which
//!     is false: `FindEquationalProof` returns an inspectable `ProofObject`
//!     (`ProofLength`, `ProofDataset`, `ProofGraph`). Corrected, and the 2-seat
//!     license gotcha documented (see below).
//!   - `wolfram-graphics` — same seat gotcha + cross-links to the new cards.
//!
//! ## The seat gotcha (why it is worth carding)
//!
//! `$MaxLicenseProcesses == 2` on this install. Every long-lived
//! `Wolfram/AgentTools` MCP server holds a seat for the lifetime of its agent
//! session, so once two sessions are up, ANY new kernel — `wolfram -script`,
//! `wolframscript`, another MCP server — aborts with `No valid password found.`
//! (rc 85). That message names the wrong cause: the license is valid
//! (`$LicenseType == "Professional"`, no expiration), the seats are simply gone.
//! Both cards now say so, because an agent that trusts the message will waste
//! its time re-activating a perfectly good license.
//!
//! ## Why a migration is needed
//!
//! `tool_cards` is seeded lazily by the `toolbox` MCP tool, but only when the
//! table is EMPTY (`ensure_toolbox_seeded_if_empty`). On an already-provisioned
//! install the table is non-empty, so the new cards would never be inserted and
//! the two corrected cards would never be re-hashed. This step re-runs the
//! idempotent upsert over the FULL bundled seed set so existing installs
//! converge:
//!   - the three new Wolfram cards + the `system_modeling` category are inserted
//!     (NULL embedding → the embedding cron, or `toolbox_refresh{mode:reembed}`,
//!     backfills their vectors);
//!   - `wolfram` and `wolfram-graphics` re-hash (their prose changed), which
//!     NULLs their stale `embedding` so they are re-embedded at the same signature;
//!   - every other card upserts to the same `content_hash` and is left as-is.
//!
//! Same pattern as `v49_toolbox_debug_tools.rs`, `v63_toolbox_wolfram.rs`, and
//! `v70_toolbox_discopy.rs`. Version-gated by `apply_step`; idempotent.

use sqlx::PgPool;

use crate::db::tool_cards;
use crate::tools_catalog;

pub(super) const TOOLBOX_WOLFRAM_FV: i32 = 71;
pub(super) const TOOLBOX_WOLFRAM_FV_NAME: &str = "toolbox_wolfram_fv";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Categories first (FK-safe) — this is what inserts `system_modeling` —
    // then every bundled tool card.
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
        assert_eq!(TOOLBOX_WOLFRAM_FV, 71);
        assert_eq!(TOOLBOX_WOLFRAM_FV_NAME, "toolbox_wolfram_fv");
    }

    /// The `system_modeling` category this migration seeds must exist in the
    /// bundled category set, in the formal_verification domain.
    #[test]
    fn system_modeling_category_is_seeded_under_fv() {
        let cat = tools_catalog::tool_category_seeds()
            .into_iter()
            .find(|c| c.slug == "system_modeling")
            .expect("system_modeling category present");
        assert_eq!(
            cat.domain,
            tools_catalog::ToolDomain::FormalVerification.as_str()
        );
    }

    /// All three new Wolfram FV cards are present, in the formal_verification
    /// domain, each in its intended category.
    #[test]
    fn wolfram_fv_cards_are_present_and_correctly_placed() {
        let seeds = tools_catalog::tool_seeds();
        let fv = tools_catalog::ToolDomain::FormalVerification.as_str();
        for (slug, category) in [
            ("wolfram-prover", "first_order_atp"),
            ("wolfram-solver", "smt_solver"),
            ("wolfram-modeling", "system_modeling"),
        ] {
            let card = seeds
                .iter()
                .find(|t| t.slug == slug)
                .unwrap_or_else(|| panic!("{slug} card missing from seed set"));
            assert_eq!(card.domain, fv, "{slug} must be formal_verification");
            assert_eq!(card.category, category, "{slug} category");
        }
    }

    /// One Wolfram installation, five distinct globally-unique slugs across two
    /// domains. Guards against a future edit collapsing or duplicating them.
    #[test]
    fn wolfram_family_slugs_are_distinct_across_domains() {
        let seeds = tools_catalog::tool_seeds();
        let dia = tools_catalog::ToolDomain::Diagramming.as_str();
        let fv = tools_catalog::ToolDomain::FormalVerification.as_str();
        let domain_of = |slug: &str| {
            seeds
                .iter()
                .find(|t| t.slug == slug)
                .unwrap_or_else(|| panic!("{slug} present"))
                .domain
        };
        assert_eq!(domain_of("wolfram"), fv);
        assert_eq!(domain_of("wolfram-prover"), fv);
        assert_eq!(domain_of("wolfram-solver"), fv);
        assert_eq!(domain_of("wolfram-modeling"), fv);
        assert_eq!(domain_of("wolfram-graphics"), dia);
    }

    /// The `wolfram` card must no longer assert it is "not a machine-checked
    /// prover" — `wolfram-prover` demonstrably returns a `ProofObject`. This
    /// pins the correction so it cannot silently regress.
    #[test]
    fn wolfram_card_no_longer_denies_machine_checked_proof() {
        let seeds = tools_catalog::tool_seeds();
        let wolfram = seeds
            .iter()
            .find(|t| t.slug == "wolfram")
            .expect("wolfram card present");
        assert!(
            !wolfram.limitations.contains("not a machine-checked prover"),
            "the `wolfram` card still denies machine-checked proof, but \
             FindEquationalProof returns an inspectable ProofObject"
        );
    }

    /// The 2-seat license exhaustion masquerades as `No valid password found.`
    /// Both agent-facing Wolfram entrypoints must warn about it, or an agent
    /// will waste its time re-activating a valid license.
    #[test]
    fn seat_exhaustion_gotcha_is_documented_on_both_entrypoints() {
        let seeds = tools_catalog::tool_seeds();
        for slug in ["wolfram", "wolfram-graphics"] {
            let card = seeds
                .iter()
                .find(|t| t.slug == slug)
                .unwrap_or_else(|| panic!("{slug} present"));
            assert!(
                card.limitations.contains("No valid password found"),
                "{slug} must document the seat-exhaustion error message"
            );
            assert!(
                card.limitations.contains("MaxLicenseProcesses"),
                "{slug} must document the 2-process seat cap"
            );
        }
    }
}
