//! A2A typed capability descriptors.
//!
//! Extends `a2a_agents.capabilities` JSONB with optional structured
//! shadow-ASR facets:
//!
//!   {
//!     "type_tags":         ["container", "async", ...],
//!     "effects":           ["network", "database", ...],
//!     "parameter_shapes":  [{constructor: "Request", args: [...]}, ...],
//!     "free_text":         "optional human-readable description"
//!   }
//!
//! Pattern H in the unified-semantic-representation plan. Specialty
//! matching becomes a JSON-path query on the typed descriptor; agents
//! that don't carry a typed descriptor fall back to free-text matching.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Typed capability descriptor stored inside `a2a_agents.capabilities`
/// JSONB. All fields are optional so legacy free-text descriptors
/// continue to validate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TypedCapability {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameter_shapes: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_text: Option<String>,
}

impl TypedCapability {
    pub fn is_empty(&self) -> bool {
        self.type_tags.is_empty()
            && self.effects.is_empty()
            && self.parameter_shapes.is_empty()
            && self.free_text.is_none()
    }
}

/// Parse a typed capability descriptor from a JSONB row's `capabilities`
/// column. Returns the default (empty) descriptor when the JSON doesn't
/// match the typed schema (i.e. the legacy free-text-only shape).
pub fn parse_capability(value: &serde_json::Value) -> TypedCapability {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

/// Find agents whose typed capability descriptor matches the given filter.
/// Filter semantics:
/// - `required_type_tags` → JSONB array contains ALL of these
/// - `required_effects`    → JSONB array contains ALL of these
/// - `min_score`           → minimum tag-overlap score (0.0-1.0)
#[derive(Debug, Clone, Default)]
pub struct AgentMatchFilter {
    pub required_type_tags: Vec<String>,
    pub required_effects: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AgentMatch {
    /// `a2a_agents.id` (the BIGSERIAL primary key).
    pub agent_id: i64,
    pub name: String,
    /// `a2a_agents.specialty` is a `TEXT[]`, so this is the full tag set.
    pub specialty: Vec<String>,
    pub capability: TypedCapability,
    pub score: f32,
}

pub async fn find_agents_by_typed_capability(
    pool: &PgPool,
    filter: &AgentMatchFilter,
    limit: i64,
) -> Result<Vec<AgentMatch>, sqlx::Error> {
    // Probe whether the table exists; gracefully degrade to empty when
    // a2a is not enabled in this deployment.
    let exists: Option<bool> = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM pg_tables
             WHERE schemaname = 'public' AND tablename = 'a2a_agents'
         )",
    )
    .fetch_one(pool)
    .await?;
    if !exists.unwrap_or(false) {
        return Ok(Vec::new());
    }

    // a2a_agents' PK is `id` (BIGINT) and `specialty` is `TEXT[]` — decode
    // them as such (the earlier `agent_id`/scalar-`specialty` shapes did not
    // match the schema and would fail at runtime).
    let rows: Vec<(i64, String, Vec<String>, Option<serde_json::Value>)> = sqlx::query_as(
        "SELECT id, name, specialty, capabilities
         FROM a2a_agents
         ORDER BY id
         LIMIT $1",
    )
    .bind(limit.max(1))
    .fetch_all(pool)
    .await?;

    let mut out: Vec<AgentMatch> = Vec::with_capacity(rows.len());
    for (agent_id, name, specialty, caps) in rows {
        let cap = caps.as_ref().map(parse_capability).unwrap_or_default();
        if let Some(score) = score_capability(filter, &cap) {
            out.push(AgentMatch {
                agent_id,
                name,
                specialty,
                capability: cap,
                score,
            });
        }
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

/// Pure match-and-score (DB-independent, hence unit-testable): `None` when
/// the capability lacks a required tag/effect (filtered out), else the
/// overlap score `(|tag_overlap| + |effect_overlap|) / (|required| + 1)` in
/// `[0, 1]`. The `+1` keeps an unconstrained query from dividing by zero and
/// from scoring 1.0 for every agent.
fn score_capability(filter: &AgentMatchFilter, cap: &TypedCapability) -> Option<f32> {
    let has_all_tags = filter
        .required_type_tags
        .iter()
        .all(|t| cap.type_tags.contains(t));
    let has_all_effects = filter
        .required_effects
        .iter()
        .all(|e| cap.effects.contains(e));
    if !has_all_tags || !has_all_effects {
        return None;
    }
    let required_total = (filter.required_type_tags.len() + filter.required_effects.len()) as f32;
    let overlap = (filter
        .required_type_tags
        .iter()
        .filter(|t| cap.type_tags.contains(*t))
        .count()
        + filter
            .required_effects
            .iter()
            .filter(|e| cap.effects.contains(*e))
            .count()) as f32;
    Some(overlap / (required_total + 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_empty_jsonb_is_default() {
        let v = json!({});
        let cap = parse_capability(&v);
        assert!(cap.is_empty());
    }

    #[test]
    fn parse_legacy_free_text_only() {
        let v = json!({ "free_text": "Rust code review" });
        let cap = parse_capability(&v);
        assert!(cap.type_tags.is_empty());
        assert_eq!(cap.free_text.as_deref(), Some("Rust code review"));
    }

    #[test]
    fn parse_full_typed() {
        let v = json!({
            "type_tags": ["async", "container"],
            "effects": ["network"],
            "parameter_shapes": [{"constructor": "Request"}],
            "free_text": "HTTP handler"
        });
        let cap = parse_capability(&v);
        assert_eq!(cap.type_tags, vec!["async", "container"]);
        assert_eq!(cap.effects, vec!["network"]);
        assert_eq!(cap.parameter_shapes.len(), 1);
        assert_eq!(cap.free_text.as_deref(), Some("HTTP handler"));
    }

    #[test]
    fn parse_unknown_jsonb_is_default() {
        // Mismatched schema (object where typed shape expects arrays).
        // serde silently falls back to default for the typed fields,
        // preserving anything that DOES parse.
        let v = json!("a plain string");
        let cap = parse_capability(&v);
        assert!(cap.is_empty());
    }

    fn cap(tags: &[&str], effects: &[&str]) -> TypedCapability {
        TypedCapability {
            type_tags: tags.iter().map(|s| s.to_string()).collect(),
            effects: effects.iter().map(|s| s.to_string()).collect(),
            parameter_shapes: Vec::new(),
            free_text: None,
        }
    }

    #[test]
    fn score_requires_all_tags_and_effects() {
        let filter = AgentMatchFilter {
            required_type_tags: vec!["async".into()],
            required_effects: vec!["network".into()],
        };
        // Missing the required effect → filtered out.
        assert!(score_capability(&filter, &cap(&["async"], &[])).is_none());
        // Has both → matches with a positive score.
        let s = score_capability(&filter, &cap(&["async", "container"], &["network"]))
            .expect("should match");
        assert!(s > 0.0 && s <= 1.0, "score in (0,1]: {s}");
    }

    #[test]
    fn unconstrained_filter_matches_everything_with_zero_score() {
        let filter = AgentMatchFilter::default();
        // No requirements → matches, score 0/(0+1) = 0.
        let s = score_capability(&filter, &cap(&["async"], &["network"])).expect("matches");
        assert_eq!(s, 0.0);
    }

    #[test]
    fn more_overlap_scores_higher() {
        let one = AgentMatchFilter {
            required_type_tags: vec!["async".into()],
            required_effects: vec![],
        };
        let two = AgentMatchFilter {
            required_type_tags: vec!["async".into(), "container".into()],
            required_effects: vec![],
        };
        let c = cap(&["async", "container"], &[]);
        let s1 = score_capability(&one, &c).expect("matches");
        let s2 = score_capability(&two, &c).expect("matches");
        assert!(
            s2 > s1,
            "two satisfied requirements outrank one: {s2} > {s1}"
        );
    }
}
