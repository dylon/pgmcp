//! CUDA-free web UI crate for pgmcp.
//!
//! This crate owns the browser assets and the resumable websocket reader. It
//! deliberately does not depend on the root `pgmcp` crate; daemon-only routes
//! remain responsible for embedding, fuzzy indexes, MCP tool twins, and
//! token-gated mutations.

mod assets;
mod db;
pub mod security;
mod state;
mod ws;

pub use state::{WebuiOptions, WebuiState};

use axum::Router;
use axum::routing::get;

pub fn router(state: WebuiState) -> Router {
    Router::new()
        .route("/webui", get(assets::index))
        .route("/webui/", get(assets::index))
        .route("/webui/ws", get(ws::handler))
        .route("/webui/{*path}", get(assets::asset))
        .with_state(state)
}
