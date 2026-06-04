//! Migration step 28: `project_deps_gitstate_v1` (Phase 4).
//!
//! Adds:
//! - `project_dependencies` — a **bitemporal** project→project dependency edge
//!   (`valid_from`/`valid_to`, mirroring `lock_order_edges`), the source of the
//!   `project_depends_on` unified-graph edge. `kind`/`source` are closed
//!   vocabularies (ADR-003) built from `DepKind`/`DepSource`. The upsert-and-
//!   close cron keeps history (a removed dep gets `valid_to = now()`).
//! - `projects` git-state columns (`git_current_branch`, `git_head_sha`,
//!   `git_dirty`, `stable_branch`, `git_scanned_at`) so the coordination layer
//!   can tell whether a dependency is on its stable branch and clean.
//!
//! Additive + `IF NOT EXISTS`, so idempotent and version-gated.

use sqlx::PgPool;

use crate::deps::{dep_kind_sql_in_list, dep_source_sql_in_list};

pub(super) const PROJECT_DEPS_GITSTATE_V1: i32 = 28;
pub(super) const PROJECT_DEPS_GITSTATE_V1_NAME: &str = "project_deps_gitstate_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    let create = format!(
        "CREATE TABLE IF NOT EXISTS project_dependencies (
            id BIGSERIAL PRIMARY KEY,
            dependent_project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            dependency_project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            dep_name     TEXT,
            kind         TEXT CHECK (kind IS NULL OR kind IN ({kind})),
            source       TEXT NOT NULL CHECK (source IN ({source})),
            confidence   DOUBLE PRECISION NOT NULL DEFAULT 1.0,
            valid_from   TIMESTAMPTZ NOT NULL DEFAULT now(),
            valid_to     TIMESTAMPTZ,
            last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
        kind = dep_kind_sql_in_list(),
        source = dep_source_sql_in_list(),
    );
    sqlx::query(&create).execute(pool).await?;

    // One live edge per (dependent, dependency, source) — partial unique on the
    // open interval, so historical (closed) rows don't collide.
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS uq_project_deps_live
            ON project_dependencies (dependent_project_id, dependency_project_id, source)
            WHERE valid_to IS NULL",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_project_deps_dependency
            ON project_dependencies (dependency_project_id) WHERE valid_to IS NULL",
    )
    .execute(pool)
    .await?;

    // Live git-state columns on projects (the scanner fills them).
    for col in [
        "git_current_branch TEXT",
        "git_head_sha TEXT",
        "git_dirty BOOLEAN",
        "stable_branch TEXT",
        "git_scanned_at TIMESTAMPTZ",
    ] {
        sqlx::query(&format!(
            "ALTER TABLE projects ADD COLUMN IF NOT EXISTS {col}"
        ))
        .execute(pool)
        .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(PROJECT_DEPS_GITSTATE_V1, 28);
        assert_eq!(PROJECT_DEPS_GITSTATE_V1_NAME, "project_deps_gitstate_v1");
    }
}
