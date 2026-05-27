//! Memory-graph mirror for experiments (the `agent_outcomes` dual-write
//! pattern, applied to the experiment subsystem). Mirroring lets experiments
//! participate in the existing PPR / unified retrieval / reflection machinery
//! and surface via `memory_find_entities_for_code`, and exposes a PROV-O-shaped
//! provenance view over `memory_relations`.
//!
//! Entities (open TEXT types — no schema change): `experiment:<slug>`
//! (`entity_type='experiment'`), `hypothesis:<slug>#<id>`
//! (`entity_type='hypothesis'`), `metric:<name>` (`entity_type='metric'`).
//! Relations: `(experiment)-[prov:used]->(hypothesis)`,
//! `(hypothesis)-[prov:used]->(metric)` on open; and
//! `(hypothesis)-[confirms|refutes|inconclusive_for]->(metric)` on decision.
//!
//! Best-effort: each `memory_create_*` commits its own transaction (mirrors
//! `best_practices::record_outcome`); a partial mirror is acceptable since the
//! authoritative record is the dedicated `experiments` tables.

use sqlx::PgPool;

use crate::a2a::best_practices::attach_tier;
use crate::db::queries::{self, NewEntityInput, NewRelationInput, ScopeSpec};

/// Stable entity name for an experiment.
pub fn experiment_entity(slug: &str) -> String {
    format!("experiment:{slug}")
}
/// Stable entity name for a hypothesis.
pub fn hypothesis_entity(slug: &str, hypothesis_id: i64) -> String {
    format!("hypothesis:{slug}#{hypothesis_id}")
}
/// Stable entity name for a metric.
pub fn metric_entity(metric: &str) -> String {
    format!("metric:{metric}")
}

/// Mirror experiment + first-hypothesis creation into the memory graph. The
/// experiment entity carries an episodic observation describing the question.
pub async fn mirror_open(
    pool: &PgPool,
    slug: &str,
    title: &str,
    question: &str,
    kind: &str,
    project_id: Option<i32>,
    hypothesis_id: i64,
    hypothesis_statement: &str,
    primary_metric: &str,
) -> Result<(), sqlx::Error> {
    let scope = ScopeSpec {
        user_id: None,
        agent_id: None,
        session_id: None,
        project_id,
    };
    let scope_id = queries::find_or_create_scope(pool, &scope).await?;

    let exp_name = experiment_entity(slug);
    let hyp_name = hypothesis_entity(slug, hypothesis_id);
    let met_name = metric_entity(primary_metric);
    let observation = format!(
        "[{kind}] Experiment '{title}': {question} — H: {hypothesis_statement} (metric: {primary_metric})"
    );

    let inputs = vec![
        NewEntityInput {
            name: exp_name.clone(),
            entity_type: "experiment".to_string(),
            observations: vec![observation],
        },
        NewEntityInput {
            name: hyp_name.clone(),
            entity_type: "hypothesis".to_string(),
            observations: vec![hypothesis_statement.to_string()],
        },
        NewEntityInput {
            name: met_name.clone(),
            entity_type: "metric".to_string(),
            observations: Vec::new(),
        },
    ];
    let ids = queries::memory_create_entities(pool, &inputs, scope_id, "agent_write").await?;
    if let Some(experiment_entity_id) = ids.first() {
        // Episodic: it happened.
        attach_tier(pool, *experiment_entity_id, "episodic").await?;
    }

    let relations = [
        NewRelationInput {
            from: exp_name,
            to: hyp_name.clone(),
            relation_type: "prov:used".to_string(),
        },
        NewRelationInput {
            from: hyp_name,
            to: met_name,
            relation_type: "prov:used".to_string(),
        },
    ];
    let _ = queries::memory_create_relations(pool, &relations, "agent_write").await?;
    Ok(())
}

/// The relation verb for a verdict.
fn verdict_verb(verdict: &str) -> &'static str {
    match verdict {
        "accepted" => "confirms",
        "rejected" => "refutes",
        _ => "inconclusive_for",
    }
}

/// Mirror a decision: append a procedural observation to the experiment entity
/// and add the `(hypothesis)-[confirms|refutes|inconclusive_for]->(metric)`
/// edge. Returns the new observation's id (for `experiments.observation_id`).
#[allow(clippy::too_many_arguments)]
pub async fn mirror_decision(
    pool: &PgPool,
    slug: &str,
    project_id: Option<i32>,
    hypothesis_id: i64,
    primary_metric: &str,
    verdict: &str,
    summary: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let scope = ScopeSpec {
        user_id: None,
        agent_id: None,
        session_id: None,
        project_id,
    };
    let scope_id = queries::find_or_create_scope(pool, &scope).await?;

    let exp_name = experiment_entity(slug);
    let hyp_name = hypothesis_entity(slug, hypothesis_id);
    let met_name = metric_entity(primary_metric);
    let observation = format!("[{verdict}] {summary}");

    // Append the decision observation to the (existing) experiment entity.
    let inputs = vec![NewEntityInput {
        name: exp_name.clone(),
        entity_type: "experiment".to_string(),
        observations: vec![observation.clone()],
    }];
    let ids = queries::memory_create_entities(pool, &inputs, scope_id, "agent_write").await?;
    let experiment_entity_id = ids.first().copied();
    if let Some(eid) = experiment_entity_id {
        // Procedural: durable "what worked / didn't" knowledge.
        attach_tier(pool, eid, "procedural").await?;
    }

    let relations = [NewRelationInput {
        from: hyp_name,
        to: met_name,
        relation_type: verdict_verb(verdict).to_string(),
    }];
    let _ = queries::memory_create_relations(pool, &relations, "agent_write").await?;

    // Recover the observation id just written (active row, exact content).
    let observation_id: Option<i64> = match experiment_entity_id {
        Some(eid) => {
            sqlx::query_scalar(
                "SELECT id FROM memory_observations
                 WHERE entity_id = $1 AND content = $2 AND valid_to IS NULL
                 ORDER BY id DESC LIMIT 1",
            )
            .bind(eid)
            .bind(&observation)
            .fetch_optional(pool)
            .await?
        }
        None => None,
    };
    Ok(observation_id)
}
