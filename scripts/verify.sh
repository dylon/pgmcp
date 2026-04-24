#!/usr/bin/env bash
# Project verification gate. Runs unconditionally via the pre-push hook
# (.githooks/pre-push) and by developers before claiming work complete.
#
# CUDA is mandatory — there is no CPU-only build mode. The daemon's backend
# abstraction (src/fcm/) provides a CPU fallback at runtime for GPU-init
# failures, but every build links cudarc, ort/cuda, and the nvcc-generated
# fused-reduction PTX.
#
# Every gate must pass; the script exits non-zero on the first failure.
# There are no environment-variable overrides — if you need to skip a gate,
# fix the underlying issue instead.
#
# Exit codes:
#   0   = all gates passed
#   1   = a gate failed
#   2   = environment problem (cargo/nvcc missing, wrong cwd, etc.)

set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v cargo >/dev/null 2>&1; then
    echo "verify.sh: cargo not found on PATH" >&2
    exit 2
fi

if ! command -v nvcc >/dev/null 2>&1; then
    echo "verify.sh: nvcc not found on PATH (CUDA toolkit is mandatory)" >&2
    exit 2
fi

run_gate() {
    local name="$1"; shift
    local start
    start=$(date +%s)
    echo "=== ${name} ==="
    echo "+ $*"
    "$@"
    local elapsed=$(( $(date +%s) - start ))
    echo "    (${elapsed}s)"
    echo
}

run_gate "Gate 1/8: cargo fmt --check" \
    cargo fmt --check
run_gate "Gate 2/8: cargo build --all-targets" \
    cargo build --all-targets
run_gate "Gate 3/8: cargo clippy --all-targets -- -D warnings" \
    cargo clippy --all-targets -- -D warnings
run_gate "Gate 4/8: cargo test --release --bin pgmcp" \
    cargo test --release --bin pgmcp
run_gate "Gate 5/8: cargo test --release -p pgmcp-testing" \
    cargo test --release -p pgmcp-testing
run_gate "Gate 6/8: cargo test --release --test gpu_fallback_smoke -- --ignored" \
    cargo test --release --test gpu_fallback_smoke -- --ignored
run_gate "Gate 7/8: cargo smoke (GPU smoke scenarios)" \
    cargo smoke
# Gate 8: run every `tests/*.rs` across the workspace. Tier-C real-DB tests
# self-skip via `require_test_*!()` when `PGMCP_TEST_DATABASE_URL` is unset,
# so this stays green for contributors without a local Postgres+pgvector
# install; with the env var set, it becomes a full integration check.
run_gate "Gate 8/8: cargo test --release --tests" \
    cargo test --release --tests

echo "verify.sh: all gates passed"
