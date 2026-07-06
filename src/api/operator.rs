//! Shared plumbing for the token-gated operator WRITE surface (ADR-034
//! admin-console amendment; see `docs/decisions/034-webui-admin-console.md`):
//! the `[webui] writes_enabled` kill-switch guard, the real-pool accessor, the
//! request-IP capture for the audit trail, the operator actor label, and the
//! canonical DB-error → 500 mapping. Used by both [`super::mandates_write`] and
//! [`super::work_items_write`] so the two write modules share one definition of
//! "am I allowed to mutate, and against what pool".
//!
//! These routes live on the `webui_api` sub-router, so `require_webui_auth`
//! (`src/api/auth.rs`) has ALREADY established operator authority (the `[webui]
//! token` + origin allow-list) by the time any of these run — the operator IS
//! the user. `writes_enabled_or_403` is the second, independent gate: a master
//! switch to run the console read-only without dropping the token.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::StatusCode;
use axum::http::request::Parts;

use super::ApiState;

/// Actor label recorded in `webui_audit_log.actor` and
/// `work_item_status_history.actor_id` for every operator-console mutation.
pub(crate) const OPERATOR: &str = "operator";

/// The `[webui] writes_enabled` master kill-switch. Returns 403 when writes are
/// disabled, so the read surface + realtime feed stay live while every mutation
/// is refused — even for an authenticated operator.
pub(crate) fn writes_enabled_or_403(state: &ApiState) -> Result<(), (StatusCode, String)> {
    if state.config.load().webui.writes_enabled {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "writes disabled: set [webui] writes_enabled = true to enable operator mutations"
                .to_string(),
        ))
    }
}

/// The daemon's real `PgPool`, or a 500 for the (test-only) mock `DbClient`
/// that has no pool. Operator writes are transactional and cannot run on a mock.
pub(crate) fn operator_pool(state: &ApiState) -> Result<&sqlx::PgPool, (StatusCode, String)> {
    state.db.pool().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "operator writes require a real PgPool DbClient".to_string(),
    ))
}

/// Optional source-IP extractor for the audit trail. `Option<ConnectInfo<..>>`
/// is NOT a valid axum 0.8 extractor (ConnectInfo has no
/// `OptionalFromRequestParts`), so we read the `ConnectInfo` that the serve
/// layer (`into_make_service_with_connect_info::<SocketAddr>()`) stored in the
/// request extensions directly, yielding `None` when it is absent (e.g. a unit
/// test invoking a handler). Extraction is infallible, so it never alters a
/// handler's response.
pub(crate) struct OptionalPeer(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for OptionalPeer {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(OptionalPeer(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0),
        ))
    }
}

/// The connecting operator's source IP (loopback in the common local-first
/// posture) recorded in the audit trail.
pub(crate) fn request_ip(peer: OptionalPeer) -> Option<String> {
    peer.0.map(|addr| addr.ip().to_string())
}

/// Canonical `sqlx::Error` → HTTP 500. The DB is the trust anchor for every
/// operator mutation, so a DB failure is an internal error, not a client one.
pub(crate) fn map_db_500(e: sqlx::Error) -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("database error: {e}"),
    )
}
