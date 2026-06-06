//! The graded report tree and the grade arithmetic.
//!
//! `letter_grade`/`grade_gpa` are lifted verbatim (same thresholds, same
//! `&'static str` grades) from `tool_engineering_scorecard` so both scorecards
//! and the aggregator share one implementation — see slice 3 of the plan, which
//! deletes the now-duplicate body-local copies in both scorecard tools.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::findings::{Finding, FindingCategory, Pillar, Severity};

/// 0..100 → letter. Thresholds match the historical scorecard exactly.
pub fn letter_grade(score: f64) -> &'static str {
    if score >= 90.0 {
        "A"
    } else if score >= 80.0 {
        "B"
    } else if score >= 70.0 {
        "C"
    } else if score >= 60.0 {
        "D"
    } else {
        "F"
    }
}

/// Letter → 4-point GPA.
pub fn grade_gpa(grade: &str) -> f64 {
    match grade {
        "A" => 4.0,
        "B" => 3.0,
        "C" => 2.0,
        "D" => 1.0,
        _ => 0.0,
    }
}

/// Convert a 4-point GPA back to a letter (GPA × 25 re-enters the 0..100 band).
pub fn gpa_letter(gpa: f64) -> &'static str {
    letter_grade(gpa * 25.0)
}

/// One graded dimension. `score == None` means the backing data was absent
/// (e.g. no symbol extraction, no OSV advisories); such dims render as `N/A`
/// and are excluded from the pillar mean rather than scored 0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionScore {
    pub name: String,
    pub description: String,
    /// 0..100, or `None` for data-absent (`N/A`).
    pub score: Option<f64>,
}

impl DimensionScore {
    pub fn present(name: impl Into<String>, description: impl Into<String>, score: f64) -> Self {
        DimensionScore {
            name: name.into(),
            description: description.into(),
            score: Some(score.clamp(0.0, 100.0)),
        }
    }

    /// A dimension whose data source was absent — excluded from the pillar mean.
    pub fn absent(name: impl Into<String>, description: impl Into<String>) -> Self {
        DimensionScore {
            name: name.into(),
            description: description.into(),
            score: None,
        }
    }

    pub fn grade(&self) -> Option<&'static str> {
        self.score.map(letter_grade)
    }

    /// 4-point GPA on a *continuous* linear map from the 0..100 score
    /// (`score / 25`, clamped to 0..4) — deliberately NOT the lossy
    /// score→letter→`grade_gpa` bucketing, which collapsed distinct scores
    /// (e.g. 59 and 24 both → F → 0.0) and, combined with `gpa_letter`'s
    /// `gpa × 25` re-projection, made a pillar's letter contradict its GPA.
    /// With this map the pillar/overall GPA stays proportional to the
    /// underlying scores and `gpa_letter(mean gpa) == letter_grade(mean score)`
    /// — a single absolute scale (90/80/70/60), no curve. `None` (data-absent)
    /// is preserved and excluded from the pillar mean.
    pub fn gpa(&self) -> Option<f64> {
        self.score.map(|s| (s / 25.0).clamp(0.0, 4.0))
    }
}

/// A graded pillar = a set of dimensions averaged into a GPA.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PillarReport {
    pub pillar: Pillar,
    pub dimensions: Vec<DimensionScore>,
}

impl PillarReport {
    /// Mean of the *scorable* dimensions' 4-point GPAs. `None` if every
    /// dimension is data-absent (guards the div-by-zero the plan flags).
    pub fn gpa(&self) -> Option<f64> {
        let gpas: Vec<f64> = self.dimensions.iter().filter_map(|d| d.gpa()).collect();
        if gpas.is_empty() {
            None
        } else {
            Some(gpas.iter().sum::<f64>() / gpas.len() as f64)
        }
    }

    pub fn grade(&self) -> Option<&'static str> {
        self.gpa().map(gpa_letter)
    }

    /// Lowest-scoring scorable dimension — the "biggest lever".
    pub fn weakest(&self) -> Option<&DimensionScore> {
        self.dimensions
            .iter()
            .filter(|d| d.score.is_some())
            .min_by(|a, b| {
                a.score
                    .unwrap()
                    .partial_cmp(&b.score.unwrap())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }
}

/// One gate of the Operational Readiness Review (Engineering pillar only).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrrGate {
    pub name: String,
    pub pass: bool,
}

/// How a source tool fared during aggregation — kept distinct so a clean-looking
/// pillar can be told apart from one that was merely starved of data or slow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    /// Ran to completion (may have produced 0 findings — genuinely clean).
    Ran,
    /// Ran but its data source was absent (no advisories, no symbol extraction…).
    DataUnavailable,
    /// Errored or exceeded its timeout.
    ErroredOrTimedOut,
}

/// One appendix row: which tool ran, how it fared, how many findings, how long.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRun {
    pub tool: String,
    pub category: FindingCategory,
    pub finding_count: usize,
    pub millis: u64,
    pub outcome: ToolOutcome,
    /// Present when `outcome != Ran` — the reason, for the appendix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Per-pillar GPA history (oldest → newest) for the trend strip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PillarTrend {
    pub pillar: Pillar,
    pub gpas: Vec<f64>,
}

impl PillarTrend {
    /// 3-point EWMA so a single stale-cron run doesn't render as a spike.
    /// `span` 3 → alpha 0.5. Returns a smoothed series the same length as input.
    pub fn ewma(&self, span: usize) -> Vec<f64> {
        if self.gpas.is_empty() {
            return Vec::new();
        }
        let alpha = 2.0 / (span.max(1) as f64 + 1.0);
        let mut out = Vec::with_capacity(self.gpas.len());
        let mut acc = self.gpas[0];
        out.push(acc);
        for &x in &self.gpas[1..] {
            acc = alpha * x + (1.0 - alpha) * acc;
            out.push(acc);
        }
        out
    }

    /// The most recent two raw points as (previous, latest) for the delta column.
    pub fn delta(&self) -> Option<(f64, f64)> {
        match self.gpas.len() {
            0 | 1 => None,
            n => Some((self.gpas[n - 2], self.gpas[n - 1])),
        }
    }
}

/// One row of the "worst files" roll-up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopIssue {
    pub path: String,
    pub weighted: f64,
    pub count: usize,
    pub worst: Severity,
}

/// Render-time knobs captured from the tool params; not part of the persisted
/// report (skipped from serialization).
#[derive(Debug, Clone)]
pub struct ReportOptions {
    pub include_findings: bool,
    /// Internal execution knob: when false, the aggregate skips the finding
    /// collectors entirely and marks finding-backed dimensions as N/A. Public
    /// reports keep this true; the quality-history cron sets it false to
    /// snapshot GPAs without retaining whole finding payloads.
    pub compute_findings: bool,
    pub include_recommended_fixes: bool,
    /// Display floor — findings below this severity are hidden (default Low, so
    /// Info-severity placeholders stay in the appendix but out of the list).
    pub min_severity: Severity,
    pub trend_points: usize,
    pub top_n: usize,
}

impl Default for ReportOptions {
    fn default() -> Self {
        ReportOptions {
            include_findings: true,
            compute_findings: true,
            include_recommended_fixes: true,
            min_severity: Severity::Low,
            trend_points: 12,
            top_n: 10,
        }
    }
}

/// The fully-assembled, graded report. Built by `super::aggregate::aggregate`,
/// consumed by `crate::render`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityReport {
    pub project: String,
    pub computed_at: DateTime<Utc>,
    pub pgmcp_version: String,
    pub pillars: Vec<PillarReport>,
    pub findings: Vec<Finding>,
    pub orr: Vec<OrrGate>,
    pub effect_breakdown: serde_json::Value,
    pub tool_runs: Vec<ToolRun>,
    pub trend: Vec<PillarTrend>,
    /// Render knobs — not serialized (the renderer reads them; persistence and
    /// the JSON envelope don't need them).
    #[serde(skip)]
    pub options: ReportOptions,
}

impl QualityReport {
    pub fn pillar(&self, p: Pillar) -> Option<&PillarReport> {
        self.pillars.iter().find(|pr| pr.pillar == p)
    }

    pub fn trend_for(&self, p: Pillar) -> Option<&PillarTrend> {
        self.trend.iter().find(|t| t.pillar == p)
    }

    /// Overall GPA = unweighted mean of the pillar GPAs that are scorable.
    /// `None` if no pillar could be graded.
    pub fn overall_gpa(&self) -> Option<f64> {
        let gpas: Vec<f64> = self.pillars.iter().filter_map(|p| p.gpa()).collect();
        if gpas.is_empty() {
            None
        } else {
            Some(gpas.iter().sum::<f64>() / gpas.len() as f64)
        }
    }

    pub fn overall_grade(&self) -> Option<&'static str> {
        self.overall_gpa().map(gpa_letter)
    }

    pub fn orr_pass(&self) -> bool {
        !self.orr.is_empty() && self.orr.iter().all(|g| g.pass)
    }

    /// Findings at or above the display floor, sorted by severity (desc) then
    /// path. Used by the category sections and recomputed counts.
    pub fn displayed_findings(&self) -> Vec<&Finding> {
        let floor = self.options.min_severity.rank();
        let mut v: Vec<&Finding> = self
            .findings
            .iter()
            .filter(|f| f.severity_rank >= floor)
            .collect();
        v.sort_by(|a, b| {
            b.severity_rank
                .cmp(&a.severity_rank)
                .then_with(|| a.location_label().cmp(&b.location_label()))
        });
        v
    }

    pub fn displayed_in_category(&self, cat: FindingCategory) -> Vec<&Finding> {
        self.displayed_findings()
            .into_iter()
            .filter(|f| f.category == cat)
            .collect()
    }

    /// "Worst files" roll-up over the *displayed* (filtered) findings, so the
    /// roll-up and the category counts agree. Findings without a concrete path
    /// (project/cluster-level) are skipped.
    pub fn top_issues(&self) -> Vec<TopIssue> {
        use std::collections::HashMap;
        let mut by_path: HashMap<String, (f64, usize, Severity)> = HashMap::new();
        for f in self.displayed_findings() {
            let path = match &f.location {
                Some(loc) => loc.path.clone(),
                None => continue,
            };
            let entry = by_path.entry(path).or_insert((0.0, 0, Severity::Info));
            entry.0 += f.severity.weight();
            entry.1 += 1;
            if f.severity.rank() > entry.2.rank() {
                entry.2 = f.severity;
            }
        }
        let mut rows: Vec<TopIssue> = by_path
            .into_iter()
            .map(|(path, (weighted, count, worst))| TopIssue {
                path,
                weighted,
                count,
                worst,
            })
            .collect();
        rows.sort_by(|a, b| {
            b.weighted
                .partial_cmp(&a.weighted)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.count.cmp(&a.count))
        });
        rows.truncate(self.options.top_n);
        rows
    }
}

/// Severity-weighted `finding_density` for one pillar: `100·(1 − clamp(Σ weight
/// / files, 0, 1))`. Computed over ALL findings mapped to the pillar (not the
/// display-filtered set) so the grade reflects every issue. `file_count == 0`
/// yields a perfect 100 (an empty project has no density).
pub fn finding_density(findings: &[Finding], pillar: Pillar, file_count: i64) -> f64 {
    if file_count <= 0 {
        return 100.0;
    }
    // De-duplicate by file: a file flagged by several collectors counts ONCE, at
    // its worst severity, so a ranking-style collector that touches many files
    // can no longer saturate the density (the artifact that floored Engineering
    // to 0.0). Project-/cluster-level findings (no location) are genuinely
    // distinct cross-cutting issues and each contribute.
    use std::collections::HashMap;
    let mut worst_per_file: HashMap<&str, f64> = HashMap::new();
    let mut unlocated_weight: f64 = 0.0;
    for f in findings.iter().filter(|f| f.category.pillar() == pillar) {
        let w = f.severity.weight();
        match &f.location {
            Some(loc) => {
                let slot = worst_per_file.entry(loc.path.as_str()).or_insert(0.0);
                if w > *slot {
                    *slot = w;
                }
            }
            None => unlocated_weight += w,
        }
    }
    let weighted: f64 = worst_per_file.values().sum::<f64>() + unlocated_weight;
    100.0 * (1.0 - (weighted / file_count as f64).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quality::findings::Finding;

    #[test]
    fn letter_and_gpa_round_trip_at_boundaries() {
        assert_eq!(letter_grade(90.0), "A");
        assert_eq!(letter_grade(89.999), "B");
        assert_eq!(letter_grade(0.0), "F");
        assert_eq!(grade_gpa("A"), 4.0);
        assert_eq!(grade_gpa("F"), 0.0);
        assert_eq!(gpa_letter(4.0), "A");
        assert_eq!(gpa_letter(0.0), "F");
    }

    #[test]
    fn all_absent_pillar_is_na_not_zero() {
        let p = PillarReport {
            pillar: Pillar::Security,
            dimensions: vec![
                DimensionScore::absent("a", ""),
                DimensionScore::absent("b", ""),
            ],
        };
        assert_eq!(p.gpa(), None, "all-absent pillar must be N/A");
        assert_eq!(p.grade(), None);
    }

    #[test]
    fn pillar_mean_ignores_absent_dims() {
        let p = PillarReport {
            pillar: Pillar::Architecture,
            dimensions: vec![
                DimensionScore::present("a", "", 95.0), // 95/25 = 3.8
                DimensionScore::absent("b", ""),        // ignored
                DimensionScore::present("c", "", 85.0), // 85/25 = 3.4
            ],
        };
        // Continuous GPA: mean(3.8, 3.4) = 3.6 (not the old letter-bucketed 3.5).
        assert!((p.gpa().unwrap() - 3.6).abs() < 1e-9);
        assert_eq!(p.weakest().map(|d| d.name.as_str()), Some("c"));
    }

    #[test]
    fn finding_density_penalizes_critical() {
        let crit = Finding::new(
            "secret_detection",
            FindingCategory::Security,
            "p",
            Severity::Critical,
            "x",
        );
        let many = vec![crit.clone(), crit.clone(), crit.clone()];
        // 3 × weight 10 = 30 over 100 files → 0.30 → score 70.
        assert!((finding_density(&many, Pillar::Security, 100) - 70.0).abs() < 1e-9);
        // Empty project → perfect.
        assert_eq!(finding_density(&many, Pillar::Security, 0), 100.0);
        // Wrong pillar → no penalty.
        assert_eq!(finding_density(&many, Pillar::Engineering, 100), 100.0);
    }

    #[test]
    fn ewma_smooths_a_spike() {
        let t = PillarTrend {
            pillar: Pillar::Engineering,
            gpas: vec![3.0, 3.0, 0.0, 3.0],
        };
        let s = t.ewma(3);
        assert_eq!(s.len(), 4);
        // The spike at index 2 is damped, not reproduced verbatim.
        assert!(s[2] > 0.0 && s[2] < 3.0);
        assert_eq!(t.delta(), Some((0.0, 3.0)));
    }
}
