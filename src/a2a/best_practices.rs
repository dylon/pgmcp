//! Cross-agent best-practice exchange (Part A).
//!
//! This module is the single seam through which running agents (Claude
//! Code, Codex CLI, peer pgmcp daemons) record "what worked / what
//! failed" and recover it later. Every capture lands in two places at
//! once:
//!
//! 1. `agent_outcomes` — the authoritative, cheaply-aggregatable ledger
//!    (one row per peer report; supports `GROUP BY task_kind, outcome`).
//! 2. a mirrored `memory_observation` under `{project_id, agent_id}` with
//!    `source='agent_write'`, tier `procedural`, plus an
//!    `(approach)-[worked_for|…]->(task_kind)` relation — so the practice
//!    participates in the existing PPR / unified retrieval and reflection
//!    machinery and can later be promoted to a durable mandate.
//!
//! The capture is intentionally LLM-free: the heuristic terminal-state
//! and explicit-report signals work with `[memory] backend = "disabled"`.
//! Cross-agent reflection + promotion (phase A4) layers on top.

use std::sync::atomic::Ordering;

use sqlx::PgPool;
use uuid::Uuid;

use crate::config::A2aReflectionConfig;
use crate::context::SystemContext;
use crate::db::queries::{self, NewEntityInput, NewRelationInput, ScopeSpec};
use crate::llm::LlmExtractor;
use crate::llm::reflect::{ReflectionRequest, ReflectionTrigger, run_reflection};
use crate::stats::tracker::StatsTracker;

/// Outcome polarity of a reported approach. Serialized form matches the
/// `memory_outcome` Postgres enum verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Worked,
    Failed,
    Mixed,
    Prefer,
    Avoid,
    SupersededByPeer,
}

impl Outcome {
    /// The `memory_outcome` enum label (also the `agent_outcomes.outcome`
    /// value).
    pub fn as_db_str(self) -> &'static str {
        match self {
            Outcome::Worked => "worked",
            Outcome::Failed => "failed",
            Outcome::Mixed => "mixed",
            Outcome::Prefer => "prefer",
            Outcome::Avoid => "avoid",
            Outcome::SupersededByPeer => "superseded_by_peer",
        }
    }

    /// Relation verb for the `(approach)-[verb]->(task_kind)` edge.
    pub fn relation_verb(self) -> &'static str {
        match self {
            Outcome::Worked => "worked_for",
            Outcome::Failed => "failed_for",
            Outcome::Mixed => "mixed_for",
            Outcome::Prefer => "preferred_for",
            Outcome::Avoid => "avoided_for",
            Outcome::SupersededByPeer => "superseded_for",
        }
    }

    /// Parse a loose user/agent-supplied string. Returns `None` for an
    /// unrecognized label so callers can reject it explicitly.
    pub fn parse(s: &str) -> Option<Outcome> {
        match s.trim().to_lowercase().as_str() {
            "worked" | "works" | "success" => Some(Outcome::Worked),
            "failed" | "fails" | "failure" | "broken" => Some(Outcome::Failed),
            "mixed" | "partial" => Some(Outcome::Mixed),
            "prefer" | "preferred" => Some(Outcome::Prefer),
            "avoid" | "avoided" => Some(Outcome::Avoid),
            "superseded_by_peer" | "superseded" => Some(Outcome::SupersededByPeer),
            _ => None,
        }
    }

    /// Map to a durable-mandate polarity for A4 promotion. `Mixed`
    /// carries no actionable polarity and is dropped (`None`). The other
    /// arms map to the softer advisory polarities (`prefer`/`avoid`)
    /// rather than the imperative `always`/`never`, since a peer-agreed
    /// practice is guidance, not a hard rule.
    pub fn to_polarity(self) -> Option<&'static str> {
        match self {
            Outcome::Worked | Outcome::Prefer => Some("prefer"),
            Outcome::Failed | Outcome::Avoid => Some("avoid"),
            Outcome::SupersededByPeer => Some("correction"),
            Outcome::Mixed => None,
        }
    }
}

/// One agent's report about an approach for a kind of task.
#[derive(Debug, Clone)]
pub struct OutcomeReport {
    /// Lowercased peer / client identity (`CallerInfo.client_name`, the
    /// A2A peer name, or an adapter `--name`).
    pub agent_id: String,
    /// Owning project, or `None` for a workspace-general practice.
    pub project_id: Option<i32>,
    /// Normalized task kind, e.g. `"rust-collections"` or
    /// `"a2a_pattern_sequential:Solver"`.
    pub task_kind: String,
    /// Short imperative, e.g. `"preallocate Vec with capacity"`.
    pub approach: String,
    pub outcome: Outcome,
    /// Caller confidence in `[0, 1]`; clamped on write.
    pub confidence: f32,
    /// Optional supporting snippet (the artifact text that justified it).
    pub evidence: Option<String>,
    /// Originating A2A task (provenance + the join key the Part-B
    /// trajectory labeler uses).
    pub parent_task_id: Option<Uuid>,
    /// Memory tier for the mirrored observation. `"procedural"` for durable,
    /// explicitly-reported best-practice signal; `"episodic"` for weaker
    /// auto-captured signal (e.g. a completed-task write-back, which is only
    /// evidence the task *ran*, not that the approach was *good*).
    pub tier: &'static str,
}

/// Ids produced by [`record_outcome`].
#[derive(Debug, Clone, Copy)]
pub struct RecordedOutcome {
    pub outcome_id: i64,
    pub observation_id: Option<i64>,
    pub approach_entity_id: i64,
}

/// Capture one outcome report: ledger row + mirrored observation + tier +
/// relation + trust bump. Best-effort but returns the row ids on success.
///
/// Not wrapped in a single outer transaction: `memory_create_entities`
/// and `memory_create_relations` each commit their own transaction, and a
/// partial capture of an advisory ledger is acceptable.
pub async fn record_outcome(
    pool: &PgPool,
    report: &OutcomeReport,
) -> Result<RecordedOutcome, sqlx::Error> {
    let scope = ScopeSpec {
        user_id: None,
        agent_id: Some(report.agent_id.clone()),
        session_id: None,
        project_id: report.project_id,
    };
    let scope_id = queries::find_or_create_scope(pool, &scope).await?;

    let approach_key = format!("approach:{}", normalize_approach(&report.approach));
    let task_kind_key = format!("task_kind:{}", normalize_approach(&report.task_kind));
    let observation = render_observation(report);

    // Create the approach entity (carrying the outcome observation) and
    // the task_kind entity (no observation) so the relation endpoints —
    // which `memory_create_relations` resolves by name — both exist.
    let inputs = vec![
        NewEntityInput {
            name: approach_key.clone(),
            entity_type: "best_practice".to_string(),
            observations: vec![observation.clone()],
        },
        NewEntityInput {
            name: task_kind_key.clone(),
            entity_type: "task_kind".to_string(),
            observations: Vec::new(),
        },
    ];
    let entity_ids =
        queries::memory_create_entities(pool, &inputs, scope_id, "agent_write").await?;
    let approach_entity_id = *entity_ids
        .first()
        .expect("memory_create_entities returns one id per input");

    // Tier reflects signal strength: procedural for durable explicit
    // reports, episodic for weak auto-captured ones (the report decides).
    attach_tier(pool, approach_entity_id, report.tier).await?;

    // Recover the observation id we just wrote (active row, matched by
    // exact content — each distinct report renders distinct content).
    let observation_id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM memory_observations
         WHERE entity_id = $1 AND content = $2 AND valid_to IS NULL
         ORDER BY id DESC LIMIT 1",
    )
    .bind(approach_entity_id)
    .bind(&observation)
    .fetch_optional(pool)
    .await?;

    // Authoritative ledger row.
    let outcome_id: i64 = sqlx::query_scalar(
        "INSERT INTO agent_outcomes
            (agent_id, project_id, task_kind, approach, outcome, confidence,
             evidence, parent_task_id, observation_id)
         VALUES ($1, $2, $3, $4, $5::memory_outcome, $6, $7, $8, $9)
         RETURNING id",
    )
    .bind(&report.agent_id)
    .bind(report.project_id)
    .bind(&report.task_kind)
    .bind(&report.approach)
    .bind(report.outcome.as_db_str())
    .bind(report.confidence.clamp(0.0, 1.0))
    .bind(report.evidence.as_deref())
    .bind(report.parent_task_id)
    .bind(observation_id)
    .fetch_one(pool)
    .await?;

    // (approach) -[verb]-> (task_kind) in the knowledge graph.
    let relations = [NewRelationInput {
        from: approach_key,
        to: task_kind_key,
        relation_type: report.outcome.relation_verb().to_string(),
    }];
    let _ = queries::memory_create_relations(pool, &relations, "agent_write").await?;

    // Per-agent trust counter (anti-flooding prior; A4 promotion reads it).
    sqlx::query(
        "INSERT INTO agent_trust (agent_id, reports_total, updated_at)
         VALUES ($1, 1, NOW())
         ON CONFLICT (agent_id) DO UPDATE
            SET reports_total = agent_trust.reports_total + 1,
                updated_at = NOW()",
    )
    .bind(&report.agent_id)
    .execute(pool)
    .await?;

    Ok(RecordedOutcome {
        outcome_id,
        observation_id,
        approach_entity_id,
    })
}

/// Attach an entity to a memory tier. No existing helper writes
/// `memory_entity_tier`, so this is the canonical tier writer for Part A.
pub async fn attach_tier(pool: &PgPool, entity_id: i64, tier: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO memory_entity_tier (entity_id, tier, weight)
         VALUES ($1, $2::memory_tier, 1.0)
         ON CONFLICT (entity_id, tier) DO NOTHING",
    )
    .bind(entity_id)
    .bind(tier)
    .execute(pool)
    .await?;
    Ok(())
}

/// Render the canonical, mandate-shaped observation sentence so the FTS
/// index and embedding retrieval treat a captured outcome exactly like a
/// session mandate.
fn render_observation(report: &OutcomeReport) -> String {
    let mut s = format!(
        "[{}] For {}: {}. (agent={}, confidence={:.2})",
        report.outcome.as_db_str(),
        report.task_kind.trim(),
        report.approach.trim(),
        report.agent_id,
        report.confidence.clamp(0.0, 1.0),
    );
    if let Some(ev) = &report.evidence {
        let ev = ev.trim();
        if !ev.is_empty() {
            s.push_str("\nEvidence: ");
            s.push_str(ev);
        }
    }
    s
}

/// Normalize an approach / task-kind string into a stable entity-name key:
/// trim, collapse internal whitespace, lowercase. This is what makes two
/// agents reporting "Preallocate  Vec capacity" and "preallocate vec
/// capacity" collapse onto the same `best_practice` entity.
fn normalize_approach(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Distill a completed peer artifact into the shared best-practice graph
/// (Part A write-back seam). Best-effort and config-gated: a no-op unless
/// `[a2a] writeback_enabled = true`, and any failure is logged and
/// swallowed so it never breaks an orchestration. `task_kind` should
/// encode the pattern + role, e.g. `"a2a_pattern_sequential:Solver"`.
///
/// The heuristic signal is deliberately weak (`Worked`, confidence 0.4):
/// a peer task that returned an artifact "ran without error", which is not
/// the same as "was correct". The high-confidence signal comes from the
/// explicit `a2a_report_outcome` tool. The ≥2-agent consensus gate (A4)
/// keeps these low-confidence records from being promoted on their own.
pub async fn writeback_peer_artifact(
    ctx: &SystemContext,
    parent_task_id: Uuid,
    agent_name: &str,
    task_kind: &str,
    text: &str,
) {
    if !ctx.config().load().a2a.writeback_enabled {
        return;
    }
    let trimmed = text.trim();
    if trimmed.chars().count() < 16 {
        return; // too short to be a useful record
    }
    let Some(pool) = ctx.db().pool() else {
        return;
    };
    // A completed task is only evidence the approach RAN, not that it was
    // good — so this auto-capture is deliberately weak: Outcome::Mixed (below
    // the labeler's worked/prefer gate) at the episodic tier. Explicit
    // `a2a_report_outcome` calls carry the strong procedural signal instead.
    let report = OutcomeReport {
        agent_id: agent_name.to_string(),
        project_id: None, // pattern collaborations are workspace-general
        task_kind: task_kind.to_string(),
        approach: first_line(trimmed, 140),
        outcome: Outcome::Mixed,
        confidence: 0.4,
        evidence: Some(truncate_chars(trimmed, 2000)),
        parent_task_id: Some(parent_task_id),
        tier: "episodic",
    };
    match record_outcome(pool, &report).await {
        Ok(_) => {
            ctx.stats()
                .a2a_outcomes_recorded
                .fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                agent = agent_name,
                task_kind,
                "a2a best-practice write-back failed (non-fatal)"
            );
        }
    }
}

/// Retrieve peer best practices relevant to a prompt as a Markdown block
/// (read-before-act, Part A). Scope union (G1): `{workspace-global ∪
/// current-project}` — general practices reach every agent on every task,
/// project-specific ones stay contained. No-op (empty string) unless
/// `[a2a] inject_best_practices = true`. LLM-free: ranks by FTS match
/// against the prompt then importance, so it works with
/// `[memory] backend = "disabled"`.
pub async fn retrieve_for_prompt(
    ctx: &SystemContext,
    project_id: Option<i32>,
    query: &str,
    budget_bytes: usize,
) -> String {
    if !ctx.config().load().a2a.inject_best_practices {
        return String::new();
    }
    let Some(pool) = ctx.db().pool() else {
        return String::new();
    };
    let rows: Vec<(String, f32)> = sqlx::query_as(
        "SELECT o.content, o.importance
         FROM memory_observations o
         JOIN memory_entities e      ON e.id = o.entity_id AND e.valid_to IS NULL
         JOIN memory_entity_tier t   ON t.entity_id = e.id AND t.tier IN ('procedural','reflective')
         JOIN memory_entity_scope es ON es.entity_id = e.id
         JOIN memory_scope s         ON s.id = es.scope_id
         WHERE o.valid_to IS NULL
           AND (s.project_id IS NULL OR s.project_id = $1)
         ORDER BY
           (CASE WHEN $2 <> '' AND to_tsvector('english', o.content)
                       @@ plainto_tsquery('english', $2) THEN 1 ELSE 0 END) DESC,
           o.importance DESC,
           o.id DESC
         LIMIT 40",
    )
    .bind(project_id)
    .bind(query)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    if rows.is_empty() {
        return String::new();
    }

    let mut out = String::from("## Best practices from peer agents\n\n");
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut emitted = 0usize;
    for (content, _importance) in rows {
        let one = content.replace('\n', " ");
        if !seen.insert(one.clone()) {
            continue;
        }
        let line = format!("- {one}\n");
        if out.len() + line.len() > budget_bytes {
            break;
        }
        out.push_str(&line);
        emitted += 1;
        if emitted >= 20 {
            break;
        }
    }
    if emitted == 0 { String::new() } else { out }
}

/// First non-empty line of `s`, trimmed and capped to `max_chars` chars.
fn first_line(s: &str, max_chars: usize) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    truncate_chars(line, max_chars)
}

/// Truncate to at most `max_chars` Unicode scalar values (char-boundary safe).
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect()
    }
}

/// Summary of one cross-agent reflection pass (A4).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct A2aReflectionReport {
    /// Consensus groups attached to a shared (agent_id=NULL) scope.
    pub consensus_groups: u64,
    /// Shared scopes LLM-reflected (0 when no extractor is configured).
    pub scopes_reflected: u64,
    /// Practices promoted to `durable_mandates` this pass.
    pub mandates_promoted: u64,
}

/// Cross-agent reflection + promotion (Part A phase A4). Three steps:
/// (1) consensus-gate peer outcomes into their shared `{project,
/// agent_id:NULL}` scope (≥`min_agents` distinct agents agreeing, mean
/// confidence ≥`min_confidence`), marking them `reflective` and bumping
/// importance; (2) optionally LLM-reflect each touched scope into
/// higher-order facts (the step-1 attach IS the heuristic consolidation
/// when no extractor is present); (3) promote the strongest agreed
/// practices to `durable_mandates` (trust-weighted, workspace/project
/// scope-routed). Best-effort per step; returns a small report.
pub async fn run_cross_agent_reflection(
    pool: &PgPool,
    stats: &StatsTracker,
    extractor: Option<&dyn LlmExtractor>,
    cfg: &A2aReflectionConfig,
) -> Result<A2aReflectionReport, sqlx::Error> {
    let mut report = A2aReflectionReport::default();

    // Keep RLM trajectory success labels fresh from explicit outcomes
    // (Part-A↔B seam) so the B4 learning loop sees up-to-date cohorts.
    let _ = crate::fuzzy::trajectory_index::label_trajectories_from_outcomes(pool).await;

    let shared =
        consensus_promote_to_shared_scope(pool, cfg.min_agents, cfg.min_confidence).await?;
    report.consensus_groups = shared.groups;

    // LLM synthesis per touched shared scope, when an extractor exists.
    if let Some(extractor) = extractor {
        for scope_id in shared.scope_ids {
            let req = ReflectionRequest {
                scope_id: Some(scope_id),
                session_id: None,
                since: None,
                max_observations: 200,
                trigger: ReflectionTrigger::Cron,
            };
            if run_reflection(pool, stats, extractor, req).await.is_ok() {
                report.scopes_reflected += 1;
            }
        }
    }

    report.mandates_promoted = promote_agreed_to_durable(
        pool,
        cfg.min_agents,
        cfg.min_confidence,
        cfg.promote_threshold,
        cfg.workspace_promotion,
        cfg.write_to_file,
    )
    .await?;

    Ok(report)
}

struct SharedScopes {
    groups: u64,
    scope_ids: Vec<i64>,
}

/// Step 1: attach every consensus group's `best_practice` entity to its
/// shared `{project, agent_id:NULL}` scope, mark it `reflective`, and bump
/// importance so it ranks high in `retrieve_for_prompt` and is well above
/// any retention threshold.
async fn consensus_promote_to_shared_scope(
    pool: &PgPool,
    min_agents: i64,
    min_confidence: f32,
) -> Result<SharedScopes, sqlx::Error> {
    // One row per agreed (approach, project) — grouping also by task_kind +
    // outcome so disagreeing outcomes don't falsely coalesce.
    let groups: Vec<(String, Option<i32>)> = sqlx::query_as(
        "SELECT approach, project_id
         FROM agent_outcomes
         GROUP BY approach, task_kind, outcome, project_id
         HAVING COUNT(DISTINCT agent_id) >= $1 AND AVG(confidence) >= $2",
    )
    .bind(min_agents)
    .bind(min_confidence)
    .fetch_all(pool)
    .await?;

    let mut scope_ids: Vec<i64> = Vec::with_capacity(groups.len());
    let mut count = 0u64;
    for (approach, project_id) in &groups {
        let entity_name = format!("approach:{}", normalize_approach(approach));
        let entity_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_entities
             WHERE name = $1 AND entity_type = 'best_practice' AND valid_to IS NULL
             LIMIT 1",
        )
        .bind(&entity_name)
        .fetch_optional(pool)
        .await?;
        let Some(entity_id) = entity_id else {
            continue;
        };

        let scope = ScopeSpec {
            user_id: None,
            agent_id: None,
            session_id: None,
            project_id: *project_id,
        };
        let scope_id = queries::find_or_create_scope(pool, &scope).await?;
        sqlx::query(
            "INSERT INTO memory_entity_scope (entity_id, scope_id)
             VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(entity_id)
        .bind(scope_id)
        .execute(pool)
        .await?;
        attach_tier(pool, entity_id, "reflective").await?;
        sqlx::query(
            "UPDATE memory_entities SET importance = GREATEST(importance, 0.8) WHERE id = $1",
        )
        .bind(entity_id)
        .execute(pool)
        .await?;
        sqlx::query(
            "UPDATE memory_observations SET importance = GREATEST(importance, 0.8)
             WHERE entity_id = $1 AND valid_to IS NULL",
        )
        .bind(entity_id)
        .execute(pool)
        .await?;
        if !scope_ids.contains(&scope_id) {
            scope_ids.push(scope_id);
        }
        count += 1;
    }
    Ok(SharedScopes {
        groups: count,
        scope_ids,
    })
}

/// Step 3: promote the strongest agreed practices to `durable_mandates`.
/// Trust-weighted score = mean(confidence) · mean(agent_trust prior);
/// scope routing (G1): `workspace` when agreed across ≥`workspace_promotion`
/// distinct projects, else `project`. Idempotent (skips equivalent
/// existing rows) and bumps the contributing agents' trust prior.
async fn promote_agreed_to_durable(
    pool: &PgPool,
    min_agents: i64,
    min_confidence: f32,
    promote_threshold: f32,
    workspace_promotion: i64,
    write_to_file: bool,
) -> Result<u64, sqlx::Error> {
    let rows: Vec<(String, String, String, Option<i32>, i64)> = sqlx::query_as(
        "SELECT ao.approach, ao.task_kind, ao.outcome::text, ao.project_id,
                (SELECT COUNT(DISTINCT a2.project_id) FROM agent_outcomes a2
                  WHERE a2.approach = ao.approach AND a2.task_kind = ao.task_kind) AS project_span
         FROM agent_outcomes ao
         LEFT JOIN agent_trust t ON t.agent_id = ao.agent_id
         GROUP BY ao.approach, ao.task_kind, ao.outcome, ao.project_id
         HAVING COUNT(DISTINCT ao.agent_id) >= $1
            AND AVG(ao.confidence) >= $2
            AND AVG(ao.confidence) * COALESCE(AVG(t.importance_prior), 0.5) >= $3",
    )
    .bind(min_agents)
    .bind(min_confidence)
    .bind(promote_threshold)
    .fetch_all(pool)
    .await?;

    let mut promoted = 0u64;
    for (approach, task_kind, outcome_str, project_id, project_span) in rows {
        let Some(outcome) = Outcome::parse(&outcome_str) else {
            continue;
        };
        let Some(polarity) = outcome.to_polarity() else {
            continue; // Mixed carries no actionable polarity.
        };
        let (scope, mandate_project): (&str, Option<i32>) = if project_span >= workspace_promotion {
            ("workspace", None)
        } else {
            ("project", project_id)
        };
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM durable_mandates
              WHERE scope = $1 AND project_id IS NOT DISTINCT FROM $2
                AND polarity = $3 AND imperative = $4)",
        )
        .bind(scope)
        .bind(mandate_project)
        .bind(polarity)
        .bind(&approach)
        .fetch_one(pool)
        .await?;
        if exists {
            continue;
        }
        sqlx::query(
            "INSERT INTO durable_mandates
                (scope, project_id, polarity, imperative, target, source_mandate_id)
             VALUES ($1, $2, $3, $4, $5, NULL)",
        )
        .bind(scope)
        .bind(mandate_project)
        .bind(polarity)
        .bind(&approach)
        .bind(&task_kind)
        .execute(pool)
        .await?;
        sqlx::query(
            "UPDATE agent_trust
                SET reports_promoted = reports_promoted + 1,
                    importance_prior = LEAST(1.0, importance_prior + 0.05),
                    updated_at = NOW()
              WHERE agent_id IN (
                  SELECT DISTINCT agent_id FROM agent_outcomes
                  WHERE approach = $1 AND task_kind = $2
              )",
        )
        .bind(&approach)
        .bind(&task_kind)
        .execute(pool)
        .await?;

        // Optional belt-and-suspenders file write (G2): append the agreed
        // practice to the project's AGENTS.md so it survives even DB loss
        // and is version-controllable. Project-scoped only (workspace-scope
        // has no single safe target). Best-effort; default off.
        if write_to_file
            && scope == "project"
            && let Some(pid) = mandate_project
            && let Ok(Some(root)) =
                sqlx::query_scalar::<_, String>("SELECT path FROM projects WHERE id = $1")
                    .bind(pid)
                    .fetch_optional(pool)
                    .await
        {
            let file = format!("{}/AGENTS.md", root.trim_end_matches('/'));
            let bullet = format!("- **{polarity}**: {approach} _(task: {task_kind})_");
            let _ = crate::mcp::tools::tool_session_mandates::append_bullet_to_marker(
                &file,
                "## Agreed best practices (pgmcp)",
                &bullet,
            );
        }

        promoted += 1;
    }
    Ok(promoted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_db_str_round_trips_through_parse() {
        for o in [
            Outcome::Worked,
            Outcome::Failed,
            Outcome::Mixed,
            Outcome::Prefer,
            Outcome::Avoid,
            Outcome::SupersededByPeer,
        ] {
            assert_eq!(Outcome::parse(o.as_db_str()), Some(o), "round-trip {o:?}");
        }
    }

    #[test]
    fn polarity_mapping_drops_mixed_only() {
        assert_eq!(Outcome::Worked.to_polarity(), Some("prefer"));
        assert_eq!(Outcome::Prefer.to_polarity(), Some("prefer"));
        assert_eq!(Outcome::Failed.to_polarity(), Some("avoid"));
        assert_eq!(Outcome::Avoid.to_polarity(), Some("avoid"));
        assert_eq!(Outcome::SupersededByPeer.to_polarity(), Some("correction"));
        assert_eq!(Outcome::Mixed.to_polarity(), None);
    }

    #[test]
    fn normalize_collapses_whitespace_and_case() {
        assert_eq!(
            normalize_approach("  Preallocate   Vec   Capacity "),
            "preallocate vec capacity"
        );
    }

    #[test]
    fn render_observation_is_mandate_shaped_and_includes_evidence() {
        let report = OutcomeReport {
            agent_id: "agent-a".into(),
            project_id: Some(1),
            task_kind: "rust-collections".into(),
            approach: "preallocate Vec with capacity".into(),
            outcome: Outcome::Worked,
            confidence: 0.8,
            evidence: Some("Vec::with_capacity(n) avoided reallocations".into()),
            parent_task_id: None,
            tier: "procedural",
        };
        let s = render_observation(&report);
        assert!(s.starts_with("[worked] For rust-collections: preallocate Vec with capacity."));
        assert!(s.contains("agent=agent-a"));
        assert!(s.contains("confidence=0.80"));
        assert!(s.contains("\nEvidence: Vec::with_capacity"));
    }

    #[test]
    fn render_observation_omits_empty_evidence() {
        let report = OutcomeReport {
            agent_id: "agent-b".into(),
            project_id: None,
            task_kind: "x".into(),
            approach: "y".into(),
            outcome: Outcome::Failed,
            confidence: 1.5, // out of range — must clamp to 1.00
            evidence: Some("   ".into()),
            parent_task_id: None,
            tier: "procedural",
        };
        let s = render_observation(&report);
        assert!(!s.contains("Evidence:"));
        assert!(s.contains("confidence=1.00"));
    }
}
