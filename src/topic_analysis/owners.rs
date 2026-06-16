//! `topic_owners` collector — per-topic ownership / bus-factor from git blame.
//! Joins `chunk_topic_assignments → file_chunks.blame_author` and derives, per
//! topic, the author distribution + bus factor + Herfindahl concentration.

use std::collections::BTreeMap;

use serde::Serialize;
use sqlx::PgPool;

use super::loaders::load_topic_author_lines;
use super::measures;
use super::render::{Body, Renderable, Section, View};

#[derive(Debug, Clone, Serialize)]
pub struct AuthorShare {
    pub author: String,
    pub chunks: i64,
    pub share: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopicOwnership {
    pub topic_id: i32,
    pub label: String,
    pub total_chunks: i64,
    pub distinct_authors: usize,
    /// Min authors covering ≥50% of the topic's blamed chunks (1 = single owner).
    pub bus_factor: usize,
    /// Σ author_share² ∈ (0,1]: 1 = a single owner, →1/k = evenly shared.
    pub herfindahl: f64,
    pub top_authors: Vec<AuthorShare>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OwnersReport {
    pub project: String,
    pub topics: Vec<TopicOwnership>,
}

/// Build per-topic ownership for a project. Topics are returned sorted by size;
/// `top_authors` is capped at `top_authors_n`.
pub async fn collect_topic_owners(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
    top_authors_n: usize,
) -> Result<OwnersReport, sqlx::Error> {
    let rows = load_topic_author_lines(pool, project_id).await?;

    // Group by topic, preserving label, into author → chunks.
    let mut per_topic: BTreeMap<i32, (String, Vec<AuthorShare>)> = BTreeMap::new();
    for r in rows {
        let entry = per_topic
            .entry(r.topic_id)
            .or_insert((r.label.clone(), Vec::new()));
        entry.1.push(AuthorShare {
            author: r.author,
            chunks: r.chunk_count,
            share: 0.0, // filled below
        });
    }

    let mut topics: Vec<TopicOwnership> = per_topic
        .into_iter()
        .map(|(topic_id, (label, mut authors))| {
            authors.sort_by_key(|a| std::cmp::Reverse(a.chunks));
            let total: i64 = authors.iter().map(|a| a.chunks).sum();
            let total_f = total.max(1) as f64;
            for a in &mut authors {
                a.share = a.chunks as f64 / total_f;
            }
            let counts: Vec<f64> = authors.iter().map(|a| a.chunks as f64).collect();
            let distinct_authors = authors.len();
            let bus_factor = measures::bus_factor(&counts, 0.5);
            let herfindahl = measures::herfindahl(&counts);
            authors.truncate(top_authors_n);
            TopicOwnership {
                topic_id,
                label,
                total_chunks: total,
                distinct_authors,
                bus_factor,
                herfindahl,
                top_authors: authors,
            }
        })
        .collect();
    topics.sort_by_key(|t| std::cmp::Reverse(t.total_chunks));

    Ok(OwnersReport {
        project: project_name.to_string(),
        topics,
    })
}

fn fmt_f(x: f64) -> String {
    format!("{x:.3}")
}

impl Renderable for OwnersReport {
    fn to_view(&self) -> View {
        let rows: Vec<Vec<String>> = self
            .topics
            .iter()
            .map(|t| {
                let top = t
                    .top_authors
                    .first()
                    .map(|a| format!("{} ({:.0}%)", a.author, a.share * 100.0))
                    .unwrap_or_else(|| "—".into());
                vec![
                    t.label.clone(),
                    t.distinct_authors.to_string(),
                    if t.bus_factor == 1 {
                        "1 ⚠".into()
                    } else {
                        t.bus_factor.to_string()
                    },
                    fmt_f(t.herfindahl),
                    top,
                ]
            })
            .collect();
        let single_owner = self.topics.iter().filter(|t| t.bus_factor == 1).count();
        View {
            title: format!("Topic ownership — {}", self.project),
            summary: vec![
                ("topics".into(), self.topics.len().to_string()),
                ("single_owner_topics".into(), single_owner.to_string()),
            ],
            sections: vec![Section {
                heading: "Per-topic ownership & bus factor".into(),
                body: if rows.is_empty() {
                    Body::Note(
                        "No blame data — run the git indexer (blame) so authorship is populated."
                            .into(),
                    )
                } else {
                    Body::Table {
                        headers: vec![
                            "topic".into(),
                            "authors".into(),
                            "bus_factor".into(),
                            "herfindahl".into(),
                            "top_author".into(),
                        ],
                        rows,
                    }
                },
            }],
        }
    }
}
