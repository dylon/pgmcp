# Developing pgmcp

## Prerequisites

- Rust 1.85+ (edition 2024)
- PostgreSQL 15+ with `pgvector` and `pg_trgm` extensions
- AOCL-BLIS (for ndarray BLAS on the CPU fallback; Arch: `pacman -S aocl-blis`)
- **CUDA toolkit 12+ with `nvcc` on PATH, plus an NVIDIA GPU.** CUDA is
  mandatory. There is no CPU-only build mode; `Cargo.toml` has no
  `[features]` table. The daemon's `src/fcm/` module provides a CPU
  fallback at runtime if GPU init fails, but the CPU path is not a
  build-time configuration.

## First-clone setup

    git clone <repo>
    cd pgmcp
    git config core.hooksPath .githooks     # activate pre-push verification
    ./scripts/verify.sh                      # confirms your toolchain

The `git config core.hooksPath` step is per-clone (not tracked in the repo).
Skipping it leaves your pushes unverified and risks the failure mode that
motivated this process. See `CLAUDE.md` for history.

## Verification gates

`./scripts/verify.sh` runs all six gates unconditionally. Gates fail-fast;
script exits non-zero on the first failure.

| # | Command | Purpose |
|---|---------|---------|
| 1 | `cargo fmt --check` | Formatting |
| 2 | `cargo build --all-targets` | Full build (nvcc runs in build.rs) |
| 3 | `cargo clippy --all-targets -- -D warnings` | Lints clean |
| 4 | `cargo test --release --bin pgmcp` | Unit / proptest suite |
| 5 | `cargo test --release --test gpu_fallback_smoke -- --ignored` | GPU-init-failure → CPU-fallback path |
| 6 | `cargo smoke` | GPU smoke scenarios (`examples/gpu_smoke.rs`) |

All six gates require a working CUDA toolkit and GPU. Gate 5 intentionally
forces GPU-unavailable via `CUDA_VISIBLE_DEVICES=""` to exercise the CPU
fallback inside `src/fcm/make_backend`.

## Adding a compile-time choice

`Cargo.toml` has no `[features]` table and we do not plan to reintroduce
one. If a future change genuinely needs a swappable implementation
(e.g. Metal / ROCm backend), add it as a new `FcmBackend` impl under
`src/fcm/` and wire it through `BackendChoice` + `make_backend`.
Trait dispatch is cheap relative to GEMM cost; feature gates are not.

## CI

`.github/workflows/verify.yml` targets a self-hosted runner labelled
`[self-hosted, linux, cuda]`. It runs `scripts/verify.sh` end-to-end on
every push and PR to `main`. `ubuntu-latest` jobs are not viable because
the CUDA toolkit is required at build time.

## Memory / LMDB / mmap scratch files

Several features (topic clustering warm-start, online FCM, FCM data matrix)
use persistent and scratch storage:

- LMDB centroid store: `$XDG_DATA_HOME/pgmcp/topics.lmdb`
  (falls back to `$HOME/.local/share/pgmcp/topics.lmdb`)
- FCM mmap scratch: `$XDG_CACHE_HOME/pgmcp/fcm-scratch-<pid>-<ts>.dat`
  (auto-unlinked on drop)

These are safe to delete between runs; the daemon recreates them.
