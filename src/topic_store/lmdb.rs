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
    /// Centroid vector in f32; length = `d` (1024 for BGE-M3).
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

    /// Read every (chunk_id, dense_membership) pair currently in the store.
    /// Used by `run_online_global_topic_scan` so it can convert per-chunk
    /// LMDB memberships into per-topic `chunk_ids` / `memberships` lists
    /// before persisting to Postgres `code_topics` / `chunk_topic_assignments`.
    ///
    /// Returns a `Vec` rather than an iterator because the read txn has to
    /// outlive every borrow returned through it, and the membership tables
    /// fit comfortably in RAM for any K we ship (K ≤ 500, n ≤ ~1.2M → ≤ 2.3
    /// GB at f32). Callers that need a tighter memory budget should stream
    /// directly off LMDB using `iter_memberships_dense_visit`.
    pub fn collect_memberships_dense(&self) -> Result<Vec<(i64, Vec<f32>)>> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| PgmcpError::Other(format!("heed: read_txn: {}", e)))?;
        let mut out = Vec::new();
        for entry in self
            .memberships_dense
            .iter(&rtxn)
            .map_err(|e| PgmcpError::Other(format!("heed: iter memb: {}", e)))?
        {
            let (key, value) =
                entry.map_err(|e| PgmcpError::Other(format!("heed: iter step: {}", e)))?;
            out.push((key as i64, value));
        }
        Ok(out)
    }

    /// Streaming version: invoke `visit` for each `(chunk_id, membership)`
    /// pair without materializing the full `Vec`. The closure returns
    /// `ControlFlow::Break(())` to stop iteration early.
    pub fn iter_memberships_dense_visit<F>(&self, mut visit: F) -> Result<()>
    where
        F: FnMut(i64, Vec<f32>) -> std::ops::ControlFlow<()>,
    {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| PgmcpError::Other(format!("heed: read_txn: {}", e)))?;
        for entry in self
            .memberships_dense
            .iter(&rtxn)
            .map_err(|e| PgmcpError::Other(format!("heed: iter memb: {}", e)))?
        {
            let (key, value) =
                entry.map_err(|e| PgmcpError::Other(format!("heed: iter step: {}", e)))?;
            if let std::ops::ControlFlow::Break(()) = visit(key as i64, value) {
                break;
            }
        }
        Ok(())
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
    fn collect_memberships_dense_returns_all_stored_pairs() {
        let dir = TempDir::new().unwrap();
        let store = CentroidStore::open(dir.path().join("memb.lmdb")).unwrap();

        // Store three dense membership rows.
        store.store_membership_dense(7, &[0.2, 0.5, 0.3]).unwrap();
        store.store_membership_dense(8, &[0.1, 0.9, 0.0]).unwrap();
        store.store_membership_dense(9, &[0.4, 0.4, 0.2]).unwrap();

        let mut collected = store.collect_memberships_dense().unwrap();
        collected.sort_by_key(|(cid, _)| *cid);

        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0].0, 7);
        assert_eq!(collected[0].1, vec![0.2, 0.5, 0.3]);
        assert_eq!(collected[1].0, 8);
        assert_eq!(collected[1].1, vec![0.1, 0.9, 0.0]);
        assert_eq!(collected[2].0, 9);
        assert_eq!(collected[2].1, vec![0.4, 0.4, 0.2]);
    }

    #[test]
    fn iter_memberships_dense_visit_short_circuits_on_break() {
        let dir = TempDir::new().unwrap();
        let store = CentroidStore::open(dir.path().join("memb2.lmdb")).unwrap();
        for cid in 1..=5 {
            store
                .store_membership_dense(cid, &[cid as f32 * 0.1])
                .unwrap();
        }
        let mut seen = 0usize;
        store
            .iter_memberships_dense_visit(|_chunk_id, _mu| {
                seen += 1;
                if seen == 2 {
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            })
            .unwrap();
        assert_eq!(seen, 2, "visit must stop on Break(()) without finishing");
    }

    #[test]
    fn warm_start_centroid_round_trip_recovers_centroids_unchanged() {
        let dir = TempDir::new().unwrap();
        let store = CentroidStore::open(dir.path().join("warm.lmdb")).unwrap();

        // Mimic the FCM warm-start payload: K=4, d=8, scope="global".
        let k = 4usize;
        let d = 8usize;
        let now = 1_000_000_i64;
        let records: Vec<StoredCentroid> = (0..k)
            .map(|i| StoredCentroid {
                scope: "global".into(),
                centroid: (0..d)
                    .map(|j| ((i as f32) * 10.0 + j as f32) / 100.0)
                    .collect(),
                created_at: now + i as i64,
                d,
                k_total: k,
            })
            .collect();
        store.store_centroids("global", &records).unwrap();

        let loaded = store.load_centroids("global").unwrap();
        assert_eq!(loaded.len(), k);
        for orig in &records {
            let found = loaded.iter().any(|l| {
                l.scope == orig.scope
                    && l.d == orig.d
                    && l.k_total == orig.k_total
                    && l.centroid
                        .iter()
                        .zip(orig.centroid.iter())
                        .all(|(a, b)| (a - b).abs() < 1e-6)
            });
            assert!(found, "centroid round-trip lost a record");
        }
    }

    #[test]
    fn warm_start_k_mismatch_load_returns_all_records_for_decision_layer() {
        // The LMDB store itself does not enforce K — it just returns
        // whatever records exist for the scope. The K-mismatch logic
        // lives in `load_warm_start_centroids` (src/cron/topic_clustering.rs)
        // and decides cold-restart based on `records.len() != k`. This
        // test guards the contract: load_centroids returns the full set,
        // letting the caller compare counts.
        let dir = TempDir::new().unwrap();
        let store = CentroidStore::open(dir.path().join("kmis.lmdb")).unwrap();
        let records: Vec<StoredCentroid> = (0..3)
            .map(|i| StoredCentroid {
                scope: "global".into(),
                centroid: vec![i as f32; 4],
                created_at: 0,
                d: 4,
                k_total: 3,
            })
            .collect();
        store.store_centroids("global", &records).unwrap();
        let loaded = store.load_centroids("global").unwrap();
        assert_eq!(
            loaded.len(),
            3,
            "load_centroids returns every stored record so the warm-start \
             decision layer can detect K mismatches"
        );
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

    // ========================================================================
    // Property tests
    // ========================================================================

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

        /// store_centroids → load_centroids returns the same records
        /// (up to ordering and bitwise equality on the f32 centroid data).
        #[test]
        fn prop_centroid_round_trip(
            k in 1usize..5,
            d in 1usize..8,
        ) {
            let dir = TempDir::new().unwrap();
            let store = CentroidStore::open(dir.path().join("rt.lmdb")).unwrap();
            let records: Vec<StoredCentroid> = (0..k)
                .map(|i| StoredCentroid {
                    scope: "rt".into(),
                    centroid: (0..d).map(|j| ((i * d + j) as f32).sin()).collect(),
                    created_at: 1000 + i as i64,
                    d,
                    k_total: k,
                })
                .collect();
            store.store_centroids("rt", &records).unwrap();

            let loaded = store.load_centroids("rt").unwrap();
            prop_assert_eq!(loaded.len(), records.len());
            // Every input centroid must appear in output (order not specified).
            for orig in &records {
                let found = loaded.iter().any(|l| {
                    l.centroid.len() == orig.centroid.len()
                        && l.centroid
                            .iter()
                            .zip(orig.centroid.iter())
                            .all(|(a, b)| (a - b).abs() < 1e-6)
                });
                let dim = orig.d;
                prop_assert!(found, "round-trip lost centroid with d={}", dim);
            }
        }

        /// store_assignment → load_assignment returns exactly what was stored.
        #[test]
        fn prop_assignment_round_trip(
            chunk_id in 1i64..10_000,
            n_entries in 1usize..5,
        ) {
            let dir = TempDir::new().unwrap();
            let store = CentroidStore::open(dir.path().join("rt2.lmdb")).unwrap();
            let entries: Vec<AssignmentEntry> = (0..n_entries)
                .map(|i| AssignmentEntry {
                    topic_id: i as u32,
                    membership: ((i + 1) as f32 * 0.1).clamp(0.0, 1.0),
                })
                .collect();
            store.store_assignment(chunk_id, &entries).unwrap();
            let loaded = store.load_assignment(chunk_id).unwrap().expect("present");
            prop_assert_eq!(loaded.len(), entries.len());
            for (l, e) in loaded.iter().zip(entries.iter()) {
                prop_assert_eq!(l.topic_id, e.topic_id);
                prop_assert!((l.membership - e.membership).abs() < 1e-6);
            }
        }

        /// store_membership_dense → load_membership_dense recovers the full
        /// vector bit-identically (within f32 tolerance).
        #[test]
        fn prop_membership_dense_round_trip(
            chunk_id in 1i64..10_000,
            dim in 1usize..64,
        ) {
            let dir = TempDir::new().unwrap();
            let store = CentroidStore::open(dir.path().join("rt3.lmdb")).unwrap();
            let membership: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.01).collect();
            store.store_membership_dense(chunk_id, &membership).unwrap();
            let loaded = store.load_membership_dense(chunk_id).unwrap().expect("present");
            prop_assert_eq!(loaded.len(), membership.len());
            for (l, m) in loaded.iter().zip(membership.iter()) {
                prop_assert!((l - m).abs() < 1e-6);
            }
        }
    }
} // keep Bytes import alive for potential future use
