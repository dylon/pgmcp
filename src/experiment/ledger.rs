//! Render a structured experiment record to a committed markdown "scientific
//! ledger" (`docs/scientific-ledger/<slug>-<date>.md`). The structured record
//! is the source of truth; this is the rendered, git-tracked, human-readable,
//! and re-indexable view. A leading YAML frontmatter block carries
//! `pgmcp_experiment: <slug>` (the join key back to the DB row) — see
//! `crate::indexer::frontmatter`.
//!
//! Section structure mirrors the hand-authored ledgers under
//! `docs/scientific-ledger/` so generated and legacy ledgers chunk identically
//! once markdown heading-aware chunking is in effect.

use std::path::{Component, Path, PathBuf};

use sqlx::PgPool;
use uuid::Uuid;

use crate::db::queries::{
    self, ExperimentCoreRow, ExperimentEvent, ExperimentHypothesisRow, ExperimentResultRow,
};

/// The outcome of a render.
#[derive(Debug, Clone)]
pub struct RenderedLedger {
    pub path: PathBuf,
    pub content: String,
    pub written: bool,
}

fn fmt_opt_f64(v: Option<f64>) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{x:.6}"),
        _ => "n/a".to_string(),
    }
}

fn safe_ledger_dir(raw: &str) -> anyhow::Result<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        anyhow::bail!("ledger_dir must be non-empty");
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        anyhow::bail!("ledger_dir must be relative");
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("ledger_dir must not contain parent/root components")
            }
        }
    }
    if out.as_os_str().is_empty() {
        anyhow::bail!("ledger_dir must contain at least one normal component");
    }
    Ok(out)
}

fn safe_ledger_slug(raw: &str) -> anyhow::Result<String> {
    let slug = raw.trim();
    if slug.is_empty() {
        anyhow::bail!("experiment slug must be non-empty");
    }
    if slug.len() > 160 {
        anyhow::bail!("experiment slug is too long for a ledger filename");
    }
    if !slug
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        anyhow::bail!(
            "experiment slug must contain only ASCII letters, digits, '-' or '_' for ledger rendering"
        );
    }
    Ok(slug.to_string())
}

fn atomic_write(path: &Path, content: &str) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("ledger path has no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("ledger path has no file name"))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    if let Err(e) = std::fs::write(&tmp, content).and_then(|_| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Render the markdown body (pure; no I/O) from the fetched record.
pub fn render_markdown(
    core: &ExperimentCoreRow,
    hypotheses: &[ExperimentHypothesisRow],
    results: &[ExperimentResultRow],
    events: &[ExperimentEvent],
) -> String {
    let date = core.created_at.format("%Y-%m-%d").to_string();
    // Headline verdict + p: the most recent decision, if any.
    let latest = results.first();
    let headline_verdict = latest
        .map(|r| r.verdict.clone())
        .or_else(|| hypotheses.first().map(|h| h.verdict.clone()))
        .unwrap_or_else(|| "pending".to_string());
    let headline_p = latest.and_then(|r| r.p_value);

    let mut s = String::with_capacity(2048);

    // ── Frontmatter ──
    s.push_str("---\n");
    s.push_str(&format!("pgmcp_experiment: {}\n", core.slug));
    s.push_str(&format!("title: {}\n", core.title.replace('\n', " ")));
    s.push_str(&format!("date: {date}\n"));
    s.push_str(&format!(
        "project: {}\n",
        core.project.as_deref().unwrap_or("workspace")
    ));
    s.push_str(&format!("kind: {}\n", core.kind));
    s.push_str(&format!("status: {}\n", core.status));
    s.push_str(&format!("verdict: {headline_verdict}\n"));
    s.push_str(&format!("p_value: {}\n", fmt_opt_f64(headline_p)));
    if let Some(plan) = &core.plan_ref {
        s.push_str(&format!("plan: {plan}\n"));
    }
    if let Some(git) = &core.git_ref {
        s.push_str(&format!("git_ref: {git}\n"));
    }
    s.push_str("---\n\n");

    // ── Title + question ──
    s.push_str(&format!("# {}\n\n", core.title));
    s.push_str(&format!(
        "**Kind:** {}  |  **Status:** {}  |  **Correction:** {}\n\n",
        core.kind, core.status, core.correction
    ));

    s.push_str("## Method\n\n");
    s.push_str(&format!("**Question:** {}\n\n", core.question));
    if let Some(ctx) = &core.context
        && !ctx.trim().is_empty()
    {
        s.push_str(&format!("{ctx}\n\n"));
    }

    // ── Hypotheses (with verdicts + frozen criteria) ──
    s.push_str("## Hypotheses\n\n");
    for (i, h) in hypotheses.iter().enumerate() {
        let tag = match h.verdict.as_str() {
            "accepted" => "✅ accepted",
            "rejected" => "❌ rejected",
            "inconclusive" => "❔ inconclusive",
            _ => "… pending",
        };
        s.push_str(&format!("**H{}.** {} — *{}*\n\n", i + 1, h.statement, tag));
        s.push_str(&format!(
            "- metric: `{}`{} · predicted: {} · planned n/arm: {}\n",
            h.primary_metric,
            h.unit
                .as_deref()
                .map(|u| format!(" ({u})"))
                .unwrap_or_default(),
            h.predicted_direction,
            h.planned_n
                .map(|n| n.to_string())
                .unwrap_or_else(|| "—".to_string()),
        ));
        s.push_str(&format!(
            "- pre-registered criterion (locked {}): `{}`\n\n",
            h.criterion_locked_at.format("%Y-%m-%d %H:%M:%SZ"),
            h.acceptance_criterion_json,
        ));
    }

    // ── Measurements / Decisions (now WITH p-values, CIs, effect sizes) ──
    s.push_str("## Measurements & Decisions\n\n");
    if results.is_empty() {
        s.push_str("_No decisions recorded yet._\n\n");
    } else {
        s.push_str("| Metric | Test | Statistic | p | Effect | 95% CI | Verdict |\n");
        s.push_str("|--------|------|-----------|---|--------|--------|--------|\n");
        for r in results {
            let ci = match (r.ci_low, r.ci_high) {
                (Some(lo), Some(hi)) if lo.is_finite() && hi.is_finite() => {
                    format!("[{lo:.4}, {hi:.4}]")
                }
                _ => "—".to_string(),
            };
            s.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {} | {} |\n",
                r.metric_name,
                r.test_type,
                fmt_opt_f64(r.statistic),
                fmt_opt_f64(r.p_value),
                fmt_opt_f64(r.effect_size),
                ci,
                r.verdict,
            ));
        }
        s.push('\n');
        for r in results {
            if let Some(rat) = &r.rationale
                && !rat.trim().is_empty()
            {
                s.push_str(&format!(
                    "**Decision on `{}`:**\n\n{}\n\n",
                    r.metric_name, rat
                ));
            }
        }
    }

    // ── What did NOT work (rejected/inconclusive hypotheses) ──
    let not_worked: Vec<&ExperimentResultRow> =
        results.iter().filter(|r| r.verdict != "accepted").collect();
    s.push_str("## What did NOT work\n\n");
    if not_worked.is_empty() {
        s.push_str("_Nothing rejected (or no decisions yet)._\n\n");
    } else {
        for r in not_worked {
            s.push_str(&format!(
                "- `{}`: {} (test={}, p={})\n",
                r.metric_name,
                r.verdict,
                r.test_type,
                fmt_opt_f64(r.p_value)
            ));
        }
        s.push('\n');
    }

    // ── Reproducibility / timeline ──
    s.push_str("## Reproducibility\n\n");
    if let Some(git) = &core.git_ref {
        s.push_str(&format!("- git ref: `{git}`\n"));
    }
    s.push_str("- See each hypothesis's pre-registered criterion above; raw samples are retained in `experiment_samples`.\n\n");

    s.push_str("## Timeline\n\n");
    for e in events {
        s.push_str(&format!(
            "- {} — **{}**: {}\n",
            e.at.format("%Y-%m-%d %H:%M:%SZ"),
            e.event,
            e.detail
        ));
    }
    s.push('\n');

    s.push_str("---\n");
    s.push_str("_Rendered from the pgmcp experiment record (the structured source of truth). Edit the experiment, not this file._\n");
    s
}

/// Fetch the record, render it, and (unless `dry_run`) write it to
/// `<ledger_dir>/<slug>-<date>.md` relative to `base_dir`. Returns the path
/// and content.
pub async fn render_and_write(
    pool: &PgPool,
    experiment_id: Option<i64>,
    slug: Option<&str>,
    ledger_dir: &str,
    base_dir: &Path,
    dry_run: bool,
) -> anyhow::Result<RenderedLedger> {
    if let Some(id) = experiment_id
        && id <= 0
    {
        anyhow::bail!("experiment_id must be positive");
    }
    let slug = slug.map(str::trim).filter(|s| !s.is_empty());
    if experiment_id.is_none() && slug.is_none() {
        anyhow::bail!("experiment_id or non-empty slug is required");
    }
    let ledger_dir = safe_ledger_dir(ledger_dir)?;
    let core = queries::get_experiment_core(pool, experiment_id, slug)
        .await?
        .ok_or_else(|| anyhow::anyhow!("experiment not found"))?;
    let slug = safe_ledger_slug(&core.slug)?;
    let hyps = queries::list_experiment_hypotheses(pool, core.id).await?;
    let results = queries::list_experiment_results(pool, core.id).await?;
    let events = queries::experiment_timeline(pool, core.id).await?;

    let content = render_markdown(&core, &hyps, &results, &events);
    let date = core.created_at.format("%Y-%m-%d").to_string();
    let dir = base_dir.join(ledger_dir);
    let path = dir.join(format!("{slug}-{date}.md"));

    let written = if dry_run {
        false
    } else {
        atomic_write(&path, &content)?;
        true
    };
    Ok(RenderedLedger {
        path,
        content,
        written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn core() -> ExperimentCoreRow {
        ExperimentCoreRow {
            id: 1,
            slug: "arena-alloc-hotpath".to_string(),
            title: "Arena allocation on the hot path".to_string(),
            question: "Does arena allocation reduce p99 latency?".to_string(),
            context: Some("The dispatcher allocs per-call.".to_string()),
            kind: "optimization".to_string(),
            status: "decided".to_string(),
            project: Some("pgmcp".to_string()),
            git_ref: Some("abc123".to_string()),
            plan_ref: None,
            correction: "benjamini_hochberg".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn renders_frontmatter_and_sections() {
        let md = render_markdown(&core(), &[], &[], &[]);
        assert!(md.starts_with("---\n"));
        assert!(md.contains("pgmcp_experiment: arena-alloc-hotpath"));
        assert!(md.contains("## Method"));
        assert!(md.contains("## Hypotheses"));
        assert!(md.contains("## Measurements & Decisions"));
        assert!(md.contains("## What did NOT work"));
        assert!(md.contains("## Timeline"));
    }

    #[test]
    fn ledger_path_guards_reject_escape_inputs() {
        assert!(safe_ledger_dir("docs/scientific-ledger").is_ok());
        assert!(safe_ledger_dir("../outside").is_err());
        assert!(safe_ledger_dir("/tmp/ledger").is_err());
        assert!(safe_ledger_dir(" ").is_err());
        assert!(safe_ledger_slug("safe-slug_1").is_ok());
        assert!(safe_ledger_slug("../escape").is_err());
        assert!(safe_ledger_slug("bad/slug").is_err());
        assert!(safe_ledger_slug(" ").is_err());
    }
}
