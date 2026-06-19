//! Symbol + import + reference data types for the tree-sitter parsing layer.
//!
//! These mirror the `file_symbols` and `symbol_references` schema in
//! `src/db/migrations.rs`. The cron job (Phase 0b — TBD) extracts them via
//! `LanguageBackend` and persists in bulk.

#![allow(dead_code)] // Types are wired up by `LanguageBackend` impls and the
// future symbol-extraction cron. Marking allow here so the foundational
// types compile clean before any backend lands.

use serde::{Deserialize, Serialize};

use crate::parsing::type_tags::TypeShape;

/// What kind of symbol this is. Maps 1:1 to `file_symbols.kind` text values.
///
/// The `Block`, `Impl`, `Lambda`, `Namespace`, `Macro` variants were added in
/// the shadow_asr_v1 schema bump to give the explicit scope-graph chain
/// requested by the unified semantic representation. Existing backends do
/// not emit them by default; per-backend population follows in Phase B+.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
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
    /// Anonymous block scope (Rust `{ ... }`, C compound statement,
    /// Python comprehension, JS `let`-block, Rholang `new x in { ... }`).
    Block,
    /// `impl Foo` / `impl Trait for Foo` block. Methods inside are nested as
    /// `Function` rows with this `Impl` as parent.
    Impl,
    /// Lambda / anonymous function / closure expression that introduces a
    /// scope but isn't named in source.
    Lambda,
    /// Named module-like grouping that doesn't fit `Module` semantics
    /// (e.g. C++ namespace, Rust extern block, Scala `package object`).
    Namespace,
    /// Macro definition (`macro_rules!`, `defmacro`, Lisp `defmacro`,
    /// Scala `macro`).
    Macro,
    /// Catch-all for backends that surface a symbol that doesn't fit the
    /// canonical taxonomy (e.g. Python `TypeVar`, TS `type` alias).
    #[default]
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
            SymbolKind::Block => "block",
            SymbolKind::Impl => "impl",
            SymbolKind::Lambda => "lambda",
            SymbolKind::Namespace => "namespace",
            SymbolKind::Macro => "macro",
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
            "block" => SymbolKind::Block,
            "impl" => SymbolKind::Impl,
            "lambda" => SymbolKind::Lambda,
            "namespace" => SymbolKind::Namespace,
            "macro" => SymbolKind::Macro,
            _ => SymbolKind::Other,
        }
    }

    /// True when this kind introduces a lexical scope. Used by the resolution
    /// pass to chase `parent_id` chains and assemble `scope_path`.
    pub fn introduces_scope(self) -> bool {
        matches!(
            self,
            SymbolKind::Function
                | SymbolKind::Struct
                | SymbolKind::Enum
                | SymbolKind::Trait
                | SymbolKind::Interface
                | SymbolKind::Class
                | SymbolKind::Module
                | SymbolKind::Block
                | SymbolKind::Impl
                | SymbolKind::Lambda
                | SymbolKind::Namespace
                | SymbolKind::Macro
        )
    }
}

/// How a parameter is passed: by reference, mutable reference, owned, etc.
/// Backends emit the closest equivalent in their source language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamModifier {
    /// Borrowed reference (Rust `&T`, C++ `const T&`, Java/JS pass-by-ref
    /// for objects).
    Ref,
    /// Mutable borrowed reference (Rust `&mut T`, C++ `T&`).
    MutRef,
    /// Owned by-value (Rust `T`, C `T`, Java primitive).
    Own,
    /// Moved-out (C++ `T&&` move, Rust `T` consumed by callee).
    Move,
    /// `in`-passing semantics (C# / Swift `in` parameter).
    In,
    /// `out`-passing semantics (C# `out`, Swift `inout` of result).
    Out,
    /// `inout` semantics (C# `ref`, Swift `inout`).
    Inout,
    /// Python keyword-only argument (after `*`).
    KwOnly,
    /// Python positional-only argument (before `/`).
    PosOnly,
}

impl ParamModifier {
    pub fn as_db_str(self) -> &'static str {
        match self {
            ParamModifier::Ref => "ref",
            ParamModifier::MutRef => "mut_ref",
            ParamModifier::Own => "own",
            ParamModifier::Move => "move",
            ParamModifier::In => "in",
            ParamModifier::Out => "out",
            ParamModifier::Inout => "inout",
            ParamModifier::KwOnly => "kwonly",
            ParamModifier::PosOnly => "posonly",
        }
    }

    pub fn from_db_str(s: &str) -> Option<ParamModifier> {
        Some(match s {
            "ref" => ParamModifier::Ref,
            "mut_ref" => ParamModifier::MutRef,
            "own" => ParamModifier::Own,
            "move" => ParamModifier::Move,
            "in" => ParamModifier::In,
            "out" => ParamModifier::Out,
            "inout" => ParamModifier::Inout,
            "kwonly" => ParamModifier::KwOnly,
            "posonly" => ParamModifier::PosOnly,
            _ => return None,
        })
    }
}

/// One parameter of a function / method / contract / rule.
///
/// Persisted into the `symbol_parameters` table by the persistence layer.
/// `position` is the 0-indexed source order; backends are responsible for
/// emitting parameters in source order. `type_tags` carries owned `String`
/// values rather than `&'static str` so the struct round-trips through serde
/// (the persistence layer reads back from `text[]`). Backends should
/// construct tags via `.to_string()` on the `vocabulary::TAG_*` constants —
/// the constants are still the only legitimate source, the strings are just
/// the storage shape.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Parameter {
    pub position: u32,
    /// `None` for anonymous parameters (C-style `void foo(int, int)`).
    pub name: Option<String>,
    /// Raw source text of the type annotation (e.g. `"&mut Vec<u8>"`).
    pub type_raw: Option<String>,
    /// Open-set tags from `crate::parsing::type_tags::vocabulary::SEED_TYPE_TAGS`.
    /// Stored as the `text[]` column `symbol_parameters.type_tags`.
    #[serde(default)]
    pub type_tags: Vec<String>,
    /// Structural shape (`Vec<Result<u8, IoError>>` decomposition).
    #[serde(default)]
    pub type_shape: Option<TypeShape>,
    /// Default value source text, when the parameter has one.
    #[serde(default)]
    pub default_value: Option<String>,
    /// How the parameter is passed.
    #[serde(default)]
    pub modifier: Option<ParamModifier>,
    /// `true` for variadic (`...args`, `*args`, `T... xs`).
    #[serde(default)]
    pub is_variadic: bool,
    /// `true` for the implicit receiver (Rust `self`, Python `self`/`cls`,
    /// MeTTa rule's first arg if convention says so).
    #[serde(default)]
    pub is_self: bool,
}

/// Return type information for a function-like symbol.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReturnType {
    pub type_raw: Option<String>,
    #[serde(default)]
    pub type_tags: Vec<String>,
    #[serde(default)]
    pub type_shape: Option<TypeShape>,
}

/// One generic / type parameter on a function or type definition.
///
/// `bounds` collects trait / typeclass / superclass constraints in their
/// source-text form — full resolution requires type inference and is left
/// to a future phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenericParam {
    pub name: String,
    #[serde(default)]
    pub bounds: Vec<String>,
    /// Default type (e.g. Rust `<T = String>`).
    #[serde(default)]
    pub default: Option<String>,
}

/// One symbol definition extracted from a file.
///
/// `file_id` and `parent_id` are filled in by the persistence layer; the
/// `LanguageBackend::extract_symbols` impl returns rows with placeholder
/// `file_id = 0` and `parent_id = None`.
///
/// Fields below `signature` are the shadow-ASR additions. Backends populate
/// them per language; defaults are intentionally permissive so a backend
/// can populate only what it can extract cleanly (e.g. Coq leaves
/// `parameters`/`return_type`/`type_tags` empty, while Rust + MeTTa
/// populate them fully).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Symbol {
    pub file_id: i64,
    pub name: String,
    #[serde(default)]
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    #[serde(default)]
    pub parent_id: Option<i64>,
    /// `public` / `private` / `module` / `None` (language-specific; backends
    /// should map to one of these or leave `None`).
    #[serde(default)]
    pub visibility: Option<String>,
    /// Raw text of the signature line (e.g. `pub fn foo(x: i32) -> bool`).
    #[serde(default)]
    pub signature: Option<String>,
    /// Structured parameters (shadow-ASR addition).
    #[serde(default)]
    pub parameters: Vec<Parameter>,
    /// Return type information (shadow-ASR addition).
    #[serde(default)]
    pub return_type: Option<ReturnType>,
    /// Generic / type parameters (shadow-ASR addition).
    #[serde(default)]
    pub generic_params: Vec<GenericParam>,
    /// Effects emitted by this symbol — open-set names from
    /// `crate::parsing::type_tags::vocabulary::SEED_EFFECTS` (shadow-ASR
    /// addition). Stored on `symbol_effects` after persistence. Backends
    /// produce `String` values from the `vocabulary::EFFECT_*` constants
    /// (`.to_string()` on construction).
    #[serde(default)]
    pub effects: Vec<String>,
    /// Qualified scope path like `crate::config::Config::validate` (shadow-
    /// ASR addition). Populated by the resolution pass when `parent_id`
    /// chains have been resolved.
    #[serde(default)]
    pub scope_path: Option<String>,
    /// Nesting depth of this symbol (0 = top-level) — shadow-ASR addition.
    #[serde(default)]
    pub scope_depth: Option<u32>,
}

/// One token-level identifier occurrence (ADR-024, v45 `symbol_occurrences`).
/// Produced by [`crate::parsing::backend::LanguageBackend::extract_occurrences`];
/// the extraction cron fills `enclosing_symbol_id` / `resolved_target_id` and
/// upgrades a code occurrence coinciding with a definition to
/// `OccurrenceKind::Definition`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Occurrence {
    pub name: String,
    /// 1-based line.
    pub start_line: u32,
    /// 0-based UTF-8 char column where the identifier starts.
    pub start_col: u32,
    /// 0-based UTF-8 char column just past the identifier's last char.
    pub end_col: u32,
    pub occurrence_kind: crate::parsing::occurrence_kind::OccurrenceKind,
    /// Coarse type tags where a binder annotation is available (else empty).
    #[serde(default)]
    pub type_tags: Vec<String>,
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
            SymbolKind::Block,
            SymbolKind::Impl,
            SymbolKind::Lambda,
            SymbolKind::Namespace,
            SymbolKind::Macro,
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
    fn symbol_kind_default_is_other() {
        let k: SymbolKind = Default::default();
        assert_eq!(k, SymbolKind::Other);
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
        // Shadow-ASR additions round-trip the same way.
        assert_eq!(
            serde_json::to_string(&SymbolKind::Block).expect("serialize block"),
            "\"block\""
        );
        assert_eq!(
            serde_json::to_string(&SymbolKind::Impl).expect("serialize impl"),
            "\"impl\""
        );
        assert_eq!(
            serde_json::to_string(&SymbolKind::Lambda).expect("serialize lambda"),
            "\"lambda\""
        );
        assert_eq!(
            serde_json::to_string(&SymbolKind::Namespace).expect("serialize namespace"),
            "\"namespace\""
        );
        assert_eq!(
            serde_json::to_string(&SymbolKind::Macro).expect("serialize macro"),
            "\"macro\""
        );
    }

    #[test]
    fn introduces_scope_correctness() {
        for k in [
            SymbolKind::Function,
            SymbolKind::Class,
            SymbolKind::Module,
            SymbolKind::Block,
            SymbolKind::Impl,
            SymbolKind::Lambda,
            SymbolKind::Namespace,
            SymbolKind::Macro,
        ] {
            assert!(k.introduces_scope(), "{k:?} should introduce a scope");
        }
        // `Const` and `Other` don't introduce lexical scopes for our resolver.
        assert!(!SymbolKind::Const.introduces_scope());
        assert!(!SymbolKind::Other.introduces_scope());
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
            ..Default::default()
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

    #[test]
    fn param_modifier_round_trips_through_db_str() {
        for m in [
            ParamModifier::Ref,
            ParamModifier::MutRef,
            ParamModifier::Own,
            ParamModifier::Move,
            ParamModifier::In,
            ParamModifier::Out,
            ParamModifier::Inout,
            ParamModifier::KwOnly,
            ParamModifier::PosOnly,
        ] {
            assert_eq!(ParamModifier::from_db_str(m.as_db_str()), Some(m));
        }
        assert_eq!(ParamModifier::from_db_str("does_not_exist"), None);
    }

    #[test]
    fn parameter_round_trips_through_json() {
        let p = Parameter {
            position: 0,
            name: Some("user".into()),
            type_raw: Some("&mut User".into()),
            type_tags: vec![
                crate::parsing::type_tags::vocabulary::TAG_MUTABLE_REF.to_string(),
                crate::parsing::type_tags::vocabulary::TAG_OWNED.to_string(),
            ],
            type_shape: Some(crate::parsing::type_tags::TypeShape::leaf("User")),
            default_value: None,
            modifier: Some(ParamModifier::MutRef),
            is_variadic: false,
            is_self: false,
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let parsed: Parameter = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, p);
    }

    #[test]
    fn return_type_round_trips_through_json() {
        let rt = ReturnType {
            type_raw: Some("Result<Token, AuthError>".into()),
            type_tags: vec![
                crate::parsing::type_tags::vocabulary::TAG_RESULT.to_string(),
                crate::parsing::type_tags::vocabulary::TAG_SUM_TYPE.to_string(),
            ],
            type_shape: Some(crate::parsing::type_tags::TypeShape::applied(
                "Result",
                vec![
                    crate::parsing::type_tags::TypeShape::leaf("Token"),
                    crate::parsing::type_tags::TypeShape::leaf("AuthError"),
                ],
            )),
        };
        let json = serde_json::to_string(&rt).expect("serialize");
        let parsed: ReturnType = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, rt);
    }

    #[test]
    fn generic_param_round_trips_through_json() {
        let g = GenericParam {
            name: "T".into(),
            bounds: vec!["Send".into(), "Sync".into(), "'static".into()],
            default: Some("String".into()),
        };
        let json = serde_json::to_string(&g).expect("serialize");
        let parsed: GenericParam = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, g);
    }

    #[test]
    fn symbol_default_initializes_shadow_asr_fields() {
        // Default initialization must produce empty / None values for every
        // shadow-ASR field so backends can opt in incrementally with
        // `..Default::default()` without polluting unset rows.
        let s: Symbol = Default::default();
        assert!(s.parameters.is_empty());
        assert!(s.return_type.is_none());
        assert!(s.generic_params.is_empty());
        assert!(s.effects.is_empty());
        assert!(s.scope_path.is_none());
        assert!(s.scope_depth.is_none());
    }

    #[test]
    fn symbol_with_shadow_asr_fields_round_trips() {
        // Full shadow-ASR-populated symbol — exercises every new field via
        // serde so we catch any default-tag mismatch.
        let s = Symbol {
            file_id: 1,
            name: "authenticate".into(),
            kind: SymbolKind::Function,
            start_line: 10,
            end_line: 25,
            parent_id: Some(7),
            visibility: Some("public".into()),
            signature: Some("pub async fn authenticate(user: &User, password: &str) -> Result<Token, AuthError>".into()),
            parameters: vec![
                Parameter {
                    position: 0,
                    name: Some("user".into()),
                    type_raw: Some("&User".into()),
                    type_tags: vec![
                        crate::parsing::type_tags::vocabulary::TAG_REFERENCE.to_string(),
                    ],
                    type_shape: Some(crate::parsing::type_tags::TypeShape::leaf("User")),
                    default_value: None,
                    modifier: Some(ParamModifier::Ref),
                    is_variadic: false,
                    is_self: false,
                },
                Parameter {
                    position: 1,
                    name: Some("password".into()),
                    type_raw: Some("&str".into()),
                    type_tags: vec![
                        crate::parsing::type_tags::vocabulary::TAG_REFERENCE.to_string(),
                        crate::parsing::type_tags::vocabulary::TAG_STRING.to_string(),
                    ],
                    type_shape: Some(crate::parsing::type_tags::TypeShape::leaf("str")),
                    default_value: None,
                    modifier: Some(ParamModifier::Ref),
                    is_variadic: false,
                    is_self: false,
                },
            ],
            return_type: Some(ReturnType {
                type_raw: Some("Result<Token, AuthError>".into()),
                type_tags: vec![
                    crate::parsing::type_tags::vocabulary::TAG_RESULT.to_string(),
                    crate::parsing::type_tags::vocabulary::TAG_SUM_TYPE.to_string(),
                ],
                type_shape: Some(crate::parsing::type_tags::TypeShape::applied(
                    "Result",
                    vec![
                        crate::parsing::type_tags::TypeShape::leaf("Token"),
                        crate::parsing::type_tags::TypeShape::leaf("AuthError"),
                    ],
                )),
            }),
            generic_params: vec![],
            effects: vec![
                crate::parsing::type_tags::vocabulary::EFFECT_ASYNC.to_string(),
            ],
            scope_path: Some("crate::auth::authenticate".into()),
            scope_depth: Some(2),
        };
        let json = serde_json::to_string(&s).expect("serialize");
        let parsed: Symbol = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.name, s.name);
        assert_eq!(parsed.parameters.len(), 2);
        assert_eq!(parsed.parameters[0].name.as_deref(), Some("user"));
        assert_eq!(
            parsed
                .return_type
                .as_ref()
                .map(|rt| rt.type_raw.as_deref().unwrap_or("")),
            Some("Result<Token, AuthError>")
        );
        assert_eq!(parsed.effects, vec!["async".to_string()]);
        assert_eq!(
            parsed.scope_path.as_deref(),
            Some("crate::auth::authenticate")
        );
        assert_eq!(parsed.scope_depth, Some(2));
    }
}
