//! Pure invariant-mining helpers (Code-Digital-Twin rationale enrichment).
//!
//! These extract `facet='invariant'` candidates from text sources — ADRs,
//! mandate files (CLAUDE.md/AGENTS.md), commit messages, code comments — by
//! detecting *invariant-language cues* ("must", "never", "always", "do not",
//! "invariant", …). All pure + exhaustively testable; the DB-touching cron
//! (`crate::cron::ontology_invariants`) wraps them.
//!
//! A candidate's [`name`](InvariantCandidate::name) is a *normalized merge key*
//! derived from the constraint phrase, so the same invariant expressed in an
//! ADR, a mandate, and a commit collapses onto **one** concept carrying three
//! evidence rows (the deterministic precursor to the Phase-9 egglog
//! canonicalization).

/// Invariant-language cues (lowercase substring match). "must" subsumes
/// "must not"; the set is intentionally small + high-precision.
const CUES: &[&str] = &[
    "must",
    "never",
    "always",
    "do not",
    "don't",
    "shall",
    "invariant",
    "required",
    "may not",
    "cannot",
    "forbidden",
    "prohibited",
    "mandatory",
];

/// Tokens dropped when building the normalized merge key — modal/auxiliary verbs
/// and high-frequency function words that carry no invariant identity.
const NAME_STOP: &[&str] = &[
    "must", "should", "shall", "the", "a", "an", "to", "of", "is", "are", "be", "will", "may",
    "it", "not", "do", "dont", "that", "this", "until", "its", "in", "on", "for", "and", "or",
    "we", "you", "always", "never", "with", "as", "at", "by", "if",
];

/// A mined invariant: a stable merge `name`, the constraint sentence, and a short
/// rationale (source provenance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantCandidate {
    pub name: String,
    pub constraint_text: String,
    pub rationale: String,
}

/// Does `text` contain any invariant-language cue (case-insensitive)?
pub fn has_invariant_cue(text: &str) -> bool {
    let lower = text.to_lowercase();
    CUES.iter().any(|c| lower.contains(c))
}

/// Lines of `text` that carry an invariant cue, trimmed and length-bounded
/// (12..=400 chars), de-duplicated in first-seen order. Markdown bullet/heading
/// markers are stripped from the front.
pub fn invariant_cue_lines(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw
            .trim()
            .trim_start_matches(['#', '-', '*', '>', '+', ' ', '\t'])
            .trim();
        if line.len() < 12 || line.len() > 400 {
            continue;
        }
        if has_invariant_cue(line) && seen.insert(line.to_string()) {
            out.push(line.to_string());
        }
    }
    out
}

/// Build a stable, order-independent merge key from a constraint phrase: lowercase
/// salient tokens (stopwords + cues dropped), de-duplicated, first six joined with
/// `-`. Two sources expressing the same rule yield the same key ⇒ one concept.
pub fn normalize_invariant_name(constraint: &str) -> String {
    let lower = constraint.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut toks: Vec<String> = Vec::new();
    for t in lower.split(|c: char| !(c.is_alphanumeric())) {
        if t.len() < 2 || NAME_STOP.contains(&t) {
            continue;
        }
        if seen.insert(t.to_string()) {
            toks.push(t.to_string());
        }
        if toks.len() == 6 {
            break;
        }
    }
    if toks.is_empty() {
        // Degenerate constraint (all stopwords) — fall back to a trimmed slug so
        // the candidate still has a stable, non-empty name.
        return lower
            .split_whitespace()
            .take(6)
            .collect::<Vec<_>>()
            .join("-");
    }
    toks.join("-")
}

/// Extract a single invariant from an ADR: the first cue-bearing line is the
/// constraint; the first `#`/`##` heading (else `fallback`) seeds the rationale.
/// `None` when the ADR carries no invariant-language cue.
pub fn extract_adr_invariant(content: &str, fallback: &str) -> Option<InvariantCandidate> {
    let title = content
        .lines()
        .find_map(|l| {
            let t = l.trim();
            t.strip_prefix("# ")
                .or_else(|| t.strip_prefix("## "))
                .map(str::trim)
        })
        .unwrap_or(fallback);
    let constraint = invariant_cue_lines(content).into_iter().next()?;
    Some(InvariantCandidate {
        name: normalize_invariant_name(&constraint),
        constraint_text: constraint,
        rationale: format!("ADR: {title}"),
    })
}

/// One invariant per cue-bearing line of `text` (mandate files, comment blocks).
pub fn extract_line_invariants(text: &str, rationale: &str) -> Vec<InvariantCandidate> {
    invariant_cue_lines(text)
        .into_iter()
        .map(|line| InvariantCandidate {
            name: normalize_invariant_name(&line),
            constraint_text: line,
            rationale: rationale.to_string(),
        })
        .collect()
}

/// Extract an invariant from a commit's subject+body, if cued. The constraint is
/// the first cue line across subject then body.
pub fn extract_commit_invariant(subject: &str, body: Option<&str>) -> Option<InvariantCandidate> {
    let mut joined = String::with_capacity(subject.len() + body.map_or(0, |b| b.len()) + 1);
    joined.push_str(subject);
    if let Some(b) = body {
        joined.push('\n');
        joined.push_str(b);
    }
    let constraint = invariant_cue_lines(&joined).into_iter().next()?;
    Some(InvariantCandidate {
        name: normalize_invariant_name(&constraint),
        constraint_text: constraint,
        rationale: "git commit".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cue_detection() {
        assert!(has_invariant_cue("ambiguity must propagate end-to-end"));
        assert!(has_invariant_cue("Never disambiguate over the parse tree"));
        assert!(has_invariant_cue("This is an INVARIANT of the pipeline"));
        assert!(!has_invariant_cue(
            "just a normal descriptive sentence here"
        ));
    }

    #[test]
    fn cue_lines_strip_markers_and_bound_length() {
        let text = "# Heading\n- ambiguity must propagate end-to-end\nshort\n* always validate the input token stream\nplain line no cue word here";
        let lines = invariant_cue_lines(text);
        assert!(
            lines
                .iter()
                .any(|l| l == "ambiguity must propagate end-to-end")
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "always validate the input token stream")
        );
        assert!(
            !lines.iter().any(|l| l == "short"),
            "too-short line excluded"
        );
    }

    #[test]
    fn normalized_name_is_stable_across_sources() {
        // The same rule from an ADR line, a mandate bullet, and a commit subject
        // must collapse to one merge key.
        let a = normalize_invariant_name("ambiguity must propagate end-to-end");
        let b = normalize_invariant_name("- ambiguity must propagate end-to-end");
        let c = normalize_invariant_name("ambiguity MUST propagate end-to-end");
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert!(a.contains("ambiguity") && a.contains("propagate"));
        assert!(!a.contains("must"), "modal verb dropped from the key");
    }

    #[test]
    fn adr_extract_uses_title_for_rationale() {
        let adr = "# ADR-099: Ambiguity propagation\n\n## Decision\nambiguity must propagate end-to-end until evidence rejects it\n";
        let inv = extract_adr_invariant(adr, "fallback").expect("cued ADR");
        assert!(inv.constraint_text.contains("ambiguity"));
        assert!(inv.rationale.contains("Ambiguity propagation"));
    }

    #[test]
    fn adr_without_cue_is_none() {
        assert!(extract_adr_invariant("# Title\n\nplain prose, no directive", "f").is_none());
    }

    #[test]
    fn commit_extract_reads_subject_and_body() {
        let inv =
            extract_commit_invariant("feat: parser", Some("ambiguity must propagate end-to-end"))
                .expect("cued body");
        assert!(inv.constraint_text.contains("ambiguity"));
    }
}
