//! F2 regression — the optional `project` parameter on `trigger_cron`.
//!
//! Adding `project` must not break existing callers: when absent it defaults to
//! `None` (all-projects behavior, unchanged), and when present it carries the
//! single-project scope through to the per-project cron entry points. No DB.

use pgmcp::mcp::server::TriggerCronParams;

#[test]
fn project_defaults_to_none_when_absent() {
    let p: TriggerCronParams =
        serde_json::from_value(serde_json::json!({ "job": "symbol-extraction" }))
            .expect("must deserialize without a project field (serde default)");
    assert_eq!(p.job, "symbol-extraction");
    assert_eq!(p.project, None, "absent project must default to None");
}

#[test]
fn project_is_parsed_when_present() {
    let p: TriggerCronParams =
        serde_json::from_value(serde_json::json!({ "job": "call-graph", "project": "pgmcp" }))
            .expect("must deserialize with a project field");
    assert_eq!(p.project.as_deref(), Some("pgmcp"));
}
