//! Engineering principles — the canonical, cross-agent behavioral mandates.
//!
//! A direct sibling of [`crate::docguidelines`]: the same universal,
//! always-on, cross-agent enforcement surface, but for the user's four
//! *engineering* mandates rather than documentation guidelines. pgmcp is the one
//! component shared by every agent the user runs (Claude Code, Codex, any other
//! MCP client), so these principles live here and are injected through the
//! MCP-protocol channels pgmcp controls for every client — NOT only in
//! `~/.claude/CLAUDE.md`, which Claude Code alone reads.
//!
//! # Where these are surfaced (all fed from [`principle_seeds`])
//!
//! ```text
//!                         src/engprinciples  ◄── CANONICAL (this module)
//!                                 │
//!     ┌──────────────┬───────────┴────────────┬───────────────────┐
//!     ▼              ▼                         ▼                   ▼
//!  MCP instructions  orient output       engineering_         pgmcp://engineering-principles
//!  (every client,    (first-call         principles tool      (MCP resource,
//!   always-on)       reinforce)          (enumerate)          advisory pull)
//! ```
//!
//! - [`render_instructions_banner`] is concatenated, unconditionally, into the
//!   per-client MCP `instructions` string in
//!   `crate::mcp::server::compose_instructions`, so every connecting agent
//!   receives the full list on the `initialize` handshake (the always-on seam).
//! - [`compact_for_orient`] feeds the `engineering_principles` key of the
//!   `orient` tool's envelope (high-traffic reinforcement at task start).
//! - [`principles_json`] backs the `engineering_principles` MCP tool
//!   (`crate::mcp::tools::tool_engineering_principles`) and the
//!   `pgmcp://engineering-principles` resource.
//!
//! # Enforcement ceiling
//!
//! MCP surfaces are *advisory*: they maximize reach (every agent always sees the
//! principles), not compulsion. Two of the four mandates have a complementary
//! mechanical gate — pipe-output/cleanup is steered by the user-scope
//! `~/.claude/hooks/pgmcp-output-capture-enforce.sh` PreToolUse hook, and the
//! boyscout mandate by the `pgmcp bug-gate` step in `scripts/verify.sh`. The
//! other two (full generality, Occam's Razor) are judgment properties with no
//! mechanical oracle; durable re-injection here is the most effective
//! enforcement achievable at the universal pgmcp layer. See ADR-022.
//!
//! # Design
//!
//! At four entries this is deliberately **DB-free** — a static Rust seed list
//! with cheap referential-integrity unit tests, mirroring [`crate::docguidelines`]
//! without migration / embedding / cron machinery. The list is flat (a
//! four-item taxonomy would add nothing). To add or edit a principle, change
//! [`principle_seeds`]; the tests pin the invariants and every consumer
//! re-renders automatically. `text` is stored **verbatim** — the wording is the
//! contract; never reword it.

use serde_json::{Value, json};

/// One engineering principle. `text` is stored verbatim.
#[derive(Debug, Clone, Copy)]
pub struct PrincipleSeed {
    /// kebab-case unique id.
    pub slug: &'static str,
    /// A short human-readable title (banner heading / JSON label).
    pub title: &'static str,
    /// The mandate, verbatim.
    pub text: &'static str,
}

const fn p(slug: &'static str, title: &'static str, text: &'static str) -> PrincipleSeed {
    PrincipleSeed { slug, title, text }
}

/// The canonical four engineering mandates, verbatim, in the user's order.
///
/// Every rendering ([`render_instructions_banner`], [`principles_json`],
/// [`compact_for_orient`]) is derived from this list — exactly one place to edit.
pub fn principle_seeds() -> Vec<PrincipleSeed> {
    vec![
        p(
            "no-overfit-generalize",
            "Full generality (no overfitting)",
            "Never overfit a solution to solve one problem just to regress elsewhere; all \
             solutions must be fully generalized.",
        ),
        p(
            "boyscout-fix-all-bugs",
            "Boy Scout rule (fix every bug)",
            "Always follow the boyscout rule: leave a system in better shape than it was when you \
             started working on it. Fix all the issues you discover whether they were pre-existing \
             or not. No bug, regardless its rarity, is acceptable.",
        ),
        p(
            "pipe-output-cleanup",
            "Capture command output, then clean up",
            "Always pipe command output for validation, compilation, and evaluation tasks to files \
             for follow-up analysis in case you accidentally trigger a hard-to-reproduce bug or \
             need to complete several queries against it. Then, be sure to clean up all temporary \
             files when you are done with them so they do not consume unnecessary space.",
        ),
        p(
            "occam-razor-simplicity",
            "Occam's Razor (simplest, not simpler)",
            "Adhere to Occam's Razor such that changes are kept as simple as possible to accomplish \
             their goals but no simpler. This does not conflict with making fully general changes \
             because that is a requirement of the implementation and therefore aligns with Occam's \
             Razor. By aligning with Occam's Razor, you will not make extraneous changes for the \
             sake of extraneous changes.",
        ),
    ]
}

/// The directive banner injected into EVERY client's MCP `instructions`
/// (always-on, universal). Lists the verbatim mandates under a MUST-follow
/// preamble and points at the `engineering_principles` tool for the structured
/// form. Also reused as the body of the `pgmcp://engineering-principles`
/// resource.
pub fn render_instructions_banner() -> String {
    let seeds = principle_seeds();
    let mut out = String::with_capacity(2048);
    out.push_str(
        "### Engineering principles (pgmcp — apply across ALL agents)\n\n\
         On EVERY engineering task in ANY project you MUST uphold these mandates. They are \
         enforced uniformly for every agent that connects to pgmcp; two are additionally gated \
         (the pipe-output/cleanup hook and the `pgmcp bug-gate` verify step). Call the \
         `engineering_principles` tool for the structured list (slug + title + text).\n",
    );
    for s in &seeds {
        out.push_str("\n**");
        out.push_str(s.title);
        out.push_str("**\n- ");
        out.push_str(s.text);
        out.push('\n');
    }
    out
}

/// Full structured payload for the `engineering_principles` MCP tool and the
/// `pgmcp://engineering-principles` resource: the verbatim list (slug + title +
/// text), a count, and an enforcement note.
pub fn principles_json() -> Value {
    let seeds = principle_seeds();
    let items: Vec<Value> = seeds
        .iter()
        .map(|s| json!({ "slug": s.slug, "title": s.title, "text": s.text }))
        .collect();
    json!({
        "engineering_principles": items,
        "count": seeds.len(),
        "note": "These mandates apply to ALL engineering work you do, in every project and as every \
                 agent. pgmcp injects them into its MCP `instructions` for all clients; they are \
                 also surfaced in `orient` and at the `pgmcp://engineering-principles` resource. \
                 Pipe-output/cleanup and the boyscout rule additionally have mechanical gates \
                 (PreToolUse hook; verify.sh bug-gate).",
    })
}

/// Compact reinforcement embedded in the `orient` envelope. The full text is
/// already always-on in the MCP `instructions`, so here we keep a concise
/// pointer (count + titles + a directive note) to avoid bloating the response.
pub fn compact_for_orient() -> Value {
    let titles: Vec<&'static str> = principle_seeds().iter().map(|s| s.title).collect();
    json!({
        "count": principle_seeds().len(),
        "principles": titles,
        "note": "Uphold pgmcp's engineering principles on every task (full text is in this \
                 server's MCP instructions; call `engineering_principles` to enumerate them).",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn there_are_four_principles() {
        assert_eq!(principle_seeds().len(), 4);
    }

    #[test]
    fn slugs_are_unique() {
        let seeds = principle_seeds();
        let set: HashSet<&str> = seeds.iter().map(|s| s.slug).collect();
        assert_eq!(set.len(), seeds.len(), "duplicate principle slug");
    }

    #[test]
    fn fields_nonempty() {
        for s in principle_seeds() {
            assert!(!s.slug.trim().is_empty(), "empty slug");
            assert!(
                !s.title.trim().is_empty(),
                "empty title for slug {}",
                s.slug
            );
            assert!(!s.text.trim().is_empty(), "empty text for slug {}", s.slug);
        }
    }

    #[test]
    fn banner_contains_every_principle_and_anchors() {
        let banner = render_instructions_banner();
        assert!(banner.contains("MUST uphold"));
        assert!(banner.contains("engineering_principles"));
        for s in principle_seeds() {
            assert!(
                banner.contains(s.text),
                "banner missing principle: {}",
                s.slug
            );
            assert!(banner.contains(s.title), "banner missing title: {}", s.slug);
        }
    }

    #[test]
    fn json_payload_shape_is_complete() {
        let v = principles_json();
        assert_eq!(v["count"].as_u64(), Some(4));
        assert_eq!(
            v["engineering_principles"].as_array().map(Vec::len),
            Some(4)
        );
        assert!(v["note"].is_string());
        for item in v["engineering_principles"].as_array().expect("array") {
            assert!(item["slug"].is_string());
            assert!(item["title"].is_string());
            assert!(item["text"].is_string());
        }
    }

    #[test]
    fn orient_payload_is_a_concise_pointer() {
        let v = compact_for_orient();
        assert_eq!(v["count"].as_u64(), Some(4));
        assert_eq!(v["principles"].as_array().map(Vec::len), Some(4));
        assert!(
            v["note"]
                .as_str()
                .expect("note")
                .contains("engineering_principles")
        );
    }
}
