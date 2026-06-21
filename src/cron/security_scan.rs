//! `security_scan` — runs installed external security scanners over each indexed
//! project and persists their findings.
//!
//! This is the engine behind the opt-in `[security_scan]` cron and the on-demand
//! `security_scan` MCP tool. For each project root it runs every **applicable**,
//! **installed**, non-(offline-gated) scanner from [`SCANNERS`], parses the
//! structured output into [`RawFinding`]s, and upserts them into
//! `external_scanner_findings` (fingerprint-keyed: a re-scan refreshes a finding;
//! one not re-seen flips to `resolved`). A per-(project, scanner) audit row lands
//! in `external_scanner_runs`. `syft` output is stored as an SBOM artifact rather
//! than findings.
//!
//! ## Safety / privacy
//! - **No shell**: each scanner is `Command::new(bin).args(..).current_dir(root)`;
//!   a missing binary is recorded `absent` and skipped, never fatal.
//! - **Bounded**: each run is wrapped in a per-project `tokio::time::timeout`
//!   (kill-on-drop); at most `max_concurrent` scanners run at once.
//! - **Local**: scanners run over the code locally; source is never uploaded.
//!   `trufflehog` runs with `--no-verification` (it never phones the provider with
//!   a secret), and `offline_only` skips scanners that fetch advisory/vuln DBs or
//!   rule packs. Secret values are redacted out of the stored `raw` payloads.
//! - **Output cap**: stdout/stderr are truncated to [`MAX_OUTPUT_BYTES`] before
//!   parsing so a pathological scanner cannot exhaust memory or the row size.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::process::Command as TokioCommand;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{error, info};

use crate::config::SecurityScanConfig;
use crate::db::queries;
use crate::tracker::severity::Severity;

/// Cap on captured stdout/stderr per scanner invocation (8 MiB). Larger output is
/// truncated before parsing — enough for any realistic JSON report.
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Hard cap on a presence/version probe (`<bin> --version`).
const PROBE_TIMEOUT_SECS: u64 = 5;

// ============================================================================
// Finding + execution types
// ============================================================================

/// A normalized finding emitted by a scanner parser, before persistence.
#[derive(Debug, Clone)]
struct RawFinding {
    rule_id: Option<String>,
    severity: Severity,
    file_path: Option<String>,
    line: Option<i64>,
    title: String,
    message: Option<String>,
    raw: Value,
}

/// Captured output of a completed scanner process.
#[derive(Debug)]
struct ScanOutput {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

/// The terminal outcome of attempting to run one scanner over one project.
enum ExecOutcome {
    Ran(ScanOutput),
    Timeout,
    Absent,
    Skipped(&'static str),
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunStatus {
    Ok,
    Timeout,
    Error,
    Absent,
    Skipped,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            RunStatus::Ok => "ok",
            RunStatus::Timeout => "timeout",
            RunStatus::Error => "error",
            RunStatus::Absent => "absent",
            RunStatus::Skipped => "skipped",
        }
    }
}

/// Outcome of one (project, scanner) task, folded into the [`ScanReport`].
struct TaskResult {
    status: RunStatus,
    upserted: usize,
    resolved: u64,
}

/// Facts about a project used to gate which scanners apply.
#[derive(Debug, Clone)]
struct ProjectFacts {
    id: i32,
    name: String,
    root: PathBuf,
    has_cargo_toml: bool,
    has_cargo_lock: bool,
    has_python: bool,
    has_c_cpp: bool,
    has_dockerfile: bool,
    has_compile_commands: bool,
}

// ============================================================================
// Scanner registry
// ============================================================================

/// A single scanner: how to detect it, how to invoke it, and how to parse it.
struct ScannerSpec {
    /// Tool-card slug + the `scanner` column value (e.g. `"gitleaks"`).
    slug: &'static str,
    /// Binary to execute (e.g. `"cargo"` for the cargo-* subcommands).
    bin: &'static str,
    /// Binary probed for *presence* when it differs from `bin` (e.g.
    /// `"cargo-audit"` while `bin` is `"cargo"`).
    presence_bin: Option<&'static str>,
    /// Args for the presence/version probe.
    version_args: &'static [&'static str],
    /// True if the scanner fetches advisory/vuln DBs or rule packs over the
    /// network (gated by `offline_only`).
    network: bool,
    /// True if the scanner writes its findings to stderr (cppcheck's template).
    output_is_stderr: bool,
    /// True if the scanner emits an SBOM artifact, not findings (syft).
    is_sbom: bool,
    /// Whether this scanner applies to a project.
    applies: fn(&ProjectFacts) -> bool,
    /// Build the argv (cwd is the project root). Empty ⇒ skip (no inputs).
    build_args: fn(&ProjectFacts) -> Vec<String>,
    /// Parse the relevant output stream into findings.
    parse: fn(&str) -> Vec<RawFinding>,
}

/// The closed registry of repo-applicable scanners. Network/RE/forensics tools
/// (nmap, nuclei, radare2, …) are intentionally absent — they need a live target
/// or manual driving and are catalogued (`src/tools_catalog`) only.
static SCANNERS: &[ScannerSpec] = &[
    ScannerSpec {
        slug: "gitleaks",
        bin: "gitleaks",
        presence_bin: None,
        version_args: &["version"],
        network: false,
        output_is_stderr: false,
        is_sbom: false,
        applies: applies_always,
        build_args: args_gitleaks,
        parse: parse_gitleaks,
    },
    ScannerSpec {
        slug: "trufflehog",
        bin: "trufflehog",
        presence_bin: None,
        version_args: &["--version"],
        network: false,
        output_is_stderr: false,
        is_sbom: false,
        applies: applies_always,
        build_args: args_trufflehog,
        parse: parse_trufflehog,
    },
    ScannerSpec {
        slug: "detect-secrets",
        bin: "detect-secrets",
        presence_bin: None,
        version_args: &["--version"],
        network: false,
        output_is_stderr: false,
        is_sbom: false,
        applies: applies_always,
        build_args: args_detect_secrets,
        parse: parse_detect_secrets,
    },
    ScannerSpec {
        slug: "semgrep",
        bin: "semgrep",
        presence_bin: None,
        version_args: &["--version"],
        network: true,
        output_is_stderr: false,
        is_sbom: false,
        applies: applies_always,
        build_args: args_semgrep,
        parse: parse_semgrep,
    },
    ScannerSpec {
        slug: "trivy",
        bin: "trivy",
        presence_bin: None,
        version_args: &["--version"],
        network: true,
        output_is_stderr: false,
        is_sbom: false,
        applies: applies_always,
        build_args: args_trivy,
        parse: parse_trivy,
    },
    ScannerSpec {
        slug: "cargo-audit",
        bin: "cargo",
        presence_bin: Some("cargo-audit"),
        version_args: &["--version"],
        network: true,
        output_is_stderr: false,
        is_sbom: false,
        applies: applies_cargo_lock,
        build_args: args_cargo_audit,
        parse: parse_cargo_audit,
    },
    ScannerSpec {
        slug: "cargo-deny",
        bin: "cargo",
        presence_bin: Some("cargo-deny"),
        version_args: &["--version"],
        network: true,
        output_is_stderr: true,
        is_sbom: false,
        applies: applies_cargo_toml,
        build_args: args_cargo_deny,
        parse: parse_cargo_deny,
    },
    ScannerSpec {
        slug: "grype",
        bin: "grype",
        presence_bin: None,
        version_args: &["version"],
        network: true,
        output_is_stderr: false,
        is_sbom: false,
        applies: applies_cargo_lock,
        build_args: args_grype,
        parse: parse_grype,
    },
    ScannerSpec {
        slug: "bandit",
        bin: "bandit",
        presence_bin: None,
        version_args: &["--version"],
        network: false,
        output_is_stderr: false,
        is_sbom: false,
        applies: applies_python,
        build_args: args_bandit,
        parse: parse_bandit,
    },
    ScannerSpec {
        slug: "cppcheck",
        bin: "cppcheck",
        presence_bin: None,
        version_args: &["--version"],
        network: false,
        output_is_stderr: true,
        is_sbom: false,
        applies: applies_c_cpp,
        build_args: args_cppcheck,
        parse: parse_cppcheck,
    },
    ScannerSpec {
        slug: "clang-tidy",
        bin: "clang-tidy",
        presence_bin: None,
        version_args: &["--version"],
        network: false,
        output_is_stderr: false,
        is_sbom: false,
        applies: applies_compile_db,
        build_args: args_clang_tidy,
        parse: parse_clang_tidy,
    },
    ScannerSpec {
        slug: "hadolint",
        bin: "hadolint",
        presence_bin: None,
        version_args: &["--version"],
        network: false,
        output_is_stderr: false,
        is_sbom: false,
        applies: applies_dockerfile,
        build_args: args_hadolint,
        parse: parse_hadolint,
    },
    ScannerSpec {
        slug: "syft",
        bin: "syft",
        presence_bin: None,
        version_args: &["version"],
        network: false,
        output_is_stderr: false,
        is_sbom: true,
        applies: applies_always,
        build_args: args_syft,
        parse: parse_none,
    },
];

// ---- applicability predicates ----
fn applies_always(_: &ProjectFacts) -> bool {
    true
}
fn applies_cargo_lock(f: &ProjectFacts) -> bool {
    f.has_cargo_lock
}
fn applies_cargo_toml(f: &ProjectFacts) -> bool {
    f.has_cargo_toml
}
fn applies_python(f: &ProjectFacts) -> bool {
    f.has_python
}
fn applies_c_cpp(f: &ProjectFacts) -> bool {
    f.has_c_cpp
}
fn applies_compile_db(f: &ProjectFacts) -> bool {
    f.has_compile_commands && f.has_c_cpp
}
fn applies_dockerfile(f: &ProjectFacts) -> bool {
    f.has_dockerfile
}

// ---- argv builders ----
fn vecs(a: &[&str]) -> Vec<String> {
    a.iter().map(|x| (*x).to_string()).collect()
}
fn args_gitleaks(_: &ProjectFacts) -> Vec<String> {
    vecs(&[
        "dir",
        ".",
        "--report-format=json",
        "--report-path=/dev/stdout",
        "--no-banner",
    ])
}
fn args_trufflehog(_: &ProjectFacts) -> Vec<String> {
    vecs(&[
        "filesystem",
        ".",
        "--json",
        "--no-update",
        "--no-verification",
    ])
}
fn args_detect_secrets(_: &ProjectFacts) -> Vec<String> {
    vecs(&["scan", "--all-files"])
}
fn args_semgrep(_: &ProjectFacts) -> Vec<String> {
    vecs(&[
        "scan",
        "--config",
        "auto",
        "--json",
        "--quiet",
        "--metrics=off",
        "--disable-version-check",
        ".",
    ])
}
fn args_trivy(_: &ProjectFacts) -> Vec<String> {
    vecs(&[
        "fs",
        "--format",
        "json",
        "--quiet",
        "--scanners",
        "vuln,secret,misconfig",
        ".",
    ])
}
fn args_cargo_audit(_: &ProjectFacts) -> Vec<String> {
    vecs(&["audit", "--json"])
}
fn args_cargo_deny(_: &ProjectFacts) -> Vec<String> {
    vecs(&["deny", "--format", "json", "check", "advisories", "bans"])
}
fn args_grype(_: &ProjectFacts) -> Vec<String> {
    vecs(&["dir:.", "-o", "json", "-q"])
}
fn args_bandit(_: &ProjectFacts) -> Vec<String> {
    vecs(&["-r", ".", "-f", "json", "-q"])
}
fn args_cppcheck(_: &ProjectFacts) -> Vec<String> {
    vecs(&[
        "--enable=warning,style,performance,portability,information",
        "--inconclusive",
        "--quiet",
        "--template={severity}|{file}|{line}|{id}|{message}",
        ".",
    ])
}
fn args_syft(_: &ProjectFacts) -> Vec<String> {
    vecs(&["dir:.", "-o", "syft-json", "-q"])
}
/// clang-tidy needs an explicit file list; pull it from `compile_commands.json`.
fn args_clang_tidy(f: &ProjectFacts) -> Vec<String> {
    let cc = f.root.join("compile_commands.json");
    let Ok(text) = std::fs::read_to_string(&cc) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    if let Some(arr) = v.as_array() {
        for e in arr {
            if let Some(file) = e.get("file").and_then(Value::as_str)
                && seen.insert(file.to_string())
            {
                files.push(file.to_string());
            }
        }
    }
    if files.is_empty() {
        return Vec::new();
    }
    let mut args = vecs(&["-p", ".", "--quiet"]);
    args.extend(files);
    args
}
/// hadolint takes Dockerfile paths; collect every `Dockerfile*` in the root.
fn args_hadolint(f: &ProjectFacts) -> Vec<String> {
    let mut dockerfiles = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&f.root) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("Dockerfile") && e.path().is_file() {
                dockerfiles.push(name);
            }
        }
    }
    if dockerfiles.is_empty() {
        return Vec::new();
    }
    dockerfiles.sort();
    let mut args = vecs(&["--no-color", "--format", "json"]);
    args.extend(dockerfiles);
    args
}

// ============================================================================
// Public entry points
// ============================================================================

/// Daemon-facing entry point: run a full sweep and log the summary, swallowing
/// errors so one bad tick never kills the cron thread.
pub async fn run_or_log(pool: PgPool, cfg: SecurityScanConfig) {
    let report = run_security_scan(&pool, &cfg, None).await;
    report.log_summary();
}

/// Run one sweep over every indexed project (optionally filtered by name/path
/// substring). Returns the [`ScanReport`]. Drives the on-demand MCP tool too.
pub async fn run_security_scan(
    pool: &PgPool,
    cfg: &SecurityScanConfig,
    project_filter: Option<&str>,
) -> ScanReport {
    let mut report = ScanReport::default();

    // 1. Probe presence/version of each scanner once.
    let allow: Option<HashSet<String>> = if cfg.tools.is_empty() {
        None
    } else {
        Some(cfg.tools.iter().map(|t| t.to_ascii_lowercase()).collect())
    };
    let mut versions: HashMap<&'static str, String> = HashMap::new();
    let mut active: Vec<&'static ScannerSpec> = Vec::new();
    for spec in SCANNERS {
        if let Some(a) = &allow
            && !a.contains(spec.slug)
        {
            continue;
        }
        if cfg.offline_only && spec.network {
            report.scanners_skipped_offline.push(spec.slug.to_string());
            continue;
        }
        match probe_present(spec).await {
            Some(version) => {
                versions.insert(spec.slug, version);
                active.push(spec);
                report.scanners_available.push(spec.slug.to_string());
            }
            None => report.scanners_missing.push(spec.slug.to_string()),
        }
    }
    if active.is_empty() {
        info!("security-scan: no applicable scanners installed; nothing to do");
        return report;
    }

    // 2. Enumerate project roots, filtered + on-disk.
    let projects = match queries::list_projects(pool).await {
        Ok(p) => p,
        Err(e) => {
            error!(error = %e, "security-scan: list_projects failed");
            return report;
        }
    };
    let exclude: Vec<String> = cfg
        .exclude_projects
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();

    let mut facts_list: Vec<ProjectFacts> = Vec::new();
    for p in projects {
        if let Some(filter) = project_filter {
            let f = filter.to_ascii_lowercase();
            if !p.name.to_ascii_lowercase().contains(&f)
                && !p.path.to_ascii_lowercase().contains(&f)
            {
                continue;
            }
        }
        let name_lc = p.name.to_ascii_lowercase();
        if exclude.iter().any(|x| name_lc.contains(x.as_str())) {
            continue;
        }
        let root = PathBuf::from(&p.path);
        if !root.is_dir() {
            continue;
        }
        facts_list.push(build_facts(pool, &p, root).await);
    }
    report.projects_scanned = facts_list.len();

    // 3. Fan out (project × applicable scanner) tasks, bounded by a semaphore.
    let sem = Arc::new(Semaphore::new(cfg.max_concurrent.max(1)));
    let timeout_secs = cfg.per_project_timeout_secs.max(1);
    let mut set: JoinSet<TaskResult> = JoinSet::new();
    for facts in facts_list {
        let facts = Arc::new(facts);
        for spec in active.iter().copied() {
            if !(spec.applies)(&facts) {
                continue;
            }
            let pool = pool.clone();
            let facts = Arc::clone(&facts);
            let sem = Arc::clone(&sem);
            let version = versions.get(spec.slug).cloned();
            set.spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .expect("security-scan semaphore closed");
                run_and_persist(&pool, spec, &facts, timeout_secs, version).await
            });
        }
    }

    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(r) => report.merge(&r),
            Err(e) => error!(error = %e, "security-scan: scanner task panicked"),
        }
    }
    report
}

// ============================================================================
// Per-(project, scanner) execution + persistence
// ============================================================================

async fn run_and_persist(
    pool: &PgPool,
    spec: &ScannerSpec,
    facts: &ProjectFacts,
    timeout_secs: u64,
    version: Option<String>,
) -> TaskResult {
    let start = Instant::now();
    let outcome = execute_scanner(spec, facts, timeout_secs).await;
    let duration_ms = start.elapsed().as_millis() as i64;
    let ver = version.as_deref();

    match outcome {
        ExecOutcome::Absent => {
            record_run(
                pool,
                facts.id,
                spec.slug,
                RunStatus::Absent,
                None,
                duration_ms,
                0,
                ver,
                None,
            )
            .await;
            TaskResult {
                status: RunStatus::Absent,
                upserted: 0,
                resolved: 0,
            }
        }
        ExecOutcome::Skipped(why) => {
            record_run(
                pool,
                facts.id,
                spec.slug,
                RunStatus::Skipped,
                None,
                duration_ms,
                0,
                ver,
                Some(why),
            )
            .await;
            TaskResult {
                status: RunStatus::Skipped,
                upserted: 0,
                resolved: 0,
            }
        }
        ExecOutcome::Timeout => {
            record_run(
                pool,
                facts.id,
                spec.slug,
                RunStatus::Timeout,
                None,
                duration_ms,
                0,
                ver,
                Some("exceeded per-project timeout"),
            )
            .await;
            TaskResult {
                status: RunStatus::Timeout,
                upserted: 0,
                resolved: 0,
            }
        }
        ExecOutcome::Error(e) => {
            record_run(
                pool,
                facts.id,
                spec.slug,
                RunStatus::Error,
                None,
                duration_ms,
                0,
                ver,
                Some(&e),
            )
            .await;
            TaskResult {
                status: RunStatus::Error,
                upserted: 0,
                resolved: 0,
            }
        }
        ExecOutcome::Ran(out) => {
            // SBOM generators store an artifact, not findings.
            if spec.is_sbom {
                if let Ok(sbom) = serde_json::from_str::<Value>(out.stdout.trim())
                    && !sbom.is_null()
                {
                    let _ =
                        queries::upsert_scanner_sbom(pool, facts.id, spec.slug, "syft-json", &sbom)
                            .await;
                }
                record_run(
                    pool,
                    facts.id,
                    spec.slug,
                    RunStatus::Ok,
                    out.exit_code,
                    duration_ms,
                    0,
                    ver,
                    Some("sbom stored"),
                )
                .await;
                return TaskResult {
                    status: RunStatus::Ok,
                    upserted: 0,
                    resolved: 0,
                };
            }

            let text = if spec.output_is_stderr {
                &out.stderr
            } else {
                &out.stdout
            };
            let findings = (spec.parse)(text);
            let detail = if findings.is_empty() && out.exit_code.is_some_and(|c| c != 0) {
                Some(snippet(&out.stderr))
            } else {
                None
            };
            let run_id = record_run(
                pool,
                facts.id,
                spec.slug,
                RunStatus::Ok,
                out.exit_code,
                duration_ms,
                findings.len() as i32,
                ver,
                detail.as_deref(),
            )
            .await
            .unwrap_or(0);

            let mut seen: Vec<String> = Vec::with_capacity(findings.len());
            let mut upserted = 0usize;
            for f in &findings {
                let fp = fingerprint(spec.slug, &facts.name, f);
                let provenance_key = format!("security_scan:{}:{}", spec.slug, fp);
                let title = truncate(&f.title, 500);
                let message = f.message.as_deref().map(|m| truncate(m, 4000));
                let res = queries::upsert_scanner_finding(
                    pool,
                    facts.id,
                    run_id,
                    spec.slug,
                    f.rule_id.as_deref(),
                    f.severity.as_str(),
                    f.file_path.as_deref(),
                    f.line.map(|l| l as i32),
                    &title,
                    message.as_deref(),
                    &f.raw,
                    &fp,
                    &provenance_key,
                    "security",
                )
                .await;
                match res {
                    Ok(()) => upserted += 1,
                    Err(e) => {
                        error!(scanner = spec.slug, error = %e, "security-scan: upsert finding failed")
                    }
                }
                seen.push(fp);
            }
            let resolved = queries::mark_unseen_resolved(pool, facts.id, spec.slug, &seen)
                .await
                .unwrap_or(0);
            TaskResult {
                status: RunStatus::Ok,
                upserted,
                resolved,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn record_run(
    pool: &PgPool,
    project_id: i32,
    scanner: &str,
    status: RunStatus,
    exit_code: Option<i32>,
    duration_ms: i64,
    findings_count: i32,
    version: Option<&str>,
    detail: Option<&str>,
) -> Option<i64> {
    match queries::insert_scanner_run(
        pool,
        project_id,
        scanner,
        status.as_str(),
        exit_code,
        duration_ms,
        findings_count,
        version,
        detail,
    )
    .await
    {
        Ok(id) => Some(id),
        Err(e) => {
            error!(scanner, error = %e, "security-scan: insert run row failed");
            None
        }
    }
}

/// Spawn a scanner, capping output and enforcing a kill-on-timeout deadline.
async fn execute_scanner(
    spec: &ScannerSpec,
    facts: &ProjectFacts,
    timeout_secs: u64,
) -> ExecOutcome {
    let args = (spec.build_args)(facts);
    if args.is_empty() {
        return ExecOutcome::Skipped("no applicable inputs");
    }
    let mut cmd = TokioCommand::new(spec.bin);
    cmd.args(&args)
        .current_dir(&facts.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0");
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ExecOutcome::Absent,
        Err(e) => return ExecOutcome::Error(format!("spawn failed: {e}")),
    };
    match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await {
        Err(_) => ExecOutcome::Timeout,
        Ok(Err(e)) => ExecOutcome::Error(format!("wait failed: {e}")),
        Ok(Ok(output)) => {
            let mut so = output.stdout;
            so.truncate(MAX_OUTPUT_BYTES);
            let mut se = output.stderr;
            se.truncate(MAX_OUTPUT_BYTES);
            ExecOutcome::Ran(ScanOutput {
                stdout: String::from_utf8_lossy(&so).into_owned(),
                stderr: String::from_utf8_lossy(&se).into_owned(),
                exit_code: output.status.code(),
            })
        }
    }
}

/// Probe a scanner's presence (and capture a version string). `None` ⇒ the
/// binary is not installed (spawn returned `NotFound`).
async fn probe_present(spec: &ScannerSpec) -> Option<String> {
    let bin = spec.presence_bin.unwrap_or(spec.bin);
    let mut cmd = TokioCommand::new(bin);
    cmd.args(spec.version_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let fut = cmd.output();
    match tokio::time::timeout(Duration::from_secs(PROBE_TIMEOUT_SECS), fut).await {
        Ok(Ok(o)) => {
            let src = if o.stdout.is_empty() {
                &o.stderr
            } else {
                &o.stdout
            };
            let line = String::from_utf8_lossy(src);
            Some(line.lines().next().unwrap_or("").trim().to_string())
        }
        // Spawned but the probe errored/timed out ⇒ still treat as present.
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
        Ok(Err(_)) | Err(_) => Some(String::new()),
    }
}

async fn build_facts(pool: &PgPool, p: &queries::ProjectInfo, root: PathBuf) -> ProjectFacts {
    let langs = queries::project_languages(pool, p.id)
        .await
        .unwrap_or_default();
    let has_python = langs.iter().any(|l| l.eq_ignore_ascii_case("python"));
    let has_c_cpp = langs.iter().any(|l| {
        matches!(
            l.to_ascii_lowercase().as_str(),
            "c" | "cpp" | "c++" | "cc" | "cxx" | "h" | "hpp" | "cu"
        )
    });
    ProjectFacts {
        id: p.id,
        name: p.name.clone(),
        has_cargo_toml: root.join("Cargo.toml").is_file(),
        has_cargo_lock: root.join("Cargo.lock").is_file(),
        has_python,
        has_c_cpp,
        has_dockerfile: has_dockerfile(&root),
        has_compile_commands: root.join("compile_commands.json").is_file(),
        root,
    }
}

fn has_dockerfile(root: &std::path::Path) -> bool {
    if root.join("Dockerfile").is_file() {
        return true;
    }
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with("Dockerfile") && e.path().is_file() {
                return true;
            }
        }
    }
    false
}

// ============================================================================
// Report
// ============================================================================

/// Summary of one sweep, logged and returned to the on-demand tool.
#[derive(Debug, Default)]
pub struct ScanReport {
    pub projects_scanned: usize,
    pub findings_upserted: usize,
    pub findings_resolved: u64,
    pub runs_ok: usize,
    pub runs_timeout: usize,
    pub runs_error: usize,
    pub runs_absent: usize,
    pub runs_skipped: usize,
    pub scanners_available: Vec<String>,
    pub scanners_missing: Vec<String>,
    pub scanners_skipped_offline: Vec<String>,
}

impl ScanReport {
    fn merge(&mut self, r: &TaskResult) {
        self.findings_upserted += r.upserted;
        self.findings_resolved += r.resolved;
        match r.status {
            RunStatus::Ok => self.runs_ok += 1,
            RunStatus::Timeout => self.runs_timeout += 1,
            RunStatus::Error => self.runs_error += 1,
            RunStatus::Absent => self.runs_absent += 1,
            RunStatus::Skipped => self.runs_skipped += 1,
        }
    }

    pub fn log_summary(&self) {
        info!(
            projects = self.projects_scanned,
            scanners_available = self.scanners_available.len(),
            scanners_missing = self.scanners_missing.len(),
            runs_ok = self.runs_ok,
            runs_timeout = self.runs_timeout,
            runs_error = self.runs_error,
            findings_upserted = self.findings_upserted,
            findings_resolved = self.findings_resolved,
            "security-scan: sweep complete"
        );
        if !self.scanners_missing.is_empty() {
            info!(missing = ?self.scanners_missing, "security-scan: scanners not installed (skipped)");
        }
        if !self.scanners_skipped_offline.is_empty() {
            info!(skipped = ?self.scanners_skipped_offline, "security-scan: network scanners skipped (offline_only)");
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn fingerprint(scanner: &str, project: &str, f: &RawFinding) -> String {
    let key = format!(
        "{scanner}|{project}|{}|{}|{}|{}",
        f.file_path.as_deref().unwrap_or(""),
        f.line.map(|l| l.to_string()).unwrap_or_default(),
        f.rule_id.as_deref().unwrap_or(""),
        f.title,
    );
    sha256_hex(&key)
}

/// Char-boundary-safe truncation with an ellipsis when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// First non-empty line, truncated — a compact `detail` for an audit row.
fn snippet(s: &str) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    truncate(line, 240)
}

fn map_sev(s: &str, default: Severity) -> Severity {
    match s.trim().to_ascii_lowercase().as_str() {
        "critical" | "crit" | "blocker" => Severity::Critical,
        "high" | "error" | "important" => Severity::High,
        "medium" | "moderate" | "warning" | "warn" | "med" => Severity::Medium,
        "low" | "info" | "informational" | "note" | "style" | "negligible" | "performance"
        | "portability" | "unknown" | "hint" => Severity::Low,
        _ => default,
    }
}

fn js(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}
fn ji(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(Value::as_i64)
}

/// Slice the outermost `[...]` array out of noisy output (e.g. a leading banner).
fn slice_array(s: &str) -> &str {
    match (s.find('['), s.rfind(']')) {
        (Some(a), Some(b)) if b >= a => &s[a..=b],
        _ => s.trim(),
    }
}
/// Slice the outermost `{...}` object out of noisy output.
fn slice_object(s: &str) -> &str {
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b >= a => &s[a..=b],
        _ => s.trim(),
    }
}

/// Replace the value of secret-bearing keys with a redaction marker so live
/// secrets never land in our own DB.
fn redact_keys(mut v: Value, keys: &[&str]) -> Value {
    if let Some(obj) = v.as_object_mut() {
        for k in keys {
            if obj.contains_key(*k) {
                obj.insert((*k).to_string(), json!("(redacted)"));
            }
        }
    }
    v
}

// ============================================================================
// Parsers — one per scanner
// ============================================================================

fn parse_none(_: &str) -> Vec<RawFinding> {
    Vec::new()
}

fn parse_gitleaks(out: &str) -> Vec<RawFinding> {
    let arr: Vec<Value> = serde_json::from_str(slice_array(out)).unwrap_or_default();
    arr.iter()
        .map(|v| {
            let rule = js(v, "RuleID");
            let desc = js(v, "Description").unwrap_or_else(|| "Hardcoded secret".into());
            RawFinding {
                rule_id: rule.clone(),
                severity: Severity::High,
                file_path: js(v, "File"),
                line: ji(v, "StartLine"),
                title: format!("Secret: {}", rule.as_deref().unwrap_or("leak")),
                message: Some(desc),
                raw: redact_keys(v.clone(), &["Secret", "Match"]),
            }
        })
        .collect()
}

fn parse_trufflehog(out: &str) -> Vec<RawFinding> {
    out.lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.starts_with('{') {
                return None;
            }
            let v: Value = serde_json::from_str(line).ok()?;
            let detector = js(&v, "DetectorName")?;
            let verified = v.get("Verified").and_then(Value::as_bool).unwrap_or(false);
            let (file, line_no) = trufflehog_loc(&v);
            Some(RawFinding {
                rule_id: Some(detector.clone()),
                severity: if verified {
                    Severity::Critical
                } else {
                    Severity::Medium
                },
                file_path: file,
                line: line_no,
                title: format!(
                    "Secret: {detector} ({})",
                    if verified { "verified" } else { "unverified" }
                ),
                message: None,
                raw: redact_keys(v.clone(), &["Raw", "RawV2", "Redacted"]),
            })
        })
        .collect()
}

fn trufflehog_loc(v: &Value) -> (Option<String>, Option<i64>) {
    if let Some(d) = v.pointer("/SourceMetadata/Data").and_then(Value::as_object) {
        for sv in d.values() {
            let file = js(sv, "file").or_else(|| js(sv, "path"));
            let line = ji(sv, "line");
            if file.is_some() || line.is_some() {
                return (file, line);
            }
        }
    }
    (None, None)
}

fn parse_detect_secrets(out: &str) -> Vec<RawFinding> {
    let v: Value = serde_json::from_str(slice_object(out)).unwrap_or(Value::Null);
    let mut findings = Vec::new();
    if let Some(results) = v.get("results").and_then(Value::as_object) {
        for (path, arr) in results {
            let Some(items) = arr.as_array() else {
                continue;
            };
            for it in items {
                let typ = js(it, "type").unwrap_or_else(|| "secret".into());
                findings.push(RawFinding {
                    rule_id: Some(typ.clone()),
                    severity: Severity::Medium,
                    file_path: Some(path.clone()),
                    line: ji(it, "line_number"),
                    title: format!("Potential secret: {typ}"),
                    message: js(it, "hashed_secret").map(|h| format!("hashed_secret={h}")),
                    // detect-secrets stores only a hashed secret — safe to keep.
                    raw: it.clone(),
                });
            }
        }
    }
    findings
}

fn parse_semgrep(out: &str) -> Vec<RawFinding> {
    let v: Value = serde_json::from_str(slice_object(out)).unwrap_or(Value::Null);
    let mut findings = Vec::new();
    if let Some(results) = v.get("results").and_then(Value::as_array) {
        for r in results {
            let check = js(r, "check_id");
            let sev = r
                .pointer("/extra/severity")
                .and_then(Value::as_str)
                .unwrap_or("WARNING");
            findings.push(RawFinding {
                rule_id: check.clone(),
                severity: map_sev(sev, Severity::Medium),
                file_path: js(r, "path"),
                line: r.pointer("/start/line").and_then(Value::as_i64),
                title: check.clone().unwrap_or_else(|| "semgrep finding".into()),
                message: r
                    .pointer("/extra/message")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                raw: r.clone(),
            });
        }
    }
    findings
}

fn parse_trivy(out: &str) -> Vec<RawFinding> {
    let v: Value = serde_json::from_str(slice_object(out)).unwrap_or(Value::Null);
    let mut findings = Vec::new();
    let Some(results) = v.get("Results").and_then(Value::as_array) else {
        return findings;
    };
    for res in results {
        let target = js(res, "Target");
        if let Some(vulns) = res.get("Vulnerabilities").and_then(Value::as_array) {
            for vu in vulns {
                let id = js(vu, "VulnerabilityID");
                let pkg = js(vu, "PkgName").unwrap_or_default();
                let ver = js(vu, "InstalledVersion").unwrap_or_default();
                let sev = js(vu, "Severity").unwrap_or_default();
                findings.push(RawFinding {
                    rule_id: id.clone(),
                    severity: map_sev(&sev, Severity::Medium),
                    file_path: target.clone(),
                    line: None,
                    title: format!("{} in {pkg} {ver}", id.as_deref().unwrap_or("CVE")),
                    message: js(vu, "Title").or_else(|| js(vu, "Description")),
                    raw: vu.clone(),
                });
            }
        }
        if let Some(miscs) = res.get("Misconfigurations").and_then(Value::as_array) {
            for mc in miscs {
                let id = js(mc, "ID");
                let sev = js(mc, "Severity").unwrap_or_default();
                let title = js(mc, "Title").unwrap_or_else(|| "misconfiguration".into());
                findings.push(RawFinding {
                    rule_id: id.clone(),
                    severity: map_sev(&sev, Severity::Medium),
                    file_path: target.clone(),
                    line: mc
                        .pointer("/CauseMetadata/StartLine")
                        .and_then(Value::as_i64),
                    title: format!("Misconfig {}: {title}", id.as_deref().unwrap_or("")),
                    message: js(mc, "Message"),
                    raw: mc.clone(),
                });
            }
        }
        if let Some(secrets) = res.get("Secrets").and_then(Value::as_array) {
            for sc in secrets {
                let rule = js(sc, "RuleID");
                let sev = js(sc, "Severity").unwrap_or_else(|| "HIGH".into());
                findings.push(RawFinding {
                    rule_id: rule.clone(),
                    severity: map_sev(&sev, Severity::High),
                    file_path: target.clone(),
                    line: ji(sc, "StartLine"),
                    title: format!("Secret: {}", js(sc, "Title").or(rule).unwrap_or_default()),
                    message: None,
                    raw: redact_keys(sc.clone(), &["Match", "Code"]),
                });
            }
        }
    }
    findings
}

fn parse_cargo_audit(out: &str) -> Vec<RawFinding> {
    let v: Value = serde_json::from_str(slice_object(out)).unwrap_or(Value::Null);
    let mut findings = Vec::new();
    if let Some(list) = v.pointer("/vulnerabilities/list").and_then(Value::as_array) {
        for it in list {
            let adv = it.get("advisory").cloned().unwrap_or(Value::Null);
            let id = js(&adv, "id");
            let title = js(&adv, "title").unwrap_or_else(|| "advisory".into());
            let pkg = it
                .pointer("/package/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let ver = it
                .pointer("/package/version")
                .and_then(Value::as_str)
                .unwrap_or("");
            findings.push(RawFinding {
                rule_id: id.clone(),
                severity: Severity::High,
                file_path: Some("Cargo.lock".into()),
                line: None,
                title: format!("{} in {pkg} {ver}", id.as_deref().unwrap_or("RUSTSEC")),
                message: Some(title),
                raw: it.clone(),
            });
        }
    }
    if let Some(obj) = v.get("warnings").and_then(Value::as_object) {
        for (kind, arr) in obj {
            let Some(items) = arr.as_array() else {
                continue;
            };
            for it in items {
                let pkg = it
                    .pointer("/package/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                findings.push(RawFinding {
                    rule_id: Some(format!("warning:{kind}")),
                    severity: Severity::Low,
                    file_path: Some("Cargo.lock".into()),
                    line: None,
                    title: format!("{kind}: {pkg}"),
                    message: js(it.get("advisory").unwrap_or(&Value::Null), "title"),
                    raw: it.clone(),
                });
            }
        }
    }
    findings
}

fn parse_cargo_deny(out: &str) -> Vec<RawFinding> {
    let mut findings = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("diagnostic") {
            continue;
        }
        let fields = v.get("fields").cloned().unwrap_or(Value::Null);
        let sev = js(&fields, "severity").unwrap_or_else(|| "warning".into());
        let sevl = sev.to_ascii_lowercase();
        if sevl == "note" || sevl == "help" {
            continue;
        }
        let code = js(&fields, "code");
        let msg = js(&fields, "message").unwrap_or_else(|| "diagnostic".into());
        findings.push(RawFinding {
            rule_id: code.clone(),
            severity: map_sev(&sev, Severity::Medium),
            file_path: Some("Cargo.toml".into()),
            line: None,
            title: format!(
                "{}: {}",
                code.as_deref().unwrap_or("cargo-deny"),
                first_line(&msg)
            ),
            message: Some(msg),
            raw: fields,
        });
    }
    findings
}

fn parse_grype(out: &str) -> Vec<RawFinding> {
    let v: Value = serde_json::from_str(slice_object(out)).unwrap_or(Value::Null);
    let mut findings = Vec::new();
    if let Some(matches) = v.get("matches").and_then(Value::as_array) {
        for m in matches {
            let vuln = m.get("vulnerability").cloned().unwrap_or(Value::Null);
            let id = js(&vuln, "id");
            let sev = js(&vuln, "severity").unwrap_or_default();
            let name = m
                .pointer("/artifact/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let ver = m
                .pointer("/artifact/version")
                .and_then(Value::as_str)
                .unwrap_or("");
            findings.push(RawFinding {
                rule_id: id.clone(),
                severity: map_sev(&sev, Severity::Medium),
                file_path: None,
                line: None,
                title: format!("{} in {name} {ver}", id.as_deref().unwrap_or("CVE")),
                message: js(&vuln, "description"),
                raw: m.clone(),
            });
        }
    }
    findings
}

fn parse_bandit(out: &str) -> Vec<RawFinding> {
    let v: Value = serde_json::from_str(slice_object(out)).unwrap_or(Value::Null);
    let mut findings = Vec::new();
    if let Some(results) = v.get("results").and_then(Value::as_array) {
        for r in results {
            let sev = js(r, "issue_severity").unwrap_or_default();
            findings.push(RawFinding {
                rule_id: js(r, "test_id"),
                severity: map_sev(&sev, Severity::Medium),
                file_path: js(r, "filename"),
                line: ji(r, "line_number"),
                title: js(r, "test_name").unwrap_or_else(|| "bandit finding".into()),
                message: js(r, "issue_text"),
                raw: r.clone(),
            });
        }
    }
    findings
}

fn parse_cppcheck(out: &str) -> Vec<RawFinding> {
    let mut findings = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(5, '|').collect();
        if parts.len() < 5 {
            continue;
        }
        let id = parts[3];
        let message = parts[4];
        findings.push(RawFinding {
            rule_id: Some(id.to_string()),
            severity: map_sev(parts[0], Severity::Low),
            file_path: if parts[1].is_empty() {
                None
            } else {
                Some(parts[1].to_string())
            },
            line: parts[2].parse::<i64>().ok(),
            title: format!("{id}: {}", truncate(message, 120)),
            message: Some(message.to_string()),
            raw: json!({
                "severity": parts[0], "file": parts[1], "line": parts[2],
                "id": id, "message": message,
            }),
        });
    }
    findings
}

fn clang_tidy_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(.+?):(\d+):(\d+):\s+(warning|error):\s+(.*?)(?:\s+\[([A-Za-z0-9.\-,]+)\])?$")
            .expect("clang-tidy diagnostic regex")
    })
}

fn parse_clang_tidy(out: &str) -> Vec<RawFinding> {
    let re = clang_tidy_re();
    let mut findings = Vec::new();
    let mut seen = HashSet::new();
    for line in out.lines() {
        let Some(c) = re.captures(line) else {
            continue;
        };
        let file = c.get(1).map(|m| m.as_str().to_string());
        let lno = c.get(2).and_then(|m| m.as_str().parse::<i64>().ok());
        let level = c.get(4).map_or("warning", |m| m.as_str());
        let msg = c.get(5).map(|m| m.as_str().to_string()).unwrap_or_default();
        let check = c.get(6).map(|m| m.as_str().to_string());
        let key = format!("{file:?}:{lno:?}:{check:?}");
        if !seen.insert(key) {
            continue;
        }
        findings.push(RawFinding {
            rule_id: check.clone(),
            severity: map_sev(level, Severity::Medium),
            file_path: file,
            line: lno,
            title: format!(
                "{}: {}",
                check.as_deref().unwrap_or("clang-tidy"),
                truncate(&msg, 120)
            ),
            message: Some(msg),
            raw: json!({ "check": check, "level": level }),
        });
    }
    findings
}

fn parse_hadolint(out: &str) -> Vec<RawFinding> {
    let arr: Vec<Value> = serde_json::from_str(slice_array(out)).unwrap_or_default();
    arr.iter()
        .map(|v| {
            let level = js(v, "level").unwrap_or_else(|| "info".into());
            // hadolint lints are best-practice → cap at Medium.
            let severity = match level.to_ascii_lowercase().as_str() {
                "error" => Severity::Medium,
                _ => Severity::Low,
            };
            let code = js(v, "code");
            let msg = js(v, "message").unwrap_or_default();
            RawFinding {
                rule_id: code.clone(),
                severity,
                file_path: js(v, "file"),
                line: ji(v, "line"),
                title: format!(
                    "{}: {}",
                    code.as_deref().unwrap_or("hadolint"),
                    truncate(&msg, 120)
                ),
                message: Some(msg),
                raw: v.clone(),
            }
        })
        .collect()
}

fn first_line(s: &str) -> String {
    truncate(s.lines().next().unwrap_or("").trim(), 120)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitleaks_parser_redacts_and_maps() {
        let out = r#"[{"RuleID":"aws-access-token","Description":"AWS Access Key","File":"src/a.rs","StartLine":42,"Secret":"AKIAEXAMPLE","Match":"key=AKIAEXAMPLE"}]"#;
        let f = parse_gitleaks(out);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].file_path.as_deref(), Some("src/a.rs"));
        assert_eq!(f[0].line, Some(42));
        assert_eq!(f[0].rule_id.as_deref(), Some("aws-access-token"));
        // The secret value must be redacted out of the stored payload.
        assert_eq!(f[0].raw.get("Secret").unwrap(), "(redacted)");
        assert_eq!(f[0].raw.get("Match").unwrap(), "(redacted)");
    }

    #[test]
    fn semgrep_parser_maps_severity() {
        let out = r#"{"results":[{"check_id":"rules.sql-injection","path":"app.py","start":{"line":7},"extra":{"severity":"ERROR","message":"SQLi"}}]}"#;
        let f = parse_semgrep(out);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].line, Some(7));
        assert_eq!(f[0].file_path.as_deref(), Some("app.py"));
    }

    #[test]
    fn trivy_parser_extracts_vulns() {
        let out = r#"{"Results":[{"Target":"Cargo.lock","Vulnerabilities":[{"VulnerabilityID":"CVE-2021-1","PkgName":"foo","InstalledVersion":"1.0","Severity":"CRITICAL","Title":"bad"}]}]}"#;
        let f = parse_trivy(out);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Critical);
        assert_eq!(f[0].rule_id.as_deref(), Some("CVE-2021-1"));
    }

    #[test]
    fn cppcheck_template_parser() {
        let out = "error|src/x.c|10|nullPointer|Null pointer dereference\nwarning|src/y.c|3|uninitvar|Uninitialized variable";
        let f = parse_cppcheck(out);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].line, Some(10));
        assert_eq!(f[1].severity, Severity::Medium);
    }

    #[test]
    fn clang_tidy_text_parser() {
        let out = "/p/a.cpp:12:5: warning: use after move [bugprone-use-after-move]";
        let f = parse_clang_tidy(out);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule_id.as_deref(), Some("bugprone-use-after-move"));
        assert_eq!(f[0].line, Some(12));
    }

    #[test]
    fn hadolint_caps_at_medium() {
        let out = r#"[{"file":"Dockerfile","line":3,"level":"error","code":"DL3008","message":"Pin versions"}]"#;
        let f = parse_hadolint(out);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Medium);
        assert_eq!(f[0].rule_id.as_deref(), Some("DL3008"));
    }

    #[test]
    fn fingerprint_is_stable_and_keyed() {
        let f = RawFinding {
            rule_id: Some("r".into()),
            severity: Severity::High,
            file_path: Some("a.rs".into()),
            line: Some(1),
            title: "t".into(),
            message: None,
            raw: Value::Null,
        };
        let a = fingerprint("gitleaks", "proj", &f);
        let b = fingerprint("gitleaks", "proj", &f);
        assert_eq!(a, b, "same finding ⇒ same fingerprint");
        let c = fingerprint("gitleaks", "other", &f);
        assert_ne!(a, c, "project participates in the key");
        assert_eq!(a.len(), 64, "sha256 hex");
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("hello", 10), "hello");
        let t = truncate("héllo wörld", 4);
        assert!(t.chars().count() <= 4);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn empty_output_yields_no_findings() {
        assert!(parse_gitleaks("").is_empty());
        assert!(parse_semgrep("not json").is_empty());
        assert!(parse_trivy("").is_empty());
        assert!(parse_bandit("garbage").is_empty());
    }
}
