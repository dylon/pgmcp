# ADR-010: JSON-based data tables (client-defined observation stores)

**Status:** Accepted
**Date:** 2026-05-29

## Context

pgmcp already ships several *purpose-built* structured stores — the work-item
tracker (`src/tracker/`), the experiments subsystem (`src/experiment/`,
`tool_experiments`), and the memory graph (`src/db/queries/memory_*`). Each has
a fixed schema tailored to its job. What was missing is a **general-purpose,
client-defined data table**: a way for an MCP client (an agent) to spin up an
ad-hoc table at runtime, record rows of observations into it over a
session/project (benchmark numbers, review findings, tracked metrics,
decisions — anything tabular), then query, aggregate, and render that data into
a report.

The request was explicitly: *"create, read, update, and delete data tables to
record observations, etc. These data tables can then be analyzed and formatted
for reports."*

## Decision

Add a `data_table_*` MCP tool family (12 tools) backed by **three fixed
PostgreSQL tables** (v19 migration), a DB-free domain layer (`src/datatable/`),
a query module (`src/db/queries/data_tables.rs`), and a multi-format report
renderer (`src/datatable/report/`). The key choices:

1. **No dynamic DDL.** A user "table" is a *row* in `data_tables`; its optional
   typed schema is rows in `data_table_columns`; its observations are rows in
   `data_table_rows`, each row's fields stored in one JSONB `data` column. A
   `data_table_create` is an `INSERT`; a `data_table_drop` is a `DELETE` + FK
   `CASCADE`. **No tool argument ever reaches SQL as an identifier**, and there
   is no `CREATE TABLE` / `ALTER TABLE` / `DROP TABLE` emitted from a tool call.
   This eliminates the entire DDL-injection class and keeps every user table
   inside the migration-tracked schema.

2. **Hybrid / optional schema.** A table MAY declare typed columns
   (`text | integer | number | boolean | timestamp | json`); declaring any makes
   the table `strict` and rows are validated against the declared types (with
   declared defaults applied for absent fields). Declaring none leaves the table
   `open` — a free-form JSON bucket. `schema_mode` is stored explicitly on
   `data_tables` so the validation path is a column read, not a JOIN-count.

3. **Closed `ColumnType` vocabulary, ADR-003 idiom.** `data_table_columns.data_type`
   is `TEXT` + a `CHECK` built from the closed
   [`crate::datatable::column_type::ColumnType`] enum's `sql_in_list()`, with a
   golden test pinning the set. One source of truth shared by the DB CHECK, the
   row validator, and the type-aware filter/sort/aggregate SQL casts.

4. **All user values are bound parameters.** Filter operands and patch payloads
   bind through `QueryBuilder`; JSON field keys are inlined into SQL only as
   single-quoted **string literals** with `''` escaping (a string-literal
   context, never an identifier). The only non-bound SQL fragments are
   comparison casts (from the closed `ColumnType`), the comparison operator /
   sort direction / combinator (from closed `FilterOp` / `SortDir` /
   `Combinator` enums), and aggregate function names — all finite, audited sets.

5. **Descriptive analysis, computed in Rust.** `data_table_aggregate` groups by
   zero or more fields and computes `count | sum | avg | min | max | stddev |
   median | count_distinct` per group. The aggregation is a *pure, unit-tested*
   Rust function (`crate::datatable::aggregate::compute_aggregation`) over the
   filtered rows the query layer loads (capped at `MAX_AGG_SCAN = 100_000`),
   reusing `crate::stats::inference::median`. Non-coercible values are skipped
   and counted (`n_ignored`). *Inferential* statistics (hypothesis tests, effect
   sizes, acceptance criteria) remain the job of the experiments subsystem; data
   tables deliberately do not duplicate `src/stats` inference.

6. **Seven-format report renderer, reusing `crate::render` primitives.**
   `data_table_report` renders a `TableReport` view-model to `markdown | org |
   latex | html | text` (unicode box-drawing) `| json | csv`, optionally writing
   the result to a file (documented to overwrite). The renderer is a pure
   `fn(&TableReport) -> String` per format (the `src/render` idiom). It reuses
   `crate::render::{glyphs, sparkline}` but defines its own seven-variant
   `DataReportFormat` enum rather than extend `crate::render::ReportFormat` (CSV
   is meaningless for a `QualityReport`).

7. **Embedded for discovery.** Each table's `name + description` is embedded
   (BGE-M3, 1024-d) on write (`data_table_create`/`_alter`, best-effort with the
   embedding-migration cron as the backfill net) so `data_table_search` can rank
   tables by semantic similarity, consistent with work_items / experiments. Row
   *data* is never embedded.

## Rationale

- **Why no dynamic DDL.** Letting a client name a table that becomes a real
  Postgres table would require runtime `CREATE TABLE` with a client-supplied
  identifier — a SQL-injection and operational hazard (schema sprawl, migration
  tracking, lock contention). The JSONB-rows-in-fixed-tables model is the
  document-store pattern; it fits pgmcp's existing heavy JSONB usage
  (`experiments.hardware`, `experiment_runs.command_spec`, `code_topics.top_files`,
  …) and keeps everything inside the v-migration discipline.

- **Why hybrid schema.** Schemaless-only is the simplest but gives no
  validation and weak reports; strict-only is safest but inconvenient for
  ad-hoc capture. Hybrid is strictly more general — it *contains* both as the
  `open` and `strict` modes — and matches the two real use-cases ("just dump
  JSON" and "structured observations").

- **Why in-Rust aggregation.** SQL-pushdown of `AVG`/`STDDEV`/`percentile_cont`
  over JSONB returns Postgres `NUMERIC`, which this crate's sqlx build cannot
  decode (no `bigdecimal`/`rust_decimal` feature), and the per-metric dynamic
  result typing (count→bigint, avg→numeric, min/max→numeric-or-text) is
  bug-prone. Computing over loaded rows in Rust is correct, unit-testable
  without a database, handles type-aware `min`/`max` and `n_ignored` naturally,
  and is appropriate to the data scale (observations, not analytics warehouses).
  The `MAX_AGG_SCAN` cap bounds memory; it is documented, not silent.

## Consequences

- **Schema (v19).** `data_tables`, `data_table_columns`, `data_table_rows` +
  a GIN index on `data_table_rows.data` (for `@>` containment filters) + an
  HNSW index on `data_tables.embedding`. Applies on the next daemon restart.
- **Trust boundary is structural, not conventional.** Unlike the work-item
  tracker there is no privileged state machine — data tables are plain CRUD — so
  there is no actor-gating. The only attribution rule: `created_by` is set
  server-side (`"agent"`), and the caller-supplied `source` is documented as
  untrusted.
- **Guards.** Insert batches are capped (`MAX_INSERT_BATCH = 1000`) with a
  per-row size limit (256 KiB); selects clamp to `MAX_SELECT_ROWS = 1000`;
  unscoped updates/deletes are refused unless `all = true`; dropping a table
  over `DROP_CONFIRM_THRESHOLD = 50` rows requires `confirm = true`.
- **Edge cases.** Adding a column does not backfill existing rows (declared-but-
  missing fields read as `null`; `required`/`default` apply to future writes
  only). Renaming a strict-table column also migrates the JSON key in existing
  rows. Row identity is the surrogate `id` (duplicate observations are valid).

## Alternatives considered

- **Dynamic `CREATE TABLE` per user table** — rejected (injection / operational
  hazard; see Rationale).
- **Schemaless-only** or **strict-typed-only** — rejected in favor of the hybrid
  superset.
- **SQL-pushdown aggregation** — rejected in favor of in-Rust compute (NUMERIC
  decode + dynamic-type result decoding complexity vs. correctness/testability).
- **Extending `crate::render::ReportFormat`** — rejected; CSV has no meaning for
  a `QualityReport`, so the data-table renderer owns its own format enum and
  reuses only the shared `glyphs`/`sparkline` primitives.

## References

- Implementation: `src/datatable/` (domain + renderer), `src/db/queries/data_tables.rs`,
  `src/db/migrations/v19_data_tables.rs`, `src/mcp/params/data_tables.rs`,
  `src/mcp/tools/data_tables/`, `src/mcp/server/handlers/data_tables.rs`.
- Embedding backfill: `src/cron/embedding_migration.rs` (`migrate_data_tables_batch`).
- Idiom precedents: ADR-003 (closed-vocabulary TEXT+CHECK), `src/db/queries/experiments.rs`
  (JSONB-as-text), `src/render/` (multi-format pure renderers).
