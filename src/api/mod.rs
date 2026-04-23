pub mod handlers;

use std::sync::Arc;

use arc_swap::ArcSwap;
use sqlx::PgPool;

use crate::config::Config;
use crate::embed::pool::QueryEmbedder;

/// Shared state for REST API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub db_pool: PgPool,
    pub query_embedder: QueryEmbedder,
    pub config: Arc<ArcSwap<Config>>,
}
