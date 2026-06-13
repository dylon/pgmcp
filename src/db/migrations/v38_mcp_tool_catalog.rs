//! Migration step 38: `mcp_tool_catalog`.
//!
//! A semantically-searchable catalog of the server's OWN MCP tools — distinct
//! from `tool_cards`, which describes external developer tools installed on the
//! machine. Seeded from `McpServer::static_tool_catalog()` and embedded by the
//! embedding-migration cron (1024d-direct, the `tool_cards` / `durable_mandates`
//! model: the `embedding vector(1024)` column is filled by the cron, NOT on the
//! seed path). The `tool_catalog` meta-tool ranks against it so a `Learned`
//! client can discover and `enable_tools` any tool outside its default surface.
//!
//! The HNSW index on `embedding` is built unconditionally by
//! `ensure_v31_embedding_hnsw_index(pool, cfg, "mcp_tool_catalog")` in
//! `run_migrations` (the same generic helper `tool_cards` uses), so it tracks the
//! configured `m` / `ef_construction` on fresh and upgraded installs alike.
//!
//! Additive + `IF NOT EXISTS`, idempotent, version-gated.

use sqlx::PgPool;

pub(super) const MCP_TOOL_CATALOG: i32 = 38;
pub(super) const MCP_TOOL_CATALOG_NAME: &str = "mcp_tool_catalog";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mcp_tool_catalog (
            id                  BIGSERIAL PRIMARY KEY,
            name                TEXT UNIQUE NOT NULL,
            domain              TEXT NOT NULL DEFAULT '',
            description         TEXT NOT NULL DEFAULT '',
            input_schema        TEXT NOT NULL DEFAULT '',
            content_hash        BIGINT,
            embedding           vector(1024),
            embedding_signature TEXT DEFAULT 'bge-m3-v1',
            created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_mcp_tool_catalog_domain ON mcp_tool_catalog(domain)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(MCP_TOOL_CATALOG, 38);
        assert_eq!(MCP_TOOL_CATALOG_NAME, "mcp_tool_catalog");
    }
}
