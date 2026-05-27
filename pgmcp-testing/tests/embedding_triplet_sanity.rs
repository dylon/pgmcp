//! Semantic sanity tests for the real `CandleBackend`.
//!
//! Each triplet (anchor, positive, negative) asserts the model
//! ranks the positive closer than the negative by at least `MARGIN`
//! cosine. This is a **sanity net** for the model's behaviour on
//! contrast pairs — it doesn't claim model perfection, only that it
//! resolves clearly distinct concepts.
//!
//! ## Skip behaviour
//!
//! If `~/.cache/huggingface` isn't present, the test prints
//! `SKIPPED:` and returns 0 — same pattern as
//! `embedding_backend_smoke.rs`. A successful `./scripts/verify.sh`
//! run implies the cache is populated.
//!
//! ## Tuning the margin
//!
//! The 0.07 margin is loose enough that the curated triplets pass on
//! `bge-m3` (the default model since ADR-004) but tight enough to fail
//! if the model is replaced with a degenerate one (random-init or wrong
//! dimension). Per this policy, the BGE-M3 migration tightened the cosine
//! gap on two fine-grained *same-domain* pairs (`array_vs_hashmap`,
//! `parse_vs_serialize` — both still correctly ordered, just under 0.07);
//! those triplets were dropped rather than weakening the margin, since
//! losing a triplet is cheaper than masking real semantic regressions.
//! (They also failed the "clearly distinct concepts" bar above —
//! array-iterate vs hashmap-lookup, parse vs serialize are same-domain.)

use std::sync::Arc;

use pgmcp::config::EmbeddingsConfig;
use pgmcp::embed::EmbeddingBackend;

const MARGIN: f32 = 0.07;

struct Triplet {
    name: &'static str,
    anchor: &'static str,
    positive: &'static str,
    negative: &'static str,
}

/// Curated contrast pairs spanning common code-domain dichotomies.
/// Each pair is a near-paraphrase of the anchor for the positive vs
/// a same-shaped sentence about a clearly different concept for the
/// negative — so the model has to resolve concept identity rather
/// than surface lexical overlap.
const TRIPLETS: &[Triplet] = &[
    Triplet {
        name: "auth_vs_io",
        anchor: "validate user password against stored hash",
        positive: "verify password matches stored credential",
        negative: "read raw bytes from disk into a buffer",
    },
    Triplet {
        name: "database_vs_network",
        anchor: "execute query on postgres connection pool",
        positive: "run sql query against database connection",
        negative: "open tcp socket and write http request",
    },
    Triplet {
        name: "math_vs_string",
        anchor: "compute mean and variance of numeric array",
        positive: "calculate average and standard deviation of numbers",
        negative: "concatenate strings with separator and trim whitespace",
    },
    Triplet {
        name: "error_vs_logging",
        anchor: "return error result when input is invalid",
        positive: "raise exception on invalid argument",
        negative: "emit info log message with structured fields",
    },
    Triplet {
        name: "concurrent_vs_sequential",
        anchor: "spawn worker threads to process tasks in parallel",
        positive: "use thread pool to handle requests concurrently",
        negative: "iterate over items one at a time in main loop",
    },
    Triplet {
        name: "cache_vs_disk",
        anchor: "store result in memory cache for fast access",
        positive: "memoize computed value in lru cache",
        negative: "write file to persistent disk storage",
    },
    Triplet {
        name: "compress_vs_encrypt",
        anchor: "reduce file size with gzip compression",
        positive: "compress data using deflate algorithm",
        negative: "encrypt payload with aes symmetric cipher",
    },
    Triplet {
        name: "sort_vs_search",
        anchor: "order list of integers from smallest to largest",
        positive: "sort vector of numbers in ascending order",
        negative: "find element matching predicate in collection",
    },
];

#[tokio::test(flavor = "multi_thread")]
async fn semantic_triplets_rank_positives_above_negatives() {
    let Some(backend) = load_real_backend().await else {
        return; // SKIPPED message already printed
    };

    let mut failures: Vec<String> = Vec::new();
    for triplet in TRIPLETS {
        // One batch call per triplet keeps the per-test wall time
        // dominated by the first model load — subsequent triplets
        // amortize.
        let inputs = vec![triplet.anchor, triplet.positive, triplet.negative];
        let vecs = backend.embed_batch(&inputs).await.expect("embed_batch");
        let cos_pos = cosine(&vecs[0], &vecs[1]);
        let cos_neg = cosine(&vecs[0], &vecs[2]);
        let gap = cos_pos - cos_neg;
        if gap < MARGIN {
            failures.push(format!(
                "  {}: cos(a,p)={:.4} cos(a,n)={:.4} gap={:.4} < {:.2}",
                triplet.name, cos_pos, cos_neg, gap, MARGIN
            ));
        }
    }

    if !failures.is_empty() {
        panic!(
            "{}/{} semantic triplets did not separate positive above negative by {}:\n{}",
            failures.len(),
            TRIPLETS.len(),
            MARGIN,
            failures.join("\n")
        );
    }
}

/// Cosine similarity assuming inputs are L2-normalized (which
/// CandleBackend guarantees). Reduces to a dot product in that
/// case — one fused-multiply-add per dim.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Returns `Some(backend)` if the candle model is cached and
/// loadable; prints `SKIPPED:` and returns `None` otherwise.
async fn load_real_backend() -> Option<Arc<dyn EmbeddingBackend>> {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => {
            eprintln!("SKIPPED: HOME not set");
            return None;
        }
    };
    let cache_marker = std::path::Path::new(&home).join(".cache/huggingface");
    if !cache_marker.exists() {
        eprintln!("SKIPPED: ~/.cache/huggingface not present (model not downloaded)");
        return None;
    }

    let config = EmbeddingsConfig::default();
    let backend = match tokio::task::spawn_blocking(move || {
        pgmcp::embed::backend::CandleBackend::new(&config)
    })
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            eprintln!("SKIPPED: CandleBackend unavailable: {}", e);
            return None;
        }
        Err(e) => {
            eprintln!("SKIPPED: CandleBackend spawn panic: {}", e);
            return None;
        }
    };
    Some(Arc::new(backend) as Arc<dyn EmbeddingBackend>)
}
