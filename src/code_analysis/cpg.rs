//! Code Property Graph (AST ∪ CFG ∪ DFG) construction via
//! libgrammstein's `code::cpg::CodePropertyGraph`.
//!
//! Built on demand for the `code_property_graph` MCP tool (Phase 8);
//! not persisted (the underlying `ParsedCode` is rebuilt per call so
//! the graph never goes stale). pgmcp's Shadow-ASR symbols provide
//! the resolution_kind tiers that the CPG's call edges can be
//! filtered by downstream.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 6.

use std::sync::Arc;

use libgrammstein::code::ast::CodeParser;
use libgrammstein::code::language::CodeLanguage;
use libgrammstein::code::{CodePropertyGraph, ParsedCode};

/// Errors surfaced by CPG construction.
#[derive(Debug, thiserror::Error)]
pub enum CpgError {
    /// libgrammstein parser error.
    #[error("parse error: {0}")]
    Parse(String),
}

/// Build a Code Property Graph from a source string. The `language`
/// param is a concrete `CodeLanguage` impl (e.g. `Python`, `Rust`,
/// `JavaScript` from `libgrammstein::code::languages::*`).
pub fn build_cpg<L>(source: &str, language: Arc<L>) -> Result<CodePropertyGraph, CpgError>
where
    L: CodeLanguage + 'static,
{
    let mut parser = CodeParser::new(language).map_err(|e| CpgError::Parse(format!("{e:?}")))?;
    let parsed: ParsedCode = parser
        .parse(source)
        .map_err(|e| CpgError::Parse(format!("{e:?}")))?;
    Ok(CodePropertyGraph::from_parsed_code(&parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libgrammstein::code::languages::python::Python;

    #[test]
    fn empty_python_source_does_not_panic() {
        let _ = build_cpg("", Arc::new(Python));
    }
}
