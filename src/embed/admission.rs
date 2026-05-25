//! Process-wide GPU admission control for embedder instances.
//!
//! BGE-M3 (XLM-RoBERTa-Large) weighs ~1.1 GiB resident in BF16. Several code
//! paths each construct their own [`Embedder`](super::model::Embedder) and
//! upload it to the GPU:
//!
//! - the embedding pool's `pool_size` workers (always on, resident for life), and
//! - the `embedding-migration` cron, which builds a transient copy every tick.
//!
//! Uncoordinated, these can exceed the VRAM budget on a small card (e.g. an
//! 8 GiB RTX 4060 Ti), producing the recurring
//! `XLM-RoBERTa forward: CUDA_ERROR_OUT_OF_MEMORY` / `XLMRobertaModel::new:
//! CUDA_ERROR_OUT_OF_MEMORY` failures. This module hands out a bounded number
//! of permits — one per resident embedder copy — so the total never exceeds
//! `embeddings.gpu_max_resident_embedders` (raised to at least `pool_size` so
//! the always-on workers never starve; see [`init`]).
//!
//! Pool workers acquire a permit for their entire lifetime ([`acquire_owned`]).
//! The migration cron uses [`try_acquire_owned`] (non-blocking) and defers to
//! its next tick when no permit is free, rather than piling a third copy onto
//! a full GPU.
//!
//! When `use_gpu = false` the semaphore is never initialized; both accessors
//! report "no gating" and every path runs unguarded (CPU embedders don't
//! contend for VRAM).

use std::sync::{Arc, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Set once at daemon startup (when `use_gpu` is true). `None` until then.
static ADMISSION: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// Initialize the global GPU embedder admission semaphore with `permits`
/// resident-copy slots. Idempotent: a second call is a no-op (the first
/// initialization at daemon startup wins), so re-running `make_backend` or
/// tests can't resize a live semaphore out from under in-flight permits.
///
/// Callers should pass `gpu_max_resident_embedders.max(pool_size)` so the
/// always-on pool workers can always obtain their permits; the headroom above
/// `pool_size` is what transient consumers (the migration cron) compete for.
pub fn init(permits: usize) {
    let _ = ADMISSION.set(Arc::new(Semaphore::new(permits.max(1))));
}

/// The global semaphore, or `None` when GPU admission is not in effect
/// (CPU mode / not yet initialized → callers proceed unguarded).
pub fn semaphore() -> Option<Arc<Semaphore>> {
    ADMISSION.get().cloned()
}

/// Acquire one resident-copy permit, blocking the calling thread until one is
/// free. Returns `None` when admission is disabled (`use_gpu = false`) — the
/// caller then proceeds unguarded. Returns `None` if the semaphore was closed
/// (only happens on teardown). The returned permit must be held for as long as
/// the embedder occupies the GPU; dropping it (including on unwind) frees the
/// slot.
///
/// `rt` is the worker thread's runtime handle; pool workers are plain
/// `std::thread`s, so we bridge into the async semaphore via `block_on`.
pub fn acquire_owned(rt: &tokio::runtime::Handle) -> Option<OwnedSemaphorePermit> {
    let sem = semaphore()?;
    rt.block_on(sem.acquire_owned()).ok()
}

/// Outcome of a non-blocking admission attempt. A three-state result (rather
/// than `Result<Option<_>, ()>`) so callers handle each case explicitly.
pub enum Admission {
    /// GPU admission is disabled (`use_gpu = false`) — proceed unguarded.
    Disabled,
    /// A resident-copy permit was granted; hold it for the embedder's lifetime.
    Granted(OwnedSemaphorePermit),
    /// The budget is fully used — defer (don't construct another embedder now).
    Deferred,
}

/// Try to acquire one resident-copy permit without blocking. Used by the
/// migration cron (which runs inside the tokio runtime) to avoid piling a
/// third resident embedder onto a full GPU: on [`Admission::Deferred`] it
/// skips the pass and retries on the next tick.
pub fn try_acquire_owned() -> Admission {
    match semaphore() {
        None => Admission::Disabled,
        Some(sem) => match sem.try_acquire_owned() {
            Ok(permit) => Admission::Granted(permit),
            Err(_) => Admission::Deferred,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // This is the ONLY test that touches the process-global `ADMISSION`, so
    // `init` is deterministic here (OnceLock is set-once).
    #[test]
    fn admission_bounds_resident_copies_and_defers() {
        init(2);
        assert!(semaphore().is_some(), "init should establish the semaphore");

        // Two resident slots can be taken...
        let p1 = match try_acquire_owned() {
            Admission::Granted(p) => p,
            _ => panic!("permit 1 of 2 should be granted"),
        };
        let p2 = match try_acquire_owned() {
            Admission::Granted(p) => p,
            _ => panic!("permit 2 of 2 should be granted"),
        };

        // ...the third is refused, so the migration cron defers instead of
        // piling a third resident embedder onto a full GPU.
        assert!(
            matches!(try_acquire_owned(), Admission::Deferred),
            "budget exhausted → try_acquire must defer"
        );

        // Freeing a slot makes it grantable again.
        drop(p1);
        assert!(
            matches!(try_acquire_owned(), Admission::Granted(_)),
            "a freed slot is grantable again"
        );
        drop(p2);
    }
}
