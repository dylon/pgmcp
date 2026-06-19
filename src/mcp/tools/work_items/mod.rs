//! MCP tool bodies for the work-item / plan tracker
//! (`crate::db::migrations::v4_work_items`, domain in `crate::tracker`,
//! queries in `crate::db::queries::work_items`).
//!
//! These are the agent-facing CRUD + lifecycle tools. The trust boundary is
//! enforced in `lifecycle::tool_work_item_set_status`: the actor is ALWAYS
//! [`crate::tracker::transition::Actor::Agent`], never read from params, so an
//! agent can never self-`verify`/`defer`/`reject` (those belong to the
//! user/gatekeeper paths). Each tool exposes `pub async fn tool_<name>(ctx:
//! &SystemContext, params: <Name>Params) -> Result<CallToolResult, McpError>`;
//! the `#[tool]`-annotated method on `McpServer` is a one-line forward into the
//! corresponding `tool_<name>` here, mirroring `tool_experiments`.

mod crud;
pub use crud::*;
mod lifecycle;
pub use lifecycle::*;
mod tags;
pub use tags::*;
mod progress;
pub use progress::*;
mod analysis;
pub use analysis::*;
mod definitions;
pub use definitions::*;
mod verify;
pub use verify::*;
mod bugs;
pub use bugs::*;
mod git_link;
pub use git_link::*;
mod ingestion;
pub use ingestion::*;
mod collab;
pub use collab::*;
mod visibility;
pub use visibility::*;
mod relations;
pub use relations::*;
mod reporting;
pub use reporting::*;
mod experiment_link;
pub use experiment_link::*;
mod views;
pub use views::*;
mod bulk;
pub use bulk::*;

/// Derive a kebab-case slug from a title (copied from `tool_experiments` so the
/// two subsystems share an identical slugging rule without a cross-module dep).
pub(crate) fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_dash = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() {
        "work-item".to_string()
    } else {
        s
    }
}

/// Generate a stable, human-legible `public_id`: the title slug plus a short
/// random suffix to guarantee uniqueness (`my-task-3f9a1c`).
pub(crate) fn gen_public_id(title: &str) -> String {
    format!(
        "{}-{}",
        slugify(title),
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    )
}

/// Non-empty trimmed view of an optional string param: `None`/blank → `None`,
/// else the trimmed `&str`. Shared by the create/update/triage/resolve bodies so
/// "was a value actually supplied?" is decided uniformly.
pub(crate) fn nonblank(opt: &Option<String>) -> Option<&str> {
    opt.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// Embed an item's title (+ body) on write so semantic backlog search works
/// immediately, without waiting for a backfill. Failures are non-fatal —
/// `None` leaves the `work_items.embedding` column NULL (mirrors the
/// experiment subsystem's `embed_opt`).
pub(crate) async fn embed_title_body(
    ctx: &crate::context::SystemContext,
    title: &str,
    body: Option<&str>,
    extra: Option<&str>,
) -> Option<pgvector::Vector> {
    let mut text = title.to_string();
    if let Some(b) = body.filter(|b| !b.trim().is_empty()) {
        text.push('\n');
        text.push_str(b);
    }
    // Bug items fold their reproduction / expected-vs-actual / root-cause text
    // here so "find similar bugs" semantic search sees it (the cron's
    // work_items backfill composes the same fields from the sidecar).
    if let Some(e) = extra.filter(|e| !e.trim().is_empty()) {
        text.push('\n');
        text.push_str(e);
    }
    if text.trim().is_empty() {
        return None;
    }
    match ctx.embed().embed_query(&text).await {
        Ok(v) => Some(pgvector::Vector::from(v)),
        Err(e) => {
            tracing::error!(error = %e, "work_item embed-on-write failed; leaving embedding NULL");
            None
        }
    }
}
