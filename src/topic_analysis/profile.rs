//! `project_topic_profile` collector — per-project topic fingerprint: the topic
//! histogram, a specialization index (normalized Shannon entropy + Gini), the
//! dominant topics, and the project's stored coherence metrics.

use serde::Serialize;
use sqlx::PgPool;

use super::loaders::load_project_topic_histogram;
use super::measures;
use super::render::{Body, Renderable, Section, View};

#[derive(Debug, Clone, Serialize)]
pub struct TopicShare {
    pub topic_id: i32,
    pub label: String,
    pub keywords: Vec<String>,
    pub chunk_count: i64,
    pub share: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopicCoherence {
    pub npmi_coherence: Option<f64>,
    pub topic_diversity: Option<f64>,
    pub mean_max_membership: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectTopicProfile {
    pub project: String,
    pub n_topics: usize,
    pub total_chunks: i64,
    /// 1 − normalized entropy: 1 = single-theme specialist, 0 = even generalist.
    pub specialization_index: f64,
    pub shannon_norm: f64,
    pub gini: f64,
    pub coherence: Option<TopicCoherence>,
    pub top_topics: Vec<TopicShare>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileReport {
    pub projects: Vec<ProjectTopicProfile>,
}

/// Extract a project's coherence sub-object from the workspace `topics_quality`
/// JSON (keyed by scope `project:NAME`).
fn coherence_for(quality: &Option<serde_json::Value>, project: &str) -> Option<TopicCoherence> {
    let key = format!("project:{project}");
    let obj = quality.as_ref()?.get(&key)?;
    let f = |k: &str| obj.get(k).and_then(serde_json::Value::as_f64);
    Some(TopicCoherence {
        npmi_coherence: f("npmi_coherence"),
        topic_diversity: f("topic_diversity"),
        mean_max_membership: f("mean_max_membership"),
    })
}

/// Build one project's profile from its topic histogram + the shared quality JSON.
pub async fn collect_project_profile(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
    top_n: usize,
    quality: &Option<serde_json::Value>,
) -> Result<ProjectTopicProfile, sqlx::Error> {
    let hist = load_project_topic_histogram(pool, project_id).await?;
    let counts: Vec<f64> = hist.iter().map(|r| r.chunk_count as f64).collect();
    let total: i64 = hist.iter().map(|r| r.chunk_count).sum();
    let total_f = total.max(1) as f64;

    let top_topics = hist
        .iter()
        .take(top_n)
        .map(|r| TopicShare {
            topic_id: r.topic_id,
            label: r.label.clone(),
            keywords: r.keywords.clone().unwrap_or_default(),
            chunk_count: r.chunk_count,
            share: r.chunk_count as f64 / total_f,
        })
        .collect();

    Ok(ProjectTopicProfile {
        project: project_name.to_string(),
        n_topics: hist.len(),
        total_chunks: total,
        specialization_index: measures::specialization_index(&counts),
        shannon_norm: measures::normalized_entropy(&counts),
        gini: measures::gini(&counts),
        coherence: coherence_for(quality, project_name),
        top_topics,
    })
}

fn fmt_f(x: f64) -> String {
    format!("{x:.3}")
}

impl Renderable for ProfileReport {
    fn to_view(&self) -> View {
        // Single project → detailed; many → comparison table sorted by focus.
        if self.projects.len() == 1 {
            let p = &self.projects[0];
            let mut summary = vec![
                ("project".into(), p.project.clone()),
                ("topics".into(), p.n_topics.to_string()),
                ("chunks".into(), p.total_chunks.to_string()),
                ("specialization_index".into(), fmt_f(p.specialization_index)),
                ("gini".into(), fmt_f(p.gini)),
            ];
            if let Some(n) = p.coherence.as_ref().and_then(|c| c.npmi_coherence) {
                summary.push(("npmi_coherence".into(), fmt_f(n)));
            }
            let rows: Vec<Vec<String>> = p
                .top_topics
                .iter()
                .map(|t| {
                    vec![
                        t.label.clone(),
                        format!("{:.1}%", t.share * 100.0),
                        t.chunk_count.to_string(),
                        t.keywords.join(" / "),
                    ]
                })
                .collect();
            View {
                title: format!("Topic profile — {}", p.project),
                summary,
                sections: vec![Section {
                    heading: "Dominant topics".into(),
                    body: Body::Table {
                        headers: vec![
                            "topic".into(),
                            "share".into(),
                            "chunks".into(),
                            "keywords".into(),
                        ],
                        rows,
                    },
                }],
            }
        } else {
            let mut sorted = self.projects.clone();
            sorted.sort_by(|a, b| {
                b.specialization_index
                    .partial_cmp(&a.specialization_index)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let rows: Vec<Vec<String>> = sorted
                .iter()
                .map(|p| {
                    vec![
                        p.project.clone(),
                        p.n_topics.to_string(),
                        p.total_chunks.to_string(),
                        fmt_f(p.specialization_index),
                        fmt_f(p.gini),
                        p.coherence
                            .as_ref()
                            .and_then(|c| c.npmi_coherence)
                            .map(fmt_f)
                            .unwrap_or_else(|| "—".into()),
                    ]
                })
                .collect();
            View {
                title: format!("Topic profiles — {} projects", self.projects.len()),
                summary: vec![],
                sections: vec![Section {
                    heading: "By specialization (focused → broad)".into(),
                    body: Body::Table {
                        headers: vec![
                            "project".into(),
                            "topics".into(),
                            "chunks".into(),
                            "specialization".into(),
                            "gini".into(),
                            "npmi".into(),
                        ],
                        rows,
                    },
                }],
            }
        }
    }
}
