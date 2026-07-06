//! Token + origin authentication for the webui-consumed REST surface.
//!
//! ADR-034 originally left `/api/{query,mandates,work_items,status,stats}`
//! unauthenticated — only `/webui/ws` was token-gated. The admin-console
//! expansion (see `docs/decisions/034-webui-admin-console.md`) surfaces richer
//! operational data and audited operator writes, so the whole webui-consumed
//! surface is gated by the same `[webui] token` + origin allow-list as the
//! websocket. Producer routes (hooks, CI, A2A, control) keep their own
//! credentials and are deliberately NOT wrapped by this layer.
//!
//! When `[webui] token` is unset/empty the token check is a pass-through
//! (the documented loopback-trust posture), so existing local installs behave
//! exactly as before this layer was introduced.

use std::collections::HashMap;

use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::ApiState;

/// Reject requests that fail the webui origin allow-list (403) or the token
/// check (401). Reuses the exact primitives the websocket handshake uses
/// (`pgmcp_webui::security`) so REST and WS share a single auth definition.
pub async fn require_webui_auth(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Response {
    // Snapshot owned auth material so no ArcSwap guard is held across the await
    // (the middleware future must stay Send for axum's service stack).
    let (expected, allowed) = {
        let config = state.config.load();
        (
            config.webui.token.clone().filter(|s| !s.is_empty()),
            crate::cli::daemon::default_webui_origins(&config),
        )
    };

    if !pgmcp_webui::security::origin_allowed(request.headers(), &allowed) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }

    // The websocket authenticates with `?token=`; the browser REST client uses
    // `Authorization: Bearer`. Accept either so curl/debugging and the SPA both
    // work.
    let query_token = Query::<HashMap<String, String>>::try_from_uri(request.uri())
        .ok()
        .and_then(|q| q.0.get("token").cloned());

    if !pgmcp_webui::security::token_authorized(
        request.headers(),
        query_token.as_deref(),
        expected.as_deref(),
    ) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid webui token").into_response();
    }

    next.run(request).await
}
