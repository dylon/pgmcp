//! Queries for client-defined JSON data tables (`src/datatable/`, the v19
//! migration). Plain `sqlx` free functions over `&PgPool`.
//!
//! JSONB idiom (the crate's sqlx build has no `json` feature): a row's `data` is
//! written by binding a JSON **text** string with a `$n::jsonb` cast and read
//! back via `data::text` + `serde_json::from_str` — the same convention as
//! `experiments.rs`. Embeddings are `pgvector::Vector` (1024-d BGE-M3).
//!
//! Dynamic filter / sort SQL is injection-inert by construction: every operand
//! and every patch payload is a **bound parameter**; the only non-bound
//! fragments are (a) comparison casts chosen from the closed [`ColumnType`]
//! enum, (b) the comparison operator / sort direction / combinator chosen from
//! closed [`FilterOp`] / [`SortDir`] / [`Combinator`] enums, and (c) JSON field
//! keys, which are inlined as **single-quoted string literals with `''`
//! escaping** (a string-literal context — never an identifier), so even a
//! malformed key cannot break out. The tool layer additionally validates field
//! names against a charset for clean errors.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use pgvector::Vector;
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::datatable::{ColumnType, Combinator, FilterOp, SortDir};

/// Default cap on rows returned by a single `select`.
pub const MAX_SELECT_ROWS: i64 = 1000;
/// Cap on rows scanned for an in-Rust aggregation pass.
pub const MAX_AGG_SCAN: i64 = 100_000;

// ── Row structs ──────────────────────────────────────────────────────────────

const TABLE_COLS: &str =
    "id, project_id, name, description, schema_mode, created_by, created_at, updated_at";

/// A `data_tables` row (embedding column omitted — reads never need the vector).
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct DataTableRow {
    pub id: i64,
    pub project_id: Option<i32>,
    pub name: String,
    pub description: Option<String>,
    pub schema_mode: String,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A `data_tables` row enriched with child counts (for `data_table_list`).
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct DataTableSummary {
    pub id: i64,
    pub project_id: Option<i32>,
    pub name: String,
    pub description: Option<String>,
    pub schema_mode: String,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub row_count: i64,
    pub column_count: i64,
}

const COLUMN_COLS: &str =
    "id, table_id, name, data_type, required, default_json, position, description";

/// A `data_table_columns` row.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct DataColumnRow {
    pub id: i64,
    pub table_id: i64,
    pub name: String,
    pub data_type: String,
    pub required: bool,
    pub default_json: Option<String>,
    pub position: i32,
    pub description: Option<String>,
}

/// A `data_table_rows` row with its `data` parsed back into a JSON value.
#[derive(Debug, Clone, Serialize)]
pub struct DataRow {
    pub id: i64,
    pub data: Value,
    pub created_by: Option<String>,
    pub source: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct DataRowSql {
    id: i64,
    data_text: String,
    created_by: Option<String>,
    source: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl DataRowSql {
    fn into_row(self) -> DataRow {
        DataRow {
            id: self.id,
            data: serde_json::from_str(&self.data_text).unwrap_or(Value::Null),
            created_by: self.created_by,
            source: self.source,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

// ── Filter / sort representation ─────────────────────────────────────────────

/// One predicate over a JSON field.
#[derive(Debug, Clone)]
pub struct FieldPredicate {
    pub field: String,
    pub op: FilterOp,
    pub value: Value,
}

/// A set of predicates combined with `AND`/`OR`.
#[derive(Debug, Clone)]
pub struct RowFilter {
    pub predicates: Vec<FieldPredicate>,
    pub combinator: Combinator,
}

impl RowFilter {
    /// The empty filter (matches every row).
    pub fn none() -> Self {
        Self {
            predicates: Vec::new(),
            combinator: Combinator::All,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.predicates.is_empty()
    }
}

/// A sort directive over a JSON field (default: newest row id first).
#[derive(Debug, Clone)]
pub struct RowSort {
    pub field: Option<String>,
    pub dir: SortDir,
}

impl Default for RowSort {
    fn default() -> Self {
        Self {
            field: None,
            dir: SortDir::Desc,
        }
    }
}

// ── Table-definition CRUD (metadata only — no dynamic DDL) ───────────────────

pub async fn create_table(
    pool: &PgPool,
    project_id: Option<i32>,
    name: &str,
    description: Option<&str>,
    schema_mode: &str,
    created_by: Option<&str>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO data_tables (project_id, name, description, schema_mode, created_by)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(project_id)
    .bind(name)
    .bind(description)
    .bind(schema_mode)
    .bind(created_by)
    .fetch_one(pool)
    .await
}

pub async fn get_table(pool: &PgPool, id: i64) -> Result<Option<DataTableRow>, sqlx::Error> {
    sqlx::query_as::<_, DataTableRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {TABLE_COLS} FROM data_tables WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Resolve a table by name within a scope. `project_id = None` is the global
/// scope (`project_id IS NULL`); `IS NOT DISTINCT FROM` gives NULL-safe equality.
pub async fn get_table_by_name(
    pool: &PgPool,
    project_id: Option<i32>,
    name: &str,
) -> Result<Option<DataTableRow>, sqlx::Error> {
    sqlx::query_as::<_, DataTableRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {TABLE_COLS} FROM data_tables
         WHERE name = $1 AND project_id IS NOT DISTINCT FROM $2"
    )))
    .bind(name)
    .bind(project_id)
    .fetch_optional(pool)
    .await
}

/// List tables (with child counts), optionally scoped to a project
/// (`project_id = None` lists every table across all scopes).
pub async fn list_tables(
    pool: &PgPool,
    project_id: Option<i32>,
    limit: i64,
) -> Result<Vec<DataTableSummary>, sqlx::Error> {
    sqlx::query_as::<_, DataTableSummary>(
        "SELECT t.id, t.project_id, t.name, t.description, t.schema_mode, t.created_by,
                t.created_at, t.updated_at,
                (SELECT COUNT(*) FROM data_table_rows r WHERE r.table_id = t.id) AS row_count,
                (SELECT COUNT(*) FROM data_table_columns c WHERE c.table_id = t.id) AS column_count
         FROM data_tables t
         WHERE ($1::int IS NULL OR t.project_id = $1)
         ORDER BY t.name
         LIMIT $2",
    )
    .bind(project_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Update the description and/or rename the table (COALESCE — `None` leaves a
/// field unchanged). Bumps `updated_at`.
pub async fn update_table_meta(
    pool: &PgPool,
    id: i64,
    new_name: Option<&str>,
    description: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE data_tables
         SET name = COALESCE($2, name),
             description = COALESCE($3, description),
             updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(new_name)
    .bind(description)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_schema_mode(pool: &PgPool, id: i64, mode: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE data_tables SET schema_mode = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(mode)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_table_embedding(
    pool: &PgPool,
    id: i64,
    embedding: &[f32],
    signature: &str,
) -> Result<(), sqlx::Error> {
    let v = Vector::from(embedding.to_vec());
    sqlx::query(
        "UPDATE data_tables SET embedding = $2, embedding_signature = $3, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(v)
    .bind(signature)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a table; FK CASCADE removes its columns + rows. Returns rows affected
/// on `data_tables` (0 if the id was absent).
pub async fn delete_table(pool: &PgPool, id: i64) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("DELETE FROM data_tables WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

// ── Column-definition CRUD ───────────────────────────────────────────────────

pub async fn list_columns(pool: &PgPool, table_id: i64) -> Result<Vec<DataColumnRow>, sqlx::Error> {
    sqlx::query_as::<_, DataColumnRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMN_COLS} FROM data_table_columns WHERE table_id = $1 ORDER BY position, id"
    )))
    .bind(table_id)
    .fetch_all(pool)
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn add_column(
    pool: &PgPool,
    table_id: i64,
    name: &str,
    data_type: &str,
    required: bool,
    default_json: Option<&str>,
    position: i32,
    description: Option<&str>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO data_table_columns
            (table_id, name, data_type, required, default_json, position, description)
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(table_id)
    .bind(name)
    .bind(data_type)
    .bind(required)
    .bind(default_json)
    .bind(position)
    .bind(description)
    .fetch_one(pool)
    .await
}

pub async fn drop_column(pool: &PgPool, table_id: i64, name: &str) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("DELETE FROM data_table_columns WHERE table_id = $1 AND name = $2")
        .bind(table_id)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

/// Update a column's `required` flag (COALESCE) and/or its default. When
/// `set_default` is true the default is replaced with `default_json` (which may
/// be `None` to clear it); otherwise the default is left unchanged.
pub async fn update_column(
    pool: &PgPool,
    table_id: i64,
    name: &str,
    required: Option<bool>,
    set_default: bool,
    default_json: Option<&str>,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE data_table_columns
         SET required = COALESCE($3, required),
             default_json = CASE WHEN $4 THEN $5 ELSE default_json END
         WHERE table_id = $1 AND name = $2",
    )
    .bind(table_id)
    .bind(name)
    .bind(required)
    .bind(set_default)
    .bind(default_json)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

pub async fn rename_column(
    pool: &PgPool,
    table_id: i64,
    old: &str,
    new: &str,
) -> Result<u64, sqlx::Error> {
    let r =
        sqlx::query("UPDATE data_table_columns SET name = $3 WHERE table_id = $1 AND name = $2")
            .bind(table_id)
            .bind(old)
            .bind(new)
            .execute(pool)
            .await?;
    Ok(r.rows_affected())
}

/// Rename a JSON key inside every row that carries it (so existing rows match a
/// renamed strict-table column). Both keys are bound.
pub async fn rename_row_key(
    pool: &PgPool,
    table_id: i64,
    old: &str,
    new: &str,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE data_table_rows
         SET data = (data - $2) || jsonb_build_object($3, data -> $2), updated_at = now()
         WHERE table_id = $1 AND data ? $2",
    )
    .bind(table_id)
    .bind(old)
    .bind(new)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

// ── Row DML ──────────────────────────────────────────────────────────────────

/// Bulk-insert pre-validated, pre-serialized row objects. One round trip via
/// `UNNEST`; every payload is a bound parameter. Returns the new ids.
pub async fn insert_rows(
    pool: &PgPool,
    table_id: i64,
    rows_json: &[String],
    created_by: Option<&str>,
    source: Option<&str>,
) -> Result<Vec<i64>, sqlx::Error> {
    if rows_json.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO data_table_rows (table_id, data, created_by, source)
         SELECT $1, j::jsonb, $3, $4 FROM UNNEST($2::text[]) AS j
         RETURNING id",
    )
    .bind(table_id)
    .bind(rows_json)
    .bind(created_by)
    .bind(source)
    .fetch_all(pool)
    .await
}

pub async fn get_row(
    pool: &PgPool,
    table_id: i64,
    row_id: i64,
) -> Result<Option<DataRow>, sqlx::Error> {
    let r = sqlx::query_as::<_, DataRowSql>(
        "SELECT id, data::text AS data_text, created_by, source, created_at, updated_at
         FROM data_table_rows WHERE table_id = $1 AND id = $2",
    )
    .bind(table_id)
    .bind(row_id)
    .fetch_optional(pool)
    .await?;
    Ok(r.map(DataRowSql::into_row))
}

pub async fn count_rows(
    pool: &PgPool,
    table_id: i64,
    filter: &RowFilter,
    types: &HashMap<String, ColumnType>,
) -> Result<i64, sqlx::Error> {
    let mut qb =
        QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM data_table_rows WHERE table_id = ");
    qb.push_bind(table_id);
    push_filter(&mut qb, filter, types);
    qb.build_query_scalar::<i64>().fetch_one(pool).await
}

#[allow(clippy::too_many_arguments)]
pub async fn select_rows(
    pool: &PgPool,
    table_id: i64,
    filter: &RowFilter,
    sort: &RowSort,
    limit: i64,
    offset: i64,
    types: &HashMap<String, ColumnType>,
) -> Result<Vec<DataRow>, sqlx::Error> {
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT id, data::text AS data_text, created_by, source, created_at, updated_at
         FROM data_table_rows WHERE table_id = ",
    );
    qb.push_bind(table_id);
    push_filter(&mut qb, filter, types);
    qb.push(" ORDER BY ");
    match &sort.field {
        Some(f) => {
            let fe = esc_lit(f);
            match types.get(f).and_then(|t| t.sql_cast()) {
                Some(cast) => {
                    qb.push(format!(
                        "(data ->> '{fe}')::{cast} {} NULLS LAST",
                        sort.dir.as_sql()
                    ));
                }
                None => {
                    qb.push(format!(
                        "(data ->> '{fe}') {} NULLS LAST",
                        sort.dir.as_sql()
                    ));
                }
            }
            qb.push(", id DESC");
        }
        None => {
            qb.push("id DESC");
        }
    }
    qb.push(" LIMIT ");
    qb.push_bind(limit.clamp(1, MAX_AGG_SCAN));
    qb.push(" OFFSET ");
    qb.push_bind(offset.max(0));
    let rows = qb.build_query_as::<DataRowSql>().fetch_all(pool).await?;
    Ok(rows.into_iter().map(DataRowSql::into_row).collect())
}

/// Shallow-merge a JSON patch into rows matching `filter` (`data = data ||
/// patch`). Returns rows affected.
pub async fn update_rows(
    pool: &PgPool,
    table_id: i64,
    filter: &RowFilter,
    patch_json: &str,
    types: &HashMap<String, ColumnType>,
) -> Result<u64, sqlx::Error> {
    let mut qb = QueryBuilder::<Postgres>::new("UPDATE data_table_rows SET data = data || ");
    qb.push_bind(patch_json.to_string());
    qb.push("::jsonb, updated_at = now() WHERE table_id = ");
    qb.push_bind(table_id);
    push_filter(&mut qb, filter, types);
    let r = qb.build().execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn update_row_by_id(
    pool: &PgPool,
    table_id: i64,
    row_id: i64,
    patch_json: &str,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE data_table_rows SET data = data || $3::jsonb, updated_at = now()
         WHERE table_id = $1 AND id = $2",
    )
    .bind(table_id)
    .bind(row_id)
    .bind(patch_json)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

pub async fn delete_rows(
    pool: &PgPool,
    table_id: i64,
    filter: &RowFilter,
    types: &HashMap<String, ColumnType>,
) -> Result<u64, sqlx::Error> {
    let mut qb = QueryBuilder::<Postgres>::new("DELETE FROM data_table_rows WHERE table_id = ");
    qb.push_bind(table_id);
    push_filter(&mut qb, filter, types);
    let r = qb.build().execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn delete_row_by_id(
    pool: &PgPool,
    table_id: i64,
    row_id: i64,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("DELETE FROM data_table_rows WHERE table_id = $1 AND id = $2")
        .bind(table_id)
        .bind(row_id)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

// ── Semantic table discovery ─────────────────────────────────────────────────

/// Tables ranked by cosine similarity of their name+description embedding to a
/// query embedding (only embedded tables; optional project scope).
pub async fn search_tables(
    pool: &PgPool,
    query_embedding: &[f32],
    project_id: Option<i32>,
    limit: i64,
) -> Result<Vec<(DataTableRow, f64)>, sqlx::Error> {
    let v = Vector::from(query_embedding.to_vec());
    let rows = sqlx::query_as::<
        _,
        (
            i64,
            Option<i32>,
            String,
            Option<String>,
            String,
            Option<String>,
            DateTime<Utc>,
            DateTime<Utc>,
            f64,
        ),
    >(sqlx::AssertSqlSafe(format!(
        "SELECT {TABLE_COLS}, 1.0 - (embedding <=> $1) AS similarity
         FROM data_tables
         WHERE embedding IS NOT NULL AND ($2::int IS NULL OR project_id = $2)
         ORDER BY embedding <=> $1
         LIMIT $3"
    )))
    .bind(v)
    .bind(project_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                project_id,
                name,
                description,
                schema_mode,
                created_by,
                created_at,
                updated_at,
                sim,
            )| {
                (
                    DataTableRow {
                        id,
                        project_id,
                        name,
                        description,
                        schema_mode,
                        created_by,
                        created_at,
                        updated_at,
                    },
                    sim,
                )
            },
        )
        .collect())
}

// ── Safe filter compilation ──────────────────────────────────────────────────

/// Escape a JSON field key for inlining as a single-quoted SQL string literal.
fn esc_lit(s: &str) -> String {
    s.replace('\'', "''")
}

/// `{field: value}` as JSON text (bound for `@>` containment).
fn one_key_json(field: &str, value: &Value) -> String {
    let mut m = serde_json::Map::new();
    m.insert(field.to_string(), value.clone());
    Value::Object(m).to_string()
}

/// Operand value as comparison text (bound; cast applied in SQL).
fn operand_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Escape LIKE wildcards so `contains` is a literal substring match.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Cast for an ordered comparison: the declared column's cast, else numeric when
/// the operand is a number (open tables), else text.
fn ordered_cast(
    field: &str,
    value: &Value,
    types: &HashMap<String, ColumnType>,
) -> Option<&'static str> {
    match types.get(field) {
        Some(t) => t.sql_cast(),
        None if value.is_number() => Some("numeric"),
        None => None,
    }
}

fn push_filter(
    qb: &mut QueryBuilder<Postgres>,
    filter: &RowFilter,
    types: &HashMap<String, ColumnType>,
) {
    if filter.predicates.is_empty() {
        return;
    }
    qb.push(" AND (");
    for (i, p) in filter.predicates.iter().enumerate() {
        if i > 0 {
            qb.push(format!(" {} ", filter.combinator.as_sql()));
        }
        push_predicate(qb, p, types);
    }
    qb.push(")");
}

fn push_predicate(
    qb: &mut QueryBuilder<Postgres>,
    p: &FieldPredicate,
    types: &HashMap<String, ColumnType>,
) {
    let f = esc_lit(&p.field);
    match p.op {
        FilterOp::Eq => {
            qb.push("data @> ");
            qb.push_bind(one_key_json(&p.field, &p.value));
            qb.push("::jsonb");
        }
        FilterOp::Ne => {
            qb.push("NOT (data @> ");
            qb.push_bind(one_key_json(&p.field, &p.value));
            qb.push("::jsonb)");
        }
        FilterOp::Gt | FilterOp::Lt | FilterOp::Gte | FilterOp::Lte => {
            let cmp = p.op.sql_cmp().unwrap_or(">");
            match ordered_cast(&p.field, &p.value, types) {
                Some(cast) => {
                    qb.push(format!("(data ->> '{f}')::{cast} {cmp} "));
                    qb.push_bind(operand_text(&p.value));
                    qb.push(format!("::{cast}"));
                }
                None => {
                    qb.push(format!("(data ->> '{f}') {cmp} "));
                    qb.push_bind(operand_text(&p.value));
                }
            }
        }
        FilterOp::Contains => {
            qb.push(format!("(data ->> '{f}') ILIKE '%' || "));
            qb.push_bind(like_escape(&operand_text(&p.value)));
            qb.push(" || '%'");
        }
        FilterOp::Exists => {
            qb.push(format!("(data ? '{f}')"));
        }
    }
}
