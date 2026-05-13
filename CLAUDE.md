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

## Session-level mandates (`src/sessions.rs`)

pgmcp observes user prompts via the UserPromptSubmit hook
(`~/.claude/hooks/pgmcp-rag.sh` POSTs `{session_id, cwd, prompt}` to
`POST /api/session/observe`) and extracts imperative directives with a
tiered heuristic regex pipeline calibrated against the user's actual
prompt history. Extracted mandates are persisted by session_id with 12
polarities (always/never/prefer/avoid/remember/from_now_on/correction/
permission/constraint/mandate/process_rule/project_rule) and re-injected
on every subsequent prompt as `additionalContext` to alleviate the LLM's
short-term-memory problem.

The agent can introspect via the `session_mandates` MCP tool and promote
a session mandate to durable scope via `promote_session_mandate`
(inserts into `durable_mandates`; with `write_to_file=true`, appends to
the named target file under a `## Promoted session mandates (pgmcp)`
marker section, idempotent on re-run).

Prompts are persisted locally in `session_prompts` (sha256-deduped,
embedded for cross-session retrieval); same privacy posture as
`file_chunks` — purely local, no remote shipping.

## Software pattern catalog (`src/patterns/`)

The curated catalog ships ~810 entries across 14 paradigms in 21 per-family
files: `gof`, `solid_grasp`, `principles`, `functional`, `concurrency`,
`architecture`, `declarative`, `anti_patterns`, `code_smells`, `security`,
`testing`, `idioms`, `aop`, `observability`, `deployment`,
`data_engineering`, `api_design`, `ml_ai`, `distributed_data`,
`kubernetes`, and `sources` (registry). `kind` is constrained to
`pattern | anti_pattern | principle | code_smell`. `mod.rs` exposes the
`pat(...)` helper and assembles `pattern_seeds()`. To add a new pattern,
append a `pat(...)` call to the appropriate per-family file; referential
integrity tests in `mod.rs` automatically check slug/paradigm/source/kind
consistency. The current embedding signature is
`pgmcp-pattern-embedding-v3`; bump it whenever seed prose changes so
existing installs re-embed cleanly.

## CUDA host compiler pin (`.cargo/config.toml`)

The CUDA host compiler is force-pinned to `g++-14` via `.cargo/config.toml`
(`NVCC_CCBIN = { value = "g++-14", force = true }`) because GCC 15+ ships
C++23 `<functional>` (explicit object parameters / "deducing this") that
`nvcc` 12.x cannot parse. Without the pin, the `candle-kernels` transitive
build (via `cudaforge` → `nvcc moe_wmma*.cu`) explodes against the system
g++. `force = true` is required because Cargo's `[env]` is non-forcing by
default — a developer-exported `NVCC_CCBIN` would otherwise silently
shadow the project setting and resurrect the build break. Do not remove
the pin or `force = true` without verifying every transitive `.cu` compile
against your system g++. `scripts/verify.sh` preflights for `g++-14` so
misconfigured hosts fail fast with a clear message instead of in Gate 2.

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
