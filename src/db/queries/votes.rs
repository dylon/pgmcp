//! Queries for `votes` (ADR-023, schema v43).
//!
//! One generic ledger over any votable entity. [`cast_vote`] is an idempotent
//! upsert keyed on the `UNIQUE (target_type, target_id, agent_id)` constraint —
//! a re-vote updates direction/weight rather than inserting a second row, so the
//! "at most one vote per (target, agent)" invariant holds by construction.

use sqlx::PgPool;

/// Cast (or update) a vote. Returns the row id.
pub async fn cast_vote(
    pool: &PgPool,
    target_type: &str,
    target_id: i64,
    agent_id: &str,
    direction: &str,
    weight: f32,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO votes (target_type, target_id, agent_id, direction, weight)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (target_type, target_id, agent_id)
         DO UPDATE SET direction = EXCLUDED.direction, weight = EXCLUDED.weight, updated_at = now()
         RETURNING id",
    )
    .bind(target_type)
    .bind(target_id)
    .bind(agent_id)
    .bind(direction)
    .bind(weight)
    .fetch_one(pool)
    .await
}

/// Retract an agent's vote on a target. Returns true if a vote was removed.
pub async fn retract_vote(
    pool: &PgPool,
    target_type: &str,
    target_id: i64,
    agent_id: &str,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "DELETE FROM votes WHERE target_type = $1 AND target_id = $2 AND agent_id = $3",
    )
    .bind(target_type)
    .bind(target_id)
    .bind(agent_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Aggregate tally for a target.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct VoteTally {
    pub up_votes: i64,
    pub down_votes: i64,
    /// Signed weighted score: `Σ(up.weight) − Σ(down.weight)`.
    pub net_weight: f64,
    pub voters: i64,
}

/// Tally votes for one target (counts + signed weighted score + distinct voters).
pub async fn tally_votes(
    pool: &PgPool,
    target_type: &str,
    target_id: i64,
) -> Result<VoteTally, sqlx::Error> {
    sqlx::query_as::<_, VoteTally>(
        "SELECT
            COUNT(*) FILTER (WHERE direction = 'up')::bigint   AS up_votes,
            COUNT(*) FILTER (WHERE direction = 'down')::bigint AS down_votes,
            COALESCE(SUM(CASE direction WHEN 'up' THEN weight ELSE -weight END), 0)::double precision
                AS net_weight,
            COUNT(DISTINCT agent_id)::bigint AS voters
         FROM votes
         WHERE target_type = $1 AND target_id = $2",
    )
    .bind(target_type)
    .bind(target_id)
    .fetch_one(pool)
    .await
}
