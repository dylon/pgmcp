//! Shadow-ASR extraction for Rholang (process calculus).
//!
//! Rholang's semantics center on channels and process composition.
//! Symbols come in three flavors here:
//!
//! - `contract name(@x, @y) = { … }` — function-like definitions whose
//!   parameters are quoted-process patterns.
//! - `new x in { … }` — fresh-channel declarations.
//! - `let x = expr in { … }` — value bindings.
//!
//! The mapper surfaces:
//! - Per-contract parameters with `process` / `quoted_process` /
//!   `name` type tags.
//! - Channel-effect names (`channel_send`, `channel_send_persistent`,
//!   `channel_send_sync`, `channel_receive_linear`,
//!   `channel_receive_persistent`, `channel_receive_peek`,
//!   `contract_define`, `process_spawn`, `registry_lookup`,
//!   `channel_eval`).
//! - Return type tags reflecting the process calculus shape
//!   (`process` for the contract body's return).

use tree_sitter::Node;

use crate::parsing::symbols::{ParamModifier, Parameter, ReturnType as SemReturnType};
use crate::parsing::type_tags::TypeShape;
use crate::parsing::type_tags::vocabulary as v;

fn node_text<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

/// Build `Parameter` rows from a Rholang `contract`'s `formals: (names …)`
/// field. Each formal is typically a `quote(var)` pattern (`@x`).
pub(super) fn parameters_from_formals(formals: Node<'_>, src: &str) -> Vec<Parameter> {
    let mut out: Vec<Parameter> = Vec::new();
    let mut cursor = formals.walk();
    for (position, child) in (0_u32..).zip(formals.named_children(&mut cursor)) {
        // child is typically `quote(var)` for `@x`, or `var` for bare names.
        let (name, is_quoted) = match child.kind() {
            "quote" => {
                let mut inner_cursor = child.walk();
                let mut inner_name = String::new();
                for inner in child.named_children(&mut inner_cursor) {
                    if inner.kind() == "var" {
                        inner_name = node_text(inner, src).to_string();
                        break;
                    }
                }
                (Some(inner_name), true)
            }
            "var" => (Some(node_text(child, src).to_string()), false),
            "wildcard" => (Some("_".to_string()), false),
            _ => (Some(node_text(child, src).to_string()), false),
        };
        let mut type_tags: Vec<String> = vec![v::TAG_NAME.to_string(), v::TAG_PROCESS.to_string()];
        if is_quoted {
            type_tags.push(v::TAG_QUOTED_PROCESS.to_string());
        }
        type_tags.sort();
        type_tags.dedup();
        let type_shape = Some(TypeShape::leaf_raw(
            if is_quoted { "QuotedProcess" } else { "Name" },
            node_text(child, src),
        ));
        out.push(Parameter {
            position,
            name,
            type_raw: Some(node_text(child, src).to_string()),
            type_tags,
            type_shape,
            default_value: None,
            modifier: Some(ParamModifier::Own),
            is_variadic: false,
            is_self: false,
        });
    }
    out
}

/// Build the return type for a contract — Rholang contracts return a
/// process, not a value. We surface that explicitly so cross-language
/// tools can recognize process-typed functions.
pub(super) fn return_type_for_contract() -> SemReturnType {
    SemReturnType {
        type_raw: Some("Process".to_string()),
        type_tags: vec![v::TAG_PROCESS.to_string(), v::TAG_UNIT.to_string()],
        type_shape: Some(TypeShape::leaf("Process")),
    }
}

/// Walk a contract body recursively, accumulating effects from every
/// channel send, receive, and process-spawn site.
pub(super) fn effects_for_contract_body(body: Node<'_>, src: &str) -> Vec<String> {
    let mut found: std::collections::BTreeSet<&'static str> = std::collections::BTreeSet::new();
    found.insert(v::EFFECT_CONTRACT_DEFINE);
    walk_for_effects(body, src, &mut found);
    found.into_iter().map(String::from).collect()
}

/// Walk a Rholang `name_decl` block body and collect channel-flavored
/// effects.
pub(super) fn effects_for_block(body: Node<'_>, src: &str) -> Vec<String> {
    let mut found: std::collections::BTreeSet<&'static str> = std::collections::BTreeSet::new();
    walk_for_effects(body, src, &mut found);
    found.into_iter().map(String::from).collect()
}

fn walk_for_effects(
    node: Node<'_>,
    src: &str,
    found: &mut std::collections::BTreeSet<&'static str>,
) {
    let kind = node.kind();
    match kind {
        "send" => {
            // Examine `send_type` field for the variant.
            if let Some(st) = node.child_by_field_name("send_type") {
                match st.kind() {
                    "send_single" => found.insert(v::EFFECT_CHANNEL_SEND),
                    "send_multiple" => found.insert(v::EFFECT_CHANNEL_SEND_PERSISTENT),
                    _ => found.insert(v::EFFECT_CHANNEL_SEND),
                };
            } else {
                found.insert(v::EFFECT_CHANNEL_SEND);
            }
        }
        "send_sync" => {
            found.insert(v::EFFECT_CHANNEL_SEND_SYNC);
        }
        "linear_bind" => {
            found.insert(v::EFFECT_CHANNEL_RECEIVE_LINEAR);
        }
        "repeated_bind" => {
            found.insert(v::EFFECT_CHANNEL_RECEIVE_PERSISTENT);
        }
        "peek_bind" => {
            found.insert(v::EFFECT_CHANNEL_RECEIVE_PEEK);
        }
        "eval" => {
            found.insert(v::EFFECT_CHANNEL_EVAL);
        }
        "par" => {
            // par-composition implies process spawn.
            found.insert(v::EFFECT_PROCESS_SPAWN);
        }
        "block" => {
            // No effect of its own — children will be walked below.
        }
        "uri_literal" => {
            // `rho:registry:lookup` in a name_decl → registry lookup.
            let text = node_text(node, src);
            if text.contains("registry:lookup") {
                found.insert(v::EFFECT_REGISTRY_LOOKUP);
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_for_effects(child, src, found);
    }
}

/// Build a type-shape and tag set for a Rholang `new` channel
/// declaration. Channel decls with a URI become registry imports
/// (handled in `extract_imports`); the simple form is a bare channel.
pub(super) fn return_type_for_channel(has_uri: bool) -> SemReturnType {
    let mut type_tags: Vec<String> = vec![
        v::TAG_CHANNEL.to_string(),
        v::TAG_NAME.to_string(),
        v::TAG_PROCESS.to_string(),
    ];
    if has_uri {
        type_tags.push(v::TAG_REGISTRY_URI.to_string());
    }
    type_tags.sort();
    SemReturnType {
        type_raw: Some(
            if has_uri {
                "RegistryChannel"
            } else {
                "Channel"
            }
            .to_string(),
        ),
        type_tags,
        type_shape: Some(TypeShape::leaf(if has_uri {
            "RegistryChannel"
        } else {
            "Channel"
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_rholang(src: &str) -> tree_sitter::Tree {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_rholang::LANGUAGE.into())
            .expect("set_language rholang");
        p.parse(src, None).expect("parse")
    }

    fn first_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
        if node.kind() == kind {
            return Some(node);
        }
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            if let Some(found) = first_of_kind(child, kind) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn parameters_from_contract_formals() {
        let src = "contract @\"foo\"(@x, @y) = { Nil }";
        let tree = parse_rholang(src);
        let contract = first_of_kind(tree.root_node(), "contract").expect("contract");
        let formals = contract.child_by_field_name("formals").expect("formals");
        let params = parameters_from_formals(formals, src);
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name.as_deref(), Some("x"));
        assert_eq!(params[1].name.as_deref(), Some("y"));
        // Quoted process tag should be present on @x.
        assert!(
            params[0]
                .type_tags
                .contains(&v::TAG_QUOTED_PROCESS.to_string())
        );
        assert!(params[0].type_tags.contains(&v::TAG_PROCESS.to_string()));
        assert!(params[0].type_tags.contains(&v::TAG_NAME.to_string()));
    }

    #[test]
    fn effects_for_contract_includes_contract_define() {
        let src = "contract @\"foo\"() = { Nil }";
        let tree = parse_rholang(src);
        let contract = first_of_kind(tree.root_node(), "contract").expect("contract");
        let body = contract.child_by_field_name("proc").expect("body");
        let effects = effects_for_contract_body(body, src);
        assert!(effects.contains(&v::EFFECT_CONTRACT_DEFINE.to_string()));
    }

    #[test]
    fn effects_detects_send_single() {
        let src = "new x in { x!(\"hello\") }";
        let tree = parse_rholang(src);
        let block = first_of_kind(tree.root_node(), "block").expect("block");
        let effects = effects_for_block(block, src);
        assert!(effects.contains(&v::EFFECT_CHANNEL_SEND.to_string()));
    }

    #[test]
    fn effects_detects_linear_receive() {
        let src = "for(@m <- chan) { Nil }";
        let tree = parse_rholang(src);
        let root = tree.root_node();
        let effects = effects_for_block(root, src);
        assert!(effects.contains(&v::EFFECT_CHANNEL_RECEIVE_LINEAR.to_string()));
    }

    #[test]
    fn return_type_for_contract_marks_process_and_unit() {
        let rt = return_type_for_contract();
        assert!(rt.type_tags.contains(&v::TAG_PROCESS.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_UNIT.to_string()));
    }

    #[test]
    fn return_type_for_channel_marks_channel_name_process() {
        let rt = return_type_for_channel(false);
        assert!(rt.type_tags.contains(&v::TAG_CHANNEL.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_NAME.to_string()));
        assert!(rt.type_tags.contains(&v::TAG_PROCESS.to_string()));
        assert!(!rt.type_tags.contains(&v::TAG_REGISTRY_URI.to_string()));

        let rt_uri = return_type_for_channel(true);
        assert!(rt_uri.type_tags.contains(&v::TAG_REGISTRY_URI.to_string()));
    }
}
