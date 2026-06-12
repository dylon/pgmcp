//! Period work-summary engine (`work_summary` MCP tool).
//!
//! Summarizes a time period's work (typically a month) across the git repos in a
//! workspace into deterministic, bullet-pointed, multi-format output. Live git is
//! the authoritative source for every fact (commits, line churn, uncommitted
//! state); the temporal-graph index is consulted only as a freshness-gated
//! enrichment ([`enrich`]). See `docs/decisions` and the boundary spec
//! `docs/formal/tla/WorkSummaryBoundary.tla`.

pub mod enrich;
pub mod git;
pub mod narrative;
pub mod report;
pub mod repos;

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use rmcp::ErrorData as McpError;

use crate::context::SystemContext;
use crate::mcp::server::WorkSummaryParams;
use crate::mcp::tools::sota_helpers::pool_or_err;
use crate::render::ReportFormat;

use enrich::EnrichmentInfo;
use git::Uncommitted;
use repos::Repo;

/// Primary rollup axis for the rendered summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Project,
    Theme,
    Week,
}

impl GroupBy {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "project" | "projects" => Some(GroupBy::Project),
            "theme" | "themes" | "type" => Some(GroupBy::Theme),
            "week" | "weekly" => Some(GroupBy::Week),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            GroupBy::Project => "project",
            GroupBy::Theme => "theme",
            GroupBy::Week => "week",
        }
    }
}

/// Temporal-graph enrichment mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphMode {
    Auto,
    On,
    Off,
}

impl GraphMode {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(GraphMode::Auto),
            "on" | "true" | "yes" => Some(GraphMode::On),
            "off" | "false" | "no" => Some(GraphMode::Off),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            GraphMode::Auto => "auto",
            GraphMode::On => "on",
            GraphMode::Off => "off",
        }
    }
}

/// A fully-normalized, validated, clamped request — the only thing [`summarize`]
/// consumes. Building it ([`WorkSummaryRequest::from_params`]) is the request
/// boundary the TLA⁺ slice models: every reject happens here, before any work.
#[derive(Debug, Clone)]
pub struct WorkSummaryRequest {
    pub workspace_root: String,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    /// `None` = all contributors; `Some(regex)` = case-insensitive `--author`.
    pub author: Option<String>,
    pub author_label: String,
    pub group_by: GroupBy,
    pub include_uncommitted: bool,
    pub use_graph: GraphMode,
    pub narrative: bool,
    pub narrative_backend: String,
    pub narrative_max_tokens: usize,
    pub max_repos: usize,
    pub limit: usize,
    pub format: ReportFormat,
}

impl WorkSummaryRequest {
    /// Validate + normalize + clamp raw params. Returns `invalid_params` for any
    /// malformed input *before* any repo/DB work is done.
    pub fn from_params(ctx: &SystemContext, p: WorkSummaryParams) -> Result<Self, McpError> {
        let cfg = ctx.config().load();
        let wl = &cfg.worklog;

        // Workspace root: explicit, else first configured [workspace] path.
        let raw_root = match p.workspace_root.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => cfg.workspace.paths.first().cloned().ok_or_else(|| {
                McpError::invalid_params(
                    "workspace_root is required (no [workspace] paths configured)",
                    None,
                )
            })?,
        };
        let workspace_root = repos::normalize_dir(&raw_root);
        if workspace_root.is_empty() {
            return Err(McpError::invalid_params(
                "workspace_root must be non-empty",
                None,
            ));
        }

        // Window: explicit since/until win; else `month`; else current UTC month.
        let (since, until) = match (p.since.as_deref(), p.until.as_deref()) {
            (Some(s), Some(u)) => (parse_instant(s, false)?, parse_instant(u, true)?),
            _ => match p.month.as_deref().map(str::trim) {
                Some(m) if !m.is_empty() => parse_month(m)?,
                _ => current_month(),
            },
        };
        if since >= until {
            return Err(McpError::invalid_params(
                "since must be strictly before until",
                None,
            ));
        }

        // Author: omitted → local git user ("my work"); "all" → no filter; else regex.
        let (author, author_label) = match p.author.as_deref().map(str::trim) {
            None => match wl.default_author.as_deref().map(str::trim) {
                Some(a) if a.eq_ignore_ascii_case("all") => (None, "all".to_string()),
                Some(a) if !a.is_empty() => (Some(a.to_string()), a.to_string()),
                _ => {
                    let me = git::resolve_author(&workspace_root);
                    let label = me.clone().unwrap_or_else(|| "all".to_string());
                    (me, label)
                }
            },
            Some("") => return Err(McpError::invalid_params("author must be non-empty", None)),
            Some(a) if a.eq_ignore_ascii_case("all") => (None, "all".to_string()),
            Some(a) => (Some(a.to_string()), a.to_string()),
        };

        // Format: restricted to the renditions this tool implements.
        let format = match p.format.as_deref() {
            None => restricted_format(&wl.default_format)?,
            Some(s) => restricted_format(s)?,
        };

        let group_by = match p.group_by.as_deref() {
            None => GroupBy::Project,
            Some(s) => GroupBy::parse(s).ok_or_else(|| {
                McpError::invalid_params("group_by must be project|theme|week", None)
            })?,
        };
        let use_graph = match p.use_graph.as_deref() {
            None => GraphMode::parse(&wl.graph_enrichment).unwrap_or(GraphMode::Auto),
            Some(s) => GraphMode::parse(s)
                .ok_or_else(|| McpError::invalid_params("use_graph must be auto|on|off", None))?,
        };

        Ok(WorkSummaryRequest {
            workspace_root,
            since,
            until,
            author,
            author_label,
            group_by,
            include_uncommitted: p.include_uncommitted.unwrap_or(true),
            use_graph,
            narrative: p.narrative.unwrap_or(wl.narrative_default),
            narrative_backend: wl.narrative_backend.clone(),
            narrative_max_tokens: wl.narrative_max_tokens as usize,
            max_repos: p.max_repos.unwrap_or(wl.max_repos).clamp(1, 1000) as usize,
            limit: p.limit.unwrap_or(wl.max_projects).clamp(1, 1000) as usize,
            format,
        })
    }
}

// ── Report model (Serialize → the `json` format + the structured envelope) ──

#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkSummaryReport {
    pub workspace_root: String,
    pub since: String,
    pub until: String,
    pub author: String,
    pub group_by: String,
    pub totals: Totals,
    pub projects: Vec<ProjectSummary>,
    pub weeks: Vec<WeekRollup>,
    pub themes: Vec<ThemeRollup>,
    pub normalized: NormalizedParams,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Totals {
    pub commits: u64,
    pub added: u64,
    pub deleted: u64,
    pub projects: usize,
    pub active_days: usize,
    pub busiest_days: Vec<(String, u32)>,
    pub type_mix: Vec<(String, u32)>,
    /// Per-day commit counts across the whole window (for the cadence sparkline).
    pub daily: Vec<(String, u32)>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectSummary {
    pub name: String,
    pub commits: u64,
    pub added: u64,
    pub deleted: u64,
    pub first: Option<String>,
    pub last: Option<String>,
    pub top_scopes: Vec<(String, u32)>,
    pub top_keywords: Vec<(String, u32)>,
    pub samples: Vec<String>,
    pub uncommitted: Option<Uncommitted>,
    pub enrichment: EnrichmentInfo,
    pub narrative: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WeekRollup {
    pub iso_week: String,
    pub commits: u64,
    pub added: u64,
    pub deleted: u64,
    pub projects: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ThemeRollup {
    pub theme: String,
    pub commits: u64,
    pub top_scopes: Vec<(String, u32)>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NormalizedParams {
    pub workspace_root: String,
    pub since: String,
    pub until: String,
    pub author: String,
    pub format: String,
    pub group_by: String,
    pub include_uncommitted: bool,
    pub use_graph: String,
    pub narrative: bool,
    pub max_repos: usize,
    pub limit: usize,
    pub repos_scanned: usize,
    /// Which narrative engine actually ran (`qwen3-4b` etc., or a deterministic
    /// fallback reason). `None` when `narrative=false`.
    pub narrative_engine: Option<String>,
}

#[derive(Default)]
struct WeekAcc {
    commits: u64,
    added: u64,
    deleted: u64,
    projects: HashSet<String>,
}

/// Run the summary. Consumes a validated [`WorkSummaryRequest`]; performs the
/// bounded per-repo live-git reads, folds the cross-cutting rollups, and attaches
/// freshness-gated enrichment.
pub async fn summarize(
    ctx: &SystemContext,
    req: &WorkSummaryRequest,
) -> Result<WorkSummaryReport, McpError> {
    let repos = repos::canonical_repos(ctx, &req.workspace_root, req.max_repos).await?;
    let repos_scanned = repos.len();
    let pool = pool_or_err(ctx)?;

    let since_g = req.since.format("%Y-%m-%d %H:%M:%S").to_string();
    let until_g = req.until.format("%Y-%m-%d %H:%M:%S").to_string();

    let mut g_daily: BTreeMap<NaiveDate, u32> = BTreeMap::new();
    let mut g_type: BTreeMap<String, u32> = BTreeMap::new();
    let mut g_scope: BTreeMap<String, BTreeMap<String, u32>> = BTreeMap::new();
    let mut g_week: BTreeMap<(i32, u32), WeekAcc> = BTreeMap::new();
    let mut projects: Vec<ProjectSummary> = Vec::with_capacity(repos.len());

    for repo in &repos {
        let stats = git::collect_commits(&repo.path, &since_g, &until_g, req.author.as_deref());
        let uncommitted = if req.include_uncommitted {
            Some(git::collect_uncommitted(&repo.path))
        } else {
            None
        };
        let dirty = uncommitted.as_ref().map(|u| u.dirty).unwrap_or(false);
        if stats.commits.is_empty() && !dirty {
            continue; // inactive in window and clean — omit.
        }

        // Fold cross-cutting accumulators (over ALL active repos, pre-truncation).
        for (d, c) in &stats.per_day {
            *g_daily.entry(*d).or_insert(0) += c;
        }
        for (t, c) in &stats.type_counts {
            *g_type.entry(t.clone()).or_insert(0) += c;
        }
        for commit in &stats.commits {
            if let Some(d) = commit.date {
                let w = d.iso_week();
                let acc = g_week.entry((w.year(), w.week())).or_default();
                acc.commits += 1;
                acc.added += commit.added;
                acc.deleted += commit.deleted;
                acc.projects.insert(repo.name.clone());
            }
            if let Some((t, scopes)) = git::parse_conventional(&commit.subject) {
                let bucket = g_scope.entry(t).or_default();
                for s in scopes {
                    *bucket.entry(s).or_insert(0) += 1;
                }
            }
        }

        let enrichment = enrich::enrich(
            pool,
            repo.project_id,
            &repo.path,
            &repo.name,
            req.since,
            req.until,
            req.author.as_deref(),
            stats.commits.len() as u64,
            req.use_graph != GraphMode::Off,
        )
        .await;

        projects.push(build_project_summary(repo, &stats, uncommitted, enrichment));
    }

    // Sort busiest-first; compute totals over the full (pre-truncate) set.
    projects.sort_by(|a, b| b.commits.cmp(&a.commits).then_with(|| a.name.cmp(&b.name)));
    let totals = build_totals(&g_daily, &g_type, &projects, req);
    let weeks = build_weeks(&g_week);
    let themes = build_themes(&g_type, &g_scope);
    projects.truncate(req.limit);

    let narrative_engine = if req.narrative {
        Some(
            narrative::annotate(
                &mut projects,
                req.narrative_backend.clone(),
                req.narrative_max_tokens,
            )
            .await,
        )
    } else {
        None
    };

    Ok(WorkSummaryReport {
        workspace_root: req.workspace_root.clone(),
        since: req.since.to_rfc3339(),
        until: req.until.to_rfc3339(),
        author: req.author_label.clone(),
        group_by: req.group_by.as_str().to_string(),
        totals,
        projects,
        weeks,
        themes,
        normalized: NormalizedParams {
            workspace_root: req.workspace_root.clone(),
            since: req.since.to_rfc3339(),
            until: req.until.to_rfc3339(),
            author: req.author_label.clone(),
            format: req.format.as_str().to_string(),
            group_by: req.group_by.as_str().to_string(),
            include_uncommitted: req.include_uncommitted,
            use_graph: req.use_graph.as_str().to_string(),
            narrative: req.narrative,
            max_repos: req.max_repos,
            limit: req.limit,
            repos_scanned,
            narrative_engine,
        },
    })
}

fn build_project_summary(
    repo: &Repo,
    stats: &git::CommitStats,
    uncommitted: Option<Uncommitted>,
    enrichment: EnrichmentInfo,
) -> ProjectSummary {
    let first = stats
        .commits
        .iter()
        .filter_map(|c| c.date)
        .min()
        .map(|d| d.to_string());
    let last = stats
        .commits
        .iter()
        .filter_map(|c| c.date)
        .max()
        .map(|d| d.to_string());
    ProjectSummary {
        name: repo.name.clone(),
        commits: stats.commits.len() as u64,
        added: stats.added,
        deleted: stats.deleted,
        first,
        last,
        top_scopes: top_n(&stats.scope_counts, 6),
        top_keywords: top_n(&stats.keyword_counts, 8),
        samples: representative_subjects(stats),
        uncommitted,
        enrichment,
        narrative: None,
    }
}

/// First, middle, and last commit subjects (deduped), as representative samples.
fn representative_subjects(stats: &git::CommitStats) -> Vec<String> {
    let n = stats.commits.len();
    if n == 0 {
        return Vec::new();
    }
    let idxs = [0usize, n / 2, n.saturating_sub(1)];
    let mut out: Vec<String> = Vec::with_capacity(3);
    for &i in &idxs {
        let s = stats.commits[i].subject.trim().to_string();
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

fn build_totals(
    daily: &BTreeMap<NaiveDate, u32>,
    type_counts: &BTreeMap<String, u32>,
    projects: &[ProjectSummary],
    req: &WorkSummaryRequest,
) -> Totals {
    let commits = projects.iter().map(|p| p.commits).sum();
    let added = projects.iter().map(|p| p.added).sum();
    let deleted = projects.iter().map(|p| p.deleted).sum();
    let active_projects = projects.iter().filter(|p| p.commits > 0).count();

    let mut busiest: Vec<(String, u32)> = daily.iter().map(|(d, c)| (d.to_string(), *c)).collect();
    busiest.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    busiest.truncate(5);

    Totals {
        commits,
        added,
        deleted,
        projects: active_projects,
        active_days: daily.len(),
        busiest_days: busiest,
        type_mix: top_n(type_counts, 12),
        daily: window_daily(req.since, req.until, daily),
    }
}

/// Day-by-day commit counts across the whole window (0-filled) for the sparkline.
fn window_daily(
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    daily: &BTreeMap<NaiveDate, u32>,
) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut d = since.date_naive();
    let end = until.date_naive();
    // Cap at 366 buckets so a pathological window can't balloon the payload.
    while d < end && out.len() < 366 {
        out.push((d.to_string(), daily.get(&d).copied().unwrap_or(0)));
        d += Duration::days(1);
    }
    out
}

fn build_weeks(g_week: &BTreeMap<(i32, u32), WeekAcc>) -> Vec<WeekRollup> {
    g_week
        .iter()
        .map(|((y, w), acc)| WeekRollup {
            iso_week: format!("{y}-W{w:02}"),
            commits: acc.commits,
            added: acc.added,
            deleted: acc.deleted,
            projects: acc.projects.len(),
        })
        .collect()
}

fn build_themes(
    type_counts: &BTreeMap<String, u32>,
    scopes_by_type: &BTreeMap<String, BTreeMap<String, u32>>,
) -> Vec<ThemeRollup> {
    let mut themes: Vec<ThemeRollup> = type_counts
        .iter()
        .map(|(t, c)| ThemeRollup {
            theme: t.clone(),
            commits: *c as u64,
            top_scopes: scopes_by_type
                .get(t)
                .map(|m| top_n(m, 5))
                .unwrap_or_default(),
        })
        .collect();
    themes.sort_by(|a, b| {
        b.commits
            .cmp(&a.commits)
            .then_with(|| a.theme.cmp(&b.theme))
    });
    themes
}

/// Top-`n` `(key, count)` pairs by count desc, key asc.
fn top_n(map: &BTreeMap<String, u32>, n: usize) -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = map.iter().map(|(k, c)| (k.clone(), *c)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

// ── Date parsing helpers (input-boundary) ───────────────────────────────────

/// Parse `YYYY-MM` → `[first-of-month, first-of-next-month)` (UTC).
fn parse_month(m: &str) -> Result<(DateTime<Utc>, DateTime<Utc>), McpError> {
    let (y, mo) = m
        .split_once('-')
        .and_then(|(y, mo)| Some((y.parse::<i32>().ok()?, mo.parse::<u32>().ok()?)))
        .ok_or_else(|| McpError::invalid_params("month must be 'YYYY-MM'", None))?;
    if !(1..=12).contains(&mo) {
        return Err(McpError::invalid_params(
            "month must be 'YYYY-MM' (01..12)",
            None,
        ));
    }
    let start = Utc
        .with_ymd_and_hms(y, mo, 1, 0, 0, 0)
        .single()
        .ok_or_else(|| McpError::invalid_params("invalid month", None))?;
    let (ny, nmo) = if mo == 12 { (y + 1, 1) } else { (y, mo + 1) };
    let end = Utc
        .with_ymd_and_hms(ny, nmo, 1, 0, 0, 0)
        .single()
        .ok_or_else(|| McpError::invalid_params("invalid month", None))?;
    Ok((start, end))
}

/// Parse a `since`/`until` instant — `YYYY-MM-DD` (midnight UTC) or RFC3339.
fn parse_instant(s: &str, _is_until: bool) -> Result<DateTime<Utc>, McpError> {
    let s = s.trim();
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("valid midnight")));
    }
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| McpError::invalid_params("date must be 'YYYY-MM-DD' or RFC3339", None))
}

/// Parse a format string, restricted to the renditions `work_summary` renders.
fn restricted_format(s: &str) -> Result<ReportFormat, McpError> {
    match ReportFormat::parse(s.trim()) {
        Some(f @ (ReportFormat::Markdown | ReportFormat::Org | ReportFormat::Json)) => Ok(f),
        _ => Err(McpError::invalid_params(
            "format must be markdown|org|json",
            None,
        )),
    }
}

/// `[first-of-this-month, first-of-next-month)` for the current UTC date.
fn current_month() -> (DateTime<Utc>, DateTime<Utc>) {
    let now = Utc::now();
    // `parse_month` round-trips the current (always-valid) year-month.
    parse_month(&format!("{:04}-{:02}", now.year(), now.month()))
        .expect("current month is always valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_month_spans_the_calendar_month() {
        let (s, e) = parse_month("2026-05").expect("valid");
        assert_eq!(s.to_rfc3339(), "2026-05-01T00:00:00+00:00");
        assert_eq!(e.to_rfc3339(), "2026-06-01T00:00:00+00:00");
        // December rolls the year.
        let (_, e2) = parse_month("2026-12").expect("valid");
        assert_eq!(e2.to_rfc3339(), "2027-01-01T00:00:00+00:00");
        assert!(parse_month("2026-13").is_err());
        assert!(parse_month("nope").is_err());
    }

    #[test]
    fn parse_instant_accepts_date_and_rfc3339() {
        assert_eq!(
            parse_instant("2026-05-01", false).unwrap().to_rfc3339(),
            "2026-05-01T00:00:00+00:00"
        );
        assert!(parse_instant("2026-05-01T12:30:00Z", false).is_ok());
        assert!(parse_instant("garbage", false).is_err());
    }

    #[test]
    fn top_n_orders_by_count_then_key() {
        let mut m = BTreeMap::new();
        m.insert("fix".to_string(), 3u32);
        m.insert("feat".to_string(), 3u32);
        m.insert("docs".to_string(), 1u32);
        assert_eq!(
            top_n(&m, 2),
            vec![("feat".to_string(), 3), ("fix".to_string(), 3)]
        );
    }
}
