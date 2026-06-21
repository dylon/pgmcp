//! `csm_synthesize_protocol` — fold a plan (a work-item subtree) into a typed,
//! possibly-cyclic Multiparty-Session-Type `GlobalType`, validate + project it, and
//! emit a client-drivable plan (Crucible E5).
//!
//! This is the keystone of the "plan → state machine → orchestrator" pipeline: a
//! Planner's work-item tree (ingested via `work_item_ingest_plan`) becomes a typed
//! protocol whose states bind to fleet specialists, with an optional **Critic-gated
//! loop** (the cyclic case) expressed as `Rec`/`Var` with a sender-driven `Choice`.
//!
//! pgmcp SYNTHESIZES / VALIDATES / PROJECTS / DRIVES-coordination only — it emits a
//! plan and never touches files or executes work; the orchestrator (pi) drives it.
//!
//! ## The fold
//! Actionable leaf items `T0..Tn` (in subtree order) become a linear request/response
//! chain `O→Wᵢ:tᵢ_req . Wᵢ→O:tᵢ_done`. With a `critic_agent`, the workers run once and
//! then a Critic loop gates completion:
//! `<chain> . μloop. O→C:verify_req . C→O { pass: O→Wᵢ:tᵢ_release.end,
//! revise: O→Wᵢ:tᵢ_req.Wᵢ→O:tᵢ_done.loop }`. Re-running the workers in the `revise`
//! branch makes every worker face a same-sender receive in BOTH branches (`release`
//! vs `req`) — the MPST external-choice merge that keeps the bystander projectable
//! (a bare loop-`Var` would be unmergeable against the pass-branch `Recv`). Without a
//! critic, the chain just ends (a statically drivable linear protocol).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use sqlx::PgPool;

use crate::context::SystemContext;
use crate::csm::driver::ProtocolDriver;
use crate::csm::machine::Network;
use crate::csm::media::check_media_discipline;
use crate::csm::mpst::global::{self, GlobalType};
use crate::csm::mpst::project::project;
use crate::csm::mpst::wellformed::well_formed;
use crate::csm::role::{Label, Role};
use crate::db::queries::{WorkItemRow, get_work_item_by_public_id, get_work_item_subtree};
use crate::mcp::server::CsmSynthesizeProtocolParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

/// Soft advisory threshold above which a synthesized protocol is unusually large.
const LARGE_PROTOCOL_HINT: usize = 32;

/// A single bound worker state: its role, the work item it represents, and the
/// fleet peer chosen to fill it.
struct Worker {
    role: Role,
    task_public_id: String,
    task_title: String,
    peer: String,
}

/// The minimal view of a work item the hierarchy-preserving fold needs (id,
/// tree edge, identity, and the optional assignee). Decoupling the fold from the
/// full [`WorkItemRow`] keeps [`synth_node`] / [`assign_workers`] unit-testable
/// without constructing a 30-column DB row.
struct PlanItem {
    id: i64,
    parent_id: Option<i64>,
    public_id: String,
    title: String,
    assignee: Option<String>,
}

impl PlanItem {
    fn from_row(r: &WorkItemRow) -> Self {
        PlanItem {
            id: r.id,
            parent_id: r.parent_id,
            public_id: r.public_id.clone(),
            title: r.title.clone(),
            assignee: r.assignee.clone(),
        }
    }
}

/// Soft peer→url lookup (does not fail when a peer is not yet registered — this is
/// a synthesis/preview tool, not an executor).
async fn lookup_url(pool: &PgPool, name: &str) -> Result<Option<String>, McpError> {
    sqlx::query_scalar::<_, String>("SELECT url FROM a2a_agents WHERE name = $1")
        .bind(name)
        .fetch_optional(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("agent lookup failed: {e}"), None))
}

/// The release sequence used in the Critic's `pass` branch: `O→Wᵢ:tᵢ_release … . end`.
fn build_releases(orchestrator: &Role, workers: &[Worker]) -> GlobalType {
    workers
        .iter()
        .enumerate()
        .rev()
        .fold(global::end(), |cont, (i, w)| {
            global::interaction(
                orchestrator.clone(),
                w.role.clone(),
                Label::text(format!("t{i}_release")),
                cont,
            )
        })
}

/// `O→Wᵢ:tᵢ_req . Wᵢ→O:tᵢ_done . … . tail` — the flat worker request/response
/// chain. Used by the **Critic-gated** mode, whose verify/revise loop re-runs the
/// leaf set; hierarchy preservation applies to the linear (non-Critic) synthesis
/// via [`synth_node`].
fn workers_chain(orchestrator: &Role, workers: &[Worker], tail: GlobalType) -> GlobalType {
    workers.iter().enumerate().rev().fold(tail, |cont, (i, w)| {
        global::interaction(
            orchestrator.clone(),
            w.role.clone(),
            Label::text(format!("t{i}_req")),
            global::interaction(
                w.role.clone(),
                orchestrator.clone(),
                Label::text(format!("t{i}_done")),
                cont,
            ),
        )
    })
}

/// The **hierarchy-PRESERVING** fold (ADR-030): synthesize the protocol for the
/// plan subtree rooted at `idx`, continuing as `tail` after it completes. A leaf
/// work-item (a doable unit) becomes a worker request/response
/// `O→W:<id>_req . W→O:<id>_done`; an **interior** item becomes a hierarchical
/// `GlobalBox` composite state `box⟨enter_<id>⟩{ children… }⟨exit_<id>⟩` whose
/// body is its children in subtree order. The work-item tree's nesting is thus
/// carried into the protocol as nested HSM boxes — a Planner's *hierarchical*
/// plan projects to a genuinely hierarchical (pushdown) protocol, not the
/// flattened linear chain the pre-ADR-030 fold produced. Boundary labels are
/// keyed by `public_id` (globally unique ⇒ the visibly-pushdown WF-VPA check
/// holds: each label maps to exactly one stack action).
fn synth_node(
    idx: usize,
    rows: &[PlanItem],
    kids: &BTreeMap<i64, Vec<usize>>,
    worker_role: &BTreeMap<i64, Role>,
    orchestrator: &Role,
    tail: GlobalType,
) -> GlobalType {
    let row = &rows[idx];
    match kids.get(&row.id) {
        Some(children) if !children.is_empty() => global::gbox(
            Label::text(format!("enter_{}", row.public_id)),
            synth_seq(
                children,
                rows,
                kids,
                worker_role,
                orchestrator,
                global::end(),
            ),
            Label::text(format!("exit_{}", row.public_id)),
            tail,
        ),
        // A leaf bound to a worker role is a request/response unit; an unbound
        // leaf (none assigned) contributes nothing but the continuation.
        _ => match worker_role.get(&row.id) {
            Some(w) => global::interaction(
                orchestrator.clone(),
                w.clone(),
                Label::text(format!("{}_req", row.public_id)),
                global::interaction(
                    w.clone(),
                    orchestrator.clone(),
                    Label::text(format!("{}_done", row.public_id)),
                    tail,
                ),
            ),
            None => tail,
        },
    }
}

/// Synthesize a sequence of sibling subtrees in order, continuing as `tail`.
fn synth_seq(
    idxs: &[usize],
    rows: &[PlanItem],
    kids: &BTreeMap<i64, Vec<usize>>,
    worker_role: &BTreeMap<i64, Role>,
    orchestrator: &Role,
    tail: GlobalType,
) -> GlobalType {
    idxs.iter().rev().fold(tail, |cont, &i| {
        synth_node(i, rows, kids, worker_role, orchestrator, cont)
    })
}

/// Children indices per parent id, in subtree (row) order — the plan tree
/// reconstructed from the flat `WITH RECURSIVE` row set.
fn children_map(rows: &[PlanItem]) -> BTreeMap<i64, Vec<usize>> {
    let mut kids: BTreeMap<i64, Vec<usize>> = BTreeMap::new();
    for (i, r) in rows.iter().enumerate() {
        if let Some(p) = r.parent_id {
            kids.entry(p).or_default().push(i);
        }
    }
    kids
}

/// Assign a worker role to every **leaf** of the subtree (the doable units), in
/// subtree order, binding each to a fleet peer (explicit override, else the
/// item's assignee, else the default solver). Interior nodes become boxes and get
/// no worker. Returns `(workers, node-id → role)`.
fn assign_workers(
    root_idx: usize,
    rows: &[PlanItem],
    kids: &BTreeMap<i64, Vec<usize>>,
    bindings: &BTreeMap<&str, &str>,
    default_agent: &str,
) -> (Vec<Worker>, BTreeMap<i64, Role>) {
    let mut workers = Vec::new();
    let mut worker_role = BTreeMap::new();
    fn walk(
        idx: usize,
        rows: &[PlanItem],
        kids: &BTreeMap<i64, Vec<usize>>,
        bindings: &BTreeMap<&str, &str>,
        default_agent: &str,
        workers: &mut Vec<Worker>,
        worker_role: &mut BTreeMap<i64, Role>,
    ) {
        let row = &rows[idx];
        match kids.get(&row.id) {
            Some(children) if !children.is_empty() => {
                for &c in children {
                    walk(c, rows, kids, bindings, default_agent, workers, worker_role);
                }
            }
            _ => {
                let role = Role::new(format!("W{}", workers.len()));
                worker_role.insert(row.id, role.clone());
                let peer = bindings
                    .get(row.public_id.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| row.assignee.clone().filter(|a| !a.trim().is_empty()))
                    .unwrap_or_else(|| default_agent.to_string());
                workers.push(Worker {
                    role,
                    task_public_id: row.public_id.clone(),
                    task_title: row.title.clone(),
                    peer,
                });
            }
        }
    }
    walk(
        root_idx,
        rows,
        kids,
        bindings,
        default_agent,
        &mut workers,
        &mut worker_role,
    );
    (workers, worker_role)
}

/// Fold the plan subtree (+ optional critic) into a `GlobalType`.
///
/// - **No critic:** the hierarchy-preserving fold ([`synth_node`]) — the plan
///   tree's nesting becomes nested `GlobalBox` composite states (ADR-030).
/// - **Critic-gated:** the flat verify/revise loop over the leaf set (the loop
///   re-runs the workers; a hierarchical revise would face an unmergeable
///   bystander projection, so the Critic mode flattens — the useful, projectable
///   form). The `release`/`req` same-sender receives in both branches are the
///   MPST external-choice merge that keeps each worker bystander projectable.
#[allow(clippy::too_many_arguments)]
fn build_protocol(
    orchestrator: &Role,
    root_idx: usize,
    rows: &[PlanItem],
    kids: &BTreeMap<i64, Vec<usize>>,
    worker_role: &BTreeMap<i64, Role>,
    workers: &[Worker],
    critic: Option<&Role>,
) -> GlobalType {
    match critic {
        Some(c) => {
            let loop_node = global::rec(
                "loop",
                global::interaction(
                    orchestrator.clone(),
                    c.clone(),
                    Label::text("verify_req"),
                    global::choice(
                        c.clone(),
                        orchestrator.clone(),
                        vec![
                            // pass: release every worker, then terminate.
                            global::gbranch(
                                Label::text("pass"),
                                build_releases(orchestrator, workers),
                            ),
                            // revise: re-run the workers, then loop back to the verify.
                            global::gbranch(
                                Label::text("revise"),
                                workers_chain(orchestrator, workers, global::var("loop")),
                            ),
                        ],
                    ),
                ),
            );
            // Initial worker run, then the verify/revise loop.
            workers_chain(orchestrator, workers, loop_node)
        }
        // No critic: the hierarchy-preserving nested-box protocol.
        None => synth_node(
            root_idx,
            rows,
            kids,
            worker_role,
            orchestrator,
            global::end(),
        ),
    }
}

pub async fn tool_csm_synthesize_protocol(
    ctx: &SystemContext,
    params: CsmSynthesizeProtocolParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    // 1. Resolve the plan root + subtree.
    let root = get_work_item_by_public_id(pool, &params.public_id)
        .await
        .map_err(|e| McpError::internal_error(format!("work-item lookup failed: {e}"), None))?
        .ok_or_else(|| {
            McpError::invalid_params(format!("no work item '{}'", params.public_id), None)
        })?;
    let max_rows = params.max_rows.unwrap_or(200).clamp(1, 100_000);
    let rows = get_work_item_subtree(pool, root.id, max_rows)
        .await
        .map_err(|e| McpError::internal_error(format!("subtree query failed: {e}"), None))?;

    // 2. Reconstruct the plan tree and bind a worker role to every LEAF (the
    //    doable units), in subtree order. Interior items become hierarchical
    //    `GlobalBox` composite states (ADR-030), so the plan's nesting is
    //    preserved in the synthesized protocol rather than flattened.
    let items: Vec<PlanItem> = rows.iter().map(PlanItem::from_row).collect();
    let root_idx = items
        .iter()
        .position(|r| r.id == root.id)
        .ok_or_else(|| McpError::internal_error("subtree query omitted its root row", None))?;
    let kids = children_map(&items);
    let bindings: BTreeMap<&str, &str> = params
        .role_bindings
        .iter()
        .flatten()
        .map(|b| (b.public_id.as_str(), b.agent.as_str()))
        .collect();
    let (workers, worker_role) = assign_workers(
        root_idx,
        &items,
        &kids,
        &bindings,
        &params.default_solver_agent,
    );
    if workers.is_empty() {
        return Err(McpError::invalid_params(
            format!(
                "subtree of '{}' has no leaf items to synthesize",
                params.public_id
            ),
            None,
        ));
    }

    let orchestrator = Role::new("O");
    let critic_role = params.critic_agent.as_ref().map(|_| Role::new("C"));

    // 3. Fold → GlobalType (hierarchy-preserving unless Critic-gated).
    let g = build_protocol(
        &orchestrator,
        root_idx,
        &items,
        &kids,
        &worker_role,
        &workers,
        critic_role.as_ref(),
    );

    // 5. Validate: well-formedness, then the black-box media discipline, then
    //    projectability per role. Black-box set = every participant (all fleet peers
    //    are black-box, Text-only); since the fold emits only Text labels this always
    //    passes — a Latent edge would be the only thing it could reject.
    let participants: Vec<Role> = g.participants().into_iter().collect();
    let black_box: BTreeSet<Role> = participants.iter().cloned().collect();

    let (well_formed_ok, well_formed_error) = match well_formed(&g) {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.message())),
    };
    let (media_ok, media_error) = match check_media_discipline(&g, &black_box) {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.message())),
    };
    let projections: Vec<serde_json::Value> = participants
        .iter()
        .map(|role| match project(&g, role) {
            Ok(_) => json!({ "role": role.as_str(), "projectable": true }),
            Err(e) => {
                json!({ "role": role.as_str(), "projectable": false, "error": e.message() })
            }
        })
        .collect();

    // 6. Build the network + (if linear) the drivable plan. role → peer map for the
    //    plan output.
    let mut role_peer: BTreeMap<String, String> = BTreeMap::new();
    role_peer.insert(
        "O".to_string(),
        params
            .orchestrator
            .clone()
            .unwrap_or_else(|| "pi".to_string()),
    );
    for w in &workers {
        role_peer.insert(w.role.as_str().to_string(), w.peer.clone());
    }
    if let (Some(cr), Some(ca)) = (critic_role.as_ref(), params.critic_agent.as_ref()) {
        role_peer.insert(cr.as_str().to_string(), ca.clone());
    }

    let (drivable, plan, drive_reason, build_error) =
        match Network::build(format!("synthesized:{}", params.public_id), &g) {
            Ok(net) => match ProtocolDriver::plan(&net, &orchestrator) {
                Some(steps) => {
                    let plan: Vec<serde_json::Value> = steps
                        .iter()
                        .map(|s| {
                            let peer_role = s.peer.as_str();
                            json!({
                                "peer_role": peer_role,
                                "agent": role_peer.get(peer_role),
                                "request": s.request.name,
                                "response": s.response.name,
                            })
                        })
                        .collect();
                    (true, Some(plan), None, None)
                }
                None => (
                    false,
                    None,
                    Some(
                        "the orchestrator faces a sender-driven Choice (the Critic gate) \
                         resolved at runtime; drive it client-side, looping on the Critic's \
                         `revise` branch until `pass`"
                            .to_string(),
                    ),
                    None,
                ),
            },
            Err(e) => (false, None, None, Some(e.message())),
        };

    // 7. Soft-resolve each bound peer's URL (unregistered peers are reported, not fatal).
    let mut role_bindings_out: Vec<serde_json::Value> = Vec::with_capacity(workers.len() + 2);
    role_bindings_out.push(json!({
        "role": "O",
        "agent": role_peer.get("O"),
        "is_orchestrator": true,
    }));
    for w in &workers {
        let url = lookup_url(pool, &w.peer).await?;
        let registered = url.is_some();
        role_bindings_out.push(json!({
            "role": w.role.as_str(),
            "agent": w.peer,
            "url": url,
            "registered": registered,
            "task_public_id": w.task_public_id,
            "task_title": w.task_title,
        }));
    }
    if let (Some(cr), Some(ca)) = (critic_role.as_ref(), params.critic_agent.as_ref()) {
        let url = lookup_url(pool, ca).await?;
        let registered = url.is_some();
        role_bindings_out.push(json!({
            "role": cr.as_str(),
            "agent": ca,
            "url": url,
            "registered": registered,
            "is_critic": true,
        }));
    }

    let global_type = serde_json::to_value(&g)
        .map_err(|e| McpError::internal_error(format!("serialize GlobalType: {e}"), None))?;

    json_result(&json!({
        "public_id": params.public_id,
        "root_title": root.title,
        "actionable_count": workers.len(),
        "large_protocol": workers.len() > LARGE_PROTOCOL_HINT,
        "participants": participants.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
        "global_type": global_type,
        "well_formed": well_formed_ok,
        "well_formed_error": well_formed_error,
        "media_ok": media_ok,
        "media_error": media_error,
        "projections": projections,
        "role_bindings": role_bindings_out,
        "drivable": drivable,
        "plan": plan,
        "drive_reason": drive_reason,
        "build_error": build_error,
        "next": "register any unregistered peers (a2a_register_agent / fleet/register-fleet.sh), \
                 then drive the plan client-side (or trigger a fixed a2a_pattern_* topology); \
                 call csm_validate_run(task_id) on the recorded run for a conformance verdict",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::conformance::{Event, check_conformance};
    use crate::csm::machine::EdgeKind;

    fn item(id: i64, parent: Option<i64>, public_id: &str) -> PlanItem {
        PlanItem {
            id,
            parent_id: parent,
            public_id: public_id.to_string(),
            title: format!("task {id}"),
            assignee: None,
        }
    }

    /// The Critic-gated mode flattens to the leaf set: well-formed, media-clean,
    /// every worker projects (the `release`/`req` same-sender merge that keeps the
    /// bystander projectable), and NOT statically drivable (O faces the verify
    /// Choice, resolved at runtime).
    #[test]
    fn critic_mode_is_flat_well_formed_and_projects() {
        // root → { a, b } (two leaves).
        let items = vec![
            item(1, None, "root"),
            item(2, Some(1), "a"),
            item(3, Some(1), "b"),
        ];
        let kids = children_map(&items);
        let (workers, worker_role) =
            assign_workers(0, &items, &kids, &BTreeMap::new(), "code-generator");
        assert_eq!(workers.len(), 2, "two leaves ⇒ two workers");

        let o = Role::new("O");
        let c = Role::new("C");
        let g = build_protocol(&o, 0, &items, &kids, &worker_role, &workers, Some(&c));

        assert!(
            well_formed(&g).is_ok(),
            "well_formed failed: {:?}",
            well_formed(&g).err().map(|e| e.message())
        );
        let bb: BTreeSet<Role> = g.participants().into_iter().collect();
        assert!(check_media_discipline(&g, &bb).is_ok());
        for role in g.participants() {
            assert!(
                project(&g, &role).is_ok(),
                "role {} did not project: {:?}",
                role.as_str(),
                project(&g, &role).err().map(|e| e.message())
            );
        }
        // A Critic-gated loop is NOT a static linear chain (O faces a Choice).
        let net = Network::build("test", &g).expect("network builds");
        assert!(ProtocolDriver::plan(&net, &o).is_none());
    }

    /// The non-Critic mode PRESERVES the plan hierarchy (ADR-030): interior items
    /// become nested `GlobalBox` composite states. The synthesized protocol is
    /// well-formed, projects for every role, compiles to genuine pushdown
    /// `Call`/`Return` boundary edges (not a flat chain), and a complete run
    /// conforms (well-nested) — the operational proof that a *hierarchical*
    /// crucible plan becomes a hierarchical/pushdown protocol.
    #[test]
    fn hierarchy_is_preserved_as_nested_boxes_and_conforms() {
        // root → phase → { a, b }: a two-level nesting.
        let items = vec![
            item(1, None, "root"),
            item(2, Some(1), "phase"),
            item(3, Some(2), "a"),
            item(4, Some(2), "b"),
        ];
        let kids = children_map(&items);
        let (workers, worker_role) =
            assign_workers(0, &items, &kids, &BTreeMap::new(), "code-generator");
        assert_eq!(workers.len(), 2, "two leaves a,b ⇒ two workers");

        let o = Role::new("O");
        let g = build_protocol(&o, 0, &items, &kids, &worker_role, &workers, None);

        assert!(
            well_formed(&g).is_ok(),
            "well_formed failed: {:?}",
            well_formed(&g).err().map(|e| e.message())
        );
        for role in g.participants() {
            assert!(
                project(&g, &role).is_ok(),
                "role {} did not project: {:?}",
                role.as_str(),
                project(&g, &role).err().map(|e| e.message())
            );
        }

        let net = Network::build("test", &g).expect("network builds");
        let om = net.machine(&o).expect("orchestrator machine");
        assert!(
            om.edges
                .iter()
                .any(|e| matches!(e.kind, EdgeKind::Call { .. })),
            "a hierarchical plan must compile to pushdown Call edges, not a flat chain"
        );

        // A complete run is well-nested and conforms. The trace carries only the
        // real worker communications; the enter/exit box boundaries are taken by
        // the conformance ε-closure.
        let w0 = workers[0].role.clone();
        let w1 = workers[1].role.clone();
        let trace = vec![
            Event::new(o.clone(), w0.clone(), Label::text("a_req")),
            Event::new(w0, o.clone(), Label::text("a_done")),
            Event::new(o.clone(), w1.clone(), Label::text("b_req")),
            Event::new(w1, o.clone(), Label::text("b_done")),
        ];
        check_conformance(&net, &trace)
            .unwrap_or_else(|e| panic!("hierarchical run should conform: {}", e.message()));
    }
}
