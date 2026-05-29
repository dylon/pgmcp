//! Experiment bridge tool (Phase 10): `work_item_link_experiment`.
//!
//! Links a tracker work_item to a scientific experiment (`work_item_experiment`
//! bridge) and seeds an `experiment_verdict` acceptance criterion. Once linked,
//! the experiment gains the tracker's priority/tags/progress/roll-up/claiming,
//! and `experiment_decide` posts the engine's statistical verdict back as
//! trusted (`source='experiment'`) verification evidence — auto-verifying the
//! task on an accepted hypothesis through the normal gatekeeper path.
//!
//! If no `work_item_public_id` is given, a `kind='experiment'` tracking task is
//! created from the experiment's title/question (embedded on write).

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries::{self, NewWorkItem};
use crate::mcp::server::WorkItemLinkExperimentParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::{id_of_public, map_db_err};
use crate::mcp::tools::work_items::gen_public_id;

pub async fn tool_work_item_link_experiment(
    ctx: &SystemContext,
    params: WorkItemLinkExperimentParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let slug = params.experiment_slug.trim();
    if slug.is_empty() {
        return Err(McpError::invalid_params(
            "experiment_slug must be non-empty",
            None,
        ));
    }
    // Resolve the (active) experiment by slug.
    let exp = queries::get_experiment_core(pool, None, Some(slug))
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| {
            McpError::invalid_params(format!("no experiment with slug '{slug}'"), None)
        })?;

    // Resolve an existing work_item, or create a `kind=experiment` tracking task.
    let (work_item_id, work_item_public_id, created) = match params
        .work_item_public_id
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        Some(pid) => (id_of_public(pool, pid).await?, pid.to_string(), false),
        None => {
            let title = params
                .title
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(&exp.title);
            let public_id = gen_public_id(title);
            let embedding = super::embed_title_body(ctx, title, Some(&exp.question), None).await;
            let new_item = NewWorkItem {
                public_id: &public_id,
                kind: "experiment",
                title,
                body: Some(&exp.question),
                origin: "agent_write",
                embedding,
                ..Default::default()
            };
            let id = queries::insert_work_item(pool, new_item)
                .await
                .map_err(map_db_err)?;
            ctx.stats()
                .work_items_created
                .fetch_add(1, Ordering::Relaxed);
            (id, public_id, true)
        }
    };

    // Insert the bridge row (idempotent).
    queries::link_work_item_experiment(pool, work_item_id, exp.id, params.hypothesis_id, slug)
        .await
        .map_err(map_db_err)?;

    // Seed the experiment_verdict acceptance criterion (unless one already
    // exists or the caller opted out). The acceptance_uri pins which experiment
    // (and optionally hypothesis) supplies the verdict.
    let seed = params.seed_criterion.unwrap_or(true);
    let mut criterion_id = queries::experiment_verdict_criterion_id(pool, work_item_id)
        .await
        .map_err(map_db_err)?;
    if seed && criterion_id.is_none() {
        let uri = match params.hypothesis_id {
            Some(h) => format!("experiment://{slug}::hypothesis/{h}"),
            None => format!("experiment://{slug}"),
        };
        let cid = queries::insert_acceptance_criterion(
            pool,
            work_item_id,
            "experiment_verdict",
            "The pre-registered hypothesis is accepted by the statistical engine over the frozen criterion.",
            Some(&uri),
            None,
            "single",
            None,
            true,
        )
        .await
        .map_err(|e| McpError::invalid_params(format!("criterion rejected: {e}"), None))?;
        criterion_id = Some(cid);
    }

    json_result(&json!({
        "linked": true,
        "work_item_public_id": work_item_public_id,
        "work_item_created": created,
        "experiment_slug": exp.slug,
        "experiment_id": exp.id,
        "experiment_status": exp.status,
        "hypothesis_id": params.hypothesis_id,
        "criterion_id": criterion_id,
    }))
}
