//! Proactive surfacing — the pgmcp digest (roadmap Phase 4).
//!
//! pgmcp computes a great deal it never proactively shows: overdue tracker
//! items, a growing embedding backlog, a falling engineering GPA. Everything
//! dies as JSON unless an agent polls for it. This module turns pull into push
//! by riding the two channels agents already read:
//!
//! - the **SessionStart** `pgmcp context` CLI ([`crate::cli::context`]), and
//! - the **UserPromptSubmit** observe `additional_context`
//!   ([`crate::api::handlers::session_observe`]).
//!
//! Both call the single composition seam [`compose_digest`], which assembles up
//! to three sections:
//!
//! - **TRACKER** — overdue / blocked / needs-triage / next-actionable counts via
//!   the Phase-2 [`crate::db::queries::list_work_items`] filters.
//! - **HEALTH** — index staleness (`projects.last_scanned_at`), embedding backlog
//!   ([`crate::cron::embedding_migration::full_backlog_counts`]), and — on the
//!   daemon path, where a live [`StatsTracker`] is available — recently-panicked
//!   cron jobs.
//! - **TREND** — Phase-1 GPA slope + forecast
//!   ([`crate::quality::history::gpa_series_since`] +
//!   [`crate::quality::forecast`]), gated on `cfg.include_trends`.
//!
//! `compose_digest` takes `Option<&StatsTracker>`: the daemon (REST) passes
//! `Some` (so HEALTH can include the cron-failure signal); the CLI (`pgmcp
//! context`, which has no live stats) passes `None`.
//!
//! ## Trust boundary (the unifying invariant)
//!
//! The digest is **structurally read-only**. It issues only `SELECT`s for
//! everything it surfaces, plus exactly ONE write: an INSERT into its own
//! `digest_emissions` rate-limit ledger (via [`maybe_emit`]). It performs NO
//! status transitions and constructs no [`crate::tracker::transition::Actor`].
//! `pgmcp-testing/tests/digest_trust_boundary.rs` is a source-grep test that
//! bans `set_work_item_status` and `Actor::` from every file under `src/digest/`,
//! so this property cannot silently regress.
//!
//! ## Delivery seams
//!
//! - [`maybe_emit`] dedupes (by `content_sha256` within `ttl_secs`) and
//!   rate-limits (per-session, `max_per_session`) using the same nudge-gate idiom
//!   as `src/sessions.rs`, then records the emission.
//! - [`webhook::post_webhook`] is the optional outbound POST (daemon-only,
//!   fire-and-forget, min-severity gated, empty-URL default off).
//! - [`notify_digest_ready`] is the `pg_notify('pgmcp_digest', …)` seam, wired
//!   but off by default (`cfg.pg_notify`). No SSE endpoint is built — there is no
//!   consumer in the single-user setup; this is the documented reserved wiring
//!   point. A future consumer would `LISTEN pgmcp_digest`.

pub mod webhook;

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sqlx::PgPool;

use crate::config::DigestConfig;
use crate::stats::tracker::StatsTracker;
use crate::tracker::kind::join_quoted;

// ============================================================================
// Closed vocabularies (ADR-003 closed-enum idiom: ALL + as_str + parse +
// sql_in_list, pinned by a golden test).
// ============================================================================

/// The delivery channel a digest was emitted on. Closed set; the DB CHECK on
/// `digest_emissions.channel` is built from [`sql_in_list`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestChannel {
    /// The SessionStart `pgmcp context` CLI output.
    SessionStart,
    /// The UserPromptSubmit observe `additional_context`.
    Prompt,
    /// An outbound webhook POST (daemon-only, opt-in).
    Webhook,
}

impl DigestChannel {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [DigestChannel] = &[Self::SessionStart, Self::Prompt, Self::Webhook];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::Prompt => "prompt",
            Self::Webhook => "webhook",
        }
    }

    /// Parse a channel from its `as_str` form. A deliberate member of the closed
    /// `DigestChannel` surface (the round-trip partner of `as_str`, exercised by
    /// the golden test); `#[allow(dead_code)]` documents it has no non-test
    /// caller yet — the same idiom as `Severity::rank` in
    /// [`crate::tracker::severity`].
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == s)
    }
}

/// SQL `IN (...)` value list built from [`DigestChannel::ALL`] — the single
/// source of truth shared with the `digest_emissions_channel_check` constraint.
pub fn sql_in_list() -> String {
    join_quoted(DigestChannel::ALL.iter().map(|c| c.as_str()))
}

/// Which subsystem a digest item came from. Drives the section grouping in
/// [`Digest::render_markdown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestCategory {
    /// Work-item tracker state (overdue / blocked / triage / next-actionable).
    Tracker,
    /// pgmcp operational health (index staleness, embedding backlog, cron fail).
    Health,
    /// Quality trajectory (GPA slope / forecast).
    Trend,
    /// Concurrency defects (deadlock cycles, blocked receives, lock contention).
    Concurrency,
    /// Ontology design invariants governing files in scope (read-only surfacing).
    Ontology,
}

impl DigestCategory {
    /// The closed category set. A deliberate member of the public surface (pinned
    /// by the golden test); `#[allow(dead_code)]` documents it has no non-test
    /// caller — `as_str`/`heading` are reached per-item during rendering.
    #[allow(dead_code)]
    pub const ALL: &'static [DigestCategory] = &[
        Self::Tracker,
        Self::Health,
        Self::Trend,
        Self::Concurrency,
        Self::Ontology,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tracker => "tracker",
            Self::Health => "health",
            Self::Trend => "trend",
            Self::Concurrency => "concurrency",
            Self::Ontology => "ontology",
        }
    }

    /// The Markdown section heading for this category.
    fn heading(self) -> &'static str {
        match self {
            Self::Tracker => "Tracker",
            Self::Health => "Health",
            Self::Trend => "Trend",
            Self::Concurrency => "Concurrency",
            Self::Ontology => "Ontology",
        }
    }
}

/// How urgent a digest item is. Ordered (Critical highest); the ordinal is used
/// both to severity-sort the rendered block and to gate the optional webhook
/// (`webhook_min_severity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestSeverity {
    /// FYI; lowest rank.
    Info,
    /// Worth noticing.
    Notice,
    /// Acute — overdue work, large backlog, a regression in flight.
    High,
    /// Severe — e.g. a panicked cron, a metric already past its red line.
    Critical,
}

impl DigestSeverity {
    /// Highest first (the rendering order). A deliberate member of the closed
    /// surface (pinned by the golden test); `#[allow(dead_code)]` documents it
    /// has no non-test caller — comparison goes through the `Ord` derive and the
    /// `parse`/`rank`/`glyph` helpers.
    #[allow(dead_code)]
    pub const ALL: &'static [DigestSeverity] =
        &[Self::Critical, Self::High, Self::Notice, Self::Info];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Notice => "notice",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "info" => Some(Self::Info),
            "notice" => Some(Self::Notice),
            "high" => Some(Self::High),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }

    /// Ordinal rank (Info = 0 … Critical = 3). Derived from the `Ord` derive's
    /// declaration order — kept explicit for the glyph table and webhook gate.
    fn rank(self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Notice => 1,
            Self::High => 2,
            Self::Critical => 3,
        }
    }

    /// A geometric severity glyph (not emoji, per the rendering policy).
    fn glyph(self) -> char {
        match self {
            Self::Info => '·',
            Self::Notice => '▸',
            Self::High => '▲',
            Self::Critical => '◆',
        }
    }
}

// ============================================================================
// Digest model
// ============================================================================

/// One line of the digest: a severity, the subsystem it came from, and the
/// already-rendered human text (no trailing newline).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DigestItem {
    pub severity: DigestSeverity,
    pub category: DigestCategory,
    pub text: String,
}

impl DigestItem {
    fn new(severity: DigestSeverity, category: DigestCategory, text: impl Into<String>) -> Self {
        Self {
            severity,
            category,
            text: text.into(),
        }
    }
}

/// A composed digest — an ordered bag of items. Rendered to a byte-budgeted
/// Markdown block, fingerprinted for dedup, and summarized by max severity.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Digest {
    pub items: Vec<DigestItem>,
}

impl Digest {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The highest severity across all items, or `None` when empty. Drives the
    /// webhook gate.
    pub fn max_severity(&self) -> Option<DigestSeverity> {
        self.items.iter().map(|i| i.severity).max()
    }

    /// A stable fingerprint of the digest content (severity + category + text of
    /// every item, in severity-sorted order so two digests with the same content
    /// in a different insertion order dedupe together). Used by [`maybe_emit`]
    /// for the within-TTL dedup.
    pub fn content_sha256(&self) -> String {
        let mut sorted: Vec<&DigestItem> = self.items.iter().collect();
        sorted.sort_by(Self::item_order);
        let mut hasher = Sha256::new();
        for it in sorted {
            hasher.update(it.severity.as_str().as_bytes());
            hasher.update(b"\x1f");
            hasher.update(it.category.as_str().as_bytes());
            hasher.update(b"\x1f");
            hasher.update(it.text.as_bytes());
            hasher.update(b"\x1e");
        }
        format!("{:x}", hasher.finalize())
    }

    /// Stable total order over items: severity desc, then category, then text —
    /// the same order [`render_markdown`](Self::render_markdown) and
    /// [`content_sha256`](Self::content_sha256) agree on.
    fn item_order(a: &&DigestItem, b: &&DigestItem) -> std::cmp::Ordering {
        b.severity
            .rank()
            .cmp(&a.severity.rank())
            .then_with(|| a.category.as_str().cmp(b.category.as_str()))
            .then_with(|| a.text.cmp(&b.text))
    }

    /// Render a compact, severity-sorted, byte-budgeted Markdown block grouped by
    /// category. Items are emitted highest-severity-first; once appending the
    /// next line would exceed `max_bytes`, rendering stops (so the most urgent
    /// signal always survives truncation). Returns the empty string when the
    /// digest is empty.
    pub fn render_markdown(&self, max_bytes: usize) -> String {
        if self.items.is_empty() {
            return String::new();
        }
        let mut ordered: Vec<&DigestItem> = self.items.iter().collect();
        ordered.sort_by(Self::item_order);

        const HEADER: &str = "## pgmcp digest\n\n";
        // Preallocate: header + ~64 bytes/item, capped at the budget so we never
        // over-reserve for a long item list that the budget will truncate.
        let est = HEADER.len() + ordered.len() * 64;
        let mut out = String::with_capacity(est.min(max_bytes.max(HEADER.len())));
        out.push_str(HEADER);

        let mut current: Option<DigestCategory> = None;
        for it in ordered {
            // A category sub-heading when the group changes.
            let mut prefix = String::new();
            if current != Some(it.category) {
                if current.is_some() {
                    prefix.push('\n');
                }
                let _ = write!(prefix, "### {}\n\n", it.category.heading());
            }
            let line = format!("- {} {}\n", it.severity.glyph(), it.text);
            if out.len() + prefix.len() + line.len() > max_bytes {
                break;
            }
            out.push_str(&prefix);
            out.push_str(&line);
            current = Some(it.category);
        }
        out
    }
}

// ============================================================================
// Composition (all SELECTs; no writes)
// ============================================================================

/// How many items per tracker bucket to count toward the digest. The digest
/// reports counts + the top public_ids, not the full lists, so a small cap keeps
/// the query cheap and the rendered text bounded.
const TRACKER_SAMPLE_LIMIT: i64 = 50;
/// How many leading `public_id`s to name in a bucket's line before eliding.
const TRACKER_NAME_LIMIT: usize = 3;
/// Index-staleness threshold (days) past which a project's `last_scanned_at`
/// raises a Notice.
const STALE_INDEX_DAYS: i64 = 7;
/// Embedding-backlog count above which HEALTH raises a Notice/High.
const BACKLOG_NOTICE: i64 = 1_000;
const BACKLOG_HIGH: i64 = 50_000;
/// Trend window (days) over which the GPA slope is fit.
const TREND_WINDOW_DAYS: i64 = 30;
/// The C-grade boundary an engineering-GPA forecast counts down to.
const GPA_RED_LINE: f64 = 2.0;
/// A trend `pct_change` magnitude (over the window) worth surfacing.
const TREND_PCT_NOTICE: f64 = 5.0;

/// Assemble a [`Digest`] from the live tracker / health / trend signals. All
/// queries are `SELECT`s. `project_id` scopes the tracker + trend sections (and
/// the staleness check) to one project when known; `None` reports across all
/// indexed projects. `stats` carries the daemon's live cron-outcome state (the
/// CLI passes `None`, so HEALTH simply omits the cron-failure signal there).
///
/// Best-effort: any single source that errors (a missing table on a partial
/// install, a transient query failure) is skipped rather than failing the whole
/// digest — the proactive surface should degrade, not break the hook.
pub async fn compose_digest(
    pool: &PgPool,
    project_id: Option<i32>,
    stats: Option<&StatsTracker>,
    cfg: &DigestConfig,
) -> Digest {
    let mut items: Vec<DigestItem> = Vec::with_capacity(8);

    collect_tracker(pool, project_id, &mut items).await;
    collect_health(pool, project_id, stats, &mut items).await;
    if cfg.include_trends {
        collect_trend(pool, project_id, &mut items).await;
    }
    if cfg.include_concurrency {
        collect_concurrency(pool, project_id, &mut items).await;
    }
    if cfg.include_ontology {
        collect_ontology(pool, project_id, &mut items).await;
    }

    Digest { items }
}

/// ONTOLOGY: design invariants governing files in scope — the "constraint
/// surfacing" anti-mistake path. Read-only (a single SELECT). Canonical
/// invariants surface at High severity; non-canonical (e.g. agent-asserted)
/// ones are surfaced too but visibly labeled `(candidate)` and de-emphasized,
/// so unverified assertions are not mistaken for established rules.
async fn collect_ontology(pool: &PgPool, project_id: Option<i32>, out: &mut Vec<DigestItem>) {
    let rows: Result<Vec<(String, Option<String>, String)>, _> = sqlx::query_as(
        "SELECT e.name, m.constraint_text, m.status
         FROM ontology_concept_meta m
         JOIN memory_entities e     ON e.id = m.entity_id AND e.valid_to IS NULL
         JOIN memory_code_anchor a  ON a.entity_id = m.entity_id
         LEFT JOIN file_symbols s   ON s.id = a.symbol_id
         JOIN indexed_files f       ON f.id = COALESCE(a.file_id, s.file_id)
         WHERE m.facet = 'invariant' AND ($1::int IS NULL OR f.project_id = $1)
         GROUP BY e.name, m.constraint_text, m.status
         ORDER BY (m.status = 'canonical') DESC, MAX(m.confidence) DESC, e.name
         LIMIT 8",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await;
    let Ok(rows) = rows else {
        return;
    };
    for (name, constraint, status) in rows {
        let Some(constraint) = constraint else {
            continue;
        };
        let (sev, text) = if status == "canonical" {
            (
                DigestSeverity::High,
                format!("invariant `{name}`: {constraint}"),
            )
        } else {
            (
                DigestSeverity::Notice,
                format!("invariant `{name}` (candidate): {constraint}"),
            )
        };
        out.push(DigestItem::new(sev, DigestCategory::Ontology, text));
    }
}

/// TRACKER: overdue / blocked / needs-triage / next-actionable buckets.
async fn collect_tracker(pool: &PgPool, project_id: Option<i32>, out: &mut Vec<DigestItem>) {
    use crate::db::queries::{WorkItemFilter, list_work_items};

    // (severity, category-label, filter, line-prefix)
    let overdue = WorkItemFilter {
        project_id,
        overdue: true,
        limit: TRACKER_SAMPLE_LIMIT,
        ..Default::default()
    };
    let blocked = WorkItemFilter {
        project_id,
        status: Some("blocked"),
        limit: TRACKER_SAMPLE_LIMIT,
        ..Default::default()
    };
    let triage = WorkItemFilter {
        project_id,
        needs_triage: true,
        limit: TRACKER_SAMPLE_LIMIT,
        ..Default::default()
    };
    let actionable = WorkItemFilter {
        project_id,
        next_actionable: true,
        limit: TRACKER_SAMPLE_LIMIT,
        ..Default::default()
    };

    if let Ok(rows) = list_work_items(pool, &overdue).await
        && !rows.is_empty()
    {
        out.push(DigestItem::new(
            DigestSeverity::High,
            DigestCategory::Tracker,
            tracker_line("overdue", &rows),
        ));
    }
    if let Ok(rows) = list_work_items(pool, &blocked).await
        && !rows.is_empty()
    {
        out.push(DigestItem::new(
            DigestSeverity::Notice,
            DigestCategory::Tracker,
            tracker_line("blocked", &rows),
        ));
    }
    if let Ok(rows) = list_work_items(pool, &triage).await
        && !rows.is_empty()
    {
        out.push(DigestItem::new(
            DigestSeverity::Notice,
            DigestCategory::Tracker,
            tracker_line("awaiting triage", &rows),
        ));
    }
    if let Ok(rows) = list_work_items(pool, &actionable).await
        && !rows.is_empty()
    {
        out.push(DigestItem::new(
            DigestSeverity::Info,
            DigestCategory::Tracker,
            tracker_line("actionable now", &rows),
        ));
    }
}

/// Render a tracker bucket line: "N item(s) <label>: id-a, id-b, id-c +K more".
fn tracker_line(label: &str, rows: &[crate::db::queries::WorkItemRow]) -> String {
    let n = rows.len();
    let mut line = String::with_capacity(32 + n.min(TRACKER_NAME_LIMIT) * 16);
    let _ = write!(line, "{n} item{} {label}", if n == 1 { "" } else { "s" });
    let named: Vec<&str> = rows
        .iter()
        .take(TRACKER_NAME_LIMIT)
        .map(|r| r.public_id.as_str())
        .collect();
    if !named.is_empty() {
        let _ = write!(line, ": {}", named.join(", "));
        if n > named.len() {
            let _ = write!(line, " +{} more", n - named.len());
        }
    }
    line
}

/// HEALTH: index staleness, embedding backlog, recently-panicked crons.
async fn collect_health(
    pool: &PgPool,
    project_id: Option<i32>,
    stats: Option<&StatsTracker>,
    out: &mut Vec<DigestItem>,
) {
    // Embedding backlog (cross-cutting; not project-scoped).
    if let Ok(counts) = crate::cron::embedding_migration::full_backlog_counts(pool).await {
        let total = counts.total();
        if total >= BACKLOG_HIGH {
            out.push(DigestItem::new(
                DigestSeverity::High,
                DigestCategory::Health,
                format!("embedding backlog is large ({total} chunks unembedded)"),
            ));
        } else if total >= BACKLOG_NOTICE {
            out.push(DigestItem::new(
                DigestSeverity::Notice,
                DigestCategory::Health,
                format!("embedding backlog ({total} chunks unembedded)"),
            ));
        }
    }

    // Index staleness via projects.last_scanned_at.
    if let Some(pid) = project_id {
        if let Ok(Some(days)) = days_since_last_scan(pool, pid).await
            && days >= STALE_INDEX_DAYS
        {
            out.push(DigestItem::new(
                DigestSeverity::Notice,
                DigestCategory::Health,
                format!("index is {days}d stale (last scanned {days} day(s) ago)"),
            ));
        }
    } else if let Ok(n) = stale_project_count(pool, STALE_INDEX_DAYS).await
        && n > 0
    {
        out.push(DigestItem::new(
            DigestSeverity::Notice,
            DigestCategory::Health,
            format!("{n} project(s) have a stale index (>{STALE_INDEX_DAYS}d)"),
        ));
    }

    // Cron failures — daemon path only (the CLI has no live StatsTracker).
    if let Some(stats) = stats {
        let mut panicked: Vec<String> = stats
            .last_cron_outcomes
            .iter()
            .filter(|e| e.value().outcome == crate::stats::tracker::CronJobOutcome::Panicked)
            .map(|e| e.key().clone())
            .collect();
        if !panicked.is_empty() {
            panicked.sort();
            out.push(DigestItem::new(
                DigestSeverity::Critical,
                DigestCategory::Health,
                format!("cron job(s) panicked recently: {}", panicked.join(", ")),
            ));
        }
    }

    // Topic-model health (Phase 1 quality gate, `pgmcp_metadata['topics_quality']`).
    // A degenerate scope means discover_topics / topic_hierarchy are unreliable
    // there. Read-only SELECT — consistent with the digest trust boundary.
    if let Some(q) = crate::db::queries::get_topic_quality(pool).await
        && let Some(obj) = q.as_object()
    {
        let mut degenerate: Vec<String> = obj
            .iter()
            .filter(|(_, v)| {
                v.get("distinct_label_ratio")
                    .and_then(|x| x.as_f64())
                    .map(|r| r < 0.3)
                    .unwrap_or(false)
            })
            .map(|(scope, _)| scope.clone())
            .collect();
        if !degenerate.is_empty() {
            degenerate.sort();
            let shown = degenerate
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            out.push(DigestItem::new(
                DigestSeverity::Notice,
                DigestCategory::Health,
                format!(
                    "{} topic scope(s) degenerate (e.g. {shown}); discover_topics unreliable there",
                    degenerate.len()
                ),
            ));
        }
    }
}

/// Whole days since a project was last scanned, or `None` when never scanned.
async fn days_since_last_scan(pool: &PgPool, project_id: i32) -> Result<Option<i64>, sqlx::Error> {
    let days: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (now() - last_scanned_at)) / 86400.0
           FROM projects WHERE id = $1 AND last_scanned_at IS NOT NULL",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await?;
    Ok(days.map(|d| d.floor() as i64))
}

/// Count of projects whose `last_scanned_at` is older than `days` (excludes
/// never-scanned, which are reported elsewhere as freshly-discovered).
async fn stale_project_count(pool: &PgPool, days: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*)::int8 FROM projects
          WHERE last_scanned_at IS NOT NULL
            AND last_scanned_at < now() - ($1::bigint * interval '1 day')",
    )
    .bind(days.max(0))
    .fetch_one(pool)
    .await
}

/// TREND: engineering-GPA slope + projected red-line crossing for one project.
async fn collect_trend(pool: &PgPool, project_id: Option<i32>, out: &mut Vec<DigestItem>) {
    let Some(pid) = project_id else {
        return; // trend is inherently per-project; nothing to say across all
    };
    let series = crate::quality::history::gpa_series_since(pool, pid, TREND_WINDOW_DAYS).await;
    if series.len() < 2 {
        return; // not enough history to fit a trajectory
    }

    let Some(slope) = crate::quality::history::overall_gpa_slope_per_day(&series) else {
        return;
    };
    // Percent change across the window, on the overall GPA endpoints.
    let first = series.iter().find_map(|p| p.overall).map(f64::from);
    let last = series.iter().rev().find_map(|p| p.overall).map(f64::from);
    let pct = match (first, last) {
        (Some(a), Some(b)) => crate::quality::forecast::pct_change(a, b),
        _ => None,
    };

    let latest = match last {
        Some(v) => v,
        None => return,
    };

    // A falling GPA approaching the C-grade floor is the headline trend signal.
    if let Some(weeks) = crate::quality::forecast::weeks_to_threshold(latest, slope, GPA_RED_LINE) {
        let sev = if weeks <= 4.0 {
            DigestSeverity::High
        } else {
            DigestSeverity::Notice
        };
        out.push(DigestItem::new(
            sev,
            DigestCategory::Trend,
            format!(
                "engineering GPA {latest:.2} falling — crosses the C-grade floor ({GPA_RED_LINE:.1}) in ~{weeks:.0}w"
            ),
        ));
    } else if let Some(p) = pct
        && p.abs() >= TREND_PCT_NOTICE
    {
        // Not heading for the red line, but a notable move over the window.
        let dir = if p >= 0.0 { "up" } else { "down" };
        out.push(DigestItem::new(
            DigestSeverity::Info,
            DigestCategory::Trend,
            format!(
                "engineering GPA {dir} {:.0}% over {TREND_WINDOW_DAYS}d (now {latest:.2})",
                p.abs()
            ),
        ));
    }
}

/// CONCURRENCY (ADR-011): open deadlock cycles / blocked receives / channel
/// cycles + trending lock contention, per project. SELECT-only — reads
/// `concurrency_findings` + `concurrency_health_history` (filled by the opt-in
/// concurrency-scan cron); the digest's read-only trust boundary
/// (`pgmcp-testing/tests/digest_trust_boundary.rs`) auto-covers it.
async fn collect_concurrency(pool: &PgPool, project_id: Option<i32>, out: &mut Vec<DigestItem>) {
    let Some(pid) = project_id else {
        return; // per-project, like trend
    };

    // Recently-observed findings per kind (n total, hi = critical/high).
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT finding_kind, COUNT(*) AS n,
                COUNT(*) FILTER (WHERE severity IN ('critical', 'high')) AS hi
         FROM concurrency_findings
         WHERE project_id = $1
           AND finding_kind IN ('deadlock_cycle', 'channel_cycle', 'blocked_recv', 'lock_contention')
           AND observed_at > now() - interval '30 days'
         GROUP BY finding_kind",
    )
    .bind(pid)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    for (kind, n, hi) in &rows {
        if *n == 0 {
            continue;
        }
        let (sev, label) = match kind.as_str() {
            "deadlock_cycle" => (DigestSeverity::Critical, "lock-order deadlock cycle"),
            "channel_cycle" => (DigestSeverity::Critical, "channel deadlock cycle"),
            "blocked_recv" => (DigestSeverity::High, "blocked receive (no producer)"),
            "lock_contention" if *hi > 0 => (DigestSeverity::Notice, "high-contention lock"),
            _ => continue,
        };
        let plural = if *n == 1 { "" } else { "s" };
        out.push(DigestItem::new(
            sev,
            DigestCategory::Concurrency,
            format!("{n} {label}{plural} (observed in the last 30d)"),
        ));
    }

    // Deadlock-cycle-count trajectory — a rising count is the headline trend.
    let series = crate::db::queries::concurrency_metric_series(
        pool,
        pid,
        "deadlock_cycle_count",
        TREND_WINDOW_DAYS as i32,
    )
    .await
    .unwrap_or_default();
    if series.len() >= 2 {
        let points: Vec<(f64, f64)> = series.iter().map(|(da, v)| (-da, *v)).collect();
        let current = series.last().map(|(_, v)| *v).unwrap_or(0.0);
        if let Some(slope) = crate::quality::forecast::ols_slope(&points)
            && slope > 0.0
            && current > 0.0
        {
            out.push(DigestItem::new(
                DigestSeverity::Notice,
                DigestCategory::Concurrency,
                format!(
                    "deadlock-cycle count rising (~{:.1}/week, now {current:.0})",
                    slope * 7.0
                ),
            ));
        }
    }
}

// ============================================================================
// Emission gate + ledger (the digest's ONLY write)
// ============================================================================

/// Dedup + rate-limit a digest emission, recording it on success. Returns
/// `true` when the digest should be delivered on `channel` for `session_id`,
/// `false` when it is suppressed (empty, an identical digest already emitted
/// within `ttl_secs`, or the per-session cap reached).
///
/// On a `true` return this inserts one `digest_emissions` row — the only write
/// the digest subsystem performs. The query helpers live in
/// [`crate::db::queries::digest`].
pub async fn maybe_emit(
    pool: &PgPool,
    session_id: &str,
    channel: DigestChannel,
    project_id: Option<i32>,
    cfg: &DigestConfig,
    digest: &Digest,
) -> bool {
    if digest.is_empty() {
        return false;
    }
    let sha = digest.content_sha256();

    // Dedup: identical content already pushed to this session within the TTL?
    if crate::db::queries::digest::recently_emitted(pool, session_id, &sha, cfg.ttl_secs as i64)
        .await
        .unwrap_or(false)
    {
        return false;
    }
    // Per-session lifetime cap (across channels).
    let count = crate::db::queries::digest::session_emit_count(pool, session_id)
        .await
        .unwrap_or(i64::MAX);
    if count >= cfg.max_per_session as i64 {
        return false;
    }

    // Record the emission (the sole write).
    if let Err(e) = crate::db::queries::digest::insert_digest_emission(
        pool,
        session_id,
        channel.as_str(),
        project_id,
        &sha,
        digest.items.len() as i32,
    )
    .await
    {
        tracing::warn!(error = %e, "insert_digest_emission failed; suppressing emission");
        return false;
    }
    true
}

/// `pg_notify('pgmcp_digest', payload)` seam — the reserved wiring point for a
/// future `LISTEN pgmcp_digest` consumer (e.g. an SSE bridge). No consumer is
/// built in the single-user setup, so callers invoke this only when
/// `cfg.pg_notify` is set (default false). `payload` is a short JSON summary
/// (session, channel, severity, item count, sha) — never the digest body.
pub async fn notify_digest_ready(
    pool: &PgPool,
    session_id: &str,
    channel: DigestChannel,
    digest: &Digest,
) -> Result<(), sqlx::Error> {
    let payload = serde_json::json!({
        "session_id": session_id,
        "channel": channel.as_str(),
        "max_severity": digest.max_severity().map(|s| s.as_str()),
        "item_count": digest.items.len(),
        "content_sha256": digest.content_sha256(),
    })
    .to_string();
    sqlx::query("SELECT pg_notify('pgmcp_digest', $1)")
        .bind(payload)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ── DigestChannel golden test (closed vocab pinned) ──────────────────

    #[test]
    fn digest_channel_vocabulary_is_pinned() {
        let got: HashSet<&str> = DigestChannel::ALL.iter().map(|c| c.as_str()).collect();
        let expected: HashSet<&str> = ["session_start", "prompt", "webhook"].into_iter().collect();
        assert_eq!(
            got, expected,
            "DigestChannel vocabulary drifted from pinned set"
        );
        assert_eq!(DigestChannel::ALL.len(), 3);
        assert_eq!(got.len(), 3, "duplicate as_str() value in DigestChannel");
    }

    #[test]
    fn digest_channel_parse_roundtrips() {
        for c in DigestChannel::ALL {
            assert_eq!(DigestChannel::parse(c.as_str()), Some(*c));
        }
        assert_eq!(DigestChannel::parse("nonsense"), None);
    }

    #[test]
    fn sql_in_list_quotes_every_channel() {
        let s = sql_in_list();
        assert!(s.contains("'session_start'"), "got: {s}");
        assert!(s.contains("'prompt'"));
        assert!(s.contains("'webhook'"));
        assert_eq!(s.matches('\'').count(), DigestChannel::ALL.len() * 2);
        assert_eq!(s.matches(',').count(), DigestChannel::ALL.len() - 1);
    }

    #[test]
    fn severity_is_ordered_and_parses() {
        assert!(DigestSeverity::Critical > DigestSeverity::High);
        assert!(DigestSeverity::High > DigestSeverity::Notice);
        assert!(DigestSeverity::Notice > DigestSeverity::Info);
        for s in DigestSeverity::ALL {
            assert_eq!(DigestSeverity::parse(s.as_str()), Some(*s));
        }
        assert_eq!(DigestSeverity::parse("bogus"), None);
    }

    // ── Digest model behavior ────────────────────────────────────────────

    fn sample() -> Digest {
        Digest {
            items: vec![
                DigestItem::new(DigestSeverity::Info, DigestCategory::Trend, "trend info"),
                DigestItem::new(
                    DigestSeverity::Critical,
                    DigestCategory::Health,
                    "cron panicked: foo",
                ),
                DigestItem::new(
                    DigestSeverity::High,
                    DigestCategory::Tracker,
                    "3 items overdue",
                ),
            ],
        }
    }

    #[test]
    fn max_severity_picks_the_highest() {
        assert_eq!(sample().max_severity(), Some(DigestSeverity::Critical));
        assert_eq!(Digest::default().max_severity(), None);
    }

    #[test]
    fn render_is_severity_sorted_and_grouped() {
        let md = sample().render_markdown(4096);
        assert!(md.starts_with("## pgmcp digest"), "got:\n{md}");
        // Critical (Health) must precede High (Tracker) must precede Info (Trend).
        let crit = md.find("cron panicked").expect("critical present");
        let high = md.find("overdue").expect("high present");
        let info = md.find("trend info").expect("info present");
        assert!(crit < high, "critical before high:\n{md}");
        assert!(high < info, "high before info:\n{md}");
        // Geometric glyphs, not emoji.
        assert!(md.contains('◆'), "critical glyph:\n{md}");
        assert!(md.contains('▲'), "high glyph:\n{md}");
    }

    #[test]
    fn render_respects_the_byte_budget_keeping_the_most_severe() {
        let full = sample().render_markdown(4096);
        // A tight budget: header + only the first (Critical) item should fit.
        let tight = sample().render_markdown(80);
        assert!(
            tight.len() <= 80,
            "over budget: {} bytes\n{tight}",
            tight.len()
        );
        assert!(
            tight.contains("cron panicked"),
            "most-severe item must survive truncation:\n{tight}"
        );
        assert!(
            !tight.contains("trend info"),
            "lowest-severity item must be dropped first:\n{tight}"
        );
        assert!(full.len() > tight.len());
    }

    #[test]
    fn empty_render_is_empty_string() {
        assert!(Digest::default().render_markdown(1024).is_empty());
    }

    #[test]
    fn content_sha_is_order_independent_and_stable() {
        let a = sample();
        let mut reordered = a.clone();
        reordered.items.reverse();
        assert_eq!(
            a.content_sha256(),
            reordered.content_sha256(),
            "sha must be insertion-order independent"
        );
        // A content change flips the sha.
        let mut changed = a.clone();
        changed.items.push(DigestItem::new(
            DigestSeverity::Info,
            DigestCategory::Health,
            "extra",
        ));
        assert_ne!(a.content_sha256(), changed.content_sha256());
        assert_eq!(a.content_sha256().len(), 64);
    }

    #[test]
    fn item_order_is_severity_desc_then_category_then_text() {
        // Two items, same category, differing severity: higher severity sorts
        // first under `item_order` (the order render + sha agree on).
        let hi = DigestItem::new(DigestSeverity::High, DigestCategory::Tracker, "a");
        let lo = DigestItem::new(DigestSeverity::Info, DigestCategory::Tracker, "b");
        let (ra, rb) = (&hi, &lo);
        assert_eq!(Digest::item_order(&ra, &rb), std::cmp::Ordering::Less);
        // Same severity + category: tie-break on text ascending.
        let a = DigestItem::new(DigestSeverity::Info, DigestCategory::Health, "aaa");
        let b = DigestItem::new(DigestSeverity::Info, DigestCategory::Health, "bbb");
        let (ra, rb) = (&a, &b);
        assert_eq!(Digest::item_order(&ra, &rb), std::cmp::Ordering::Less);
    }

    // NOTE: `tracker_line`'s "N item(s) <label>: id-a, id-b +K more" naming +
    // elision contract is exercised end-to-end against real `WorkItemRow`s in
    // `pgmcp-testing/tests/digest_compose_smoke.rs` (which seeds rows through the
    // real query path). `WorkItemRow` has no `Default`, so a hand-rolled unit
    // here would only re-test `format!`, not the function.
}
