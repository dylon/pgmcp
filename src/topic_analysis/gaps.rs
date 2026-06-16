//! `topic_coverage_gaps` collector — where the topic model is weak: orphan
//! chunks (uncategorized code), thin topics (too few chunks), and low-cohesion
//! topics, per project.

use serde::Serialize;
use sqlx::PgPool;

use super::loaders::load_project_topic_histogram;
use super::render::{Body, Renderable, Section, View};

#[derive(Debug, Clone, Serialize)]
pub struct OrphanFile {
    pub path: String,
    pub orphan_chunks: i64,
    pub total_chunks: i64,
    pub orphan_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThinTopic {
    pub topic_id: i32,
    pub label: String,
    pub chunk_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LowCohesionTopic {
    pub topic_id: i32,
    pub label: String,
    pub avg_internal_similarity: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectCoverageGaps {
    pub project: String,
    pub orphan_chunk_count: i64,
    pub orphan_files: Vec<OrphanFile>,
    pub thin_topics: Vec<ThinTopic>,
    pub low_cohesion_topics: Vec<LowCohesionTopic>,
    pub scope_npmi: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoverageGapsReport {
    pub projects: Vec<ProjectCoverageGaps>,
}

/// Build one project's coverage-gap report.
pub async fn collect_project_gaps(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
    thin_threshold: i64,
    low_sim: f64,
    quality: &Option<serde_json::Value>,
) -> Result<ProjectCoverageGaps, sqlx::Error> {
    // Orphans (uncategorized chunks), via the existing file-summary query.
    let summary = crate::db::queries::find_orphan_file_summary(pool, Some(project_name)).await?;
    let orphan_chunk_count: i64 = summary.iter().map(|f| f.orphan_chunks).sum();
    let mut orphan_files: Vec<OrphanFile> = summary
        .into_iter()
        .filter(|f| f.orphan_chunks > 0)
        .map(|f| OrphanFile {
            path: f.path,
            orphan_chunks: f.orphan_chunks,
            total_chunks: f.total_chunks,
            orphan_pct: f.orphan_pct,
        })
        .collect();
    orphan_files.sort_by(|a, b| {
        b.orphan_pct
            .partial_cmp(&a.orphan_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    orphan_files.truncate(20);

    // Thin / low-cohesion topics from the per-project histogram.
    let hist = load_project_topic_histogram(pool, project_id).await?;
    let mut thin_topics: Vec<ThinTopic> = hist
        .iter()
        .filter(|r| r.chunk_count < thin_threshold)
        .map(|r| ThinTopic {
            topic_id: r.topic_id,
            label: r.label.clone(),
            chunk_count: r.chunk_count,
        })
        .collect();
    thin_topics.sort_by_key(|t| t.chunk_count);
    let mut low_cohesion_topics: Vec<LowCohesionTopic> = hist
        .iter()
        .filter_map(|r| {
            r.avg_internal_similarity
                .filter(|&s| s < low_sim)
                .map(|s| LowCohesionTopic {
                    topic_id: r.topic_id,
                    label: r.label.clone(),
                    avg_internal_similarity: s,
                })
        })
        .collect();
    low_cohesion_topics.sort_by(|a, b| {
        a.avg_internal_similarity
            .partial_cmp(&b.avg_internal_similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let scope_npmi = quality
        .as_ref()
        .and_then(|q| q.get(format!("project:{project_name}")))
        .and_then(|o| o.get("npmi_coherence"))
        .and_then(serde_json::Value::as_f64);

    Ok(ProjectCoverageGaps {
        project: project_name.to_string(),
        orphan_chunk_count,
        orphan_files,
        thin_topics,
        low_cohesion_topics,
        scope_npmi,
    })
}

fn fmt_f(x: f64) -> String {
    format!("{x:.3}")
}

impl Renderable for CoverageGapsReport {
    fn to_view(&self) -> View {
        if self.projects.len() == 1 {
            let p = &self.projects[0];
            let orphan_rows: Vec<Vec<String>> = p
                .orphan_files
                .iter()
                .map(|f| {
                    vec![
                        f.path.clone(),
                        f.orphan_chunks.to_string(),
                        format!("{:.1}%", f.orphan_pct),
                    ]
                })
                .collect();
            let thin_rows: Vec<Vec<String>> = p
                .thin_topics
                .iter()
                .map(|t| vec![t.label.clone(), t.chunk_count.to_string()])
                .collect();
            let lowc_rows: Vec<Vec<String>> = p
                .low_cohesion_topics
                .iter()
                .map(|t| vec![t.label.clone(), fmt_f(t.avg_internal_similarity)])
                .collect();
            View {
                title: format!("Topic coverage gaps — {}", p.project),
                summary: vec![
                    ("orphan_chunks".into(), p.orphan_chunk_count.to_string()),
                    ("thin_topics".into(), p.thin_topics.len().to_string()),
                    (
                        "scope_npmi".into(),
                        p.scope_npmi.map(fmt_f).unwrap_or_else(|| "—".into()),
                    ),
                ],
                sections: vec![
                    Section {
                        heading: "Files with uncategorized (orphan) chunks".into(),
                        body: if orphan_rows.is_empty() {
                            Body::Note("None — every chunk is assigned to a topic.".into())
                        } else {
                            Body::Table {
                                headers: vec!["file".into(), "orphans".into(), "orphan_pct".into()],
                                rows: orphan_rows,
                            }
                        },
                    },
                    Section {
                        heading: "Thin topics (few chunks)".into(),
                        body: if thin_rows.is_empty() {
                            Body::Note("None.".into())
                        } else {
                            Body::Table {
                                headers: vec!["topic".into(), "chunks".into()],
                                rows: thin_rows,
                            }
                        },
                    },
                    Section {
                        heading: "Low-cohesion topics".into(),
                        body: if lowc_rows.is_empty() {
                            Body::Note(
                                "None below the cohesion threshold (or cohesion not computed)."
                                    .into(),
                            )
                        } else {
                            Body::Table {
                                headers: vec!["topic".into(), "avg_internal_similarity".into()],
                                rows: lowc_rows,
                            }
                        },
                    },
                ],
            }
        } else {
            let mut rows: Vec<Vec<String>> = self
                .projects
                .iter()
                .map(|p| {
                    vec![
                        p.project.clone(),
                        p.orphan_chunk_count.to_string(),
                        p.thin_topics.len().to_string(),
                        p.low_cohesion_topics.len().to_string(),
                    ]
                })
                .collect();
            rows.sort_by(|a, b| {
                b[1].parse::<i64>()
                    .unwrap_or(0)
                    .cmp(&a[1].parse::<i64>().unwrap_or(0))
            });
            View {
                title: format!("Topic coverage gaps — {} projects", self.projects.len()),
                summary: vec![],
                sections: vec![Section {
                    heading: "By orphan count".into(),
                    body: Body::Table {
                        headers: vec![
                            "project".into(),
                            "orphan_chunks".into(),
                            "thin_topics".into(),
                            "low_cohesion".into(),
                        ],
                        rows,
                    },
                }],
            }
        }
    }
}
