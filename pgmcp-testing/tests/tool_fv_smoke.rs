//! Smoke + coverage tests for the native formal-verification tools (`router_fv`,
//! Task #22 §4-A / ADR-032). Each test drives a tool through
//! `McpServer::call_tool_cli` with a valid inline spec and asserts a well-formed,
//! correct verdict. These are in-process (no subprocess, no prattail); only the
//! server construction touches the DB. This file also satisfies
//! `query_inventory_vs_coverage::every_dispatched_tool_has_an_integration_test`.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(result)).expect("FV tool body must be JSON")
}

#[tokio::test]
async fn fv_protocol_soundness_end_is_deadlock_free() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    // The `End` global type is well-formed ⇒ deadlock-free + progress by typing
    // (CsmDeadlockFreedom.v). Adjacent-tagged JSON for GlobalType::End.
    let r = server
        .call_tool_cli("protocol_soundness", json!({"global_type": {"type": "end"}}))
        .await
        .expect("protocol_soundness must not error on a valid GlobalType");
    let v = body(&r);
    assert_eq!(v["well_formed"].as_bool(), Some(true));
    assert_eq!(v["deadlock_free"].as_bool(), Some(true));
    assert_eq!(v["has_progress"].as_bool(), Some(true));
}

#[tokio::test]
async fn fv_language_inclusion_identical_automaton_is_included() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    // A 2-state SFA accepting the single-symbol words in [0,10).
    let sfa = json!({
        "num_states": 2, "initial": 0, "accepting": [1],
        "transitions": [{"from": 0, "to": 1, "lo": 0, "hi": 10}]
    });
    let r = server
        .call_tool_cli(
            "language_inclusion",
            json!({"impl_sfa": sfa.clone(), "spec_sfa": sfa}),
        )
        .await
        .expect("language_inclusion must not error");
    // L(A) ⊆ L(A) holds for any A.
    assert_eq!(body(&r)["included"].as_bool(), Some(true));
}

#[tokio::test]
async fn fv_presburger_decide_satisfiable_inequality() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    // x₀ ≤ 5 is satisfiable.
    let r = server
        .call_tool_cli(
            "presburger_decide",
            json!({"formula": {"op": "atom", "terms": [[0, 1]], "rhs": 5, "rel": "le"}, "bit_width": 8}),
        )
        .await
        .expect("presburger_decide must not error");
    assert_eq!(body(&r)["satisfiable"].as_bool(), Some(true));
}

#[tokio::test]
async fn fv_behavioral_check_ex_holds_at_initial() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    // s0 → s1, with `q` labelling s1: EX q holds at s0.
    let r = server
        .call_tool_cli(
            "behavioral_check",
            json!({
                "num_states": 2, "initial": 0,
                "transitions": [{"from": 0, "to": 1}],
                "labels": [[], ["q"]],
                "formula": {"op": "ex", "inner": {"op": "atom", "prop": "q"}}
            }),
        )
        .await
        .expect("behavioral_check must not error");
    assert_eq!(body(&r)["holds"].as_bool(), Some(true));
}

#[tokio::test]
async fn fv_kat_hoare_check_valid_triple() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    // {x} x := true {x} is valid.
    let r = server
        .call_tool_cli(
            "kat_hoare_check",
            json!({
                "atoms": ["x"],
                "precond": {"op": "atom", "name": "x"},
                "program": [{"kind": "assign", "var": "x", "value": true}],
                "postcond": {"op": "atom", "name": "x"}
            }),
        )
        .await
        .expect("kat_hoare_check must not error");
    assert_eq!(body(&r)["valid"].as_bool(), Some(true));
}

#[tokio::test]
async fn fv_effect_verify_no_reachable_effects_conforms() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    // A non-existent seed symbol has no reachable effects, so reachable ⊆ allowed
    // holds vacuously (conforms = true) — exercises the effect-layer query + verdict.
    let r = server
        .call_tool_cli(
            "effect_verify",
            json!({"seed_symbol_id": -1, "allowed_effects": ["pure"], "max_depth": 1}),
        )
        .await
        .expect("effect_verify must not error on an empty reachable set");
    assert_eq!(body(&r)["conforms"].as_bool(), Some(true));
}
