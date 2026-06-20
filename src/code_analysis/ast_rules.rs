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
pub const AST_RULE_LANGUAGES: &[&str] = &["python", "typescript", "tsx", "clojure", "clojurescript"];

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
        // The plain-TS and TSX grammars share node kinds for the constructs we
        // match (call_expression / member_expression / new_expression), so one
        // walker handles both. JavaScript is intentionally NOT added here: its
        // analyzers still use the regex fallback, and adding it would suppress
        // those without a JS-specific node audit.
        "typescript" | "tsx" => scan_typescript(language, content),
        "clojure" | "clojurescript" => scan_clojure(content),
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

// ============================================================================
// TypeScript / TSX rule set (Group 1c). Mirrors `scan_python`.
// ============================================================================

fn scan_typescript(language: &str, content: &str) -> Vec<AstRuleHit> {
    let mut parser = Parser::new();
    let lang = match language {
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        _ => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
    };
    if parser.set_language(&lang).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(content, None) else {
        return Vec::new();
    };
    let src = content.as_bytes();
    let mut out = Vec::new();
    walk_typescript(tree.root_node(), src, content, &mut out);
    out
}

fn walk_typescript(node: Node, src: &[u8], content: &str, out: &mut Vec<AstRuleHit>) {
    match node.kind() {
        "call_expression" => {
            if let Some(hit) = classify_ts_call(node, src, content) {
                out.push(hit);
            }
        }
        // `new Function("body")` constructs code at runtime (CWE-95).
        "new_expression" => {
            if let Some(hit) = classify_ts_new(node, src, content) {
                out.push(hit);
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_typescript(child, src, content, out);
    }
}

fn classify_ts_call(call: Node, src: &[u8], content: &str) -> Option<AstRuleHit> {
    let func = call.child_by_field_name("function")?;
    let callee = ts_dotted_name(func, src)?;
    let line = call.start_position().row as u32 + 1;
    let args_text = call
        .child_by_field_name("arguments")
        .and_then(|a| a.utf8_text(src).ok())
        .unwrap_or("");
    let snippet = || line_text(content, line);

    // Weak hashes: crypto.createHash('md5') / createHash('sha1').
    if is_callee(&callee, "createHash") {
        let lowered = args_text.to_ascii_lowercase();
        if lowered.contains("\"md5\"") || lowered.contains("'md5'") || lowered.contains("`md5`") {
            return Some(hit(
                "weak_md5",
                "crypto",
                "MD5 is cryptographically broken; use SHA-256 or BLAKE2 for security purposes.",
                line,
                snippet(),
            ));
        }
        if lowered.contains("\"sha1\"") || lowered.contains("'sha1'") || lowered.contains("`sha1`")
        {
            return Some(hit(
                "weak_sha1",
                "crypto",
                "SHA-1 is broken; use SHA-256+ for security purposes.",
                line,
                snippet(),
            ));
        }
    }
    // ECB cipher mode: crypto.createCipheriv('aes-128-ecb', ...).
    if (is_callee(&callee, "createCipheriv") || is_callee(&callee, "createDecipheriv"))
        && args_text.to_ascii_lowercase().contains("-ecb")
    {
        return Some(hit(
            "ecb_mode",
            "crypto",
            "ECB mode leaks plaintext structure; use an AEAD mode (GCM) or CBC+HMAC.",
            line,
            snippet(),
        ));
    }
    // Math.random() used where cryptographic randomness is implied is weak, but
    // that requires data-flow to confirm intent; we keep to high-confidence
    // call-shape rules here.

    // Unsafe deserialization / dynamic code execution.
    if callee == "eval" {
        return Some(hit(
            "eval_call",
            "deserialize",
            "eval executes arbitrary code on its argument (CWE-95); avoid on untrusted data.",
            line,
            snippet(),
        ));
    }
    // node-serialize `unserialize` / `serialize.unserialize` runs IIFE payloads.
    if is_callee(&callee, "unserialize") {
        return Some(hit(
            "node_unserialize",
            "deserialize",
            "node-serialize unserialize executes embedded functions (CWE-502); use JSON.parse.",
            line,
            snippet(),
        ));
    }
    // vm.runInThisContext / vm.runInNewContext compiles+runs untrusted code.
    if callee == "vm.runInThisContext"
        || callee == "vm.runInNewContext"
        || is_callee(&callee, "runInThisContext")
        || is_callee(&callee, "runInNewContext")
    {
        return Some(hit(
            "vm_run_untrusted",
            "deserialize",
            "vm.runInThisContext/runInNewContext executes arbitrary code (CWE-95).",
            line,
            snippet(),
        ));
    }
    None
}

fn classify_ts_new(new_node: Node, src: &[u8], content: &str) -> Option<AstRuleHit> {
    // `new Function(...)` — the constructor is the `constructor` field
    // (an identifier `Function`).
    let ctor = new_node.child_by_field_name("constructor")?;
    let name = ts_dotted_name(ctor, src)?;
    let line = new_node.start_position().row as u32 + 1;
    if name == "Function" || is_callee(&name, "Function") {
        return Some(hit(
            "new_function_ctor",
            "deserialize",
            "new Function(...) compiles a string into code at runtime (CWE-95).",
            line,
            line_text(content, line),
        ));
    }
    None
}

/// Render a tree-sitter TS `member_expression` / `identifier` callee as a
/// dotted path (`crypto.createHash`, `eval`). Returns `None` for dynamic
/// (computed / call-chained) callees we can't statically name.
fn ts_dotted_name(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier"
        | "property_identifier"
        | "type_identifier"
        | "shorthand_property_identifier" => node.utf8_text(src).ok().map(|s| s.to_string()),
        "member_expression" => {
            let obj = node.child_by_field_name("object")?;
            let prop = node.child_by_field_name("property")?;
            let obj_s = ts_dotted_name(obj, src)?;
            let prop_s = prop.utf8_text(src).ok()?;
            Some(format!("{obj_s}.{prop_s}"))
        }
        _ => None,
    }
}

// ============================================================================
// Clojure / ClojureScript rule set (Group 1d). Code-injection sinks.
// ============================================================================

fn scan_clojure(content: &str) -> Vec<AstRuleHit> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_clojure::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(content, None) else {
        return Vec::new();
    };
    let src = content.as_bytes();
    let mut out = Vec::new();
    walk_clojure(tree.root_node(), src, content, &mut out);
    out
}

fn walk_clojure(node: Node, src: &[u8], content: &str, out: &mut Vec<AstRuleHit>) {
    if node.kind() == "list_lit"
        && let Some(hit) = classify_clojure_form(node, src, content)
    {
        out.push(hit);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_clojure(child, src, content, out);
    }
}

fn classify_clojure_form(form: Node, src: &[u8], content: &str) -> Option<AstRuleHit> {
    // Head symbol of the list form is the callee.
    let head = form.named_child(0)?;
    if head.kind() != "sym_lit" {
        return None;
    }
    let callee = head.utf8_text(src).ok()?;
    let line = form.start_position().row as u32 + 1;
    let snippet = || line_text(content, line);

    // Split an optional namespace qualifier (`clojure.core/eval` → ns
    // `clojure.core`, name `eval`).
    let (ns, bare) = match callee.rsplit_once('/') {
        Some((ns, name)) => (Some(ns), name),
        None => (None, callee),
    };

    match bare {
        "eval" => Some(hit(
            "clojure_eval",
            "deserialize",
            "eval executes arbitrary Clojure forms (CWE-95); never on untrusted data.",
            line,
            snippet(),
        )),
        // `clojure.edn/read-string` is the SAFE reader (no `*read-eval*`); only
        // flag the core / unqualified `read-string`.
        "read-string" if ns != Some("clojure.edn") && ns != Some("edn") => Some(hit(
            "clojure_read_string",
            "deserialize",
            "read-string can execute code via `*read-eval*` reader literals (CWE-502); use \
             clojure.edn/read-string for untrusted input.",
            line,
            snippet(),
        )),
        "load-string" => Some(hit(
            "clojure_load_string",
            "deserialize",
            "load-string compiles and runs a string of Clojure source (CWE-95).",
            line,
            snippet(),
        )),
        _ => None,
    }
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

    // ========================================================================
    // TypeScript rule-set tests (Group 1c)
    // ========================================================================

    fn ts_rule_ids(src: &str) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = scan("typescript", src)
            .into_iter()
            .map(|h| h.rule_id)
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn ts_languages_registered() {
        assert!(has_rules("typescript"));
        assert!(has_rules("tsx"));
    }

    #[test]
    fn ts_flags_weak_hash_md5_and_sha1() {
        let src = "import crypto from 'crypto';\nconst a = crypto.createHash('md5');\nconst b = crypto.createHash(\"sha1\");\n";
        assert_eq!(ts_rule_ids(src), vec!["weak_md5", "weak_sha1"]);
    }

    #[test]
    fn ts_createhash_sha256_is_safe() {
        let src = "const a = crypto.createHash('sha256');\n";
        assert!(scan("typescript", src).is_empty(), "sha256 must not flag");
    }

    #[test]
    fn ts_flags_ecb_cipher() {
        let src = "const c = crypto.createCipheriv('aes-128-ecb', key, null);\n";
        assert_eq!(ts_rule_ids(src), vec!["ecb_mode"]);
    }

    #[test]
    fn ts_flags_eval_and_new_function() {
        let src = "const r = eval(userInput);\nconst f = new Function('a', 'return a + 1');\n";
        assert_eq!(ts_rule_ids(src), vec!["eval_call", "new_function_ctor"]);
    }

    #[test]
    fn ts_flags_node_unserialize_and_vm() {
        let src = "import serialize from 'node-serialize';\nconst o = serialize.unserialize(payload);\nconst x = vm.runInNewContext(code);\n";
        assert_eq!(
            ts_rule_ids(src),
            vec!["node_unserialize", "vm_run_untrusted"]
        );
    }

    #[test]
    fn ts_does_not_match_in_comments_or_strings() {
        // The AST-matching guarantee: these must NOT flag.
        let src = "const s = \"never call eval on untrusted input\";\n// crypto.createHash('md5') is weak\nconst y = 1;\n";
        assert!(
            scan("typescript", src).is_empty(),
            "comment/string must not match"
        );
    }

    #[test]
    fn tsx_walker_handles_jsx_source() {
        // TSX variant must parse + scan without panic and still flag eval.
        let src = "function App() { const r = eval(x); return <div>{r}</div>; }";
        let ids: Vec<&'static str> = scan("tsx", src).into_iter().map(|h| h.rule_id).collect();
        assert!(ids.contains(&"eval_call"), "ids: {:?}", ids);
    }

    // ========================================================================
    // Clojure rule-set tests (Group 1d)
    // ========================================================================

    fn clj_rule_ids(src: &str) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = scan("clojure", src).into_iter().map(|h| h.rule_id).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn clojure_languages_registered() {
        assert!(has_rules("clojure"));
        assert!(has_rules("clojurescript"));
    }

    #[test]
    fn clojure_flags_eval_read_string_load_string() {
        let src = "(eval form)\n(read-string s)\n(load-string code)\n";
        assert_eq!(
            clj_rule_ids(src),
            vec!["clojure_eval", "clojure_load_string", "clojure_read_string"]
        );
    }

    #[test]
    fn clojure_edn_read_string_is_safe() {
        let src = "(clojure.edn/read-string s)\n";
        assert!(
            scan("clojure", src).is_empty(),
            "edn/read-string must not flag"
        );
    }

    #[test]
    fn clojure_does_not_match_in_strings_or_comments() {
        let src = "(def s \"call eval on this\")\n;; (read-string x) is dangerous\n(def y 1)\n";
        assert!(
            scan("clojure", src).is_empty(),
            "comment/string must not match"
        );
    }

    #[test]
    fn clojurescript_variant_scans() {
        let ids: Vec<&'static str> = scan("clojurescript", "(eval form)\n")
            .into_iter()
            .map(|h| h.rule_id)
            .collect();
        assert!(ids.contains(&"clojure_eval"), "ids: {:?}", ids);
    }
}
