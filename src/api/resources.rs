//! `GET /api/resources` — system + process + GPU resource telemetry for the
//! webui Resources pane. Reads the O(1) snapshot the resource sampler
//! (`crate::stats::resources`) maintains, plus live worker-pool numbers, so the
//! request itself never touches `/proc` or NVML.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::api::ApiState;

pub async fn resources(State(state): State<ApiState>) -> Json<Value> {
    let snapshot = state.stats.resources();
    let counters = state.stats.snapshot();
    let counter = |key: &str| counters.get(key).cloned().unwrap_or(Value::Null);
    let (db_pool_size, db_pool_idle) = state
        .db
        .pool()
        .map(|p| (p.size(), p.num_idle() as u32))
        .unwrap_or((0, 0));

    Json(json!({
        "system": snapshot.as_deref(),
        "worker_pools": {
            "active_work_pool_threads": counter("active_work_pool_threads"),
            "work_pool_queue_depth": counter("work_pool_queue_depth"),
            "work_pool_tasks_completed": counter("work_pool_tasks_completed"),
            "pool_pressure_ms_total": counter("pool_pressure_ms_total"),
            "embed_workers_alive": counter("embed_workers_alive"),
            "db_pool_size": db_pool_size,
            "db_pool_idle": db_pool_idle,
            "db_pool_active": db_pool_size.saturating_sub(db_pool_idle),
        },
        "uptime_secs": counter("uptime_secs"),
    }))
}
