//! `lsp_query` parameters + the `LspOp` operation vocabulary (ADR-026).
//!
//! One read-only MCP tool exposes LSP-shaped analytical queries over the indexed
//! symbol graph (`file_symbols`, `symbol_references`, `symbol_occurrences`). A
//! single `op` enum (ADR-003 closed vocab) keeps the tool catalog small
//! (ADR-016 / Occam). NO mutating ops (rename / format / code-action) are
//! exposed — analysis only, at the shadow-ASR level.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspOp {
    /// All symbols defined in one file (textDocument/documentSymbol).
    DocumentSymbol,
    /// Symbols by name across the project (workspace/symbol).
    WorkspaceSymbol,
    /// The defining occurrence(s) of a symbol (textDocument/definition).
    Definition,
    /// All references to a symbol (textDocument/references).
    References,
    /// Signature + parameter/return types + effects of a symbol (hover).
    Hover,
    /// The definition of a symbol's type (textDocument/typeDefinition).
    TypeDefinition,
    /// Implementations of a trait/interface (textDocument/implementation).
    Implementation,
    /// Callers of a symbol (callHierarchy/incomingCalls).
    CallHierarchyIncoming,
    /// Callees of a symbol (callHierarchy/outgoingCalls).
    CallHierarchyOutgoing,
    /// Super-types of a type (typeHierarchy/supertypes).
    TypeHierarchySuper,
    /// Sub-types of a type (typeHierarchy/subtypes).
    TypeHierarchySub,
    /// Foldable span of each symbol in a file (textDocument/foldingRange).
    FoldingRange,
    /// Signature of a symbol (textDocument/signatureHelp).
    SignatureHelp,
    /// Same-file occurrences of an identifier (textDocument/documentHighlight).
    DocumentHighlight,
    /// The set of supported ops + backing data (server capabilities).
    Capabilities,
}

impl LspOp {
    pub const ALL: &'static [LspOp] = &[
        Self::DocumentSymbol,
        Self::WorkspaceSymbol,
        Self::Definition,
        Self::References,
        Self::Hover,
        Self::TypeDefinition,
        Self::Implementation,
        Self::CallHierarchyIncoming,
        Self::CallHierarchyOutgoing,
        Self::TypeHierarchySuper,
        Self::TypeHierarchySub,
        Self::FoldingRange,
        Self::SignatureHelp,
        Self::DocumentHighlight,
        Self::Capabilities,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DocumentSymbol => "document_symbol",
            Self::WorkspaceSymbol => "workspace_symbol",
            Self::Definition => "definition",
            Self::References => "references",
            Self::Hover => "hover",
            Self::TypeDefinition => "type_definition",
            Self::Implementation => "implementation",
            Self::CallHierarchyIncoming => "call_hierarchy_incoming",
            Self::CallHierarchyOutgoing => "call_hierarchy_outgoing",
            Self::TypeHierarchySuper => "type_hierarchy_super",
            Self::TypeHierarchySub => "type_hierarchy_sub",
            Self::FoldingRange => "folding_range",
            Self::SignatureHelp => "signature_help",
            Self::DocumentHighlight => "document_highlight",
            Self::Capabilities => "capabilities",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LspQueryParams {
    /// Project name or id.
    pub project: String,
    /// The LSP operation (document_symbol | workspace_symbol | definition |
    /// references | hover | type_definition | implementation |
    /// call_hierarchy_incoming | call_hierarchy_outgoing | type_hierarchy_super
    /// | type_hierarchy_sub | folding_range | signature_help |
    /// document_highlight | capabilities).
    pub op: String,
    /// File path (relative or absolute) — required by document_symbol /
    /// folding_range / document_highlight.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Symbol name — required by the symbol-centric ops.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Optional scoping symbol name (restrict `references` to a lexical scope).
    #[serde(default)]
    pub scope: Option<String>,
    /// Result cap (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<i64>,
}
