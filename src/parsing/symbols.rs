//! Symbol + import + reference data types for the tree-sitter parsing layer.
//!
//! These mirror the `file_symbols` and `symbol_references` schema in
//! `src/db/migrations.rs`. The cron job (Phase 0b — TBD) extracts them via
//! `LanguageBackend` and persists in bulk.

#![allow(dead_code)] // Types are wired up by `LanguageBackend` impls and the
// future symbol-extraction cron. Marking allow here so the foundational
// types compile clean before any backend lands.

use serde::{Deserialize, Serialize};

/// What kind of symbol this is. Maps 1:1 to `file_symbols.kind` text values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Interface,
    Class,
    Const,
    Module,
    /// Catch-all for backends that surface a symbol that doesn't fit the
    /// canonical taxonomy (e.g. Python `TypeVar`, TS `type` alias).
    Other,
}

impl SymbolKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::Interface => "interface",
            SymbolKind::Class => "class",
            SymbolKind::Const => "const",
            SymbolKind::Module => "module",
            SymbolKind::Other => "other",
        }
    }

    pub fn from_db_str(s: &str) -> SymbolKind {
        match s {
            "function" => SymbolKind::Function,
            "struct" => SymbolKind::Struct,
            "enum" => SymbolKind::Enum,
            "trait" => SymbolKind::Trait,
            "interface" => SymbolKind::Interface,
            "class" => SymbolKind::Class,
            "const" => SymbolKind::Const,
            "module" => SymbolKind::Module,
            _ => SymbolKind::Other,
        }
    }
}

/// One symbol definition extracted from a file.
///
/// `file_id` and `parent_id` are filled in by the persistence layer; the
/// `LanguageBackend::extract_symbols` impl returns rows with placeholder
/// `file_id = 0` and `parent_id = None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub file_id: i64,
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub parent_id: Option<i64>,
    /// `public` / `private` / `module` / `None` (language-specific; backends
    /// should map to one of these or leave `None`).
    pub visibility: Option<String>,
    /// Raw text of the signature line (e.g. `pub fn foo(x: i32) -> bool`).
    pub signature: Option<String>,
}

/// One import statement extracted from a file. The `target_raw` form is the
/// canonical resolvable identifier per the language: Rust path, Python module,
/// JS specifier, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Import {
    pub target_raw: String,
    pub source_line: u32,
    /// Optional alias / specifier (e.g. `import { Foo as Bar }` → `Some("Bar")`).
    pub alias: Option<String>,
}

/// What kind of cross-symbol edge this reference is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolRefKind {
    /// Function call / method invocation.
    Call,
    /// Type usage (e.g. `Foo` in `let x: Foo`).
    TypeUse,
    /// Class / trait / interface inheritance (`extends`, `: Base`, etc.).
    Inherit,
    /// Trait / interface implementation (`impl Foo for Bar`, `class Foo: Bar`).
    Impl,
    /// Inside an `import` statement — used to materialize the directed edge
    /// for the import graph.
    ImportUse,
}

impl SymbolRefKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            SymbolRefKind::Call => "call",
            SymbolRefKind::TypeUse => "type_use",
            SymbolRefKind::Inherit => "inherit",
            SymbolRefKind::Impl => "impl",
            SymbolRefKind::ImportUse => "import_use",
        }
    }
}

/// One symbol reference (call, type use, inheritance, impl, or import-use).
///
/// `source_file_id`, `source_symbol_id`, `target_file_id`, `target_symbol_id`
/// are filled in by the persistence layer. `LanguageBackend` impls return
/// rows with placeholder `source_file_id = 0` and `target_*_id = None`; the
/// cron resolves `target_symbol_id` via name match within the same language.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolReference {
    pub source_file_id: i64,
    pub source_symbol_id: Option<i64>,
    pub target_file_id: Option<i64>,
    pub target_symbol_id: Option<i64>,
    pub target_raw: String,
    pub ref_kind: SymbolRefKind,
    pub source_line: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_kind_round_trips_through_db_str() {
        for k in [
            SymbolKind::Function,
            SymbolKind::Struct,
            SymbolKind::Enum,
            SymbolKind::Trait,
            SymbolKind::Interface,
            SymbolKind::Class,
            SymbolKind::Const,
            SymbolKind::Module,
            SymbolKind::Other,
        ] {
            assert_eq!(SymbolKind::from_db_str(k.as_db_str()), k);
        }
    }

    #[test]
    fn symbol_kind_from_db_str_unknown_returns_other() {
        assert_eq!(SymbolKind::from_db_str("nonexistent"), SymbolKind::Other);
        assert_eq!(SymbolKind::from_db_str(""), SymbolKind::Other);
    }

    #[test]
    fn symbol_kind_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&SymbolKind::Function).expect("serialize function"),
            "\"function\""
        );
        assert_eq!(
            serde_json::to_string(&SymbolKind::Other).expect("serialize other"),
            "\"other\""
        );
    }

    #[test]
    fn symbol_ref_kind_db_str_dispatch() {
        assert_eq!(SymbolRefKind::Call.as_db_str(), "call");
        assert_eq!(SymbolRefKind::TypeUse.as_db_str(), "type_use");
        assert_eq!(SymbolRefKind::Inherit.as_db_str(), "inherit");
        assert_eq!(SymbolRefKind::Impl.as_db_str(), "impl");
        assert_eq!(SymbolRefKind::ImportUse.as_db_str(), "import_use");
    }

    #[test]
    fn symbol_round_trips_through_json() {
        let s = Symbol {
            file_id: 42,
            name: "build_request_headers".into(),
            kind: SymbolKind::Function,
            start_line: 10,
            end_line: 25,
            parent_id: Some(7),
            visibility: Some("public".into()),
            signature: Some("pub fn build_request_headers(token: &str) -> HeaderMap".into()),
        };
        let json = serde_json::to_string(&s).expect("serialize");
        let parsed: Symbol = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.name, s.name);
        assert_eq!(parsed.kind, s.kind);
        assert_eq!(parsed.parent_id, s.parent_id);
        assert_eq!(parsed.visibility, s.visibility);
    }

    #[test]
    fn import_round_trips_through_json() {
        let i = Import {
            target_raw: "crate::config::Config".into(),
            source_line: 3,
            alias: None,
        };
        let json = serde_json::to_string(&i).expect("serialize");
        let parsed: Import = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.target_raw, i.target_raw);
        assert_eq!(parsed.source_line, i.source_line);
    }

    #[test]
    fn symbol_reference_round_trips() {
        let r = SymbolReference {
            source_file_id: 100,
            source_symbol_id: Some(50),
            target_file_id: None,
            target_symbol_id: None,
            target_raw: "validate_email".into(),
            ref_kind: SymbolRefKind::Call,
            source_line: 17,
        };
        let json = serde_json::to_string(&r).expect("serialize");
        let parsed: SymbolReference = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.target_raw, r.target_raw);
        assert_eq!(parsed.ref_kind, r.ref_kind);
        assert_eq!(parsed.source_line, r.source_line);
    }
}
