//! `target-cleanup` cron — periodic, safe, recoverable disk reclamation.
//!
//! Two phases per run, both **dry-run by default** (`[cron.target_cleanup]
//! dry_run = true`): a manifest of every intended deletion is written and
//! nothing is removed until an operator sets `dry_run = false`.
//!
//! 1. **Rust `target/` reclamation.** Discovers genuine cargo target dirs
//!    (a directory literally named `target` whose parent holds `Cargo.toml`),
//!    classifies each owning project by staleness (last git commit, falling
//!    back to newest source mtime), and applies tiered, recoverable removals.
//!    Tiers 1/2 may be gated on disk pressure (`free_floor_gb`); Tier 0 always
//!    runs; the running daemon's own project is always protected. The tiers:
//!      - **Tier 0** — `target/**/incremental/` scratch (rustc regenerates it
//!        transparently; zero rebuild cost). Runs for every non-stale project.
//!      - **Tier 1** — mtime-trim: regular files under `target/` older than
//!        `active_days`, preserving the recent working set. Active/warm projects.
//!      - **Tier 2** — full `target/` wipe for projects idle longer than
//!        `stale_days`. Recoverable by `cargo build`.
//!
//! 2. **Provenance-first `/tmp` + `/var/tmp` sweep.** Reuses pgmcp's
//!    connected-agent file monitor (`client_file_events`) + agent liveness
//!    (`sessions.last_seen`, `mcp_clients.alive`) as the primary signal: a tmp
//!    file an agent authored is *protected* while that agent is live and
//!    *removed* once it is gone; files with no provenance fall back to a
//!    conservative own-uid + live-process-exclusion + age sweep.
//!
//! ## Safety model
//!
//! Every deletion funnels through one chokepoint ([`safe_remove`]) that
//! re-derives the `*/target` + sibling-`Cargo.toml` invariant, resolves
//! symlinks via `canonicalize`, refuses `$HOME`/`/` and allowlisted roots, and
//! requires the path to live under the specific `target/` it is operating in —
//! so an upstream logic bug fails *closed*. A `/proc/*/{exe,maps,fd}` scan
//! ([`collect_proc_paths`]) skips any artifact a live process holds open —
//! including the daemon's own `(deleted)`-inode binary, which `fuser`/`lsof`
//! by-path checks miss. Build artifacts are gitignored, regeneratable output;
//! recovery is `cargo build`, so age-gated unattended removal is recoverable by
//! construction.
//!
//! Design + rationale: `~/.claude/plans/plan-how-to-periodically-mutable-dragon.md`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, Metadata};
use std::io::{BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::TargetCleanupConfig;

/// Maximum directory depth walked when searching the configured `roots` for
/// `target/` dirs. `~/Workspace/<group>/<project>/target` is depth 4 and a
/// workspace member's `target` is depth 5; 6 leaves headroom while bounding the
/// cost of a stray deep tree.
const ROOT_WALK_MAX_DEPTH: usize = 6;

/// Maximum depth walked *inside* a `target/` (Tier-1 trim, incremental
/// discovery, size) or a tmp dir. Build trees and tmp scratch are shallow;
/// this is a runaway guard, not a tuning knob.
const INNER_WALK_MAX_DEPTH: usize = 32;

// ============================================================================
// Public entry points
// ============================================================================

/// Daemon-facing entry point: run a full sweep and log the summary, swallowing
/// errors so one bad tick never kills the cron thread. Called from the
/// scheduler (`schedule_maintenance_jobs`) on the configured interval. Returns
/// the [`CleanupReport`] so the scheduler can persist its reclamation counts
/// into the `cron_run_history.counters` ledger (see [`CleanupReport::to_counters`]).
pub async fn run_or_log(pool: PgPool, cfg: TargetCleanupConfig) -> CleanupReport {
    let report = run_target_cleanup(&pool, &cfg, None).await;
    report.log_summary();
    report
}

/// Orchestrate one sweep: gather the async DB inputs (project list + tmp
/// provenance + liveness), then run the entire blocking filesystem pass off the
/// async runtime via `spawn_blocking`. Returns the [`CleanupReport`].
pub async fn run_target_cleanup(
    pool: &PgPool,
    cfg: &TargetCleanupConfig,
    project_filter: Option<&str>,
) -> CleanupReport {
    let now = Utc::now();

    // Project roots from pgmcp's own index — every known project's `<path>`.
    // These persist across restarts, so this is populated immediately after a
    // daemon restart (no Ready gate needed).
    let project_candidates: Vec<(PathBuf, String)> = match crate::db::queries::list_projects(pool)
        .await
    {
        Ok(projects) => projects
            .into_iter()
            .map(|p| (PathBuf::from(p.path), p.name))
            .collect(),
        Err(e) => {
            warn!(error = %e, "target-cleanup: list_projects failed; relying on configured roots only");
            Vec::new()
        }
    };

    // Tmp provenance + liveness snapshots (only when the tmp sweep is enabled).
    let (tmp_prov, live_sessions, alive_mcp) = if cfg.sweep_tmp {
        let prov = load_tmp_provenance(pool, &cfg.tmp_dirs).await;
        let live = load_live_sessions(pool, now, cfg.tmp_session_grace_secs).await;
        let alive = load_alive_mcp(pool).await;
        (prov, live, alive)
    } else {
        (HashMap::new(), HashSet::new(), HashSet::new())
    };

    let self_roots = detect_self_project_roots();

    let inputs = PassInputs {
        cfg: cfg.clone(),
        now,
        self_roots,
        project_candidates,
        project_filter: project_filter.map(str::to_string),
        tmp_prov,
        live_sessions,
        alive_mcp,
    };

    match tokio::task::spawn_blocking(move || cleanup_pass(inputs)).await {
        Ok(report) => report,
        Err(e) => {
            error!(error = %e, "target-cleanup: blocking pass panicked");
            CleanupReport::default()
        }
    }
}

// ============================================================================
// Async DB input gathering
// ============================================================================

/// One provenance row (latest event per path) from `client_file_events`.
#[derive(Debug, Clone, sqlx::FromRow)]
struct ProvRow {
    abs_path: String,
    session_id: Option<Uuid>,
    mcp_session_id: Option<String>,
    #[allow(dead_code)]
    source: String,
    ts: DateTime<Utc>,
}

/// The attributing actor + recency for a tmp file with provenance.
#[derive(Debug, Clone)]
struct ProvRecord {
    session_id: Option<Uuid>,
    mcp_session_id: Option<String>,
    ts: DateTime<Utc>,
}

impl From<ProvRow> for ProvRecord {
    fn from(r: ProvRow) -> Self {
        Self {
            session_id: r.session_id,
            mcp_session_id: r.mcp_session_id,
            ts: r.ts,
        }
    }
}

/// Load the latest `client_file_events` row per tmp-dir path into a
/// `path → ProvRecord` map. One bulk `DISTINCT ON` query per run.
async fn load_tmp_provenance(pool: &PgPool, tmp_dirs: &[String]) -> HashMap<PathBuf, ProvRecord> {
    if tmp_dirs.is_empty() {
        return HashMap::new();
    }
    let patterns: Vec<String> = tmp_dirs
        .iter()
        .map(|d| format!("{}/%", d.trim_end_matches('/')))
        .collect();
    let rows: Vec<ProvRow> = match sqlx::query_as::<_, ProvRow>(
        "SELECT DISTINCT ON (abs_path) abs_path, session_id, mcp_session_id, source, ts
         FROM client_file_events
         WHERE abs_path LIKE ANY($1::text[])
         ORDER BY abs_path, ts DESC",
    )
    .bind(&patterns)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %e, "target-cleanup: tmp provenance query failed; tmp sweep falls back to age-only");
            Vec::new()
        }
    };
    let mut map = HashMap::with_capacity(rows.len());
    for r in rows {
        let path = PathBuf::from(&r.abs_path);
        map.insert(path, ProvRecord::from(r));
    }
    map
}

/// Sessions whose `last_seen` is within `grace_secs` of `now` — i.e. still
/// effectively active. Used as hook-sourced provenance liveness.
async fn load_live_sessions(pool: &PgPool, now: DateTime<Utc>, grace_secs: u64) -> HashSet<Uuid> {
    let cutoff = now - Duration::seconds(grace_secs as i64);
    match sqlx::query_scalar::<_, Uuid>("SELECT id FROM sessions WHERE last_seen >= $1")
        .bind(cutoff)
        .fetch_all(pool)
        .await
    {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            error!(error = %e, "target-cleanup: live-sessions query failed");
            HashSet::new()
        }
    }
}

/// MCP client sessions still alive (PID confirmed by the liveness cron). Used
/// as pid-sourced provenance liveness for `ebpf`/`proc_fd` rows.
async fn load_alive_mcp(pool: &PgPool) -> HashSet<String> {
    match sqlx::query_scalar::<_, String>("SELECT mcp_session_id FROM mcp_clients WHERE alive")
        .fetch_all(pool)
        .await
    {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            error!(error = %e, "target-cleanup: alive-mcp query failed");
            HashSet::new()
        }
    }
}

// ============================================================================
// Blocking pass
// ============================================================================

/// Everything the blocking pass needs — owned so the closure is `'static`.
struct PassInputs {
    cfg: TargetCleanupConfig,
    now: DateTime<Utc>,
    self_roots: Vec<PathBuf>,
    project_candidates: Vec<(PathBuf, String)>,
    project_filter: Option<String>,
    tmp_prov: HashMap<PathBuf, ProvRecord>,
    live_sessions: HashSet<Uuid>,
    alive_mcp: HashSet<String>,
}

/// A discovered, validated cargo target directory.
#[derive(Debug, Clone)]
struct TargetDir {
    /// Canonical `<project>/target`.
    target: PathBuf,
    /// Canonical project root (`target`'s parent; holds `Cargo.toml`).
    project_root: PathBuf,
    /// Human project name (DB name or parent basename).
    name: String,
}

/// Project staleness classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Staleness {
    Active,
    Warm,
    Stale,
}

/// The synchronous heart of the cron: discovery → /proc snapshot → tiered
/// target removal → tmp sweep → manifest. Pure of async; safe to `spawn_blocking`.
fn cleanup_pass(inputs: PassInputs) -> CleanupReport {
    let PassInputs {
        cfg,
        now,
        self_roots,
        project_candidates,
        project_filter,
        tmp_prov,
        live_sessions,
        alive_mcp,
    } = inputs;

    let mut report = CleanupReport {
        dry_run: cfg.dry_run,
        ..Default::default()
    };

    // Allowlist of project roots that must never be touched: the configured
    // entries plus every self-anchor of the running daemon (each canonicalized,
    // so the comparison against the canonicalized discovered `project_root`s is
    // exact).
    let mut allowlist: HashSet<PathBuf> = HashSet::new();
    for a in &cfg.allowlist {
        if let Ok(c) = fs::canonicalize(a) {
            allowlist.insert(c);
        }
    }
    for root in &self_roots {
        if let Ok(c) = fs::canonicalize(root) {
            allowlist.insert(c);
        }
    }

    // Discover target dirs (DB project roots ∪ configured-roots walk).
    let targets = discover_targets(&cfg.roots, &project_candidates, project_filter.as_deref());

    // Open the manifest (best-effort: tracing still records on failure).
    let mut manifest = Manifest::create(now, cfg.dry_run);
    report.manifest_path = manifest.path.as_ref().map(|p| p.display().to_string());
    // Bound the audit log: keep only the newest `manifest_keep` manifests. This
    // is housekeeping of the cron's own logs (runs in dry-run and armed modes);
    // critical at a frequent cadence where ~48 manifests/day would otherwise
    // accumulate unbounded in the same filesystem the cron is reclaiming.
    prune_old_manifests(cfg.manifest_keep);

    // Disk-pressure gate for Tiers 1/2 (Tier 0 always runs). `free_floor_gb=0`
    // ⇒ always escalate (the "Moderate" default).
    let reference = targets
        .first()
        .map(|t| t.target.clone())
        .or_else(|| cfg.tmp_dirs.first().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    let escalate = cfg.free_floor_gb == 0
        || crate::health::fs::avail_bytes(&reference)
            .map(|avail| avail < cfg.free_floor_gb.saturating_mul(1 << 30))
            .unwrap_or(true);
    if !escalate {
        debug!(
            free_floor_gb = cfg.free_floor_gb,
            "target-cleanup: free space above floor; running Tier 0 only"
        );
    }

    // /proc snapshot of artifacts held open under any target — catches the
    // daemon's own `(deleted)` binary and any running test/dlopen'd .so.
    let busy_target_paths = collect_proc_paths(|p| p.contains("/target/"));

    let now_str = now.to_rfc3339();
    let active_cutoff = now - Duration::days(cfg.active_days as i64);

    for t in &targets {
        report.targets_scanned += 1;

        if allowlist.contains(&t.project_root) {
            report.targets_skipped_allowlist += 1;
            debug!(project = %t.name, target = %t.target.display(), "target-cleanup: skip (allowlist/self)");
            continue;
        }
        if path_is_busy(&busy_target_paths, &t.target) {
            report.targets_skipped_busy += 1;
            debug!(project = %t.name, target = %t.target.display(), "target-cleanup: skip (busy — open files under target)");
            continue;
        }
        if let Some(newest) = newest_mtime(&t.target)
            && now.timestamp() - newest < (cfg.build_quiet_mins as i64) * 60
        {
            report.targets_skipped_build_quiet += 1;
            debug!(project = %t.name, target = %t.target.display(), "target-cleanup: skip (build-quiet — recently modified)");
            continue;
        }

        let staleness = classify_staleness(
            project_age_days(&t.project_root, now),
            cfg.active_days,
            cfg.stale_days,
        );

        // Record the target's total size for the disk-pressure alert (Part 3),
        // reused there so it need not re-walk the targets.
        report
            .target_sizes
            .push((t.name.clone(), path_size(&t.target)));

        // Reap superseded cargo artifacts (safe in every tier; preserves the
        // live working set). Skip only the stale-escalate arm, which wipes the
        // whole `target/` below and would make this a redundant walk.
        if cfg.reap_superseded && !matches!((staleness, escalate), (Staleness::Stale, true)) {
            let (rb, rf) =
                reap_superseded(t, &cfg, &allowlist, &now_str, &mut manifest, &mut report);
            report.reap_superseded_bytes += rb;
            report.reap_files += rf;
        }

        // Two statements per tier (not `report.x += f(&mut report)`) so the
        // `&mut report` passed into the helper is released before we touch the
        // counter field — otherwise the borrow checker sees report aliased.
        match (staleness, escalate) {
            (Staleness::Stale, true) => {
                let b = safe_remove(
                    &t.target,
                    &t.target,
                    &t.name,
                    "tier2-full",
                    &cfg,
                    &allowlist,
                    &now_str,
                    &mut manifest,
                    &mut report,
                );
                report.tier2_bytes += b;
            }
            (Staleness::Stale, false) => {
                // Conservative under no disk pressure: scratch only.
                let b =
                    tier0_incremental(t, &cfg, &allowlist, &now_str, &mut manifest, &mut report);
                report.tier0_bytes += b;
            }
            (_, true) => {
                let b0 =
                    tier0_incremental(t, &cfg, &allowlist, &now_str, &mut manifest, &mut report);
                report.tier0_bytes += b0;
                let b1 = tier1_trim(
                    t,
                    active_cutoff,
                    &cfg,
                    &allowlist,
                    &now_str,
                    &mut manifest,
                    &mut report,
                );
                report.tier1_bytes += b1;
            }
            (_, false) => {
                let b =
                    tier0_incremental(t, &cfg, &allowlist, &now_str, &mut manifest, &mut report);
                report.tier0_bytes += b;
            }
        }
    }

    // Phase 2: provenance-first tmp sweep.
    if cfg.sweep_tmp {
        sweep_tmp(
            &cfg,
            now,
            &tmp_prov,
            &live_sessions,
            &alive_mcp,
            &now_str,
            &mut manifest,
            &mut report,
        );
    }

    manifest.finish();
    report
}

// ============================================================================
// Discovery
// ============================================================================

/// Discover genuine cargo target dirs from two sources, deduplicated by
/// canonical target path: (1) every indexed project root's `<root>/target`,
/// and (2) a bounded, gitignore-blind walk of each configured `root`.
fn discover_targets(
    roots: &[String],
    project_candidates: &[(PathBuf, String)],
    filter: Option<&str>,
) -> Vec<TargetDir> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut out: Vec<TargetDir> = Vec::new();

    // Source 1: indexed project roots.
    for (root, name) in project_candidates {
        if let Some(td) = genuine_target(root, Some(name)) {
            push_unique(&mut seen, &mut out, td);
        }
    }

    // Source 2: configured-roots walk.
    for root in roots {
        let root = PathBuf::from(root);
        walk_for_targets(&root, ROOT_WALK_MAX_DEPTH, &mut seen, &mut out);
    }

    if let Some(f) = filter {
        out.retain(|t| t.name.contains(f) || t.project_root.to_string_lossy().contains(f));
    }
    out
}

/// If `<project_root>/target` is a genuine cargo target dir (parent holds
/// `Cargo.toml`, `target` is a directory), return the canonicalized [`TargetDir`].
fn genuine_target(project_root: &Path, name: Option<&str>) -> Option<TargetDir> {
    let target = project_root.join("target");
    if !project_root.join("Cargo.toml").is_file() {
        return None;
    }
    let md = fs::symlink_metadata(&target).ok()?;
    if !md.file_type().is_dir() {
        return None; // not a dir, or a symlink — never operate on a symlinked target
    }
    let target = fs::canonicalize(&target).ok()?;
    let project_root = fs::canonicalize(project_root).ok()?;
    // canonicalize could have resolved `target` outside the project (symlink) —
    // require it to still be the project's direct child named `target`.
    if target.parent() != Some(project_root.as_path()) || target.file_name()? != "target" {
        return None;
    }
    let name = name
        .map(str::to_string)
        .or_else(|| {
            project_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| project_root.display().to_string());
    Some(TargetDir {
        target,
        project_root,
        name,
    })
}

/// Bounded, symlink-skipping, gitignore-blind recursive walk that collects
/// `target/` dirs (case-sensitive) with a sibling `Cargo.toml`, pruning descent
/// into any matched `target/` so a nested `target/.../target` cannot appear.
fn walk_for_targets(
    dir: &Path,
    depth: usize,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<TargetDir>,
) {
    if depth == 0 {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let md = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.file_type().is_symlink() || !md.file_type().is_dir() {
            continue;
        }
        if path.file_name().map(|n| n == "target").unwrap_or(false) {
            // Candidate target dir — validate sibling Cargo.toml; prune descent.
            if let Some(td) = genuine_target(dir, None) {
                push_unique(seen, out, td);
            }
            continue; // never recurse into a target/
        }
        walk_for_targets(&path, depth - 1, seen, out);
    }
}

fn push_unique(seen: &mut HashSet<PathBuf>, out: &mut Vec<TargetDir>, td: TargetDir) {
    if seen.insert(td.target.clone()) {
        out.push(td);
    }
}

// ============================================================================
// Staleness
// ============================================================================

/// Days since a project was last active: last git commit time if available,
/// else the newest mtime among its files (excluding `target/`), else a very
/// large number (treated as stale) when nothing can be read.
fn project_age_days(project_root: &Path, now: DateTime<Utc>) -> f64 {
    let last_active =
        git_last_commit_epoch(project_root).or_else(|| newest_source_mtime(project_root));
    match last_active {
        Some(epoch) => ((now.timestamp() - epoch).max(0) as f64) / 86_400.0,
        None => f64::INFINITY,
    }
}

/// `git -C <root> log -1 --format=%ct` → committer epoch seconds.
fn git_last_commit_epoch(project_root: &Path) -> Option<i64> {
    let out = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["log", "-1", "--format=%ct"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse::<i64>().ok()
}

/// Newest mtime among a project's files, **excluding `target/`**, so a fresh
/// rebuild can never make an abandoned project look active. Bounded-depth.
fn newest_source_mtime(project_root: &Path) -> Option<i64> {
    let mut newest: Option<i64> = None;
    walk_source_mtime(project_root, INNER_WALK_MAX_DEPTH, &mut newest);
    newest
}

fn walk_source_mtime(dir: &Path, depth: usize, newest: &mut Option<i64>) {
    if depth == 0 {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let md = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let ft = md.file_type();
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if path.file_name().map(|n| n == "target").unwrap_or(false) {
                continue; // exclude build output
            }
            walk_source_mtime(&path, depth - 1, newest);
        } else if ft.is_file() {
            let m = md.mtime();
            if newest.map(|n| m > n).unwrap_or(true) {
                *newest = Some(m);
            }
        }
    }
}

/// Pure staleness classification — unit-tested without a filesystem.
fn classify_staleness(age_days: f64, active_days: u64, stale_days: u64) -> Staleness {
    if age_days > stale_days as f64 {
        Staleness::Stale
    } else if age_days <= active_days as f64 {
        Staleness::Active
    } else {
        Staleness::Warm
    }
}

// ============================================================================
// Tiered removals
// ============================================================================

/// Tier 0 — remove every `target/**/incremental/` directory (pure rustc scratch).
#[allow(clippy::too_many_arguments)]
fn tier0_incremental(
    t: &TargetDir,
    cfg: &TargetCleanupConfig,
    allowlist: &HashSet<PathBuf>,
    now_str: &str,
    manifest: &mut Manifest,
    report: &mut CleanupReport,
) -> u64 {
    let mut dirs: Vec<PathBuf> = Vec::new();
    collect_named_dirs(&t.target, "incremental", INNER_WALK_MAX_DEPTH, &mut dirs);
    let mut bytes = 0;
    for d in dirs {
        bytes += safe_remove(
            &d,
            &t.target,
            &t.name,
            "tier0-incremental",
            cfg,
            allowlist,
            now_str,
            manifest,
            report,
        );
    }
    bytes
}

/// Tier 1 — remove regular files under `target/` older than `cutoff` (mtime),
/// preserving the recent working set. Recoverable by rebuild.
#[allow(clippy::too_many_arguments)]
fn tier1_trim(
    t: &TargetDir,
    cutoff: DateTime<Utc>,
    cfg: &TargetCleanupConfig,
    allowlist: &HashSet<PathBuf>,
    now_str: &str,
    manifest: &mut Manifest,
    report: &mut CleanupReport,
) -> u64 {
    let cutoff_epoch = cutoff.timestamp();
    let mut old_files: Vec<PathBuf> = Vec::new();
    for_each_file(
        &t.target,
        INNER_WALK_MAX_DEPTH,
        &|_, _| false,
        &mut |path, md| {
            if md.mtime() < cutoff_epoch {
                old_files.push(path.to_path_buf());
            }
        },
    );
    let mut bytes = 0;
    for f in old_files {
        bytes += safe_remove(
            &f,
            &t.target,
            &t.name,
            "tier1-trim",
            cfg,
            allowlist,
            now_str,
            manifest,
            report,
        );
    }
    bytes
}

// ============================================================================
// Superseded-artifact reaper
// ============================================================================
//
// Cargo writes a fresh hash-suffixed copy of every crate's artifacts
// (`lib<crate>-<hash>.rlib`/`.rmeta`, `<crate>-<hash>.d`, bin `<crate>-<hash>`)
// into `target/<profile>/deps/` whenever a unit's identity changes — a toolchain
// bump, a `Cargo.lock` update, a feature/cfg/profile change — and **never** GCs
// the old ones. Over weeks these superseded duplicates dominate a large
// `target/`. The reaper removes them while preserving the live working set, so
// the next `cargo build` stays incremental (it never has to rebuild from
// scratch). A wrong reap costs at worst an incremental recompile of one unit —
// never source loss — because every reaped path is gitignored, regeneratable
// output funnelled through [`safe_remove`]'s fail-closed chokepoint. Unlike
// Tier 2 (which needs a *stale* project) and Tier 1 (whose mtime cutoff skips
// freshly-built files), the reaper runs in every tier, so it reclaims even an
// actively-developed project's huge, freshly-rebuilt `target/debug/`.

/// One cargo compilation unit within a profile's `deps/`: all files sharing the
/// metadata `hash` (the `rlib`/`rmeta`/`d`/bin of a single build of one crate).
struct Unit {
    hash: String,
    /// Crate-identity stem (hash-stripped name of the unit's primary artifact),
    /// used to group a crate's units across rebuilds for the keep-floor.
    stem: String,
    stem_priority: u8,
    /// Newest mtime (epoch secs) across the unit's files.
    mtime: i64,
    size: u64,
    files: Vec<PathBuf>,
}

impl Unit {
    fn new(hash: String) -> Self {
        Self {
            hash,
            stem: String::new(),
            stem_priority: 0,
            mtime: i64::MIN,
            size: 0,
            files: Vec::new(),
        }
    }

    fn add(&mut self, path: PathBuf, mtime: i64, size: u64, priority: u8, stem: String) {
        if mtime > self.mtime {
            self.mtime = mtime;
        }
        self.size += size;
        // The highest-priority artifact (rlib > rmeta > dylib/so > a > bin > d)
        // names the unit's stem, so a crate's lib units group consistently
        // across rebuilds (always via the `lib<crate>` rlib) and never collide
        // with its same-named bin/test units (which derive the bare stem).
        if self.stem.is_empty() || priority > self.stem_priority {
            self.stem_priority = priority;
            self.stem = stem;
        }
        self.files.push(path);
    }
}

/// Artifact-kind ranking for choosing a unit's canonical stem-naming file.
fn type_priority(fname: &str) -> u8 {
    if fname.ends_with(".rlib") {
        6
    } else if fname.ends_with(".rmeta") {
        5
    } else if fname.ends_with(".dylib") || fname.ends_with(".so") {
        4
    } else if fname.ends_with(".a") {
        3
    } else if fname.ends_with(".d") {
        1
    } else {
        2 // no/unknown extension = an executable (bin / test / bench / example)
    }
}

/// Strip a known cargo deps extension, leaving `<stem>-<hash>` (or the whole
/// name for an extensionless executable artifact).
fn strip_dep_ext(name: &str) -> &str {
    for ext in [".rlib", ".rmeta", ".dylib", ".so", ".a", ".d"] {
        if let Some(s) = name.strip_suffix(ext) {
            return s;
        }
    }
    name
}

/// Split a deps/fingerprint/build artifact name into `(stem, Some(hash))` by
/// peeling a known extension then a trailing `-<hex>{8,}` metadata hash. Returns
/// `(name, None)` when no hash is present. Pure — unit-tested without a
/// filesystem.
fn split_unit_hash(name: &str) -> (String, Option<String>) {
    let base = strip_dep_ext(name);
    if let Some(pos) = base.rfind('-') {
        let cand = &base[pos + 1..];
        if cand.len() >= 8 && cand.bytes().all(|b| b.is_ascii_hexdigit()) {
            return (base[..pos].to_string(), Some(cand.to_string()));
        }
    }
    (base.to_string(), None)
}

/// Pure keep/reap decision over candidate units, each given as `(stem, mtime)`.
/// Returns the (ascending) indices to reap. No IO — unit-tested like
/// [`classify_staleness`].
///
/// Rule: let `last_build` be the newest mtime present; every unit within
/// `window_secs` of it is the live working set and is kept. Among the *older*
/// units, a stem that also has an in-window unit was rebuilt this session, so
/// all its older units are superseded and reaped; a stem with **no** in-window
/// unit (an unchanged dependency cargo didn't rewrite) keeps its newest
/// `keep_per_stem` as a safety floor and reaps the rest.
fn select_superseded(items: &[(&str, i64)], keep_per_stem: usize, window_secs: i64) -> Vec<usize> {
    if items.is_empty() {
        return Vec::new();
    }
    let last_build = items
        .iter()
        .map(|&(_, m)| m)
        .max()
        .expect("items non-empty");
    let recent_cutoff = last_build - window_secs;

    let mut stem_has_recent: HashMap<&str, bool> = HashMap::new();
    for &(stem, m) in items {
        let entry = stem_has_recent.entry(stem).or_insert(false);
        if m >= recent_cutoff {
            *entry = true;
        }
    }

    let mut old_by_stem: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, &(stem, m)) in items.iter().enumerate() {
        if m >= recent_cutoff {
            continue; // live working set
        }
        old_by_stem.entry(stem).or_default().push(i);
    }

    let mut reap: Vec<usize> = Vec::new();
    for (stem, mut idxs) in old_by_stem {
        if stem_has_recent.get(stem).copied().unwrap_or(false) {
            reap.extend(idxs); // rebuilt this session → every older unit is dead
        } else {
            idxs.sort_by_key(|&i| std::cmp::Reverse(items[i].1));
            reap.extend(idxs.into_iter().skip(keep_per_stem));
        }
    }
    reap.sort_unstable();
    reap
}

/// Collect the build-profile dirs under a `target/` — every immediate subdir
/// that holds a `deps/` (debug, release, custom profiles), plus one level deeper
/// for cross-compile target-triple dirs (e.g. `x86_64-unknown-linux-gnu/debug`).
/// Symlinks are never followed.
fn collect_profile_dirs(target: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(target) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let Ok(md) = fs::symlink_metadata(&p) else {
            continue;
        };
        if md.file_type().is_symlink() || !md.file_type().is_dir() {
            continue;
        }
        if p.join("deps").is_dir() {
            out.push(p.clone());
        }
        // One level deeper: target-triple dirs hold their own profile subdirs.
        let Ok(sub) = fs::read_dir(&p) else {
            continue;
        };
        for e2 in sub.flatten() {
            let p2 = e2.path();
            let Ok(md2) = fs::symlink_metadata(&p2) else {
                continue;
            };
            if md2.file_type().is_symlink() || !md2.file_type().is_dir() {
                continue;
            }
            if p2.join("deps").is_dir() {
                out.push(p2);
            }
        }
    }
}

/// Reap superseded units within one profile dir: group `deps/` files into units
/// by metadata hash, decide which are superseded ([`select_superseded`]), then
/// remove those units' files plus their hash-matched `.fingerprint/` and
/// `build/` dirs so cargo's three-way cache stays coherent — a unit is left
/// either fully present or fully gone, so cargo cleanly recompiles only what is
/// missing. Returns `(bytes, files)` reclaimed (would-be, under dry-run).
#[allow(clippy::too_many_arguments)]
fn reap_profile(
    prof: &Path,
    t: &TargetDir,
    cfg: &TargetCleanupConfig,
    allowlist: &HashSet<PathBuf>,
    now_str: &str,
    manifest: &mut Manifest,
    report: &mut CleanupReport,
) -> (u64, u64) {
    let deps = prof.join("deps");
    let mut units: HashMap<String, Unit> = HashMap::new();
    if let Ok(entries) = fs::read_dir(&deps) {
        for entry in entries.flatten() {
            let p = entry.path();
            let Ok(md) = fs::symlink_metadata(&p) else {
                continue;
            };
            if !md.file_type().is_file() {
                continue; // deps/ holds files; skip dirs and symlinks
            }
            let Some(fname) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let (stem, Some(hash)) = split_unit_hash(fname) else {
                continue; // only hash-keyed artifacts participate
            };
            units
                .entry(hash.clone())
                .or_insert_with(|| Unit::new(hash))
                .add(p.clone(), md.mtime(), md.len(), type_priority(fname), stem);
        }
    }
    if units.is_empty() {
        return (0, 0);
    }

    let unit_vec: Vec<&Unit> = units.values().collect();
    let items: Vec<(&str, i64)> = unit_vec
        .iter()
        .map(|u| (u.stem.as_str(), u.mtime))
        .collect();
    let reap_idx = select_superseded(&items, cfg.reap_keep_per_stem, cfg.reap_window_secs as i64);

    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut reaped_hashes: HashSet<&str> = HashSet::with_capacity(reap_idx.len());
    for &i in &reap_idx {
        let u = unit_vec[i];
        reaped_hashes.insert(u.hash.as_str());
        for f in &u.files {
            let b = safe_remove(
                f,
                &t.target,
                &t.name,
                "reap-superseded",
                cfg,
                allowlist,
                now_str,
                manifest,
                report,
            );
            if b > 0 {
                bytes += b;
                files += 1;
            }
        }
    }

    // Hash-matched fingerprint + build dirs (coherent with the reaped units).
    for sub in [".fingerprint", "build"] {
        let dir = prof.join(sub);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let Ok(md) = fs::symlink_metadata(&p) else {
                continue;
            };
            if md.file_type().is_symlink() || !md.file_type().is_dir() {
                continue;
            }
            let Some(fname) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let (_stem, Some(hash)) = split_unit_hash(fname) else {
                continue;
            };
            if reaped_hashes.contains(hash.as_str()) {
                let b = safe_remove(
                    &p,
                    &t.target,
                    &t.name,
                    "reap-superseded",
                    cfg,
                    allowlist,
                    now_str,
                    manifest,
                    report,
                );
                if b > 0 {
                    bytes += b;
                    files += 1;
                }
            }
        }
    }
    (bytes, files)
}

/// Reaper entry — remove superseded cargo artifacts across every profile dir of
/// `t.target`, preserving the live working set. See [`reap_profile`] /
/// [`select_superseded`]. Returns `(bytes, files)` reclaimed (would-be, dry-run).
#[allow(clippy::too_many_arguments)]
fn reap_superseded(
    t: &TargetDir,
    cfg: &TargetCleanupConfig,
    allowlist: &HashSet<PathBuf>,
    now_str: &str,
    manifest: &mut Manifest,
    report: &mut CleanupReport,
) -> (u64, u64) {
    let mut profiles: Vec<PathBuf> = Vec::new();
    collect_profile_dirs(&t.target, &mut profiles);
    let mut bytes = 0u64;
    let mut files = 0u64;
    for prof in &profiles {
        let (b, f) = reap_profile(prof, t, cfg, allowlist, now_str, manifest, report);
        bytes += b;
        files += f;
    }
    (bytes, files)
}

// ============================================================================
// The deletion chokepoint
// ============================================================================

/// The single point through which **every** target-side deletion passes. It
/// independently re-derives the safety invariant and refuses anything that
/// fails it, returning the bytes reclaimed (or that *would* be, under dry-run).
///
/// Guards (all must hold): the canonicalized candidate lives under
/// `target_root`; `target_root` is named `target`, its parent holds
/// `Cargo.toml`, and that parent is not allowlisted; the candidate is not `/`,
/// `$HOME`, or shallower than 3 components. A failure logs `DENY` and removes
/// nothing — the chokepoint fails closed.
#[allow(clippy::too_many_arguments)]
fn safe_remove(
    candidate: &Path,
    target_root: &Path,
    project: &str,
    tier: &str,
    cfg: &TargetCleanupConfig,
    allowlist: &HashSet<PathBuf>,
    now_str: &str,
    manifest: &mut Manifest,
    report: &mut CleanupReport,
) -> u64 {
    let real = match fs::canonicalize(candidate) {
        Ok(r) => r,
        Err(_) => return 0, // vanished mid-run — idempotent no-op
    };

    if let Err(reason) = validate_under_target(&real, target_root, allowlist) {
        error!(reason, project, tier, path = %real.display(), "target-cleanup: DENY removal");
        report.errors += 1;
        return 0;
    }

    let bytes = path_size(&real);
    manifest.record(now_str, tier, bytes, project, &real);

    if cfg.dry_run {
        debug!(project, tier, bytes, path = %real.display(), "target-cleanup: DRY-RUN would remove");
        return bytes;
    }

    let result = if real.is_dir() {
        fs::remove_dir_all(&real)
    } else {
        fs::remove_file(&real)
    };
    match result {
        Ok(()) => {
            debug!(project, tier, bytes, path = %real.display(), "target-cleanup: removed");
            bytes
        }
        Err(e) => {
            error!(error = %e, project, tier, path = %real.display(), "target-cleanup: removal failed");
            report.errors += 1;
            0
        }
    }
}

/// Pure invariant check for a target-side removal. `real` must already be
/// canonical. Separated so it is unit-testable against real tempdirs.
fn validate_under_target(
    real: &Path,
    target_root: &Path,
    allowlist: &HashSet<PathBuf>,
) -> Result<(), &'static str> {
    // Never the root, $HOME, or anything shallower than 3 components.
    if real == Path::new("/") {
        return Err("is-root");
    }
    if real.components().count() < 3 {
        return Err("too-shallow");
    }
    if let Some(home) = std::env::var_os("HOME")
        && Path::new(&home) == real
    {
        return Err("is-home");
    }
    // target_root must itself be a genuine `*/target`.
    if target_root.file_name() != Some(std::ffi::OsStr::new("target")) {
        return Err("target-root-not-named-target");
    }
    let project_root = match target_root.parent() {
        Some(p) => p,
        None => return Err("target-root-no-parent"),
    };
    if !project_root.join("Cargo.toml").is_file() {
        return Err("target-root-without-sibling-cargo-toml");
    }
    if allowlist.contains(project_root) {
        return Err("allowlisted-project");
    }
    // The candidate must live within this target_root (or be it).
    if real != target_root && !real.starts_with(target_root) {
        return Err("not-under-target-root");
    }
    Ok(())
}

// ============================================================================
// Provenance-first tmp sweep
// ============================================================================

/// What to do with a tmp file after classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TmpDecision {
    /// Attributed to a still-live agent — never delete.
    ProtectLive,
    /// Attributed, agent gone, past grace — delete (provenance).
    RemoveProvenance,
    /// Attributed, agent gone, still within grace — leave for now.
    WithinGrace,
    /// Unattributed and old enough — delete (age).
    RemoveAge,
    /// Keep (unattributed but fresh / held open / etc.).
    Keep,
}

#[allow(clippy::too_many_arguments)]
fn sweep_tmp(
    cfg: &TargetCleanupConfig,
    now: DateTime<Utc>,
    tmp_prov: &HashMap<PathBuf, ProvRecord>,
    live_sessions: &HashSet<Uuid>,
    alive_mcp: &HashSet<String>,
    now_str: &str,
    manifest: &mut Manifest,
    report: &mut CleanupReport,
) {
    let euid = current_euid();
    // Snapshot of tmp paths any process holds open (fd + maps + exe).
    let canon_dirs: Vec<PathBuf> = cfg
        .tmp_dirs
        .iter()
        .filter_map(|d| fs::canonicalize(d).ok())
        .collect();
    let open_tmp_paths = collect_proc_paths(|p| {
        canon_dirs
            .iter()
            .any(|d| p.starts_with(&*d.to_string_lossy()))
    });

    for dir in &cfg.tmp_dirs {
        let root = match fs::canonicalize(dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let age_days = if dir.trim_end_matches('/').ends_with("var/tmp") {
            cfg.tmp_unattributed_var_age_days
        } else {
            cfg.tmp_unattributed_age_days
        };
        let age_cutoff = now.timestamp() - (age_days as i64) * 86_400;

        // Collect candidate files first (own-uid, not excluded, not socket),
        // so removal never mutates a directory we are iterating.
        let mut candidates: Vec<(PathBuf, Metadata)> = Vec::new();
        for_each_file(
            &root,
            INNER_WALK_MAX_DEPTH,
            &|path, _| is_excluded_tmp_component(path),
            &mut |path, md| {
                if md.uid() == euid && !is_excluded_tmp_path(path) {
                    candidates.push((path.to_path_buf(), md.clone()));
                }
            },
        );

        for (path, md) in candidates {
            let decision = classify_tmp_file(
                &path,
                &md,
                now,
                age_cutoff,
                cfg.tmp_attributed_grace_secs,
                tmp_prov,
                live_sessions,
                alive_mcp,
                &open_tmp_paths,
            );
            match decision {
                TmpDecision::ProtectLive => report.tmp_protected_live += 1,
                TmpDecision::WithinGrace => report.tmp_within_grace += 1,
                TmpDecision::Keep => {}
                TmpDecision::RemoveProvenance => {
                    let n = safe_remove_tmp(
                        &path, &root, euid, "tmp-prov", cfg, now_str, manifest, report,
                    );
                    if n > 0 || cfg.dry_run {
                        report.tmp_prov_bytes += n;
                        report.tmp_files_removed += 1;
                    }
                }
                TmpDecision::RemoveAge => {
                    let n = safe_remove_tmp(
                        &path, &root, euid, "tmp-age", cfg, now_str, manifest, report,
                    );
                    if n > 0 || cfg.dry_run {
                        report.tmp_age_bytes += n;
                        report.tmp_files_removed += 1;
                    }
                }
            }
        }
    }
}

/// Pure tmp-file classification — the provenance-first decision, unit-tested
/// against synthetic provenance/liveness maps.
#[allow(clippy::too_many_arguments)]
fn classify_tmp_file(
    path: &Path,
    md: &Metadata,
    now: DateTime<Utc>,
    age_cutoff_epoch: i64,
    attributed_grace_secs: u64,
    tmp_prov: &HashMap<PathBuf, ProvRecord>,
    live_sessions: &HashSet<Uuid>,
    alive_mcp: &HashSet<String>,
    open_tmp_paths: &HashSet<PathBuf>,
) -> TmpDecision {
    // Provenance lookup: by the walked path and (if different) its canonical form.
    let rec = tmp_prov.get(path).or_else(|| {
        fs::canonicalize(path)
            .ok()
            .and_then(|c| if c == path { None } else { tmp_prov.get(&c) })
    });

    if let Some(rec) = rec {
        if actor_is_live(rec, live_sessions, alive_mcp) {
            return TmpDecision::ProtectLive;
        }
        // Agent gone: respect the post-departure grace before deleting.
        let grace_cutoff = now - Duration::seconds(attributed_grace_secs as i64);
        if rec.ts < grace_cutoff {
            return TmpDecision::RemoveProvenance;
        }
        return TmpDecision::WithinGrace;
    }

    // Unattributed: never touch a file a process holds open; require both mtime
    // and atime older than the threshold.
    if open_tmp_paths.contains(path) {
        return TmpDecision::Keep;
    }
    if md.mtime() < age_cutoff_epoch && md.atime() < age_cutoff_epoch {
        TmpDecision::RemoveAge
    } else {
        TmpDecision::Keep
    }
}

/// True if the provenance actor is still live (hook session or pid client).
fn actor_is_live(
    rec: &ProvRecord,
    live_sessions: &HashSet<Uuid>,
    alive_mcp: &HashSet<String>,
) -> bool {
    if let Some(sid) = rec.session_id
        && live_sessions.contains(&sid)
    {
        return true;
    }
    if let Some(m) = rec.mcp_session_id.as_ref()
        && alive_mcp.contains(m)
    {
        return true;
    }
    false
}

/// Tmp-scoped deletion chokepoint: re-checks the file is under a configured tmp
/// root, owned by us, a regular file, and not name-excluded, before removing.
#[allow(clippy::too_many_arguments)]
fn safe_remove_tmp(
    candidate: &Path,
    tmp_root: &Path,
    euid: u32,
    tier: &str,
    cfg: &TargetCleanupConfig,
    now_str: &str,
    manifest: &mut Manifest,
    report: &mut CleanupReport,
) -> u64 {
    let real = match fs::canonicalize(candidate) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    if !real.starts_with(tmp_root) {
        error!(tier, path = %real.display(), "target-cleanup: DENY tmp removal (outside tmp root)");
        report.errors += 1;
        return 0;
    }
    if is_excluded_tmp_path(&real) {
        return 0;
    }
    let md = match fs::symlink_metadata(&real) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let ft = md.file_type();
    if !ft.is_file() || md.uid() != euid {
        return 0; // only ever remove our own regular files
    }
    let bytes = md.len();
    manifest.record(now_str, tier, bytes, "(tmp)", &real);
    if cfg.dry_run {
        debug!(tier, bytes, path = %real.display(), "target-cleanup: DRY-RUN would remove tmp");
        return bytes;
    }
    match fs::remove_file(&real) {
        Ok(()) => bytes,
        Err(e) => {
            error!(error = %e, tier, path = %real.display(), "target-cleanup: tmp removal failed");
            report.errors += 1;
            0
        }
    }
}

/// Names that must never be swept (live-session sockets/dirs), matched as any
/// path component (exact + prefix forms).
fn is_excluded_tmp_name(name: &str) -> bool {
    const EXACT: &[&str] = &[
        ".X11-unix",
        ".ICE-unix",
        ".XIM-unix",
        ".font-unix",
        ".Xauthority",
        ".gnupg",
        "gnupg",
    ];
    const PREFIX: &[&str] = &[
        "systemd-private-",
        "ssh-",
        "pulse-",
        "pulse",
        "dbus-",
        ".org.chromium.",
    ];
    if EXACT.contains(&name) {
        return true;
    }
    PREFIX.iter().any(|p| name.starts_with(p))
}

/// True if any component of `path` is an excluded tmp name.
fn is_excluded_tmp_path(path: &Path) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .map(is_excluded_tmp_name)
            .unwrap_or(false)
    })
}

/// `skip_dir` predicate for the tmp walker: prune excluded dirs.
fn is_excluded_tmp_component(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(is_excluded_tmp_name)
        .unwrap_or(false)
}

// ============================================================================
// /proc open-file scan
// ============================================================================

/// Collect absolute paths any live process holds open — `/proc/<pid>/exe`
/// (stripping a trailing ` (deleted)`), file-backed `/proc/<pid>/maps` lines,
/// and `/proc/<pid>/fd/*` symlinks — keeping those for which `keep(path)` is
/// true. Catches the daemon's own `(deleted)`-inode binary that by-path
/// `fuser`/`lsof` checks miss.
fn collect_proc_paths(keep: impl Fn(&str) -> bool) -> HashSet<PathBuf> {
    let mut out: HashSet<PathBuf> = HashSet::new();
    let proc = match fs::read_dir("/proc") {
        Ok(p) => p,
        Err(_) => return out,
    };
    for entry in proc.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue; // only numeric PID dirs
        }
        let base = entry.path();

        // exe
        if let Ok(target) = fs::read_link(base.join("exe")) {
            consider_proc_path(&target.to_string_lossy(), &keep, &mut out);
        }
        // maps — file-backed pathnames (6th column).
        if let Ok(contents) = fs::read_to_string(base.join("maps")) {
            for line in contents.lines() {
                if let Some(p) = line.split_whitespace().nth(5)
                    && p.starts_with('/')
                {
                    consider_proc_path(p, &keep, &mut out);
                }
            }
        }
        // fd/* symlinks
        if let Ok(fds) = fs::read_dir(base.join("fd")) {
            for fd in fds.flatten() {
                if let Ok(target) = fs::read_link(fd.path()) {
                    consider_proc_path(&target.to_string_lossy(), &keep, &mut out);
                }
            }
        }
    }
    out
}

fn consider_proc_path(raw: &str, keep: &impl Fn(&str) -> bool, out: &mut HashSet<PathBuf>) {
    let stripped = raw.strip_suffix(" (deleted)").unwrap_or(raw);
    if stripped.starts_with('/') && keep(stripped) {
        out.insert(PathBuf::from(stripped));
    }
}

/// True if any collected busy path is `target` or lives beneath it.
fn path_is_busy(busy: &HashSet<PathBuf>, target: &Path) -> bool {
    busy.iter().any(|p| p == target || p.starts_with(target))
}

// ============================================================================
// Filesystem helpers
// ============================================================================

/// Recursively visit regular files under `dir` (bounded depth, never following
/// symlinks). `skip_dir` prunes whole subtrees. Directory entries are collected
/// before visiting so a visitor may safely delete files.
fn for_each_file(
    dir: &Path,
    depth: usize,
    skip_dir: &dyn Fn(&Path, &Metadata) -> bool,
    visit: &mut dyn FnMut(&Path, &Metadata),
) {
    if depth == 0 {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        paths.push(e.path());
    }
    for path in paths {
        let md = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let ft = md.file_type();
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if skip_dir(&path, &md) {
                continue;
            }
            for_each_file(&path, depth - 1, skip_dir, visit);
        } else if ft.is_file() {
            visit(&path, &md);
        }
        // sockets / fifos / device nodes: ignored
    }
}

/// Collect directories named exactly `name` (bounded depth, no symlinks);
/// matched dirs are not descended into.
fn collect_named_dirs(dir: &Path, name: &str, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let md = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.file_type().is_symlink() || !md.file_type().is_dir() {
            continue;
        }
        if path.file_name().map(|n| n == name).unwrap_or(false) {
            out.push(path);
            continue; // do not recurse into a match
        }
        collect_named_dirs(&path, name, depth - 1, out);
    }
}

/// Size of a path: a file's length, or the recursive sum of file lengths under
/// a directory (no symlink following).
fn path_size(path: &Path) -> u64 {
    let md = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if md.file_type().is_file() {
        return md.len();
    }
    if !md.file_type().is_dir() {
        return 0;
    }
    let mut total = 0u64;
    let mut sum = |_: &Path, m: &Metadata| total += m.len();
    for_each_file(path, INNER_WALK_MAX_DEPTH, &|_, _| false, &mut sum);
    total
}

/// Newest mtime (epoch seconds) of any file under `dir`, or `None` if empty.
fn newest_mtime(dir: &Path) -> Option<i64> {
    let mut newest: Option<i64> = None;
    let mut visit = |_: &Path, m: &Metadata| {
        let mt = m.mtime();
        if newest.map(|n| mt > n).unwrap_or(true) {
            newest = Some(mt);
        }
    };
    for_each_file(dir, INNER_WALK_MAX_DEPTH, &|_, _| false, &mut visit);
    newest
}

// `avail_bytes` lives in `crate::health::fs` (shared with the disk watchdog,
// which also needs the inode axis); the escalate gate above calls it directly.

fn current_euid() -> u32 {
    unsafe { libc::geteuid() }
}

/// Every candidate project root for the running daemon, each unconditionally
/// allowlisted so the daemon never deletes its own build artifacts. We return
/// *all* anchors we can derive because no single one is reliable across launch
/// styles:
///
/// * `CARGO_MANIFEST_DIR` — the source tree this binary was *built* from,
///   embedded at compile time. The only anchor that survives an installed-binary
///   launch (e.g. `~/.local/bin/pgmcp` with cwd `$HOME`), where `current_exe()`
///   has no `target/` ancestor and `current_dir()` is not the project root.
/// * `current_exe()` above `target/` — correct when run straight from
///   `<root>/target/release/pgmcp`.
/// * `current_dir()` — last-resort fallback when launched from the project root.
///
/// Anchors need not exist on this host (a binary built elsewhere harmlessly
/// contributes a non-matching path); the caller canonicalizes each and the
/// allowlist `HashSet` dedups.
fn detect_self_project_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    // Compile-time source root: robust to where the binary is installed/launched.
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));

    // Runtime: the dir above a `target/` ancestor of the executable.
    if let Ok(exe) = std::env::current_exe() {
        for anc in exe.ancestors() {
            if anc.file_name().map(|n| n == "target").unwrap_or(false)
                && let Some(parent) = anc.parent()
            {
                roots.push(parent.to_path_buf());
                break;
            }
        }
    }

    // Last-resort: the daemon's working directory.
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }

    roots
}

// ============================================================================
// Manifest
// ============================================================================

/// Append-only TSV ledger of intended removals (the dry-run review artifact and
/// the post-run recovery list). Best-effort: if the file can't be opened, the
/// run still proceeds and `tracing` records each candidate.
struct Manifest {
    writer: Option<BufWriter<fs::File>>,
    path: Option<PathBuf>,
}

impl Manifest {
    fn create(now: DateTime<Utc>, dry_run: bool) -> Self {
        let Some(dir) = manifest_dir() else {
            error!("target-cleanup: could not resolve state dir; manifest disabled");
            return Self {
                writer: None,
                path: None,
            };
        };
        if let Err(e) = fs::create_dir_all(&dir) {
            error!(error = %e, dir = %dir.display(), "target-cleanup: manifest dir create failed");
            return Self {
                writer: None,
                path: None,
            };
        }
        let path = dir.join(format!("manifest-{}.tsv", now.format("%Y%m%dT%H%M%SZ")));
        match fs::File::create(&path) {
            Ok(file) => {
                let mut writer = BufWriter::new(file);
                let _ = writeln!(
                    writer,
                    "# pgmcp target-cleanup manifest\t{}\tdry_run={}",
                    now.to_rfc3339(),
                    dry_run
                );
                let _ = writeln!(writer, "# ts\ttier\tbytes\tproject\tpath");
                Self {
                    writer: Some(writer),
                    path: Some(path),
                }
            }
            Err(e) => {
                error!(error = %e, path = %path.display(), "target-cleanup: manifest open failed");
                Self {
                    writer: None,
                    path: None,
                }
            }
        }
    }

    fn record(&mut self, ts: &str, tier: &str, bytes: u64, project: &str, path: &Path) {
        if let Some(w) = self.writer.as_mut() {
            let _ = writeln!(w, "{ts}\t{tier}\t{bytes}\t{project}\t{}", path.display());
        }
    }

    fn finish(&mut self) {
        if let Some(w) = self.writer.as_mut() {
            let _ = w.flush();
        }
    }
}

/// `$XDG_STATE_HOME/pgmcp/target-cleanup` (or `$HOME/.local/state/...`).
fn manifest_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("pgmcp").join("target-cleanup"))
}

/// Keep only the newest `keep` `manifest-*.tsv` files in the state dir, pruning
/// the rest so the audit log stays bounded under a frequent cadence (one
/// manifest is written per run — ~48/day at a 30-min interval). `keep == 0`
/// disables pruning. Best-effort housekeeping of the cron's *own* logs, applied
/// in both dry-run and armed modes — it never touches user `target/` data.
fn prune_old_manifests(keep: usize) {
    if let Some(dir) = manifest_dir() {
        prune_manifests_in(&dir, keep);
    }
}

/// Pure core of [`prune_old_manifests`]: prune all but the newest `keep`
/// `manifest-*.tsv` entries directly under `dir`. Filenames embed a sortable
/// UTC timestamp, so lexical order is chronological. A failed unlink is logged
/// and skipped (the run still completes).
fn prune_manifests_in(dir: &Path, keep: usize) {
    if keep == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut manifests: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.file_name().and_then(|n| n.to_str()),
                Some(n) if n.starts_with("manifest-") && n.ends_with(".tsv")
            )
        })
        .collect();
    if manifests.len() <= keep {
        return;
    }
    manifests.sort();
    let cut = manifests.len() - keep;
    for stale in &manifests[..cut] {
        if let Err(e) = fs::remove_file(stale) {
            error!(path = %stale.display(), error = %e, "target-cleanup: manifest prune failed");
        }
    }
}

// ============================================================================
// Report
// ============================================================================

/// Outcome of one cron pass.
#[derive(Debug, Default, Clone)]
pub struct CleanupReport {
    pub dry_run: bool,
    pub targets_scanned: usize,
    pub targets_skipped_busy: usize,
    pub targets_skipped_allowlist: usize,
    pub targets_skipped_build_quiet: usize,
    pub tier0_bytes: u64,
    pub tier1_bytes: u64,
    pub tier2_bytes: u64,
    /// Bytes reclaimed by the superseded-artifact reaper (would-be, under dry-run).
    pub reap_superseded_bytes: u64,
    /// Count of superseded artifact files/dirs reaped (would-be, under dry-run).
    pub reap_files: u64,
    pub tmp_prov_bytes: u64,
    pub tmp_age_bytes: u64,
    pub tmp_files_removed: u64,
    pub tmp_protected_live: u64,
    pub tmp_within_grace: u64,
    /// Per-target total size `(project_name, bytes)` captured during the scan,
    /// surfaced by the disk-pressure alert so it need not re-walk targets.
    pub target_sizes: Vec<(String, u64)>,
    pub manifest_path: Option<String>,
    pub errors: u64,
}

impl CleanupReport {
    pub fn total_bytes(&self) -> u64 {
        self.tier0_bytes
            + self.tier1_bytes
            + self.tier2_bytes
            + self.reap_superseded_bytes
            + self.tmp_prov_bytes
            + self.tmp_age_bytes
    }

    /// Render this pass's reclamation counts as a `cron_run_history.counters`
    /// JSON object, so `cron_history` reflects what was actually reclaimed (per
    /// tier + reaper + tmp, in bytes and files) instead of an empty `{}`. The
    /// keys mirror the `RECLAIMED` log line emitted by [`log_summary`].
    pub fn to_counters(&self) -> serde_json::Value {
        serde_json::json!({
            "dry_run": self.dry_run,
            "targets_scanned": self.targets_scanned,
            "targets_skipped_busy": self.targets_skipped_busy,
            "targets_skipped_allowlist": self.targets_skipped_allowlist,
            "targets_skipped_build_quiet": self.targets_skipped_build_quiet,
            "tier0_bytes": self.tier0_bytes,
            "tier1_bytes": self.tier1_bytes,
            "tier2_bytes": self.tier2_bytes,
            "reap_superseded_bytes": self.reap_superseded_bytes,
            "reap_files": self.reap_files,
            "tmp_prov_bytes": self.tmp_prov_bytes,
            "tmp_age_bytes": self.tmp_age_bytes,
            "tmp_files_removed": self.tmp_files_removed,
            "tmp_protected_live": self.tmp_protected_live,
            "tmp_within_grace": self.tmp_within_grace,
            "total_bytes": self.total_bytes(),
            "errors": self.errors,
        })
    }

    /// Emit the single summary line (the `RECLAIMED …` record).
    pub fn log_summary(&self) {
        info!(
            dry_run = self.dry_run,
            targets = self.targets_scanned,
            skipped_busy = self.targets_skipped_busy,
            skipped_allowlist = self.targets_skipped_allowlist,
            skipped_build_quiet = self.targets_skipped_build_quiet,
            tier0_bytes = self.tier0_bytes,
            tier1_bytes = self.tier1_bytes,
            tier2_bytes = self.tier2_bytes,
            reap_superseded_bytes = self.reap_superseded_bytes,
            reap_files = self.reap_files,
            tmp_prov_bytes = self.tmp_prov_bytes,
            tmp_age_bytes = self.tmp_age_bytes,
            tmp_files_removed = self.tmp_files_removed,
            tmp_protected_live = self.tmp_protected_live,
            tmp_within_grace = self.tmp_within_grace,
            total_bytes = self.total_bytes(),
            errors = self.errors,
            manifest = self.manifest_path.as_deref().unwrap_or("(none)"),
            "target-cleanup RECLAIMED"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // -- dependency-free RAII temp dir ------------------------------------

    static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let p =
                std::env::temp_dir().join(format!("pgmcp-tc-test-{}-{}", std::process::id(), seq));
            fs::create_dir_all(&p).expect("create temp dir");
            TempDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir -p");
        }
        fs::write(path, contents).expect("write file");
    }

    /// Build a minimal cargo project with a populated target/ under `root`.
    fn make_project(root: &Path, name: &str) -> PathBuf {
        let proj = root.join(name);
        write(&proj.join("Cargo.toml"), "[package]\nname=\"x\"\n");
        write(&proj.join("src/lib.rs"), "pub fn f() {}\n");
        write(
            &proj.join("target/debug/incremental/foo/bar.bin"),
            "scratch",
        );
        write(&proj.join("target/debug/deps/libx.rlib"), "artifact");
        write(&proj.join("target/release/x"), "binary");
        proj
    }

    fn cfg_apply() -> TargetCleanupConfig {
        TargetCleanupConfig {
            dry_run: false,
            sweep_tmp: false,
            ..Default::default()
        }
    }

    // -- report ------------------------------------------------------------

    #[test]
    fn to_counters_reports_reclamation_fields() {
        let report = CleanupReport {
            dry_run: false,
            targets_scanned: 30,
            tier0_bytes: 7,
            reap_superseded_bytes: 48,
            reap_files: 2818,
            tmp_prov_bytes: 5,
            tmp_files_removed: 3,
            errors: 0,
            ..Default::default()
        };
        let c = report.to_counters();
        assert_eq!(c["targets_scanned"], 30);
        assert_eq!(c["tier0_bytes"], 7);
        assert_eq!(c["reap_superseded_bytes"], 48);
        assert_eq!(c["reap_files"], 2818);
        // total_bytes folds tier0/1/2 + reap_superseded + tmp_{prov,age}.
        assert_eq!(c["total_bytes"], 7 + 48 + 5);
        assert_eq!(c["dry_run"], false);
        // A populated object, not the empty `{}` this fixes in `cron_history`.
        assert!(c.as_object().is_some_and(|m| !m.is_empty()));
    }

    // -- staleness ---------------------------------------------------------

    #[test]
    fn classify_staleness_boundaries() {
        assert_eq!(classify_staleness(0.0, 14, 60), Staleness::Active);
        assert_eq!(classify_staleness(14.0, 14, 60), Staleness::Active);
        assert_eq!(classify_staleness(14.001, 14, 60), Staleness::Warm);
        assert_eq!(classify_staleness(60.0, 14, 60), Staleness::Warm);
        assert_eq!(classify_staleness(60.001, 14, 60), Staleness::Stale);
        assert_eq!(classify_staleness(f64::INFINITY, 14, 60), Staleness::Stale);
    }

    // -- superseded reaper -------------------------------------------------

    /// Set a path's atime+mtime to `epoch` seconds (test-only helper; libc is
    /// already a dependency — see `current_euid`).
    fn set_mtime(path: &Path, epoch: i64) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("cstring");
        let tv = libc::timeval {
            tv_sec: epoch as libc::time_t,
            tv_usec: 0,
        };
        let times = [tv, tv];
        let rc = unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0, "utimes failed for {}", path.display());
    }

    #[test]
    fn split_unit_hash_parses_cargo_artifact_names() {
        assert_eq!(
            split_unit_hash("libserde-1a2b3c4d5e6f7890.rlib"),
            ("libserde".to_string(), Some("1a2b3c4d5e6f7890".to_string()))
        );
        assert_eq!(
            split_unit_hash("serde-1a2b3c4d5e6f7890.d"),
            ("serde".to_string(), Some("1a2b3c4d5e6f7890".to_string()))
        );
        // Extensionless executable (bin / test / bench).
        assert_eq!(
            split_unit_hash("my_bin-deadbeefdeadbeef"),
            ("my_bin".to_string(), Some("deadbeefdeadbeef".to_string()))
        );
        // Dashes inside the crate name are preserved (hash is the last segment).
        assert_eq!(
            split_unit_hash("proc-macro2-00112233aabbccdd.rmeta"),
            (
                "proc-macro2".to_string(),
                Some("00112233aabbccdd".to_string())
            )
        );
        // No hash → the (ext-stripped) name with None.
        assert_eq!(split_unit_hash("README"), ("README".to_string(), None));
        // A too-short suffix is not a hash.
        assert_eq!(split_unit_hash("foo-abc"), ("foo-abc".to_string(), None));
        // A non-hex suffix is not a hash.
        assert_eq!(
            split_unit_hash("foo-zzzzzzzz"),
            ("foo-zzzzzzzz".to_string(), None)
        );
    }

    #[test]
    fn select_superseded_keeps_live_window() {
        // Everything within the window of the newest → keep all.
        let items = [("libx", 100i64), ("libx", 95), ("liby", 90)];
        assert!(select_superseded(&items, 2, 50).is_empty());
        // Empty input is a no-op.
        assert!(select_superseded(&[], 2, 50).is_empty());
    }

    #[test]
    fn select_superseded_reaps_old_when_stem_rebuilt() {
        // libx has a recent unit (1000) and two old ones (40, 30); the recent
        // build supersedes both old units regardless of the keep floor.
        let items = [("libx", 1000i64), ("libx", 40), ("libx", 30)];
        assert_eq!(select_superseded(&items, 2, 10), vec![1, 2]);
    }

    #[test]
    fn select_superseded_floor_protects_unrebuilt_stem() {
        // libz sets last_build=1000 (recent); liby was not rebuilt this session,
        // so keep its newest 2 old units and reap only the oldest (mtime 30).
        let items = [("libz", 1000i64), ("liby", 50), ("liby", 40), ("liby", 30)];
        assert_eq!(select_superseded(&items, 2, 5), vec![3]);
    }

    #[test]
    fn select_superseded_keep_zero_reaps_all_old() {
        let items = [("libz", 1000i64), ("liby", 50), ("liby", 40)];
        assert_eq!(select_superseded(&items, 0, 5), vec![1, 2]);
    }

    #[test]
    fn select_superseded_window_boundary_is_inclusive() {
        // mtime exactly at last_build - window is kept (>=).
        let items = [("libx", 100i64), ("libx", 90)];
        assert!(select_superseded(&items, 0, 10).is_empty());
        // One second older falls outside the window and is reaped.
        let items2 = [("libx", 100i64), ("libx", 89)];
        assert_eq!(select_superseded(&items2, 0, 10), vec![1]);
    }

    #[test]
    fn reap_superseded_removes_old_units_keeps_live() {
        let tmp = TempDir::new();
        let proj = make_project(tmp.path(), "p");
        let deps = proj.join("target/debug/deps");
        let fp = proj.join("target/debug/.fingerprint");
        let build = proj.join("target/debug/build");
        // Superseded unit (hash aaaa…): rlib + rmeta + dep-info, with matching
        // fingerprint and build dirs.
        write(
            &deps.join("libfoo-aaaaaaaa11111111.rlib"),
            "OLD-RLIB-CONTENT",
        );
        write(&deps.join("libfoo-aaaaaaaa11111111.rmeta"), "OLD-RMETA");
        write(&deps.join("foo-aaaaaaaa11111111.d"), "old depinfo");
        write(&fp.join("foo-aaaaaaaa11111111/lib"), "fp");
        write(
            &build.join("foo-aaaaaaaa11111111/out/generated"),
            "buildout",
        );
        // Live unit (hash bbbb…) = the current working set.
        write(&deps.join("libfoo-bbbbbbbb22222222.rlib"), "NEW-RLIB");
        write(&deps.join("libfoo-bbbbbbbb22222222.rmeta"), "NEW-RMETA");
        write(&deps.join("foo-bbbbbbbb22222222.d"), "new depinfo");
        write(&fp.join("foo-bbbbbbbb22222222/lib"), "fp");

        // Age the superseded unit ~100 days back (well outside the 24h window).
        let old = Utc::now().timestamp() - 100 * 86400;
        for p in [
            deps.join("libfoo-aaaaaaaa11111111.rlib"),
            deps.join("libfoo-aaaaaaaa11111111.rmeta"),
            deps.join("foo-aaaaaaaa11111111.d"),
            fp.join("foo-aaaaaaaa11111111/lib"),
            fp.join("foo-aaaaaaaa11111111"),
            build.join("foo-aaaaaaaa11111111/out/generated"),
            build.join("foo-aaaaaaaa11111111/out"),
            build.join("foo-aaaaaaaa11111111"),
        ] {
            set_mtime(&p, old);
        }

        let td = genuine_target(&proj, Some("p")).unwrap();
        let cfg = cfg_apply();
        let allow = HashSet::new();
        let mut manifest = Manifest {
            writer: None,
            path: None,
        };
        let mut report = CleanupReport::default();
        let (bytes, files) = reap_superseded(&td, &cfg, &allow, "now", &mut manifest, &mut report);

        assert!(bytes > 0, "reaped some bytes");
        assert_eq!(files, 5, "3 deps files + fingerprint dir + build dir");
        // Superseded unit fully gone (deps + fingerprint + build), coherently.
        assert!(!deps.join("libfoo-aaaaaaaa11111111.rlib").exists());
        assert!(!deps.join("libfoo-aaaaaaaa11111111.rmeta").exists());
        assert!(!deps.join("foo-aaaaaaaa11111111.d").exists());
        assert!(!fp.join("foo-aaaaaaaa11111111").exists());
        assert!(!build.join("foo-aaaaaaaa11111111").exists());
        // Live working set preserved → next build stays incremental.
        assert!(deps.join("libfoo-bbbbbbbb22222222.rlib").exists());
        assert!(deps.join("libfoo-bbbbbbbb22222222.rmeta").exists());
        assert!(fp.join("foo-bbbbbbbb22222222").exists());
        // Source untouched — the recoverability invariant.
        assert!(proj.join("src/lib.rs").exists());
    }

    #[test]
    fn reap_superseded_dry_run_keeps_files() {
        let tmp = TempDir::new();
        let proj = make_project(tmp.path(), "p");
        let deps = proj.join("target/debug/deps");
        write(&deps.join("libfoo-aaaaaaaa11111111.rlib"), "OLD");
        write(&deps.join("libfoo-bbbbbbbb22222222.rlib"), "NEW");
        set_mtime(
            &deps.join("libfoo-aaaaaaaa11111111.rlib"),
            Utc::now().timestamp() - 100 * 86400,
        );
        let td = genuine_target(&proj, Some("p")).unwrap();
        let cfg = TargetCleanupConfig {
            dry_run: true,
            sweep_tmp: false,
            ..Default::default()
        };
        let allow = HashSet::new();
        let mut manifest = Manifest {
            writer: None,
            path: None,
        };
        let mut report = CleanupReport::default();
        let (bytes, _files) = reap_superseded(&td, &cfg, &allow, "now", &mut manifest, &mut report);
        assert!(bytes > 0, "dry-run reports would-be bytes");
        assert!(
            deps.join("libfoo-aaaaaaaa11111111.rlib").exists(),
            "dry-run deleted nothing"
        );
    }

    // -- discovery ---------------------------------------------------------

    #[test]
    fn genuine_target_requires_sibling_cargo_toml() {
        let tmp = TempDir::new();
        // A `target` dir with NO sibling Cargo.toml is rejected.
        write(&tmp.path().join("notaproj/target/junk"), "x");
        assert!(genuine_target(&tmp.path().join("notaproj"), None).is_none());

        // With a Cargo.toml it is accepted.
        let proj = make_project(tmp.path(), "realproj");
        let td = genuine_target(&proj, Some("realproj")).expect("genuine");
        assert_eq!(td.target.file_name().unwrap(), "target");
        assert_eq!(td.name, "realproj");
    }

    #[test]
    fn discovery_is_case_sensitive_and_prunes_nested() {
        let tmp = TempDir::new();
        let proj = make_project(tmp.path(), "p");
        // Capital-T `Target` with no Cargo.toml must NOT be discovered.
        write(&tmp.path().join("cap/Target/inner"), "x");
        // A nested `target` *inside* a target must NOT appear (descent pruned).
        write(
            &proj.join("target/debug/build/sub/target/Cargo.toml"),
            "[package]",
        );
        write(&proj.join("target/debug/build/sub/target/junk"), "x");

        let found = discover_targets(&[tmp.path().to_string_lossy().into_owned()], &[], None);
        // Exactly one genuine target — p/target — despite the decoys.
        assert_eq!(found.len(), 1, "found: {found:?}");
        assert_eq!(found[0].project_root, fs::canonicalize(&proj).unwrap());
    }

    #[test]
    fn discovery_unions_db_roots_and_dedups() {
        let tmp = TempDir::new();
        let proj = make_project(tmp.path(), "p");
        let canon = fs::canonicalize(&proj).unwrap();
        // Same project via DB candidate AND via roots walk → one entry.
        let found = discover_targets(
            &[tmp.path().to_string_lossy().into_owned()],
            &[(proj.clone(), "p".into())],
            None,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].project_root, canon);
    }

    // -- safe_remove guard -------------------------------------------------

    #[test]
    fn validate_rejects_paths_outside_target() {
        let tmp = TempDir::new();
        let proj = make_project(tmp.path(), "p");
        let target = fs::canonicalize(proj.join("target")).unwrap();
        let allow: HashSet<PathBuf> = HashSet::new();

        // A source file (outside target) is refused even if asked.
        let src = fs::canonicalize(proj.join("src/lib.rs")).unwrap();
        assert!(validate_under_target(&src, &target, &allow).is_err());

        // A file inside target is accepted.
        let inside = fs::canonicalize(proj.join("target/release/x")).unwrap();
        assert!(validate_under_target(&inside, &target, &allow).is_ok());
    }

    #[test]
    fn validate_respects_allowlist() {
        let tmp = TempDir::new();
        let proj = make_project(tmp.path(), "p");
        let target = fs::canonicalize(proj.join("target")).unwrap();
        let project_root = fs::canonicalize(&proj).unwrap();
        let inside = fs::canonicalize(proj.join("target/release/x")).unwrap();

        let mut allow: HashSet<PathBuf> = HashSet::new();
        allow.insert(project_root);
        assert_eq!(
            validate_under_target(&inside, &target, &allow),
            Err("allowlisted-project")
        );
    }

    #[test]
    fn validate_rejects_symlinked_target_without_cargo_toml() {
        let tmp = TempDir::new();
        // target_root that is not a real `<proj>/target` (no Cargo.toml sibling).
        write(&tmp.path().join("bogus/target/x"), "x");
        let target = fs::canonicalize(tmp.path().join("bogus/target")).unwrap();
        let inside = fs::canonicalize(tmp.path().join("bogus/target/x")).unwrap();
        let allow: HashSet<PathBuf> = HashSet::new();
        assert_eq!(
            validate_under_target(&inside, &target, &allow),
            Err("target-root-without-sibling-cargo-toml")
        );
    }

    // -- tiers + recoverability -------------------------------------------

    #[test]
    fn tier0_removes_incremental_only() {
        let tmp = TempDir::new();
        let proj = make_project(tmp.path(), "p");
        let td = genuine_target(&proj, Some("p")).unwrap();
        let cfg = cfg_apply();
        let allow = HashSet::new();
        let mut manifest = Manifest {
            writer: None,
            path: None,
        };
        let mut report = CleanupReport::default();
        let bytes = tier0_incremental(&td, &cfg, &allow, "now", &mut manifest, &mut report);
        assert!(bytes > 0);
        assert!(
            !proj.join("target/debug/incremental").exists(),
            "incremental gone"
        );
        assert!(
            proj.join("target/debug/deps/libx.rlib").exists(),
            "deps kept"
        );
        assert!(proj.join("target/release/x").exists(), "release kept");
        // Source untouched — recoverability invariant.
        assert!(proj.join("src/lib.rs").exists());
    }

    #[test]
    fn tier2_full_wipe_keeps_source() {
        let tmp = TempDir::new();
        let proj = make_project(tmp.path(), "p");
        let td = genuine_target(&proj, Some("p")).unwrap();
        let cfg = cfg_apply();
        let allow = HashSet::new();
        let mut manifest = Manifest {
            writer: None,
            path: None,
        };
        let mut report = CleanupReport::default();
        let bytes = safe_remove(
            &td.target,
            &td.target,
            &td.name,
            "tier2-full",
            &cfg,
            &allow,
            "now",
            &mut manifest,
            &mut report,
        );
        assert!(bytes > 0);
        assert!(!proj.join("target").exists(), "target wiped");
        assert!(proj.join("Cargo.toml").exists(), "manifest kept");
        assert!(proj.join("src/lib.rs").exists(), "source kept");
    }

    #[test]
    fn dry_run_removes_nothing_but_reports_bytes() {
        let tmp = TempDir::new();
        let proj = make_project(tmp.path(), "p");
        let td = genuine_target(&proj, Some("p")).unwrap();
        let cfg = TargetCleanupConfig {
            dry_run: true,
            sweep_tmp: false,
            ..Default::default()
        };
        let allow = HashSet::new();
        let mut manifest = Manifest {
            writer: None,
            path: None,
        };
        let mut report = CleanupReport::default();
        let bytes = tier0_incremental(&td, &cfg, &allow, "now", &mut manifest, &mut report);
        assert!(bytes > 0, "dry-run still reports would-be bytes");
        assert!(
            proj.join("target/debug/incremental").exists(),
            "dry-run kept files"
        );
    }

    #[test]
    fn allowlisted_project_survives_safe_remove() {
        let tmp = TempDir::new();
        let proj = make_project(tmp.path(), "p");
        let td = genuine_target(&proj, Some("p")).unwrap();
        let cfg = cfg_apply();
        let mut allow = HashSet::new();
        allow.insert(td.project_root.clone());
        let mut manifest = Manifest {
            writer: None,
            path: None,
        };
        let mut report = CleanupReport::default();
        let bytes = safe_remove(
            &td.target,
            &td.target,
            &td.name,
            "tier2-full",
            &cfg,
            &allow,
            "now",
            &mut manifest,
            &mut report,
        );
        assert_eq!(bytes, 0, "allowlisted project not removed");
        assert!(proj.join("target").exists());
        assert_eq!(report.errors, 1, "DENY recorded as an error");
    }

    #[test]
    fn self_project_roots_include_cargo_manifest_dir() {
        // The compile-time source root must always be among the self-allowlist
        // anchors, regardless of where the binary is installed or what cwd it
        // runs under — this is what keeps pgmcp's own `target/` out of the
        // delete set when the daemon runs from an installed path such as
        // `~/.local/bin/pgmcp` (cwd `$HOME`), where `current_exe()` has no
        // `target/` ancestor and `current_dir()` is not the project root.
        let roots = detect_self_project_roots();
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(
            roots.contains(&manifest_dir),
            "CARGO_MANIFEST_DIR ({}) missing from self-roots: {:?}",
            manifest_dir.display(),
            roots
        );
    }

    #[test]
    fn prune_manifests_keeps_newest_n() {
        let tmp = TempDir::new();
        let dir = tmp.path();
        // Five manifests whose names sort chronologically, plus an unrelated
        // file the pruner must never touch.
        for ts in [
            "20260101T000000Z",
            "20260102T000000Z",
            "20260103T000000Z",
            "20260104T000000Z",
            "20260105T000000Z",
        ] {
            write(&dir.join(format!("manifest-{ts}.tsv")), "row");
        }
        write(&dir.join("unrelated.log"), "keep me");

        prune_manifests_in(dir, 2);

        let mut left: Vec<String> = fs::read_dir(dir)
            .expect("read temp dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "manifest-20260104T000000Z.tsv".to_string(),
                "manifest-20260105T000000Z.tsv".to_string(),
                "unrelated.log".to_string(),
            ],
            "prune must keep the 2 newest manifests and never touch other files"
        );

        // keep == 0 disables pruning entirely.
        prune_manifests_in(dir, 0);
        assert_eq!(
            fs::read_dir(dir).expect("read temp dir").count(),
            3,
            "keep=0 must prune nothing"
        );
    }

    // -- /proc busy matching ----------------------------------------------

    #[test]
    fn path_is_busy_prefix_match() {
        let target = PathBuf::from("/ws/p/target");
        let mut busy = HashSet::new();
        busy.insert(PathBuf::from("/ws/p/target/release/pgmcp"));
        assert!(path_is_busy(&busy, &target));

        let mut other = HashSet::new();
        other.insert(PathBuf::from("/ws/other/target/release/x"));
        assert!(!path_is_busy(&other, &target));
    }

    // -- tmp exclusions ----------------------------------------------------

    #[test]
    fn tmp_exclusions_match_live_session_names() {
        assert!(is_excluded_tmp_name(".X11-unix"));
        assert!(is_excluded_tmp_name("systemd-private-abcdef.service-1234"));
        assert!(is_excluded_tmp_name("ssh-XXXXabcd"));
        assert!(is_excluded_tmp_name("dbus-abcd"));
        assert!(!is_excluded_tmp_name("massif.out.1234"));
        assert!(!is_excluded_tmp_name("perf.data"));
        assert!(is_excluded_tmp_path(Path::new("/tmp/.X11-unix/X0")));
        assert!(!is_excluded_tmp_path(Path::new("/tmp/scratch/massif.out")));
    }

    // -- provenance classification ----------------------------------------

    fn meta_for(path: &Path) -> Metadata {
        fs::symlink_metadata(path).unwrap()
    }

    #[test]
    fn classify_tmp_protects_live_agent_and_removes_gone() {
        let tmp = TempDir::new();
        let file = tmp.path().join("scratch.py");
        write(&file, "print()");
        let md = meta_for(&file);
        let now = Utc::now();

        let live_session = Uuid::from_u128(1);
        let gone_session = Uuid::from_u128(2);
        let live_sessions: HashSet<Uuid> = [live_session].into_iter().collect();
        let alive_mcp: HashSet<String> = HashSet::new();
        let open: HashSet<PathBuf> = HashSet::new();

        // Attributed to a LIVE session → ProtectLive.
        let mut prov = HashMap::new();
        prov.insert(
            file.clone(),
            ProvRecord {
                session_id: Some(live_session),
                mcp_session_id: None,
                ts: now - Duration::hours(5),
            },
        );
        assert_eq!(
            classify_tmp_file(
                &file,
                &md,
                now,
                now.timestamp(),
                3600,
                &prov,
                &live_sessions,
                &alive_mcp,
                &open
            ),
            TmpDecision::ProtectLive
        );

        // Attributed to a GONE session, past grace → RemoveProvenance.
        let mut prov2 = HashMap::new();
        prov2.insert(
            file.clone(),
            ProvRecord {
                session_id: Some(gone_session),
                mcp_session_id: None,
                ts: now - Duration::hours(5),
            },
        );
        assert_eq!(
            classify_tmp_file(
                &file,
                &md,
                now,
                now.timestamp(),
                3600,
                &prov2,
                &live_sessions,
                &alive_mcp,
                &open
            ),
            TmpDecision::RemoveProvenance
        );

        // Gone session but WITHIN grace → WithinGrace.
        let mut prov3 = HashMap::new();
        prov3.insert(
            file.clone(),
            ProvRecord {
                session_id: Some(gone_session),
                mcp_session_id: None,
                ts: now - Duration::minutes(5),
            },
        );
        assert_eq!(
            classify_tmp_file(
                &file,
                &md,
                now,
                now.timestamp(),
                3600,
                &prov3,
                &live_sessions,
                &alive_mcp,
                &open
            ),
            TmpDecision::WithinGrace
        );
    }

    #[test]
    fn classify_tmp_unattributed_uses_age_and_busy() {
        let tmp = TempDir::new();
        let file = tmp.path().join("massif.out.123");
        write(&file, "junk");
        let md = meta_for(&file);
        let now = Utc::now();
        let prov: HashMap<PathBuf, ProvRecord> = HashMap::new();
        let live: HashSet<Uuid> = HashSet::new();
        let alive: HashSet<String> = HashSet::new();

        // Old enough (cutoff in the future ⇒ file older than cutoff) and not open.
        let empty_open: HashSet<PathBuf> = HashSet::new();
        let future_cutoff = now.timestamp() + 86_400;
        assert_eq!(
            classify_tmp_file(
                &file,
                &md,
                now,
                future_cutoff,
                3600,
                &prov,
                &live,
                &alive,
                &empty_open
            ),
            TmpDecision::RemoveAge
        );

        // Held open by a process → Keep, regardless of age.
        let mut open = HashSet::new();
        open.insert(file.clone());
        assert_eq!(
            classify_tmp_file(
                &file,
                &md,
                now,
                future_cutoff,
                3600,
                &prov,
                &live,
                &alive,
                &open
            ),
            TmpDecision::Keep
        );

        // Fresh (cutoff in the past ⇒ file newer than cutoff) → Keep.
        let past_cutoff = now.timestamp() - 86_400;
        assert_eq!(
            classify_tmp_file(
                &file,
                &md,
                now,
                past_cutoff,
                3600,
                &prov,
                &live,
                &alive,
                &empty_open
            ),
            TmpDecision::Keep
        );
    }

    #[test]
    fn actor_live_via_either_identity() {
        let s = Uuid::from_u128(7);
        let live_sessions: HashSet<Uuid> = [s].into_iter().collect();
        let alive_mcp: HashSet<String> = ["m1".to_string()].into_iter().collect();

        let hook = ProvRecord {
            session_id: Some(s),
            mcp_session_id: None,
            ts: Utc::now(),
        };
        assert!(actor_is_live(&hook, &live_sessions, &alive_mcp));

        let pid = ProvRecord {
            session_id: None,
            mcp_session_id: Some("m1".into()),
            ts: Utc::now(),
        };
        assert!(actor_is_live(&pid, &live_sessions, &alive_mcp));

        let dead = ProvRecord {
            session_id: Some(Uuid::from_u128(9)),
            mcp_session_id: Some("gone".into()),
            ts: Utc::now(),
        };
        assert!(!actor_is_live(&dead, &live_sessions, &alive_mcp));
    }
}
