//! Language-agnostic intraprocedural data-flow IR for taint analysis
//! (graph-roadmap Phase 2.1).
//!
//! Each `LanguageBackend::extract_dataflow` walks its AST and emits one
//! [`FunctionDataflow`] per function; the language-agnostic engine in
//! [`crate::code_analysis::taint_dataflow`] runs real source→sink reachability
//! over it. This replaces the regex source/sink *co-occurrence* heuristic
//! (`tool_taint_analysis`'s old behavior) with an actual def-use flow check:
//! a finding requires that a value *derived from* a source *reaches* a sink
//! argument without passing through a sanitizer.
//!
//! The IR is deliberately small and backend-agnostic — backends only need to
//! identify: (a) variable→variable assignments (flow edges), (b) variables that
//! originate taint (sources), (c) variables whose taint is cleared
//! (sanitized), and (d) call sites that consume values dangerously (sinks).

/// A local data-flow node within a single function — a variable definition or
/// a transient value. Dense `u32` ids, assigned by the extractor per function.
pub type FlowNode = u32;

/// A value that originates taint (attacker-controllable input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaintSource {
    /// The flow node that becomes tainted at this source.
    pub node: FlowNode,
    /// Source category: `env`, `argv`, `stdin`, `request`, `file_read`, `socket`, …
    pub kind: String,
    /// 1-based source line, for the report.
    pub line: u32,
}

/// A call site that consumes values dangerously. A finding is raised when any
/// `args` node is tainted (reachable from a source, not sanitized).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaintSink {
    /// Value nodes flowing into the dangerous call.
    pub args: Vec<FlowNode>,
    /// Sink category: `command`, `sql`, `eval`, `deserialize`, `path`, `html`, …
    pub kind: String,
    /// The sink callee, for the report (e.g. `Command::new`, `eval`).
    pub callee: String,
    /// 1-based sink line.
    pub line: u32,
}

/// Intraprocedural def-use facts for one function.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FunctionDataflow {
    pub function: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Number of distinct flow nodes (for sizing).
    pub node_count: u32,
    /// Taint propagates from `.0` to `.1` (assignment RHS→LHS, arg→result,
    /// receiver→result, move/borrow). Directed.
    pub flow_edges: Vec<(FlowNode, FlowNode)>,
    /// Nodes that originate taint.
    pub sources: Vec<TaintSource>,
    /// Nodes whose taint is cleared by a sanitizer/validator — taint does not
    /// propagate *out of* a sanitized node.
    pub sanitized: Vec<FlowNode>,
    /// Dangerous consumption sites.
    pub sinks: Vec<TaintSink>,
}

impl FunctionDataflow {
    /// `true` when there's nothing to analyze (no source or no sink) — lets the
    /// engine skip the reachability search cheaply.
    pub fn is_trivial(&self) -> bool {
        self.sources.is_empty() || self.sinks.is_empty()
    }
}
