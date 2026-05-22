//! `GraphReader` / `GraphWriter` — testability seam for the graph subsystem.
//!
//! The cron jobs that build and consume the code graph (`graph_analysis`,
//! `call_graph`, the graph-shaped MCP tools) historically held `&PgPool`
//! directly. The trait pair below lets tests pass a deterministic in-memory
//! adjacency store while production code keeps its Postgres-backed path —
//! breaking the Zone-of-Pain coupling that `architecture_violations` flags
//! on `src/graph`.
//!
//! Wiring scope:
//!   - Today: trait + a thin `PgGraphStore` adapter that delegates to the
//!     existing `crate::graph::builder::*` and `crate::db::queries::*`
//!     free functions. Consumers continue to call those directly until they
//!     migrate; new consumers should take `&dyn GraphReader` instead.
//!   - Future migration: change cron functions to accept
//!     `reader: &dyn GraphReader` and tests pass an in-memory mock.

use async_trait::async_trait;
use sqlx::PgPool;

use crate::error::Result;
use crate::graph::CodeGraph;
use crate::graph::builder::{FileMetaRow, GraphEdgeRow};

/// Read side of the code graph. Production impl talks to Postgres; tests
/// supply an in-memory mock that yields a fixed `Vec<GraphEdgeRow>` and
/// `Vec<FileMetaRow>`.
#[allow(dead_code)]
#[async_trait]
pub trait GraphReader: Send + Sync {
    /// Load every `code_graph_edges` row for the given project.
    async fn load_import_edges(&self, project_id: i32) -> Result<Vec<GraphEdgeRow>>;

    /// Load every `indexed_files` (id, relative_path, language) tuple for
    /// the given project.
    async fn load_file_metas(&self, project_id: i32) -> Result<Vec<FileMetaRow>>;

    /// Build a `CodeGraph` from the edge + file-meta pair. Default impl
    /// chains the two reads and forwards to `crate::graph::builder::build_graph`.
    async fn load_code_graph(&self, project_id: i32) -> Result<CodeGraph> {
        let edges = self.load_import_edges(project_id).await?;
        let metas = self.load_file_metas(project_id).await?;
        Ok(crate::graph::builder::build_graph(&edges, &metas))
    }
}

/// Write side of the code graph. Production impl issues UNNEST batched
/// INSERTs; tests can record-and-ignore in a `Mutex<Vec<_>>`.
#[allow(dead_code)]
#[async_trait]
pub trait GraphWriter: Send + Sync {
    /// Upsert a batch of `code_graph_edges` rows (idempotent on
    /// `(project_id, source_file_id, target_file_id, edge_type)`).
    /// Returns the number of rows inserted.
    async fn upsert_code_graph_edges(&self, project_id: i32, edges: &[GraphEdgeRow])
    -> Result<u64>;
}

/// Production reader+writer backed by a real `PgPool`. Single-purpose
/// adapter; doesn't own the pool — accepts a borrowed handle so the
/// daemon's existing pool sharing stays intact.
#[allow(dead_code)]
pub struct PgGraphStore<'a> {
    pub pool: &'a PgPool,
}

impl<'a> PgGraphStore<'a> {
    #[allow(dead_code)]
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl<'a> GraphReader for PgGraphStore<'a> {
    async fn load_import_edges(&self, project_id: i32) -> Result<Vec<GraphEdgeRow>> {
        #[derive(sqlx::FromRow)]
        struct Row {
            source_file_id: i64,
            source_relative_path: String,
            source_language: String,
            target_file_id: Option<i64>,
            target_relative_path: Option<String>,
            target_language: Option<String>,
            edge_type: String,
            weight: f64,
        }
        let rows: Vec<Row> = sqlx::query_as::<_, Row>(
            "SELECT e.source_file_id, sf.relative_path AS source_relative_path, \
                    sf.language AS source_language, \
                    e.target_file_id, tf.relative_path AS target_relative_path, \
                    tf.language AS target_language, e.edge_type, e.weight \
             FROM code_graph_edges e \
             JOIN indexed_files sf ON e.source_file_id = sf.id \
             LEFT JOIN indexed_files tf ON e.target_file_id = tf.id \
             WHERE e.project_id = $1 AND e.edge_type = 'import'",
        )
        .bind(project_id)
        .fetch_all(self.pool)
        .await
        .map_err(crate::error::PgmcpError::Database)?;
        Ok(rows
            .into_iter()
            .map(|r| GraphEdgeRow {
                source_file_id: r.source_file_id,
                source_relative_path: r.source_relative_path,
                source_language: r.source_language,
                target_file_id: r.target_file_id,
                target_relative_path: r.target_relative_path,
                target_language: r.target_language,
                edge_type: r.edge_type,
                weight: r.weight,
            })
            .collect())
    }

    async fn load_file_metas(&self, project_id: i32) -> Result<Vec<FileMetaRow>> {
        #[derive(sqlx::FromRow)]
        struct Row {
            file_id: i64,
            relative_path: String,
            language: String,
        }
        let rows: Vec<Row> = sqlx::query_as::<_, Row>(
            "SELECT id AS file_id, relative_path, language FROM indexed_files WHERE project_id = $1",
        )
        .bind(project_id)
        .fetch_all(self.pool)
        .await
        .map_err(crate::error::PgmcpError::Database)?;
        Ok(rows
            .into_iter()
            .map(|r| FileMetaRow {
                file_id: r.file_id,
                relative_path: r.relative_path,
                language: r.language,
            })
            .collect())
    }
}

#[async_trait]
impl<'a> GraphWriter for PgGraphStore<'a> {
    async fn upsert_code_graph_edges(
        &self,
        _project_id: i32,
        _edges: &[GraphEdgeRow],
    ) -> Result<u64> {
        // Write path uses an UNNEST batched INSERT against the live schema;
        // not implemented here because the cron job (`graph_analysis`) and
        // `call_graph` already have purpose-built bulk writers
        // (`db::queries::upsert_code_graph_edges_batch`, etc.). Wire those
        // through this trait when the cron migrates to take
        // `&dyn GraphWriter`.
        Err(crate::error::PgmcpError::Other(
            "PgGraphStore::upsert_code_graph_edges: production path uses cron-specific bulk writers; not yet wired through the trait".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn traits_are_object_safe() {
        fn _assert_object_safe(_: Box<dyn GraphReader>, _: Box<dyn GraphWriter>) {}
        fn _assert_send_sync<T: Send + Sync>() {}
        _assert_send_sync::<Arc<dyn GraphReader>>();
        _assert_send_sync::<Arc<dyn GraphWriter>>();
    }
}
