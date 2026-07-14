//! Documentation-authoring guidelines — the canonical, single source of truth.
//!
//! This module holds the curated set of guidelines the user requires for ALL
//! documentation produced in ANY project. It is the cross-agent enforcement
//! surface: pgmcp is the one component shared by every agent the user runs
//! (Claude Code, Codex, and any other MCP client), so the guidelines live here
//! and are injected through the MCP-protocol channels pgmcp controls for every
//! client — NOT in `~/.claude/CLAUDE.md`, which only Claude Code reads.
//!
//! # Where these are surfaced (all fed from [`guideline_seeds`])
//!
//! ```text
//!                         src/docguidelines  ◄── CANONICAL (this module)
//!                                 │
//!     ┌──────────────┬───────────┴────────────┬───────────────────┐
//!     ▼              ▼                         ▼                   ▼
//!  MCP instructions  orient output       documentation_      pgmcp://guidelines
//!  (every client,    (first-call         guidelines tool     (MCP resource,
//!   always-on)       reinforce)          (enumerate)          advisory pull)
//! ```
//!
//! - [`render_instructions_banner`] is concatenated, unconditionally, into the
//!   per-client MCP `instructions` string in `crate::mcp::server::compose_instructions`,
//!   so every connecting agent receives the full list on the `initialize`
//!   handshake (the always-on, universal seam).
//! - [`compact_for_orient`] feeds the `documentation_guidelines` key of the
//!   `orient` tool's envelope (high-traffic reinforcement at task start).
//! - [`guidelines_json`] backs the `documentation_guidelines` MCP tool
//!   (`crate::mcp::tools::tool_documentation_guidelines`) and the
//!   `pgmcp://guidelines` resource.
//!
//! # Enforcement ceiling
//!
//! MCP surfaces are *advisory*: they maximize reach (every agent always sees the
//! guidelines), not compulsion. pgmcp never observes an agent's file writes, so
//! it cannot hard-gate documentation quality. This module is the most effective
//! enforcement achievable at the universal pgmcp layer.
//!
//! # Design
//!
//! At 26 entries this is deliberately **DB-free** — a static Rust seed list with
//! cheap referential-integrity unit tests, mirroring the spirit of
//! `src/patterns/` and `src/tools_catalog/` without their migration / embedding /
//! cron machinery. To add or edit a guideline, change [`guideline_seeds`] (and,
//! if it adds a new axis, [`GuidelineCategory`]); the tests below pin the
//! invariants and every consumer re-renders automatically.

use serde_json::{Value, json};

/// The closed taxonomy axis a guideline belongs to. Used to group the rendered
/// banner and to expose a stable category vocabulary to tool/resource callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuidelineCategory {
    Placement,
    Coverage,
    Pedagogy,
    Diagrams,
    MathNotation,
    Citations,
    AlgorithmsCode,
}

impl GuidelineCategory {
    /// All categories, in canonical reading order (drives banner grouping).
    pub const ALL: [GuidelineCategory; 7] = [
        GuidelineCategory::Placement,
        GuidelineCategory::Coverage,
        GuidelineCategory::Pedagogy,
        GuidelineCategory::Diagrams,
        GuidelineCategory::MathNotation,
        GuidelineCategory::Citations,
        GuidelineCategory::AlgorithmsCode,
    ];

    /// Stable machine slug (used in JSON output).
    pub const fn as_str(self) -> &'static str {
        match self {
            GuidelineCategory::Placement => "placement",
            GuidelineCategory::Coverage => "coverage",
            GuidelineCategory::Pedagogy => "pedagogy",
            GuidelineCategory::Diagrams => "diagrams",
            GuidelineCategory::MathNotation => "math_notation",
            GuidelineCategory::Citations => "citations",
            GuidelineCategory::AlgorithmsCode => "algorithms_code",
        }
    }

    /// Human-readable heading (used in the rendered banner).
    pub const fn title(self) -> &'static str {
        match self {
            GuidelineCategory::Placement => "Placement & structure",
            GuidelineCategory::Coverage => "Coverage",
            GuidelineCategory::Pedagogy => "Pedagogy",
            GuidelineCategory::Diagrams => "Diagrams",
            GuidelineCategory::MathNotation => "Mathematical notation",
            GuidelineCategory::Citations => "Citations & DOIs",
            GuidelineCategory::AlgorithmsCode => "Algorithms & code",
        }
    }
}

/// One documentation guideline. `text` is stored verbatim — never reword the
/// user's guideline; the wording is the contract.
#[derive(Debug, Clone, Copy)]
pub struct GuidelineSeed {
    /// kebab-case unique id (`math-backticks`, `diagrams-flows`).
    pub slug: &'static str,
    /// The taxonomy axis (see [`GuidelineCategory`]).
    pub category: GuidelineCategory,
    /// The guideline, verbatim.
    pub text: &'static str,
}

const fn g(slug: &'static str, category: GuidelineCategory, text: &'static str) -> GuidelineSeed {
    GuidelineSeed {
        slug,
        category,
        text,
    }
}

/// The canonical 26 documentation guidelines, in the user's original order.
///
/// Every other rendering ([`render_instructions_banner`], [`guidelines_json`],
/// [`compact_for_orient`]) is derived from this list — there is exactly one
/// place to edit.
pub fn guideline_seeds() -> Vec<GuidelineSeed> {
    use GuidelineCategory::{
        AlgorithmsCode, Citations, Coverage, Diagrams, MathNotation, Pedagogy, Placement,
    };
    vec![
        g(
            "doc-placement",
            Placement,
            "Ensure appropriate placement of documentation files and directories.",
        ),
        g(
            "doc-naming-structure",
            Placement,
            "Ensure the files and directories are intuitively named and well structured.",
        ),
        g(
            "coverage-doc-types",
            Coverage,
            "Ensure there is a plethora of theoretical, scientific, design, architectural, \
             engineering, security, and usage documentation.",
        ),
        g(
            "coverage-semantics",
            Coverage,
            "Ensure complete and thorough coverage of intended behavior (semantics).",
        ),
        g(
            "coverage-syntax",
            Coverage,
            "Ensure complete and thorough coverage of syntax where appropriate.",
        ),
        g(
            "pedagogy-presentation",
            Pedagogy,
            "Ensure the documents are presented pedagogically with examples, diagrams, \
             mathematical formulae, citations, pseudocode, code snippets, and related (as \
             appropriate).",
        ),
        g(
            "diagrams-plenty",
            Diagrams,
            "Ensure there are plenty of diagrams to illustrate the concepts and how the components \
             work together.",
        ),
        g(
            "diagrams-best-types",
            Diagrams,
            "Choose the best types of diagrams for each illustration.",
        ),
        g(
            "diagrams-best-actors",
            Diagrams,
            "Choose the best actors to represent the components in the diagrams.",
        ),
        g(
            "diagrams-pgmcp-catalog",
            Diagrams,
            "Determine the best diagrams for each illustration and refer to the diagramming \
             catalog in pgmcp for available tooling.",
        ),
        g(
            "diagrams-prefer-plantuml",
            Diagrams,
            "Prefer PlantUML over Mermaid for any diagram type both support; reach for Mermaid \
             only where PlantUML has no equivalent. PlantUML output is byte-reproducible and \
             renders LaTeX; Mermaid's is neither.",
        ),
        g(
            "diagrams-plantuml-latex",
            Diagrams,
            "PlantUML supports LaTeX expressions — <latex>…</latex> inline and <math>…</math>, \
             typeset by the bundled JLaTeXMath into embedded vector SVG — so render mathematical \
             formulae in diagram labels with PlantUML LaTeX rather than unicode literals.",
        ),
        g(
            "diagrams-fully-colored",
            Diagrams,
            "Ensure all the diagrams are fully colored with intuitive colorization per concept.",
        ),
        g(
            "diagrams-complete",
            Diagrams,
            "Ensure All diagrams are complete, end-to-end, easy to follow, comprehensible, and \
             intuitive.",
        ),
        g(
            "diagrams-flows",
            Diagrams,
            "Ensure that diagrams that illustrate flows are easy to follow, end-to-end, and \
             complete.",
        ),
        g(
            "math-mathjax",
            MathNotation,
            "Use MathJax syntax for LaTeX expressions in mathematical prose; do not spell formulae \
             out with unicode literals. Unicode remains correct for non-mathematical text — \
             box-drawing, arrows, enumerations, and separators.",
        ),
        g(
            "math-delimiters",
            MathNotation,
            // Raw string: the escape sequences below are LaTeX, not Rust. The
            // `$…$` / `$$…$$` prohibition is not stylistic — GitHub's CommonMark
            // pass rewrites backslash escapes inside those spans before MathJax
            // sees them, corrupting the expression loudly or silently.
            r"Delimit math for GitHub-flavored Markdown: inline math is a backtick span wrapped in dollar signs, and display math is a fenced block whose info-string is `math`. Never use $…$ or $$…$$ — GitHub's CommonMark pass strips backslash escapes (\_ \{ \} \; \, \#) before MathJax parses them, corrupting the expression loudly or silently. Write a literal dollar sign as inline code, and never let an ASCII letter abut the opening delimiter.",
        ),
        g(
            "math-backticks",
            MathNotation,
            "Ensure all mathematical expressions are properly delimited as math spans rather than \
             left as bare prose or as an inert code span.",
        ),
        g(
            "citations-exist",
            Citations,
            "Ensure all citations exist, are correctly represented, and are properly used.",
        ),
        g(
            "citations-doi-links",
            Citations,
            "Link as many citations as possible to their DOIs.",
        ),
        g(
            "citations-doi-valid",
            Citations,
            "Ensure all DOIs exist and are properly represented and utilized.",
        ),
        g(
            "pedagogy-define-terms",
            Pedagogy,
            "Ensure all symbols, acronyms, and key terms are well and pedagogically defined prior \
             to use, whether that be in mathematical formulae, markdown tables, prose, or \
             elsewhere.",
        ),
        g(
            "pedagogy-intuition-rationale",
            Pedagogy,
            "Ensure the intuitions, theoretical bases, and rationale of all the components are \
             clearly and fully conveyed; upon reading, the reader should understand what each \
             component is, what it does, how it does it, and why it was selected for its purposes.",
        ),
        g(
            "pedagogy-logical-flow",
            Pedagogy,
            "Ensure the flow of the documents is logical and precise.",
        ),
        g(
            "algorithms-literate-pseudocode",
            AlgorithmsCode,
            "Use pseudocode to describe algorithms and present all algorithms in literate \
             programming form (Knuth's literate programming).",
        ),
        g(
            "code-snippets-valid",
            AlgorithmsCode,
            "Ensure all code snippets are valid (syntactically and semantically).",
        ),
    ]
}

/// The directive banner injected into EVERY client's MCP `instructions`
/// (always-on, universal). Groups the verbatim guidelines by category under a
/// MUST-follow preamble, and points at the `documentation_guidelines` tool for
/// the structured form. Also reused as the body of the `pgmcp://guidelines`
/// resource and as the source for the `~/.claude/CLAUDE.md` mirror.
pub fn render_instructions_banner() -> String {
    let seeds = guideline_seeds();
    // Preamble + 7 headings + 26 bullets; comfortably under one realloc.
    let mut out = String::with_capacity(4096);
    out.push_str(
        "### Documentation guidelines (pgmcp — apply across ALL agents)\n\n\
         When you produce or edit ANY documentation in ANY project — design / architecture / \
         theory docs, READMEs, ADRs, API & usage guides, papers — you MUST follow these \
         guidelines. They are enforced uniformly for every agent that connects to pgmcp. Call the \
         `documentation_guidelines` tool for the structured list (slug + category + text).\n",
    );
    for cat in GuidelineCategory::ALL {
        out.push_str("\n**");
        out.push_str(cat.title());
        out.push_str("**\n");
        for s in seeds.iter().filter(|s| s.category == cat) {
            out.push_str("- ");
            out.push_str(s.text);
            out.push('\n');
        }
    }
    out
}

/// Full structured payload for the `documentation_guidelines` MCP tool and the
/// `pgmcp://guidelines` resource: the verbatim list (slug + category + text),
/// the category vocabulary, a count, and an enforcement note.
pub fn guidelines_json() -> Value {
    let seeds = guideline_seeds();
    let items: Vec<Value> = seeds
        .iter()
        .map(|s| {
            json!({
                "slug": s.slug,
                "category": s.category.as_str(),
                "text": s.text,
            })
        })
        .collect();
    let categories: Vec<Value> = GuidelineCategory::ALL
        .iter()
        .map(|c| json!({ "slug": c.as_str(), "title": c.title() }))
        .collect();
    json!({
        "documentation_guidelines": items,
        "count": seeds.len(),
        "categories": categories,
        "note": "These guidelines apply to ALL documentation you produce, in every project and as \
                 every agent. pgmcp injects them into its MCP `instructions` for all clients; they \
                 are also surfaced in `orient` and at the `pgmcp://guidelines` resource.",
    })
}

/// Compact reinforcement embedded in the `orient` envelope. The full text is
/// already always-on in the MCP `instructions`, so here we keep a concise
/// pointer (count + category titles + a directive note) to avoid bloating an
/// already-large response.
pub fn compact_for_orient() -> Value {
    let titles: Vec<&'static str> = GuidelineCategory::ALL.iter().map(|c| c.title()).collect();
    json!({
        "count": guideline_seeds().len(),
        "categories": titles,
        "note": "When producing documentation, follow pgmcp's documentation guidelines (full text \
                 is in this server's MCP instructions; call `documentation_guidelines` to \
                 enumerate them).",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn there_are_twenty_six_guidelines() {
        assert_eq!(guideline_seeds().len(), 26);
    }

    #[test]
    fn slugs_are_unique() {
        let seeds = guideline_seeds();
        let set: HashSet<&str> = seeds.iter().map(|s| s.slug).collect();
        assert_eq!(set.len(), seeds.len(), "duplicate guideline slug");
    }

    #[test]
    fn every_category_is_represented_and_fields_nonempty() {
        let seeds = guideline_seeds();
        for cat in GuidelineCategory::ALL {
            assert!(
                seeds.iter().any(|s| s.category == cat),
                "category {cat:?} has no guidelines"
            );
        }
        for s in &seeds {
            assert!(!s.slug.trim().is_empty(), "empty slug");
            assert!(!s.text.trim().is_empty(), "empty text for slug {}", s.slug);
        }
    }

    #[test]
    fn category_partition_is_exact() {
        let seeds = guideline_seeds();
        let count = |c: GuidelineCategory| seeds.iter().filter(|s| s.category == c).count();
        assert_eq!(count(GuidelineCategory::Placement), 2);
        assert_eq!(count(GuidelineCategory::Coverage), 3);
        assert_eq!(count(GuidelineCategory::Pedagogy), 4);
        assert_eq!(count(GuidelineCategory::Diagrams), 9);
        assert_eq!(count(GuidelineCategory::MathNotation), 3);
        assert_eq!(count(GuidelineCategory::Citations), 3);
        assert_eq!(count(GuidelineCategory::AlgorithmsCode), 2);
        // The partition is total: the per-category counts sum to the whole.
        let total: usize = GuidelineCategory::ALL.iter().map(|c| count(*c)).sum();
        assert_eq!(total, seeds.len());
    }

    #[test]
    fn banner_contains_every_guideline_and_directive_anchors() {
        let banner = render_instructions_banner();
        assert!(banner.contains("MUST follow"));
        assert!(banner.contains("documentation_guidelines"));
        assert!(banner.contains("Knuth's literate programming"));
        // The two mandates most easily lost to a reword: the diagram-engine
        // ranking and the math-notation switch. Pin them by name.
        assert!(banner.contains("Prefer PlantUML over Mermaid"));
        assert!(banner.contains("MathJax"));
        for s in guideline_seeds() {
            assert!(
                banner.contains(s.text),
                "banner missing guideline: {}",
                s.slug
            );
        }
        for cat in GuidelineCategory::ALL {
            assert!(
                banner.contains(cat.title()),
                "banner missing heading: {}",
                cat.as_str()
            );
        }
    }

    #[test]
    fn json_payload_shape_is_complete() {
        let v = guidelines_json();
        assert_eq!(v["count"].as_u64(), Some(26));
        assert_eq!(
            v["documentation_guidelines"].as_array().map(Vec::len),
            Some(26)
        );
        assert_eq!(v["categories"].as_array().map(Vec::len), Some(7));
        assert!(v["note"].is_string());
        // Each item carries the three contract fields.
        for item in v["documentation_guidelines"].as_array().expect("array") {
            assert!(item["slug"].is_string());
            assert!(item["category"].is_string());
            assert!(item["text"].is_string());
        }
    }

    #[test]
    fn orient_payload_is_a_concise_pointer() {
        let v = compact_for_orient();
        assert_eq!(v["count"].as_u64(), Some(26));
        assert_eq!(v["categories"].as_array().map(Vec::len), Some(7));
        assert!(
            v["note"]
                .as_str()
                .expect("note")
                .contains("documentation_guidelines")
        );
    }
}
