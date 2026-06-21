//! Query layer for the work-item / plan tracker
//! (`crate::db::migrations::v4_work_items`).
//!
//! Plain `sqlx` free functions over `&PgPool`, following the freshest sibling
//! idiom (`crate::db::queries::experiments`) rather than the `DbClient` trait:
//! the tracker tool surface is large and these are called directly from tool
//! bodies via the CLI/MCP pool. Embeddings are `Option<pgvector::Vector>`
//! (1024-d BGE-M3); JSONB is not used on the Phase-1 tables.
//!
//! The single trust-critical entry point is [`set_work_item_status`], which
//! runs [`crate::tracker::transition::check_transition`] before any status
//! `UPDATE` and writes the append-only `work_item_status_history` row in the
//! same transaction — so a `→verified` without passing evidence, or a
//! `→deferred` without a user negotiation, cannot be persisted.

use chrono::{DateTime, Utc};
use pgvector::Vector;
use sqlx::{PgPool, Postgres, Transaction};

use crate::tracker::status::WorkItemStatus;
use crate::tracker::transition::{Actor, TransitionContext, TransitionError, check_transition};

/// A work-item row (embedding column omitted — reads never need the vector).
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct WorkItemRow {
    pub id: i64,
    pub public_id: String,
    pub parent_id: Option<i64>,
    pub project_id: Option<i32>,
    pub definition_id: Option<i64>,
    pub root_id: Option<i64>,
    pub kind: String,
    pub status: String,
    pub title: String,
    pub body: Option<String>,
    pub parametric: bool,
    pub parametric_corpus: Option<String>,
    pub parametric_expected: Option<i32>,
    pub priority: i32,
    pub weight: f32,
    pub computed_score: Option<f64>,
    pub claimed_percent: i16,
    pub origin: String,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub verified_at: Option<DateTime<Utc>>,
    pub due_at: Option<DateTime<Utc>>,
    pub snooze_until: Option<DateTime<Utc>>,
    /// Bug impact axis (v12 bug-tracker); NULL for non-bug kinds. See
    /// [`crate::tracker::severity::Severity`].
    #[sqlx(default)]
    pub severity: Option<String>,
    // Claim/lease state (v5 collaboration layer; NULL until claimed).
    #[sqlx(default)]
    pub claimed_by: Option<String>,
    #[sqlx(default)]
    pub claimed_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    pub lease_expires_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    pub claim_count: i32,
    /// Durable ownership intent (v16); distinct from the ephemeral `claimed_by`
    /// lease — set via `work_item_assign`, never auto-cleared. `my-work` filters
    /// on it.
    #[sqlx(default)]
    pub assignee: Option<String>,
    #[sqlx(default)]
    pub assigned_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    pub assigned_by: Option<String>,
}

/// Explicit column list (no `embedding`) shared by every `SELECT`.
const WORK_ITEM_COLS: &str = "id, public_id, parent_id, project_id, definition_id, root_id, \
     kind, status, title, body, parametric, parametric_corpus, parametric_expected, \
     priority, weight, computed_score, claimed_percent, origin, created_by, \
     created_at, updated_at, started_at, completed_at, verified_at, due_at, snooze_until, \
     severity, claimed_by, claimed_at, lease_expires_at, claim_count, \
     assignee, assigned_at, assigned_by";

/// One row of the append-only transition audit.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct StatusHistoryRow {
    pub id: i64,
    pub item_id: i64,
    pub from_status: Option<String>,
    pub to_status: String,
    pub actor_kind: String,
    pub actor_id: Option<String>,
    pub evidence_id: Option<i64>,
    pub negotiation_id: Option<i64>,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Fallible tracker op: a DB error, a refused transition, or a missing item.
#[derive(Debug)]
pub enum WorkItemOpError {
    Db(sqlx::Error),
    Transition(TransitionError),
    NotFound,
}

impl std::fmt::Display for WorkItemOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "database error: {e}"),
            Self::Transition(t) => write!(f, "{}", t.message()),
            Self::NotFound => write!(f, "work item not found"),
        }
    }
}

impl std::error::Error for WorkItemOpError {}

impl From<sqlx::Error> for WorkItemOpError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// Parameters for [`insert_work_item`]. Grouped to dodge the
/// `too_many_arguments` clippy lint and keep call sites readable. A manual
/// `Default` (below) supplies safe values (`status='pending'`, `weight=1.0`,
/// `kind='task'`, `origin='user_explicit'`) so `..Default::default()` never
/// yields a CHECK-violating empty status.
#[derive(Debug)]
pub struct NewWorkItem<'a> {
    pub public_id: &'a str,
    pub parent_id: Option<i64>,
    pub project_id: Option<i32>,
    pub definition_id: Option<i64>,
    pub kind: &'a str,
    /// Initial status (defaults to `pending`); ingestion may seed e.g.
    /// `claimed_done` for a checked checklist item.
    pub status: &'a str,
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub priority: i32,
    pub weight: f32,
    pub parametric: bool,
    pub parametric_corpus: Option<&'a str>,
    pub parametric_expected: Option<i32>,
    pub origin: &'a str,
    pub created_by: Option<&'a str>,
    /// Bug impact axis (v12); NULL for non-bug kinds.
    pub severity: Option<&'a str>,
    pub embedding: Option<Vector>,
}

impl<'a> Default for NewWorkItem<'a> {
    fn default() -> Self {
        Self {
            public_id: "",
            parent_id: None,
            project_id: None,
            definition_id: None,
            kind: "task",
            status: "pending",
            title: "",
            body: None,
            priority: 0,
            weight: 1.0,
            parametric: false,
            parametric_corpus: None,
            parametric_expected: None,
            origin: "user_explicit",
            created_by: None,
            severity: None,
            embedding: None,
        }
    }
}

/// Insert a work item, computing `root_id` from the parent (a root keeps
/// `root_id = NULL`, meaning "self"; a child inherits `COALESCE(parent.root_id,
/// parent.id)`). Returns the new id. The caller supplies a stable `public_id`.
pub async fn insert_work_item(pool: &PgPool, item: NewWorkItem<'_>) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let new_id = insert_work_item_in_tx(&mut tx, &item).await?;
    tx.commit().await?;
    Ok(new_id)
}

/// Transactional variant used when callers must commit the spine row together
/// with sidecar rows. If a parent is supplied, lock it while deriving `root_id`
/// so a concurrent reparent/delete cannot race the child insert.
pub async fn insert_work_item_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    item: &NewWorkItem<'_>,
) -> Result<i64, sqlx::Error> {
    let root_id = match item.parent_id {
        None => None,
        Some(parent_id) => Some(
            sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(root_id, id) FROM work_items WHERE id = $1 FOR SHARE",
            )
            .bind(parent_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?,
        ),
    };

    sqlx::query_scalar::<_, i64>(
        "INSERT INTO work_items
            (public_id, parent_id, project_id, definition_id, root_id, kind, status,
             title, body, priority, weight, parametric, parametric_corpus,
             parametric_expected, origin, created_by, embedding, severity)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
             $14, $15, $16, $17, $18)
         RETURNING id",
    )
    .bind(item.public_id)
    .bind(item.parent_id)
    .bind(item.project_id)
    .bind(item.definition_id)
    .bind(root_id)
    .bind(item.kind)
    .bind(item.status)
    .bind(item.title)
    .bind(item.body)
    .bind(item.priority)
    .bind(item.weight)
    .bind(item.parametric)
    .bind(item.parametric_corpus)
    .bind(item.parametric_expected)
    .bind(item.origin)
    .bind(item.created_by)
    .bind(item.embedding.clone())
    .bind(item.severity)
    .fetch_one(&mut **tx)
    .await
}

/// Fetch one item by numeric id.
pub async fn get_work_item(pool: &PgPool, id: i64) -> Result<Option<WorkItemRow>, sqlx::Error> {
    sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {WORK_ITEM_COLS} FROM work_items WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Fetch one item by its stable `public_id`.
pub async fn get_work_item_by_public_id(
    pool: &PgPool,
    public_id: &str,
) -> Result<Option<WorkItemRow>, sqlx::Error> {
    sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {WORK_ITEM_COLS} FROM work_items WHERE public_id = $1"
    )))
    .bind(public_id)
    .fetch_optional(pool)
    .await
}

/// Filters for [`list_work_items`]. `None` fields are unconstrained.
#[derive(Debug, Default)]
pub struct WorkItemFilter<'a> {
    pub project_id: Option<i32>,
    pub kind: Option<&'a str>,
    pub status: Option<&'a str>,
    pub parent_id: Option<i64>,
    /// When true, restrict to overdue items (due_at in the past, not yet
    /// done/cancelled/deferred).
    pub overdue: bool,
    /// When false (default), hide currently-snoozed items (snooze_until in the
    /// future).
    pub include_snoozed: bool,
    /// Restrict to items owned by this assignee (the `my-work` view).
    pub assignee: Option<&'a str>,
    /// Restrict to bugs awaiting triage (`kind='bug' AND status='triage'`).
    pub needs_triage: bool,
    /// Restrict to workable-now items (actionable status AND no unresolved
    /// blocker) — the `next-actionable` view.
    pub next_actionable: bool,
    pub limit: i64,
}

/// List items matching the filter, newest first, capped by `limit`.
pub async fn list_work_items(
    pool: &PgPool,
    f: &WorkItemFilter<'_>,
) -> Result<Vec<WorkItemRow>, sqlx::Error> {
    let limit = f.limit.clamp(1, 1000);
    sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {WORK_ITEM_COLS} FROM work_items w
         WHERE ($1::int IS NULL OR project_id = $1)
           AND ($2::text IS NULL OR kind = $2)
           AND ($3::text IS NULL OR status = $3)
           AND ($4::bigint IS NULL OR parent_id = $4)
           AND ($6::bool IS NOT TRUE OR (
                 due_at IS NOT NULL AND due_at < NOW()
                 AND status NOT IN ('verified','cancelled','deferred')))
           AND ($7::bool IS TRUE OR snooze_until IS NULL OR snooze_until <= NOW())
           AND ($8::text IS NULL OR assignee = $8)
           AND ($9::bool IS NOT TRUE OR (kind = 'bug' AND status = 'triage'))
           AND ($10::bool IS NOT TRUE OR (status IN ('pending','confirmed','ready') AND {NOT_BLOCKED}))
         ORDER BY priority DESC, computed_score DESC NULLS LAST, created_at DESC
         LIMIT $5"
    )))
    .bind(f.project_id)
    .bind(f.kind)
    .bind(f.status)
    .bind(f.parent_id)
    .bind(limit)
    .bind(f.overdue)
    .bind(f.include_snoozed)
    .bind(f.assignee)
    .bind(f.needs_triage)
    .bind(f.next_actionable)
    .fetch_all(pool)
    .await
}

/// Update mutable non-status fields. Each `Some` overwrites; `None` keeps the
/// existing value (COALESCE). Returns the updated row, or `NotFound`.
/// Update mutable fields. The schedule fields (`due_at`/`snooze_until`) carry
/// three-way semantics: `clear_*=true` sets the column NULL; otherwise the
/// value is COALESCE-set (None = leave unchanged). This lets a caller set,
/// clear, or leave a due date / snooze independently.
#[allow(clippy::too_many_arguments)]
pub async fn update_work_item_fields(
    pool: &PgPool,
    id: i64,
    title: Option<&str>,
    body: Option<&str>,
    priority: Option<i32>,
    weight: Option<f32>,
    due_at: Option<DateTime<Utc>>,
    clear_due: bool,
    snooze_until: Option<DateTime<Utc>>,
    clear_snooze: bool,
    severity: Option<&str>,
) -> Result<WorkItemRow, WorkItemOpError> {
    let mut tx = pool.begin().await?;
    let res = update_work_item_fields_in_tx(
        &mut tx,
        id,
        title,
        body,
        priority,
        weight,
        due_at,
        clear_due,
        snooze_until,
        clear_snooze,
        severity,
    )
    .await;
    match res {
        Ok(row) => {
            tx.commit().await?;
            Ok(row)
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// Transactional variant of [`update_work_item_fields`].
#[allow(clippy::too_many_arguments)]
pub async fn update_work_item_fields_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    title: Option<&str>,
    body: Option<&str>,
    priority: Option<i32>,
    weight: Option<f32>,
    due_at: Option<DateTime<Utc>>,
    clear_due: bool,
    snooze_until: Option<DateTime<Utc>>,
    clear_snooze: bool,
    severity: Option<&str>,
) -> Result<WorkItemRow, WorkItemOpError> {
    let row = sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "UPDATE work_items SET
            title = COALESCE($2, title),
            body = COALESCE($3, body),
            priority = COALESCE($4, priority),
            weight = COALESCE($5, weight),
            due_at = CASE WHEN $7 THEN NULL ELSE COALESCE($6, due_at) END,
            snooze_until = CASE WHEN $9 THEN NULL ELSE COALESCE($8, snooze_until) END,
            severity = COALESCE($10, severity),
            updated_at = NOW()
         WHERE id = $1
         RETURNING {WORK_ITEM_COLS}"
    )))
    .bind(id)
    .bind(title)
    .bind(body)
    .bind(priority)
    .bind(weight)
    .bind(due_at)
    .bind(clear_due)
    .bind(snooze_until)
    .bind(clear_snooze)
    .bind(severity)
    .fetch_optional(&mut **tx)
    .await?;
    row.ok_or(WorkItemOpError::NotFound)
}

/// The 1:1 bug-detail sidecar (v12 bug-tracker) for `kind='bug'` items.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct BugDetailsRow {
    pub item_id: i64,
    pub reproduction_steps: Option<String>,
    pub expected_behavior: Option<String>,
    pub actual_behavior: Option<String>,
    pub environment: Option<String>,
    pub affected_version: Option<String>,
    pub fixed_in_version: Option<String>,
    pub root_cause: Option<String>,
    pub is_regression: bool,
    pub reported_by: Option<String>,
    pub reported_at: DateTime<Utc>,
    pub triaged_by: Option<String>,
    pub triaged_at: Option<DateTime<Utc>>,
    pub resolution: Option<String>,
}

const BUG_DETAIL_COLS: &str = "item_id, reproduction_steps, expected_behavior, actual_behavior, \
     environment, affected_version, fixed_in_version, root_cause, is_regression, reported_by, \
     reported_at, triaged_by, triaged_at, resolution";

/// Input for [`upsert_bug_details`] — COALESCE semantics (a `None` field leaves
/// the stored value unchanged). `set_triaged_at` stamps `triaged_at = NOW()`
/// (the triage milestone). The caller validates `severity`/`resolution`
/// vocabularies (`Severity::parse` / `BugResolution::parse`) before persisting.
#[derive(Debug, Default)]
pub struct BugDetailFields<'a> {
    pub reproduction_steps: Option<&'a str>,
    pub expected_behavior: Option<&'a str>,
    pub actual_behavior: Option<&'a str>,
    pub environment: Option<&'a str>,
    pub affected_version: Option<&'a str>,
    pub fixed_in_version: Option<&'a str>,
    pub root_cause: Option<&'a str>,
    pub is_regression: Option<bool>,
    pub reported_by: Option<&'a str>,
    pub triaged_by: Option<&'a str>,
    pub set_triaged_at: bool,
    pub resolution: Option<&'a str>,
}

impl BugDetailFields<'_> {
    /// True when no field carries a value — lets create/update skip touching the
    /// sidecar for a non-bug item that supplied no bug fields.
    pub fn is_empty(&self) -> bool {
        self.reproduction_steps.is_none()
            && self.expected_behavior.is_none()
            && self.actual_behavior.is_none()
            && self.environment.is_none()
            && self.affected_version.is_none()
            && self.fixed_in_version.is_none()
            && self.root_cause.is_none()
            && self.is_regression.is_none()
            && self.reported_by.is_none()
            && self.triaged_by.is_none()
            && !self.set_triaged_at
            && self.resolution.is_none()
    }
}

/// Insert-or-update the 1:1 bug-detail sidecar for an item. On conflict (the
/// row already exists) each supplied field COALESCE-overwrites and the rest are
/// left intact; `reported_at`/`created_at` keep their original values and
/// `updated_at` is refreshed.
pub async fn upsert_bug_details(
    pool: &PgPool,
    item_id: i64,
    f: &BugDetailFields<'_>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    upsert_bug_details_in_tx(&mut tx, item_id, f).await?;
    tx.commit().await?;
    Ok(())
}

/// Freeze a bug's machine-checkable reproduction criterion (v44 columns) for
/// `work_item_assert_fixed` (ADR-023). `criterion_locked_at` is set once
/// (anti-tamper, like an experiment hypothesis's locked criterion), so an agent
/// cannot quietly rewrite the bar after asserting a fix. Creates the sidecar row
/// if absent. Idempotent on the lock timestamp.
pub async fn freeze_bug_criterion(
    pool: &PgPool,
    item_id: i64,
    verification_command: &str,
    expected_signal: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO work_item_bug_details
            (item_id, verification_command, expected_signal, criterion_locked_at)
         VALUES ($1, $2, $3, NOW())
         ON CONFLICT (item_id) DO UPDATE SET
            verification_command = EXCLUDED.verification_command,
            expected_signal = COALESCE(EXCLUDED.expected_signal, work_item_bug_details.expected_signal),
            criterion_locked_at = COALESCE(work_item_bug_details.criterion_locked_at, NOW()),
            updated_at = NOW()",
    )
    .bind(item_id)
    .bind(verification_command)
    .bind(expected_signal)
    .execute(pool)
    .await?;
    Ok(())
}

/// Transactional variant of [`upsert_bug_details`].
pub async fn upsert_bug_details_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    item_id: i64,
    f: &BugDetailFields<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO work_item_bug_details
            (item_id, reproduction_steps, expected_behavior, actual_behavior, environment,
             affected_version, fixed_in_version, root_cause, is_regression, reported_by,
             triaged_by, triaged_at, resolution)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, COALESCE($9, FALSE), $10, $11,
             CASE WHEN $12 THEN NOW() ELSE NULL END, $13)
         ON CONFLICT (item_id) DO UPDATE SET
            reproduction_steps = COALESCE($2, work_item_bug_details.reproduction_steps),
            expected_behavior  = COALESCE($3, work_item_bug_details.expected_behavior),
            actual_behavior    = COALESCE($4, work_item_bug_details.actual_behavior),
            environment        = COALESCE($5, work_item_bug_details.environment),
            affected_version   = COALESCE($6, work_item_bug_details.affected_version),
            fixed_in_version   = COALESCE($7, work_item_bug_details.fixed_in_version),
            root_cause         = COALESCE($8, work_item_bug_details.root_cause),
            is_regression      = COALESCE($9, work_item_bug_details.is_regression),
            reported_by        = COALESCE($10, work_item_bug_details.reported_by),
            triaged_by         = COALESCE($11, work_item_bug_details.triaged_by),
            triaged_at         = CASE WHEN $12 THEN NOW() ELSE work_item_bug_details.triaged_at END,
            resolution         = COALESCE($13, work_item_bug_details.resolution),
            updated_at         = NOW()",
    )
    .bind(item_id)
    .bind(f.reproduction_steps)
    .bind(f.expected_behavior)
    .bind(f.actual_behavior)
    .bind(f.environment)
    .bind(f.affected_version)
    .bind(f.fixed_in_version)
    .bind(f.root_cause)
    .bind(f.is_regression)
    .bind(f.reported_by)
    .bind(f.triaged_by)
    .bind(f.set_triaged_at)
    .bind(f.resolution)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Fetch the bug-detail sidecar for an item, if present.
pub async fn fetch_bug_details(
    pool: &PgPool,
    item_id: i64,
) -> Result<Option<BugDetailsRow>, sqlx::Error> {
    sqlx::query_as::<_, BugDetailsRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {BUG_DETAIL_COLS} FROM work_item_bug_details WHERE item_id = $1"
    )))
    .bind(item_id)
    .fetch_optional(pool)
    .await
}

/// Transition an item's status. Runs the legality + actor-capability + evidence
/// gate ([`check_transition`]) and, on success, performs the status `UPDATE`
/// (maintaining `started_at`/`verified_at`/`completed_at`) and the
/// `work_item_status_history` insert in one transaction. `evidence_id` /
/// `negotiation_id` authorize `→verified` / `→deferred` respectively and are
/// recorded in the history row.
#[allow(clippy::too_many_arguments)]
pub async fn set_work_item_status(
    pool: &PgPool,
    id: i64,
    to: WorkItemStatus,
    actor: Actor,
    actor_id: Option<&str>,
    reason: Option<&str>,
    evidence_id: Option<i64>,
    negotiation_id: Option<i64>,
) -> Result<WorkItemRow, WorkItemOpError> {
    let mut tx = pool.begin().await?;
    let res = set_work_item_status_in_tx(
        &mut tx,
        id,
        to,
        actor,
        actor_id,
        reason,
        evidence_id,
        negotiation_id,
    )
    .await;

    match res {
        Ok(updated) => {
            tx.commit().await?;
            Ok(updated)
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// Transactional variant of [`set_work_item_status`]. Callers that need to
/// commit the status transition together with sidecar/audit rows should use
/// this helper inside their existing transaction.
#[allow(clippy::too_many_arguments)]
pub async fn set_work_item_status_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    to: WorkItemStatus,
    actor: Actor,
    actor_id: Option<&str>,
    reason: Option<&str>,
    evidence_id: Option<i64>,
    negotiation_id: Option<i64>,
) -> Result<WorkItemRow, WorkItemOpError> {
    let current = sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {WORK_ITEM_COLS} FROM work_items WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(WorkItemOpError::NotFound)?;
    let from = WorkItemStatus::parse(&current.status).ok_or_else(|| {
        WorkItemOpError::Db(sqlx::Error::Decode(
            format!("unknown stored status '{}'", current.status).into(),
        ))
    })?;

    let row: (bool, bool) = sqlx::query_as(
        "SELECT
            EXISTS (SELECT 1 FROM verification_evidence WHERE item_id = $1) AS present,
            (
              EXISTS (SELECT 1 FROM acceptance_criteria WHERE item_id = $1 AND required)
              AND NOT EXISTS (
                SELECT 1 FROM acceptance_criteria ac
                WHERE ac.item_id = $1 AND ac.required
                  AND NOT EXISTS (
                    SELECT 1 FROM verification_evidence e
                    WHERE e.criterion_id = ac.id
                      AND e.verdict = 'pass'
                      AND e.source IN ('ci','stop_hook','subagent_audit',
                                       'external_auditor','user_signoff','experiment')
                  )
              )
            ) AS passing",
    )
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    let ctx = TransitionContext {
        evidence_present: row.0,
        evidence_passing: row.1,
        user_negotiation: negotiation_id.is_some(),
    };
    check_transition(from, to, actor, ctx).map_err(WorkItemOpError::Transition)?;

    let updated = sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "UPDATE work_items SET
            status = $2,
            updated_at = NOW(),
            started_at = CASE WHEN $2 = 'in_progress' AND started_at IS NULL
                              THEN NOW() ELSE started_at END,
            verified_at = CASE WHEN $2 = 'verified' THEN NOW() ELSE verified_at END,
            completed_at = CASE WHEN $2 IN ('verified','cancelled')
                                THEN NOW() ELSE completed_at END
         WHERE id = $1
         RETURNING {WORK_ITEM_COLS}"
    )))
    .bind(id)
    .bind(to.as_str())
    .fetch_one(&mut **tx)
    .await?;

    sqlx::query(
        "INSERT INTO work_item_status_history
            (item_id, from_status, to_status, actor_kind, actor_id, evidence_id,
             negotiation_id, reason)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(from.as_str())
    .bind(to.as_str())
    .bind(actor.as_str())
    .bind(actor_id)
    .bind(evidence_id)
    .bind(negotiation_id)
    .bind(reason)
    .execute(&mut **tx)
    .await?;

    // Auto-unblock cascade: when an item reaches `verified`, dependents that
    // were `blocked` solely on it may now be actionable. In the SAME tx, move
    // each such dependent `blocked → ready` as `Actor::System` — legal
    // (System is in the Blocked→Ready actor set) and provably incapable of
    // reaching a judgment state (System has NO arm into verified/rejected/
    // deferred/confirmed; see `transition.rs::system_absent_from_judgment_columns`).
    // Routed through `check_transition` so the gate is never bypassed.
    if to == WorkItemStatus::Verified {
        let candidates: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT w.id FROM work_items w
             JOIN item_relations r ON (
                 (r.relation_type = 'depends_on' AND r.from_item_id = w.id AND r.to_item_id = $1)
                 OR (r.relation_type = 'blocks' AND r.to_item_id = w.id AND r.from_item_id = $1))
             WHERE w.status = 'blocked'",
        )
        .bind(id)
        .fetch_all(&mut **tx)
        .await?;
        for dep_id in candidates {
            // Still blocked iff an UNRESOLVED blocker remains (the inverse of
            // the NOT_BLOCKED predicate, evaluated within this tx so the
            // just-verified row counts as cleared).
            let still_blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                    SELECT 1 FROM item_relations r JOIN work_items b ON (
                        (r.relation_type = 'depends_on' AND r.from_item_id = $1 AND b.id = r.to_item_id)
                        OR (r.relation_type = 'blocks' AND r.to_item_id = $1 AND b.id = r.from_item_id))
                    WHERE b.status NOT IN ('verified','claimed_done','deferred','cancelled'))",
            )
            .bind(dep_id)
            .fetch_one(&mut **tx)
            .await?;
            if still_blocked {
                continue;
            }
            // Trust gate: System may perform ONLY Blocked → Ready.
            check_transition(
                WorkItemStatus::Blocked,
                WorkItemStatus::Ready,
                Actor::System,
                TransitionContext::default(),
            )
            .map_err(WorkItemOpError::Transition)?;
            sqlx::query(
                "UPDATE work_items SET status = 'ready', updated_at = NOW()
                 WHERE id = $1 AND status = 'blocked'",
            )
            .bind(dep_id)
            .execute(&mut **tx)
            .await?;
            sqlx::query(
                "INSERT INTO work_item_status_history
                    (item_id, from_status, to_status, actor_kind, actor_id, reason)
                 VALUES ($1, 'blocked', 'ready', 'system', 'system',
                         'auto-unblocked: last blocker verified')",
            )
            .bind(dep_id)
            .execute(&mut **tx)
            .await?;
        }
    }

    Ok(updated)
}

/// Set (or clear) an item's durable `assignee` (v16). `assignee=None` unassigns
/// (also clears `assigned_at`). Orthogonal to the runtime `claimed_by` lease —
/// no status transition is involved (assignment ≠ execution).
pub async fn assign_work_item(
    pool: &PgPool,
    id: i64,
    assignee: Option<&str>,
    assigned_by: Option<&str>,
) -> Result<WorkItemRow, WorkItemOpError> {
    let row = sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "UPDATE work_items SET
            assignee = $2,
            assigned_at = CASE WHEN $2 IS NULL THEN NULL ELSE NOW() END,
            assigned_by = $3,
            updated_at = NOW()
         WHERE id = $1
         RETURNING {WORK_ITEM_COLS}"
    )))
    .bind(id)
    .bind(assignee)
    .bind(assigned_by)
    .fetch_optional(pool)
    .await?;
    row.ok_or(WorkItemOpError::NotFound)
}

/// Read-only "what can I do now" frontier: actionable-status items
/// (`pending`/`confirmed`/`ready`) whose every blocker is cleared
/// (`NOT_BLOCKED`), ranked like `claim_next` but WITHOUT claiming (no lease, no
/// `FOR UPDATE`). Optionally scoped to a plan subtree and/or a durable assignee.
pub async fn next_actionable_work_items(
    pool: &PgPool,
    plan_root_id: Option<i64>,
    assignee: Option<&str>,
    limit: i64,
) -> Result<Vec<WorkItemRow>, sqlx::Error> {
    let cap = limit.clamp(1, 1000);
    sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "WITH RECURSIVE subtree AS (
            SELECT id FROM work_items WHERE id = $1
            UNION ALL
            SELECT c.id FROM work_items c JOIN subtree s ON c.parent_id = s.id
         )
         SELECT {WORK_ITEM_COLS} FROM work_items w
         WHERE w.status IN ('pending','confirmed','ready')
           AND ($1::bigint IS NULL OR w.id IN (SELECT id FROM subtree))
           AND ($3::text IS NULL OR w.assignee = $3)
           AND {NOT_BLOCKED}
         ORDER BY w.priority DESC, w.computed_score DESC NULLS LAST, w.id
         LIMIT $2"
    )))
    .bind(plan_root_id)
    .bind(cap)
    .bind(assignee)
    .fetch_all(pool)
    .await
}

/// One chronological event in an item's unified timeline.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TimelineRow {
    pub kind: String,
    pub at: DateTime<Utc>,
    pub actor: Option<String>,
    pub summary: String,
    pub detail: serde_json::Value,
}

/// The full per-item timeline: a chronological UNION of status transitions,
/// progress notes, claim/handoff events, verification evidence, and scope
/// negotiations — the `work_item_history` feed. The per-event `detail` jsonb is
/// cast to text in SQL and re-parsed in Rust (sqlx is built without the `json`
/// feature), keeping the decode dependency-free.
pub async fn work_item_timeline(
    pool: &PgPool,
    item_id: i64,
    limit: i64,
) -> Result<Vec<TimelineRow>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Raw {
        kind: String,
        at: DateTime<Utc>,
        actor: Option<String>,
        summary: String,
        detail_text: String,
    }
    let cap = limit.clamp(1, 1000);
    let rows = sqlx::query_as::<_, Raw>(
        "SELECT kind, at, actor, summary, detail_text FROM (
            SELECT 'status' AS kind, created_at AS at, actor_id AS actor,
                   (COALESCE(from_status, '∅') || ' → ' || to_status) AS summary,
                   jsonb_build_object('from', from_status, 'to', to_status,
                       'actor_kind', actor_kind, 'reason', reason,
                       'evidence_id', evidence_id, 'negotiation_id', negotiation_id)::text AS detail_text
              FROM work_item_status_history WHERE item_id = $1
            UNION ALL
            SELECT 'progress', created_at, actor_id, left(note, 80),
                   jsonb_build_object('note', note, 'percent', percent, 'provenance', provenance)::text
              FROM work_item_progress WHERE item_id = $1
            UNION ALL
            SELECT 'claim', created_at, agent_id, action,
                   jsonb_build_object('action', action, 'lease_expires_at', lease_expires_at)::text
              FROM work_item_claims WHERE work_item_id = $1
            UNION ALL
            SELECT 'evidence', created_at, runner_identity, (source || ' ' || verdict),
                   jsonb_build_object('verdict', verdict, 'source', source,
                       'exit_code', exit_code, 'criterion_id', criterion_id)::text
              FROM verification_evidence WHERE item_id = $1
            UNION ALL
            SELECT 'negotiation', created_at, granted_by, action,
                   jsonb_build_object('action', action, 'reason', reason)::text
              FROM scope_negotiations WHERE item_id = $1
         ) t
         ORDER BY at ASC
         LIMIT $2",
    )
    .bind(item_id)
    .bind(cap)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| TimelineRow {
            kind: r.kind,
            at: r.at,
            actor: r.actor,
            summary: r.summary,
            detail: serde_json::from_str(&r.detail_text).unwrap_or(serde_json::Value::Null),
        })
        .collect())
}

/// Return an item's subtree (the item plus all descendants via `parent_id`),
/// ordered by depth then priority — the materialized hierarchy for
/// `work_item_tree`. Bounded by `max_rows` and cycle-suppressed for safety on
/// pathological/corrupted trees.
pub async fn get_work_item_subtree(
    pool: &PgPool,
    root_id: i64,
    max_rows: i64,
) -> Result<Vec<WorkItemRow>, sqlx::Error> {
    let cap = max_rows.clamp(1, 100_000);
    let child_cols = WORK_ITEM_COLS
        .split(", ")
        .map(|c| format!("c.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "WITH RECURSIVE subtree AS (
            SELECT {cols}, 0 AS depth, ARRAY[id] AS path FROM work_items WHERE id = $1
            UNION ALL
            SELECT {child_cols}, s.depth + 1, s.path || c.id
            FROM work_items c JOIN subtree s ON c.parent_id = s.id
            WHERE NOT c.id = ANY(s.path)
              AND cardinality(s.path) < $2
         )
         SELECT {cols} FROM subtree
         ORDER BY depth, priority DESC, id
         LIMIT $2",
        cols = WORK_ITEM_COLS,
    )))
    .bind(root_id)
    .bind(cap)
    .fetch_all(pool)
    .await
}

/// Re-parent an item (move its subtree) and recompute `root_id` for the whole
/// moved subtree. Passing `new_parent_id = None` makes it a root.
pub async fn reparent_work_item(
    pool: &PgPool,
    id: i64,
    new_parent_id: Option<i64>,
) -> Result<(), WorkItemOpError> {
    let mut tx = pool.begin().await?;
    // Update the moved node's parent + root.
    sqlx::query(
        "UPDATE work_items SET
            parent_id = $2,
            root_id = (SELECT COALESCE(p.root_id, p.id) FROM work_items p WHERE p.id = $2),
            updated_at = NOW()
         WHERE id = $1",
    )
    .bind(id)
    .bind(new_parent_id)
    .execute(&mut *tx)
    .await?;
    // Recompute root_id for all descendants: their root is the moved node's new
    // root (or the moved node itself if it is now a root).
    sqlx::query(
        "WITH RECURSIVE moved AS (
            SELECT id, COALESCE(root_id, id) AS new_root FROM work_items WHERE id = $1
            UNION ALL
            SELECT c.id, m.new_root
            FROM work_items c JOIN moved m ON c.parent_id = m.id
         )
         UPDATE work_items w SET root_id = m.new_root, updated_at = NOW()
         FROM moved m WHERE w.id = m.id AND w.id <> $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Resolve an optional project name to its id. `None` or an unknown name yields
/// `Ok(None)` (a workspace-global item); a known name yields `Ok(Some(id))`.
pub async fn resolve_project_id(
    pool: &PgPool,
    project: Option<&str>,
) -> Result<Option<i32>, sqlx::Error> {
    match project {
        None => Ok(None),
        Some(name) => {
            sqlx::query_scalar::<_, i32>("SELECT id FROM projects WHERE name = $1 LIMIT 1")
                .bind(name)
                .fetch_optional(pool)
                .await
        }
    }
}

/// One countable leaf of a subtree roll-up (deferred/cancelled excluded by the
/// query). `universal_satisfied` is precomputed in SQL for parametric leaves.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RollupLeafRow {
    pub status: String,
    pub weight: f32,
    pub parametric: bool,
    pub universal_satisfied: bool,
}

/// Gather the countable leaves of the subtree rooted at `root_id` (the item
/// plus all descendants via `parent_id`), excluding deferred/cancelled. For a
/// parametric leaf, `universal_satisfied` is true iff some `universal`
/// acceptance criterion has passing evidence covering the full corpus
/// (`coverage_count >= parametric_expected`) — the anti-single-case rule.
pub async fn fetch_rollup_leaves(
    pool: &PgPool,
    root_id: i64,
) -> Result<Vec<RollupLeafRow>, sqlx::Error> {
    sqlx::query_as::<_, RollupLeafRow>(
        "WITH RECURSIVE subtree AS (
            SELECT id, parent_id, status, weight, parametric, parametric_expected
              FROM work_items WHERE id = $1
            UNION ALL
            SELECT c.id, c.parent_id, c.status, c.weight, c.parametric, c.parametric_expected
              FROM work_items c JOIN subtree s ON c.parent_id = s.id
         )
         SELECT s.status, s.weight, s.parametric,
            CASE WHEN s.parametric THEN COALESCE((
                SELECT bool_or(e.verdict = 'pass'
                               AND s.parametric_expected IS NOT NULL
                               AND e.coverage_count >= s.parametric_expected)
                  FROM acceptance_criteria ac
                  JOIN verification_evidence e ON e.criterion_id = ac.id
                 WHERE ac.item_id = s.id AND ac.coverage_mode = 'universal'
            ), FALSE) ELSE FALSE END AS universal_satisfied
         FROM subtree s
         WHERE NOT EXISTS (SELECT 1 FROM work_items c WHERE c.parent_id = s.id)
           AND s.status NOT IN ('deferred', 'cancelled')",
    )
    .bind(root_id)
    .fetch_all(pool)
    .await
}

/// Compute the weighted completion roll-up for the subtree at `root_id`,
/// mapping the gathered leaves through the pure aggregator
/// [`crate::tracker::rollup::aggregate`].
pub async fn compute_rollup(
    pool: &PgPool,
    root_id: i64,
) -> Result<crate::tracker::rollup::RollupResult, sqlx::Error> {
    let rows = fetch_rollup_leaves(pool, root_id).await?;
    let leaves: Vec<crate::tracker::rollup::LeafContribution> = rows
        .iter()
        .map(|r| crate::tracker::rollup::LeafContribution {
            status: WorkItemStatus::parse(&r.status).unwrap_or(WorkItemStatus::Pending),
            weight: r.weight as f64,
            parametric: r.parametric,
            universal_satisfied: r.universal_satisfied,
        })
        .collect();
    Ok(crate::tracker::rollup::aggregate(&leaves))
}

/// Recompute `computed_score` for active items (re-prioritization) and return
/// the top `limit` by score. The recency term mirrors
/// `crate::embed::rerank_ext::recency_multiplier` (`0.5^(age/half_life)`),
/// computed set-based in SQL for a one-pass update; plus the manual `priority`
/// base and a dependency-unblock bonus (an item that blocks more others ranks
/// higher, so finishing it frees the most work). Active =
/// pending/ready/in_progress/blocked. Updates ALL active items in scope; the
/// returned slice is the top `limit` for the now/next/later plan.
pub async fn reprioritize_work_items(
    pool: &PgPool,
    project_id: Option<i32>,
    half_life_days: f64,
    limit: i64,
) -> Result<Vec<WorkItemRow>, sqlx::Error> {
    let cap = limit.clamp(1, 500);
    let half_life = if half_life_days.is_finite() && half_life_days > 0.0 {
        half_life_days
    } else {
        14.0
    };
    sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "WITH rescored AS (
            UPDATE work_items w SET computed_score =
                  w.priority::float8
                + 10.0::float8 * power(0.5::float8,
                    GREATEST(extract(epoch FROM (NOW() - w.updated_at)) / 86400.0, 0.0) / $2)
                + 5.0::float8 * (COALESCE((SELECT count(*) FROM item_relations r
                        WHERE r.from_item_id = w.id AND r.relation_type = 'blocks'), 0))::float8
            WHERE w.status IN ('pending','confirmed','ready','in_progress','blocked')
              AND ($1::int IS NULL OR w.project_id = $1)
            RETURNING {cols}
         )
         SELECT {cols} FROM rescored
         ORDER BY computed_score DESC NULLS LAST, priority DESC, id
         LIMIT $3",
        cols = WORK_ITEM_COLS,
    )))
    .bind(project_id)
    .bind(half_life)
    .bind(cap)
    .fetch_all(pool)
    .await
}

/// A semantic-search hit: a work item plus its cosine similarity to the query.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct WorkItemSearchHit {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub item: WorkItemRow,
    pub similarity: f64,
}

/// Semantic backlog search: cosine nearest-neighbours over
/// `work_items.embedding` (HNSW, 1024-d BGE-M3). Only items that carry an
/// embedding are returned; optional project scope. `similarity = 1 - distance`.
pub async fn search_work_items(
    pool: &PgPool,
    query: Vector,
    project_id: Option<i32>,
    limit: i64,
) -> Result<Vec<WorkItemSearchHit>, sqlx::Error> {
    let cap = limit.clamp(1, 100);
    sqlx::query_as::<_, WorkItemSearchHit>(sqlx::AssertSqlSafe(format!(
        "SELECT {WORK_ITEM_COLS}, 1.0 - (embedding <=> $1) AS similarity
         FROM work_items
         WHERE embedding IS NOT NULL
           AND ($2::int IS NULL OR project_id = $2)
         ORDER BY embedding <=> $1
         LIMIT $3"
    )))
    .bind(query)
    .bind(project_id)
    .bind(cap)
    .fetch_all(pool)
    .await
}

// ── Plan definitions + validation (Phase 4) ──────────────────────────────────

/// A plan-definition template row.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct PlanDefinitionRow {
    pub id: i64,
    pub slug: String,
    pub version: i32,
    pub title: String,
    pub description: Option<String>,
    pub extends_id: Option<i64>,
    pub status: String,
    pub body_toml: Option<String>,
    pub created_at: DateTime<Utc>,
}

const PLAN_DEF_COLS: &str =
    "id, slug, version, title, description, extends_id, status, body_toml, created_at";

/// A `definition_rules` row, mappable to a `validate::RuleSpec`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DefinitionRuleRow {
    pub rule_kind: String,
    pub applies_to_kind: Option<String>,
    pub child_kind: Option<String>,
    pub min_count: Option<i32>,
    pub max_count: Option<i32>,
    pub field_name: Option<String>,
    pub pattern: Option<String>,
    pub severity: String,
}

impl DefinitionRuleRow {
    pub fn into_spec(self) -> crate::tracker::validate::RuleSpec {
        crate::tracker::validate::RuleSpec {
            rule_kind: self.rule_kind,
            applies_to_kind: self.applies_to_kind,
            child_kind: self.child_kind,
            min_count: self.min_count,
            max_count: self.max_count,
            field_name: self.field_name,
            pattern: self.pattern,
            severity: self.severity,
        }
    }
}

/// Upsert a plan definition by `(slug, version)`, returning its id.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_plan_definition(
    pool: &PgPool,
    slug: &str,
    version: i32,
    title: &str,
    description: Option<&str>,
    extends_id: Option<i64>,
    status: &str,
    body_toml: Option<&str>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO plan_definitions (slug, version, title, description, extends_id, status, body_toml)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (slug, version) DO UPDATE SET
            title = EXCLUDED.title, description = EXCLUDED.description,
            extends_id = EXCLUDED.extends_id, status = EXCLUDED.status,
            body_toml = EXCLUDED.body_toml
         RETURNING id",
    )
    .bind(slug)
    .bind(version)
    .bind(title)
    .bind(description)
    .bind(extends_id)
    .bind(status)
    .bind(body_toml)
    .fetch_one(pool)
    .await
}

/// Replace a definition's rules (the tool re-inserts the full set on each
/// `plan_define`, so an edit is a clean swap).
pub async fn clear_definition_rules(pool: &PgPool, definition_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM definition_rules WHERE definition_id = $1")
        .bind(definition_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_definition_rule(
    pool: &PgPool,
    definition_id: i64,
    rule_kind: &str,
    applies_to_kind: Option<&str>,
    child_kind: Option<&str>,
    min_count: Option<i32>,
    max_count: Option<i32>,
    field_name: Option<&str>,
    pattern: Option<&str>,
    severity: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO definition_rules
            (definition_id, rule_kind, applies_to_kind, child_kind, min_count, max_count,
             field_name, pattern, severity)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(definition_id)
    .bind(rule_kind)
    .bind(applies_to_kind)
    .bind(child_kind)
    .bind(min_count)
    .bind(max_count)
    .bind(field_name)
    .bind(pattern)
    .bind(severity)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch a definition by slug; `version = None` returns the highest version.
pub async fn get_plan_definition(
    pool: &PgPool,
    slug: &str,
    version: Option<i32>,
) -> Result<Option<PlanDefinitionRow>, sqlx::Error> {
    sqlx::query_as::<_, PlanDefinitionRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {PLAN_DEF_COLS} FROM plan_definitions
         WHERE slug = $1 AND ($2::int IS NULL OR version = $2)
         ORDER BY version DESC LIMIT 1"
    )))
    .bind(slug)
    .bind(version)
    .fetch_optional(pool)
    .await
}

pub async fn list_plan_definitions(pool: &PgPool) -> Result<Vec<PlanDefinitionRow>, sqlx::Error> {
    sqlx::query_as::<_, PlanDefinitionRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {PLAN_DEF_COLS} FROM plan_definitions ORDER BY slug, version DESC"
    )))
    .fetch_all(pool)
    .await
}

/// Fetch a single definition by primary key (resolves `extends_id` → parent
/// slug for export).
pub async fn get_plan_definition_by_id(
    pool: &PgPool,
    id: i64,
) -> Result<Option<PlanDefinitionRow>, sqlx::Error> {
    sqlx::query_as::<_, PlanDefinitionRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {PLAN_DEF_COLS} FROM plan_definitions WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
}

#[derive(Debug, sqlx::FromRow)]
struct ValidationFacetRow {
    public_id: String,
    parent_public_id: Option<String>,
    kind: String,
    title: String,
    has_body: bool,
    has_due: bool,
    acceptance_count: i64,
    parametric: bool,
    has_universal_criterion: bool,
    depth: i32,
    // Bug metadata (v12): severity on the spine, the rest from the sidecar.
    severity: Option<String>,
    bd_reproduction_steps: Option<String>,
    bd_expected_behavior: Option<String>,
    bd_actual_behavior: Option<String>,
    bd_environment: Option<String>,
    bd_affected_version: Option<String>,
    bd_fixed_in_version: Option<String>,
    bd_root_cause: Option<String>,
}

/// Gather the validation facets of an instance subtree (kind, parent, field
/// presence, acceptance counts, parametric coverage, depth) for the pure
/// validator.
pub async fn fetch_validation_facets(
    pool: &PgPool,
    root_id: i64,
) -> Result<Vec<crate::tracker::validate::ItemFacet>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ValidationFacetRow>(
        "WITH RECURSIVE subtree AS (
            SELECT w.id, w.public_id, w.parent_id, w.kind, w.title, w.body, w.due_at,
                   w.parametric, w.severity, 0 AS depth
              FROM work_items w WHERE w.id = $1
            UNION ALL
            SELECT c.id, c.public_id, c.parent_id, c.kind, c.title, c.body, c.due_at,
                   c.parametric, c.severity, s.depth + 1
              FROM work_items c JOIN subtree s ON c.parent_id = s.id
         )
         SELECT s.public_id,
            (SELECT p.public_id FROM work_items p WHERE p.id = s.parent_id) AS parent_public_id,
            s.kind, s.title,
            (s.body IS NOT NULL AND length(btrim(s.body)) > 0) AS has_body,
            (s.due_at IS NOT NULL) AS has_due,
            COALESCE((SELECT count(*) FROM acceptance_criteria ac WHERE ac.item_id = s.id), 0)
                AS acceptance_count,
            s.parametric,
            COALESCE((SELECT bool_or(ac.coverage_mode = 'universal')
                        FROM acceptance_criteria ac WHERE ac.item_id = s.id), FALSE)
                AS has_universal_criterion,
            s.depth,
            s.severity,
            bd.reproduction_steps AS bd_reproduction_steps,
            bd.expected_behavior  AS bd_expected_behavior,
            bd.actual_behavior    AS bd_actual_behavior,
            bd.environment        AS bd_environment,
            bd.affected_version   AS bd_affected_version,
            bd.fixed_in_version   AS bd_fixed_in_version,
            bd.root_cause         AS bd_root_cause
         FROM subtree s
         LEFT JOIN work_item_bug_details bd ON bd.item_id = s.id",
    )
    .bind(root_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            // Collect the names of present (non-blank) bug-detail fields for
            // `required_field` rules that target bug metadata.
            let mut bug_fields: Vec<String> = Vec::with_capacity(8);
            for (name, val) in [
                ("severity", &r.severity),
                ("reproduction_steps", &r.bd_reproduction_steps),
                ("expected_behavior", &r.bd_expected_behavior),
                ("actual_behavior", &r.bd_actual_behavior),
                ("environment", &r.bd_environment),
                ("affected_version", &r.bd_affected_version),
                ("fixed_in_version", &r.bd_fixed_in_version),
                ("root_cause", &r.bd_root_cause),
            ] {
                if val.as_deref().map(str::trim).is_some_and(|s| !s.is_empty()) {
                    bug_fields.push(name.to_string());
                }
            }
            crate::tracker::validate::ItemFacet {
                public_id: r.public_id,
                parent_public_id: r.parent_public_id,
                kind: r.kind,
                title: r.title,
                has_body: r.has_body,
                has_due: r.has_due,
                acceptance_count: r.acceptance_count,
                parametric: r.parametric,
                has_universal_criterion: r.has_universal_criterion,
                depth: r.depth,
                bug_fields,
            }
        })
        .collect())
}

/// Validate an instance subtree against a definition's rules; returns the
/// (severity-sorted) violations.
pub async fn validate_plan(
    pool: &PgPool,
    root_id: i64,
    definition_id: i64,
) -> Result<Vec<crate::tracker::validate::Violation>, sqlx::Error> {
    let facets = fetch_validation_facets(pool, root_id).await?;
    let rules: Vec<crate::tracker::validate::RuleSpec> = get_definition_rules(pool, definition_id)
        .await?
        .into_iter()
        .map(DefinitionRuleRow::into_spec)
        .collect();
    Ok(crate::tracker::validate::validate(&facets, &rules))
}

pub async fn get_definition_rules(
    pool: &PgPool,
    definition_id: i64,
) -> Result<Vec<DefinitionRuleRow>, sqlx::Error> {
    sqlx::query_as::<_, DefinitionRuleRow>(
        "SELECT rule_kind, applies_to_kind, child_kind, min_count, max_count, field_name,
                pattern, severity
         FROM definition_rules WHERE definition_id = $1 ORDER BY id",
    )
    .bind(definition_id)
    .fetch_all(pool)
    .await
}

// ── Verification: acceptance criteria + evidence ledger (Phase 5) ────────────

/// An acceptance-criterion row (the machine-checkable spec for an item).
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AcceptanceCriterionRow {
    pub id: i64,
    pub item_id: i64,
    pub criterion_kind: String,
    pub description: String,
    pub acceptance_uri: Option<String>,
    pub expect_exit: Option<i32>,
    pub coverage_mode: String,
    pub gate: Option<String>,
    pub required: bool,
    pub created_at: DateTime<Utc>,
}

/// An append-only verification-evidence row.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct VerificationEvidenceRow {
    pub id: i64,
    pub criterion_id: i64,
    pub item_id: i64,
    pub verdict: String,
    pub source: String,
    pub exit_code: Option<i32>,
    pub coverage_count: Option<i32>,
    pub coverage_total: Option<i32>,
    pub runner_identity: Option<String>,
    pub commit_sha: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Add an acceptance criterion to an item, returning its id.
#[allow(clippy::too_many_arguments)]
pub async fn insert_acceptance_criterion(
    pool: &PgPool,
    item_id: i64,
    criterion_kind: &str,
    description: &str,
    acceptance_uri: Option<&str>,
    expect_exit: Option<i32>,
    coverage_mode: &str,
    gate: Option<&str>,
    required: bool,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let id = insert_acceptance_criterion_in_tx(
        &mut tx,
        item_id,
        criterion_kind,
        description,
        acceptance_uri,
        expect_exit,
        coverage_mode,
        gate,
        required,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Transactional variant of [`insert_acceptance_criterion`].
#[allow(clippy::too_many_arguments)]
pub async fn insert_acceptance_criterion_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    item_id: i64,
    criterion_kind: &str,
    description: &str,
    acceptance_uri: Option<&str>,
    expect_exit: Option<i32>,
    coverage_mode: &str,
    gate: Option<&str>,
    required: bool,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO acceptance_criteria
            (item_id, criterion_kind, description, acceptance_uri, expect_exit, coverage_mode,
             gate, required)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING id",
    )
    .bind(item_id)
    .bind(criterion_kind)
    .bind(description)
    .bind(acceptance_uri)
    .bind(expect_exit)
    .bind(coverage_mode)
    .bind(gate)
    .bind(required)
    .fetch_one(&mut **tx)
    .await
}

pub async fn list_acceptance_criteria(
    pool: &PgPool,
    item_id: i64,
) -> Result<Vec<AcceptanceCriterionRow>, sqlx::Error> {
    sqlx::query_as::<_, AcceptanceCriterionRow>(
        "SELECT id, item_id, criterion_kind, description, acceptance_uri, expect_exit,
                coverage_mode, gate, required, created_at
         FROM acceptance_criteria WHERE item_id = $1 ORDER BY id",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await
}

/// Append a verification-evidence row, deriving `item_id` from the criterion
/// (the `INSERT … SELECT … WHERE ac.id = $1` yields no row — hence
/// `RowNotFound` — if the criterion doesn't exist). `detail_json` is bound as
/// JSON text with a `::jsonb` cast (the crate's sqlx has no `json` feature).
/// Returns the new evidence id.
#[allow(clippy::too_many_arguments)]
pub async fn record_verification_evidence(
    pool: &PgPool,
    criterion_id: i64,
    verdict: &str,
    source: &str,
    exit_code: Option<i32>,
    coverage_count: Option<i32>,
    coverage_total: Option<i32>,
    runner_identity: Option<&str>,
    evidence_sha256: Option<&str>,
    commit_sha: Option<&str>,
    spec_sha256: Option<&str>,
    detail_json: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO verification_evidence
            (criterion_id, item_id, verdict, source, exit_code, coverage_count, coverage_total,
             runner_identity, evidence_sha256, commit_sha, spec_sha256, detail_json)
         SELECT $1, ac.item_id, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::jsonb
         FROM acceptance_criteria ac WHERE ac.id = $1
         RETURNING id",
    )
    .bind(criterion_id)
    .bind(verdict)
    .bind(source)
    .bind(exit_code)
    .bind(coverage_count)
    .bind(coverage_total)
    .bind(runner_identity)
    .bind(evidence_sha256)
    .bind(commit_sha)
    .bind(spec_sha256)
    .bind(detail_json)
    .fetch_one(pool)
    .await
}

/// The most recent passing, trusted-source evidence id for an item (used to
/// stamp the `→verified` status-history row). `None` if nothing qualifies.
pub async fn latest_passing_evidence_id(
    pool: &PgPool,
    item_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM verification_evidence
         WHERE item_id = $1 AND verdict = 'pass'
           AND source IN ('ci','stop_hook','subagent_audit','external_auditor','user_signoff','experiment')
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(item_id)
    .fetch_optional(pool)
    .await
}

/// Upsert one ingested plan node by `public_id` (idempotent re-ingest). On a
/// fresh insert the seed `status` is used; on conflict the structural fields
/// (title/body/parametric/parent/root/definition) are refreshed but `status`
/// is **preserved** so re-ingesting an edited plan never resets work progress.
/// Returns `(id, inserted)` — `inserted=true` only for a brand-new row (via the
/// `xmax = 0` trick), so the caller adds acceptance criteria only once.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_ingested_item(
    pool: &PgPool,
    public_id: &str,
    parent_id: Option<i64>,
    project_id: Option<i32>,
    definition_id: Option<i64>,
    kind: &str,
    status: &str,
    title: &str,
    body: Option<&str>,
    parametric: bool,
    parametric_corpus: Option<&str>,
) -> Result<(i64, bool), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row = upsert_ingested_item_in_tx(
        &mut tx,
        public_id,
        parent_id,
        project_id,
        definition_id,
        kind,
        status,
        title,
        body,
        parametric,
        parametric_corpus,
    )
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// Transactional variant of [`upsert_ingested_item`]. If a parent is supplied,
/// lock it while deriving the stored root id so concurrent reparenting cannot
/// race structural refresh of an ingested tree node.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_ingested_item_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    public_id: &str,
    parent_id: Option<i64>,
    project_id: Option<i32>,
    definition_id: Option<i64>,
    kind: &str,
    status: &str,
    title: &str,
    body: Option<&str>,
    parametric: bool,
    parametric_corpus: Option<&str>,
) -> Result<(i64, bool), sqlx::Error> {
    let root_id = match parent_id {
        None => None,
        Some(parent_id) => Some(
            sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(root_id, id) FROM work_items WHERE id = $1 FOR SHARE",
            )
            .bind(parent_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?,
        ),
    };

    sqlx::query_as::<_, (i64, bool)>(
        "INSERT INTO work_items
            (public_id, parent_id, project_id, definition_id, root_id, kind, status, title, body,
             parametric, parametric_corpus, origin)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'ingest_plan')
         ON CONFLICT (public_id) DO UPDATE SET
            title = EXCLUDED.title, body = EXCLUDED.body, parametric = EXCLUDED.parametric,
            parametric_corpus = EXCLUDED.parametric_corpus, parent_id = EXCLUDED.parent_id,
            root_id = EXCLUDED.root_id, definition_id = EXCLUDED.definition_id, updated_at = NOW()
         RETURNING id, (xmax = 0) AS inserted",
    )
    .bind(public_id)
    .bind(parent_id)
    .bind(project_id)
    .bind(definition_id)
    .bind(root_id)
    .bind(kind)
    .bind(status)
    .bind(title)
    .bind(body)
    .bind(parametric)
    .bind(parametric_corpus)
    .fetch_one(&mut **tx)
    .await
}

/// Record a user scope-negotiation (defer / reinstate / cancel / scope_cut) in
/// the append-only audit log, returning its id. `actor_kind` is fixed to
/// `'user'` (the DB CHECK also enforces it).
pub async fn record_scope_negotiation(
    pool: &PgPool,
    item_id: i64,
    action: &str,
    granted_by: &str,
    reason: &str,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let id = record_scope_negotiation_in_tx(&mut tx, item_id, action, granted_by, reason).await?;
    tx.commit().await?;
    Ok(id)
}

/// Transactional variant of [`record_scope_negotiation`].
pub async fn record_scope_negotiation_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    item_id: i64,
    action: &str,
    granted_by: &str,
    reason: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO scope_negotiations (item_id, action, granted_by, actor_kind, reason)
         VALUES ($1, $2, $3, 'user', $4) RETURNING id",
    )
    .bind(item_id)
    .bind(action)
    .bind(granted_by)
    .bind(reason)
    .fetch_one(&mut **tx)
    .await
}

// ── A2A collaboration: claim / lease / presence (Phase 7) ────────────────────

/// One claim-ledger event.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ClaimEventRow {
    pub agent_id: String,
    pub action: String,
    pub to_agent_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// The not-blocked predicate (SQL fragment): the item `w` has no unresolved
/// `depends_on` target and no unresolved `blocks` source. Reused by claim CAS
/// and claim-next.
const NOT_BLOCKED: &str = "NOT EXISTS (
        SELECT 1 FROM item_relations r JOIN work_items b ON (
            (r.relation_type = 'depends_on' AND r.from_item_id = w.id AND b.id = r.to_item_id)
            OR (r.relation_type = 'blocks' AND r.to_item_id = w.id AND b.id = r.from_item_id))
        WHERE b.status NOT IN ('verified','claimed_done','deferred','cancelled'))";

/// Atomically claim a specific item: the single-row CAS only succeeds if it is
/// unclaimed (or already ours, or the lease expired), not terminal, and not
/// blocked. `Ok(None)` = contention lost / blocked / terminal. Writes a `claim`
/// ledger row in the same transaction.
pub async fn claim_work_item(
    pool: &PgPool,
    id: i64,
    agent_id: &str,
    lease_secs: i64,
) -> Result<Option<WorkItemRow>, sqlx::Error> {
    let lease = lease_secs.clamp(10, 86_400) as f64;
    let mut tx = pool.begin().await?;
    let row = sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "UPDATE work_items w SET
            claimed_by = $2,
            status = CASE WHEN w.status IN ('pending','confirmed','ready','blocked') THEN 'in_progress' ELSE w.status END,
            claimed_at = NOW(),
            lease_expires_at = NOW() + make_interval(secs => $3),
            claim_count = w.claim_count + 1,
            started_at = COALESCE(w.started_at, NOW()),
            updated_at = NOW()
         WHERE w.id = $1
           AND (w.claimed_by IS NULL OR w.claimed_by = $2 OR w.lease_expires_at < NOW())
           AND w.status IN ('pending','confirmed','ready','in_progress','blocked')
           AND {NOT_BLOCKED}
         RETURNING {WORK_ITEM_COLS}"
    )))
    .bind(id)
    .bind(agent_id)
    .bind(lease)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(r) = &row {
        sqlx::query(
            "INSERT INTO work_item_claims (work_item_id, agent_id, action, lease_expires_at)
             VALUES ($1, $2, 'claim', $3)",
        )
        .bind(r.id)
        .bind(agent_id)
        .bind(r.lease_expires_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(row)
}

/// Claim the next available (unclaimed, unblocked, pending/ready) item — top by
/// priority/score — optionally within a plan subtree. `FOR UPDATE SKIP LOCKED`
/// lets N agents each grab a distinct item with no contention. `Ok(None)` = the
/// queue is empty.
pub async fn claim_next_work_item(
    pool: &PgPool,
    agent_id: &str,
    plan_root_id: Option<i64>,
    lease_secs: i64,
) -> Result<Option<WorkItemRow>, sqlx::Error> {
    let lease = lease_secs.clamp(10, 86_400) as f64;
    let mut tx = pool.begin().await?;
    let picked: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "WITH RECURSIVE subtree AS (
            SELECT id FROM work_items WHERE id = $1
            UNION ALL
            SELECT c.id FROM work_items c JOIN subtree s ON c.parent_id = s.id
         )
         SELECT w.id FROM work_items w
         WHERE w.claimed_by IS NULL
           AND w.status IN ('pending','confirmed','ready')
           AND ($1::bigint IS NULL OR w.id IN (SELECT id FROM subtree))
           AND {NOT_BLOCKED}
         ORDER BY w.priority DESC, w.computed_score DESC NULLS LAST, w.id
         FOR UPDATE SKIP LOCKED
         LIMIT 1"
    )))
    .bind(plan_root_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(pid) = picked else {
        tx.commit().await?;
        return Ok(None);
    };
    let row = sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "UPDATE work_items SET
            claimed_by = $2, status = 'in_progress', claimed_at = NOW(),
            lease_expires_at = NOW() + make_interval(secs => $3),
            claim_count = claim_count + 1, started_at = COALESCE(started_at, NOW()),
            updated_at = NOW()
         WHERE id = $1
         RETURNING {WORK_ITEM_COLS}"
    )))
    .bind(pid)
    .bind(agent_id)
    .bind(lease)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO work_item_claims (work_item_id, agent_id, action, lease_expires_at)
         VALUES ($1, $2, 'claim', $3)",
    )
    .bind(row.id)
    .bind(agent_id)
    .bind(row.lease_expires_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(row))
}

/// Release a claim (owner-gated). `Ok(None)` = caller isn't the owner.
pub async fn release_work_item(
    pool: &PgPool,
    id: i64,
    agent_id: &str,
) -> Result<Option<WorkItemRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "UPDATE work_items SET claimed_by = NULL, lease_expires_at = NULL, updated_at = NOW()
         WHERE id = $1 AND claimed_by = $2
         RETURNING {WORK_ITEM_COLS}"
    )))
    .bind(id)
    .bind(agent_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(r) = &row {
        sqlx::query("INSERT INTO work_item_claims (work_item_id, agent_id, action) VALUES ($1, $2, 'release')")
            .bind(r.id)
            .bind(agent_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(row)
}

/// Hand off a claim to another agent (owner-gated re-key). `Ok(None)` = caller
/// isn't the current owner.
pub async fn handoff_work_item(
    pool: &PgPool,
    id: i64,
    from_agent: &str,
    to_agent: &str,
    lease_secs: i64,
) -> Result<Option<WorkItemRow>, sqlx::Error> {
    let lease = lease_secs.clamp(10, 86_400) as f64;
    let mut tx = pool.begin().await?;
    let row = sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "UPDATE work_items SET
            claimed_by = $3, claimed_at = NOW(),
            lease_expires_at = NOW() + make_interval(secs => $4),
            claim_count = claim_count + 1, updated_at = NOW()
         WHERE id = $1 AND claimed_by = $2
         RETURNING {WORK_ITEM_COLS}"
    )))
    .bind(id)
    .bind(from_agent)
    .bind(to_agent)
    .bind(lease)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(r) = &row {
        sqlx::query("INSERT INTO work_item_claims (work_item_id, agent_id, action, to_agent_id) VALUES ($1, $2, 'handoff_out', $3)")
            .bind(r.id)
            .bind(from_agent)
            .bind(to_agent)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO work_item_claims (work_item_id, agent_id, action) VALUES ($1, $2, 'handoff_in')")
            .bind(r.id)
            .bind(to_agent)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(row)
}

/// Recent claim-ledger events for an item (for `work_item_who_owns`).
pub async fn work_item_claim_history(
    pool: &PgPool,
    id: i64,
    limit: i64,
) -> Result<Vec<ClaimEventRow>, sqlx::Error> {
    sqlx::query_as::<_, ClaimEventRow>(
        "SELECT agent_id, action, to_agent_id, created_at
         FROM work_item_claims WHERE work_item_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(id)
    .bind(limit.clamp(1, 200))
    .fetch_all(pool)
    .await
}

/// Activity-driven presence upsert: mark an agent active (optionally on an
/// item). Called from every claim/progress write so presence is never stale.
pub async fn touch_presence(
    pool: &PgPool,
    agent_id: &str,
    current_work_item_id: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO agent_presence (agent_id, last_active_at, status, current_work_item_id, updated_at)
         VALUES ($1, NOW(), 'active', $2, NOW())
         ON CONFLICT (agent_id) DO UPDATE SET
            last_active_at = NOW(), status = 'active',
            current_work_item_id = COALESCE($2, agent_presence.current_work_item_id),
            updated_at = NOW()",
    )
    .bind(agent_id)
    .bind(current_work_item_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ── A2A visibility: presence decay, activity feed, roster (Phase 8) ──────────

/// Sweep expired leases (releasing the claim + writing an `expire` ledger row,
/// owner preserved) and decay stale presence (active→idle→offline). Returns
/// `(leases_expired, presence_idled)`. Run by the `work-item-presence` cron.
pub async fn sweep_presence_and_leases(
    pool: &PgPool,
    idle_secs: i64,
    offline_secs: i64,
) -> Result<(u64, u64), sqlx::Error> {
    let mut tx = pool.begin().await?;
    // Capture the to-expire claims (with their owner) before NULLing them.
    let stale: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, claimed_by FROM work_items
         WHERE claimed_by IS NOT NULL AND lease_expires_at IS NOT NULL AND lease_expires_at < NOW()
         FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (id, owner) in &stale {
        sqlx::query("INSERT INTO work_item_claims (work_item_id, agent_id, action) VALUES ($1, $2, 'expire')")
            .bind(id)
            .bind(owner)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "UPDATE work_items SET claimed_by = NULL, lease_expires_at = NULL, updated_at = NOW()
         WHERE claimed_by IS NOT NULL AND lease_expires_at IS NOT NULL AND lease_expires_at < NOW()",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    sqlx::query(
        "UPDATE agent_presence SET status = 'offline', updated_at = NOW()
         WHERE status <> 'offline' AND last_active_at < NOW() - make_interval(secs => $1)",
    )
    .bind(offline_secs.max(1) as f64)
    .execute(pool)
    .await?;
    let idled = sqlx::query(
        "UPDATE agent_presence SET status = 'idle', updated_at = NOW()
         WHERE status = 'active' AND last_active_at < NOW() - make_interval(secs => $1)",
    )
    .bind(idle_secs.max(1) as f64)
    .execute(pool)
    .await?
    .rows_affected();
    Ok((stale.len() as u64, idled))
}

/// Renew the lease on every item an agent currently holds (heartbeat). Returns
/// the number of leases renewed.
pub async fn renew_agent_leases(
    pool: &PgPool,
    agent_id: &str,
    lease_secs: i64,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE work_items SET lease_expires_at = NOW() + make_interval(secs => $2), updated_at = NOW()
         WHERE claimed_by = $1",
    )
    .bind(agent_id)
    .bind(lease_secs.clamp(10, 86_400) as f64)
    .execute(pool)
    .await?
    .rows_affected())
}

/// One workspace-activity event (progress note or claim event), agent-attributed.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ActivityRow {
    pub kind: String,
    pub agent_id: Option<String>,
    pub work_item_id: i64,
    pub public_id: String,
    pub title: String,
    pub detail: String,
    pub created_at: DateTime<Utc>,
}

/// The workspace/plan activity feed: a unioned, newest-first stream of progress
/// notes and claim events. `root_id = Some` scopes to a plan subtree (root or
/// its descendants by `root_id`).
pub async fn activity_feed(
    pool: &PgPool,
    root_id: Option<i64>,
    since: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<ActivityRow>, sqlx::Error> {
    sqlx::query_as::<_, ActivityRow>(
        "SELECT * FROM (
            SELECT 'progress' AS kind, p.actor_id AS agent_id, p.item_id AS work_item_id,
                   w.public_id, w.title, p.note AS detail, p.created_at
              FROM work_item_progress p JOIN work_items w ON w.id = p.item_id
             WHERE ($1::bigint IS NULL OR w.id = $1 OR w.root_id = $1)
               AND ($2::timestamptz IS NULL OR p.created_at > $2)
            UNION ALL
            SELECT 'claim' AS kind, c.agent_id, c.work_item_id, w.public_id, w.title,
                   c.action AS detail, c.created_at
              FROM work_item_claims c JOIN work_items w ON w.id = c.work_item_id
             WHERE ($1::bigint IS NULL OR w.id = $1 OR w.root_id = $1)
               AND ($2::timestamptz IS NULL OR c.created_at > $2)
         ) feed
         ORDER BY created_at DESC
         LIMIT $3",
    )
    .bind(root_id)
    .bind(since)
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await
}

/// An `agent_presence` row.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct PresenceRow {
    pub agent_id: String,
    pub status: String,
    pub current_work_item_id: Option<i64>,
    pub last_active_at: DateTime<Utc>,
}

/// Active-agent roster (presence rows active within the window), newest first.
pub async fn agent_presence_roster(
    pool: &PgPool,
    active_within_secs: i64,
) -> Result<Vec<PresenceRow>, sqlx::Error> {
    sqlx::query_as::<_, PresenceRow>(
        "SELECT agent_id, status, current_work_item_id, last_active_at FROM agent_presence
         WHERE last_active_at > NOW() - make_interval(secs => $1)
         ORDER BY last_active_at DESC",
    )
    .bind(active_within_secs.max(1) as f64)
    .fetch_all(pool)
    .await
}

/// Items an agent currently holds a (possibly expired) claim on.
pub async fn agent_current_items(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Vec<WorkItemRow>, sqlx::Error> {
    sqlx::query_as::<_, WorkItemRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {WORK_ITEM_COLS} FROM work_items WHERE claimed_by = $1 ORDER BY updated_at DESC"
    )))
    .bind(agent_id)
    .fetch_all(pool)
    .await
}

/// One agent's presence row, if any.
pub async fn get_agent_presence(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Option<PresenceRow>, sqlx::Error> {
    sqlx::query_as::<_, PresenceRow>(
        "SELECT agent_id, status, current_work_item_id, last_active_at FROM agent_presence
         WHERE agent_id = $1",
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await
}

// ============================================================================
// Phase 2 — tags + progress
// ============================================================================

/// A row of the `tags` catalog. `merged_into` is the rename/merge tombstone:
/// an active tag has `merged_into IS NULL`; a merged tag points at its
/// destination so old references resolve (the slug is preserved for stability).
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct TagRow {
    pub id: i64,
    pub name: String,
    pub slug: String,
    pub color: Option<String>,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub merged_into: Option<i64>,
}

/// Explicit column list shared by every `tags` `SELECT`/`RETURNING`.
const TAG_COLS: &str = "id, name, slug, color, description, created_at, merged_into";

/// One row of the append-only `work_item_progress` log. `provenance` is the
/// trust marker (`user_explicit` | `agent_write`); an MCP-authored note is
/// always `agent_write` (the tool layer never accepts provenance from params).
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ProgressRow {
    pub id: i64,
    pub item_id: i64,
    pub note: String,
    pub percent: Option<i16>,
    pub provenance: String,
    pub actor_id: Option<String>,
    pub session_id: Option<uuid::Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Explicit column list shared by every `work_item_progress` `SELECT`/`RETURNING`.
const PROGRESS_COLS: &str =
    "id, item_id, note, percent, provenance, actor_id, session_id, created_at";

/// Create or update a tag by its stable `slug`. On conflict the name is
/// overwritten and `color`/`description` are filled only when a new value is
/// supplied (`COALESCE(EXCLUDED.…, tags.…)`), so a bare re-tag never clears an
/// existing color. Returns the tag id.
pub async fn upsert_tag(
    pool: &PgPool,
    name: &str,
    slug: &str,
    color: Option<&str>,
    description: Option<&str>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO tags (name, slug, color, description)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (slug) DO UPDATE SET
            name = EXCLUDED.name,
            color = COALESCE(EXCLUDED.color, tags.color),
            description = COALESCE(EXCLUDED.description, tags.description)
         RETURNING id",
    )
    .bind(name)
    .bind(slug)
    .bind(color)
    .bind(description)
    .fetch_one(pool)
    .await
}

/// Fetch one tag by its stable `slug` (active or merged).
pub async fn get_tag_by_slug(pool: &PgPool, slug: &str) -> Result<Option<TagRow>, sqlx::Error> {
    sqlx::query_as::<_, TagRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {TAG_COLS} FROM tags WHERE slug = $1"
    )))
    .bind(slug)
    .fetch_optional(pool)
    .await
}

/// List tags ordered by name. Active-only (`merged_into IS NULL`) unless
/// `include_merged` is set.
pub async fn list_tags(pool: &PgPool, include_merged: bool) -> Result<Vec<TagRow>, sqlx::Error> {
    sqlx::query_as::<_, TagRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {TAG_COLS} FROM tags
         WHERE ($1::bool OR merged_into IS NULL)
         ORDER BY name"
    )))
    .bind(include_merged)
    .fetch_all(pool)
    .await
}

/// Attach a tag to a work item. Idempotent (`ON CONFLICT DO NOTHING`).
pub async fn tag_work_item(
    pool: &PgPool,
    item_id: i64,
    tag_id: i64,
    tagged_by: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO work_item_tags (item_id, tag_id, tagged_by)
         VALUES ($1, $2, $3)
         ON CONFLICT (item_id, tag_id) DO NOTHING",
    )
    .bind(item_id)
    .bind(tag_id)
    .bind(tagged_by)
    .execute(pool)
    .await?;
    Ok(())
}

/// Detach a tag from a work item. Returns the number of rows removed (0 if the
/// pairing did not exist).
pub async fn untag_work_item(pool: &PgPool, item_id: i64, tag_id: i64) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("DELETE FROM work_item_tags WHERE item_id = $1 AND tag_id = $2")
        .bind(item_id)
        .bind(tag_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// List the active tags currently attached to a work item, ordered by name.
/// Merged tags are excluded (their assignments have been repointed to the
/// destination by [`merge_tags`]).
pub async fn list_item_tags(pool: &PgPool, item_id: i64) -> Result<Vec<TagRow>, sqlx::Error> {
    sqlx::query_as::<_, TagRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {cols} FROM tags t
         JOIN work_item_tags wt ON wt.tag_id = t.id
         WHERE wt.item_id = $1 AND t.merged_into IS NULL
         ORDER BY t.name",
        cols = TAG_COLS
            .split(", ")
            .map(|c| format!("t.{c}"))
            .collect::<Vec<_>>()
            .join(", "),
    )))
    .bind(item_id)
    .fetch_all(pool)
    .await
}

/// Merge the `src` tag into `dst` in one transaction: repoint every
/// `work_item_tags` assignment from `src` to `dst` (dedup-safe via `ON CONFLICT
/// DO NOTHING`), drop the source assignments, and tombstone the source tag
/// (`merged_into = dst`). Returns the count of source assignments that were
/// processed. Both tags must exist (else `sqlx::Error::RowNotFound`). Idempotent
/// on re-run: a second merge repoints zero remaining assignments.
pub async fn merge_tags(pool: &PgPool, src_slug: &str, dst_slug: &str) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Resolve both ids up front; a missing tag aborts before any write.
    let src_id: i64 = sqlx::query_scalar("SELECT id FROM tags WHERE slug = $1")
        .bind(src_slug)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    let dst_id: i64 = sqlx::query_scalar("SELECT id FROM tags WHERE slug = $1")
        .bind(dst_slug)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;

    // Repoint assignments src → dst (skip pairs the item already has on dst).
    let repointed = sqlx::query(
        "INSERT INTO work_item_tags (item_id, tag_id, tagged_by)
         SELECT item_id, $2, tagged_by FROM work_item_tags WHERE tag_id = $1
         ON CONFLICT (item_id, tag_id) DO NOTHING",
    )
    .bind(src_id)
    .bind(dst_id)
    .execute(&mut *tx)
    .await?;

    // Drop the (now-redundant) source assignments.
    let removed = sqlx::query("DELETE FROM work_item_tags WHERE tag_id = $1")
        .bind(src_id)
        .execute(&mut *tx)
        .await?;

    // Tombstone the source tag so old references resolve to dst.
    sqlx::query("UPDATE tags SET merged_into = $2 WHERE id = $1")
        .bind(src_id)
        .bind(dst_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    // The number of items whose tagging was repointed: the rows we deleted from
    // the source (each was an item that carried the source tag), which is the
    // stable count regardless of how many were dedup-skipped on insert.
    Ok(removed.rows_affected().max(repointed.rows_affected()))
}

/// Rename a tag in place (by `slug`); the slug is intentionally left unchanged
/// so existing references survive. Returns the updated row, or `None` if no tag
/// has that slug.
pub async fn rename_tag(
    pool: &PgPool,
    slug: &str,
    new_name: &str,
) -> Result<Option<TagRow>, sqlx::Error> {
    sqlx::query_as::<_, TagRow>(sqlx::AssertSqlSafe(format!(
        "UPDATE tags SET name = $2 WHERE slug = $1 RETURNING {TAG_COLS}"
    )))
    .bind(slug)
    .bind(new_name)
    .fetch_optional(pool)
    .await
}

/// Append a progress note to an item. If `percent` is supplied, the item's
/// `claimed_percent` (the agent's self-reported overall %, shown but NOT
/// trusted for the verified roll-up) is updated in the same transaction.
/// Returns the new progress-row id.
pub async fn insert_progress(
    pool: &PgPool,
    item_id: i64,
    note: &str,
    percent: Option<i16>,
    provenance: &str,
    actor_id: Option<&str>,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let new_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO work_item_progress (item_id, note, percent, provenance, actor_id)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id",
    )
    .bind(item_id)
    .bind(note)
    .bind(percent)
    .bind(provenance)
    .bind(actor_id)
    .fetch_one(&mut *tx)
    .await?;

    if let Some(p) = percent {
        sqlx::query("UPDATE work_items SET claimed_percent = $2, updated_at = NOW() WHERE id = $1")
            .bind(item_id)
            .bind(p)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(new_id)
}

/// List an item's progress notes, newest first, capped by `limit` (clamped
/// 1..=500).
pub async fn list_progress(
    pool: &PgPool,
    item_id: i64,
    limit: i64,
) -> Result<Vec<ProgressRow>, sqlx::Error> {
    let cap = limit.clamp(1, 500);
    sqlx::query_as::<_, ProgressRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {PROGRESS_COLS} FROM work_item_progress
         WHERE item_id = $1
         ORDER BY created_at DESC, id DESC
         LIMIT $2"
    )))
    .bind(item_id)
    .bind(cap)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Relations (item_relations) — the blocks/depends_on DAG, orthogonal to the
// parent_id tree. Phase 9.
// ============================================================================

/// A directed relation between two items (`item_relations`).
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RelationRow {
    pub id: i64,
    pub from_item_id: i64,
    pub to_item_id: i64,
    pub relation_type: String,
    pub created_by: Option<String>,
}

/// Lightweight item identity used when reporting cycles / relations without
/// pulling the full `WorkItemRow`.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ItemMeta {
    pub id: i64,
    pub public_id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
}

/// Insert (or idempotently re-affirm) a typed relation. The `UNIQUE
/// (from_item_id, to_item_id, relation_type)` constraint makes re-linking a
/// no-op that still returns the existing row id. An invalid `relation_type` is
/// rejected by the table CHECK (surfaced as a DB error → `invalid_params`).
pub async fn insert_relation(
    pool: &PgPool,
    from_item_id: i64,
    to_item_id: i64,
    relation_type: &str,
    created_by: Option<&str>,
) -> Result<i64, sqlx::Error> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO item_relations (from_item_id, to_item_id, relation_type, created_by)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (from_item_id, to_item_id, relation_type)
         DO UPDATE SET created_by = COALESCE(EXCLUDED.created_by, item_relations.created_by)
         RETURNING id",
    )
    .bind(from_item_id)
    .bind(to_item_id)
    .bind(relation_type)
    .bind(created_by)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Remove a typed relation. Returns true iff a row was deleted.
pub async fn delete_relation(
    pool: &PgPool,
    from_item_id: i64,
    to_item_id: i64,
    relation_type: &str,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "DELETE FROM item_relations
         WHERE from_item_id = $1 AND to_item_id = $2 AND relation_type = $3",
    )
    .bind(from_item_id)
    .bind(to_item_id)
    .bind(relation_type)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// All relations touching an item (either direction), newest first.
pub async fn list_item_relations(
    pool: &PgPool,
    item_id: i64,
) -> Result<Vec<RelationRow>, sqlx::Error> {
    sqlx::query_as::<_, RelationRow>(
        "SELECT id, from_item_id, to_item_id, relation_type, created_by
           FROM item_relations
          WHERE from_item_id = $1 OR to_item_id = $1
          ORDER BY created_at DESC, id DESC",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await
}

/// The "must-precede" constraint edges `(pre, post)` derived from the ordering
/// relations: `depends_on(a → b)` means b must precede a (edge b→a);
/// `blocks(a → b)` means a must precede b (edge a→b). A directed cycle over
/// these edges is an unschedulable loop. (`relates_to`/`duplicates`/
/// `supersedes`/`derived_from` carry no ordering and are excluded.) Cycle
/// *existence* is direction-invariant, so this mapping is for human-meaningful
/// reporting; the detector would find the same loops either way.
///
/// When `root_id` is `Some`, only edges whose *both* endpoints live in that
/// plan's subtree (`work_items.root_id = root OR id = root`) are returned — the
/// plan-scoped cycle report. `None` returns the whole-workspace schedule graph.
pub async fn fetch_constraint_edges(
    pool: &PgPool,
    root_id: Option<i64>,
) -> Result<Vec<(i64, i64)>, sqlx::Error> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT e.pre, e.post FROM (
            SELECT to_item_id AS pre, from_item_id AS post,
                   from_item_id AS a, to_item_id AS b
              FROM item_relations WHERE relation_type = 'depends_on'
            UNION ALL
            SELECT from_item_id AS pre, to_item_id AS post,
                   from_item_id AS a, to_item_id AS b
              FROM item_relations WHERE relation_type = 'blocks'
         ) e
         WHERE $1::bigint IS NULL OR (
            EXISTS (SELECT 1 FROM work_items w WHERE w.id = e.a AND (w.root_id = $1 OR w.id = $1))
            AND EXISTS (SELECT 1 FROM work_items w WHERE w.id = e.b AND (w.root_id = $1 OR w.id = $1))
         )",
    )
    .bind(root_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Fetch identity/status metadata for a set of item ids (cycle reporting).
pub async fn fetch_items_meta(pool: &PgPool, ids: &[i64]) -> Result<Vec<ItemMeta>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, ItemMeta>(
        "SELECT id, public_id, title, kind, status FROM work_items WHERE id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Code anchors (work_item_code_anchor) — Phase 9b.
// ============================================================================

/// Anchor an item to a code location (file / chunk / symbol). At least one of
/// `file_id`/`chunk_id`/`symbol_id` must be non-NULL (table CHECK).
pub async fn insert_code_anchor(
    pool: &PgPool,
    item_id: i64,
    file_id: Option<i64>,
    chunk_id: Option<i64>,
    symbol_id: Option<i64>,
    anchor_type: &str,
) -> Result<i64, sqlx::Error> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO work_item_code_anchor (item_id, file_id, chunk_id, symbol_id, anchor_type)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id",
    )
    .bind(item_id)
    .bind(file_id)
    .bind(chunk_id)
    .bind(symbol_id)
    .bind(anchor_type)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Resolve a `project:relative/path` or bare path to an `indexed_files.id`
/// (longest-suffix match within the optional project). Returns None if no file
/// matches — the anchor tool turns that into an `invalid_params`.
pub async fn resolve_file_id_by_path(
    pool: &PgPool,
    path: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM indexed_files
          WHERE relative_path = $1 OR relative_path LIKE '%' || $1
          ORDER BY length(relative_path) ASC
          LIMIT 1",
    )
    .bind(path)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}

/// An open `kind='bug'` work-item anchored (via `work_item_code_anchor`) to one
/// of the given paths. Backs the boyscout `pgmcp bug-gate` verify step (ADR-022):
/// it flags bugs the author should fix before pushing changes that touch the same
/// files. "Open" = not yet terminal (`verified`/`cancelled`/`deferred`).
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct OpenBugAnchor {
    pub public_id: String,
    pub title: String,
    pub severity: Option<String>,
    pub status: String,
    pub relative_path: String,
}

/// Find open bugs anchored to any of `paths`. Path matching mirrors
/// [`resolve_file_id_by_path`]'s suffix idiom so a project-relative diff path
/// (`src/foo.rs`) matches an indexed `relative_path` even when the latter carries
/// a longer prefix. Returns at most `limit` rows, ordered by severity then id.
pub async fn open_bugs_anchored_to_paths(
    pool: &PgPool,
    paths: &[String],
    limit: i64,
) -> Result<Vec<OpenBugAnchor>, sqlx::Error> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, OpenBugAnchor>(
        // `sev_rank` is selected (not just used in ORDER BY) because SELECT
        // DISTINCT requires every ORDER BY expression to appear in the select
        // list. It is functionally determined by `severity` (already selected),
        // so it does not change the distinct grouping; `OpenBugAnchor`'s FromRow
        // simply ignores the extra column.
        "SELECT DISTINCT w.public_id, w.title, w.severity, w.status, f.relative_path,
                CASE w.severity WHEN 'critical' THEN 0 WHEN 'high' THEN 1
                                WHEN 'medium' THEN 2 WHEN 'low' THEN 3 ELSE 4 END AS sev_rank
           FROM work_items w
           JOIN work_item_code_anchor a ON a.item_id = w.id
           JOIN indexed_files f ON f.id = a.file_id
          WHERE w.kind = 'bug'
            AND w.status NOT IN ('verified', 'cancelled', 'deferred')
            AND (f.relative_path = ANY($1)
                 OR EXISTS (SELECT 1 FROM unnest($1::text[]) p
                             WHERE f.relative_path LIKE '%' || p))
          ORDER BY sev_rank, w.public_id
          LIMIT $2",
    )
    .bind(paths)
    .bind(limit)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Burndown / velocity (Phase 9e) — read over work_item_status_history.
// ============================================================================

/// Subtree status counts for a burndown snapshot. `total` excludes
/// `cancelled`/`deferred` (a user-deferred subtree neither helps nor hurts).
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct BurndownSummary {
    pub total: i64,
    pub verified: i64,
    pub in_progress: i64,
    pub blocked: i64,
}

/// One day's count of items that reached `verified` (the burndown series).
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct BurndownDay {
    pub day: String,
    pub verified: i64,
}

/// Snapshot the status mix of a plan subtree (`root_id = root OR id = root`).
pub async fn burndown_summary(pool: &PgPool, root_id: i64) -> Result<BurndownSummary, sqlx::Error> {
    sqlx::query_as::<_, BurndownSummary>(
        "SELECT
            count(*) FILTER (WHERE status NOT IN ('cancelled','deferred'))            AS total,
            count(*) FILTER (WHERE status = 'verified')                                AS verified,
            count(*) FILTER (WHERE status IN ('in_progress','claimed_done','verifying')) AS in_progress,
            count(*) FILTER (WHERE status = 'blocked')                                 AS blocked
           FROM work_items
          WHERE root_id = $1 OR id = $1",
    )
    .bind(root_id)
    .fetch_one(pool)
    .await
}

/// Per-day count of `→verified` transitions in the subtree since `since`
/// (the realized burndown / velocity series), oldest first.
pub async fn burndown_series(
    pool: &PgPool,
    root_id: i64,
    since: DateTime<Utc>,
) -> Result<Vec<BurndownDay>, sqlx::Error> {
    sqlx::query_as::<_, BurndownDay>(
        "SELECT to_char(date_trunc('day', h.created_at), 'YYYY-MM-DD') AS day,
                count(*) AS verified
           FROM work_item_status_history h
           JOIN work_items w ON w.id = h.item_id
          WHERE (w.root_id = $1 OR w.id = $1)
            AND h.to_status = 'verified'
            AND h.created_at >= $2
          GROUP BY 1
          ORDER BY 1",
    )
    .bind(root_id)
    .bind(since)
    .fetch_all(pool)
    .await
}

/// All structural rules of a plan definition, in stable insertion order
/// (`plan_definition_export`).
pub async fn list_definition_rules(
    pool: &PgPool,
    definition_id: i64,
) -> Result<Vec<DefinitionRuleRow>, sqlx::Error> {
    sqlx::query_as::<_, DefinitionRuleRow>(
        "SELECT rule_kind, applies_to_kind, child_kind, min_count, max_count,
                field_name, pattern, severity
           FROM definition_rules
          WHERE definition_id = $1
          ORDER BY id ASC",
    )
    .bind(definition_id)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Experiment bridge (Phase 10) — work_item_experiment + verdict→evidence sync.
// The bridge table is created by the guarded late ensure_work_item_experiment_
// bridge migration; these helpers no-op gracefully if it is absent only insofar
// as the SQL would error — callers treat the link tool as requiring the bridge.
// ============================================================================

/// Link a work_item to an experiment (idempotent on the composite PK).
pub async fn link_work_item_experiment(
    pool: &PgPool,
    work_item_id: i64,
    experiment_id: i64,
    hypothesis_id: Option<i64>,
    experiment_slug: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    link_work_item_experiment_in_tx(
        &mut tx,
        work_item_id,
        experiment_id,
        hypothesis_id,
        experiment_slug,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn link_work_item_experiment_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    work_item_id: i64,
    experiment_id: i64,
    hypothesis_id: Option<i64>,
    experiment_slug: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO work_item_experiment
            (work_item_id, experiment_id, hypothesis_id, experiment_slug)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (work_item_id, experiment_id)
         DO UPDATE SET hypothesis_id = EXCLUDED.hypothesis_id,
                       experiment_slug = EXCLUDED.experiment_slug",
    )
    .bind(work_item_id)
    .bind(experiment_id)
    .bind(hypothesis_id)
    .bind(experiment_slug)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The id of an item's `experiment_verdict` acceptance criterion, if one exists.
pub async fn experiment_verdict_criterion_id(
    pool: &PgPool,
    work_item_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let id = experiment_verdict_criterion_id_in_tx(&mut tx, work_item_id).await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn experiment_verdict_criterion_id_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    work_item_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM acceptance_criteria
          WHERE item_id = $1 AND criterion_kind = 'experiment_verdict'
          ORDER BY id ASC LIMIT 1",
    )
    .bind(work_item_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|(id,)| id))
}

/// DISABLED 2026-06-20 — full revert of the experiment→work-item self-verification
/// loophole. `experiment_open`/`record_measurement`/`decide` are agent-callable with
/// NO token and the agent supplies the measurements, so a "frozen" criterion is
/// trivially gamed; this previously posted `source='experiment'` evidence and drove
/// linked bugs →verified via a synthesized `Actor::Gatekeeper` — the success-inflation
/// vector flagged in review. It is now an inert no-op (returns 0): experiments post NO
/// tracker verification evidence — only CI `source='ci'` may flip →verified. (The old
/// docstring claim "the agent cannot fabricate a statistical verdict over a frozen
/// criterion" was false — the agent controls the inputs.) Re-enable ONLY behind a
/// human/CI gate. See docs/reviews/uncommitted-cowboy-changes-2026-06-20.md.
pub async fn sync_experiment_verdict_to_work_items(
    _pool: &PgPool,
    _experiment_id: i64,
    _verdict: &str,
    _detail_json: &str,
) -> Result<u64, sqlx::Error> {
    Ok(0)
    /* ORIGINAL BODY (disabled, preserved per the no-silent-disable mandate):
    let evidence_verdict = match verdict {
        "accepted" => "pass",
        "rejected" => "fail",
        _ => "unknown",
    };
    let links: Vec<(i64,)> =
        sqlx::query_as("SELECT work_item_id FROM work_item_experiment WHERE experiment_id = $1")
            .bind(experiment_id)
            .fetch_all(pool)
            .await?;
    let mut synced = 0u64;
    for (wid,) in links {
        let Some(cid) = experiment_verdict_criterion_id(pool, wid).await? else {
            continue;
        };
        record_verification_evidence(
            pool, cid, evidence_verdict, "experiment",
            None, None, None, Some("pgmcp-stats-engine"), None, None, None, detail_json,
        ).await?;
        synced += 1;
        if evidence_verdict == "pass" {
            corroborate_manual_required_criteria_with_experiment(pool, wid, detail_json).await?;
            drive_work_item_to_verified(pool, wid).await;
        }
    }
    Ok(synced)
    */
}

// DISABLED 2026-06-20 — full revert of the experiment self-verification loophole.
// `corroborate_manual_required_criteria_with_experiment` laundered agent-assertable
// `source='manual'` passes on UNRELATED required criteria into trusted
// `source='experiment'` passes purely so they "no longer strand the item" — pure
// success-inflation with no honest semantics (an experiment about hypothesis H does
// NOT establish that an unrelated manual criterion C is satisfied). This function was
// UNCOMMITTED, so it is preserved here (commented, not deleted) per the
// no-silent-disable mandate. See docs/reviews/uncommitted-cowboy-changes-2026-06-20.md.
/*
async fn corroborate_manual_required_criteria_with_experiment(
    pool: &PgPool,
    work_item_id: i64,
    detail_json: &str,
) -> Result<u64, sqlx::Error> {
    let criteria: Vec<(i64,)> = sqlx::query_as(
        "SELECT ac.id
           FROM acceptance_criteria ac
          WHERE ac.item_id = $1
            AND ac.required
            AND ac.criterion_kind <> 'experiment_verdict'
            AND EXISTS (
                  SELECT 1
                    FROM verification_evidence e
                   WHERE e.criterion_id = ac.id
                     AND e.verdict = 'pass'
                     AND e.source = 'manual')
            AND NOT EXISTS (
                  SELECT 1
                    FROM verification_evidence e
                   WHERE e.criterion_id = ac.id
                     AND e.verdict = 'pass'
                     AND e.source IN ('ci','stop_hook','subagent_audit',
                                      'external_auditor','user_signoff','experiment'))",
    )
    .bind(work_item_id)
    .fetch_all(pool)
    .await?;

    let mut corroborated = 0u64;
    for (criterion_id,) in criteria {
        record_verification_evidence(
            pool, criterion_id, "pass", "experiment",
            None, None, None, Some("pgmcp-stats-engine"), None, None, None, detail_json,
        )
        .await?;
        corroborated += 1;
    }
    Ok(corroborated)
}
*/

// DISABLED 2026-06-20 — full revert (see `sync_experiment_verdict_to_work_items`).
// This drove an experiment-linked item →verified via a synthesized
// `Actor::Gatekeeper` (including DIRECTLY from `Triage`/`Confirmed`), bypassing the
// CI-only trust boundary on agent-controlled experiment evidence. Preserved
// (commented, not deleted) per the no-silent-disable mandate. The matching
// `(Triage,Verified)`/`(Confirmed,Verified)` matrix arms were also reverted in
// `src/tracker/transition.rs`. See docs/reviews/uncommitted-cowboy-changes-2026-06-20.md.
/*
async fn drive_work_item_to_verified(pool: &PgPool, work_item_id: i64) {
    use WorkItemStatus::{
        Blocked, ClaimedDone, Confirmed, InProgress, Pending, Ready, Triage, Verified, Verifying,
    };
    let Ok(Some(row)) = get_work_item(pool, work_item_id).await else {
        return;
    };
    let Some(status) = WorkItemStatus::parse(&row.status) else {
        return;
    };
    let steps: &[(WorkItemStatus, Actor)] = match status {
        Pending | Ready | Blocked => &[(InProgress, Actor::Agent), (Verifying, Actor::Agent)],
        InProgress => &[(Verifying, Actor::Agent)],
        ClaimedDone => &[(Verifying, Actor::System)],
        Triage | Confirmed => &[],
        Verifying => &[],
        Verified => return,
        _ => return,
    };
    for (to, actor) in steps {
        if let Err(e) = set_work_item_status(
            pool, work_item_id, *to, *actor,
            Some("experiment-sync"), Some("experiment verdict"), None, None,
        )
        .await
        {
            tracing::warn!(work_item_id, error = ?e, "experiment-sync: pre-verify transition refused");
            return;
        }
    }
    match latest_passing_evidence_id(pool, work_item_id).await {
        Ok(eid) => {
            if let Err(e) = set_work_item_status(
                pool, work_item_id, Verified, Actor::Gatekeeper,
                Some("pgmcp-stats-engine"), Some("experiment verdict"), eid, None,
            )
            .await
            {
                tracing::warn!(work_item_id, error = ?e, "experiment-sync: gatekeeper verify refused");
            }
        }
        Err(e) => {
            tracing::error!(work_item_id, error = %e, "experiment-sync: evidence lookup failed")
        }
    }
}
*/

// ============================================================================
// Git/PR close-the-loop (work_item_git_links + work_item_finding_provenance)
// — Phase 3. Free functions over &PgPool, the tracker's established design.
// ============================================================================

/// Insert (or idempotently re-affirm) a work-item ↔ repo-artifact link. The
/// `UNIQUE (item_id, link_type, ref_value)` constraint makes a re-scan / re-link
/// a no-op that still returns the existing row id. On conflict the optional
/// `commit_id` and `created_by` are filled in if they were previously NULL
/// (`COALESCE(EXCLUDED.…, existing)`), so a later indexer pass can resolve a
/// `commit_id` for a link first made by `ref_value` alone. Returns
/// `(id, inserted)` — `inserted=true` only for a brand-new row, via the
/// `xmax = 0` trick (the same idiom as [`upsert_ingested_item`]).
#[allow(clippy::too_many_arguments)]
pub async fn insert_git_link(
    pool: &PgPool,
    item_id: i64,
    project_id: Option<i32>,
    link_type: &str,
    ref_value: &str,
    commit_id: Option<i64>,
    detected_by: &str,
    created_by: Option<&str>,
) -> Result<(i64, bool), sqlx::Error> {
    sqlx::query_as::<_, (i64, bool)>(
        "INSERT INTO work_item_git_links
            (item_id, project_id, link_type, ref_value, commit_id, detected_by, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (item_id, link_type, ref_value) DO UPDATE SET
            commit_id  = COALESCE(EXCLUDED.commit_id, work_item_git_links.commit_id),
            project_id = COALESCE(EXCLUDED.project_id, work_item_git_links.project_id),
            created_by = COALESCE(work_item_git_links.created_by, EXCLUDED.created_by)
         RETURNING id, (xmax = 0) AS inserted",
    )
    .bind(item_id)
    .bind(project_id)
    .bind(link_type)
    .bind(ref_value)
    .bind(commit_id)
    .bind(detected_by)
    .bind(created_by)
    .fetch_one(pool)
    .await
}

/// Resolve a commit SHA (full or a unique prefix) to its `git_commits.id` within
/// a project, or `None` if it has not been indexed. Prefix-tolerant: an exact
/// `commit_hash = $2` match wins; otherwise a `LIKE $2 || '%'` prefix match is
/// used when it identifies exactly one commit (an ambiguous prefix yields
/// `None` so a wrong commit is never linked).
pub async fn resolve_commit_id(
    pool: &PgPool,
    project_id: i32,
    sha: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let sha = sha.trim();
    if sha.is_empty() {
        return Ok(None);
    }
    // Exact match first (covers the full-SHA common case cheaply).
    if let Some(id) = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM git_commits WHERE project_id = $1 AND commit_hash = $2 LIMIT 1",
    )
    .bind(project_id)
    .bind(sha)
    .fetch_optional(pool)
    .await?
    {
        return Ok(Some(id));
    }
    // Prefix match — only when it is unambiguous (exactly one commit). LIMIT 2
    // distinguishes "one" from "many" without scanning the whole prefix set.
    let prefix = format!("{}%", sha.replace('%', "\\%").replace('_', "\\_"));
    let matches: Vec<i64> = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM git_commits WHERE project_id = $1 AND commit_hash LIKE $2 LIMIT 2",
    )
    .bind(project_id)
    .bind(&prefix)
    .fetch_all(pool)
    .await?;
    match matches.as_slice() {
        [only] => Ok(Some(*only)),
        _ => Ok(None), // zero or ambiguous (>1) → do not guess
    }
}

/// A code location to anchor an auto-promoted finding to (mirrors the
/// `work_item_code_anchor` columns; at least one of the ids must be non-NULL,
/// enforced by the table CHECK). The findings cron resolves a finding's file
/// path to `file_id` before calling [`promote_finding`].
#[derive(Debug, Default, Clone, Copy)]
pub struct FindingAnchor {
    pub file_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub symbol_id: Option<i64>,
}

impl FindingAnchor {
    fn is_empty(&self) -> bool {
        self.file_id.is_none() && self.chunk_id.is_none() && self.symbol_id.is_none()
    }
}

/// Idempotently promote a finding (a `bug_prediction` defect-prone file or a
/// `documented_tech_debt` marker) into a `pending` work item, keyed by
/// `provenance_key`. The whole operation is one transaction:
///
/// 1. INSERT the provenance row `ON CONFLICT (provenance_key) DO UPDATE SET
///    last_seen_at = now() RETURNING item_id, (xmax = 0)`. If the key already
///    exists (`xmax <> 0`), the existing `item_id` is returned with
///    `created = false` and **no new item is inserted** — re-running the cron
///    never duplicates.
/// 2. On a fresh key, INSERT the work item from `item` (status forced to
///    `pending` by the caller — never pre-`confirmed`), back-patch the
///    provenance row's `item_id` to the new id, and (when an anchor is given)
///    INSERT a `work_item_code_anchor` row.
///
/// Returns `(item_id, created)`. The provenance INSERT is what guarantees
/// idempotency even under a race: the UNIQUE on `provenance_key` makes the
/// second concurrent inserter take the conflict branch.
pub async fn promote_finding(
    pool: &PgPool,
    provenance_key: &str,
    finding_source: &str,
    item: NewWorkItem<'_>,
    anchor: FindingAnchor,
) -> Result<(i64, bool), WorkItemOpError> {
    let mut tx = pool.begin().await?;

    // Step 1: claim the provenance key. A pre-existing key short-circuits with
    // its already-promoted item_id (no dup item). The placeholder item_id 0 on
    // a fresh insert is back-patched in step 2 (FK is deferred to commit via the
    // same tx; we insert the item first to satisfy it — see below).
    //
    // We cannot reference the not-yet-inserted item id in the provenance INSERT,
    // and the provenance.item_id FK is NOT NULL, so the order is: probe the key
    // first; if present, return; else insert the item, then the provenance row.
    let existing: Option<i64> = sqlx::query_scalar::<_, i64>(
        "SELECT item_id FROM work_item_finding_provenance WHERE provenance_key = $1",
    )
    .bind(provenance_key)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(item_id) = existing {
        // Refresh last_seen_at so the ledger reflects the finding still exists,
        // then return the existing item — created=false, no dup.
        sqlx::query(
            "UPDATE work_item_finding_provenance SET last_seen_at = now() WHERE provenance_key = $1",
        )
        .bind(provenance_key)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok((item_id, false));
    }

    // Step 2: fresh finding — insert the item, the provenance row, and the
    // optional code anchor, all in this transaction.
    let new_id: i64 = sqlx::query_scalar::<_, i64>(
        "INSERT INTO work_items
            (public_id, parent_id, project_id, definition_id, root_id, kind, status,
             title, body, priority, weight, parametric, parametric_corpus,
             parametric_expected, origin, created_by, embedding, severity)
         VALUES ($1, $2, $3, $4,
             (SELECT COALESCE(p.root_id, p.id) FROM work_items p WHERE p.id = $2),
             $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
         RETURNING id",
    )
    .bind(item.public_id)
    .bind(item.parent_id)
    .bind(item.project_id)
    .bind(item.definition_id)
    .bind(item.kind)
    .bind(item.status)
    .bind(item.title)
    .bind(item.body)
    .bind(item.priority)
    .bind(item.weight)
    .bind(item.parametric)
    .bind(item.parametric_corpus)
    .bind(item.parametric_expected)
    .bind(item.origin)
    .bind(item.created_by)
    .bind(item.embedding)
    .bind(item.severity)
    .fetch_one(&mut *tx)
    .await?;

    // Provenance row. The UNIQUE(provenance_key) + the ON CONFLICT DO NOTHING
    // guards the race: if a concurrent tx inserted the key between our probe and
    // here, this yields no row — we detect that and fall back to its item_id,
    // rolling back our just-inserted item.
    let prov_inserted: Option<i64> = sqlx::query_scalar::<_, i64>(
        "INSERT INTO work_item_finding_provenance (provenance_key, item_id, finding_source)
         VALUES ($1, $2, $3)
         ON CONFLICT (provenance_key) DO NOTHING
         RETURNING id",
    )
    .bind(provenance_key)
    .bind(new_id)
    .bind(finding_source)
    .fetch_optional(&mut *tx)
    .await?;
    if prov_inserted.is_none() {
        // Lost the race: another tx promoted this finding first. Abandon our
        // item insert (roll back) and return the winner's item_id, created=false.
        tx.rollback().await?;
        let winner: Option<i64> = sqlx::query_scalar::<_, i64>(
            "SELECT item_id FROM work_item_finding_provenance WHERE provenance_key = $1",
        )
        .bind(provenance_key)
        .fetch_optional(pool)
        .await?;
        return match winner {
            Some(item_id) => Ok((item_id, false)),
            // Extremely unlikely (the conflicting row vanished); surface as a
            // not-found so the caller can retry rather than silently dropping.
            None => Err(WorkItemOpError::NotFound),
        };
    }

    // Optional code anchor (file / chunk / symbol). Skipped when empty.
    if !anchor.is_empty() {
        sqlx::query(
            "INSERT INTO work_item_code_anchor (item_id, file_id, chunk_id, symbol_id, anchor_type)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(new_id)
        .bind(anchor.file_id)
        .bind(anchor.chunk_id)
        .bind(anchor.symbol_id)
        .bind("finding")
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok((new_id, true))
}
