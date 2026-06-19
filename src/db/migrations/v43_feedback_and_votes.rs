//! Migration step 43: `agent_feedback` + `votes`.
//!
//! Two cross-cutting agent-voice tables (ADR-023):
//!   - `agent_feedback` — what connecting agents like / dislike / want
//!     feature-wise about pgmcp, with a closed `category` + 5-point `sentiment` +
//!     triage `status`. Embedded on write (`embedding`) for semantic recall, the
//!     same posture as `work_items`; a small HNSW index keeps `<=>` retrieval
//!     fast as the corpus grows. Promotable into a tracked work-item
//!     (`promoted_work_item_id`).
//!   - `votes` — generic one-vote-per-(target, agent) ledger over any votable
//!     entity (`target_type` ∈ work_item/feedback/bug/experiment). The
//!     `UNIQUE (target_type, target_id, agent_id)` constraint is the integrity
//!     mechanism behind "at most one vote per issue per agent".
//!
//! All closed vocabularies install their `CHECK` from the Rust enum's
//! `sql_in_list()` via the ADR-003 `install_check` idiom. Additive, idempotent,
//! version-gated.

use sqlx::PgPool;

use crate::feedback::{FeedbackCategory, FeedbackSentiment, FeedbackStatus};
use crate::voting::{VoteDirection, VoteTargetType};

pub(super) const FEEDBACK_AND_VOTES: i32 = 43;
pub(super) const FEEDBACK_AND_VOTES_NAME: &str = "feedback_and_votes";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // --- agent_feedback ----------------------------------------------------
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS agent_feedback (
            id                    BIGSERIAL PRIMARY KEY,
            agent_id              TEXT        NOT NULL,
            category              TEXT        NOT NULL,
            sentiment             TEXT        NOT NULL,
            subject               TEXT,
            body                  TEXT        NOT NULL,
            about_tool            TEXT,
            project_id            INTEGER     REFERENCES projects(id) ON DELETE SET NULL,
            status                TEXT        NOT NULL DEFAULT 'open',
            responded_by          TEXT,
            response              TEXT,
            promoted_work_item_id BIGINT      REFERENCES work_items(id) ON DELETE SET NULL,
            embedding             vector(1024),
            embedding_signature   TEXT        NOT NULL DEFAULT 'bge-m3-v1',
            created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at            TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .execute(pool)
    .await?;

    for idx in [
        "CREATE INDEX IF NOT EXISTS ix_agent_feedback_status ON agent_feedback (status)",
        "CREATE INDEX IF NOT EXISTS ix_agent_feedback_category ON agent_feedback (category)",
        "CREATE INDEX IF NOT EXISTS ix_agent_feedback_agent ON agent_feedback (agent_id)",
        "CREATE INDEX IF NOT EXISTS ix_agent_feedback_embedding ON agent_feedback \
            USING hnsw (embedding vector_cosine_ops) WITH (m = 16, ef_construction = 64)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }

    super::v4_work_items::install_check(
        pool,
        "agent_feedback",
        "agent_feedback_category_check",
        &format!("category IN ({})", FeedbackCategory::sql_in_list()),
    )
    .await?;
    super::v4_work_items::install_check(
        pool,
        "agent_feedback",
        "agent_feedback_sentiment_check",
        &format!("sentiment IN ({})", FeedbackSentiment::sql_in_list()),
    )
    .await?;
    super::v4_work_items::install_check(
        pool,
        "agent_feedback",
        "agent_feedback_status_check",
        &format!("status IN ({})", FeedbackStatus::sql_in_list()),
    )
    .await?;

    // --- votes -------------------------------------------------------------
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS votes (
            id          BIGSERIAL PRIMARY KEY,
            target_type TEXT        NOT NULL,
            target_id   BIGINT      NOT NULL,
            agent_id    TEXT        NOT NULL,
            direction   TEXT        NOT NULL,
            weight      REAL        NOT NULL DEFAULT 1.0,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            UNIQUE (target_type, target_id, agent_id),
            CHECK (weight > 0.0)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS ix_votes_target ON votes (target_type, target_id)")
        .execute(pool)
        .await?;

    super::v4_work_items::install_check(
        pool,
        "votes",
        "votes_target_type_check",
        &format!("target_type IN ({})", VoteTargetType::sql_in_list()),
    )
    .await?;
    super::v4_work_items::install_check(
        pool,
        "votes",
        "votes_direction_check",
        &format!("direction IN ({})", VoteDirection::sql_in_list()),
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(FEEDBACK_AND_VOTES, 43);
        assert_eq!(FEEDBACK_AND_VOTES_NAME, "feedback_and_votes");
    }
}
