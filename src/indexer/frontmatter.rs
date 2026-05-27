//! Minimal YAML frontmatter parsing for indexed documents.
//!
//! Recognizes a leading `---\n … \n---\n` block of flat `key: value` scalar
//! pairs (the subset the rendered scientific ledgers emit — see
//! `crate::experiment::ledger`). This is deliberately NOT a full YAML parser:
//! no nesting, lists, or multi-line scalars. The goal is to (a) let a
//! committed ledger round-trip to its DB row via `pgmcp_experiment: <slug>`,
//! and (b) strip the frontmatter from the body so heading-aware chunking sees
//! the real `## …` structure rather than the `---` fence.
//!
//! No new dependency: the project's sqlx build has no `serde_yaml`, and a flat
//! key/value scan is all the ledger format needs.

use std::collections::BTreeMap;

/// A parsed frontmatter block plus the remaining document body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frontmatter {
    /// Flat scalar key/value pairs (insertion order not preserved; lookups by key).
    pub fields: BTreeMap<String, String>,
    /// The document content with the frontmatter block removed.
    pub body: String,
}

impl Frontmatter {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }
    /// The `pgmcp_experiment` slug, if this document is a rendered ledger.
    pub fn experiment_slug(&self) -> Option<&str> {
        self.get("pgmcp_experiment")
    }
}

/// Split a leading `---` frontmatter block from `content`. When there is no
/// well-formed leading block, returns empty `fields` and the original content
/// as `body` (so callers can always use `.body`).
pub fn parse(content: &str) -> Frontmatter {
    // Must start with a `---` fence on the very first line. Tolerate a UTF-8 BOM.
    let trimmed = content.strip_prefix('\u{feff}').unwrap_or(content);
    let after_open = match trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
    {
        Some(rest) => rest,
        None => {
            return Frontmatter {
                fields: BTreeMap::new(),
                body: content.to_string(),
            };
        }
    };

    // Find the closing fence: a line that is exactly `---`.
    let mut fields = BTreeMap::new();
    let mut body_start: Option<usize> = None;
    let mut offset = 0usize; // byte offset within `after_open`
    for line in after_open.split_inclusive('\n') {
        let line_trimmed = line.trim_end_matches(['\n', '\r']);
        if line_trimmed == "---" {
            body_start = Some(offset + line.len());
            break;
        }
        if let Some((k, v)) = line_trimmed.split_once(':') {
            let key = k.trim();
            if !key.is_empty() {
                fields.insert(key.to_string(), v.trim().to_string());
            }
        }
        offset += line.len();
    }

    match body_start {
        Some(start) => {
            // Skip a single trailing newline after the closing fence.
            let body = after_open[start..]
                .strip_prefix('\n')
                .or_else(|| after_open[start..].strip_prefix("\r\n"))
                .unwrap_or(&after_open[start..])
                .to_string();
            Frontmatter { fields, body }
        }
        // No closing fence → not frontmatter; treat the whole thing as body.
        None => Frontmatter {
            fields: BTreeMap::new(),
            body: content.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_block_and_strips_body() {
        let doc = "---\npgmcp_experiment: arena-alloc\ntitle: Arena alloc\nkind: optimization\n---\n# Heading\n\nbody text\n";
        let fm = parse(doc);
        assert_eq!(fm.experiment_slug(), Some("arena-alloc"));
        assert_eq!(fm.get("kind"), Some("optimization"));
        assert!(fm.body.starts_with("# Heading"));
        assert!(!fm.body.contains("pgmcp_experiment"));
    }

    #[test]
    fn no_frontmatter_returns_whole_body() {
        let doc = "# Just a doc\n\nno frontmatter here\n";
        let fm = parse(doc);
        assert!(fm.fields.is_empty());
        assert_eq!(fm.body, doc);
    }

    #[test]
    fn unterminated_fence_is_not_frontmatter() {
        let doc = "---\nkey: value\nno closing fence\n";
        let fm = parse(doc);
        assert!(fm.fields.is_empty());
        assert_eq!(fm.body, doc);
    }

    #[test]
    fn values_with_colons_keep_remainder() {
        let doc = "---\nplan: ~/.claude/plans/x.md\nurl: https://example.com\n---\nbody\n";
        let fm = parse(doc);
        assert_eq!(fm.get("plan"), Some("~/.claude/plans/x.md"));
        assert_eq!(fm.get("url"), Some("https://example.com"));
    }
}
