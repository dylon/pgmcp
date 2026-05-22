#!/usr/bin/env bash
# Harness for the recovery-times scientific ledger.
#
# Emits markdown table rows to stdout in the schema documented in
# docs/scientific-ledger/recovery-times-2026-04-28.md. The raw stdout
# of each tool invocation is captured to a sibling .raw.log file so
# the rows are reproducible later.
#
# Five scenarios; each runs the corresponding command, parses elapsed
# time, and prints one or more rows. Some scenarios require sudo for
# the drop_caches step — the script self-skips those rows with a
# diagnostic line if `sudo -n` is unavailable.

set -euo pipefail

DATE=$(date -u +%Y-%m-%d)
HW_FILE="/home/dylon/.claude/hardware-specifications.md"
LEDGER="$(dirname "$0")/../docs/scientific-ledger/recovery-times-2026-04-28.md"
RAW_LOG="$(dirname "$0")/../docs/scientific-ledger/recovery-times-2026-04-28.raw.log"

hardware_notes() {
    if [ -f "$HW_FILE" ]; then
        # First H1 in the hardware file is typically "CPU / RAM / GPU /
        # kernel". Compress to a single line.
        awk '/^# /{print substr($0,3); exit}' "$HW_FILE"
    else
        echo "host: $(uname -n) ($(uname -r))"
    fi
}

require_workspace_count() {
    # Workspace file count is reported by `pgmcp stats`. Falls back to
    # the indexed_files row count via psql if `pgmcp stats` isn't
    # available locally.
    if command -v pgmcp >/dev/null 2>&1; then
        pgmcp stats 2>/dev/null | awk '/Indexed files/ {print $NF; exit}'
    elif [ -n "${PGMCP_TEST_DATABASE_URL:-}" ]; then
        psql "$PGMCP_TEST_DATABASE_URL" -tAc \
            'SELECT COUNT(*) FROM indexed_files' 2>/dev/null
    else
        echo "0"
    fi
}

emit_row() {
    local scenario="$1"
    local n_files="$2"
    local observed="$3"
    local notes="$4"
    local hw
    hw=$(hardware_notes)
    printf "| %s | %-26s | %-20s | %-22s | %-30s | %s |\n" \
        "$DATE" "$scenario" "$n_files" "$observed" "$hw" "$notes"
}

run_cold_warm_daemon_ready() {
    if ! command -v systemctl >/dev/null 2>&1; then
        echo "# (skipped) systemctl not available; cannot measure daemon-ready" >&2
        return
    fi
    local files
    files=$(require_workspace_count)

    if sudo -n true 2>/dev/null; then
        local start end ready
        sudo -n systemctl stop pgmcp 2>/dev/null || true
        sync
        echo 3 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null
        start=$(date +%s.%N)
        sudo -n systemctl start pgmcp
        until [ "$(curl -s -o /dev/null -w '%{http_code}' http://localhost:3100/health 2>/dev/null || echo 0)" = "200" ]; do
            sleep 0.1
        done
        end=$(date +%s.%N)
        ready=$(awk "BEGIN{printf \"%.2f\", $end - $start}")
        emit_row "cold-daemon-ready" "$files" "${ready}s" \
            "drop_caches + systemctl start; curl-poll /health to 200"
    else
        echo "# (skipped) sudo -n unavailable; cannot drop_caches for cold measurement" >&2
    fi

    if command -v systemctl >/dev/null 2>&1 \
       && [ "$(curl -s -o /dev/null -w '%{http_code}' http://localhost:3100/health 2>/dev/null || echo 0)" = "200" ]; then
        # warm: restart-only, no cache flush
        local start end ready
        sudo -n systemctl stop pgmcp 2>/dev/null || true
        start=$(date +%s.%N)
        sudo -n systemctl start pgmcp 2>/dev/null
        until [ "$(curl -s -o /dev/null -w '%{http_code}' http://localhost:3100/health 2>/dev/null || echo 0)" = "200" ]; do
            sleep 0.1
        done
        end=$(date +%s.%N)
        ready=$(awk "BEGIN{printf \"%.2f\", $end - $start}")
        emit_row "warm-daemon-ready" "$files" "${ready}s" \
            "systemctl restart; pages cached from prior run"
    fi
}

run_embed_pool_warmup() {
    # Greps the structured log for the `embed_pool_warmup` span that
    # the embed-pool worker stamps on first successful query (added
    # alongside this script as part of the C.10 baseline work).
    local log="${HOME}/.local/share/pgmcp/pgmcp.log"
    if [ ! -f "$log" ]; then
        echo "# (skipped) ${log} not present; daemon hasn't run as a user service" >&2
        return
    fi
    local elapsed
    elapsed=$(grep -m 1 'phase="ready"' "$log" 2>/dev/null \
        | awk 'match($0, /elapsed=([0-9.]+)/, m) {print m[1]; exit}')
    if [ -n "$elapsed" ]; then
        emit_row "embed-pool-warmup" "n/a" "${elapsed}s" \
            "first successful embed_query after Initializing"
    else
        echo "# (skipped) no embed_pool_warmup span in $log" >&2
    fi
}

run_health_probe_latency() {
    if ! command -v curl >/dev/null 2>&1; then
        return
    fi
    if [ "$(curl -s -o /dev/null -w '%{http_code}' http://localhost:3100/health 2>/dev/null || echo 0)" != "200" ]; then
        echo "# (skipped) daemon not ready on :3100; cannot probe /health" >&2
        return
    fi
    local i ms_list=() ms p50 p99 total=100
    for i in $(seq 1 $total); do
        ms=$(curl -s -o /dev/null -w '%{time_total}' http://localhost:3100/health)
        ms_list+=("$(awk "BEGIN{printf \"%.3f\", $ms * 1000}")")
    done
    # p50, p99 (printf-stable sort)
    p50=$(printf '%s\n' "${ms_list[@]}" | sort -n | awk -v n=$total 'NR == int(n/2){print; exit}')
    p99=$(printf '%s\n' "${ms_list[@]}" | sort -n | awk -v n=$total 'NR == int(n*0.99){print; exit}')
    emit_row "health-probe-latency" "n/a" "p50=${p50}ms p99=${p99}ms" \
        "curl /health x 100 reps"
}

run_reindex_throughput() {
    if ! command -v pgmcp >/dev/null 2>&1; then
        echo "# (skipped) pgmcp binary not on PATH" >&2
        return
    fi
    local start end elapsed files_before files_after rate
    files_before=$(require_workspace_count)
    start=$(date +%s.%N)
    pgmcp reindex >/dev/null 2>&1
    end=$(date +%s.%N)
    elapsed=$(awk "BEGIN{printf \"%.2f\", $end - $start}")
    files_after=$(require_workspace_count)
    rate=$(awk "BEGIN{if ($elapsed > 0) printf \"%.1f\", $files_after / $elapsed; else print \"n/a\"}")
    emit_row "warm-reindex-throughput" "$files_after" "${rate} files/s" \
        "pgmcp reindex; elapsed ${elapsed}s"
}

run_tool_call_latency() {
    # Criterion benchmark; runs in-process so it does not require a
    # daemon. The bench writes its own machine-readable JSON under
    # target/criterion/; we summarise the median for the canonical row.
    if ! cargo --version >/dev/null 2>&1; then
        return
    fi
    local out
    out=$(cargo bench --bench mcp_tool_latency -- --noplot 2>&1 | tail -200)
    echo "$out" >> "$RAW_LOG"
    local median
    median=$(echo "$out" | awk '/time:/ && /\[/ {print $2 " " $3; exit}')
    if [ -n "$median" ]; then
        emit_row "tool-call-latency" "n/a" "$median" \
            "criterion bench, in-process dispatcher overhead"
    fi
}

echo "# recovery-times harness run at $DATE" > "$RAW_LOG"
echo "# host: $(hardware_notes)" >> "$RAW_LOG"
echo

run_cold_warm_daemon_ready
run_embed_pool_warmup
run_reindex_throughput
run_health_probe_latency
run_tool_call_latency

echo
echo "# raw stdout captured in $RAW_LOG"
echo "# ledger: $LEDGER"
