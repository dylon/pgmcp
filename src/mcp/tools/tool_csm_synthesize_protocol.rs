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

/// Work-item kinds that become protocol states (the doable units).
const ACTIONABLE_KINDS: &[&str] = &["task", "sub_task", "todo", "fixme", "bug", "action_item"];

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

/// `O→Wᵢ:tᵢ_req . Wᵢ→O:tᵢ_done . … . tail` — the worker request/response chain.
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

/// Fold the bound workers (+ optional critic) into a `GlobalType`.
fn build_protocol(orchestrator: &Role, workers: &[Worker], critic: Option<&Role>) -> GlobalType {
    match critic {
        // Critic-gated loop. The workers run once, then the loop verifies; on `revise`
        // the workers RE-RUN, so every worker faces a same-sender receive in BOTH choice
        // branches (`release` in pass vs `req` in revise) — the MPST external-choice merge
        // that keeps the bystander projectable. (Projecting a bare `Var` against a
        // `Recv` is unmergeable, which is why the revise branch re-engages the workers
        // rather than looping back directly.)
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
        // No critic: a linear, statically-drivable chain.
        None => workers_chain(orchestrator, workers, global::end()),
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

    // 2. Select actionable items: actionable kinds; else leaves; else the root.
    let child_of: BTreeSet<i64> = rows.iter().filter_map(|r| r.parent_id).collect();
    let actionable: Vec<&WorkItemRow> = {
        let by_kind: Vec<&WorkItemRow> = rows
            .iter()
            .filter(|r| ACTIONABLE_KINDS.contains(&r.kind.as_str()))
            .collect();
        if !by_kind.is_empty() {
            by_kind
        } else {
            let leaves: Vec<&WorkItemRow> =
                rows.iter().filter(|r| !child_of.contains(&r.id)).collect();
            if !leaves.is_empty() {
                leaves
            } else {
                rows.iter().collect()
            }
        }
    };
    if actionable.is_empty() {
        return Err(McpError::invalid_params(
            format!(
                "subtree of '{}' has no actionable items to synthesize",
                params.public_id
            ),
            None,
        ));
    }

    // 3. Bind each actionable item to a fleet peer (explicit override else default).
    let bindings: BTreeMap<&str, &str> = params
        .role_bindings
        .iter()
        .flatten()
        .map(|b| (b.public_id.as_str(), b.agent.as_str()))
        .collect();
    let workers: Vec<Worker> = actionable
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let peer = bindings
                .get(item.public_id.as_str())
                .map(|s| s.to_string())
                .or_else(|| item.assignee.clone().filter(|a| !a.trim().is_empty()))
                .unwrap_or_else(|| params.default_solver_agent.clone());
            Worker {
                role: Role::new(format!("W{i}")),
                task_public_id: item.public_id.clone(),
                task_title: item.title.clone(),
                peer,
            }
        })
        .collect();

    let orchestrator = Role::new("O");
    let critic_role = params.critic_agent.as_ref().map(|_| Role::new("C"));

    // 4. Fold → GlobalType.
    let g = build_protocol(&orchestrator, &workers, critic_role.as_ref());

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

    fn worker(i: usize) -> Worker {
        Worker {
            role: Role::new(format!("W{i}")),
            task_public_id: format!("p{i}"),
            task_title: format!("task {i}"),
            peer: "code-generator".to_string(),
        }
    }

    /// The Critic-gated loop must be well-formed, media-clean, and — critically —
    /// every worker role must still PROJECT despite being a bystander at the
    /// Choice. This is the `release`-branch merge (pass: Recv(release) vs revise:
    /// Recv(req), same sender O) that keeps the bystander projectable.
    #[test]
    fn critic_loop_well_formed_and_projects() {
        let o = Role::new("O");
        let c = Role::new("C");
        let workers = vec![worker(0), worker(1)];
        let g = build_protocol(&o, &workers, Some(&c));

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

    /// Without a critic the fold is a linear chain: well-formed, projectable, and
    /// statically drivable (one request/response step per worker).
    #[test]
    fn linear_chain_is_drivable() {
        let o = Role::new("O");
        let workers = vec![worker(0), worker(1)];
        let g = build_protocol(&o, &workers, None);

        assert!(well_formed(&g).is_ok());
        for role in g.participants() {
            assert!(project(&g, &role).is_ok());
        }

        let net = Network::build("test", &g).expect("network builds");
        let plan = ProtocolDriver::plan(&net, &o).expect("linear chain is drivable");
        assert_eq!(plan.len(), 2);
    }
}
