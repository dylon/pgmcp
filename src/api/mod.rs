pub mod handlers;

use std::sync::Arc;

use arc_swap::ArcSwap;
use sqlx::PgPool;

use crate::config::Config;

/// Shared state for REST API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub db_pool: PgPool,
    pub embed_model: Arc<tokio::sync::Mutex<fastembed::TextEmbedding>>,
    pub config: Arc<ArcSwap<Config>>,
}
