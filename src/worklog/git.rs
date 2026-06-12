//! Live `git` collectors — the authoritative spine of the work summary.
//!
//! Commit facts (including line churn, which is NOT stored in `git_commits`) come
//! from one `git log --numstat` pass per canonical repo; uncommitted/mid-stream
//! state reuses `crate::deps::gitstate` plus `git diff --shortstat`. All reads are
//! best-effort and read-only — pgmcp never mutates git.

use std::collections::BTreeMap;
use std::process::Command;

use chrono::NaiveDate;

/// Record-separator that prefixes every commit header line in the `git log`
/// pretty format; `\x1f` (unit separator) delimits the fields. Neither byte
/// occurs in normal subjects, so parsing is unambiguous.
const REC: char = '\u{1e}';
const UNIT: char = '\u{1f}';

/// A single commit in the window (already deduped by `git log` within its repo).
/// Only the fields the summary aggregates over are retained; the SHA and author
/// are matched/filtered by `git log` itself and not needed downstream.
#[derive(Debug, Clone)]
pub struct Commit {
    pub date: Option<NaiveDate>,
    pub subject: String,
    pub added: u64,
    pub deleted: u64,
}

/// Aggregated commit facts for one repo over the window.
#[derive(Debug, Clone, Default)]
pub struct CommitStats {
    pub commits: Vec<Commit>,
    pub added: u64,
    pub deleted: u64,
    /// Commits per calendar day (author date).
    pub per_day: BTreeMap<NaiveDate, u32>,
    /// Conventional-commit type → commit count (e.g. `feat`, `fix`).
    pub type_counts: BTreeMap<String, u32>,
    /// Conventional-commit scope → count (the `(scope)` content, comma-split).
    pub scope_counts: BTreeMap<String, u32>,
    /// Salient subject keyword → frequency (stopwords removed).
    pub keyword_counts: BTreeMap<String, u32>,
}

/// Uncommitted / mid-stream working-tree state for one repo.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Uncommitted {
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub dirty: bool,
    pub modified: u32,
    pub untracked: u32,
    pub deleted: u32,
    pub staged: u32,
    /// Lines added across staged + unstaged diffs (`git diff [--cached] --shortstat`).
    pub added_lines: u64,
    pub deleted_lines: u64,
}

/// Run `git` in `repo_path`, returning stdout (lossy UTF-8) or `String::new()`
/// on any failure — every collector degrades to "no data" rather than erroring.
fn git(repo_path: &str, args: &[&str]) -> String {
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Resolve the local git author name for a workspace (used to default the
/// `author` filter to "my work"). Falls back to the global config.
pub fn resolve_author(workspace_root: &str) -> Option<String> {
    let local = git(workspace_root, &["config", "user.name"]);
    let name = local.trim();
    if !name.is_empty() {
        return Some(name.to_string());
    }
    let global = Command::new("git")
        .args(["config", "--global", "user.name"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    (!global.is_empty()).then_some(global)
}

/// Collect commit facts over `[since, until)` for `author` (a case-insensitive
/// `--author` regex; `None` = all contributors). `since`/`until` are git date
/// strings (e.g. `2026-05-01`).
pub fn collect_commits(
    repo_path: &str,
    since: &str,
    until: &str,
    author: Option<&str>,
) -> CommitStats {
    let pretty = format!("--pretty=format:{REC}%H{UNIT}%ad{UNIT}%an{UNIT}%s");
    let since_arg = format!("--since={since}");
    let until_arg = format!("--until={until}");
    let mut args: Vec<&str> = vec![
        "log",
        "--all",
        "--no-merges",
        "--date=short",
        "--numstat",
        &pretty,
        &since_arg,
        &until_arg,
    ];
    let author_arg;
    if let Some(a) = author {
        args.push("-i");
        author_arg = format!("--author={a}");
        args.push(&author_arg);
    }
    parse_log(&git(repo_path, &args))
}

/// Parse the `git log --numstat` stream into aggregated [`CommitStats`].
fn parse_log(out: &str) -> CommitStats {
    let mut stats = CommitStats::default();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix(REC) {
            // Commit header: SHA \x1f date \x1f author \x1f subject. Only the date
            // and subject are retained (SHA/author are matched by `git log`).
            let mut it = rest.splitn(4, UNIT);
            let _sha = it.next();
            let date_s = it.next().unwrap_or_default();
            let _author = it.next();
            let subject = it.next().unwrap_or_default().to_string();
            let date = NaiveDate::parse_from_str(date_s, "%Y-%m-%d").ok();
            if let Some(d) = date {
                *stats.per_day.entry(d).or_insert(0) += 1;
            }
            accumulate_themes(&subject, &mut stats);
            stats.commits.push(Commit {
                date,
                subject,
                added: 0,
                deleted: 0,
            });
        } else if !line.trim().is_empty() {
            // numstat row: <added>\t<deleted>\t<path>  (binary files use '-').
            let mut it = line.split('\t');
            let a = it.next().unwrap_or("-");
            let d = it.next().unwrap_or("-");
            let (add, del) = (a.parse::<u64>().unwrap_or(0), d.parse::<u64>().unwrap_or(0));
            stats.added += add;
            stats.deleted += del;
            if let Some(c) = stats.commits.last_mut() {
                c.added += add;
                c.deleted += del;
            }
        }
    }
    stats
}

/// Fold a subject's conventional-commit type/scope and salient keywords into the
/// running tallies.
fn accumulate_themes(subject: &str, stats: &mut CommitStats) {
    if let Some((ctype, scopes)) = parse_conventional(subject) {
        *stats.type_counts.entry(ctype).or_insert(0) += 1;
        for s in scopes {
            *stats.scope_counts.entry(s).or_insert(0) += 1;
        }
    }
    for kw in keywords(subject) {
        *stats.keyword_counts.entry(kw).or_insert(0) += 1;
    }
}

/// Parse a conventional-commit prefix `type(scope1,scope2)!: …`. Returns the
/// lowercased type and the comma-split scopes (empty when no `(scope)`).
pub fn parse_conventional(subject: &str) -> Option<(String, Vec<String>)> {
    let head = subject.split(':').next()?;
    // Reject anything that isn't `type` or `type(scope)` optionally with `!`.
    let (type_part, scope_part) = match head.split_once('(') {
        Some((t, rest)) => (
            t,
            rest.strip_suffix(')').or_else(|| rest.strip_suffix(")!")),
        ),
        None => (head.trim_end_matches('!'), None),
    };
    let ctype = type_part.trim().trim_end_matches('!').to_ascii_lowercase();
    if ctype.is_empty() || !ctype.chars().all(|c| c.is_ascii_lowercase()) {
        return None;
    }
    // `head` must be strictly shorter than the subject (a real `:` delimiter).
    if head.len() >= subject.len() {
        return None;
    }
    let scopes = scope_part
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some((ctype, scopes))
}

/// Salient lowercase keywords from a subject (len ≥ 4, alphabetic-ish, minus a
/// small stoplist of conventional-commit verbs and English glue).
fn keywords(subject: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "with", "that", "this", "from", "into", "when", "then", "also", "have", "more", "than",
        "over", "make", "made", "adds", "added", "uses", "using", "fix", "fixes", "feat", "docs",
        "test", "tests", "refactor", "chore", "wip", "the", "and", "for", "add", "use", "via",
        "all", "not", "now", "are", "was", "per", "out",
    ];
    subject
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .filter(|w| w.len() >= 4 && w.chars().any(|c| c.is_ascii_alphabetic()))
        .filter(|w| !STOP.contains(w))
        .map(|w| w.to_string())
        .collect()
}

/// Read uncommitted / mid-stream state for a working tree (best-effort).
pub fn collect_uncommitted(repo_path: &str) -> Uncommitted {
    let state = crate::deps::gitstate::read_git_state(repo_path);
    let mut u = Uncommitted {
        branch: state.current_branch,
        head_sha: state.head_sha,
        dirty: state.dirty,
        ..Default::default()
    };
    for line in git(repo_path, &["status", "--porcelain"]).lines() {
        // Two-column XY status code; index col = staged, worktree col = unstaged.
        let bytes = line.as_bytes();
        let (x, y) = (
            bytes.first().copied().unwrap_or(b' '),
            bytes.get(1).copied().unwrap_or(b' '),
        );
        if x == b'?' && y == b'?' {
            u.untracked += 1;
            continue;
        }
        if x != b' ' {
            u.staged += 1;
        }
        match y {
            b'M' => u.modified += 1,
            b'D' => u.deleted += 1,
            _ => {}
        }
    }
    let (a1, d1) = parse_shortstat(&git(repo_path, &["diff", "--shortstat"]));
    let (a2, d2) = parse_shortstat(&git(repo_path, &["diff", "--cached", "--shortstat"]));
    u.added_lines = a1 + a2;
    u.deleted_lines = d1 + d2;
    u
}

/// Parse `git diff --shortstat` ("N files changed, A insertions(+), D deletions(-)").
fn parse_shortstat(s: &str) -> (u64, u64) {
    let mut added = 0u64;
    let mut deleted = 0u64;
    for part in s.split(',') {
        let p = part.trim();
        if let Some(n) = p
            .split_whitespace()
            .next()
            .and_then(|x| x.parse::<u64>().ok())
        {
            if p.contains("insertion") {
                added = n;
            } else if p.contains("deletion") {
                deleted = n;
            }
        }
    }
    (added, deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_conventional_extracts_type_and_scopes() {
        assert_eq!(
            parse_conventional("feat(tracker): add burndown"),
            Some(("feat".to_string(), vec!["tracker".to_string()]))
        );
        assert_eq!(
            parse_conventional("fix(a,b): two scopes"),
            Some(("fix".to_string(), vec!["a".to_string(), "b".to_string()]))
        );
        assert_eq!(
            parse_conventional("refactor: no scope"),
            Some(("refactor".to_string(), vec![]))
        );
        // Not conventional: a bare sentence with a colon-less head, or a ratio.
        assert_eq!(parse_conventional("WPDS Commit 2 applied"), None);
        assert_eq!(parse_conventional("Phase 4: unlocked"), None); // 'phase 4' not all-lowercase
    }

    #[test]
    fn parse_log_sums_numstat_per_commit() {
        let out = format!(
            "{REC}abc123{UNIT}2026-05-20{UNIT}Dylon Edwards{UNIT}feat(x): a\n\
             10\t2\tsrc/a.rs\n\
             5\t1\tsrc/b.rs\n\
             {REC}def456{UNIT}2026-05-21{UNIT}Dylon Edwards{UNIT}fix(y): b\n\
             -\t-\tbin.dat\n\
             3\t0\tsrc/c.rs\n"
        );
        let s = parse_log(&out);
        assert_eq!(s.commits.len(), 2);
        assert_eq!(s.commits[0].added, 15);
        assert_eq!(s.commits[0].deleted, 3);
        assert_eq!(s.added, 18);
        assert_eq!(s.deleted, 3);
        assert_eq!(s.type_counts.get("feat"), Some(&1));
        assert_eq!(s.type_counts.get("fix"), Some(&1));
        assert_eq!(s.scope_counts.get("x"), Some(&1));
        assert_eq!(s.per_day.len(), 2);
    }

    #[test]
    fn parse_shortstat_reads_insertions_and_deletions() {
        assert_eq!(
            parse_shortstat(" 3 files changed, 42 insertions(+), 9 deletions(-)"),
            (42, 9)
        );
        assert_eq!(parse_shortstat(" 1 file changed, 5 insertions(+)"), (5, 0));
        assert_eq!(parse_shortstat(""), (0, 0));
    }
}
