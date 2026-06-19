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

# Host-compiler preflight. cudaforge passes NVCC_CCBIN to nvcc -ccbin; the
# .cargo/config.toml force-pins it to g++-14. Verify the binary actually
# exists and is GCC 14.x — a missing or wrong-version ccbin would otherwise
# explode inside Gate 2 with a wall of unrelated <functional> template errors.
if ! command -v g++-14 >/dev/null 2>&1; then
    echo "verify.sh: g++-14 not found on PATH. CUDA 12.x nvcc cannot use" >&2
    echo "  the system g++ (>= 15 ships C++23 headers nvcc can't parse)." >&2
    echo "  Arch:   pacman -S gcc14" >&2
    echo "  Debian: apt install g++-14" >&2
    exit 2
fi
ccbin_major=$(g++-14 -dumpversion | cut -d. -f1)
if [ "${ccbin_major}" != "14" ]; then
    echo "verify.sh: g++-14 reports version ${ccbin_major}.x, expected 14.x" >&2
    exit 2
fi

# Test-DB visibility preflight (NOT a hard gate — the real-DB suites are
# designed to self-skip without a local Postgres+pgvector so verify stays green
# for contributors, see Gate 8 below). But a *silent* skip is how an
# enum-vs-text SQL regression once shipped green: the oracle test that would
# have caught it self-skipped and nobody noticed. So surface the skip LOUDLY
# here — never silently — without failing the run.
test_db_authority=""
if [ -n "${PGMCP_TEST_DATABASE_URL:-}" ]; then
    test_db_authority="PGMCP_TEST_DATABASE_URL"
elif [ -f "${HOME}/.config/pgmcp/test-config.toml" ]; then
    test_db_authority="~/.config/pgmcp/test-config.toml"
elif [ -f "${HOME}/.config/pgmcp/config.toml" ]; then
    test_db_authority="~/.config/pgmcp/config.toml"
fi
if [ -n "${test_db_authority}" ]; then
    echo "verify.sh: test-DB authority = ${test_db_authority}; real-DB suites will RUN."
    echo
else
    echo "verify.sh: WARNING — no test-DB authority configured" >&2
    echo "  (PGMCP_TEST_DATABASE_URL / ~/.config/pgmcp/{test-,}config.toml)." >&2
    echo "  The real-DB suites (Gate 5 + the Tier-C tests in Gate 8) will SELF-SKIP," >&2
    echo "  so SQL/schema regressions — enum-vs-text, CHECK drift, project-scoping —" >&2
    echo "  will NOT be caught. Point PGMCP_TEST_DATABASE_URL at a scratch" >&2
    echo "  Postgres+pgvector DB to make this a full integration check." >&2
    echo
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

run_gate "Gate 1/9: cargo fmt --check" \
    cargo fmt --check
run_gate "Gate 2/9: cargo build --all-targets" \
    cargo build --all-targets
run_gate "Gate 3/9: cargo clippy --all-targets -- -D warnings" \
    cargo clippy --all-targets -- -D warnings
# Build the release CLI binary BEFORE the pgmcp-testing gates: the CLI smoke
# tests (cli_harness) exec `target/release/pgmcp`, so without this build they
# would run a stale artifact. Then run the bin's unit tests.
run_gate "Gate 4/9: cargo build + test --release --bin pgmcp" \
    bash -c "cargo build --release --bin pgmcp && cargo test --release --bin pgmcp"
run_gate "Gate 5/9: cargo test --release -p pgmcp-testing" \
    cargo test --release -p pgmcp-testing
run_gate "Gate 6/9: cargo test --release --test gpu_fallback_smoke -- --ignored" \
    cargo test --release --test gpu_fallback_smoke -- --ignored
run_gate "Gate 7/9: cargo smoke (GPU smoke scenarios)" \
    cargo smoke
# Gate 8: run every `tests/*.rs` across the workspace. Tier-C real-DB tests
# self-skip via `require_test_*!()` when `PGMCP_TEST_DATABASE_URL` is unset,
# so this stays green for contributors without a local Postgres+pgvector
# install; with the env var set, it becomes a full integration check.
run_gate "Gate 8/9: cargo test --release --tests" \
    cargo test --release --tests
# Gate 9: boyscout enforcement (ADR-022) — fail if an open kind='bug' work-item
# is anchored (work_item_code_anchor) to a file touched by the current diff.
# Self-skips LOUDLY (exit 0, logged) outside a git work tree or when the DB is
# unavailable, so it stays green for contributors without the workspace indexed;
# with the daemon's Postgres reachable it is a real gate. The release bin is
# already built by Gate 4, so this reuses it.
run_gate "Gate 9/9: boyscout — no open bugs anchored to changed files" \
    cargo run --release --bin pgmcp -- bug-gate

# P13.5 advisory gates: re-run the formal-verification artefacts so
# that drift between the spec text and the proof / model state is
# caught at verify-time. Skipped (explicitly logged, never silent)
# when the respective tool is not installed — see
# feedback_feature_gated_build_verification.md for the no-silent-skip
# rule.
formal_rocq_dir="docs/formal/rocq"
formal_tla_dir="docs/formal/tla"
tlc_runner="./scripts/tlc-capped.sh"

if command -v coqc >/dev/null 2>&1; then
    if [ -d "${formal_rocq_dir}" ]; then
        # shellcheck disable=SC2044
        for v in $(find "${formal_rocq_dir}" -maxdepth 1 -name '*.v' -print | sort); do
            run_gate "Formal gate: coqc ${v}" \
                coqc "${v}"
        done
    fi
else
    echo "=== Formal gate: SKIP: coqc not found on PATH (Rocq proofs not re-checked) ==="
    echo
fi

if command -v tlc >/dev/null 2>&1; then
    if [ -d "${formal_tla_dir}" ]; then
        # shellcheck disable=SC2044
        for spec in $(find "${formal_tla_dir}" -maxdepth 1 -name '*.tla' -print | sort); do
            # `tlc` resolves the sibling .cfg by basename; cd into the
            # tla dir so relative paths Just Work.
            spec_base=$(basename "${spec}" .tla)
            run_gate "Formal gate: capped tlc ${spec_base}" \
                bash -c "cd ${formal_tla_dir} && ../../../${tlc_runner} ${spec_base}.tla"
        done
    fi
else
    echo "=== Formal gate: SKIP: tlc not found on PATH (TLA+ specs not re-checked) ==="
    echo
fi

echo "verify.sh: all gates passed"
