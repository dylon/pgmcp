//! Frequent-subtree mining via libgrammstein's TreeminerD.
//!
//! Surfaces structural code repetition that token-level duplicate
//! finders miss. The `subtree_mining` MCP tool (Phase 8) is the
//! user-facing consumer; this module wraps the underlying algorithm.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 6.

use std::sync::Arc;

use libgrammstein::code::ParsedCode;
use libgrammstein::code::ast::{AstNode, CodeParser};
use libgrammstein::code::language::CodeLanguage;
use libgrammstein::code::subtree::{FlatTree, MiningResult, TreeminerD};

/// Errors surfaced by subtree mining.
#[derive(Debug, thiserror::Error)]
pub enum SubtreeError {
    /// libgrammstein parser error.
    #[error("parse error: {0}")]
    Parse(String),
}

/// Mine frequent subtree patterns across N source strings written in
/// the same language. Each source is parsed → simplified to `AstNode`
/// → flattened to `FlatTree`, then `TreeminerD` mines patterns
/// appearing in at least `min_support_fraction` of the trees.
///
/// `min_support_fraction` is a float in [0, 1] — e.g. 0.1 = "appears
/// in at least 10% of trees".
pub fn mine_patterns<L>(
    language: Arc<L>,
    sources: &[String],
    min_support_fraction: f64,
) -> Result<MiningResult, SubtreeError>
where
    L: CodeLanguage + 'static,
{
    if sources.is_empty() {
        return Ok(MiningResult {
            patterns: Vec::new(),
            num_trees: 0,
            min_support_count: 0,
            candidates_generated: 0,
            patterns_pruned: 0,
            mining_time_ms: 0,
        });
    }
    let mut parser =
        CodeParser::new(language).map_err(|e| SubtreeError::Parse(format!("{e:?}")))?;
    let mut flat: Vec<FlatTree> = Vec::with_capacity(sources.len());
    for (idx, src) in sources.iter().enumerate() {
        let parsed: ParsedCode = parser
            .parse(src)
            .map_err(|e| SubtreeError::Parse(format!("{e:?}")))?;
        let ast = AstNode::from_ts_node(parsed.root(), src);
        flat.push(FlatTree::from_ast_node(&ast, idx as u64));
    }
    let miner = TreeminerD::new(min_support_fraction);
    Ok(miner.mine(&flat))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libgrammstein::code::languages::python::Python;

    #[test]
    fn empty_input_returns_empty_result() {
        let result = mine_patterns(Arc::new(Python), &[], 0.1).expect("mine empty");
        assert_eq!(result.patterns.len(), 0);
    }
}
