pub mod handlers;

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::db::DbClient;
use crate::embed::pool::QueryEmbedder;

/// Shared state for REST API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub db: Arc<dyn DbClient>,
    pub query_embedder: QueryEmbedder,
    pub config: Arc<ArcSwap<Config>>,
}
