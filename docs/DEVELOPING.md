# Developing pgmcp

## Prerequisites

- Rust 1.85+ (edition 2024)
- PostgreSQL 15+ with `pgvector` and `pg_trgm` extensions
- AOCL-BLIS (for ndarray BLAS used by deterministic CPU FCM tests; Arch: `pacman -S aocl-blis`)
- **CUDA toolkit 12+ with `nvcc` on PATH, plus an NVIDIA GPU.** CUDA is
  mandatory. There is no CPU-only build mode; `Cargo.toml` has no crate
  feature flags. Production compute paths fail closed when GPU init fails.

## First-clone setup

    git clone <repo>
    cd pgmcp
    git config core.hooksPath .githooks     # activate pre-push verification
    ./scripts/verify.sh                      # confirms your toolchain

The `git config core.hooksPath` step is per-clone (not tracked in the repo).
Skipping it leaves your pushes unverified and risks the failure mode that
motivated this process. See `CLAUDE.md` for history.

## Verification gates

`./scripts/verify.sh` runs all nine gates unconditionally. Gates fail-fast;
script exits non-zero on the first failure.

| # | Command | Purpose |
|---|---------|---------|
| 1 | `cargo fmt --check` | Formatting |
| 2 | `cargo build --all-targets` | Full build (nvcc runs in build.rs) |
| 3 | `cargo clippy --all-targets -- -D warnings` | Lints clean |
| 4 | `cargo build + test --release --bin pgmcp` | Unit / proptest suite |
| 5 | `cargo test --release -p pgmcp-testing` | Integration suite (one `all` binary) |
| 6 | `cargo test --release --test gpu_fallback_smoke -- --ignored` | GPU-init-failure fail-closed behavior |
| 7 | `cargo smoke` | GPU smoke scenarios (`examples/gpu_smoke.rs`) |
| 8 | `cargo test --release --tests` | Root-package integration tests |
| 9 | `pgmcp bug-gate` | Boy Scout: no open bugs anchored to changed files |

All gates require a working CUDA toolkit and GPU. Gate 6 intentionally
forces GPU-unavailable via `CUDA_VISIBLE_DEVICES=""` to verify that production
FCM returns a degenerate result instead of silently switching to CPU.

## Integration-test layout (`pgmcp-testing/tests/`)

**`pgmcp-testing` sets `autotests = false`.** Adding a `tests/foo.rs` is not
enough — you must also add `mod foo;` to `tests/main.rs`, or the file is never
compiled and its tests silently never run.

Why: Cargo autodiscovers every `tests/*.rs` as its own `[[test]]` target. With
234 such files that meant 234 independent crates, each running full `opt-level=3`
codegen and then *statically linking* the ~282 MB `libpgmcp` rlib plus candle,
cudarc, ort, and tree-sitter into a ~105 MB executable — **≈23.5 GB of linker
output to run the suite**, and well over an hour of wall time. Routing every file
through the single `tests/main.rs` target collapses that to one compile and one
link (a single ~220 MB binary, ~8 min cold including all dependencies).

Consequences to know:

- Test names gain their file as a module prefix, e.g.
  `mcp_tool_smoke::documentation_guidelines_returns_the_full_static_list`.
  Substring filters (`cargo test documentation_guidelines`, `nextest run …`) are
  unaffected.
- Shared helpers live in `tests/common/mod.rs`, declared **once** in
  `tests/main.rs`. Within a test file, reference them as `crate::common::…`
  (not `common::…`, which would resolve relative to that file's module).
- All files now share one process under `cargo test` (they get one process each
  under `cargo nextest`). Do not introduce process-global mutation —
  `env::set_var`, `set_current_dir`, a global `tracing_subscriber` init — into a
  test file without serializing it.
- The root package (`pgmcp/tests/`, 7 files) still uses autodiscovery, because
  Gate 6 invokes `--test gpu_fallback_smoke` by target name.

The linker is also pinned to **mold** via `.cargo/config.toml` (`-C
link-arg=-fuse-ld=mold`); see the comment there for why it must share the
`[build] rustflags` array rather than live in a `[target.*]` block.

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
