//! Parsing the `#<public_id>` / `fixes|closes|resolves <public_id>` convention
//! out of commit messages and PR text — shared by the git indexer's
//! auto-linkage ([`crate::indexer::git_indexer`]) and the REST `pr_event`
//! handler ([`crate::api::handlers`]).
//!
//! A work item's `public_id` is a kebab slug plus a short hex suffix
//! (`my-task-3f9a1c`; see `crate::mcp::tools::work_items::gen_public_id`), so
//! the grammar of a reference is `[a-z0-9][a-z0-9-]+` — a lowercase
//! alphanumeric/hyphen token of length ≥ 2.
//!
//! Two reference forms are recognized:
//!   1. a bare hash mention — `#my-task-3f9a1c` — a *touch* (links + advances
//!      the item to `in_progress`);
//!   2. a closing verb — `fixes my-task-3f9a1c`, `closes #my-task-3f9a1c`,
//!      `resolves`, `implements`, `ref`/`refs` — a *closing touch* (links +
//!      advances to `claimed_done`).
//!
//! [`extract_public_ids`] returns every distinct id mentioned (in first-seen
//! order); [`is_closing_ref`] reports whether the text contains a *closing*
//! reference to a specific id (so the indexer can pick the stronger transition).
//!
//! TRUST NOTE: the resulting transition always runs as `Actor::Agent` and can
//! at most reach `claimed_done`/`verifying` (a verify *candidate*) — never
//! `verified`. See [`crate::tracker::auto_transition`].

use std::sync::OnceLock;

use regex::Regex;

/// A `public_id` token: lowercase alphanumeric, then alphanumeric/hyphen, total
/// length ≥ 2. Matches `gen_public_id`'s slug-plus-hex-suffix output.
const PUBLIC_ID_TOKEN: &str = r"[a-z0-9][a-z0-9-]+";

/// A `public_id` token that is *unambiguously an id* even without a leading
/// `#`: it must contain at least one hyphen (every `gen_public_id` output is
/// `slug-hexsuffix`, so it always has ≥1 hyphen). This disambiguates a closing
/// verb followed by an id (`fixes my-task-3f9a1c`) from a closing verb followed
/// by an ordinary English word (`fixes the bug` must NOT capture `the`). The
/// hash-prefixed form (`fixes #anything`) accepts any token shape.
const HYPHENATED_PUBLIC_ID_TOKEN: &str = r"[a-z0-9]+(?:-[a-z0-9]+)+";

/// The closing verbs that, when they precede a reference, mark it as a
/// fix/close (→ `claimed_done`) rather than a bare touch (→ `in_progress`).
/// `ref`/`refs` are included as soft closers (GitHub treats `ref` as a mention,
/// but in the tracker's agent-grade model an explicit verb is a stronger signal
/// than a bare `#hash`, so we map all listed verbs to "closing").
const CLOSING_VERBS: &[&str] = &[
    "fixes",
    "fixed",
    "fix",
    "closes",
    "closed",
    "close",
    "resolves",
    "resolved",
    "resolve",
    "implements",
    "implemented",
    "implement",
    "refs",
    "ref",
];

/// Matches a bare-hash reference `#<public_id>`. Capture group 1 is the id.
fn bare_ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"(?i)#({PUBLIC_ID_TOKEN})")).expect("bare-ref regex"))
}

/// Matches a closing-verb reference `(fixes|closes|…)\s+#?<public_id>`. Capture
/// group 1 is the verb, group 2 is the id. The `#` between the verb and the id
/// is optional.
fn closing_ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let verbs = CLOSING_VERBS.join("|");
        // Word-boundary verb, whitespace/colon, then EITHER `#<any-token>` OR a
        // bare hyphenated id. The bare form must be hyphenated so `fixes the
        // bug` does not capture the ordinary word `the`; `fixes #anything`
        // accepts any token shape. Capture group 2 is the id in both branches.
        Regex::new(&format!(
            r"(?i)\b({verbs})\b[:\s]+(?:#({PUBLIC_ID_TOKEN})|({HYPHENATED_PUBLIC_ID_TOKEN}))"
        ))
        .expect("closing-ref regex")
    })
}

/// Extract every distinct `public_id` referenced in `text` (commit subject +
/// body, or PR title + body), in first-seen order. Both the bare-hash form and
/// the closing-verb form contribute ids; the returned set is the union with
/// duplicates removed. Lowercased (the convention is lowercase) so a stray
/// uppercase mention still matches a stored id.
pub fn extract_public_ids(text: &str) -> Vec<String> {
    // Preallocate for the common handful of references per message.
    let mut out: Vec<String> = Vec::with_capacity(4);
    let mut push = |id: &str| {
        let id = id.to_ascii_lowercase();
        if !out.contains(&id) {
            out.push(id);
        }
    };
    for cap in bare_ref_re().captures_iter(text) {
        if let Some(m) = cap.get(1) {
            push(m.as_str());
        }
    }
    for cap in closing_ref_re().captures_iter(text) {
        // The id is group 2 (`#<token>`) or group 3 (bare hyphenated token).
        if let Some(m) = cap.get(2).or_else(|| cap.get(3)) {
            push(m.as_str());
        }
    }
    out
}

/// Whether `text` contains a *closing* reference (a closing verb followed by the
/// id) to `public_id` specifically. The indexer calls this per-linked-id to
/// decide between the bare-touch transition (→ `in_progress`) and the closing
/// transition (→ `claimed_done`). Case-insensitive on both sides.
pub fn is_closing_ref(text: &str, public_id: &str) -> bool {
    let target = public_id.to_ascii_lowercase();
    closing_ref_re().captures_iter(text).any(|cap| {
        cap.get(2)
            .or_else(|| cap.get(3))
            .is_some_and(|m| m.as_str().eq_ignore_ascii_case(&target))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bare_hash_from_subject_and_body() {
        let subject = "fix the parser panic (#parser-panic-3f9a1c)";
        let body = "Also touches #another-task-aa11bb in passing.";
        let text = format!("{subject}\n\n{body}");
        let ids = extract_public_ids(&text);
        assert!(ids.contains(&"parser-panic-3f9a1c".to_string()));
        assert!(ids.contains(&"another-task-aa11bb".to_string()));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn extracts_closing_verb_forms() {
        for verb in [
            "fixes",
            "closes",
            "resolves",
            "implements",
            "Fixes",
            "FIXES",
        ] {
            let text = format!("{verb} my-task-abc123");
            let ids = extract_public_ids(&text);
            assert_eq!(
                ids,
                vec!["my-task-abc123".to_string()],
                "verb {verb} should extract the id"
            );
            assert!(
                is_closing_ref(&text, "my-task-abc123"),
                "verb {verb} is a closing ref"
            );
        }
    }

    #[test]
    fn closing_verb_with_hash_is_recognized() {
        let text = "This change closes #login-bug-deadbe.";
        let ids = extract_public_ids(text);
        assert_eq!(ids, vec!["login-bug-deadbe".to_string()]);
        assert!(is_closing_ref(text, "login-bug-deadbe"));
    }

    #[test]
    fn bare_hash_is_not_a_closing_ref() {
        // A bare `#hash` with no closing verb is a touch, not a close.
        let text = "Work in progress on #refactor-core-001122.";
        let ids = extract_public_ids(text);
        assert_eq!(ids, vec!["refactor-core-001122".to_string()]);
        assert!(
            !is_closing_ref(text, "refactor-core-001122"),
            "a bare #hash is a touch, not a close"
        );
    }

    #[test]
    fn no_match_prose_yields_empty() {
        let text =
            "Refactored the C# bindings and the well-formed HTTP handler; nothing to link here.";
        // `C#` has the `#` AFTER the token, not before, so it does not match the
        // bare-ref form; there is no closing verb either.
        let ids = extract_public_ids(text);
        assert!(
            ids.is_empty(),
            "prose with no #id / closing-verb id must not match, got {ids:?}"
        );
        assert!(!is_closing_ref(text, "anything"));
    }

    #[test]
    fn deduplicates_repeated_mentions() {
        let text = "fixes #dup-task-abcdef and also #dup-task-abcdef again";
        let ids = extract_public_ids(text);
        assert_eq!(
            ids,
            vec!["dup-task-abcdef".to_string()],
            "a repeated id appears once"
        );
    }

    #[test]
    fn closing_verb_does_not_capture_plain_words() {
        // A bare (non-hash) word after a closing verb is only an id when it is
        // hyphenated (every public_id is `slug-hexsuffix`). `fixes the parser`
        // must NOT capture `the` or `parser`.
        for text in [
            "fixes the parser panic in the tokenizer",
            "closes out the milestone",
            "resolve merge conflicts",
            "implement a faster path",
        ] {
            let ids = extract_public_ids(text);
            assert!(
                ids.is_empty(),
                "plain prose after a closing verb must not capture an id, got {ids:?} for {text:?}"
            );
        }
        // But a hyphenated bare id IS captured.
        let ids = extract_public_ids("fixes parser-panic-3f9a1c");
        assert_eq!(ids, vec!["parser-panic-3f9a1c".to_string()]);
        // And a hash-prefixed single word is captured (explicit reference).
        let ids = extract_public_ids("fixes #login");
        assert_eq!(ids, vec!["login".to_string()]);
    }

    #[test]
    fn is_closing_ref_targets_the_specific_id() {
        let text = "fixes #task-a-111111; also touches #task-b-222222";
        assert!(is_closing_ref(text, "task-a-111111"));
        // task-b is only a bare mention, not behind a closing verb.
        assert!(!is_closing_ref(text, "task-b-222222"));
    }
}
