//! Curated, read-only relational-table browser for the webui admin console.
//!
//! # What this is
//!
//! Two REST endpoints that let the webui render a *bounded* set of pgmcp's
//! operational tables (work-items, tool-call telemetry, cron history, sessions,
//! mandates, projects, experiments) as sortable / filterable / paginated grids.
//!
//! - `GET /api/db/tables` — the static schema catalog (which tables + columns
//!   the browser exposes, and per-column `sortable`/`filterable` flags).
//! - `GET /api/db/rows`   — one page of rows from a single curated table,
//!   with optional projection, sort, single-column filter, and limit/offset.
//!
//! # What this is NOT (ADR-034)
//!
//! This is deliberately **not** an arbitrary-SQL console. ADR-034 (webui admin
//! console) forbids a general query surface: a compromised or buggy front end
//! must never be able to `SELECT` an embedding/vector column, read another
//! project's secrets, mutate a row, or smuggle SQL through a column name. The
//! defence is a *static allow-list registry* ([`TABLES`]) plus strict
//! validation of every identifier the client supplies:
//!
//! ```text
//!   client query params                 db_browser                     Postgres
//!   ┌──────────────────┐   validate    ┌───────────────────────────┐
//!   │ table            │──────────────▶│ find_table()  → &TableSpec │  (reject → 400)
//!   │ columns (csv)    │──────────────▶│ find_column() per name     │  (reject → 400)
//!   │ sort / dir       │──────────────▶│ registered+sortable / {asc,│  (reject → 400)
//!   │                  │               │   desc}                    │
//!   │ filter_col/op    │──────────────▶│ registered+filterable /    │  (reject → 400)
//!   │                  │               │   {eq,ne,lt,gt,contains}    │
//!   │ filter_val       │══════════════▶│ BOUND as $1 (never interp.)│═════▶ $1
//!   │ limit / offset   │══════════════▶│ BOUND as $2/$3             │═════▶ $2/$3
//!   └──────────────────┘               └───────────────────────────┘
//!            ═══▶ = value path (bound parameter)   ───▶ = identifier path (allow-list)
//! ```
//!
//! Every fragment interpolated into the SQL string is either (a) a table/column
//! name that matched the registry verbatim, (b) an operator chosen by a `match`
//! over a closed set, or (c) a `$n` placeholder. The filter *value*, `limit`,
//! and `offset` are **only ever** bound as sqlx parameters. There is no code
//! path by which a caller-supplied string reaches the query text.
//!
//! # Type-directed, defensive row serialization
//!
//! Rows are fetched with the untyped `sqlx::query` API and each cell is
//! decoded per its registry column type (`text|int|float|bool|timestamp|json`).
//! Postgres integer/float widths vary per column (`int2/int4/int8`,
//! `float4/float8`), and some "text" columns are actually `uuid`/`bpchar`, so
//! each decoder tries the most likely Rust type first and falls back through
//! narrower widths and finally to a `text`/`uuid`/`null` reading. A decode
//! error can therefore never fail the whole request — it degrades to `null`.
//!
//! # CLI / no-pool mode
//!
//! When the [`ApiState`] carries a mock `DbClient` (no real `PgPool`, e.g. the
//! CLI path), `rows` returns a well-formed *empty* page rather than an error,
//! so the front end degrades gracefully. `tables` never needs the database.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::Deserialize;
use serde_json::{Map, Value};
use sqlx::Row;

use super::ApiState;

// ============================================================================
// Static allow-list registry
// ============================================================================

/// One exposed column of a curated table.
///
/// `ty` is the *logical* type used to drive both filtering (comparison /
/// casting) and JSON serialization; it is one of
/// `"text" | "int" | "float" | "bool" | "timestamp" | "json"`. It does not have
/// to match the exact Postgres width — the decoder is width-tolerant.
#[derive(Debug, Clone, Copy)]
pub struct ColumnSpec {
    pub name: &'static str,
    pub ty: &'static str,
    pub sortable: bool,
    pub filterable: bool,
}

/// One curated table: its physical name, a human label for the UI, the exposed
/// column allow-list, the default `ORDER BY` column (must be a registered,
/// sortable column of this table — asserted by a unit test), and the hard cap
/// on page size.
#[derive(Debug, Clone, Copy)]
pub struct TableSpec {
    pub name: &'static str,
    pub label: &'static str,
    pub columns: &'static [ColumnSpec],
    pub default_sort: &'static str,
    pub max_limit: i64,
}

/// Terse `ColumnSpec` constructor so the registry reads like a table.
const fn col(name: &'static str, ty: &'static str, sortable: bool, filterable: bool) -> ColumnSpec {
    ColumnSpec {
        name,
        ty,
        sortable,
        filterable,
    }
}

// Per-table column allow-lists. Columns were chosen from each table's migration
// to be useful in an operational grid AND safe: embedding/vector columns, large
// free-text bodies, and raw sha256 hashes are deliberately omitted. Every table
// exposes `id` (the stable pagination tiebreaker; see `rows`).

/// `work_items` (tracker spine, `src/db/migrations/v4_work_items.rs` +
/// v5 claim lease + v16 assignee). Skips `body`, `parametric_corpus`,
/// `embedding`, `embedding_signature`.
const WORK_ITEMS_COLS: &[ColumnSpec] = &[
    col("id", "int", true, true),
    col("public_id", "text", true, true),
    col("parent_id", "int", true, true),
    col("project_id", "int", true, true),
    col("root_id", "int", true, true),
    col("kind", "text", true, true),
    col("status", "text", true, true),
    col("title", "text", true, true),
    col("priority", "int", true, true),
    col("weight", "float", true, true),
    col("computed_score", "float", true, true),
    col("claimed_percent", "int", true, true),
    col("origin", "text", true, true),
    col("assignee", "text", true, true),
    col("claimed_by", "text", true, true),
    col("created_by", "text", true, true),
    col("created_at", "timestamp", true, true),
    col("updated_at", "timestamp", true, true),
    col("started_at", "timestamp", true, true),
    col("completed_at", "timestamp", true, true),
    col("verified_at", "timestamp", true, true),
    col("due_at", "timestamp", true, true),
];

/// `mcp_tool_calls` (per-call telemetry, `src/db/migrations.rs`). Skips
/// `params_sha256`, `request_id`.
const MCP_TOOL_CALLS_COLS: &[ColumnSpec] = &[
    col("id", "int", true, true),
    col("ts", "timestamp", true, true),
    col("tool", "text", true, true),
    col("client_name", "text", true, true),
    col("client_version", "text", true, true),
    col("protocol_version", "text", true, true),
    col("mcp_session_id", "text", true, true),
    col("project", "text", true, true),
    col("project_id", "int", true, true),
    col("cwd", "text", true, true),
    col("duration_ms", "int", true, true),
    col("outcome", "text", true, true),
    col("error_class", "text", true, true),
    col("result_bytes", "int", true, true),
    col("result_tokens_est", "int", true, true),
];

/// `cron_run_history` (ADR-018 ledger, `src/db/migrations/v40_cron_run_history.rs`).
/// Keeps the operator-relevant deltas; skips the raw start/end RSS+thread pairs.
/// `counters` is JSONB (inert for sort/filter).
const CRON_RUN_HISTORY_COLS: &[ColumnSpec] = &[
    col("id", "int", true, true),
    col("job_name", "text", true, true),
    col("trigger_source", "text", true, true),
    col("outcome", "text", true, true),
    col("skip_reason", "text", true, true),
    col("error_detail", "text", true, true),
    col("project", "text", true, true),
    col("started_at", "timestamp", true, true),
    col("completed_at", "timestamp", true, true),
    col("duration_ms", "int", true, true),
    col("rss_mb_delta", "int", true, true),
    col("threads_delta", "int", true, true),
    col("counters", "json", false, false),
];

/// `sessions` (session observation, `src/db/migrations.rs`). `id` is a `uuid`
/// surfaced as text.
const SESSIONS_COLS: &[ColumnSpec] = &[
    col("id", "text", true, true),
    col("cwd", "text", true, true),
    col("project_id", "int", true, true),
    col("first_seen", "timestamp", true, true),
    col("last_seen", "timestamp", true, true),
];

/// `session_prompts` (deduped prompt log). `prompt_text` is filterable (for
/// `contains` search) but not sortable (avoid heavy sorts on large text);
/// `prompt_sha256` is omitted.
const SESSION_PROMPTS_COLS: &[ColumnSpec] = &[
    col("id", "int", true, true),
    col("session_id", "text", true, true),
    col("ts", "timestamp", true, true),
    col("prompt_text", "text", false, true),
];

/// `durable_mandates` (promoted mandates). Skips any embedding column.
const DURABLE_MANDATES_COLS: &[ColumnSpec] = &[
    col("id", "int", true, true),
    col("scope", "text", true, true),
    col("project_id", "int", true, true),
    col("polarity", "text", true, true),
    col("imperative", "text", true, true),
    col("target", "text", true, true),
    col("source_mandate_id", "int", true, true),
    col("promoted_at", "timestamp", true, true),
    col("file_path", "text", true, true),
];

/// `session_mandates` (per-session extracted directives). `session_id` is a
/// `uuid`; `cue_tier` is `char(1)`; `salience` is `real`.
const SESSION_MANDATES_COLS: &[ColumnSpec] = &[
    col("id", "int", true, true),
    col("session_id", "text", true, true),
    col("source_prompt_id", "int", true, true),
    col("polarity", "text", true, true),
    col("imperative", "text", true, true),
    col("target", "text", true, true),
    col("cwd_prefix", "text", true, true),
    col("cue_tier", "text", true, true),
    col("salience", "float", true, true),
    col("status", "text", true, true),
    col("created_at", "timestamp", true, true),
    col("last_reinforced_at", "timestamp", true, true),
    col("reinforcement_count", "int", true, true),
];

/// `projects` (indexed workspaces, `src/db/migrations.rs`).
const PROJECTS_COLS: &[ColumnSpec] = &[
    col("id", "int", true, true),
    col("workspace_path", "text", true, true),
    col("path", "text", true, true),
    col("name", "text", true, true),
    col("git_common_dir", "text", true, true),
    col("git_root_commits", "text", true, true),
    col("discovered_at", "timestamp", true, true),
    col("last_scanned_at", "timestamp", true, true),
];

/// `experiments` (scientific-experiment subsystem, `src/db/migrations.rs`).
/// `hardware` is JSONB (inert for sort/filter); `question`/`context` are
/// filterable prose but not sortable. Skips `embedding`, `embedding_signature`.
const EXPERIMENTS_COLS: &[ColumnSpec] = &[
    col("id", "int", true, true),
    col("slug", "text", true, true),
    col("title", "text", true, true),
    col("question", "text", false, true),
    col("context", "text", false, true),
    col("kind", "text", true, true),
    col("project_id", "int", true, true),
    col("status", "text", true, true),
    col("hardware", "json", false, false),
    col("git_ref", "text", true, true),
    col("plan_ref", "text", true, true),
    col("correction", "text", true, true),
    col("decided_by", "text", true, true),
    col("observation_id", "int", true, true),
    col("created_at", "timestamp", true, true),
    col("updated_at", "timestamp", true, true),
    col("valid_from", "timestamp", true, true),
    col("valid_to", "timestamp", true, true),
    col("superseded_by", "int", true, true),
];

/// The complete browser allow-list. A table not in this slice cannot be read,
/// full stop (`rows` rejects it with 400 before touching the database).
const TABLES: &[TableSpec] = &[
    TableSpec {
        name: "work_items",
        label: "Work Items",
        columns: WORK_ITEMS_COLS,
        default_sort: "updated_at",
        max_limit: 200,
    },
    TableSpec {
        name: "mcp_tool_calls",
        label: "MCP Tool Calls",
        columns: MCP_TOOL_CALLS_COLS,
        default_sort: "ts",
        max_limit: 200,
    },
    TableSpec {
        name: "cron_run_history",
        label: "Cron Run History",
        columns: CRON_RUN_HISTORY_COLS,
        default_sort: "completed_at",
        max_limit: 200,
    },
    TableSpec {
        name: "sessions",
        label: "Sessions",
        columns: SESSIONS_COLS,
        default_sort: "last_seen",
        max_limit: 200,
    },
    TableSpec {
        name: "session_prompts",
        label: "Session Prompts",
        columns: SESSION_PROMPTS_COLS,
        default_sort: "ts",
        max_limit: 200,
    },
    TableSpec {
        name: "durable_mandates",
        label: "Durable Mandates",
        columns: DURABLE_MANDATES_COLS,
        default_sort: "promoted_at",
        max_limit: 200,
    },
    TableSpec {
        name: "session_mandates",
        label: "Session Mandates",
        columns: SESSION_MANDATES_COLS,
        default_sort: "created_at",
        max_limit: 200,
    },
    TableSpec {
        name: "projects",
        label: "Projects",
        columns: PROJECTS_COLS,
        default_sort: "discovered_at",
        max_limit: 200,
    },
    TableSpec {
        name: "experiments",
        label: "Experiments",
        columns: EXPERIMENTS_COLS,
        default_sort: "created_at",
        max_limit: 200,
    },
];

/// Default page size when the client does not send `limit`.
const DEFAULT_LIMIT: i64 = 50;

// ============================================================================
// Registry lookup + validation helpers
// ============================================================================

/// Resolve a table name against the allow-list. `None` ⇒ not exposed.
fn find_table(name: &str) -> Option<&'static TableSpec> {
    TABLES.iter().find(|t| t.name == name)
}

/// Resolve a column name against a table's allow-list.
fn find_column<'a>(table: &'a TableSpec, name: &str) -> Option<&'a ColumnSpec> {
    table.columns.iter().find(|c| c.name == name)
}

/// Map a public filter-op token to its (fixed) SQL operator. This is the only
/// place operators are produced; the returned `&'static str` is a compile-time
/// constant, never caller data.
fn sql_operator(op: &str) -> Option<&'static str> {
    match op {
        "eq" => Some("="),
        "ne" => Some("<>"),
        "lt" => Some("<"),
        "gt" => Some(">"),
        "contains" => Some("ILIKE"),
        _ => None,
    }
}

/// Postgres cast target for a logical column type, used to coerce the (text)
/// bound parameter to the column's type so `<`/`>` compare numerically /
/// temporally rather than lexically. The cast keyword is a constant chosen by
/// this `match`, never caller data. `text` columns are handled separately (the
/// column, not the param, is cast to text), so this is only reached for the
/// numeric/temporal/bool families.
fn pg_cast_type(ty: &str) -> &'static str {
    match ty {
        "int" => "bigint",
        "float" => "double precision",
        "bool" => "boolean",
        "timestamp" => "timestamptz",
        _ => "text",
    }
}

/// Trim an optional string and collapse blank to `None`.
fn trimmed(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// Pre-validate that a filter value is *parseable* as the column's type, so a
/// malformed value is a clean 400 rather than a Postgres cast error surfacing
/// as a 500. `text` (and `contains`, handled by the caller) impose no format
/// constraint. Returns a human-readable reason on failure.
fn validate_value_format(ty: &str, val: &str) -> Result<(), String> {
    match ty {
        "int" => val
            .parse::<i64>()
            .map(|_| ())
            .map_err(|_| format!("filter_val '{val}' is not an integer")),
        "float" => val
            .parse::<f64>()
            .map(|_| ())
            .map_err(|_| format!("filter_val '{val}' is not a number")),
        "bool" => match val.to_ascii_lowercase().as_str() {
            "true" | "false" | "t" | "f" | "1" | "0" | "yes" | "no" | "on" | "off" => Ok(()),
            _ => Err(format!("filter_val '{val}' is not a boolean")),
        },
        "timestamp" => {
            if DateTime::parse_from_rfc3339(val).is_ok()
                || NaiveDate::parse_from_str(val, "%Y-%m-%d").is_ok()
                || NaiveDateTime::parse_from_str(val, "%Y-%m-%d %H:%M:%S").is_ok()
            {
                Ok(())
            } else {
                Err(format!(
                    "filter_val '{val}' is not an RFC3339 / YYYY-MM-DD timestamp"
                ))
            }
        }
        // "text", "json" (json is non-filterable, never reaches here), and any
        // unknown type impose no format constraint.
        _ => Ok(()),
    }
}

/// A validated single-column filter: the SQL `WHERE` fragment (already bound to
/// `$1`) and the string value to bind for `$1`.
struct Filter {
    /// e.g. `status::text = $1` or `priority > $1::bigint` or
    /// `title::text ILIKE $1`.
    clause: String,
    /// The value to bind for `$1` (already wrapped in `%…%` for `contains`).
    bind: String,
}

/// Build + validate the optional filter from the request. Returns:
/// - `Ok(None)` when no filter fields are present;
/// - `Ok(Some(Filter))` when a complete, valid filter is present;
/// - `Err(400)` when the filter is partial, references an unknown / non-
///   filterable column, uses an unknown operator, or carries a malformed value.
fn build_filter(
    table: &TableSpec,
    params: &RowsParams,
) -> Result<Option<Filter>, (StatusCode, String)> {
    let fcol = trimmed(&params.filter_col);
    let fop = trimmed(&params.filter_op);
    let fval = params.filter_val.as_deref();

    // No filter requested at all.
    if fcol.is_none() && fop.is_none() && fval.is_none() {
        return Ok(None);
    }

    // A partial filter is a client error (all three are required together).
    let fcol = fcol.ok_or((
        StatusCode::BAD_REQUEST,
        "filter_col is required when filtering".to_string(),
    ))?;
    let fop = fop.ok_or((
        StatusCode::BAD_REQUEST,
        "filter_op is required when filtering".to_string(),
    ))?;
    let fval = fval.ok_or((
        StatusCode::BAD_REQUEST,
        "filter_val is required when filtering".to_string(),
    ))?;

    let column = find_column(table, fcol).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("unknown filter column '{fcol}' for table '{}'", table.name),
        )
    })?;
    if !column.filterable {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("column '{fcol}' is not filterable"),
        ));
    }
    let op = sql_operator(fop).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("filter_op must be one of eq|ne|lt|gt|contains, got '{fop}'"),
        )
    })?;

    // `contains` ⇒ ILIKE against the column cast to text, so it works uniformly
    // across text/uuid/numeric/timestamp columns. The value is wrapped `%…%`.
    if op == "ILIKE" {
        return Ok(Some(Filter {
            clause: format!("{}::text ILIKE $1", column.name),
            bind: format!("%{fval}%"),
        }));
    }

    // Text (incl. uuid/char) ⇒ cast the *column* to text and compare to the
    // bound text param. Numeric/temporal/bool ⇒ cast the bound *param* to the
    // column type so ordering comparisons are typed, and pre-validate the value.
    let clause = if column.ty == "text" {
        format!("{}::text {op} $1", column.name)
    } else {
        validate_value_format(column.ty, fval).map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
        format!("{} {op} $1::{}", column.name, pg_cast_type(column.ty))
    };
    Ok(Some(Filter {
        clause,
        bind: fval.to_string(),
    }))
}

// ============================================================================
// GET /api/db/tables — the static schema catalog
// ============================================================================

/// Return the curated table/column catalog the browser exposes. Pure registry
/// read — never touches the database, so it is infallible.
pub async fn tables(State(_state): State<ApiState>) -> Json<Value> {
    let tables: Vec<Value> = TABLES
        .iter()
        .map(|t| {
            let columns: Vec<Value> = t
                .columns
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "name": c.name,
                        "type": c.ty,
                        "sortable": c.sortable,
                        "filterable": c.filterable,
                    })
                })
                .collect();
            serde_json::json!({
                "name": t.name,
                "label": t.label,
                "columns": columns,
                "default_sort": t.default_sort,
                "max_limit": t.max_limit,
            })
        })
        .collect();
    Json(serde_json::json!({ "tables": tables }))
}

// ============================================================================
// GET /api/db/rows — one validated, paginated page of a curated table
// ============================================================================

/// Query parameters for [`rows`]. Every field except `table` is optional;
/// `columns` is a comma-separated projection. Unknown identifiers or operators
/// are rejected with 400 by the handler (see the module-level security notes).
#[derive(Debug, Deserialize)]
pub struct RowsParams {
    pub table: String,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub sort: Option<String>,
    pub dir: Option<String>,
    pub filter_col: Option<String>,
    pub filter_op: Option<String>,
    pub filter_val: Option<String>,
    /// Comma-separated projection; each name must be a registered column.
    pub columns: Option<String>,
}

/// Read one page of a curated table.
///
/// The declared success shape is `Json<Value>`; the handler is typed
/// `Result<Json<Value>, (StatusCode, String)>` because the security contract
/// requires rejecting invalid identifiers with 400 — a bare `Json<Value>`
/// cannot carry a non-200 status. `(StatusCode, String)` is the pervasive
/// fallible-handler idiom in `src/api/handlers.rs`.
pub async fn rows(
    State(state): State<ApiState>,
    Query(params): Query<RowsParams>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // 1. Table must be on the allow-list.
    let table = find_table(&params.table).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("unknown table '{}'", params.table),
        )
    })?;

    // 2. Projection: validate every requested column, else default to all.
    let selected: Vec<&ColumnSpec> = match trimmed(&params.columns) {
        Some(list) => {
            let mut chosen: Vec<&ColumnSpec> = Vec::new();
            for raw in list.split(',') {
                let name = raw.trim();
                if name.is_empty() {
                    continue;
                }
                let column = find_column(table, name).ok_or_else(|| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("unknown column '{name}' for table '{}'", table.name),
                    )
                })?;
                chosen.push(column);
            }
            if chosen.is_empty() {
                table.columns.iter().collect()
            } else {
                chosen
            }
        }
        None => table.columns.iter().collect(),
    };

    // 3. Sort column (registered + sortable) and direction.
    let sort_col = match trimmed(&params.sort) {
        Some(name) => {
            let column = find_column(table, name).ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("unknown sort column '{name}' for table '{}'", table.name),
                )
            })?;
            if !column.sortable {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("column '{name}' is not sortable"),
                ));
            }
            column.name
        }
        None => table.default_sort,
    };
    let dir = match trimmed(&params.dir)
        .map(|d| d.to_ascii_lowercase())
        .as_deref()
    {
        None | Some("desc") => "DESC",
        Some("asc") => "ASC",
        Some(other) => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("dir must be asc or desc, got '{other}'"),
            ));
        }
    };

    // 4. Optional single-column filter.
    let filter = build_filter(table, &params)?;

    // 5. Page bounds. `limit` is clamped to this table's cap; `offset` >= 0.
    let limit = params
        .limit
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, table.max_limit);
    let offset = params.offset.unwrap_or(0).max(0);

    // 6. No real pool (CLI / mock DbClient) ⇒ well-formed empty page.
    let Some(pool) = state.db.pool() else {
        return Ok(Json(build_response(
            table,
            &selected,
            Vec::new(),
            0,
            limit,
            offset,
        )));
    };

    // 7. Build the SQL from validated identifiers only. The filter value binds
    //    to $1 (when present); limit/offset bind to $2/$3 (or $1/$2 without a
    //    filter). A stable `id` tiebreaker makes pagination deterministic when
    //    the sort column has ties (every curated table has an `id` column).
    let projection = selected
        .iter()
        .map(|c| c.name)
        .collect::<Vec<_>>()
        .join(", ");
    let where_sql = match &filter {
        Some(f) => format!(" WHERE {}", f.clause),
        None => String::new(),
    };
    let (limit_ph, offset_ph) = if filter.is_some() {
        ("$2", "$3")
    } else {
        ("$1", "$2")
    };
    let tiebreak = if sort_col == "id" {
        String::new()
    } else {
        format!(", id {dir}")
    };

    let rows_sql = format!(
        "SELECT {projection} FROM {table}{where_sql} \
         ORDER BY {sort_col} {dir}{tiebreak} LIMIT {limit_ph} OFFSET {offset_ph}",
        table = table.name,
    );
    let count_sql = format!(
        "SELECT count(*) FROM {table}{where_sql}",
        table = table.name,
    );

    // 8a. Total (respecting the same filter) for the paginator.
    let total: i64 = {
        let mut query = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(count_sql));
        if let Some(f) = &filter {
            query = query.bind(f.bind.clone());
        }
        query.fetch_one(pool).await.map_err(|e| {
            // ADR-021: a caught DB error logs at error!.
            tracing::error!(table = table.name, error = %e, "db_browser count query failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("count query failed: {e}"),
            )
        })?
    };

    // 8b. The page itself.
    let page = {
        let mut query = sqlx::query(sqlx::AssertSqlSafe(rows_sql));
        if let Some(f) = &filter {
            query = query.bind(f.bind.clone());
        }
        query = query.bind(limit).bind(offset);
        query.fetch_all(pool).await.map_err(|e| {
            tracing::error!(table = table.name, error = %e, "db_browser rows query failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("rows query failed: {e}"),
            )
        })?
    };

    // 9. Serialize each row into a JSON object keyed by column name, decoded per
    //    the registry column type (defensively — a mismatch degrades to null).
    let json_rows: Vec<Value> = page
        .iter()
        .map(|row| {
            let mut object = Map::with_capacity(selected.len());
            for (index, column) in selected.iter().enumerate() {
                object.insert(column.name.to_string(), decode_cell(row, index, column.ty));
            }
            Value::Object(object)
        })
        .collect();

    Ok(Json(build_response(
        table, &selected, json_rows, total, limit, offset,
    )))
}

// ============================================================================
// Response assembly
// ============================================================================

/// The `columns` field of the rows response: the *effective* projection, in
/// order, as `{name, type}` objects (so the front end knows column order + how
/// to render each cell).
fn projection_meta(selected: &[&ColumnSpec]) -> Vec<Value> {
    selected
        .iter()
        .map(|c| serde_json::json!({ "name": c.name, "type": c.ty }))
        .collect()
}

/// Assemble the full `/api/db/rows` response envelope.
fn build_response(
    table: &TableSpec,
    selected: &[&ColumnSpec],
    rows: Vec<Value>,
    total: i64,
    limit: i64,
    offset: i64,
) -> Value {
    serde_json::json!({
        "table": table.name,
        "columns": projection_meta(selected),
        "rows": rows,
        "total": total,
        "limit": limit,
        "offset": offset,
        // The browser reads near-static tables; the realtime cursor the webui
        // uses elsewhere is not meaningful here, so it is reported as 0.
        "server_seq": 0,
    })
}

// ============================================================================
// Defensive, type-directed cell decoding
// ============================================================================

/// Decode one cell to JSON according to its registry type. Every branch falls
/// back to a text/uuid/null reading on a type mismatch, so a single unexpected
/// column type can never fail the request.
fn decode_cell(row: &sqlx::postgres::PgRow, index: usize, ty: &str) -> Value {
    match ty {
        "int" => decode_int(row, index),
        "float" => decode_float(row, index),
        "bool" => decode_bool(row, index),
        "timestamp" => decode_timestamp(row, index),
        "json" => decode_json(row, index),
        // "text" and any unknown type.
        _ => decode_text(row, index),
    }
}

/// `int2`/`int4`/`int8`, tried widest-first. A NULL short-circuits to `null` at
/// the first attempt (sqlx skips the type check for NULL values).
fn decode_int(row: &sqlx::postgres::PgRow, index: usize) -> Value {
    if let Ok(v) = row.try_get::<Option<i64>, _>(index) {
        return v.map(Value::from).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<i32>, _>(index) {
        return v.map(|n| Value::from(n as i64)).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<i16>, _>(index) {
        return v.map(|n| Value::from(n as i64)).unwrap_or(Value::Null);
    }
    decode_text(row, index)
}

/// `float8`/`float4`, tried widest-first. Non-finite values (NaN/±Inf) are not
/// representable in JSON and become `null`.
fn decode_float(row: &sqlx::postgres::PgRow, index: usize) -> Value {
    if let Ok(v) = row.try_get::<Option<f64>, _>(index) {
        return v.map(f64_to_value).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<f32>, _>(index) {
        return v.map(|n| f64_to_value(n as f64)).unwrap_or(Value::Null);
    }
    decode_text(row, index)
}

fn decode_bool(row: &sqlx::postgres::PgRow, index: usize) -> Value {
    if let Ok(v) = row.try_get::<Option<bool>, _>(index) {
        return v.map(Value::Bool).unwrap_or(Value::Null);
    }
    decode_text(row, index)
}

/// `timestamptz` ⇒ RFC 3339 string.
fn decode_timestamp(row: &sqlx::postgres::PgRow, index: usize) -> Value {
    if let Ok(v) = row.try_get::<Option<DateTime<Utc>>, _>(index) {
        return v
            .map(|t| Value::String(t.to_rfc3339()))
            .unwrap_or(Value::Null);
    }
    decode_text(row, index)
}

/// `jsonb`/`json` ⇒ the parsed value verbatim.
fn decode_json(row: &sqlx::postgres::PgRow, index: usize) -> Value {
    if let Ok(v) = row.try_get::<Option<Value>, _>(index) {
        return v.unwrap_or(Value::Null);
    }
    decode_text(row, index)
}

/// Text-like fallback: `text`/`varchar`/`bpchar`, then `uuid` (stringified),
/// else `null`. This is both the `text` decoder and the last-resort fallback
/// for every other decoder.
fn decode_text(row: &sqlx::postgres::PgRow, index: usize) -> Value {
    if let Ok(v) = row.try_get::<Option<String>, _>(index) {
        return v.map(Value::String).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<uuid::Uuid>, _>(index) {
        return v
            .map(|u| Value::String(u.to_string()))
            .unwrap_or(Value::Null);
    }
    Value::Null
}

/// Convert an `f64` to a JSON number, mapping non-finite values to `null`.
fn f64_to_value(n: f64) -> Value {
    serde_json::Number::from_f64(n)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

// ============================================================================
// Tests — registry integrity + validation logic (no database required)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_TYPES: &[&str] = &["text", "int", "float", "bool", "timestamp", "json"];

    #[test]
    fn table_names_are_unique() {
        let mut seen: Vec<&str> = Vec::new();
        for t in TABLES {
            assert!(!seen.contains(&t.name), "duplicate table '{}'", t.name);
            seen.push(t.name);
        }
    }

    #[test]
    fn column_names_are_unique_within_each_table() {
        for t in TABLES {
            let mut seen: Vec<&str> = Vec::new();
            for c in t.columns {
                assert!(
                    !seen.contains(&c.name),
                    "duplicate column '{}' in table '{}'",
                    c.name,
                    t.name
                );
                seen.push(c.name);
            }
        }
    }

    #[test]
    fn all_column_types_are_valid() {
        for t in TABLES {
            for c in t.columns {
                assert!(
                    VALID_TYPES.contains(&c.ty),
                    "table '{}' column '{}' has invalid type '{}'",
                    t.name,
                    c.name,
                    c.ty
                );
            }
        }
    }

    #[test]
    fn every_table_exposes_an_id_column() {
        // The `rows` handler appends `, id <dir>` as a stable pagination
        // tiebreaker; that column must exist in every curated table.
        for t in TABLES {
            assert!(
                find_column(t, "id").is_some(),
                "table '{}' is missing the required `id` column",
                t.name
            );
        }
    }

    #[test]
    fn default_sort_is_a_registered_sortable_column() {
        for t in TABLES {
            let column = find_column(t, t.default_sort).unwrap_or_else(|| {
                panic!(
                    "table '{}' default_sort '{}' is not a registered column",
                    t.name, t.default_sort
                )
            });
            assert!(
                column.sortable,
                "table '{}' default_sort '{}' is not marked sortable",
                t.name, t.default_sort
            );
        }
    }

    #[test]
    fn json_columns_are_neither_sortable_nor_filterable() {
        // We cannot meaningfully ORDER BY / compare a jsonb column in this
        // simple browser, so json columns must be inert for both axes.
        for t in TABLES {
            for c in t.columns {
                if c.ty == "json" {
                    assert!(
                        !c.sortable && !c.filterable,
                        "table '{}' json column '{}' must be non-sortable + non-filterable",
                        t.name,
                        c.name
                    );
                }
            }
        }
    }

    #[test]
    fn max_limit_is_positive_everywhere() {
        for t in TABLES {
            assert!(
                t.max_limit > 0,
                "table '{}' has non-positive max_limit",
                t.name
            );
        }
    }

    #[test]
    fn curated_table_set_is_present() {
        let names: Vec<&str> = TABLES.iter().map(|t| t.name).collect();
        for expected in [
            "work_items",
            "mcp_tool_calls",
            "cron_run_history",
            "sessions",
            "session_prompts",
            "durable_mandates",
            "session_mandates",
            "projects",
            "experiments",
        ] {
            assert!(
                names.contains(&expected),
                "missing curated table '{expected}'"
            );
        }
    }

    #[test]
    fn sensitive_columns_are_not_exposed() {
        // Vector/embedding columns and raw prompt hashes must never be readable.
        for t in TABLES {
            for banned in [
                "embedding",
                "embedding_v2",
                "prompt_sha256",
                "params_sha256",
            ] {
                assert!(
                    find_column(t, banned).is_none(),
                    "table '{}' unexpectedly exposes sensitive column '{}'",
                    t.name,
                    banned
                );
            }
        }
    }

    #[test]
    fn operator_allow_list_is_closed() {
        assert_eq!(sql_operator("eq"), Some("="));
        assert_eq!(sql_operator("ne"), Some("<>"));
        assert_eq!(sql_operator("lt"), Some("<"));
        assert_eq!(sql_operator("gt"), Some(">"));
        assert_eq!(sql_operator("contains"), Some("ILIKE"));
        // Anything outside the closed set (incl. raw SQL) is rejected.
        assert_eq!(sql_operator("="), None);
        assert_eq!(sql_operator("or"), None);
        assert_eq!(sql_operator("; drop table work_items;--"), None);
        assert_eq!(sql_operator(""), None);
    }

    #[test]
    fn cast_types_are_expected() {
        assert_eq!(pg_cast_type("int"), "bigint");
        assert_eq!(pg_cast_type("float"), "double precision");
        assert_eq!(pg_cast_type("bool"), "boolean");
        assert_eq!(pg_cast_type("timestamp"), "timestamptz");
    }

    #[test]
    fn value_format_validation_accepts_and_rejects() {
        assert!(validate_value_format("int", "42").is_ok());
        assert!(validate_value_format("int", "-7").is_ok());
        assert!(validate_value_format("int", "3.5").is_err());
        assert!(validate_value_format("int", "abc").is_err());

        assert!(validate_value_format("float", "3.14").is_ok());
        assert!(validate_value_format("float", "10").is_ok());
        assert!(validate_value_format("float", "nope").is_err());

        assert!(validate_value_format("bool", "true").is_ok());
        assert!(validate_value_format("bool", "FALSE").is_ok());
        assert!(validate_value_format("bool", "1").is_ok());
        assert!(validate_value_format("bool", "maybe").is_err());

        assert!(validate_value_format("timestamp", "2026-01-02").is_ok());
        assert!(validate_value_format("timestamp", "2026-01-02T03:04:05Z").is_ok());
        assert!(validate_value_format("timestamp", "2026-01-02 03:04:05").is_ok());
        assert!(validate_value_format("timestamp", "yesterday").is_err());

        // Text imposes no constraint.
        assert!(validate_value_format("text", "anything at all").is_ok());
    }

    #[test]
    fn find_table_honours_the_allow_list() {
        assert!(find_table("work_items").is_some());
        // Not on the allow-list, even though it exists in the database.
        assert!(find_table("file_chunks").is_none());
        assert!(find_table("pg_catalog.pg_class").is_none());
        assert!(find_table("").is_none());
    }

    #[test]
    fn find_column_honours_the_allow_list() {
        let work_items = find_table("work_items").expect("work_items registered");
        assert!(find_column(work_items, "status").is_some());
        assert!(find_column(work_items, "title").is_some());
        // Deliberately excluded from the projection.
        assert!(find_column(work_items, "body").is_none());
        assert!(find_column(work_items, "embedding").is_none());
    }
}
