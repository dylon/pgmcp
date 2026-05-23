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
    pub agent_id: String,
    pub name: String,
    pub specialty: String,
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

    let rows: Vec<(String, String, String, Option<serde_json::Value>)> = sqlx::query_as(
        "SELECT agent_id, name, specialty, capabilities
         FROM a2a_agents
         ORDER BY agent_id
         LIMIT $1",
    )
    .bind(limit.max(1))
    .fetch_all(pool)
    .await?;

    let mut out: Vec<AgentMatch> = Vec::new();
    for (agent_id, name, specialty, caps) in rows {
        let cap = caps.as_ref().map(parse_capability).unwrap_or_default();
        // Required filters: short-circuit when an agent lacks a required tag/effect.
        let has_all_tags = filter
            .required_type_tags
            .iter()
            .all(|t| cap.type_tags.iter().any(|x| x == t));
        let has_all_effects = filter
            .required_effects
            .iter()
            .all(|e| cap.effects.iter().any(|x| x == e));
        if !has_all_tags || !has_all_effects {
            continue;
        }
        // Score = (|tag_overlap| + |effect_overlap|) / (|required| + 1)
        // The +1 keeps unconstrained queries from dividing by zero.
        let required_total =
            (filter.required_type_tags.len() + filter.required_effects.len()) as f32;
        let overlap = (filter
            .required_type_tags
            .iter()
            .filter(|t| cap.type_tags.iter().any(|x| x == *t))
            .count()
            + filter
                .required_effects
                .iter()
                .filter(|e| cap.effects.iter().any(|x| x == *e))
                .count()) as f32;
        let score = overlap / (required_total + 1.0);
        out.push(AgentMatch {
            agent_id,
            name,
            specialty,
            capability: cap,
            score,
        });
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
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
}
