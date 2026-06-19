//! Migration step 46: project grouping model (`project_groups` +
//! `project_group_members`) — ADR-027, item 15.
//!
//! Link tables (not a column on `projects`) because memberships can overlap
//! (a project in both its worktree-family and a declared group), groups carry
//! their own grain of metrics (`hier_group_metrics`, v48), and the mapping is
//! re-derivable. `project_group_members` is bitemporal on the open interval
//! (`valid_to IS NULL`) per the v28 idiom, so re-grouping is non-destructive.
//! Additive + idempotent.

use sqlx::PgPool;

use crate::hierarchy::{GroupKind, GroupRole};

pub(super) const PROJECT_GROUPS: i32 = 46;
pub(super) const PROJECT_GROUPS_NAME: &str = "project_groups";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS project_groups (
            id          BIGSERIAL PRIMARY KEY,
            kind        TEXT NOT NULL,
            group_key   TEXT NOT NULL,
            label       TEXT,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE (kind, group_key)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS project_group_members (
            id          BIGSERIAL PRIMARY KEY,
            group_id    BIGINT NOT NULL REFERENCES project_groups(id) ON DELETE CASCADE,
            project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            role        TEXT NOT NULL,
            valid_from  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            valid_to    TIMESTAMPTZ
        )",
    )
    .execute(pool)
    .await?;

    // One open membership per (group, project) — the v28 bitemporal idiom.
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS ux_pgm_open
         ON project_group_members (group_id, project_id) WHERE valid_to IS NULL",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS ix_pgm_project ON project_group_members (project_id)
         WHERE valid_to IS NULL",
    )
    .execute(pool)
    .await?;

    super::v4_work_items::install_check(
        pool,
        "project_groups",
        "project_groups_kind_check",
        &format!("kind IN ({})", GroupKind::sql_in_list()),
    )
    .await?;
    super::v4_work_items::install_check(
        pool,
        "project_group_members",
        "project_group_members_role_check",
        &format!("role IN ({})", GroupRole::sql_in_list()),
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(PROJECT_GROUPS, 46);
        assert_eq!(PROJECT_GROUPS_NAME, "project_groups");
    }
}
