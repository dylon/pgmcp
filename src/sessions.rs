//! Session-level mandate observation, extraction, and persistence.
//!
//! This module mirrors `src/mandates.rs` (file-backed workspace/project
//! mandates) but for *session* scope — short-term, mid-conversation
//! standing directives extracted from user prompts and re-injected as
//! `additionalContext` to combat the LLM's short-term-memory problem.
//!
//! The extractor is a tiered heuristic regex pipeline calibrated against
//! the user's actual Claude+Codex history (see plan file for the
//! empirical mining methodology). It is intentionally LLM-free so it can
//! run in-hook on every prompt without external dependencies.

use std::collections::HashSet;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use pgvector::Vector;
use regex::Regex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;
mod polarity;
pub use polarity::*;

// ============================================================================
// Public types
// ============================================================================

/// One mandate as extracted from a prompt (no DB id; pre-persistence).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExtractedMandate {
    pub polarity: MandatePolarity,
    pub imperative: String,
    pub target: Option<String>,
    pub cwd_prefix: Option<String>,
    pub cue_tier: CueTier,
    pub salience: f32,
}

/// One persisted session mandate.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SessionMandate {
    pub id: i64,
    pub session_id: Uuid,
    pub source_prompt_id: i64,
    pub polarity: String,
    pub imperative: String,
    pub target: Option<String>,
    pub cwd_prefix: Option<String>,
    pub cue_tier: String,
    pub salience: f32,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub last_reinforced_at: DateTime<Utc>,
    pub reinforcement_count: i32,
}

/// One promoted (durable) mandate.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DurableMandate {
    pub id: i64,
    pub scope: String,
    pub project_id: Option<i32>,
    pub polarity: String,
    pub imperative: String,
    pub target: Option<String>,
    pub source_mandate_id: Option<i64>,
    pub promoted_at: DateTime<Utc>,
    pub file_path: Option<String>,
}

// ============================================================================
// Tiered cue table (empirically calibrated)
// ============================================================================

#[derive(Debug, Clone, Copy)]
struct Cue {
    pattern: &'static str,
    polarity: MandatePolarity,
    tier: CueTier,
    prior_weight: f32,
    requires_gate: bool,
}

const CUES: &[Cue] = &[
    // ---------- Tier A: strong, self-standing ----------
    Cue {
        pattern: r"(?i)\bnever\s+(?:do|make|change|modify|revert|destroy|clobber|reset|stash|amend|force.?push|skip|hide|discard|gloss|hand.?wave|fudge|hack|second.?guess|disable|delete|push|commit|merge|use|run|include|admit|forget|accept|leave|stop|panic|hardcode|hard.?code|assume|guess|fabricate|hallucinate|ad.?hoc)\b",
        polarity: MandatePolarity::Never,
        tier: CueTier::A,
        prior_weight: 10.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\bnon[- ]negotiable\b",
        polarity: MandatePolarity::Mandate,
        tier: CueTier::A,
        prior_weight: 10.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\bmandator(?:y|ily)\b",
        polarity: MandatePolarity::Mandate,
        tier: CueTier::A,
        prior_weight: 10.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\bgolden rule\b",
        polarity: MandatePolarity::Mandate,
        tier: CueTier::A,
        prior_weight: 10.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\b(?:do not|don'?t)\b[^.\n!?]{1,80}\b(?:again|ever|unless|without my (?:explicit )?(?:approval|permission))\b",
        polarity: MandatePolarity::Never,
        tier: CueTier::A,
        prior_weight: 10.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\bnever\b[^.\n!?]{1,80}\b(?:again|ever|without)\b",
        polarity: MandatePolarity::Never,
        tier: CueTier::A,
        prior_weight: 10.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\bwithout my (?:explicit )?(?:approval|permission)\b",
        polarity: MandatePolarity::Permission,
        tier: CueTier::A,
        prior_weight: 10.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\bunless (?:I (?:explicitly|specifically|first) (?:approve|ask|tell)|you (?:are|get) (?:explicitly )?asked)\b",
        polarity: MandatePolarity::Permission,
        tier: CueTier::A,
        prior_weight: 10.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\bnot allowed\b|\bforbidden\b|\bprohibited\b",
        polarity: MandatePolarity::Constraint,
        tier: CueTier::A,
        prior_weight: 9.0,
        requires_gate: false,
    },
    // ---------- Tier B: standing scope by qualifier; gate required ----------
    Cue {
        pattern: r"(?i)\b(?:do not|don'?t)\b",
        polarity: MandatePolarity::Never,
        tier: CueTier::B,
        prior_weight: 7.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bnever\b",
        polarity: MandatePolarity::Never,
        tier: CueTier::B,
        prior_weight: 7.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\balways\b",
        polarity: MandatePolarity::Always,
        tier: CueTier::B,
        prior_weight: 8.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bregardless\b",
        polarity: MandatePolarity::Always,
        tier: CueTier::B,
        prior_weight: 8.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bevery time\b|\beach time\b|\banytime you\b|\bany time you\b",
        polarity: MandatePolarity::FromNowOn,
        tier: CueTier::B,
        prior_weight: 8.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bin (?:every|each|this) (?:case|step|round|run|session|project|repo|codebase)\b",
        polarity: MandatePolarity::FromNowOn,
        tier: CueTier::B,
        prior_weight: 8.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bfor this (?:project|repo|repository|codebase)\b",
        polarity: MandatePolarity::ProjectRule,
        tier: CueTier::B,
        prior_weight: 8.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\b(?:prefer|favou?r) (?:to |using )?[a-z`]",
        polarity: MandatePolarity::Prefer,
        tier: CueTier::B,
        prior_weight: 7.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bin favou?r of\b|\binstead of\b|\brather than\b",
        polarity: MandatePolarity::Prefer,
        tier: CueTier::B,
        prior_weight: 7.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bavoid\b|\btry not to\b|\brefrain from\b|\b(?:stay|steer) away from\b",
        polarity: MandatePolarity::Avoid,
        tier: CueTier::B,
        prior_weight: 7.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\b(?:no need|need not)\b",
        polarity: MandatePolarity::Avoid,
        tier: CueTier::B,
        prior_weight: 6.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bdo not include\b",
        polarity: MandatePolarity::Avoid,
        tier: CueTier::B,
        prior_weight: 7.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bremember (?:to|that|,)\b",
        polarity: MandatePolarity::Remember,
        tier: CueTier::B,
        prior_weight: 7.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bmake sure (?:to|that|you)\b",
        polarity: MandatePolarity::Remember,
        tier: CueTier::B,
        prior_weight: 7.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bbe sure (?:to|that|you)\b",
        polarity: MandatePolarity::Remember,
        tier: CueTier::B,
        prior_weight: 7.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bkeep in mind\b",
        polarity: MandatePolarity::Remember,
        tier: CueTier::B,
        prior_weight: 7.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bdo not forget\b|\b(?:don'?t) forget\b",
        polarity: MandatePolarity::Remember,
        tier: CueTier::B,
        prior_weight: 8.0,
        requires_gate: true,
    },
    // ---------- Tier C: correction signals ----------
    Cue {
        pattern: r"(?i)\bI (?:have |already |just )?(?:told|asked|said|warned) you\b",
        polarity: MandatePolarity::Correction,
        tier: CueTier::C,
        prior_weight: 10.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\bI explicitly (?:told|said|asked|stated)\b",
        polarity: MandatePolarity::Correction,
        tier: CueTier::C,
        prior_weight: 10.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\byou keep\b|\bwhy do you keep\b",
        polarity: MandatePolarity::Correction,
        tier: CueTier::C,
        prior_weight: 9.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\bwhat the (?:hell|fuck|f\b)\b",
        polarity: MandatePolarity::Correction,
        tier: CueTier::C,
        prior_weight: 9.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\bI'?m not happy\b|\bdid I (?:say|ask|tell)\b",
        polarity: MandatePolarity::Correction,
        tier: CueTier::C,
        prior_weight: 9.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\b(?:stop|quit) (?:it|that|doing|using)\b",
        polarity: MandatePolarity::Correction,
        tier: CueTier::C,
        prior_weight: 9.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\bagain[!?]+",
        polarity: MandatePolarity::Correction,
        tier: CueTier::C,
        prior_weight: 8.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\bagain\?!\?",
        polarity: MandatePolarity::Correction,
        tier: CueTier::C,
        prior_weight: 9.0,
        requires_gate: false,
    },
    // ---------- Tier D: weak / companion-only (require Tier A/B/C cue in same sentence) ----------
    Cue {
        pattern: r"(?i)\byou must\b|\byou (?:will|shall|need to) (?:always|never)\b",
        polarity: MandatePolarity::Mandate,
        tier: CueTier::D,
        prior_weight: 7.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bmust\b",
        polarity: MandatePolarity::Mandate,
        tier: CueTier::D,
        prior_weight: 5.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\brequire(?:d|s|ment)?\b",
        polarity: MandatePolarity::Mandate,
        tier: CueTier::D,
        prior_weight: 5.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bcritical(?:ly)?\b",
        polarity: MandatePolarity::Mandate,
        tier: CueTier::D,
        prior_weight: 4.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bimportant(?:ly)?\b",
        polarity: MandatePolarity::Mandate,
        tier: CueTier::D,
        prior_weight: 3.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\brules?:",
        polarity: MandatePolarity::Mandate,
        tier: CueTier::D,
        prior_weight: 6.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\b(?:can'?t|cannot)\b",
        polarity: MandatePolarity::Constraint,
        tier: CueTier::D,
        prior_weight: 4.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bensure\b",
        polarity: MandatePolarity::Remember,
        tier: CueTier::D,
        prior_weight: 3.0,
        requires_gate: true,
    },
    Cue {
        pattern: r"(?i)\bno longer\b",
        polarity: MandatePolarity::Never,
        tier: CueTier::D,
        prior_weight: 5.0,
        requires_gate: true,
    },
    // ---------- Tier E: temporal cues (rare but high precision) ----------
    Cue {
        pattern: r"(?i)\bfrom now on\b|\bgoing forward\b|\bhenceforth\b|\bfrom this point( on)?\b|\b(?:starting|beginning) (?:now|today)\b|\bin the future\b|\bnext time\b",
        polarity: MandatePolarity::FromNowOn,
        tier: CueTier::E,
        prior_weight: 9.0,
        requires_gate: false,
    },
    // ---------- Tier F: process patterns ----------
    // Stem-based verb matches (commit/committed/committing, etc.) so the pattern handles tenses.
    Cue {
        pattern: r"(?i)\b(?:always|never|do not|don'?t)\b[^.!?\n]{0,80}\b(?:before|after|when)\s+(?:you )?(?:commit|push|merg|run|build|test|deploy|edit|writ|delet|optimi|benchmark)\w*\b",
        polarity: MandatePolarity::ProcessRule,
        tier: CueTier::F,
        prior_weight: 8.0,
        requires_gate: false,
    },
    Cue {
        pattern: r"(?i)\bbefore (?:commit|push|mak|run|build|test|deploy|edit|writ|finaliz|merg|optimi|benchmark)\w*\b",
        polarity: MandatePolarity::ProcessRule,
        tier: CueTier::F,
        prior_weight: 6.0,
        requires_gate: true,
    },
];

// ----- Disambiguation gates (Tier-B / Tier-D) -----
const STANDING_QUALIFIER_PATTERN: &str = r"(?i)\b(?:again|ever|unless|without|regardless|from now on|going forward|every time|each time|in (?:this|every) (?:project|repo|case|session|run)|next time|anytime|in the future)\b";
const EMPHATIC_PUNCTUATION_PATTERN: &str = r"!{1,}|\?!\?|\?!";
const EXPLICIT_MANDATE_MARKER_PATTERN: &str =
    r"(?i)\b(?:mandatory|non.?negotiable|golden rule|rules?:|required|critical|must)\b";
const CORRECTION_PRESENT_PATTERN: &str = r"(?i)\b(?:I (?:told|asked|said|warned) you|you keep|what the (?:hell|fuck|f\b)|again[!?]|did I (?:say|ask|tell))\b";
const ALL_CAPS_SPAN_PATTERN: &str = r"\b[A-Z][A-Z0-9_]{2,}(?:\s+[A-Z][A-Z0-9_]{2,}){3,}\b";

// ----- Exclusion filters (drop before any gate evaluation) -----
const EXCLUSION_PATTERNS: &[&str] = &[
    r"(?i)\bstop\s+(?:the\s+)?(?:daemon|server|process|service|build|run|task|job|profiling|recording|optimization)\b",
    r"(?i)\bnever\s+(?:calls?|fires?|gets?|reaches?|happens?|emits?|writes?|reads?|sets?|returns?|matches?)\b",
    r"(?i)\balways\s+(?:fires?|runs?|matches?|returns?|emits?|gets?|calls?)\b",
    r"(?i)\brequired\s+(?:by|for|to be|fields?|argument|parameter)\b",
    r"(?i)\bcritical\s+(?:path|section|risk|files?|context|race|state)\b",
    r"(?i)\bimportant\s+(?:context|note|finding|aspect|factor)\b",
];

// Scope hint
const SCOPE_HINT_PATTERN: &str = r"(?i)\bin (?:this|the) (?:project|repo|repository|codebase)\b|\bfor (?:this|the) (?:project|repo|repository|codebase)\b";

// Back-tick / quoted target
const BACKTICK_TARGET_PATTERN: &str = r"`([^`\n]{1,80})`";

// Bullet/list lead
const BULLET_LEAD_PATTERN: &str = r"^\s*(?:[-*•]\s+|\d+[.)]\s+)";

// ============================================================================
// Compiled-regex caches
// ============================================================================

struct CompiledCue {
    re: Regex,
    polarity: MandatePolarity,
    tier: CueTier,
    prior_weight: f32,
    requires_gate: bool,
}

fn compiled_cues() -> &'static [CompiledCue] {
    static CELL: OnceLock<Vec<CompiledCue>> = OnceLock::new();
    CELL.get_or_init(|| {
        CUES.iter()
            .map(|c| CompiledCue {
                re: Regex::new(c.pattern).expect("cue regex compiles"),
                polarity: c.polarity,
                tier: c.tier,
                prior_weight: c.prior_weight,
                requires_gate: c.requires_gate,
            })
            .collect()
    })
}

fn compiled_exclusions() -> &'static [Regex] {
    static CELL: OnceLock<Vec<Regex>> = OnceLock::new();
    CELL.get_or_init(|| {
        EXCLUSION_PATTERNS
            .iter()
            .map(|p| Regex::new(p).expect("exclusion regex compiles"))
            .collect()
    })
}

macro_rules! lazy_regex {
    ($fn_name:ident, $pat:expr) => {
        fn $fn_name() -> &'static Regex {
            static CELL: OnceLock<Regex> = OnceLock::new();
            CELL.get_or_init(|| Regex::new($pat).expect("regex compiles"))
        }
    };
}
lazy_regex!(standing_qualifier_re, STANDING_QUALIFIER_PATTERN);
lazy_regex!(emphatic_punct_re, EMPHATIC_PUNCTUATION_PATTERN);
lazy_regex!(explicit_marker_re, EXPLICIT_MANDATE_MARKER_PATTERN);
lazy_regex!(correction_present_re, CORRECTION_PRESENT_PATTERN);
lazy_regex!(all_caps_span_re, ALL_CAPS_SPAN_PATTERN);
lazy_regex!(scope_hint_re, SCOPE_HINT_PATTERN);
lazy_regex!(backtick_target_re, BACKTICK_TARGET_PATTERN);
lazy_regex!(bullet_lead_re, BULLET_LEAD_PATTERN);

// ============================================================================
// Pure extraction logic
// ============================================================================

const MAX_IMPERATIVE_CHARS: usize = 200;
const NEARBY_WINDOW_CHARS: usize = 150;
const PUNCT_WINDOW_CHARS: usize = 60;

/// Split a prompt into sentences. Splits ONLY on `.!?\n` and never on `,`
/// (the user's corpus has many multi-clause comma-continuations). Tracks
/// back-tick parity so we don't break inside `` `.expect(...)` ``-style spans.
fn split_sentences(prompt: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_backtick = false;
    let bytes = prompt.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        let c = *b as char;
        if c == '`' {
            in_backtick = !in_backtick;
            continue;
        }
        if !in_backtick && matches!(c, '.' | '!' | '?' | '\n') {
            let segment = &prompt[start..=i];
            if !segment.trim().is_empty() {
                out.push(segment);
            }
            start = i + 1;
        }
    }
    if start < prompt.len() {
        let tail = &prompt[start..];
        if !tail.trim().is_empty() {
            out.push(tail);
        }
    }
    out
}

/// Slice a window of ±`window_chars` around `byte_pos` on char boundaries.
fn window_around(prompt: &str, byte_pos: usize, window_chars: usize) -> &str {
    let lo = prompt[..byte_pos.min(prompt.len())]
        .char_indices()
        .rev()
        .take(window_chars)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);
    let hi = prompt
        .get(byte_pos..)
        .map(|s| {
            s.char_indices()
                .take(window_chars)
                .last()
                .map(|(i, c)| byte_pos + i + c.len_utf8())
                .unwrap_or(prompt.len())
        })
        .unwrap_or(prompt.len());
    &prompt[lo..hi.min(prompt.len())]
}

/// Check the disambiguation gates around a cue match. Returns true if at least
/// one of the 6 gates fires within the relevant window.
fn gates_fire(prompt: &str, sentence: &str, cue_pos_in_prompt: usize) -> bool {
    let near_150 = window_around(prompt, cue_pos_in_prompt, NEARBY_WINDOW_CHARS);
    let near_60 = window_around(prompt, cue_pos_in_prompt, PUNCT_WINDOW_CHARS);
    let near_100 = window_around(prompt, cue_pos_in_prompt, 100);

    // (1) standing-scope qualifier
    if standing_qualifier_re().is_match(near_150) {
        return true;
    }
    // (2) emphatic punctuation
    if emphatic_punct_re().is_match(near_60) {
        return true;
    }
    // (3) explicit-mandate marker
    if explicit_marker_re().is_match(near_150) {
        return true;
    }
    // (4) correction polarity anywhere in same prompt
    if correction_present_re().is_match(prompt) {
        return true;
    }
    // (6) ALL-CAPS span
    if all_caps_span_re().is_match(near_100) {
        return true;
    }
    // (7) bullet-list lead in the sentence — CLAUDE.md / AGENTS.md style.
    if bullet_lead_re().is_match(sentence.trim_start()) {
        return true;
    }
    // (8) back-ticked technical specifics in the sentence — the user uses
    // back-ticks to anchor mandates to precise targets (`unwrap()`, etc.).
    if backtick_target_re().is_match(sentence) {
        return true;
    }
    // (5) CLAUDE.md re-statement deferred to caller (requires fs access).
    false
}

/// Truncate at a word boundary, ≤ MAX_IMPERATIVE_CHARS.
fn truncate_imperative(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= MAX_IMPERATIVE_CHARS {
        return trimmed.to_string();
    }
    // Cap at MAX_IMPERATIVE_CHARS chars on a word boundary.
    let mut end = 0;
    for (i, c) in trimmed.char_indices() {
        if trimmed[..i].chars().count() > MAX_IMPERATIVE_CHARS && c.is_whitespace() {
            end = i;
            break;
        }
        end = i + c.len_utf8();
    }
    let mut out = trimmed[..end].trim_end().to_string();
    out.push('…');
    out
}

/// Compute companion-feature salience boost.
fn companion_boost(prompt: &str, sentence: &str, cue_pos: usize, polarity: MandatePolarity) -> f32 {
    let mut mult = 1.0_f32;
    let near_60 = window_around(prompt, cue_pos, PUNCT_WINDOW_CHARS);
    let near_100 = window_around(prompt, cue_pos, 100);

    if emphatic_punct_re().is_match(near_60) {
        mult *= 1.25;
    }
    if all_caps_span_re().is_match(near_100) {
        mult *= 1.3;
    }
    if backtick_target_re().is_match(sentence) {
        mult *= 1.15;
    }
    if bullet_lead_re().is_match(sentence) {
        mult *= 1.1;
    }
    // Tier-A / Tier-C cue also present in same sentence → ×1.5
    // Quick check: any Tier-A or Tier-C cue regex matches the sentence.
    let same_sentence_strong = compiled_cues().iter().any(|c| {
        matches!(c.tier, CueTier::A | CueTier::C)
            && c.polarity != polarity
            && c.re.is_match(sentence)
    });
    if same_sentence_strong {
        mult *= 1.5;
    }
    mult
}

fn capture_target(sentence: &str) -> Option<String> {
    backtick_target_re()
        .captures(sentence)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

fn detect_scope_hint(prompt: &str, cwd: Option<&str>) -> Option<String> {
    if scope_hint_re().is_match(prompt) {
        cwd.map(|c| c.to_string())
    } else {
        None
    }
}

/// Mask code blocks / quoted spans with spaces (preserving byte offsets and
/// newlines) so cue and tool-family regexes don't fire on quoted text. Shared
/// by [`extract_mandates`] and [`classify_tool_suggestion`].
fn mask_exclusions(prompt: &str) -> String {
    let mut masked = prompt.to_string();
    for re in compiled_exclusions() {
        // Collect ranges first to avoid borrow issues.
        let ranges: Vec<(usize, usize)> =
            re.find_iter(prompt).map(|m| (m.start(), m.end())).collect();
        for (s, e) in ranges {
            // Replace with spaces of the same length (byte-wise) to preserve byte
            // positions for downstream regex offsets that reference `prompt`.
            if let Some(slice) = masked.get_mut(s..e) {
                for b in unsafe { slice.as_bytes_mut() } {
                    if *b != b'\n' {
                        *b = b' ';
                    }
                }
            }
        }
    }
    masked
}

/// Tiered heuristic extractor. Pure function. Optionally takes the session
/// cwd for `cwd_prefix` scope-hint detection.
pub fn extract_mandates(prompt: &str, cwd: Option<&str>) -> Vec<ExtractedMandate> {
    if prompt.trim().is_empty() {
        return Vec::new();
    }

    // Mask code blocks / quoted spans first (see `mask_exclusions`).
    let masked = mask_exclusions(prompt);

    let scope_hint = detect_scope_hint(prompt, cwd);
    let mut out: Vec<ExtractedMandate> = Vec::new();
    let mut seen: HashSet<(MandatePolarity, String)> = HashSet::new();

    for sentence in split_sentences(&masked) {
        let sentence_offset = unsafe {
            // SAFETY: sentence is a sub-slice of masked which has identical bytes/layout to prompt.
            sentence.as_ptr().offset_from(masked.as_ptr()) as usize
        };
        for cue in compiled_cues() {
            for m in cue.re.find_iter(sentence) {
                let cue_pos_in_prompt = sentence_offset + m.start();

                if cue.requires_gate && !gates_fire(prompt, sentence, cue_pos_in_prompt) {
                    continue;
                }

                let target = capture_target(sentence);
                let salience = (cue.prior_weight
                    * companion_boost(prompt, sentence, cue_pos_in_prompt, cue.polarity))
                .min(10.0);

                let imperative = truncate_imperative(sentence);
                let key = (cue.polarity, imperative.to_lowercase());
                if !seen.insert(key) {
                    continue;
                }

                out.push(ExtractedMandate {
                    polarity: cue.polarity,
                    imperative,
                    target: target.clone(),
                    cwd_prefix: scope_hint.clone(),
                    cue_tier: cue.tier,
                    salience,
                });
            }
        }
    }

    out
}

// ============================================================================
// Tool-suggestion classifier (JIT adoption nudges)
// ============================================================================

/// Under-used tool families the prompt classifier can suggest. A long-context
/// prompt maps to RLM (`a2a_pattern_recursive`); a collaboration prompt to the
/// A2A patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolFamily {
    Collaboration,
    LargeContext,
    MemoryWrite,
    MemoryRead,
    WorkItem,
}

struct ToolCue {
    pattern: &'static str,
    family: ToolFamily,
    weight: f32,
}

const TOOL_CUES: &[ToolCue] = &[
    ToolCue {
        pattern: r"(?i)\b(second opinion|another (?:agent|model|perspective)|sanity[- ]?check|peer[- ]?review|red[- ]?team|adversarial|brainstorm|critique this|get .{0,30}? to (?:review|check|weigh in))\b",
        family: ToolFamily::Collaboration,
        weight: 3.0,
    },
    ToolCue {
        pattern: r"(?i)\b(whole (?:file|repo|repository|module|codebase|project)|entire (?:file|repo|repository|module|codebase|project)|across the (?:whole|entire)|too (?:long|large|big) to (?:fit|read|process)|summari[sz]e the (?:repo|repository|codebase|whole (?:file|module)))\b",
        family: ToolFamily::LargeContext,
        weight: 3.0,
    },
    ToolCue {
        pattern: r"(?i)\b(remember (?:that|this|to)|from now on|note that|keep in mind|for future reference|don'?t forget|make a note)\b",
        family: ToolFamily::MemoryWrite,
        weight: 3.0,
    },
    ToolCue {
        pattern: r"(?i)\b(have we (?:done|tried|decided|discussed)|did we (?:decide|discuss|do)|what did we|last time|previously|prior (?:decision|discussion|work)|as we (?:discussed|agreed)|do you (?:recall|remember))\b",
        family: ToolFamily::MemoryRead,
        weight: 3.0,
    },
    ToolCue {
        pattern: r"(?i)\b(track (?:this|these|it|the .{0,20}?work)|create (?:a |an )?(?:plan|task|backlog|epic|work item|milestone)|break (?:this|it|them) (?:down|into (?:tasks|steps|subtasks))|to[- ]?do list|work items?|hand (?:this |it )?off)\b",
        family: ToolFamily::WorkItem,
        weight: 3.0,
    },
];

fn compiled_tool_cues() -> &'static [(Regex, ToolFamily, f32)] {
    static CELL: OnceLock<Vec<(Regex, ToolFamily, f32)>> = OnceLock::new();
    CELL.get_or_init(|| {
        TOOL_CUES
            .iter()
            .map(|c| {
                (
                    Regex::new(c.pattern).expect("tool-cue regex compiles"),
                    c.family,
                    c.weight,
                )
            })
            .collect()
    })
}

lazy_regex!(fenced_code_re, r"(?s)```.*?```");
lazy_regex!(inline_code_re, r"`[^`]*`");

/// Blank fenced and inline code spans so the classifier doesn't fire on pasted
/// code that happens to contain a trigger word (e.g. a code block mentioning
/// "remember"). Offsets need not be preserved — the classifier only `is_match`es.
/// (Distinct from the mandate extractor's `mask_exclusions`, which must NOT strip
/// inline `code` — that would drop its back-tick target capture.)
fn strip_code(text: &str) -> String {
    let no_fences = fenced_code_re().replace_all(text, " ");
    inline_code_re().replace_all(&no_fences, " ").into_owned()
}

/// Classify a prompt into AT MOST ONE tool-family suggestion (the highest-weight
/// match; ties broken by `TOOL_CUES` order). Regex-only — same cost class as
/// [`extract_mandates`]. Returns `None` when no cue fires. Fenced / inline code
/// spans are stripped first so the classifier doesn't trigger on pasted code.
pub fn classify_tool_suggestion(prompt: &str) -> Option<ToolFamily> {
    if prompt.trim().is_empty() {
        return None;
    }
    let masked = strip_code(prompt);
    let mut best: Option<(f32, ToolFamily)> = None;
    for (re, family, weight) in compiled_tool_cues() {
        if re.is_match(&masked) {
            let better = best.is_none_or(|(w, _)| *weight > w);
            if better {
                best = Some((*weight, *family));
            }
        }
    }
    best.map(|(_, f)| f)
}

/// The single nudge line appended to `additional_context` for a classified
/// family. `brief` (codex) trades the rationale for tokens. Uses geometric
/// glyphs, not emoji, per the rendering policy.
pub fn tool_suggestion_nudge(family: ToolFamily, brief: bool) -> String {
    match (family, brief) {
        (ToolFamily::Collaboration, false) => "▸ Looks collaborative — consider a2a_pattern_deliberation (hard problems, iterate), a2a_pattern_mixture (parallel specialists), or a2a_send_task to delegate to a peer; csm_validate_run(task_id) after a pattern run.".to_string(),
        (ToolFamily::Collaboration, true) => "▸ a2a_pattern_* / a2a_send_task available for multi-agent work.".to_string(),
        (ToolFamily::LargeContext, false) => "▸ Beyond one pass — a2a_pattern_recursive decomposes a whole file/module/repo and stitches the answer (rlm_depth / rlm_budget tunable).".to_string(),
        (ToolFamily::LargeContext, true) => "▸ a2a_pattern_recursive for whole-file/repo questions.".to_string(),
        (ToolFamily::MemoryWrite, false) => "▸ Persist this durably — memory_create_entities + memory_add_observations (survives across sessions; recall later with memory_unified_search).".to_string(),
        (ToolFamily::MemoryWrite, true) => "▸ memory_add_observations to persist durable facts.".to_string(),
        (ToolFamily::MemoryRead, false) => "▸ Recall prior context first — memory_unified_search / recall_prompts / search_mandates before re-deriving.".to_string(),
        (ToolFamily::MemoryRead, true) => "▸ memory_unified_search / recall_prompts to recall prior context.".to_string(),
        (ToolFamily::WorkItem, false) => "▸ Track multi-step work — work_item_create (or work_item_ingest_plan to ingest a plan), work_item_claim_next / work_item_handoff for cross-agent.".to_string(),
        (ToolFamily::WorkItem, true) => "▸ work_item_create / work_item_ingest_plan to track multi-step work.".to_string(),
    }
}

/// Stable lowercase key for a family (used in the `nudge_emissions` log and the
/// per-(session, family) rate limit).
pub fn tool_family_key(family: ToolFamily) -> &'static str {
    match family {
        ToolFamily::Collaboration => "collaboration",
        ToolFamily::LargeContext => "large_context",
        ToolFamily::MemoryWrite => "memory_write",
        ToolFamily::MemoryRead => "memory_read",
        ToolFamily::WorkItem => "work_item",
    }
}

/// True if `(session_id, family)` was nudged within the last `ttl_secs`.
pub async fn recently_nudged(
    pool: &PgPool,
    session_id: &str,
    family: &str,
    ttl_secs: i64,
) -> Result<bool, sqlx::Error> {
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM nudge_emissions
         WHERE session_id = $1 AND family = $2
           AND ts > now() - ($3::bigint * interval '1 second')
         LIMIT 1",
    )
    .bind(session_id)
    .bind(family)
    .bind(ttl_secs.max(0))
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Lifetime count of nudges of `family` emitted in this session.
pub async fn session_nudge_count(
    pool: &PgPool,
    session_id: &str,
    family: &str,
) -> Result<i64, sqlx::Error> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::int8 FROM nudge_emissions WHERE session_id = $1 AND family = $2",
    )
    .bind(session_id)
    .bind(family)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Record a nudge emission (for rate-limiting + the Phase-3 conversion metric).
pub async fn insert_nudge_emission(
    pool: &PgPool,
    session_id: &str,
    prompt_id: Option<i64>,
    family: &str,
    channel: &str,
    client_name: Option<&str>,
    project_id: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO nudge_emissions
            (session_id, prompt_id, family, channel, client_name, project_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(session_id)
    .bind(prompt_id)
    .bind(family)
    .bind(channel)
    .bind(client_name)
    .bind(project_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ============================================================================
// Markdown rendering
// ============================================================================

/// Render an active-mandate set as a compact Markdown block. Ranks by
/// `(cue_tier DESC, last_reinforced_at DESC, salience DESC)`, capped at
/// `cap_bytes`. Truncates oldest/lowest-tier first.
pub fn render_session_mandates_md(mandates: &[SessionMandate], cap_bytes: usize) -> String {
    if mandates.is_empty() {
        return String::new();
    }
    let mut ranked: Vec<&SessionMandate> = mandates.iter().collect();
    ranked.sort_by(|a, b| {
        b.cue_tier
            .cmp(&a.cue_tier)
            .then(b.last_reinforced_at.cmp(&a.last_reinforced_at))
            .then(
                b.salience
                    .partial_cmp(&a.salience)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    let mut out = String::from("## Active session mandates (pgmcp)\n\n");
    for m in ranked {
        let polarity_label = match MandatePolarity::parse(&m.polarity) {
            Some(p) => format!("{:?}", p),
            None => m.polarity.clone(),
        };
        let target = m
            .target
            .as_deref()
            .map(|t| format!(" (`{}`)", t))
            .unwrap_or_default();
        let line = format!(
            "- **{}**{}: {} _(reinforced ×{})_\n",
            polarity_label, target, m.imperative, m.reinforcement_count
        );
        if out.len() + line.len() > cap_bytes {
            break;
        }
        out.push_str(&line);
    }
    out
}

// ============================================================================
// Utility helpers
// ============================================================================

pub fn prompt_sha256(prompt: &str) -> String {
    let digest = Sha256::digest(prompt.as_bytes());
    format!("{:x}", digest)
}

// ============================================================================
// DB helpers
// ============================================================================

pub async fn upsert_session(
    pool: &PgPool,
    id: Uuid,
    cwd: &str,
    project_id: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO sessions (id, cwd, project_id, first_seen, last_seen)
         VALUES ($1, $2, $3, NOW(), NOW())
         ON CONFLICT (id) DO UPDATE SET
            cwd = EXCLUDED.cwd,
            project_id = EXCLUDED.project_id,
            last_seen = NOW()",
    )
    .bind(id)
    .bind(cwd)
    .bind(project_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn insert_prompt(
    pool: &PgPool,
    session_id: Uuid,
    text: &str,
    sha256: &str,
    embedding: Option<&[f32]>,
) -> Result<i64, sqlx::Error> {
    // BGE-M3/1024-only: write the embedding to `embedding_v2` and stamp
    // the active signature. NULL embeddings stay NULL on `embedding_v2`
    // (the cron will pick them up to fill it once a model is configured).
    // Any non-1024 vector is rejected up front — the legacy 384/MiniLM
    // path and its `embedding` column have been removed.
    if let Some(n) = embedding.map(<[f32]>::len)
        && n != 1024
    {
        return Err(sqlx::Error::Protocol(format!(
            "insert_prompt: unsupported embedding dim {n} \
             (expected a 1024-dimension BGE-M3 embedding, got {n})"
        )));
    }
    let id: i64 = match embedding {
        Some(values) => {
            let vector = Vector::from(values.to_vec());
            sqlx::query_scalar(
                "INSERT INTO session_prompts
                    (session_id, prompt_text, prompt_sha256,
                     embedding_v2, embedding_signature)
                 VALUES ($1, $2, $3, $4, 'bge-m3-v1')
                 ON CONFLICT (session_id, prompt_sha256) DO UPDATE SET ts = NOW()
                 RETURNING id",
            )
            .bind(session_id)
            .bind(text)
            .bind(sha256)
            .bind(vector)
            .fetch_one(pool)
            .await?
        }
        None => {
            sqlx::query_scalar(
                "INSERT INTO session_prompts (session_id, prompt_text, prompt_sha256)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (session_id, prompt_sha256) DO UPDATE SET ts = NOW()
                 RETURNING id",
            )
            .bind(session_id)
            .bind(text)
            .bind(sha256)
            .fetch_one(pool)
            .await?
        }
    };
    Ok(id)
}

pub async fn upsert_mandate(
    pool: &PgPool,
    session_id: Uuid,
    source_prompt_id: i64,
    m: &ExtractedMandate,
) -> Result<i64, sqlx::Error> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO session_mandates
            (session_id, source_prompt_id, polarity, imperative, target, cwd_prefix,
             cue_tier, salience, status, created_at, last_reinforced_at, reinforcement_count)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'active', NOW(), NOW(), 1)
         ON CONFLICT (session_id, polarity, lower(imperative)) DO UPDATE SET
            last_reinforced_at = NOW(),
            reinforcement_count = session_mandates.reinforcement_count + 1,
            salience = GREATEST(session_mandates.salience, EXCLUDED.salience),
            source_prompt_id = EXCLUDED.source_prompt_id,
            target = COALESCE(EXCLUDED.target, session_mandates.target),
            cwd_prefix = COALESCE(EXCLUDED.cwd_prefix, session_mandates.cwd_prefix),
            cue_tier = CASE WHEN EXCLUDED.cue_tier < session_mandates.cue_tier
                            THEN EXCLUDED.cue_tier ELSE session_mandates.cue_tier END
         RETURNING id",
    )
    .bind(session_id)
    .bind(source_prompt_id)
    .bind(m.polarity.as_str())
    .bind(&m.imperative)
    .bind(m.target.as_deref())
    .bind(m.cwd_prefix.as_deref())
    .bind(m.cue_tier.as_char().to_string())
    .bind(m.salience)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Memory-server Phase 0: mark active session-mandate near-duplicates of the
/// newly upserted mandate as `Superseded` and link them via
/// `source_mandate_id`.
///
/// Near-duplicate = same `session_id` and `polarity`, different `lower(imperative)`
/// (the exact match path is already handled by the UNIQUE constraint in
/// `upsert_mandate`), with Damerau-Levenshtein distance ≤ `max_distance` on
/// the lowercased imperatives. The dedup runs in-process via a
/// `liblevenshtein::Transducer` over a `DynamicDawgChar` built from the
/// session's current active imperatives — no Postgres extension required
/// (the legacy `fuzzystrmatch` path is gone, see the integration plan
/// `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md` Phase 3).
///
/// `max_distance` should be small (default 3): we are de-duplicating
/// near-identical phrasings ("use rust" vs "use Rust", "use_rust"), not
/// merging semantically distinct mandates. Cross-imperative pairs that
/// happen to have a small edit distance but mean different things would
/// be a false positive — accept this for Phase 0; Phase 4 will replace
/// the regex+Levenshtein pipeline with the LLM extractor entirely.
///
/// Returns the number of rows marked superseded.
pub async fn mark_near_duplicate_superseded(
    pool: &PgPool,
    session_id: Uuid,
    keeper_id: i64,
    polarity: &str,
    imperative: &str,
    max_distance: i32,
) -> Result<u64, sqlx::Error> {
    use libdictenstein::dynamic_dawg_char::DynamicDawgChar;
    use liblevenshtein::transducer::Transducer;

    // 1. Pull candidate active mandates for the same session + polarity.
    //    Exact-case-insensitive duplicates are excluded server-side
    //    (`lower(imperative) <> lower($4)`); approximate matches are
    //    selected in-process via the Levenshtein transducer.
    let rows: Vec<(i64, String)> = sqlx::query_as::<_, (i64, String)>(
        "SELECT id, imperative FROM session_mandates
          WHERE session_id = $1
            AND status = 'active'
            AND id <> $2
            AND polarity = $3
            AND lower(imperative) <> lower($4)",
    )
    .bind(session_id)
    .bind(keeper_id)
    .bind(polarity)
    .bind(imperative)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(0);
    }

    // 2. Build a `DynamicDawgChar` keyed by lowercase imperative; remember
    //    each term's row ids so the transducer match can be mapped back.
    let mut id_index: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::with_capacity(rows.len());
    for (id, imp) in &rows {
        id_index.entry(imp.to_lowercase()).or_default().push(*id);
    }
    let terms: Vec<&str> = id_index.keys().map(|s| s.as_str()).collect();
    let dict: DynamicDawgChar<()> = DynamicDawgChar::from_terms(terms);
    let transducer = Transducer::with_transposition(dict);

    // 3. Query the transducer for terms within `max_distance` of the new
    //    imperative; collect the row ids of all near-duplicate mandates.
    let new_lower = imperative.to_lowercase();
    let max = max_distance.max(0) as usize;
    let mut superseded_ids: Vec<i64> = Vec::new();
    for candidate in transducer.query_with_distance(&new_lower, max) {
        if let Some(ids) = id_index.get(&candidate.term) {
            superseded_ids.extend(ids.iter().copied());
        }
    }

    if superseded_ids.is_empty() {
        return Ok(0);
    }

    // 4. Single bulk UPDATE.
    let result = sqlx::query(
        "UPDATE session_mandates
            SET status = 'superseded'
          WHERE id = ANY($1)",
    )
    .bind(&superseded_ids)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub async fn list_active_mandates(
    pool: &PgPool,
    session_id: Option<Uuid>,
    cwd: Option<&str>,
    limit: i32,
) -> Result<Vec<SessionMandate>, sqlx::Error> {
    let limit = limit.clamp(1, 100);
    match (session_id, cwd) {
        (Some(sid), _) => {
            sqlx::query_as::<_, SessionMandate>(
                "SELECT * FROM session_mandates
             WHERE session_id = $1 AND status = 'active'
             ORDER BY cue_tier DESC, last_reinforced_at DESC, salience DESC
             LIMIT $2",
            )
            .bind(sid)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (None, Some(cwd)) => {
            sqlx::query_as::<_, SessionMandate>(
                "SELECT m.* FROM session_mandates m
             JOIN sessions s ON s.id = m.session_id
             WHERE s.cwd = $1 AND m.status = 'active'
             ORDER BY m.cue_tier DESC, m.last_reinforced_at DESC, m.salience DESC
             LIMIT $2",
            )
            .bind(cwd)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (None, None) => Ok(Vec::new()),
    }
}

/// Manually retire a session mandate. Exposed for the future
/// `session-mandate-refinement` cron and for integration tests; the
/// production hook path does not call it yet.
#[allow(dead_code)]
pub async fn retire_mandate(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE session_mandates SET status = 'retired' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_mandate(pool: &PgPool, id: i64) -> Result<Option<SessionMandate>, sqlx::Error> {
    sqlx::query_as::<_, SessionMandate>("SELECT * FROM session_mandates WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn promote_mandate(
    pool: &PgPool,
    id: i64,
    scope: &str,
    project_id: Option<i32>,
    file_path: Option<&str>,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mandate = sqlx::query_as::<_, SessionMandate>(
        "SELECT * FROM session_mandates WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;

    let durable_id: i64 = sqlx::query_scalar(
        "INSERT INTO durable_mandates
            (scope, project_id, polarity, imperative, target, source_mandate_id, file_path)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id",
    )
    .bind(scope)
    .bind(project_id)
    .bind(&mandate.polarity)
    .bind(&mandate.imperative)
    .bind(mandate.target.as_deref())
    .bind(mandate.id)
    .bind(file_path)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query("UPDATE session_mandates SET status = 'promoted' WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(durable_id)
}

pub async fn list_durable_mandates_for_project(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<DurableMandate>, sqlx::Error> {
    sqlx::query_as::<_, DurableMandate>(
        "SELECT * FROM durable_mandates
         WHERE project_id = $1 OR scope = 'workspace'
         ORDER BY promoted_at DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Tests (pure-function extractor)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn polarities(ms: &[ExtractedMandate]) -> Vec<MandatePolarity> {
        ms.iter().map(|m| m.polarity).collect()
    }

    // ── Tool-suggestion classifier ──────────────────────────────────────

    #[test]
    fn classifies_representative_prompts() {
        assert_eq!(
            classify_tool_suggestion("Can you get a second opinion on this design?"),
            Some(ToolFamily::Collaboration)
        );
        assert_eq!(
            classify_tool_suggestion("summarize the whole repo for me"),
            Some(ToolFamily::LargeContext)
        );
        assert_eq!(
            classify_tool_suggestion("Remember that we use BF16 for inference from now on"),
            Some(ToolFamily::MemoryWrite)
        );
        assert_eq!(
            classify_tool_suggestion("have we done this migration before?"),
            Some(ToolFamily::MemoryRead)
        );
        assert_eq!(
            classify_tool_suggestion("break this down into tasks and track it"),
            Some(ToolFamily::WorkItem)
        );
    }

    #[test]
    fn no_false_positive_on_plain_prompts() {
        assert_eq!(classify_tool_suggestion("fix the typo in line 42"), None);
        assert_eq!(classify_tool_suggestion(""), None);
        assert_eq!(
            classify_tool_suggestion("run the tests and show me the output"),
            None
        );
    }

    #[test]
    fn quoted_text_is_masked() {
        // The cue sits inside a fenced code block → masked → no suggestion.
        let p = "here is code:\n```\nremember that x = 1 from now on\n```\nplease format it";
        assert_eq!(classify_tool_suggestion(p), None);
    }

    #[test]
    fn nudge_text_is_per_client() {
        let full = tool_suggestion_nudge(ToolFamily::Collaboration, false);
        let brief = tool_suggestion_nudge(ToolFamily::Collaboration, true);
        assert!(full.len() > brief.len());
        assert!(full.contains("a2a_pattern_deliberation"));
        assert!(brief.contains("a2a_"));
    }

    #[test]
    fn tier_a_no_gate_never_again() {
        let p = "Never make destructive changes without my explicit approval again!";
        let ms = extract_mandates(p, None);
        assert!(!ms.is_empty(), "expected at least one mandate: {:?}", ms);
        assert!(polarities(&ms).contains(&MandatePolarity::Never));
    }

    #[test]
    fn tier_a_no_gate_non_negotiable() {
        let p = "This is non-negotiable.";
        let ms = extract_mandates(p, None);
        assert!(polarities(&ms).contains(&MandatePolarity::Mandate));
    }

    #[test]
    fn tier_b_without_gate_is_dropped_do_not_replan() {
        let p = "Do not re-plan; just resume.";
        let ms = extract_mandates(p, None);
        // No standing-scope qualifier => task-local => dropped
        assert!(
            ms.is_empty() || ms.iter().all(|m| m.polarity == MandatePolarity::Correction),
            "expected drop, got {:?}",
            ms
        );
    }

    #[test]
    fn tier_b_without_gate_ensure_is_dropped() {
        let p = "Ensure the close ) is consumed.";
        let ms = extract_mandates(p, None);
        assert!(
            ms.is_empty(),
            "ensure without gate should drop, got {:?}",
            ms
        );
    }

    #[test]
    fn tier_b_with_punct_gate_dont_gaslight() {
        let p = "don't fucking gaslight me!!";
        let ms = extract_mandates(p, None);
        assert!(
            !ms.is_empty(),
            "expected mandate via punctuation gate: {:?}",
            ms
        );
    }

    #[test]
    fn tier_c_correction_i_asked_you() {
        let p = "I asked you to review your proposal before implementing.";
        let ms = extract_mandates(p, None);
        assert!(polarities(&ms).contains(&MandatePolarity::Correction));
    }

    #[test]
    fn tier_c_correction_you_keep() {
        let p = "Why do you keep discarding work?";
        let ms = extract_mandates(p, None);
        assert!(polarities(&ms).contains(&MandatePolarity::Correction));
    }

    #[test]
    fn exclusion_filter_stop_the_daemon() {
        let p = "Please stop the daemon.";
        let ms = extract_mandates(p, None);
        assert!(
            ms.is_empty(),
            "task-local stop request should drop, got {:?}",
            ms
        );
    }

    #[test]
    fn exclusion_filter_never_returns_null() {
        let p = "This function never returns null.";
        let ms = extract_mandates(p, None);
        // Code-behavior description; the "never" cue must be masked out.
        assert!(
            !ms.iter().any(|m| m.polarity == MandatePolarity::Never),
            "code-behavior never should be filtered: {:?}",
            ms
        );
    }

    #[test]
    fn exclusion_filter_required_parameter() {
        let p = "required parameter foo";
        let ms = extract_mandates(p, None);
        assert!(ms.is_empty(), "required-parameter should drop: {:?}", ms);
    }

    #[test]
    fn companion_backtick_capture() {
        // Use raw string so the back-ticks are real (not escape-eaten).
        let p = r#"always prefer `.expect(...)` over `unwrap()`."#;
        let ms = extract_mandates(p, None);
        let prefer = ms
            .iter()
            .find(|m| m.polarity == MandatePolarity::Prefer)
            .or_else(|| ms.iter().find(|m| m.polarity == MandatePolarity::Always));
        assert!(prefer.is_some(), "expected prefer/always mandate: {:?}", ms);
        if let Some(m) = prefer {
            assert!(m.target.is_some(), "expected back-tick target: {:?}", m);
        }
    }

    #[test]
    fn companion_all_caps_boost() {
        let p = "MUST FIX ALL FAILURES REGARDLESS";
        let ms = extract_mandates(p, None);
        assert!(!ms.is_empty());
        // Mandate cue is Tier D requires gate; ALL-CAPS span fires gate.
        // Salience should be > base prior (5.0 for "must"); boosted ×1.3 caps min at 6.5.
        assert!(
            ms.iter().any(|m| m.salience >= 5.0),
            "expected boosted salience, got {:?}",
            ms
        );
    }

    #[test]
    fn companion_bullet_lead_boost() {
        let p = "- Always prefer pattern matching to conditionals.";
        let ms = extract_mandates(p, None);
        assert!(!ms.is_empty(), "expected bullet-prefixed always: {:?}", ms);
    }

    #[test]
    fn sentence_split_not_on_comma() {
        // The corpus has many comma-continuations carrying standing rules.
        // Tier-F covers "always X before benchmarking" (benchmark is in verb list).
        let p = "Use the profiler, always benchmark before optimizing.";
        let ms = extract_mandates(p, None);
        assert!(!ms.is_empty(), "comma-continuation lost: {:?}", ms);
        assert!(
            ms.iter().any(|m| matches!(
                m.polarity,
                MandatePolarity::ProcessRule | MandatePolarity::Always
            )),
            "expected ProcessRule/Always from second clause: {:?}",
            ms
        );
    }

    #[test]
    fn cwd_prefix_scope_hint() {
        let p = "In this project, never use unsafe.";
        let ms = extract_mandates(p, Some("/home/me/proj"));
        // The "never use" matches Tier A explicitly, but check scope hint propagates.
        assert!(
            ms.iter()
                .any(|m| m.cwd_prefix.as_deref() == Some("/home/me/proj")),
            "expected cwd_prefix populated: {:?}",
            ms
        );
    }

    #[test]
    fn empty_prompt_yields_nothing() {
        assert!(extract_mandates("", None).is_empty());
        assert!(extract_mandates("   \n  ", None).is_empty());
    }

    #[test]
    fn polarity_round_trip() {
        for p in [
            MandatePolarity::Always,
            MandatePolarity::Never,
            MandatePolarity::Prefer,
            MandatePolarity::Avoid,
            MandatePolarity::Remember,
            MandatePolarity::FromNowOn,
            MandatePolarity::Correction,
            MandatePolarity::Permission,
            MandatePolarity::Constraint,
            MandatePolarity::Mandate,
            MandatePolarity::ProcessRule,
            MandatePolarity::ProjectRule,
        ] {
            assert_eq!(MandatePolarity::parse(p.as_str()), Some(p));
        }
    }

    #[test]
    fn sha256_stable() {
        let h1 = prompt_sha256("hello");
        let h2 = prompt_sha256("hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn render_empty_is_empty_string() {
        assert!(render_session_mandates_md(&[], 2048).is_empty());
    }
}
