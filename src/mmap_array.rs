//! Memory-mapped `Array2<f32>` backed by a scratch file.
//!
//! Used by global topic clustering: the full `n × 384` f32 embedding matrix
//! is too large to hold in anonymous heap for large indexes (n > ~500k hits
//! multi-GB), but is always accessed row-by-row with BLAS-friendly contiguous
//! layout. Backing it with a memory-mapped scratch file means pages can be
//! evicted by the OS under pressure (they're re-readable from the file) and
//! BLAS GEMM works on them via `ArrayViewMut2<f32>` without extra copies.
//!
//! The file is automatically unlinked when the `MmapArrayF32` is dropped.

use std::path::{Path, PathBuf};

use memmap2::{MmapMut, MmapOptions};
use ndarray::{ArrayView2, ArrayViewMut2};

use crate::error::{PgmcpError, Result};

/// A memory-mapped (n × d) f32 matrix.
///
/// SAFETY: the mmap is page-aligned (`MmapMut` guarantees page alignment),
/// and `f32` has alignment 4 which is a divisor of 4096. Constructing
/// `ArrayView2<f32>` from the mmap pointer is sound as long as nothing else
/// writes to the file (we own it exclusively via the `tempfile::NamedTempFile`).
pub struct MmapArrayF32 {
    _file: tempfile::NamedTempFile,
    mmap: MmapMut,
    n: usize,
    d: usize,
    path: PathBuf,
}

impl MmapArrayF32 {
    /// Create a new scratch-file-backed (n × d) f32 matrix initialized to 0.
    ///
    /// `scratch_dir` is the directory where the anonymous scratch file is
    /// created. The file is unique per instance and is unlinked on `Drop`.
    pub fn new(n: usize, d: usize, scratch_dir: &Path) -> Result<Self> {
        if n == 0 || d == 0 {
            return Err(PgmcpError::Other(format!(
                "MmapArrayF32::new: n={}, d={} (both must be > 0)",
                n, d
            )));
        }
        std::fs::create_dir_all(scratch_dir).map_err(|e| PgmcpError::file_io(scratch_dir, e))?;

        let bytes = n
            .checked_mul(d)
            .and_then(|nd| nd.checked_mul(std::mem::size_of::<f32>()))
            .ok_or_else(|| {
                PgmcpError::Other(format!(
                    "MmapArrayF32::new: n={} × d={} overflows usize",
                    n, d
                ))
            })?;

        let tf = tempfile::Builder::new()
            .prefix("fcm-scratch-")
            .suffix(".dat")
            .tempfile_in(scratch_dir)
            .map_err(|e| PgmcpError::file_io(scratch_dir, e))?;

        let path = tf.path().to_path_buf();

        // Extend to the required size. `set_len` zero-fills on Linux ext4/xfs.
        tf.as_file()
            .set_len(bytes as u64)
            .map_err(|e| PgmcpError::file_io(&path, e))?;

        let mmap = unsafe {
            MmapOptions::new()
                .len(bytes)
                .map_mut(tf.as_file())
                .map_err(|e| PgmcpError::file_io(&path, e))?
        };

        Ok(Self {
            _file: tf,
            mmap,
            n,
            d,
            path,
        })
    }

    /// Immutable ndarray view over the mmap as an (n × d) matrix.
    pub fn view(&self) -> ArrayView2<'_, f32> {
        let slice: &[f32] = unsafe {
            std::slice::from_raw_parts(self.mmap.as_ptr() as *const f32, self.n * self.d)
        };
        ArrayView2::from_shape((self.n, self.d), slice).expect("shape matches allocation")
    }

    /// Mutable ndarray view over the mmap as an (n × d) matrix.
    pub fn view_mut(&mut self) -> ArrayViewMut2<'_, f32> {
        let slice: &mut [f32] = unsafe {
            std::slice::from_raw_parts_mut(self.mmap.as_mut_ptr() as *mut f32, self.n * self.d)
        };
        ArrayViewMut2::from_shape((self.n, self.d), slice).expect("shape matches allocation")
    }

    /// Write one row of `d` f32 values at index `i`. L2-normalizes the row in place.
    /// Returns `Err` if `values.len() != d` or `i >= n`.
    pub fn write_row_l2_normalized(&mut self, i: usize, values: &[f32]) -> Result<()> {
        if i >= self.n {
            return Err(PgmcpError::Other(format!(
                "MmapArrayF32::write_row: i={} out of bounds (n={})",
                i, self.n
            )));
        }
        if values.len() != self.d {
            return Err(PgmcpError::Other(format!(
                "MmapArrayF32::write_row: values.len()={}, expected d={}",
                values.len(),
                self.d
            )));
        }

        let norm_sq: f32 = values.iter().map(|v| v * v).sum();
        let norm = norm_sq.sqrt();
        let inv_norm = if norm > 1e-12 { 1.0 / norm } else { 0.0 };

        let offset = i * self.d;
        let slice: &mut [f32] = unsafe {
            std::slice::from_raw_parts_mut((self.mmap.as_mut_ptr() as *mut f32).add(offset), self.d)
        };
        for (dst, &src) in slice.iter_mut().zip(values.iter()) {
            *dst = src * inv_norm;
        }
        Ok(())
    }

    /// Hint the OS that we want these pages resident (sequential prefetch).
    /// Best-effort; ignores failures.
    pub fn advise_sequential(&self) {
        let _ = self.mmap.advise(memmap2::Advice::Sequential);
    }

    pub fn nrows(&self) -> usize {
        self.n
    }
    pub fn ncols(&self) -> usize {
        self.d
    }
    pub fn byte_size(&self) -> usize {
        self.n * self.d * std::mem::size_of::<f32>()
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flush pending writes to disk synchronously. Not normally required —
    /// the OS write-back will flush lazily — but useful in tests that want
    /// deterministic durability.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.mmap.flush()
    }
}

/// Resolve the scratch directory from config or defaults.
///
/// Precedence:
/// 1. Config-provided `topic_scratch_dir` if set.
/// 2. `$XDG_CACHE_HOME/pgmcp` if XDG_CACHE_HOME is set.
/// 3. `$HOME/.cache/pgmcp` on Unix.
/// 4. `/tmp/pgmcp` as a last resort.
pub fn resolve_scratch_dir(configured: Option<&Path>) -> PathBuf {
    if let Some(p) = configured {
        return p.to_path_buf();
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("pgmcp");
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".cache").join("pgmcp");
    }
    PathBuf::from("/tmp/pgmcp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_mmap_array_zero_initialized() {
        let dir = TempDir::new().unwrap();
        let arr = MmapArrayF32::new(16, 8, dir.path()).unwrap();
        let view = arr.view();
        for ((_i, _j), v) in view.indexed_iter() {
            assert_eq!(*v, 0.0);
        }
    }

    #[test]
    fn test_mmap_array_write_row_l2_normalized() {
        let dir = TempDir::new().unwrap();
        let mut arr = MmapArrayF32::new(4, 3, dir.path()).unwrap();
        arr.write_row_l2_normalized(1, &[3.0, 4.0, 0.0]).unwrap();
        let view = arr.view();
        let row = view.row(1);
        assert!((row[0] - 0.6).abs() < 1e-6);
        assert!((row[1] - 0.8).abs() < 1e-6);
        assert!(row[2].abs() < 1e-6);
        let norm: f32 = row.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_mmap_array_write_row_rejects_bad_len() {
        let dir = TempDir::new().unwrap();
        let mut arr = MmapArrayF32::new(4, 3, dir.path()).unwrap();
        assert!(arr.write_row_l2_normalized(0, &[1.0, 2.0]).is_err());
    }

    #[test]
    fn test_mmap_array_write_row_rejects_oob() {
        let dir = TempDir::new().unwrap();
        let mut arr = MmapArrayF32::new(4, 3, dir.path()).unwrap();
        assert!(arr.write_row_l2_normalized(5, &[1.0, 2.0, 3.0]).is_err());
    }

    #[test]
    fn test_mmap_array_byte_size() {
        let dir = TempDir::new().unwrap();
        let arr = MmapArrayF32::new(100, 384, dir.path()).unwrap();
        assert_eq!(arr.byte_size(), 100 * 384 * 4);
        assert_eq!(arr.nrows(), 100);
        assert_eq!(arr.ncols(), 384);
    }

    #[test]
    fn test_mmap_array_write_all_rows() {
        let dir = TempDir::new().unwrap();
        let n = 50;
        let d = 16;
        let mut arr = MmapArrayF32::new(n, d, dir.path()).unwrap();
        for i in 0..n {
            let row: Vec<f32> = (0..d).map(|j| (i * d + j) as f32).collect();
            arr.write_row_l2_normalized(i, &row).unwrap();
        }
        let view = arr.view();
        for i in 0..n {
            let row = view.row(i);
            let norm: f32 = row.iter().map(|v| v * v).sum::<f32>().sqrt();
            // Row of zeros stays at 0; all our rows are non-zero here.
            assert!((norm - 1.0).abs() < 1e-5, "row {} norm={}", i, norm);
        }
    }

    #[test]
    fn test_mmap_array_rejects_zero_dims() {
        let dir = TempDir::new().unwrap();
        assert!(MmapArrayF32::new(0, 4, dir.path()).is_err());
        assert!(MmapArrayF32::new(4, 0, dir.path()).is_err());
    }

    #[test]
    fn test_resolve_scratch_dir_with_configured() {
        let p = PathBuf::from("/tmp/custom");
        assert_eq!(resolve_scratch_dir(Some(&p)), p);
    }

    #[test]
    fn test_scratch_file_unlinked_on_drop() {
        let dir = TempDir::new().unwrap();
        let path = {
            let arr = MmapArrayF32::new(4, 4, dir.path()).unwrap();
            let p = arr.path().to_path_buf();
            assert!(p.exists(), "scratch file should exist while arr is alive");
            p
        };
        assert!(!path.exists(), "scratch file should be unlinked after drop");
    }

    // ========================================================================
    // Property tests
    // ========================================================================

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

        /// Write values into an `MmapArrayF32` via `view_mut` and read them
        /// back through `view`. Writes through the mmap and reads through
        /// the same mmap must return the same values.
        #[test]
        fn prop_round_trip_write_read_identity(
            n in 1usize..16,
            d in 1usize..32,
        ) {
            let dir = TempDir::new().unwrap();
            let mut arr = MmapArrayF32::new(n, d, dir.path()).unwrap();
            for i in 0..n {
                for j in 0..d {
                    arr.view_mut()[[i, j]] = (i as f32) * 1000.0 + (j as f32);
                }
            }
            let v = arr.view();
            for i in 0..n {
                for j in 0..d {
                    let expected = (i as f32) * 1000.0 + (j as f32);
                    let got = v[[i, j]];
                    prop_assert!((got - expected).abs() < 1e-6,
                        "[{},{}]: expected {}, got {}", i, j, expected, got);
                }
            }
        }

        /// A freshly created `MmapArrayF32` is zero-initialised.
        #[test]
        fn prop_zero_initialized_after_create(
            n in 1usize..16,
            d in 1usize..32,
        ) {
            let dir = TempDir::new().unwrap();
            let arr = MmapArrayF32::new(n, d, dir.path()).unwrap();
            for &v in arr.view().iter() {
                prop_assert_eq!(v, 0.0);
            }
        }
    }
}
