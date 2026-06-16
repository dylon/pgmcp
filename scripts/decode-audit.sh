#!/usr/bin/env bash
# Systematic sqlx-decode audit across all read-only pgmcp tools.
#
# WHY THIS EXISTS
#   sqlx requires each Rust decode type (a `#[derive(FromRow)]` field or a
#   `query_scalar::<_, T>` turbofish) to EXACTLY match the Postgres type of the
#   returned column. A mismatch (NUMERIC -> f64/i64, FLOAT4/real -> f64, INT8 ->
#   i32, divergent UNION arms, ...) fails ONLY at runtime, ONLY when a row is
#   actually returned. The real-DB integration suite is dormant on hosts where
#   the `pgmcp` role lacks CREATEDB (require_test_db! early-returns as "passed"),
#   so this whole class of bug ships green. This script exercises every
#   read-only tool against the LIVE, populated database and flags decode errors.
#
# SAFETY
#   Runs ONLY tools that are read-only (analysis/search/metrics/report). Every
#   mutating or admin tool (create/update/delete/set/*_scan/trigger_cron/reindex/
#   a2a_*/...) is excluded by the MUTATING regex below. Read-only tools issue
#   SELECTs (+ harmless in-memory CLI telemetry; the DB telemetry writer is
#   daemon-only). No data is mutated.
#
# REQUIREMENTS
#   - A release binary (./target/release/pgmcp) built from the current tree.
#   - A populated database (the running daemon's DB). Read-only against it.
#   - The daemon may hold the GPU; tools that embed a query (semantic_search,
#     *_raptor_search, code_summarize, ...) may fall back to CPU or error on the
#     embedder — those failures are NOT decode bugs and are reported separately.
#
# USAGE
#   scripts/decode-audit.sh [PROJECT] [SAMPLE_FILE]
#     PROJECT      a project name to scope project-filtered tools (default: most-indexed)
#     SAMPLE_FILE  a relative path for file-scoped tools (default: auto-picked)
#   Env: PGMCP_BIN (default ./target/release/pgmcp), PGMCP_AUDIT_TIMEOUT (default 45).
#
# OUTPUT
#   A per-tool line (DECODE / ok / err) and a final summary; the full report is
#   written to /var/tmp/decode-audit-report.txt. Exit 0 = no decode bugs.

set -uo pipefail
cd "$(dirname "$0")/.."

BIN="${PGMCP_BIN:-./target/release/pgmcp}"
TIMEOUT="${PGMCP_AUDIT_TIMEOUT:-45}"
REPORT="${PGMCP_AUDIT_REPORT:-/var/tmp/decode-audit-report.txt}"

if [ ! -x "$BIN" ]; then
    echo "decode-audit: $BIN not found/executable; build with 'cargo build --release'" >&2
    exit 2
fi

# --- pick representative args (most tools accept project=/file=/limit=; unknown
#     keys are ignored because no Params struct uses deny_unknown_fields) -------
PROJECT="${1:-}"
if [ -z "$PROJECT" ]; then
    PROJECT=$(psql -tA -h localhost -U postgres -d pgmcp -c \
        "SELECT name FROM projects ORDER BY (SELECT count(*) FROM indexed_files f WHERE f.project_id=projects.id) DESC LIMIT 1;" 2>/dev/null)
fi
SAMPLE_FILE="${2:-}"
if [ -z "$SAMPLE_FILE" ]; then
    SAMPLE_FILE=$(psql -tA -h localhost -U postgres -d pgmcp -c \
        "SELECT relative_path FROM indexed_files WHERE language='rust' ORDER BY id LIMIT 1;" 2>/dev/null)
fi
ARGS="project=${PROJECT} file=${SAMPLE_FILE} path=${SAMPLE_FILE} limit=10"
echo "decode-audit: project='${PROJECT}' file='${SAMPLE_FILE}' timeout=${TIMEOUT}s"

# --- enumerate dispatchable tools, keep only read-only ones -------------------
MUTATING='(_create$|^create_|_create_|_update$|update_|_delete$|delete_|_drop$|_alter$|_insert$|insert_|upsert|_set_status|^set_|_set$|^add_|_add_criterion|memory_add|remove_|_untag$|^tag_create|^tag_merge|^tag_rename|^tag_list|_assign$|^assign|_reparent|reprioritize|^work_item_(claim|release|defer|resolve|triage|reinstate|handoff|bulk|link|unlink|anchor|promote_marker|attempt_verify|record_evidence|record_progress|progress_log|ingest_plan|update|create|set_status|reparent|tag|untag|add_criterion)|_anchor_code|memory_(anchor|unanchor|forget|purge|create|delete|reflect)|ontology_(create_concept|assert_invariant|link)|data_table_(create|insert|update|delete|drop|alter)|plan_(define|definition_import)|experiment_(open|decide|log_artifact|record_measurement|protocol)|promote_session_mandate|coordinate_dependency_block|coordination_respond|^a2a_|agent_heartbeat|trigger_cron|reindex|reflect|correct_query|upsert_pattern_source|refresh_pattern_catalog|toolbox_refresh|security_scan|^fix_|_fix$)'

mapfile -t TOOLS < <(rg -oN '^\s*"([a-z_]+)"\s*=>' -r '$1' src/mcp/server.rs | sort -u | grep -viE "$MUTATING")
echo "decode-audit: ${#TOOLS[@]} read-only tools to sweep"
: > "$REPORT"

SIG='mismatched types|error occurred while decoding column|not compatible with SQL type|FLOAT4|FLOAT8 not compatible'
decode=0; ran=0; errs=0
for t in "${TOOLS[@]}"; do
    out=$(timeout "$TIMEOUT" "$BIN" tool "$t" $ARGS 2>&1)
    if printf '%s\n' "$out" | grep -qiE "$SIG"; then
        detail=$(printf '%s\n' "$out" | grep -oiE 'decoding column "?[^"]*"?: mismatched types[^|]*(NUMERIC|FLOAT4|FLOAT8|INT[0-9]|[A-Z0-9_]+)' | head -1)
        echo "DECODE  $t :: $detail" | tee -a "$REPORT"
        decode=$((decode+1))
    elif printf '%s\n' "$out" | grep -qiE '^Error: |McpError|-32603|invalid type|missing field'; then
        echo "err     $t" >> "$REPORT"; errs=$((errs+1))
    else
        ran=$((ran+1))
    fi
done

# --- crons / REST: not invokable as CLI tools, but they run the same query layer
#     against the live DB. A decode bug there surfaces as a recorded failure in
#     cron_run_history (crons run periodically), so check it empirically. --------
cron_bugs=$(psql -tA -h localhost -U postgres -d pgmcp -c \
  "SELECT count(DISTINCT job_name) FROM cron_run_history
    WHERE error_detail ILIKE '%mismatched types%'
       OR error_detail ILIKE '%not compatible with SQL type%'
       OR error_detail ILIKE '%decoding column%';" 2>/dev/null || echo "?")
echo "decode-audit: cron decode-failures recorded in cron_run_history: ${cron_bugs}"
if [ "${cron_bugs}" != "0" ] && [ "${cron_bugs}" != "?" ]; then
    psql -tA -F' :: ' -h localhost -U postgres -d pgmcp -c \
      "SELECT DISTINCT job_name, left(error_detail,120) FROM cron_run_history
        WHERE error_detail ILIKE '%mismatched types%' OR error_detail ILIKE '%decoding column%'
        ORDER BY 1;" 2>/dev/null | sed 's/^/CRON-DECODE  /' | tee -a "$REPORT"
fi

echo "================================================================"
echo "decode-audit: ${decode} DECODE BUG(S), ${ran} ran-clean, ${errs} other-error (arg/empty/embed)"
echo "full report: $REPORT"
[ "$decode" -eq 0 ]
