//! Reusable finding scan + scoring primitives shared by the
//! `documented_tech_debt` / `bug_prediction` MCP tools and the
//! `findings-promotion` cron (`crate::cron::findings_promotion`).
//!
//! The tools surface findings for an agent to read; the cron idempotently
//! promotes the high-confidence / high-severity ones into `pending` work items.
//! Both must agree on *what counts* as a finding and *how it is scored*, so the
//! marker catalog, the comment-scan loop, and the defect scoring live here
//! (single source of truth) rather than being copy-pasted.

use std::collections::HashMap;

/// The comment-marker catalog: `(marker, severity_tier)`. Severity tiers are
/// `high` (FIXME/BUG/HACK/KLUDGE/WTF/XXX), `medium`, and `low`. This is the
/// canonical list the `documented_tech_debt` tool renders and the
/// `findings-promotion` cron filters to its high tier.
pub fn comment_markers() -> Vec<(&'static str, &'static str)> {
    vec![
        // high
        ("FIXME", "high"),
        ("BUG", "high"),
        ("HACK", "high"),
        ("KLUDGE", "high"),
        ("WTF", "high"),
        ("XXX", "high"),
        // medium
        ("TODO", "medium"),
        ("TBD", "medium"),
        ("WORKAROUND", "medium"),
        ("REVIEW", "medium"),
        ("SMELL", "medium"),
        ("REFACTOR", "medium"),
        ("DEPRECATED", "medium"),
        // low
        ("NOTE", "low"),
        ("OPTIMIZE", "low"),
        ("TEMP", "low"),
        ("DEBUG", "low"),
    ]
}

/// An uppercase→severity lookup over [`comment_markers`].
pub fn marker_severity_map() -> HashMap<String, &'static str> {
    comment_markers()
        .into_iter()
        .map(|(t, s)| (t.to_string(), s))
        .collect()
}

/// One comment-marker hit found in a file's content.
#[derive(Debug, Clone)]
pub struct MarkerHit {
    /// 1-based line number of the marker.
    pub line: u32,
    /// The uppercased marker keyword (e.g. `FIXME`).
    pub kind: String,
    /// Severity tier of the marker (`high` | `medium` | `low`).
    pub severity: &'static str,
    /// The trimmed source line the marker sits on (truncated to 200 chars).
    pub snippet: String,
}

/// Build the comment-marker regex over the catalog. Matches a marker after
/// `//`, `/*`, `#`, or `--`, or at line/word start — the same pattern the
/// `documented_tech_debt` tool uses. Compiled once per call (the cron compiles
/// it once per project sweep, the tool once per invocation).
fn comment_marker_regex() -> regex::Regex {
    let alt = comment_markers()
        .iter()
        .map(|(t, _)| *t)
        .collect::<Vec<_>>()
        .join("|");
    regex::Regex::new(&format!(
        r"(?im)(?:(?://|/\*|#|--)\s*|^|\s)({alt})(?:\([^)]*\))?:?\s*([^\n]*)"
    ))
    .expect("comment marker regex")
}

/// Scan `content` for comment markers, returning every hit whose severity tier
/// is in `severities` (e.g. `&["high"]` for the cron). The line/snippet
/// extraction matches the `documented_tech_debt` tool exactly so the two never
/// disagree about *where* a marker is.
pub fn scan_comment_markers(content: &str, severities: &[&str]) -> Vec<MarkerHit> {
    let re = comment_marker_regex();
    let allow = marker_severity_map();
    let mut hits: Vec<MarkerHit> = Vec::new();
    for cap in re.captures_iter(content) {
        let Some(kind_match) = cap.get(1) else {
            continue;
        };
        let kind_upper = kind_match.as_str().to_uppercase();
        let Some(&severity) = allow.get(&kind_upper) else {
            continue;
        };
        if !severities.contains(&severity) {
            continue;
        }
        let line_no = content[..kind_match.start()]
            .bytes()
            .filter(|b| *b == b'\n')
            .count() as u32
            + 1;
        let line_start = content[..kind_match.start()]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let line_end = content[kind_match.start()..]
            .find('\n')
            .map(|i| kind_match.start() + i)
            .unwrap_or(content.len());
        let snippet = truncate(content[line_start..line_end].trim(), 200);
        hits.push(MarkerHit {
            line: line_no,
            kind: kind_upper,
            severity,
            snippet,
        });
    }
    hits
}

/// Truncate `s` to at most `max` chars, appending an ellipsis when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Per-file process/structural features used by [`score_bug_files`]. Mirrors the
/// `BugRow` the `bug_prediction` tool selects from `file_metrics`.
#[derive(Debug, Clone)]
pub struct BugFeatures {
    pub relative_path: String,
    pub language: String,
    pub line_count: i32,
    pub churn_rate: Option<f64>,
    pub fix_commit_ratio: Option<f64>,
    pub commit_count: Option<i32>,
    pub author_count: Option<i32>,
    pub in_degree: Option<i32>,
    pub out_degree: Option<i32>,
}

/// A scored file from [`score_bug_files`].
#[derive(Debug, Clone)]
pub struct ScoredBugFile {
    pub relative_path: String,
    pub language: String,
    /// Defect-proneness score: a trained-logreg probability (0–1) when a model
    /// fit, else the hand-weighted heuristic sum.
    pub bug_score: f64,
    pub line_count: i32,
    pub fix_ratio: f64,
}

/// Whether [`score_bug_files`] produced trained-model or heuristic scores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoreKind {
    TrainedLogreg,
    Heuristic,
}

impl ScoreKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TrainedLogreg => "trained_logreg",
            Self::Heuristic => "heuristic",
        }
    }
}

/// Score files for defect-proneness: fit a per-project logistic regression
/// (features = churn/commits/authors/in+out-degree/LOC; label = touched by a
/// bug-fix commit, `fix_commit_ratio > 0`; `fix_commit_ratio` excluded from
/// features to avoid leakage) and fall back to the hand-weighted heuristic on
/// cold start. This is the exact scoring the `bug_prediction` tool performs,
/// extracted so the `findings-promotion` cron promotes the same scores. Results
/// are sorted descending by `bug_score`.
pub fn score_bug_files(rows: &[BugFeatures]) -> (Vec<ScoredBugFile>, ScoreKind) {
    let feature_rows: Vec<Vec<f64>> = rows
        .iter()
        .map(|r| {
            vec![
                r.churn_rate.unwrap_or(0.0),
                r.commit_count.unwrap_or(0) as f64,
                r.author_count.unwrap_or(0) as f64,
                r.in_degree.unwrap_or(0) as f64,
                r.out_degree.unwrap_or(0) as f64,
                r.line_count as f64,
            ]
        })
        .collect();
    let labels: Vec<f64> = rows
        .iter()
        .map(|r| {
            if r.fix_commit_ratio.unwrap_or(0.0) > 0.0 {
                1.0
            } else {
                0.0
            }
        })
        .collect();
    let model =
        crate::code_analysis::defect_model::LogisticModel::fit(&feature_rows, &labels, 2000, 0.3);
    let score_kind = if model.is_some() {
        ScoreKind::TrainedLogreg
    } else {
        ScoreKind::Heuristic
    };

    let mut scored: Vec<ScoredBugFile> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let churn = r.churn_rate.unwrap_or(0.0);
            let fix_ratio = r.fix_commit_ratio.unwrap_or(0.0);
            let coupling = (r.in_degree.unwrap_or(0) + r.out_degree.unwrap_or(0)) as f64;
            let size_factor = (r.line_count as f64 / 100.0).min(10.0);
            let authors = r.author_count.unwrap_or(1) as f64;
            let bug_score = match &model {
                Some(m) => m.predict(&feature_rows[i]),
                None => (churn * 0.3
                    + fix_ratio * 3.0
                    + size_factor * 0.2
                    + coupling * 0.05
                    + (authors - 1.0).max(0.0) * 0.1)
                    .max(0.0),
            };
            ScoredBugFile {
                relative_path: r.relative_path.clone(),
                language: r.language.clone(),
                bug_score,
                line_count: r.line_count,
                fix_ratio,
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.bug_score
            .partial_cmp(&a.bug_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    (scored, score_kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_17_markers_with_6_high() {
        let markers = comment_markers();
        assert_eq!(markers.len(), 17, "marker catalog drifted");
        let high = markers.iter().filter(|(_, s)| *s == "high").count();
        assert_eq!(high, 6, "high tier should be FIXME/BUG/HACK/KLUDGE/WTF/XXX");
    }

    #[test]
    fn scan_finds_high_severity_only_when_filtered() {
        let content = "fn f() {\n    // FIXME: handle the empty case\n    // TODO: later\n    let x = 1; // NOTE: trivial\n}\n";
        let high = scan_comment_markers(content, &["high"]);
        assert_eq!(high.len(), 1, "only FIXME is high");
        assert_eq!(high[0].kind, "FIXME");
        assert_eq!(high[0].line, 2);
        assert!(high[0].snippet.contains("FIXME"));

        let all = scan_comment_markers(content, &["high", "medium", "low"]);
        assert_eq!(all.len(), 3, "FIXME + TODO + NOTE");
    }

    #[test]
    fn scan_empty_content_is_empty() {
        assert!(scan_comment_markers("", &["high"]).is_empty());
        assert!(scan_comment_markers("just prose, no markers", &["high"]).is_empty());
    }

    #[test]
    fn score_bug_files_sorts_descending_and_flags_kind() {
        // One file with heavy churn + high fix ratio should outrank a clean one.
        let rows = vec![
            BugFeatures {
                relative_path: "clean.rs".into(),
                language: "rust".into(),
                line_count: 50,
                churn_rate: Some(0.0),
                fix_commit_ratio: Some(0.0),
                commit_count: Some(1),
                author_count: Some(1),
                in_degree: Some(0),
                out_degree: Some(0),
            },
            BugFeatures {
                relative_path: "hot.rs".into(),
                language: "rust".into(),
                line_count: 800,
                churn_rate: Some(5.0),
                fix_commit_ratio: Some(0.6),
                commit_count: Some(40),
                author_count: Some(5),
                in_degree: Some(8),
                out_degree: Some(6),
            },
        ];
        let (scored, _kind) = score_bug_files(&rows);
        assert_eq!(scored.len(), 2);
        assert_eq!(
            scored[0].relative_path, "hot.rs",
            "the high-churn, high-fix-ratio file ranks first"
        );
        assert!(scored[0].bug_score >= scored[1].bug_score);
    }

    #[test]
    fn score_kind_str_roundtrips() {
        assert_eq!(ScoreKind::TrainedLogreg.as_str(), "trained_logreg");
        assert_eq!(ScoreKind::Heuristic.as_str(), "heuristic");
    }
}
