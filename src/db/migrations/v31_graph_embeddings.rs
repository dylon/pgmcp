//! Migration step 31: `graph_embeddings_v1`.
//!
//! Adds `embedding vector(1024)` + `embedding_signature TEXT DEFAULT 'bge-m3-v1'`
//! to four tables so their rows become vector-seedable arms in
//! `memory_unified_nodes` (the temporal graph-RAG index):
//!
//! - `agent_messages` — the v27 social mailbox; embeds `subject || body`.
//! - `a2a_messages` — A2A task transcripts; embeds text extracted from `parts`.
//! - `memory_entities` — KB entity hubs; embeds `name + entity_type`. The existing
//!   `memory_entity` node arm flips from a NULL embedding to this column.
//! - `coordination_requests` — the v29 worktree-negotiation connector; embeds
//!   `reason || error_excerpt`.
//!
//! `a2a_tasks` deliberately gets NO embedding — it is a non-embedded HUB node
//! (like `commit`), reached via `in_task` / `evidenced_by` edges. `session_prompts`
//! (`embedding_v2`) and `data_tables` (`embedding`) already carry their columns.
//!
//! Columns are 1024d-direct (the `memory_observations` / `durable_mandates`
//! model). The embedding-migration cron (`src/cron/embedding_migration.rs`)
//! backfills NULLs — the established NON-file pattern; no synchronous embed on the
//! write paths (which are now fail-closed after the v29-era hardening).
//!
//! Additive + `IF NOT EXISTS`, so idempotent and version-gated.

use sqlx::PgPool;

pub(super) const GRAPH_EMBEDDINGS_V1: i32 = 31;
pub(super) const GRAPH_EMBEDDINGS_V1_NAME: &str = "graph_embeddings_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    for stmt in [
        "ALTER TABLE agent_messages        ADD COLUMN IF NOT EXISTS embedding vector(1024)",
        "ALTER TABLE agent_messages        ADD COLUMN IF NOT EXISTS embedding_signature TEXT DEFAULT 'bge-m3-v1'",
        "ALTER TABLE a2a_messages          ADD COLUMN IF NOT EXISTS embedding vector(1024)",
        "ALTER TABLE a2a_messages          ADD COLUMN IF NOT EXISTS embedding_signature TEXT DEFAULT 'bge-m3-v1'",
        "ALTER TABLE memory_entities       ADD COLUMN IF NOT EXISTS embedding vector(1024)",
        "ALTER TABLE memory_entities       ADD COLUMN IF NOT EXISTS embedding_signature TEXT DEFAULT 'bge-m3-v1'",
        "ALTER TABLE coordination_requests ADD COLUMN IF NOT EXISTS embedding vector(1024)",
        "ALTER TABLE coordination_requests ADD COLUMN IF NOT EXISTS embedding_signature TEXT DEFAULT 'bge-m3-v1'",
    ] {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(GRAPH_EMBEDDINGS_V1, 31);
        assert_eq!(GRAPH_EMBEDDINGS_V1_NAME, "graph_embeddings_v1");
    }
}
