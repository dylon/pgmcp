//! Semgrep-style AST rule engine (graph-roadmap Phase 2.2).
//!
//! Matches dangerous call patterns on the **tree-sitter AST** rather than by
//! line-regex, so a pattern inside a comment or string literal does NOT match
//! (the dominant false-positive source for the regex security tier) and
//! argument structure can be inspected (e.g. `yaml.load` *without* a safe
//! `Loader=`). Rules are Rust code keyed on the resolved callee path + argument
//! shape; languages without a rule set return no hits and callers fall back to
//! the regex scan (Bessey et al. CACM 2010; Semgrep; CodeQL Avgustinov 2016).
//!
//! Python ships first (clearest crypto/deserialization surface); add per
//! language by writing a `scan_<lang>` walker.

use tree_sitter::{Node, Parser};

/// One AST-matched finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstRuleHit {
    pub rule_id: &'static str,
    /// `crypto` or `deserialize` — lets `crypto_misuse` / `unsafe_deserialization`
    /// filter to their category.
    pub category: &'static str,
    pub message: &'static str,
    pub line: u32,
    pub snippet: String,
}

/// Languages with an AST rule set (callers fetch these for AST scanning and
/// leave the rest to the regex fallback). Append as `scan_<lang>` walkers land.
pub const AST_RULE_LANGUAGES: &[&str] = &["python"];

/// `true` when an AST rule set exists for `language` (caller skips the regex
/// fallback for these).
pub fn has_rules(language: &str) -> bool {
    AST_RULE_LANGUAGES.contains(&language)
}

/// Run the AST rules for `language` over `content`. Unsupported languages
/// return `Vec::new()` (caller falls back to regex).
pub fn scan(language: &str, content: &str) -> Vec<AstRuleHit> {
    match language {
        "python" => scan_python(content),
        _ => Vec::new(),
    }
}

fn scan_python(content: &str) -> Vec<AstRuleHit> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(content, None) else {
        return Vec::new();
    };
    let src = content.as_bytes();
    let mut out = Vec::new();
    walk_python(tree.root_node(), src, content, &mut out);
    out
}

fn walk_python(node: Node, src: &[u8], content: &str, out: &mut Vec<AstRuleHit>) {
    if node.kind() == "call"
        && let Some(hit) = classify_python_call(node, src, content)
    {
        out.push(hit);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_python(child, src, content, out);
    }
}

fn classify_python_call(call: Node, src: &[u8], content: &str) -> Option<AstRuleHit> {
    let func = call.child_by_field_name("function")?;
    let callee = dotted_name(func, src)?;
    let line = call.start_position().row as u32 + 1;
    let args_text = call
        .child_by_field_name("arguments")
        .and_then(|a| a.utf8_text(src).ok())
        .unwrap_or("");
    let snippet = || line_text(content, line);

    // Weak hashes (hashlib.md5 / sha1, or bare via `from hashlib import md5`).
    if is_callee(&callee, "md5") {
        return Some(hit(
            "weak_md5",
            "crypto",
            "MD5 is cryptographically broken; use SHA-256 or BLAKE2 for security purposes.",
            line,
            snippet(),
        ));
    }
    if is_callee(&callee, "sha1") {
        return Some(hit(
            "weak_sha1",
            "crypto",
            "SHA-1 is broken; use SHA-256+ for security purposes.",
            line,
            snippet(),
        ));
    }
    // ECB mode (any arg referencing MODE_ECB / an ECB cipher mode).
    if args_text.contains("MODE_ECB") || args_text.contains(".ECB") {
        return Some(hit(
            "ecb_mode",
            "crypto",
            "ECB mode leaks plaintext structure; use an AEAD mode (GCM) or CBC+HMAC.",
            line,
            snippet(),
        ));
    }
    // Unsafe deserialization.
    if callee == "pickle.load" || callee == "pickle.loads" || callee == "cPickle.loads" {
        return Some(hit(
            "pickle_load",
            "deserialize",
            "pickle.load/loads executes arbitrary code on untrusted data (CWE-502).",
            line,
            snippet(),
        ));
    }
    if callee == "marshal.loads" || callee == "marshal.load" {
        return Some(hit(
            "marshal_load",
            "deserialize",
            "marshal.load/loads is unsafe on untrusted data (CWE-502).",
            line,
            snippet(),
        ));
    }
    // yaml.load WITHOUT a safe Loader= is unsafe; yaml.safe_load is fine.
    if callee == "yaml.load" && !args_text.contains("Loader") {
        return Some(hit(
            "yaml_load_unsafe",
            "deserialize",
            "yaml.load without Loader=SafeLoader can execute arbitrary code; use yaml.safe_load.",
            line,
            snippet(),
        ));
    }
    None
}

/// Match a (possibly dotted) callee against a final-segment method name —
/// `md5` matches `hashlib.md5`, `m.md5`, and bare `md5`.
fn is_callee(callee: &str, name: &str) -> bool {
    callee == name || callee.ends_with(&format!(".{name}"))
}

/// Render a tree-sitter Python `function` node as a dotted path
/// (`hashlib.md5`, `yaml.load`, `eval`). Returns `None` for dynamic callees.
fn dotted_name(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node.utf8_text(src).ok().map(|s| s.to_string()),
        "attribute" => {
            let obj = node.child_by_field_name("object")?;
            let attr = node.child_by_field_name("attribute")?;
            let obj_s = dotted_name(obj, src)?;
            let attr_s = attr.utf8_text(src).ok()?;
            Some(format!("{obj_s}.{attr_s}"))
        }
        _ => None,
    }
}

fn line_text(content: &str, line: u32) -> String {
    content
        .lines()
        .nth((line.saturating_sub(1)) as usize)
        .unwrap_or("")
        .trim()
        .chars()
        .take(200)
        .collect()
}

fn hit(
    rule_id: &'static str,
    category: &'static str,
    message: &'static str,
    line: u32,
    snippet: String,
) -> AstRuleHit {
    AstRuleHit {
        rule_id,
        category,
        message,
        line,
        snippet,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule_ids(src: &str) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = scan("python", src).into_iter().map(|h| h.rule_id).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn flags_real_weak_hash_and_pickle() {
        let src = "import hashlib, pickle\nh = hashlib.md5(data)\nobj = pickle.loads(blob)\n";
        assert_eq!(rule_ids(src), vec!["pickle_load", "weak_md5"]);
    }

    #[test]
    fn does_not_match_in_comments_or_strings() {
        // The whole point of AST matching: these must NOT flag.
        let src = "x = \"never call pickle.loads on untrusted input\"\n# hashlib.md5 is weak, avoid it\ny = 1\n";
        assert!(
            scan("python", src).is_empty(),
            "comment/string must not match"
        );
    }

    #[test]
    fn yaml_load_loader_arg_is_safe() {
        let unsafe_src = "import yaml\nd = yaml.load(f)\n";
        assert_eq!(rule_ids(unsafe_src), vec!["yaml_load_unsafe"]);
        let safe_src = "import yaml\nd = yaml.load(f, Loader=yaml.SafeLoader)\n";
        assert!(scan("python", safe_src).is_empty(), "Loader= is safe");
        let safe2 = "import yaml\nd = yaml.safe_load(f)\n";
        assert!(scan("python", safe2).is_empty(), "safe_load is safe");
    }

    #[test]
    fn ecb_mode_flagged() {
        let src = "from Crypto.Cipher import AES\nc = AES.new(key, AES.MODE_ECB)\n";
        assert_eq!(rule_ids(src), vec!["ecb_mode"]);
    }

    #[test]
    fn unsupported_language_returns_empty() {
        assert!(scan("rust", "fn main(){}").is_empty());
        assert!(!has_rules("rust"));
        assert!(has_rules("python"));
    }
}
