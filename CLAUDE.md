# pgmcp — Agent Working Rules

## Non-negotiable: verify before claiming work complete

Before declaring any code change complete, run the full verification gate:

    ./scripts/verify.sh

If any step fails, the work is not done. There are no environment-variable
overrides or opt-outs. The script is the contract.

`./scripts/verify.sh` is also enforced on every `git push` via the pre-push
hook at `.githooks/pre-push`. Activate it once per clone:

    git config core.hooksPath .githooks

Bypass (`git push --no-verify`) is reserved for genuine emergencies; do not
automate it.

## Quick wrappers during iteration

These are individual gates, NOT a replacement for `scripts/verify.sh`:

    cargo verify-build     # build --all-targets
    cargo verify-clippy    # clippy --all-targets -- -D warnings
    cargo verify-test      # test --release --bin pgmcp
    cargo smoke            # run --release --example gpu_smoke

## CUDA is mandatory

pgmcp does not support a CPU-only build. Every build links cudarc, `ort/cuda`,
and the nvcc-generated fused-reduction PTX (`src/fcm/cuda/kernels.cu`, compiled
into `$OUT_DIR/fcm_kernels.ptx` by `build.rs`). The CUDA toolkit (nvcc +
libcudart + libcublas + libcublasLt) must be installed.

At runtime, if CUDA initialization fails (no GPU, driver mismatch,
`CUDA_VISIBLE_DEVICES=""`, etc.), `src/fcm/make_backend()` logs a warning
and returns a `CpuFcmBackend`. The trait `FcmBackend` (in `src/fcm/mod.rs`)
is the seam where a future non-CUDA primary backend (Metal, ROCm, pure-CPU)
could be plugged in without feature gates.

There is no `cuda` cargo feature. `Cargo.toml` has no `[features]` table.

## Architecture: the FCM backend trait

Swappable compute paths live behind `src/fcm::FcmBackend`. Closed
construction-time choices (precision, backend kind) are enums. See
`src/fcm/mod.rs` for the canonical definitions.

- Traits where impls are swappable and may grow (`FcmBackend`).
- Enums where the choice is closed and construction-time
  (`GpuPrecision`, `BackendChoice`, `FcmError`).

## Why this file exists

On 2026-04-22 an agent added ~1000 lines under `#[cfg(feature = "cuda")]` and
declared the work complete without ever running `cargo build --features cuda`.
30 errors surfaced when the user forced the build. On 2026-04-23 the feature
gate was removed entirely: CUDA became mandatory and the trait-based FCM
backend replaced feature-gated conditional code. `scripts/verify.sh` plus
this file plus the pre-push hook make the old failure mode structurally
impossible — there are no cargo features left to forget.

See
`~/.claude/projects/-home-dylon-Workspace-f1r3fly-io-pgmcp/memory/feedback_feature_gated_build_verification.md`
for the after-action record.
