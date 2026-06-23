//! Aggregator queries for the `active_clients` MCP tool — the live MCP-client
//! view (PID · cwd · project · liveness). Independent collector over the
//! `mcp_clients` table (populated by the capture writer + liveness cron), per
//! the aggregator convention: it reads the table directly and leaves the
//! capture path untouched.

use serde::Serialize;
use sqlx::PgPool;

/// One connected (or recently-exited) MCP client and the project it is working
/// on. `idle_secs` is wall-clock seconds since `last_seen` (last tool call).
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ActiveClientRow {
    pub mcp_session_id: String,
    pub client_name: String,
    pub client_version: Option<String>,
    pub pid: Option<i32>,
    pub cwd: Option<String>,
    pub project: Option<String>,
    pub project_id: Option<i32>,
    pub alive: bool,
    pub first_seen: chrono::DateTime<chrono::Utc>,
    pub last_seen: chrono::DateTime<chrono::Utc>,
    pub last_liveness_at: Option<chrono::DateTime<chrono::Utc>>,
    pub idle_secs: i64,
}

/// Live MCP clients (or all, with `include_exited`), ordered by project then
/// most-recent activity, optionally filtered to a single project by name. The
/// `LEFT JOIN projects` resolves the project name; an unindexed cwd yields a
/// NULL project.
pub async fn active_clients(
    pool: &PgPool,
    project: Option<&str>,
    include_exited: bool,
) -> Result<Vec<ActiveClientRow>, sqlx::Error> {
    sqlx::query_as::<_, ActiveClientRow>(
        "SELECT c.mcp_session_id, c.client_name, c.client_version, c.pid, c.cwd,
                p.name AS project, c.project_id, c.alive,
                c.first_seen, c.last_seen, c.last_liveness_at,
                GREATEST(0, EXTRACT(EPOCH FROM (now() - c.last_seen))::int8) AS idle_secs
           FROM mcp_clients c
           LEFT JOIN projects p ON p.id = c.project_id
          WHERE ($2 OR c.alive)
            AND ($1::text IS NULL OR p.name = $1)
          ORDER BY c.project_id NULLS LAST, c.last_seen DESC",
    )
    .bind(project)
    .bind(include_exited)
    .fetch_all(pool)
    .await
}

/// One (client, project) cell of the m:n attribution matrix, aggregated from
/// `client_file_events` over a recent window. `edit_count` weights
/// writes/edits/closes; `read_count` covers reads/opens. The client identity is
/// best-effort, resolved in priority order (ADR-022):
/// 1. PID-native rows (`ebpf`/`proc_fd`) join `mcp_clients` on `mcp_session_id`;
/// 2. subprocess rows (`ebpf_cgroup`, where `mcp_session_id` is NULL) join
///    `mcp_clients` on `cgroup_id` to recover the owning agent (a `cargo`/`rustc`
///    child attributed back to the agent whose cgroup it inherited);
/// 3. the row's own `agent_id` (`claude-code` | `codex` | …) — now that two
///    agents share the hook ingest, this replaces the old `client_hook ⇒
///    claude-code` hardcode;
/// 4. else `claude-code` for legacy hook rows / `unknown`.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ClientProjectMatrixRow {
    pub project_id: Option<i32>,
    pub project: Option<String>,
    pub client_key: Option<String>,
    pub client_name: String,
    pub pid: Option<i32>,
    pub edit_count: i64,
    pub read_count: i64,
    pub file_count: i64,
    pub last_edit: Option<chrono::DateTime<chrono::Utc>>,
    pub last_activity: Option<chrono::DateTime<chrono::Utc>>,
}

/// The m:n client↔project matrix over the last `since_minutes`, optionally
/// filtered to one project by name. Ordered by project then edit-weight.
pub async fn client_project_matrix(
    pool: &PgPool,
    since_minutes: i32,
    project: Option<&str>,
) -> Result<Vec<ClientProjectMatrixRow>, sqlx::Error> {
    sqlx::query_as::<_, ClientProjectMatrixRow>(
        "SELECT
            cfe.project_id,
            p.name AS project,
            COALESCE(cfe.mcp_session_id, cfe.session_id::text, mc2.mcp_session_id,
                     'agent:' || cfe.agent_id, 'cgroup:' || cfe.cgroup_id::text) AS client_key,
            COALESCE(mc.client_name, mc2.client_name, cfe.agent_id,
                     CASE WHEN cfe.source = 'client_hook' THEN 'claude-code'
                          ELSE 'unknown' END) AS client_name,
            COALESCE(mc.pid, mc2.pid, MAX(cfe.pid)) AS pid,
            COUNT(*) FILTER (WHERE cfe.op IN ('write','edit','close')) AS edit_count,
            COUNT(*) FILTER (WHERE cfe.op IN ('read','open'))          AS read_count,
            COUNT(DISTINCT cfe.abs_path)                               AS file_count,
            MAX(cfe.ts) FILTER (WHERE cfe.op IN ('write','edit','close')) AS last_edit,
            MAX(cfe.ts)                                                AS last_activity
         FROM client_file_events cfe
         LEFT JOIN projects p      ON p.id = cfe.project_id
         LEFT JOIN mcp_clients mc  ON mc.mcp_session_id = cfe.mcp_session_id
         -- Subprocess (ebpf_cgroup) rows carry no mcp_session_id; recover the
         -- owning agent by the cgroup id its process subtree inherited.
         LEFT JOIN mcp_clients mc2 ON mc2.cgroup_id = cfe.cgroup_id
                                  AND cfe.cgroup_id IS NOT NULL
         WHERE cfe.ts > now() - make_interval(mins => $1)
           AND ($2::text IS NULL OR p.name = $2)
         GROUP BY cfe.project_id, p.name,
                  COALESCE(cfe.mcp_session_id, cfe.session_id::text, mc2.mcp_session_id,
                           'agent:' || cfe.agent_id, 'cgroup:' || cfe.cgroup_id::text),
                  mc.client_name, mc2.client_name, cfe.agent_id,
                  mc.pid, mc2.pid, cfe.source
         ORDER BY cfe.project_id NULLS LAST, edit_count DESC, last_activity DESC",
    )
    .bind(since_minutes)
    .bind(project)
    .fetch_all(pool)
    .await
}

/// Touch `agent_presence` for an agent observed via the session hook, recording
/// its `session_id` + `current_project_id`. This fills the historically-dormant
/// `agent_presence.session_id` / `current_project_id` columns (the
/// `touch_presence` heartbeat path never set them), so the active-agents-by-
/// project view and the coordination layer can join agent → project. Upserts
/// by `agent_id`; never clobbers a known project with NULL.
pub async fn touch_agent_presence_project(
    pool: &PgPool,
    agent_id: &str,
    session_id: uuid::Uuid,
    current_project_id: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO agent_presence
            (agent_id, last_active_at, status, session_id, current_project_id, updated_at)
         VALUES ($1, now(), 'active', $2, $3, now())
         ON CONFLICT (agent_id) DO UPDATE SET
            last_active_at     = now(),
            status             = 'active',
            session_id         = EXCLUDED.session_id,
            current_project_id = COALESCE(EXCLUDED.current_project_id,
                                          agent_presence.current_project_id),
            updated_at         = now()",
    )
    .bind(agent_id)
    .bind(session_id)
    .bind(current_project_id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// A recently-touched file within a project, for the per-project drill-down in
/// `client_project_matrix`. `edits` counts modifications; rows are returned
/// newest-edit-first and the tool keeps the top few per project.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RecentEditedFile {
    pub project_id: Option<i32>,
    pub abs_path: String,
    pub edits: i64,
    pub last_ts: chrono::DateTime<chrono::Utc>,
}

/// Recently-touched files per project over the last `since_minutes`, optionally
/// filtered to one project by name. The tool slices the top-N per project.
pub async fn recent_edited_files(
    pool: &PgPool,
    since_minutes: i32,
    project: Option<&str>,
) -> Result<Vec<RecentEditedFile>, sqlx::Error> {
    sqlx::query_as::<_, RecentEditedFile>(
        "SELECT cfe.project_id, cfe.abs_path,
                COUNT(*) FILTER (WHERE cfe.op IN ('write','edit','close')) AS edits,
                MAX(cfe.ts) AS last_ts
         FROM client_file_events cfe
         LEFT JOIN projects p ON p.id = cfe.project_id
         WHERE cfe.ts > now() - make_interval(mins => $1)
           AND ($2::text IS NULL OR p.name = $2)
         GROUP BY cfe.project_id, cfe.abs_path
         ORDER BY cfe.project_id NULLS LAST, edits DESC, last_ts DESC",
    )
    .bind(since_minutes)
    .bind(project)
    .fetch_all(pool)
    .await
}

/// One active agent instance and the project it is on — the A2A
/// active-agents-by-project discovery row. A superset of `ActiveClientRow` that
/// also carries the advisory `a2a_agents` role/specialty (via the
/// `agent_identity` view, matched on the lowercased `client_name`).
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ActiveAgentRow {
    pub client_name: String,
    pub mcp_session_id: String,
    pub pid: Option<i32>,
    pub cwd: Option<String>,
    pub project_id: Option<i32>,
    pub project: Option<String>,
    pub alive: bool,
    pub last_seen: chrono::DateTime<chrono::Utc>,
    pub recommended_role: Option<String>,
    pub specialty: Option<Vec<String>>,
}

/// Live agent instances (from `mcp_clients`) joined to their project and the
/// advisory A2A registry identity, optionally filtered to one project by name.
/// This is the A2A "social" view (vs. `active_clients`, the ops/PID view);
/// `mcp_session_id` is the precise instance handle for message addressing.
pub async fn active_agents_by_project(
    pool: &PgPool,
    project: Option<&str>,
) -> Result<Vec<ActiveAgentRow>, sqlx::Error> {
    sqlx::query_as::<_, ActiveAgentRow>(
        "SELECT c.client_name, c.mcp_session_id, c.pid, c.cwd, c.project_id,
                p.name AS project, c.alive, c.last_seen,
                ai.recommended_role, ai.specialty
           FROM mcp_clients c
           LEFT JOIN projects p       ON p.id = c.project_id
           LEFT JOIN agent_identity ai ON ai.agent_id = c.client_name
          WHERE c.alive AND ($1::text IS NULL OR p.name = $1)
          ORDER BY c.project_id NULLS LAST, c.last_seen DESC",
    )
    .bind(project)
    .fetch_all(pool)
    .await
}
