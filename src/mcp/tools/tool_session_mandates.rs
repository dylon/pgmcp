//! MCP tools for session-scoped mandate introspection and promotion.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::sessions;

const DEFAULT_SESSION_MANDATES_LIMIT: i32 = 20;
const MAX_SESSION_MANDATES_LIMIT: i32 = 100;
const MAX_TARGET_FILE_BYTES: usize = 4096;

fn raw_pool(ctx: &SystemContext) -> Result<&sqlx::PgPool, McpError> {
    ctx.db()
        .pool()
        .ok_or_else(|| McpError::internal_error("raw pool unavailable", None))
}

fn json_result(value: serde_json::Value) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(&value)
        .map_err(|e| McpError::internal_error(format!("serialize failed: {}", e), None))?;
    Ok(CallToolResult::success(vec![rmcp::model::Content::text(
        text,
    )]))
}

fn parse_uuid(s: &str) -> Result<Uuid, McpError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(McpError::invalid_params(
            "session_id must not be blank",
            None,
        ));
    }
    Uuid::parse_str(trimmed)
        .map_err(|e| McpError::invalid_params(format!("invalid session_id UUID: {}", e), None))
}

fn normalize_status_filter(raw: Option<&str>) -> Result<String, McpError> {
    let status = raw.unwrap_or("active").trim().to_ascii_lowercase();
    let status = if status.is_empty() {
        "active".to_string()
    } else {
        status
    };
    if matches!(
        status.as_str(),
        "active" | "all" | "promoted" | "retired" | "superseded"
    ) {
        Ok(status)
    } else {
        Err(McpError::invalid_params(
            "status must be 'active', 'all', 'promoted', 'retired', or 'superseded'",
            None,
        ))
    }
}

fn normalize_cwd(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(ToOwned::to_owned)
}

fn normalize_promotion_scope(raw: &str) -> Result<String, McpError> {
    let scope = raw.trim().to_ascii_lowercase();
    if matches!(scope.as_str(), "project" | "workspace") {
        Ok(scope)
    } else {
        Err(McpError::invalid_params(
            "scope must be 'project' or 'workspace'",
            None,
        ))
    }
}

fn normalize_promotion_project_id(
    scope: &str,
    project_id: Option<i32>,
) -> Result<Option<i32>, McpError> {
    match (scope, project_id) {
        ("project", Some(id)) if id > 0 => Ok(Some(id)),
        ("project", Some(_)) => Err(McpError::invalid_params(
            "project_id must be positive when scope='project'",
            None,
        )),
        ("project", None) => Err(McpError::invalid_params(
            "project_id is required when scope='project'",
            None,
        )),
        ("workspace", Some(_)) => Err(McpError::invalid_params(
            "project_id is only valid when scope='project'",
            None,
        )),
        ("workspace", None) => Ok(None),
        _ => unreachable!("scope is normalized before project validation"),
    }
}

fn normalize_target_file(
    write_to_file: bool,
    target_file: Option<&str>,
) -> Result<Option<String>, McpError> {
    if !write_to_file {
        return Ok(None);
    }

    let Some(raw) = target_file else {
        return Err(McpError::invalid_params(
            "write_to_file=true requires target_file (no implicit CLAUDE.md/AGENTS.md path is chosen for safety)",
            None,
        ));
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(McpError::invalid_params(
            "target_file must not be blank when write_to_file=true",
            None,
        ));
    }
    if trimmed.len() > MAX_TARGET_FILE_BYTES {
        return Err(McpError::invalid_params(
            format!("target_file must be at most {MAX_TARGET_FILE_BYTES} bytes"),
            None,
        ));
    }
    Ok(Some(trimmed.to_string()))
}

pub async fn tool_session_mandates(
    ctx: &SystemContext,
    params: SessionMandatesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "session_mandates", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;

    let session_id = params.session_id.as_deref().map(parse_uuid).transpose()?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_SESSION_MANDATES_LIMIT)
        .clamp(1, MAX_SESSION_MANDATES_LIMIT);
    let status_filter = normalize_status_filter(params.status.as_deref())?;
    let cwd = normalize_cwd(params.cwd.as_deref());

    if session_id.is_none() && cwd.is_none() {
        return Err(McpError::invalid_params(
            "either session_id or cwd is required",
            None,
        ));
    }

    let mandates = if status_filter == "active" {
        sessions::list_active_mandates(pool, session_id, cwd.as_deref(), limit)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("list_active_mandates failed: {}", e), None)
            })?
    } else {
        let status_param = if status_filter == "all" {
            None::<String>
        } else {
            Some(status_filter.clone())
        };
        match (session_id, cwd.as_deref()) {
            (Some(sid), _) => {
                sqlx::query_as::<_, sessions::SessionMandate>(
                    "SELECT * FROM session_mandates
                 WHERE session_id = $1
                   AND ($2::text IS NULL OR status = $2)
                 ORDER BY cue_tier DESC, last_reinforced_at DESC, salience DESC
                 LIMIT $3",
                )
                .bind(sid)
                .bind(status_param)
                .bind(limit)
                .fetch_all(pool)
                .await
            }
            (None, Some(cwd)) => {
                sqlx::query_as::<_, sessions::SessionMandate>(
                    "SELECT m.* FROM session_mandates m
                 JOIN sessions s ON s.id = m.session_id
                 WHERE s.cwd = $1
                   AND ($2::text IS NULL OR m.status = $2)
                 ORDER BY m.cue_tier DESC, m.last_reinforced_at DESC, m.salience DESC
                 LIMIT $3",
                )
                .bind(cwd)
                .bind(status_param)
                .bind(limit)
                .fetch_all(pool)
                .await
            }
            (None, None) => Ok(Vec::new()),
        }
        .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?
    };

    let rendered = sessions::render_session_mandates_md(&mandates, 4096);

    json_result(json!({
        "session_id": session_id,
        "cwd": cwd,
        "status": status_filter,
        "limit": limit,
        "count": mandates.len(),
        "mandates": mandates,
        "rendered_markdown": rendered,
    }))
}

pub async fn tool_promote_session_mandate(
    ctx: &SystemContext,
    params: PromoteSessionMandateParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;

    if params.mandate_id <= 0 {
        return Err(McpError::invalid_params(
            "mandate_id must be positive",
            None,
        ));
    }

    let scope = normalize_promotion_scope(&params.scope)?;
    let project_id = normalize_promotion_project_id(&scope, params.project_id)?;
    let write_to_file = params.write_to_file.unwrap_or(false);
    let target_file = normalize_target_file(write_to_file, params.target_file.as_deref())?;
    let mut file_lock = if let Some(path) = target_file.as_deref() {
        Some(
            AdvisoryFileLock::try_acquire(pool, path)
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("acquire target_file lock({path}): {e}"), None)
                })?,
        )
    } else {
        None
    };

    let durable_id = match sessions::promote_mandate(
        pool,
        params.mandate_id,
        &scope,
        project_id,
        target_file.as_deref(),
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            if let Some(lock) = file_lock.take() {
                let _ = lock.release().await;
            }
            return Err(match e {
                sqlx::Error::RowNotFound => McpError::invalid_params(
                    "session mandate not found or not eligible for promotion",
                    None,
                ),
                sqlx::Error::Protocol(msg) => McpError::invalid_params(msg, None),
                other => {
                    McpError::internal_error(format!("promote_mandate failed: {}", other), None)
                }
            });
        }
    };

    let mut written_path: Option<String> = None;
    if let Some(path) = target_file.as_deref() {
        let append_result = append_mandate_to_file(path, params.mandate_id, &scope, pool).await;
        let release_result = match file_lock.take() {
            Some(lock) => lock.release().await,
            None => Ok(()),
        };
        match (append_result, release_result) {
            (Ok(()), Ok(())) => written_path = Some(path.to_string()),
            (Err(e), _) => {
                return Err(McpError::internal_error(
                    format!("append_mandate_to_file({}): {}", path, e),
                    None,
                ));
            }
            (Ok(()), Err(e)) => {
                return Err(McpError::internal_error(
                    format!("release target_file lock({}): {}", path, e),
                    None,
                ));
            }
        }
    }

    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution.
    let effect_breakdown = match ctx.db().pool() {
        Some(pool) => {
            crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, project_id).await
        }
        None => serde_json::json!({}),
    };

    json_result(json!({
        "effect_breakdown": effect_breakdown,
        "ok": true,
        "durable_mandate_id": durable_id,
        "source_session_mandate_id": params.mandate_id,
        "scope": scope,
        "project_id": project_id,
        "wrote_file": written_path,
    }))
}

/// Append the imperative to a marker section in the target file. Idempotent:
/// if the section already contains the imperative, do nothing.
async fn append_mandate_to_file(
    path: &str,
    mandate_id: i64,
    scope: &str,
    pool: &sqlx::PgPool,
) -> Result<(), String> {
    const MARKER: &str = "## Promoted session mandates (pgmcp)";

    // Fetch the mandate text we're appending.
    let mandate = sessions::get_mandate(pool, mandate_id)
        .await
        .map_err(|e| format!("get_mandate: {}", e))?
        .ok_or_else(|| format!("mandate {} not found", mandate_id))?;
    let bullet = format!(
        "- **{}** _(scope: {})_: {}",
        mandate.polarity, scope, mandate.imperative
    );

    append_bullet_to_marker(path, MARKER, &bullet)
}

fn file_lock_key(path: &str) -> (i32, i32) {
    let digest = Sha256::digest(path.as_bytes());
    let hi = i32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    let lo = i32::from_be_bytes([digest[4], digest[5], digest[6], digest[7]]);
    (hi, lo)
}

struct AdvisoryFileLock {
    conn: sqlx::pool::PoolConnection<sqlx::Postgres>,
    key_a: i32,
    key_b: i32,
}

impl AdvisoryFileLock {
    async fn try_acquire(pool: &sqlx::PgPool, path: &str) -> Result<Self, String> {
        let (key_a, key_b) = file_lock_key(path);
        let mut conn = pool
            .acquire()
            .await
            .map_err(|e| format!("acquire file lock connection: {e}"))?;

        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1, $2)")
            .bind(key_a)
            .bind(key_b)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| format!("acquire file lock: {e}"))?;
        if !acquired {
            return Err("target file is busy; retry promotion later".to_string());
        }

        Ok(Self { conn, key_a, key_b })
    }

    async fn release(mut self) -> Result<(), String> {
        let released: Result<bool, sqlx::Error> =
            sqlx::query_scalar("SELECT pg_advisory_unlock($1, $2)")
                .bind(self.key_a)
                .bind(self.key_b)
                .fetch_one(&mut *self.conn)
                .await;

        match released {
            Ok(true) => Ok(()),
            Ok(false) => Err("release file lock: lock was not held".to_string()),
            Err(e) => Err(format!("release file lock: {e}")),
        }
    }
}

/// Append a bullet under a marker section in a file (pure I/O, idempotent).
/// Creates the marker section if absent. Shared by session-mandate
/// promotion and the A4 cross-agent best-practice promotion.
pub(crate) fn append_bullet_to_marker(
    path: &str,
    marker: &str,
    bullet: &str,
) -> Result<(), String> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.contains(bullet) {
        return Ok(());
    }
    let mut out = existing.clone();
    if !out.contains(marker) {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(marker);
        out.push_str("\n\n");
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(bullet);
    out.push('\n');
    std::fs::write(path, out).map_err(|e| format!("write {}: {}", path, e))
}
