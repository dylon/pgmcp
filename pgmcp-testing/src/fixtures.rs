//! Factory functions returning ready-to-use test inputs.
//!
//! - [`test_config`] returns a stock `pgmcp::config::Config`. Same as
//!   `Config::default()`, but provided as a function so future fixture
//!   tweaks (e.g. setting `embeddings.dimensions = 8` for fast tests) live
//!   in one place.
//! - [`test_embedding`] returns a deterministic L2-normalized fp32 vector
//!   keyed by an arbitrary seed string. Useful for setting up
//!   `MockDbClient::semantic_search_results` with realistic-looking
//!   embeddings without invoking a real model.
//! - [`synthetic_corpus`] — the 30-chunk topic-clustering corpus and
//!   the 5-file graph corpus used by the Phase G/H/I/J oracles.
//! - [`synthetic_git_history`] — planted co-change patterns
//!   (Jaccard 1.0, 0.5, 0.0) used by the Phase I `find_coupled_files`
//!   oracle.
//!
//! Phase 5 will add `test_context()` returning a `SystemContext` backed by
//! `MockDbClient` + `DeterministicEmbeddingBackend`. That belongs here.

pub mod synthetic_corpus;
pub mod synthetic_git_history;

use pgmcp::config::Config;

/// Default test configuration. Currently identical to `Config::default()`.
/// Re-export with a stable name so tests don't depend on `Default` impl
/// stability.
pub fn test_config() -> Config {
    Config::default()
}

/// Deterministic L2-normalized embedding for tests.
///
/// `dim` controls the vector length (typically 384 to match the production
/// fastembed model). `seed` is hashed (xxh3-style folding via wrapping
/// arithmetic) to produce a reproducible sequence of f32s, then L2
/// normalized. Two calls with the same `(dim, seed)` always return the same
/// vector.
pub fn test_embedding(dim: usize, seed: &str) -> Vec<f32> {
    // Cheap stable hash: not cryptographic, just deterministic.
    let mut state: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(0x100_0000_01b3);
    }

    let mut v: Vec<f32> = Vec::with_capacity(dim);
    for i in 0..dim {
        // Squeeze a new pseudorandom 32-bit value out of state per index.
        let mut s = state.wrapping_add(i as u64);
        s ^= s >> 33;
        s = s.wrapping_mul(0xff51_afd7_ed55_8ccd);
        s ^= s >> 33;
        // Map the high 24 bits into [-1.0, 1.0] (avoids subnormals).
        let bits = (s >> 40) as u32; // 24 bits
        let signed = (bits as i32) - (1 << 23);
        v.push((signed as f32) / (1 << 23) as f32);
    }

    // L2-normalize (matches the production embedding model's invariant).
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_is_deterministic() {
        let a = test_embedding(384, "alpha");
        let b = test_embedding(384, "alpha");
        assert_eq!(a, b);
    }

    #[test]
    fn test_embedding_different_seeds_differ() {
        let a = test_embedding(384, "alpha");
        let b = test_embedding(384, "beta");
        assert_ne!(a, b);
    }

    #[test]
    fn test_embedding_is_unit_normalized() {
        let v = test_embedding(384, "alpha");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm = {}", norm);
    }

    #[test]
    fn test_config_returns_default() {
        let _c = test_config();
        // Just ensure no panic and the type matches.
    }
}
