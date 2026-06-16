//! `topic_trends` collector — emerging vs declining themes + quality trajectory.
//! Three modes:
//! - **longitudinal** (default): per-topic chunk-count series from the
//!   `topics-size-history` cron snapshots → OLS slope per topic.
//! - **quality**: the aggregate per-scope quality metrics over
//!   `topics_quality_history` → per-metric trajectory.
//! - **chunk_age**: an immediate proxy — per-topic recent-vs-prior chunk counts
//!   by `blame_date` (cross-cutting; ignores scope).

use std::collections::BTreeMap;

use chrono::DateTime;
use serde::Serialize;
use sqlx::PgPool;

use super::loaders::load_topic_age_split;
use super::render::{Body, Renderable, Section, View};
use crate::quality::forecast::{ols_slope, pct_change};

/// Slope magnitudes below this (per day / per week) read as "flat".
const FLAT_EPS: f64 = 1e-6;

#[derive(Debug, Clone, Serialize)]
pub struct MetricTrend {
    pub name: String,
    pub latest: f64,
    pub slope_per_week: f64,
    pub pct_change: Option<f64>,
    pub direction: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThemeTrend {
    pub topic_id: i32,
    pub label: String,
    pub latest_chunks: i64,
    pub slope_per_week: f64,
    pub pct_change: Option<f64>,
    pub direction: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopicTrendsReport {
    pub scope: String,
    pub mode: String,
    pub n_points: usize,
    pub metrics: Vec<MetricTrend>,
    pub emerging: Vec<ThemeTrend>,
    pub declining: Vec<ThemeTrend>,
    pub note: Option<String>,
}

fn epoch_days(s: &str) -> Option<f64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp() as f64 / 86_400.0)
}

fn direction(slope: f64) -> String {
    if slope > FLAT_EPS {
        "growing".into()
    } else if slope < -FLAT_EPS {
        "shrinking".into()
    } else {
        "flat".into()
    }
}

/// Pure: per-topic trends from the `topics_size_history` snapshots for `scope`.
/// Unit-tested independent of the DB.
fn themes_from_size_history(snaps: &[serde_json::Value], scope: &str) -> Vec<ThemeTrend> {
    let mut series: BTreeMap<i32, (String, Vec<(f64, f64)>)> = BTreeMap::new();
    for snap in snaps {
        let Some(day) = snap.get("at").and_then(|v| v.as_str()).and_then(epoch_days) else {
            continue;
        };
        let Some(topics) = snap.get("topics").and_then(|v| v.as_array()) else {
            continue;
        };
        for t in topics {
            if t.get("scope").and_then(|v| v.as_str()) != Some(scope) {
                continue;
            }
            let tid = t.get("topic_id").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let label = t
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let cc = t.get("chunk_count").and_then(|v| v.as_f64()).unwrap_or(0.0);
            series
                .entry(tid)
                .or_insert((label, Vec::new()))
                .1
                .push((day, cc));
        }
    }

    let mut themes = Vec::new();
    for (tid, (label, pts)) in series {
        if pts.len() < 2 {
            continue;
        }
        let slope = ols_slope(&pts).unwrap_or(0.0);
        let first = pts.first().map(|p| p.1).unwrap_or(0.0);
        let last = pts.last().map(|p| p.1).unwrap_or(0.0);
        themes.push(ThemeTrend {
            topic_id: tid,
            label,
            latest_chunks: last as i64,
            slope_per_week: slope * 7.0,
            pct_change: pct_change(first, last),
            direction: direction(slope),
        });
    }
    themes
}

fn split_emerging_declining(themes: Vec<ThemeTrend>) -> (Vec<ThemeTrend>, Vec<ThemeTrend>) {
    let mut emerging: Vec<ThemeTrend> = themes
        .iter()
        .filter(|t| t.direction == "growing" || t.direction == "emerging")
        .cloned()
        .collect();
    let mut declining: Vec<ThemeTrend> = themes
        .iter()
        .filter(|t| t.direction == "shrinking" || t.direction == "declining")
        .cloned()
        .collect();
    emerging.sort_by(|a, b| {
        b.slope_per_week
            .partial_cmp(&a.slope_per_week)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    declining.sort_by(|a, b| {
        a.slope_per_week
            .partial_cmp(&b.slope_per_week)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    emerging.truncate(20);
    declining.truncate(20);
    (emerging, declining)
}

/// Collect the topic-trends report for `scope` in `mode`.
pub async fn collect_topic_trends(
    pool: &PgPool,
    scope: &str,
    mode: &str,
    recent_days: i32,
) -> Result<TopicTrendsReport, sqlx::Error> {
    let mut report = TopicTrendsReport {
        scope: scope.to_string(),
        mode: mode.to_string(),
        n_points: 0,
        metrics: Vec::new(),
        emerging: Vec::new(),
        declining: Vec::new(),
        note: None,
    };

    match mode {
        "quality" => {
            let hist = crate::db::queries::get_topics_quality_history(pool).await;
            const METRICS: [&str; 8] = [
                "npmi_coherence",
                "umass_coherence",
                "topic_diversity",
                "modularity",
                "max_topic_share",
                "distinct_label_ratio",
                "topics_per_doc_mean",
                "mean_max_membership",
            ];
            let mut series: BTreeMap<&str, Vec<(f64, f64)>> =
                METRICS.iter().map(|&m| (m, Vec::new())).collect();
            let mut points = 0;
            for e in &hist {
                if e.get("scope").and_then(|v| v.as_str()) != Some(scope) {
                    continue;
                }
                let Some(day) = e
                    .get("computed_at")
                    .and_then(|v| v.as_str())
                    .and_then(epoch_days)
                else {
                    continue;
                };
                points += 1;
                for &m in &METRICS {
                    if let Some(v) = e.get(m).and_then(|v| v.as_f64()) {
                        series.get_mut(m).expect("metric").push((day, v));
                    }
                }
            }
            report.n_points = points;
            if points < 2 {
                report.note = Some(
                    "insufficient quality history for this scope (need ≥2 snapshots; the \
                     quality-history accrues with each topic scan)."
                        .into(),
                );
                return Ok(report);
            }
            for &m in &METRICS {
                let pts = &series[m];
                if pts.len() < 2 {
                    continue;
                }
                let slope = ols_slope(pts).unwrap_or(0.0);
                let first = pts.first().map(|p| p.1).unwrap_or(0.0);
                let last = pts.last().map(|p| p.1).unwrap_or(0.0);
                report.metrics.push(MetricTrend {
                    name: m.to_string(),
                    latest: last,
                    slope_per_week: slope * 7.0,
                    pct_change: pct_change(first, last),
                    direction: direction(slope),
                });
            }
        }
        "chunk_age" => {
            let splits = load_topic_age_split(pool, recent_days.max(1)).await?;
            report.n_points = splits.len();
            let themes: Vec<ThemeTrend> = splits
                .into_iter()
                .filter(|s| s.recent + s.prior > 0)
                .map(|s| {
                    let recent = s.recent as f64;
                    let prior = s.prior as f64;
                    let dir = if recent > prior * 1.1 {
                        "emerging"
                    } else if recent * 1.1 < prior {
                        "declining"
                    } else {
                        "flat"
                    };
                    ThemeTrend {
                        topic_id: s.topic_id,
                        label: s.label,
                        latest_chunks: s.recent,
                        slope_per_week: recent - prior,
                        pct_change: pct_change(prior, recent),
                        direction: dir.into(),
                    }
                })
                .collect();
            let (emerging, declining) = split_emerging_declining(themes);
            report.emerging = emerging;
            report.declining = declining;
            if report.emerging.is_empty() && report.declining.is_empty() {
                report.note = Some(
                    "no blame-dated chunks (or no movement) — populate git blame for the \
                     chunk_age proxy, or use mode=longitudinal."
                        .into(),
                );
            }
        }
        _ => {
            // longitudinal (default)
            let snaps = crate::db::queries::get_topics_size_history(pool).await;
            report.n_points = snaps.len();
            if snaps.len() < 2 {
                report.note = Some(
                    "insufficient history (need ≥2 snapshots; the topics-size-history cron \
                     accrues them — trigger_cron job=\"topics-size-history\" or wait for the 6h \
                     interval)."
                        .into(),
                );
                return Ok(report);
            }
            let themes = themes_from_size_history(&snaps, scope);
            let (emerging, declining) = split_emerging_declining(themes);
            report.emerging = emerging;
            report.declining = declining;
        }
    }

    Ok(report)
}

fn fmt_f(x: f64) -> String {
    format!("{x:.3}")
}

fn theme_rows(themes: &[ThemeTrend]) -> Vec<Vec<String>> {
    themes
        .iter()
        .map(|t| {
            vec![
                t.label.clone(),
                t.latest_chunks.to_string(),
                fmt_f(t.slope_per_week),
                t.pct_change
                    .map(|p| format!("{p:+.1}%"))
                    .unwrap_or_else(|| "—".into()),
            ]
        })
        .collect()
}

impl Renderable for TopicTrendsReport {
    fn to_view(&self) -> View {
        let mut sections = Vec::new();
        if let Some(note) = &self.note {
            sections.push(Section {
                heading: "Note".into(),
                body: Body::Note(note.clone()),
            });
        }
        if !self.metrics.is_empty() {
            let rows: Vec<Vec<String>> = self
                .metrics
                .iter()
                .map(|m| {
                    vec![
                        m.name.clone(),
                        fmt_f(m.latest),
                        fmt_f(m.slope_per_week),
                        m.pct_change
                            .map(|p| format!("{p:+.1}%"))
                            .unwrap_or_else(|| "—".into()),
                        m.direction.clone(),
                    ]
                })
                .collect();
            sections.push(Section {
                heading: "Quality metric trajectory".into(),
                body: Body::Table {
                    headers: vec![
                        "metric".into(),
                        "latest".into(),
                        "slope/wk".into(),
                        "change".into(),
                        "direction".into(),
                    ],
                    rows,
                },
            });
        }
        if !self.emerging.is_empty() {
            sections.push(Section {
                heading: "Emerging themes".into(),
                body: Body::Table {
                    headers: vec![
                        "theme".into(),
                        "chunks".into(),
                        "slope/wk".into(),
                        "change".into(),
                    ],
                    rows: theme_rows(&self.emerging),
                },
            });
        }
        if !self.declining.is_empty() {
            sections.push(Section {
                heading: "Declining themes".into(),
                body: Body::Table {
                    headers: vec![
                        "theme".into(),
                        "chunks".into(),
                        "slope/wk".into(),
                        "change".into(),
                    ],
                    rows: theme_rows(&self.declining),
                },
            });
        }
        View {
            title: format!("Topic trends — {} ({})", self.scope, self.mode),
            summary: vec![("data_points".into(), self.n_points.to_string())],
            sections,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn longitudinal_detects_growth_and_decline() {
        // Two snapshots a week apart: topic 1 grows 10→30, topic 2 shrinks 40→20.
        let snaps = vec![
            json!({"at": "2026-01-01T00:00:00+00:00", "topics": [
                {"scope": "global", "topic_id": 1, "label": "rising", "chunk_count": 10},
                {"scope": "global", "topic_id": 2, "label": "falling", "chunk_count": 40},
                {"scope": "project:x", "topic_id": 3, "label": "other", "chunk_count": 5}
            ]}),
            json!({"at": "2026-01-08T00:00:00+00:00", "topics": [
                {"scope": "global", "topic_id": 1, "label": "rising", "chunk_count": 30},
                {"scope": "global", "topic_id": 2, "label": "falling", "chunk_count": 20}
            ]}),
        ];
        let themes = themes_from_size_history(&snaps, "global");
        // Only the two global topics (the project:x one is filtered out).
        assert_eq!(themes.len(), 2);
        let (emerging, declining) = split_emerging_declining(themes);
        assert_eq!(emerging.len(), 1);
        assert_eq!(emerging[0].label, "rising");
        assert!(emerging[0].slope_per_week > 0.0);
        assert_eq!(declining.len(), 1);
        assert_eq!(declining[0].label, "falling");
        assert!(declining[0].slope_per_week < 0.0);
    }

    #[test]
    fn single_snapshot_yields_no_themes() {
        let snaps = vec![json!({"at": "2026-01-01T00:00:00+00:00", "topics": [
            {"scope": "global", "topic_id": 1, "label": "a", "chunk_count": 10}
        ]})];
        assert!(themes_from_size_history(&snaps, "global").is_empty());
    }
}
