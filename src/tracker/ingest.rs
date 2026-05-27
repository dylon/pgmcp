//! Plan → task-tree parser: turn an agent's markdown plan (or ExitPlanMode
//! output) into a flat list of [`ParsedNode`]s with parent links, which the
//! ingestion REST handler / `work_item_ingest_plan` tool inserts as a
//! `work_items` subtree. Pure and unit-testable (no DB, no regex deps beyond
//! the workspace `regex`).
//!
//! Mapping (plan §H.1): `#`→plan, `##`→epic, `###`→task, `####+`→sub_task;
//! `- [ ]`/`- [x]` checklist→todo (checked seeds `claimed_done`, NOT verified);
//! numbered `1.`→sub_task; an inline `TODO:`/`FIXME:`/… marker→todo/fixme; a
//! line/heading with universal phrasing ("all"/"every"/"feature parity")→
//! `parametric`; an `acceptance:` line→an acceptance criterion on the current
//! node. Hierarchy is a unified depth space (heading level + list indentation)
//! resolved with a parent stack, so nesting is arbitrary-depth.

use std::sync::OnceLock;

use regex::Regex;

use crate::tracker::kind::WorkItemKind;

/// A parsed acceptance criterion (attached to the node it follows).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedAcceptance {
    pub criterion_kind: String,
    pub description: String,
    pub acceptance_uri: Option<String>,
}

/// One node of the parsed plan tree (flat; `parent_index` references an earlier
/// entry, `None` = a root).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedNode {
    pub kind: WorkItemKind,
    pub title: String,
    pub body: Option<String>,
    /// Unified outline depth (heading level + list indentation).
    pub depth: usize,
    pub parent_index: Option<usize>,
    /// Seed status for already-done checklist items (`claimed_done`), else
    /// `None` (the inserter uses the schema default `pending`).
    pub seed_claimed_done: bool,
    pub parametric: bool,
    pub parametric_corpus: Option<String>,
    pub acceptance: Vec<ParsedAcceptance>,
}

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(#{1,6})\s+(.+?)\s*$").expect("valid heading regex"))
}
fn checklist_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(\s*)[-*+]\s+\[([ xX])\]\s+(.+?)\s*$").expect("valid checklist regex")
    })
}
fn numbered_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\s*)\d+[.)]\s+(.+?)\s*$").expect("valid numbered regex"))
}
fn acceptance_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*acceptance:\s*(.+?)\s*$").expect("valid acceptance regex")
    })
}
fn marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(TODO|FIXME|HACK|XXX|BUG)\b").expect("valid marker regex"))
}

/// Heading level → work-item kind.
fn heading_kind(level: usize) -> WorkItemKind {
    match level {
        1 => WorkItemKind::Plan,
        2 => WorkItemKind::Epic,
        3 => WorkItemKind::Task,
        _ => WorkItemKind::SubTask,
    }
}

/// Universal-quantifier phrasing → parametric clause.
fn detect_parametric(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("feature parity")
        || lower.contains("byte-equal")
        || lower
            .split_whitespace()
            .any(|w| w == "all" || w == "every" || w == "each")
}

/// Derive an acceptance-criterion kind from a URI scheme (default `test`).
fn criterion_kind_for(uri: &str) -> &'static str {
    match uri.split("://").next().unwrap_or("") {
        "lean" | "rocq" => "proof",
        "tla" => "model_check",
        "smt2" | "smt" => "smt",
        "shell" => "script",
        "auditor" => "auditor_verdict",
        "experiment" => "experiment_verdict",
        "cargo" => "test",
        _ => "test",
    }
}

/// Parse a markdown plan into a flat node list with parent links.
pub fn parse_plan(markdown: &str) -> Vec<ParsedNode> {
    let mut nodes: Vec<ParsedNode> = Vec::new();
    // Stack of (depth, node_index) for outline parenting.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut current_heading_level: usize = 0;
    let mut last_index: Option<usize> = None;

    let push_node = |nodes: &mut Vec<ParsedNode>,
                     stack: &mut Vec<(usize, usize)>,
                     depth: usize,
                     node: ParsedNode|
     -> usize {
        while let Some(&(d, _)) = stack.last() {
            if d >= depth {
                stack.pop();
            } else {
                break;
            }
        }
        let parent_index = stack.last().map(|&(_, i)| i);
        let idx = nodes.len();
        let mut node = node;
        node.parent_index = parent_index;
        node.depth = depth;
        nodes.push(node);
        stack.push((depth, idx));
        idx
    };

    for raw in markdown.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }

        // Heading → outline node.
        if let Some(c) = heading_re().captures(line) {
            let level = c[1].len();
            let title = c[2].trim().to_string();
            current_heading_level = level;
            let parametric = detect_parametric(&title);
            let idx = push_node(
                &mut nodes,
                &mut stack,
                level,
                ParsedNode {
                    kind: heading_kind(level),
                    title,
                    body: None,
                    depth: level,
                    parent_index: None,
                    seed_claimed_done: false,
                    parametric,
                    parametric_corpus: None,
                    acceptance: Vec::new(),
                },
            );
            last_index = Some(idx);
            continue;
        }

        // Checklist item → todo (under the current heading + indentation).
        if let Some(c) = checklist_re().captures(line) {
            let indent = c[1].len();
            let checked = !matches!(&c[2], " ");
            let title = c[3].trim().to_string();
            let depth = current_heading_level + 1 + indent / 2;
            let parametric = detect_parametric(&title);
            let idx = push_node(
                &mut nodes,
                &mut stack,
                depth,
                ParsedNode {
                    kind: WorkItemKind::Todo,
                    title,
                    body: None,
                    depth,
                    parent_index: None,
                    seed_claimed_done: checked,
                    parametric,
                    parametric_corpus: None,
                    acceptance: Vec::new(),
                },
            );
            last_index = Some(idx);
            continue;
        }

        // Numbered list → ordered sub_task.
        if let Some(c) = numbered_re().captures(line) {
            let indent = c[1].len();
            let title = c[2].trim().to_string();
            let depth = current_heading_level + 1 + indent / 2;
            let parametric = detect_parametric(&title);
            let idx = push_node(
                &mut nodes,
                &mut stack,
                depth,
                ParsedNode {
                    kind: WorkItemKind::SubTask,
                    title,
                    body: None,
                    depth,
                    parent_index: None,
                    seed_claimed_done: false,
                    parametric,
                    parametric_corpus: None,
                    acceptance: Vec::new(),
                },
            );
            last_index = Some(idx);
            continue;
        }

        // `acceptance:` line → attach a criterion to the current node.
        if let Some(c) = acceptance_re().captures(line) {
            if let Some(i) = last_index {
                let val = c[1].trim().to_string();
                let uri = val.contains("://").then(|| val.clone());
                let criterion_kind = uri
                    .as_deref()
                    .map(criterion_kind_for)
                    .unwrap_or("test")
                    .to_string();
                nodes[i].acceptance.push(ParsedAcceptance {
                    criterion_kind,
                    description: val,
                    acceptance_uri: uri,
                });
            }
            continue;
        }

        // Inline TODO/FIXME/… marker in prose → a todo/fixme item.
        if let Some(c) = marker_re().captures(line) {
            let marker = &c[1];
            let kind = if marker == "TODO" {
                WorkItemKind::Todo
            } else {
                WorkItemKind::Fixme
            };
            let title = line
                .trim_start_matches(['-', '*', '+', ' ', '#'])
                .trim()
                .to_string();
            let depth = current_heading_level + 1;
            let idx = push_node(
                &mut nodes,
                &mut stack,
                depth,
                ParsedNode {
                    kind,
                    title,
                    body: None,
                    depth,
                    parent_index: None,
                    seed_claimed_done: false,
                    parametric: false,
                    parametric_corpus: None,
                    acceptance: Vec::new(),
                },
            );
            last_index = Some(idx);
            continue;
        }

        // Otherwise prose → append to the current node's body.
        if let Some(i) = last_index {
            let prose = line.trim();
            match &mut nodes[i].body {
                Some(b) => {
                    b.push('\n');
                    b.push_str(prose);
                }
                None => nodes[i].body = Some(prose.to_string()),
            }
        }
    }

    nodes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_nest_by_level() {
        let md = "# Plan\n## Epic A\n### Task 1\n#### Sub 1\n## Epic B\n";
        let nodes = parse_plan(md);
        assert_eq!(nodes.len(), 5);
        assert_eq!(nodes[0].kind, WorkItemKind::Plan);
        assert_eq!(nodes[0].parent_index, None);
        assert_eq!(nodes[1].kind, WorkItemKind::Epic);
        assert_eq!(nodes[1].parent_index, Some(0));
        assert_eq!(nodes[2].kind, WorkItemKind::Task);
        assert_eq!(nodes[2].parent_index, Some(1));
        assert_eq!(nodes[3].kind, WorkItemKind::SubTask);
        assert_eq!(nodes[3].parent_index, Some(2));
        // Epic B pops back up under the plan.
        assert_eq!(nodes[4].kind, WorkItemKind::Epic);
        assert_eq!(nodes[4].parent_index, Some(0));
    }

    #[test]
    fn checklist_items_attach_to_heading_and_seed_done() {
        let md = "# Plan\n### Task\n- [ ] open item\n- [x] done item\n";
        let nodes = parse_plan(md);
        let task_idx = nodes
            .iter()
            .position(|n| n.kind == WorkItemKind::Task)
            .unwrap();
        let todos: Vec<&ParsedNode> = nodes
            .iter()
            .filter(|n| n.kind == WorkItemKind::Todo)
            .collect();
        assert_eq!(todos.len(), 2);
        assert!(todos.iter().all(|t| t.parent_index == Some(task_idx)));
        assert!(!todos[0].seed_claimed_done, "[ ] is not done");
        assert!(todos[1].seed_claimed_done, "[x] seeds claimed_done");
    }

    #[test]
    fn numbered_items_become_ordered_subtasks() {
        let md = "## Epic\n1. first\n2. second\n";
        let nodes = parse_plan(md);
        let subs: Vec<&ParsedNode> = nodes
            .iter()
            .filter(|n| n.kind == WorkItemKind::SubTask)
            .collect();
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].title, "first");
    }

    #[test]
    fn universal_phrasing_marks_parametric() {
        let md = "# Plan\n### All 11 grammars parse byte-equal\n";
        let nodes = parse_plan(md);
        let t = nodes.iter().find(|n| n.kind == WorkItemKind::Task).unwrap();
        assert!(t.parametric, "'all'/'byte-equal' phrasing is parametric");
    }

    #[test]
    fn acceptance_line_attaches_a_criterion() {
        let md = "### Task\nacceptance: cargo://tests/x.rs::test_foo\n";
        let nodes = parse_plan(md);
        let t = nodes.iter().find(|n| n.kind == WorkItemKind::Task).unwrap();
        assert_eq!(t.acceptance.len(), 1);
        assert_eq!(t.acceptance[0].criterion_kind, "test");
        assert_eq!(
            t.acceptance[0].acceptance_uri.as_deref(),
            Some("cargo://tests/x.rs::test_foo")
        );
    }

    #[test]
    fn inline_fixme_marker_becomes_fixme_item() {
        let md = "## Epic\nFIXME: the parser drops trailing tokens\n";
        let nodes = parse_plan(md);
        assert!(nodes.iter().any(|n| n.kind == WorkItemKind::Fixme));
    }

    #[test]
    fn prose_after_a_heading_is_its_body() {
        let md = "# Plan\nThis plan ships the tracker.\n";
        let nodes = parse_plan(md);
        assert_eq!(
            nodes[0].body.as_deref(),
            Some("This plan ships the tracker.")
        );
    }

    #[test]
    fn empty_input_yields_no_nodes() {
        assert!(parse_plan("").is_empty());
        assert!(parse_plan("\n\n   \n").is_empty());
    }
}
