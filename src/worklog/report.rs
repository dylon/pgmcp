//! Deterministic bullet-pointed renderers for [`WorkSummaryReport`].
//!
//! `render` is coupled to the `QualityReport` model, so (per the adoption-report
//! idiom) we hand-render Markdown/Org here and reuse only the shared
//! [`crate::render::glyphs`] (sparkline blocks). The `json` rendition is the
//! serialized report (its `normalized` field echoes the resolved params).

use crate::render::{ReportFormat, glyphs};

use super::{ProjectSummary, WorkSummaryReport};

/// Render the report in `fmt` (Markdown | Org | Json).
pub fn render(report: &WorkSummaryReport, fmt: ReportFormat) -> String {
    match fmt {
        ReportFormat::Json => serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".into()),
        ReportFormat::Org => org(report),
        _ => markdown(report),
    }
}

/// Unicode block sparkline over per-day commit counts (max-normalized).
fn sparkline(daily: &[(String, u32)]) -> String {
    let max = daily.iter().map(|(_, c)| *c).max().unwrap_or(0);
    if max == 0 {
        return String::new();
    }
    let last = glyphs::SPARK.len() - 1;
    daily
        .iter()
        .map(|(_, c)| {
            let idx = (*c as f64 / max as f64 * last as f64).round() as usize;
            glyphs::SPARK[idx.min(last)]
        })
        .collect()
}

fn day(s: &str) -> &str {
    s.split('T').next().unwrap_or(s)
}

fn scopes_phrase(scopes: &[(String, u32)]) -> String {
    scopes
        .iter()
        .map(|(s, c)| format!("{s}×{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn keywords_phrase(kws: &[(String, u32)]) -> String {
    kws.iter()
        .map(|(k, _)| k.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

fn uncommitted_phrase(p: &ProjectSummary) -> Option<String> {
    let u = p.uncommitted.as_ref()?;
    if !u.dirty {
        return None;
    }
    let branch = u.branch.as_deref().unwrap_or("?");
    Some(format!(
        "{} mod / {} staged / {} untracked / {} deleted (+{}/−{}) on `{}`",
        u.modified, u.staged, u.untracked, u.deleted, u.added_lines, u.deleted_lines, branch
    ))
}

fn enrichment_phrase(p: &ProjectSummary) -> String {
    let e = &p.enrichment;
    let mut s = format!("index {}, reconciliation {}", e.freshness, e.reconciliation);
    if let Some(c) = e.indexed_commits_in_window {
        s.push_str(&format!(
            " ({c} indexed / {} live)",
            e.live_commits_in_window
        ));
    }
    if !e.topics.is_empty() {
        s.push_str(&format!("; topics: {}", e.topics.join(", ")));
    }
    s
}

fn markdown(r: &WorkSummaryReport) -> String {
    let t = &r.totals;
    let mut o = String::new();
    o.push_str(&format!("# Work Summary — {}\n\n", r.workspace_root));
    o.push_str(&format!(
        "**Period:** {} → {} · **Author:** {} · **{} projects** · **{} commits** · **{} active days** · +{}/−{}\n\n",
        day(&r.since), day(&r.until), r.author, t.projects, t.commits, t.active_days, t.added, t.deleted
    ));
    let n = &r.normalized;
    let engine = n
        .narrative_engine
        .as_deref()
        .map(|e| format!(" ({e})"))
        .unwrap_or_default();
    o.push_str(&format!(
        "<sub>group_by={} · use_graph={} · repos_scanned={} · format={} · narrative={}{}</sub>\n\n",
        n.group_by, n.use_graph, n.repos_scanned, n.format, n.narrative, engine
    ));

    let spark = sparkline(&t.daily);
    if !spark.is_empty() {
        o.push_str(&format!("**Daily cadence** `{spark}`  "));
        let busy = t
            .busiest_days
            .iter()
            .map(|(d, c)| format!("{} ({c})", day(d)))
            .collect::<Vec<_>>()
            .join(", ");
        o.push_str(&format!("busiest: {busy}\n\n"));
    }
    if !t.type_mix.is_empty() {
        o.push_str(&format!(
            "**Type mix:** {}\n\n",
            t.type_mix
                .iter()
                .map(|(ty, c)| format!("{ty} {c}"))
                .collect::<Vec<_>>()
                .join(" · ")
        ));
    }

    // Primary rollup chosen by group_by (the per-project breakdown always follows).
    match r.group_by.as_str() {
        "week" if !r.weeks.is_empty() => {
            o.push_str("## By week\n\n");
            for w in &r.weeks {
                o.push_str(&format!(
                    "- **{}** — {} commits · +{}/−{} · {} projects\n",
                    w.iso_week, w.commits, w.added, w.deleted, w.projects
                ));
            }
            o.push('\n');
        }
        "theme" if !r.themes.is_empty() => {
            o.push_str("## By theme\n\n");
            for th in &r.themes {
                let sc = scopes_phrase(&th.top_scopes);
                let tail = if sc.is_empty() {
                    String::new()
                } else {
                    format!(" — {sc}")
                };
                o.push_str(&format!(
                    "- **{}** — {} commits{}\n",
                    th.theme, th.commits, tail
                ));
            }
            o.push('\n');
        }
        _ => {}
    }

    o.push_str("## By project\n\n");
    for p in &r.projects {
        let span = match (&p.first, &p.last) {
            (Some(f), Some(l)) if f != l => format!(" · {f}→{l}"),
            (Some(f), _) => format!(" · {f}"),
            _ => String::new(),
        };
        o.push_str(&format!(
            "- **{}** — {} commits · +{}/−{}{}\n",
            p.name, p.commits, p.added, p.deleted, span
        ));
        let themes = if !p.top_scopes.is_empty() {
            scopes_phrase(&p.top_scopes)
        } else {
            keywords_phrase(&p.top_keywords)
        };
        if !themes.is_empty() {
            o.push_str(&format!("  - themes: {themes}\n"));
        }
        if let Some(narr) = &p.narrative {
            for line in narr {
                o.push_str(&format!("  - {line}\n"));
            }
        }
        for s in &p.samples {
            o.push_str(&format!("  - “{s}”\n"));
        }
        if let Some(u) = uncommitted_phrase(p) {
            o.push_str(&format!("  - uncommitted: {u}\n"));
        }
        o.push_str(&format!("  - <sub>{}</sub>\n", enrichment_phrase(p)));
    }

    o
}

fn org(r: &WorkSummaryReport) -> String {
    let t = &r.totals;
    let mut o = String::new();
    o.push_str(&format!("#+TITLE: Work Summary — {}\n\n", r.workspace_root));
    o.push_str(&format!(
        "*Period:* {} → {} | *Author:* {} | *{} projects* | *{} commits* | *{} active days* | +{}/−{}\n\n",
        day(&r.since), day(&r.until), r.author, t.projects, t.commits, t.active_days, t.added, t.deleted
    ));
    let spark = sparkline(&t.daily);
    if !spark.is_empty() {
        o.push_str(&format!("Daily cadence: {spark}\n\n"));
    }
    if !t.type_mix.is_empty() {
        o.push_str(&format!(
            "Type mix: {}\n\n",
            t.type_mix
                .iter()
                .map(|(ty, c)| format!("{ty} {c}"))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }

    if r.group_by == "week" && !r.weeks.is_empty() {
        o.push_str("* By week\n");
        for w in &r.weeks {
            o.push_str(&format!(
                "- *{}* — {} commits, +{}/−{}, {} projects\n",
                w.iso_week, w.commits, w.added, w.deleted, w.projects
            ));
        }
        o.push('\n');
    } else if r.group_by == "theme" && !r.themes.is_empty() {
        o.push_str("* By theme\n");
        for th in &r.themes {
            o.push_str(&format!("- *{}* — {} commits\n", th.theme, th.commits));
        }
        o.push('\n');
    }

    o.push_str("* By project\n");
    for p in &r.projects {
        o.push_str(&format!(
            "- *{}* — {} commits, +{}/−{}\n",
            p.name, p.commits, p.added, p.deleted
        ));
        let themes = if !p.top_scopes.is_empty() {
            scopes_phrase(&p.top_scopes)
        } else {
            keywords_phrase(&p.top_keywords)
        };
        if !themes.is_empty() {
            o.push_str(&format!("  - themes: {themes}\n"));
        }
        if let Some(narr) = &p.narrative {
            for line in narr {
                o.push_str(&format!("  - {line}\n"));
            }
        }
        for s in &p.samples {
            o.push_str(&format!("  - \"{s}\"\n"));
        }
        if let Some(u) = uncommitted_phrase(p) {
            o.push_str(&format!("  - uncommitted: {u}\n"));
        }
        o.push_str(&format!("  - {}\n", enrichment_phrase(p)));
    }

    o
}
