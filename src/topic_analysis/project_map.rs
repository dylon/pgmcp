//! `topic_project_map` collector — cross-project theme overlap from the global
//! roll-up topics: which themes are shared substrate (span many projects) vs
//! project-specific, and each project's shared/unique split.

use serde::Serialize;
use sqlx::PgPool;

use super::loaders::load_global_topic_incidence;
use super::render::{Body, Renderable, Section, View};

#[derive(Debug, Clone, Serialize)]
pub struct ThemeBreadth {
    pub topic_id: i32,
    pub label: String,
    pub keywords: Vec<String>,
    pub breadth: i32,
    pub chunk_count: i32,
    pub projects: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectThemes {
    pub project: String,
    pub shared_count: usize,
    pub unique_count: usize,
    pub unique_themes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectMapReport {
    pub theme_count: usize,
    pub themes: Vec<ThemeBreadth>,
    pub per_project: Vec<ProjectThemes>,
    /// Set when no global roll-up exists yet.
    pub guidance: Option<String>,
}

/// Build the cross-project theme map. Returns a guidance-only report when the
/// global roll-up has not been computed.
pub async fn collect_project_map(
    pool: &PgPool,
    min_breadth: i32,
) -> Result<ProjectMapReport, sqlx::Error> {
    let themes = load_global_topic_incidence(pool).await?;
    if themes.is_empty() {
        return Ok(ProjectMapReport {
            theme_count: 0,
            themes: vec![],
            per_project: vec![],
            guidance: Some(
                "No global topic roll-up found. Run discover_topics (or the \
                 topic-clustering cron) to build cross-project themes first."
                    .into(),
            ),
        });
    }

    // Per-project shared (breadth ≥ 2) vs unique (breadth == 1) tallies.
    use std::collections::BTreeMap;
    let mut shared: BTreeMap<String, usize> = BTreeMap::new();
    let mut unique: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for t in &themes {
        for p in &t.project_names {
            if t.project_count >= 2 {
                *shared.entry(p.clone()).or_default() += 1;
            } else {
                unique.entry(p.clone()).or_default().push(t.label.clone());
            }
        }
    }
    let mut all_projects: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for t in &themes {
        for p in &t.project_names {
            all_projects.insert(p.clone());
        }
    }
    let per_project: Vec<ProjectThemes> = all_projects
        .into_iter()
        .map(|p| {
            let uniq = unique.get(&p).cloned().unwrap_or_default();
            ProjectThemes {
                shared_count: shared.get(&p).copied().unwrap_or(0),
                unique_count: uniq.len(),
                unique_themes: uniq,
                project: p,
            }
        })
        .collect();

    let displayed: Vec<ThemeBreadth> = themes
        .into_iter()
        .filter(|t| t.project_count >= min_breadth)
        .map(|t| ThemeBreadth {
            topic_id: t.topic_id,
            label: t.label,
            keywords: t.keywords.unwrap_or_default(),
            breadth: t.project_count,
            chunk_count: t.chunk_count,
            projects: t.project_names,
        })
        .collect();

    Ok(ProjectMapReport {
        theme_count: displayed.len(),
        themes: displayed,
        per_project,
        guidance: None,
    })
}

impl Renderable for ProjectMapReport {
    fn to_view(&self) -> View {
        if let Some(g) = &self.guidance {
            return View {
                title: "Cross-project theme map".into(),
                summary: vec![],
                sections: vec![Section {
                    heading: "No data".into(),
                    body: Body::Note(g.clone()),
                }],
            };
        }
        let theme_rows: Vec<Vec<String>> = self
            .themes
            .iter()
            .take(40)
            .map(|t| {
                vec![
                    t.label.clone(),
                    t.breadth.to_string(),
                    t.chunk_count.to_string(),
                    t.projects
                        .iter()
                        .take(8)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                ]
            })
            .collect();
        let mut proj = self.per_project.clone();
        proj.sort_by_key(|p| std::cmp::Reverse(p.shared_count));
        let proj_rows: Vec<Vec<String>> = proj
            .iter()
            .map(|p| {
                vec![
                    p.project.clone(),
                    p.shared_count.to_string(),
                    p.unique_count.to_string(),
                    p.unique_themes
                        .iter()
                        .take(4)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" / "),
                ]
            })
            .collect();
        View {
            title: "Cross-project theme map".into(),
            summary: vec![(
                "themes_shown".into(),
                format!("{} (breadth ≥ filter)", self.theme_count),
            )],
            sections: vec![
                Section {
                    heading: "Themes by breadth (shared substrate)".into(),
                    body: Body::Table {
                        headers: vec![
                            "theme".into(),
                            "breadth".into(),
                            "chunks".into(),
                            "projects".into(),
                        ],
                        rows: theme_rows,
                    },
                },
                Section {
                    heading: "Per-project shared vs unique".into(),
                    body: Body::Table {
                        headers: vec![
                            "project".into(),
                            "shared".into(),
                            "unique".into(),
                            "unique_themes".into(),
                        ],
                        rows: proj_rows,
                    },
                },
            ],
        }
    }
}
