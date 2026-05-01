//! C and C++ language backend (Tier 0e). One module covers two registry entries
//! via a `Variant` enum, mirroring `javascript.rs`.
//!
//! Note: pgmcp's `config.rs` does not currently route `.c`/`.cpp`/`.h`/`.hpp`
//! files through indexing — this backend ships ready, but a follow-up patch
//! to `default_file_types()` is needed for it to fire on real files.

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::parsing::LanguageBackend;
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolRefKind, SymbolReference};

#[derive(Clone, Copy)]
enum Variant {
    C,
    Cpp,
}

pub struct CCppBackend {
    variant: Variant,
}

pub static C_BACKEND: CCppBackend = CCppBackend {
    variant: Variant::C,
};
pub static CPP_BACKEND: CCppBackend = CCppBackend {
    variant: Variant::Cpp,
};

fn language_for(v: Variant) -> Language {
    match v {
        Variant::C => tree_sitter_c::LANGUAGE.into(),
        Variant::Cpp => tree_sitter_cpp::LANGUAGE.into(),
    }
}

thread_local! {
    static C_PARSER: RefCell<Parser> = RefCell::new(make_parser(Variant::C));
    static CPP_PARSER: RefCell<Parser> = RefCell::new(make_parser(Variant::Cpp));
}

fn make_parser(v: Variant) -> Parser {
    let mut p = Parser::new();
    p.set_language(&language_for(v))
        .expect("set_language c/cpp");
    p
}

fn parse(content: &str, v: Variant) -> Option<Tree> {
    let f = |p: &RefCell<Parser>| p.borrow_mut().parse(content, None);
    match v {
        Variant::C => C_PARSER.with(f),
        Variant::Cpp => CPP_PARSER.with(f),
    }
}

const SYMBOL_QUERY_C: &str = r#"
(function_definition
  declarator: (function_declarator
                declarator: (identifier) @sym.func.name)) @sym.func

(declaration
  declarator: (function_declarator
                declarator: (identifier) @sym.proto.name)) @sym.proto

(struct_specifier name: (type_identifier) @sym.struct.name) @sym.struct
(enum_specifier name: (type_identifier) @sym.enum.name) @sym.enum

(type_definition
  declarator: (type_identifier) @sym.alias.name) @sym.alias
"#;

const SYMBOL_QUERY_CPP: &str = r#"
(function_definition
  declarator: (function_declarator
                declarator: (identifier) @sym.func.name)) @sym.func

(declaration
  declarator: (function_declarator
                declarator: (identifier) @sym.proto.name)) @sym.proto

(struct_specifier name: (type_identifier) @sym.struct.name) @sym.struct
(enum_specifier name: (type_identifier) @sym.enum.name) @sym.enum
(class_specifier name: (type_identifier) @sym.class.name) @sym.class
(namespace_definition name: (namespace_identifier) @sym.ns.name) @sym.ns

(type_definition
  declarator: (type_identifier) @sym.alias.name) @sym.alias
"#;

const IMPORT_QUERY_C: &str = r#"
(preproc_include path: (system_lib_string) @import.sys) @import.stmt
(preproc_include path: (string_literal) @import.local) @import.stmt
"#;

const IMPORT_QUERY_CPP: &str = r#"
(preproc_include path: (system_lib_string) @import.sys) @import.stmt
(preproc_include path: (string_literal) @import.local) @import.stmt
(using_declaration) @import.using
"#;

const REF_QUERY_C: &str = r#"
(call_expression function: (identifier) @ref.call)
(call_expression
  function: (field_expression
              field: (field_identifier) @ref.mcall))
(type_identifier) @ref.type
"#;

const REF_QUERY_CPP: &str = r#"
(call_expression function: (identifier) @ref.call)
(call_expression
  function: (field_expression
              field: (field_identifier) @ref.mcall))
(call_expression
  function: (qualified_identifier) @ref.qcall)
(base_class_clause (type_identifier) @ref.inherit)
(type_identifier) @ref.type
"#;

static SYMBOL_Q_C: OnceLock<Query> = OnceLock::new();
static SYMBOL_Q_CPP: OnceLock<Query> = OnceLock::new();
static IMPORT_Q_C: OnceLock<Query> = OnceLock::new();
static IMPORT_Q_CPP: OnceLock<Query> = OnceLock::new();
static REF_Q_C: OnceLock<Query> = OnceLock::new();
static REF_Q_CPP: OnceLock<Query> = OnceLock::new();

fn symbol_query(v: Variant) -> &'static Query {
    let lang = language_for(v);
    match v {
        Variant::C => {
            SYMBOL_Q_C.get_or_init(|| Query::new(&lang, SYMBOL_QUERY_C).expect("symbol query c"))
        }
        Variant::Cpp => SYMBOL_Q_CPP
            .get_or_init(|| Query::new(&lang, SYMBOL_QUERY_CPP).expect("symbol query cpp")),
    }
}
fn import_query(v: Variant) -> &'static Query {
    let lang = language_for(v);
    match v {
        Variant::C => {
            IMPORT_Q_C.get_or_init(|| Query::new(&lang, IMPORT_QUERY_C).expect("import query c"))
        }
        Variant::Cpp => IMPORT_Q_CPP
            .get_or_init(|| Query::new(&lang, IMPORT_QUERY_CPP).expect("import query cpp")),
    }
}
fn ref_query(v: Variant) -> &'static Query {
    let lang = language_for(v);
    match v {
        Variant::C => REF_Q_C.get_or_init(|| Query::new(&lang, REF_QUERY_C).expect("ref query c")),
        Variant::Cpp => {
            REF_Q_CPP.get_or_init(|| Query::new(&lang, REF_QUERY_CPP).expect("ref query cpp"))
        }
    }
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

fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) && s.len() >= 2 {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn first_line(content: &str, node: Node<'_>) -> String {
    let start = node.start_byte();
    let bytes = content.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end] != b'{' && bytes[end] != b';' && bytes[end] != b'\n' {
        end += 1;
    }
    content[start..end.min(bytes.len())].trim().to_string()
}

fn is_screaming_snake(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut has_upper = false;
    for c in name.chars() {
        if c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' {
            if c.is_ascii_uppercase() {
                has_upper = true;
            }
        } else {
            return false;
        }
    }
    has_upper
}

impl LanguageBackend for CCppBackend {
    fn language_name(&self) -> &'static str {
        match self.variant {
            Variant::C => "c",
            Variant::Cpp => "cpp",
        }
    }

    fn extract_symbols(&self, content: &str) -> Vec<Symbol> {
        let Some(tree) = parse(content, self.variant) else {
            return Vec::new();
        };
        let q = symbol_query(self.variant);
        let mut cursor = QueryCursor::new();
        let mut out: Vec<Symbol> = Vec::new();
        let mut matches = cursor.matches(q, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let cap_name = q.capture_names()[cap.index as usize];
                let node = cap.node;
                let kind_and_filter = match cap_name {
                    "sym.func" | "sym.proto" => Some((SymbolKind::Function, false)),
                    "sym.struct" => Some((SymbolKind::Struct, false)),
                    "sym.enum" => Some((SymbolKind::Enum, false)),
                    "sym.alias" => Some((SymbolKind::Other, false)),
                    "sym.class" => Some((SymbolKind::Class, false)),
                    "sym.ns" => Some((SymbolKind::Module, false)),
                    _ => None,
                };
                let Some((kind, _)) = kind_and_filter else {
                    continue;
                };
                let name_node_opt = match cap_name {
                    "sym.func" | "sym.proto" => find_function_name(node),
                    "sym.struct" | "sym.enum" | "sym.class" | "sym.alias" => node
                        .child_by_field_name("name")
                        .or_else(|| node.child_by_field_name("declarator")),
                    "sym.ns" => node.child_by_field_name("name"),
                    _ => None,
                };
                let Some(name_node) = name_node_opt else {
                    continue;
                };
                let name = node_text(name_node, content).to_string();
                if name.is_empty() {
                    continue;
                }
                let signature = first_line(content, node);
                out.push(Symbol {
                    file_id: 0,
                    kind,
                    start_line: line_of(node),
                    end_line: end_line_of(node),
                    parent_id: None,
                    visibility: Some("public".into()),
                    signature: Some(signature),
                    name,
                });
            }
        }
        // Filter out non-SCREAMING_SNAKE typedefs that aren't really constants
        // — not applicable here since we map all typedefs to Other.
        out
    }

    fn extract_imports(&self, content: &str) -> Vec<Import> {
        let Some(tree) = parse(content, self.variant) else {
            return Vec::new();
        };
        let q = import_query(self.variant);
        let mut cursor = QueryCursor::new();
        let mut out: Vec<Import> = Vec::new();
        let mut matches = cursor.matches(q, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let cap_name = q.capture_names()[cap.index as usize];
                let node = cap.node;
                match cap_name {
                    "import.sys" => {
                        // System lib string includes < and >; preserve verbatim.
                        let target_raw = node_text(node, content).to_string();
                        if !target_raw.is_empty() {
                            out.push(Import {
                                target_raw,
                                source_line: line_of(node),
                                alias: None,
                            });
                        }
                    }
                    "import.local" => {
                        let target_raw = strip_quotes(node_text(node, content));
                        if !target_raw.is_empty() {
                            out.push(Import {
                                target_raw,
                                source_line: line_of(node),
                                alias: None,
                            });
                        }
                    }
                    "import.using" => {
                        // C++ `using namespace foo;` or `using foo::Bar;`.
                        // The target is the qualified_identifier or identifier child.
                        let mut walker = node.walk();
                        for child in node.named_children(&mut walker) {
                            if child.kind() == "qualified_identifier"
                                || child.kind() == "identifier"
                            {
                                let target_raw = node_text(child, content).to_string();
                                if !target_raw.is_empty() {
                                    out.push(Import {
                                        target_raw,
                                        source_line: line_of(node),
                                        alias: None,
                                    });
                                }
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }

    fn extract_references(&self, content: &str) -> Vec<SymbolReference> {
        let Some(tree) = parse(content, self.variant) else {
            return Vec::new();
        };
        let q = ref_query(self.variant);
        let mut cursor = QueryCursor::new();
        let mut out: Vec<SymbolReference> = Vec::new();
        let mut matches = cursor.matches(q, tree.root_node(), content.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let cap_name = q.capture_names()[cap.index as usize];
                let node = cap.node;
                let target_raw = node_text(node, content).to_string();
                if target_raw.is_empty() {
                    continue;
                }
                let kind = match cap_name {
                    "ref.call" | "ref.mcall" | "ref.qcall" => SymbolRefKind::Call,
                    "ref.inherit" => SymbolRefKind::Inherit,
                    "ref.type" => SymbolRefKind::TypeUse,
                    _ => continue,
                };
                out.push(SymbolReference {
                    source_file_id: 0,
                    source_symbol_id: None,
                    target_file_id: None,
                    target_symbol_id: None,
                    target_raw,
                    ref_kind: kind,
                    source_line: line_of(node),
                });
            }
        }
        out
    }
}

/// Suppress an unused warning — the SCREAMING_SNAKE filter applies to a future
/// `Const` capture path that's not yet wired.
#[allow(dead_code)]
fn _unused_screaming_snake(name: &str) -> bool {
    is_screaming_snake(name)
}

/// For function_definition / declaration captures, the captured node is the
/// outer wrapper — find the inner identifier.
fn find_function_name(node: Node<'_>) -> Option<Node<'_>> {
    // Walk down through (function_declarator (identifier)).
    let declarator = node.child_by_field_name("declarator")?;
    if declarator.kind() == "function_declarator"
        && let Some(inner) = declarator.child_by_field_name("declarator")
    {
        return Some(inner);
    }
    Some(declarator)
}

#[cfg(test)]
mod tests {
    use super::*;

    const C_SAMPLE: &str = r#"
#include <stdio.h>
#include "myheader.h"

typedef struct Point { int x; int y; } Point;

int compute(Point p) {
    return p.x + p.y;
}
"#;

    const CPP_SAMPLE: &str = r#"
#include <vector>
#include "common.h"

namespace ns {
    class Greeter : public Hello {
    public:
        std::string greet(const std::string& name);
    };
}
"#;

    #[test]
    fn c_extract_symbols_finds_struct_and_function() {
        let syms = C_BACKEND.extract_symbols(C_SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"compute"), "names: {:?}", names);
        assert!(names.contains(&"Point"));
    }

    #[test]
    fn c_extract_imports_handles_system_and_local() {
        let imps = C_BACKEND.extract_imports(C_SAMPLE);
        let targets: Vec<&str> = imps.iter().map(|i| i.target_raw.as_str()).collect();
        // System include preserves angle brackets.
        assert!(
            targets.iter().any(|t| t.contains("stdio")),
            "imports: {:?}",
            imps
        );
        assert!(targets.contains(&"myheader.h"), "imports: {:?}", imps);
    }

    #[test]
    fn cpp_extract_symbols_finds_class_and_namespace() {
        let syms = CPP_BACKEND.extract_symbols(CPP_SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Greeter"), "names: {:?}", names);
        assert!(names.contains(&"ns"));
        let greeter = syms.iter().find(|s| s.name == "Greeter").unwrap();
        assert_eq!(greeter.kind, SymbolKind::Class);
        let ns = syms.iter().find(|s| s.name == "ns").unwrap();
        assert_eq!(ns.kind, SymbolKind::Module);
    }

    #[test]
    fn cpp_extract_references_finds_inherit() {
        let refs = CPP_BACKEND.extract_references(CPP_SAMPLE);
        let inherits: Vec<&str> = refs
            .iter()
            .filter(|r| r.ref_kind == SymbolRefKind::Inherit)
            .map(|r| r.target_raw.as_str())
            .collect();
        assert!(inherits.contains(&"Hello"), "inherits: {:?}", inherits);
    }

    #[test]
    fn parse_garbage_yields_no_panic() {
        for s in ["", "   ", "#include", "class { {"] {
            let _ = C_BACKEND.extract_symbols(s);
            let _ = C_BACKEND.extract_imports(s);
            let _ = CPP_BACKEND.extract_references(s);
        }
    }

    #[test]
    fn language_names() {
        assert_eq!(C_BACKEND.language_name(), "c");
        assert_eq!(CPP_BACKEND.language_name(), "cpp");
    }
}
