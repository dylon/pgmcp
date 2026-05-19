//! Rholang language backend (Tier 0e). Uses the user's
//! `tree-sitter-rholang` grammar via path dependency in `Cargo.toml`.
//!
//! Rholang is process-calculus-based and has only one definitional construct:
//! the `contract` declaration. `new` declarations bind local channels — only
//! emitted as imports when paired with a registry URI literal
//! (`new x(\`rho:registry:lookup\`) in { ... }`).
//!
//! See: `/home/dylon/Workspace/f1r3fly.io/rholang-rs/rholang-tree-sitter/`.

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::parsing::backend::LanguageBackend;
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolRefKind, SymbolReference};

pub static RHOLANG_BACKEND: RholangBackend = RholangBackend;
pub struct RholangBackend;

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        p.set_language(&tree_sitter_rholang::LANGUAGE.into())
            .expect("set_language rholang");
        p
    });
}

static SYMBOL_Q: OnceLock<Query> = OnceLock::new();
static IMPORT_Q: OnceLock<Query> = OnceLock::new();
static REF_Q: OnceLock<Query> = OnceLock::new();

const SYMBOL_QUERY: &str = r#"
(contract) @contract.def
"#;

const IMPORT_QUERY: &str = r#"
(name_decl
  (var) @import.alias
  uri: (uri_literal) @import.target) @import.decl
"#;

const REF_QUERY: &str = r#"
(send) @send.expr
(send_sync) @sendsync.expr
"#;

fn symbol_query() -> &'static Query {
    SYMBOL_Q.get_or_init(|| {
        Query::new(&tree_sitter_rholang::LANGUAGE.into(), SYMBOL_QUERY)
            .expect("symbol query rholang")
    })
}
fn import_query() -> &'static Query {
    IMPORT_Q.get_or_init(|| {
        Query::new(&tree_sitter_rholang::LANGUAGE.into(), IMPORT_QUERY)
            .expect("import query rholang")
    })
}
fn ref_query() -> &'static Query {
    REF_Q.get_or_init(|| {
        Query::new(&tree_sitter_rholang::LANGUAGE.into(), REF_QUERY).expect("ref query rholang")
    })
}

fn parse(content: &str) -> Option<Tree> {
    PARSER.with(|p| p.borrow_mut().parse(content, None))
}

fn line_of(node: Node<'_>) -> u32 {
    (node.start_position().row as u32) + 1
}
fn end_line_of(node: Node<'_>) -> u32 {
    (node.end_position().row as u32) + 1
}
fn node_text<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

/// Strip surrounding backticks from a `uri_literal` text (`\`rho:io:stdout\`` → `rho:io:stdout`).
fn strip_backticks(s: &str) -> &str {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('`') && t.ends_with('`') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

fn first_line(content: &str, node: Node<'_>) -> String {
    let start = node.start_byte();
    let bytes = content.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end] != b'{' && bytes[end] != b'\n' {
        end += 1;
    }
    content[start..end.min(bytes.len())].trim().to_string()
}

/// Extract a Rholang contract's name, walking through `_proc_var` or `quote`
/// wrappers to find a usable identifier.
fn contract_name(node: Node<'_>, src: &str) -> Option<String> {
    let name_node = node.child_by_field_name("name")?;
    Some(extract_name_text(name_node, src))
}

fn extract_name_text(node: Node<'_>, src: &str) -> String {
    match node.kind() {
        "var" => node_text(node, src).to_string(),
        "_proc_var" | "proc_var" => {
            let mut walker = node.walk();
            for child in node.named_children(&mut walker) {
                if child.kind() == "var" {
                    return node_text(child, src).to_string();
                }
            }
            node_text(node, src).to_string()
        }
        "quote" => {
            // Quote can wrap a string_literal, var, or arbitrary process.
            let mut walker = node.walk();
            for child in node.named_children(&mut walker) {
                match child.kind() {
                    "var" => return node_text(child, src).to_string(),
                    "string_literal" => {
                        let raw = node_text(child, src);
                        return raw.trim_matches('"').to_string();
                    }
                    _ => {}
                }
            }
            // Fallback: raw text minus leading `@`.
            let raw = node_text(node, src).trim_start_matches('@');
            raw.to_string()
        }
        _ => node_text(node, src).to_string(),
    }
}

/// Walk a `send` / `send_sync` node and find its channel target identifier.
fn channel_target(node: Node<'_>, src: &str) -> Option<String> {
    let chan = node.child_by_field_name("channel")?;
    Some(extract_name_text(chan, src))
}

impl LanguageBackend for RholangBackend {
    fn language_name(&self) -> &'static str {
        "rholang"
    }

    fn extract_symbols(&self, content: &str) -> Vec<Symbol> {
        let Some(tree) = parse(content) else {
            return Vec::new();
        };
        let q = symbol_query();
        let mut cursor = QueryCursor::new();
        let mut out: Vec<Symbol> = Vec::new();
        let mut matches = cursor.matches(q, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let cap_name = q.capture_names()[cap.index as usize];
                if cap_name != "contract.def" {
                    continue;
                }
                let node = cap.node;
                let Some(name) = contract_name(node, content) else {
                    continue;
                };
                if name.is_empty() {
                    continue;
                }
                out.push(Symbol {
                    file_id: 0,
                    kind: SymbolKind::Function,
                    start_line: line_of(node),
                    end_line: end_line_of(node),
                    parent_id: None,
                    visibility: None,
                    signature: Some(first_line(content, node)),
                    name,
                });
            }
        }
        out
    }

    fn extract_imports(&self, content: &str) -> Vec<Import> {
        let Some(tree) = parse(content) else {
            return Vec::new();
        };
        let q = import_query();
        let mut cursor = QueryCursor::new();
        let mut out: Vec<Import> = Vec::new();
        let mut matches = cursor.matches(q, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            let alias_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "import.alias");
            let target_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "import.target");
            let decl_cap = m
                .captures
                .iter()
                .find(|c| q.capture_names()[c.index as usize] == "import.decl");
            if let (Some(alias), Some(target)) = (alias_cap, target_cap) {
                let line = decl_cap
                    .map(|c| line_of(c.node))
                    .unwrap_or_else(|| line_of(target.node));
                let alias_text = node_text(alias.node, content).to_string();
                let target_raw = strip_backticks(node_text(target.node, content)).to_string();
                if !target_raw.is_empty() {
                    out.push(Import {
                        target_raw,
                        source_line: line,
                        alias: if alias_text.is_empty() {
                            None
                        } else {
                            Some(alias_text)
                        },
                    });
                }
            }
        }
        out
    }

    fn extract_references(&self, content: &str) -> Vec<SymbolReference> {
        let Some(tree) = parse(content) else {
            return Vec::new();
        };
        let q = ref_query();
        let mut cursor = QueryCursor::new();
        let mut out: Vec<SymbolReference> = Vec::new();
        let mut matches = cursor.matches(q, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let cap_name = q.capture_names()[cap.index as usize];
                if cap_name != "send.expr" && cap_name != "sendsync.expr" {
                    continue;
                }
                let node = cap.node;
                if let Some(target_raw) = channel_target(node, content)
                    && !target_raw.is_empty()
                {
                    out.push(SymbolReference {
                        source_file_id: 0,
                        source_symbol_id: None,
                        target_file_id: None,
                        target_symbol_id: None,
                        target_raw,
                        ref_kind: SymbolRefKind::Call,
                        source_line: line_of(node),
                    });
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_WORLD: &str = "new stdout(`rho:io:stdout`) in {\n  \
contract helloworld(@name) = {\n    \
  stdout!(\"hello, \" ++ name)\n  \
}\n  \
|\n  \
helloworld!(\"world\")\n  \
|\n  \
helloworld!(\"world2\")\n\
}\n";

    #[test]
    fn extract_symbols_finds_contract() {
        let syms = RHOLANG_BACKEND.extract_symbols(HELLO_WORLD);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"helloworld"), "names: {:?}", names);
        let helloworld = syms.iter().find(|s| s.name == "helloworld").unwrap();
        assert_eq!(helloworld.kind, SymbolKind::Function);
    }

    #[test]
    fn extract_imports_handles_registry_uri() {
        let imps = RHOLANG_BACKEND.extract_imports(HELLO_WORLD);
        assert!(
            imps.iter().any(|i| i.target_raw == "rho:io:stdout"),
            "imports: {:?}",
            imps
        );
        assert!(imps.iter().any(|i| i.alias.as_deref() == Some("stdout")));
    }

    #[test]
    fn extract_references_finds_sends() {
        let refs = RHOLANG_BACKEND.extract_references(HELLO_WORLD);
        let calls: Vec<&str> = refs.iter().map(|r| r.target_raw.as_str()).collect();
        assert!(calls.contains(&"helloworld"), "calls: {:?}", calls);
        assert!(calls.contains(&"stdout"), "calls: {:?}", calls);
    }

    #[test]
    fn parse_garbage_yields_no_panic() {
        for s in ["", "   ", "new x in {", "contract foo("] {
            let _ = RHOLANG_BACKEND.extract_symbols(s);
            let _ = RHOLANG_BACKEND.extract_imports(s);
            let _ = RHOLANG_BACKEND.extract_references(s);
        }
    }

    #[test]
    fn language_name_is_rholang() {
        assert_eq!(RHOLANG_BACKEND.language_name(), "rholang");
    }
}
