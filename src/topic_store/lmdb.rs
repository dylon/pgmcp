//! heed-backed (LMDB) persistent store for FCM centroids + chunk assignments.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use heed::types::{Bytes, SerdeBincode, U32, U64};
use heed::{Database, Env, EnvOpenOptions};
use serde::{Deserialize, Serialize};

use crate::error::{PgmcpError, Result};

/// On-disk record for a single FCM topic centroid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCentroid {
    /// Scope string: "global", "project:<name>", or "hierarchy".
    pub scope: String,
    /// Centroid vector in f32; length = `d` (typically 384 for all-MiniLM-L6-v2).
    pub centroid: Vec<f32>,
    /// Creation timestamp (unix seconds).
    pub created_at: i64,
    /// Embedding dimension (stored so a future K/d change purges cleanly).
    pub d: usize,
    /// K — total centroid count for this scope at creation time.
    pub k_total: usize,
}

/// Single entry in a chunk's soft-membership record.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AssignmentEntry {
    pub topic_id: u32,
    pub membership: f32,
}

/// LMDB-backed store wrapper. Clone-able; thread-safe internally via heed.
#[derive(Clone)]
pub struct CentroidStore {
    env: Arc<Env>,
    centroids: Database<U32<heed::byteorder::NativeEndian>, SerdeBincode<StoredCentroid>>,
    assignments: Database<U64<heed::byteorder::NativeEndian>, SerdeBincode<Vec<AssignmentEntry>>>,
    /// Dense membership vectors (one `Vec<f32>` of length K per chunk_id).
    /// Used by the online/mini-batch FCM (Phase 8) which needs
    /// element-wise comparison of full membership rows across iterations.
    memberships_dense: Database<U64<heed::byteorder::NativeEndian>, SerdeBincode<Vec<f32>>>,
    path: PathBuf,
}

const MAP_SIZE_BYTES: usize = 8 * 1024 * 1024 * 1024; // 8 GiB; plenty for 100k chunks × 5 topics.
const MAX_DBS: u32 = 8;

impl CentroidStore {
    /// Open or create a centroid store at the given directory. The directory
    /// is created if missing. Two sub-databases are opened: `centroids` and
    /// `assignments`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&path).map_err(|e| PgmcpError::file_io(&path, e))?;

        // SAFETY: heed's EnvOpenOptions::open is unsafe because aliasing rules
        // on mmap'd databases are the caller's responsibility. We only open
        // each path once (singleton per process); concurrent opens of the same
        // LMDB env would be unsound but we don't do that.
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(MAP_SIZE_BYTES)
                .max_dbs(MAX_DBS)
                .open(&path)
                .map_err(|e| {
                    PgmcpError::Other(format!(
                        "heed: failed to open LMDB env at {}: {}",
                        path.display(),
                        e
                    ))
                })?
        };

        let mut wtxn = env
            .write_txn()
            .map_err(|e| PgmcpError::Other(format!("heed: write_txn: {}", e)))?;
        let centroids: Database<U32<heed::byteorder::NativeEndian>, SerdeBincode<StoredCentroid>> =
            env.create_database(&mut wtxn, Some("centroids"))
                .map_err(|e| PgmcpError::Other(format!("heed: create centroids db: {}", e)))?;
        let assignments: Database<
            U64<heed::byteorder::NativeEndian>,
            SerdeBincode<Vec<AssignmentEntry>>,
        > = env
            .create_database(&mut wtxn, Some("assignments"))
            .map_err(|e| PgmcpError::Other(format!("heed: create assignments db: {}", e)))?;
        let memberships_dense: Database<
            U64<heed::byteorder::NativeEndian>,
            SerdeBincode<Vec<f32>>,
        > = env
            .create_database(&mut wtxn, Some("memberships_dense"))
            .map_err(|e| PgmcpError::Other(format!("heed: create memberships_dense db: {}", e)))?;
        wtxn.commit()
            .map_err(|e| PgmcpError::Other(format!("heed: commit: {}", e)))?;

        Ok(Self {
            env: Arc::new(env),
            centroids,
            assignments,
            memberships_dense,
            path,
        })
    }

    /// Fetch all stored centroids for a given scope.
    pub fn load_centroids(&self, scope: &str) -> Result<Vec<StoredCentroid>> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| PgmcpError::Other(format!("heed: read_txn: {}", e)))?;
        let mut out = Vec::new();
        for entry in self
            .centroids
            .iter(&rtxn)
            .map_err(|e| PgmcpError::Other(format!("heed: iter centroids: {}", e)))?
        {
            let (_id, record) =
                entry.map_err(|e| PgmcpError::Other(format!("heed: entry: {}", e)))?;
            if record.scope == scope {
                out.push(record);
            }
        }
        Ok(out)
    }

    /// Replace all centroids for a scope. Previous entries with the same scope
    /// are deleted (other scopes untouched).
    pub fn store_centroids(&self, scope: &str, records: &[StoredCentroid]) -> Result<()> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| PgmcpError::Other(format!("heed: write_txn: {}", e)))?;

        // Delete old entries for this scope.
        let mut to_delete: Vec<u32> = Vec::new();
        for entry in self
            .centroids
            .iter(&wtxn)
            .map_err(|e| PgmcpError::Other(format!("heed: iter: {}", e)))?
        {
            let (id, record) =
                entry.map_err(|e| PgmcpError::Other(format!("heed: entry: {}", e)))?;
            if record.scope == scope {
                to_delete.push(id);
            }
        }
        for id in to_delete {
            self.centroids
                .delete(&mut wtxn, &id)
                .map_err(|e| PgmcpError::Other(format!("heed: delete: {}", e)))?;
        }

        // Insert new entries keyed by (topic_id within this scope).
        // Use a counter that advances past existing keys to avoid collisions
        // with other scopes.
        let mut next_id = self.max_centroid_id(&wtxn)?.map_or(0u32, |x| x + 1);
        for record in records {
            self.centroids
                .put(&mut wtxn, &next_id, record)
                .map_err(|e| PgmcpError::Other(format!("heed: put centroid: {}", e)))?;
            next_id = next_id.saturating_add(1);
        }

        wtxn.commit()
            .map_err(|e| PgmcpError::Other(format!("heed: commit: {}", e)))?;
        Ok(())
    }

    fn max_centroid_id(&self, rtxn: &heed::RoTxn) -> Result<Option<u32>> {
        let mut last = None;
        for entry in self
            .centroids
            .iter(rtxn)
            .map_err(|e| PgmcpError::Other(format!("heed: iter: {}", e)))?
        {
            let (id, _) = entry.map_err(|e| PgmcpError::Other(format!("heed: entry: {}", e)))?;
            last = Some(id);
        }
        Ok(last)
    }

    /// Write a chunk's membership vector. Overwrites any previous entry.
    pub fn store_assignment(&self, chunk_id: i64, entries: &[AssignmentEntry]) -> Result<()> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| PgmcpError::Other(format!("heed: write_txn: {}", e)))?;
        let key = chunk_id as u64;
        self.assignments
            .put(&mut wtxn, &key, &entries.to_vec())
            .map_err(|e| PgmcpError::Other(format!("heed: put assignment: {}", e)))?;
        wtxn.commit()
            .map_err(|e| PgmcpError::Other(format!("heed: commit: {}", e)))?;
        Ok(())
    }

    /// Batch-write assignments in a single transaction.
    pub fn store_assignments_batch(&self, items: &[(i64, Vec<AssignmentEntry>)]) -> Result<()> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| PgmcpError::Other(format!("heed: write_txn: {}", e)))?;
        for (chunk_id, entries) in items {
            let key = *chunk_id as u64;
            self.assignments
                .put(&mut wtxn, &key, entries)
                .map_err(|e| PgmcpError::Other(format!("heed: put assignment: {}", e)))?;
        }
        wtxn.commit()
            .map_err(|e| PgmcpError::Other(format!("heed: commit: {}", e)))?;
        Ok(())
    }

    /// Read a chunk's membership vector.
    pub fn load_assignment(&self, chunk_id: i64) -> Result<Option<Vec<AssignmentEntry>>> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| PgmcpError::Other(format!("heed: read_txn: {}", e)))?;
        let key = chunk_id as u64;
        match self
            .assignments
            .get(&rtxn, &key)
            .map_err(|e| PgmcpError::Other(format!("heed: get: {}", e)))?
        {
            Some(v) => Ok(Some(v)),
            None => Ok(None),
        }
    }

    /// Store a dense membership vector (length K) for a chunk.
    pub fn store_membership_dense(&self, chunk_id: i64, membership: &[f32]) -> Result<()> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| PgmcpError::Other(format!("heed: write_txn: {}", e)))?;
        let key = chunk_id as u64;
        self.memberships_dense
            .put(&mut wtxn, &key, &membership.to_vec())
            .map_err(|e| PgmcpError::Other(format!("heed: put memb: {}", e)))?;
        wtxn.commit()
            .map_err(|e| PgmcpError::Other(format!("heed: commit: {}", e)))?;
        Ok(())
    }

    /// Batch version — all puts in one transaction.
    pub fn store_memberships_dense_batch(&self, items: &[(i64, Vec<f32>)]) -> Result<()> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| PgmcpError::Other(format!("heed: write_txn: {}", e)))?;
        for (chunk_id, v) in items {
            let key = *chunk_id as u64;
            self.memberships_dense
                .put(&mut wtxn, &key, v)
                .map_err(|e| PgmcpError::Other(format!("heed: put memb: {}", e)))?;
        }
        wtxn.commit()
            .map_err(|e| PgmcpError::Other(format!("heed: commit: {}", e)))?;
        Ok(())
    }

    /// Load a chunk's dense membership vector.
    pub fn load_membership_dense(&self, chunk_id: i64) -> Result<Option<Vec<f32>>> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| PgmcpError::Other(format!("heed: read_txn: {}", e)))?;
        let key = chunk_id as u64;
        self.memberships_dense
            .get(&rtxn, &key)
            .map_err(|e| PgmcpError::Other(format!("heed: get: {}", e)))
    }

    /// Purge all centroids + assignments + dense memberships (used when K or d changes).
    pub fn clear_all(&self) -> Result<()> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| PgmcpError::Other(format!("heed: write_txn: {}", e)))?;
        self.centroids
            .clear(&mut wtxn)
            .map_err(|e| PgmcpError::Other(format!("heed: clear centroids: {}", e)))?;
        self.assignments
            .clear(&mut wtxn)
            .map_err(|e| PgmcpError::Other(format!("heed: clear assignments: {}", e)))?;
        self.memberships_dense
            .clear(&mut wtxn)
            .map_err(|e| PgmcpError::Other(format!("heed: clear memberships_dense: {}", e)))?;
        wtxn.commit()
            .map_err(|e| PgmcpError::Other(format!("heed: commit: {}", e)))?;
        Ok(())
    }

    /// On-disk path of the store (the LMDB environment directory).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Default XDG-compliant path for the topic store: `$XDG_DATA_HOME/pgmcp/topics.lmdb`.
pub fn default_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("pgmcp").join("topics.lmdb");
    }
    if let Some(home) = dirs::home_dir() {
        return home
            .join(".local")
            .join("share")
            .join("pgmcp")
            .join("topics.lmdb");
    }
    PathBuf::from("/tmp/pgmcp/topics.lmdb")
}

#[allow(dead_code)]
fn _bytes_unused(_: Bytes) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_open_creates_directory_and_dbs() {
        let dir = TempDir::new().unwrap();
        let store = CentroidStore::open(dir.path().join("topics.lmdb")).unwrap();
        let loaded = store.load_centroids("global").unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_store_and_load_centroids_roundtrip() {
        let dir = TempDir::new().unwrap();
        let store = CentroidStore::open(dir.path().join("topics.lmdb")).unwrap();

        let records = vec![
            StoredCentroid {
                scope: "global".into(),
                centroid: vec![0.1_f32; 4],
                created_at: 12345,
                d: 4,
                k_total: 2,
            },
            StoredCentroid {
                scope: "global".into(),
                centroid: vec![0.9_f32; 4],
                created_at: 12345,
                d: 4,
                k_total: 2,
            },
        ];
        store.store_centroids("global", &records).unwrap();

        let loaded = store.load_centroids("global").unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].d, 4);
        assert!(
            (loaded[0].centroid[0] - 0.1).abs() < 1e-6
                || (loaded[0].centroid[0] - 0.9).abs() < 1e-6
        );
    }

    #[test]
    fn test_store_centroids_replaces_scope() {
        let dir = TempDir::new().unwrap();
        let store = CentroidStore::open(dir.path().join("topics.lmdb")).unwrap();

        let v1 = vec![StoredCentroid {
            scope: "global".into(),
            centroid: vec![1.0; 4],
            created_at: 1,
            d: 4,
            k_total: 1,
        }];
        store.store_centroids("global", &v1).unwrap();

        let v2 = vec![
            StoredCentroid {
                scope: "global".into(),
                centroid: vec![2.0; 4],
                created_at: 2,
                d: 4,
                k_total: 2,
            },
            StoredCentroid {
                scope: "global".into(),
                centroid: vec![3.0; 4],
                created_at: 2,
                d: 4,
                k_total: 2,
            },
        ];
        store.store_centroids("global", &v2).unwrap();

        let loaded = store.load_centroids("global").unwrap();
        assert_eq!(loaded.len(), 2);
        for rec in &loaded {
            assert!((rec.centroid[0] - 2.0).abs() < 1e-6 || (rec.centroid[0] - 3.0).abs() < 1e-6);
        }
    }

    #[test]
    fn test_store_multiple_scopes_isolated() {
        let dir = TempDir::new().unwrap();
        let store = CentroidStore::open(dir.path().join("topics.lmdb")).unwrap();

        store
            .store_centroids(
                "global",
                &[StoredCentroid {
                    scope: "global".into(),
                    centroid: vec![1.0; 4],
                    created_at: 1,
                    d: 4,
                    k_total: 1,
                }],
            )
            .unwrap();

        store
            .store_centroids(
                "project:alpha",
                &[StoredCentroid {
                    scope: "project:alpha".into(),
                    centroid: vec![2.0; 4],
                    created_at: 2,
                    d: 4,
                    k_total: 1,
                }],
            )
            .unwrap();

        let g = store.load_centroids("global").unwrap();
        let a = store.load_centroids("project:alpha").unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(a.len(), 1);
        assert!((g[0].centroid[0] - 1.0).abs() < 1e-6);
        assert!((a[0].centroid[0] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn test_assignments_roundtrip() {
        let dir = TempDir::new().unwrap();
        let store = CentroidStore::open(dir.path().join("topics.lmdb")).unwrap();

        let entries = vec![
            AssignmentEntry {
                topic_id: 3,
                membership: 0.7,
            },
            AssignmentEntry {
                topic_id: 8,
                membership: 0.2,
            },
        ];
        store.store_assignment(42, &entries).unwrap();

        let loaded = store.load_assignment(42).unwrap().expect("entry present");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].topic_id, 3);
        assert!((loaded[0].membership - 0.7).abs() < 1e-6);

        assert!(store.load_assignment(999).unwrap().is_none());
    }

    #[test]
    fn test_batch_assignments() {
        let dir = TempDir::new().unwrap();
        let store = CentroidStore::open(dir.path().join("topics.lmdb")).unwrap();

        let items: Vec<(i64, Vec<AssignmentEntry>)> = (0..10)
            .map(|i| {
                (
                    i,
                    vec![AssignmentEntry {
                        topic_id: i as u32,
                        membership: 0.5,
                    }],
                )
            })
            .collect();
        store.store_assignments_batch(&items).unwrap();

        for i in 0..10 {
            let loaded = store.load_assignment(i).unwrap().expect("entry present");
            assert_eq!(loaded[0].topic_id, i as u32);
        }
    }

    #[test]
    fn test_clear_all_removes_everything() {
        let dir = TempDir::new().unwrap();
        let store = CentroidStore::open(dir.path().join("topics.lmdb")).unwrap();

        store
            .store_centroids(
                "global",
                &[StoredCentroid {
                    scope: "global".into(),
                    centroid: vec![1.0; 4],
                    created_at: 1,
                    d: 4,
                    k_total: 1,
                }],
            )
            .unwrap();
        store
            .store_assignment(
                1,
                &[AssignmentEntry {
                    topic_id: 0,
                    membership: 1.0,
                }],
            )
            .unwrap();

        store.clear_all().unwrap();
        assert!(store.load_centroids("global").unwrap().is_empty());
        assert!(store.load_assignment(1).unwrap().is_none());
    }
} // keep Bytes import alive for potential future use
