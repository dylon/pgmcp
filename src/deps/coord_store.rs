//! Coordination store: the `WorktreeNegotiation` state machine over
//! `coordination_requests`, plus the git-scanner `project_event` gatekeeper that
//! resolves them.
//!
//! Enforces the trust boundary proven in `docs/formal/WorktreeNegotiation.{tla,v}`
//! (and machine-checked by TLC + TLAPS + Rocq): an editor agent can drive a
//! request to `moved` (a candidate), but **only** [`resolve_on_stable`] — driven
//! by a git-scanner `stable_restored` event — may set `resolved`. [`respond`]
//! refuses any agent attempt to set `resolved`.

use serde::Serialize;
use sqlx::PgPool;

use crate::deps::coordination::CoordinationStatus;

/// Open a coordination request (a dependent's agent asks the dependency's editor
/// to move in-flight edits to a worktree). Returns the new request id.
#[allow(clippy::too_many_arguments)]
pub async fn open_request(
    pool: &PgPool,
    dependent_project_id: Option<i32>,
    dependency_project_id: i32,
    requester_agent: Option<&str>,
    requester_session: Option<&str>,
    reason: Option<&str>,
    error_excerpt: Option<&str>,
    message_id: Option<i64>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO coordination_requests
            (dependent_project_id, dependency_project_id, requester_agent,
             requester_session, reason, error_excerpt, message_id, status)
         VALUES ($1,$2,$3,$4,$5,$6,$7,'pending')
         RETURNING id",
    )
    .bind(dependent_project_id)
    .bind(dependency_project_id)
    .bind(requester_agent)
    .bind(requester_session)
    .bind(reason)
    .bind(error_excerpt)
    .bind(message_id)
    .fetch_one(pool)
    .await
}

/// An editor responds to a request: `accepted` | `declined` | `moved`. Refuses
/// `resolved` (the gatekeeper trust boundary — agent-unreachable). Returns
/// `false` if the status is not agent-settable or the request is already
/// terminal (resolved/cancelled).
pub async fn respond(
    pool: &PgPool,
    request_id: i64,
    status: CoordinationStatus,
    editor_session: Option<&str>,
    worktree_branch: Option<&str>,
) -> Result<bool, sqlx::Error> {
    if !status.is_agent_settable() {
        return Ok(false); // `resolved` is reserved for the git-scanner gatekeeper
    }
    let res = sqlx::query(
        "UPDATE coordination_requests
            SET status = $2,
                editor_session  = COALESCE($3, editor_session),
                worktree_branch = COALESCE($4, worktree_branch)
          WHERE id = $1 AND status NOT IN ('resolved', 'cancelled')",
    )
    .bind(request_id)
    .bind(status.as_str())
    .bind(editor_session)
    .bind(worktree_branch)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// A resolved coordination request — returned by the gatekeeper so the caller
/// can notify the requester that it is unblocked and (when one is linked)
/// auto-unblock the requester's gated work-item.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ResolvedRequest {
    pub id: i64,
    pub dependent_project_id: Option<i32>,
    pub requester_agent: Option<String>,
    pub requester_session: Option<String>,
    /// The §4.5 gated work-item (if the requester named one): the git-scanner
    /// gatekeeper flips it `blocked → ready` as `Actor::System` on resolve.
    pub blocked_work_item_id: Option<i64>,
}

/// GATEKEEPER: a `stable_restored` event for `dependency_project_id` resolves
/// every open (`pending`/`accepted`/`moved`) request against it. This is the
/// ONLY path to `resolved`. Returns the resolved requests (for unblock
/// notification). Mirrors the work-item `System` auto-unblock cascade.
pub async fn resolve_on_stable(
    pool: &PgPool,
    dependency_project_id: i32,
) -> Result<Vec<ResolvedRequest>, sqlx::Error> {
    sqlx::query_as::<_, ResolvedRequest>(
        "UPDATE coordination_requests
            SET status = 'resolved', resolved_at = now()
          WHERE dependency_project_id = $1
            AND status IN ('pending', 'accepted', 'moved')
        RETURNING id, dependent_project_id, requester_agent, requester_session,
                  blocked_work_item_id",
    )
    .bind(dependency_project_id)
    .fetch_all(pool)
    .await
}

/// One coordination request as seen by an editor on the dependency project.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CoordRow {
    pub id: i64,
    pub dependent_project_id: Option<i32>,
    pub dependency_project_id: i32,
    pub requester_agent: Option<String>,
    pub status: String,
    pub reason: Option<String>,
    pub error_excerpt: Option<String>,
    pub worktree_branch: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Open coordination requests against a dependency project (for its editor).
pub async fn open_for_dependency(
    pool: &PgPool,
    dependency_project_id: i32,
) -> Result<Vec<CoordRow>, sqlx::Error> {
    sqlx::query_as::<_, CoordRow>(
        "SELECT id, dependent_project_id, dependency_project_id, requester_agent,
                status, reason, error_excerpt, worktree_branch, created_at
           FROM coordination_requests
          WHERE dependency_project_id = $1
            AND status IN ('pending', 'accepted', 'moved')
          ORDER BY created_at DESC",
    )
    .bind(dependency_project_id)
    .fetch_all(pool)
    .await
}

/// One dirty, actively-edited dependency the dependent is not yet coordinating
/// about — a §4.6 proactive-warning row.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct DependencyWarning {
    pub dependency_name: String,
    /// Comma-joined `client_name`s of the live editors on the dependency.
    pub editors: Option<String>,
}

/// §4.6 proactive warnings: the live dependencies of `dependent_project_id` that
/// are (a) on a dirty working tree, (b) have ≥1 alive MCP-client editor, and (c)
/// are NOT yet under an open coordination request from this project. The
/// open-request check is the dedup — once the dependent runs
/// `coordinate_dependency_block`, the warning falls silent. Bounded to `limit`.
pub async fn pending_dependency_warnings(
    pool: &PgPool,
    dependent_project_id: i32,
    limit: i64,
) -> Result<Vec<DependencyWarning>, sqlx::Error> {
    sqlx::query_as::<_, DependencyWarning>(
        "SELECT u.name AS dependency_name,
                (SELECT string_agg(DISTINCT c.client_name, ', ')
                   FROM mcp_clients c WHERE c.project_id = u.id AND c.alive) AS editors
           FROM project_dependencies pd
           JOIN projects u ON u.id = pd.dependency_project_id
          WHERE pd.dependent_project_id = $1
            AND pd.valid_to IS NULL
            AND u.git_dirty = TRUE
            AND EXISTS (SELECT 1 FROM mcp_clients c WHERE c.project_id = u.id AND c.alive)
            AND NOT EXISTS (
                SELECT 1 FROM coordination_requests cr
                 WHERE cr.dependent_project_id = $1
                   AND cr.dependency_project_id = u.id
                   AND cr.status IN ('pending', 'accepted', 'moved'))
          ORDER BY u.name
          LIMIT $2",
    )
    .bind(dependent_project_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// GATEKEEPER + notify: resolve all open requests for `dependency_project_id`
/// (the only path to `resolved`) and send each requester a mailbox FYI that it
/// is unblocked. Returns the resolved request ids. Shared by the git-state-scan
/// cron and the `POST /api/tracker/project_event` gatekeeper endpoint.
pub async fn resolve_and_notify(
    pool: &PgPool,
    dependency_project_id: i32,
) -> Result<Vec<i64>, sqlx::Error> {
    let resolved = resolve_on_stable(pool, dependency_project_id).await?;
    if resolved.is_empty() {
        return Ok(Vec::new());
    }
    let dep_name: Option<String> = sqlx::query_scalar("SELECT name FROM projects WHERE id = $1")
        .bind(dependency_project_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    let mut ids = Vec::with_capacity(resolved.len());
    for r in &resolved {
        // §4.5 close-the-loop: if the requester linked a gated work-item, the
        // git scanner (Actor::System) flips it `blocked → ready` — the ONLY actor
        // that may, exactly like the verified-blocker auto-unblock cascade. The
        // editor (Agent) has no arm into this transition. Best-effort: a work-item
        // that is no longer `blocked` (the requester moved it on) is left as-is by
        // `check_transition` and the failure is swallowed.
        if let Some(wid) = r.blocked_work_item_id {
            match crate::db::queries::set_work_item_status(
                pool,
                wid,
                crate::tracker::status::WorkItemStatus::Ready,
                crate::tracker::transition::Actor::System,
                Some("system"),
                Some("auto-unblocked: gated dependency restored to its stable branch"),
                None,
                None,
            )
            .await
            {
                Ok(_) => tracing::info!(
                    work_item_id = wid,
                    coordination = r.id,
                    "coordination gatekeeper: System-unblocked gated work-item"
                ),
                Err(e) => tracing::debug!(
                    work_item_id = wid,
                    error = %e,
                    "coordination gatekeeper: work-item not in a System-unblockable state"
                ),
            }
        }
        let body = format!(
            "✅ Dependency '{}' is back on its stable branch & clean — you're unblocked \
             (coordination #{} resolved).",
            dep_name.as_deref().unwrap_or("?"),
            r.id
        );
        let msg = crate::a2a::mailbox_store::NewMessage {
            from_agent: "pgmcp",
            from_session: None,
            to_session: r.requester_session.as_deref(),
            to_project_id: r.dependent_project_id,
            to_agent: None,
            kind: "fyi",
            subject: Some("dependency stable — unblocked"),
            body: &body,
            reply_to: None,
            expires_at: None,
        };
        let _ = crate::a2a::mailbox_store::send(pool, &msg).await;
        ids.push(r.id);
    }
    Ok(ids)
}
