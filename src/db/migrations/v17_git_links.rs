//! Migration step 17: `git_links_v1` — Phase 3 git/PR close-the-loop schema.
//!
//! Wires work items to real repo activity (`~/.claude/plans/how-extensive-is-the-zazzy-galaxy.md`,
//! "Git/PR close-the-loop"). Two additive tables, neither touching the
//! `work_items` spine:
//!
//! - `work_item_git_links` — the item ↔ commit/PR/branch join. The
//!   `UNIQUE(item_id, link_type, ref_value)` triple is the idempotency key:
//!   re-scanning a commit or re-running `work_item_link_commit` upserts the same
//!   row instead of duplicating. `commit_id` is an optional FK into
//!   `git_commits` (resolved when the commit has been indexed for the project);
//!   `detected_by` distinguishes a hand-made link (`manual`) from the indexer's
//!   message scan (`auto_scan`). `link_type` is CHECK-constrained from the
//!   closed [`crate::tracker::git_link::GitLinkType`] enum.
//!
//! - `work_item_finding_provenance` — the idempotency ledger for cron
//!   auto-promotion. `provenance_key` is UNIQUE (e.g.
//!   `bug_prediction:<project>:<path>` or `documented_tech_debt:<project>:<path>:<line>:<kind>`),
//!   so promoting the same finding twice is a no-op. `finding_source` is
//!   CHECK-constrained from [`crate::tracker::git_link::FindingSource`].
//!
//! TRUST NOTE: nothing here can reach `verified`. Auto-linkage advances at most
//! to a verify *candidate* (`Actor::Agent` → at most `verifying`); auto-promoted
//! findings land in `pending`. See `src/tracker/auto_transition.rs`.
//!
//! Version-gated (runs once); every statement is `IF NOT EXISTS` / idempotent.

use sqlx::PgPool;

pub(super) const GIT_LINKS_V1: i32 = 17;
pub(super) const GIT_LINKS_V1_NAME: &str = "git_links_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // 1. The item ↔ repo-artifact join. UNIQUE(item_id, link_type, ref_value)
    //    is the idempotency key (re-scan / re-link = upsert, never a dup).
    //    commit_id is an optional FK into git_commits (ON DELETE SET NULL: a
    //    re-indexed / pruned commit row leaves the link intact, just unresolved).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS work_item_git_links (
            id          BIGSERIAL PRIMARY KEY,
            item_id     BIGINT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
            project_id  INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            link_type   TEXT NOT NULL,
            ref_value   TEXT NOT NULL,
            commit_id   BIGINT REFERENCES git_commits(id) ON DELETE SET NULL,
            detected_by TEXT NOT NULL DEFAULT 'manual',
            created_by  TEXT,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            UNIQUE (item_id, link_type, ref_value)
        )",
    )
    .execute(pool)
    .await?;

    // link_type CHECK, built from the closed GitLinkType enum (ADR-003 closed-
    // enum idiom, the same `install_check` helper the v12 bug-tracker uses).
    super::v4_work_items::install_check(
        pool,
        "work_item_git_links",
        "work_item_git_links_link_type_check",
        &format!("link_type IN ({})", crate::tracker::git_link::sql_in_list()),
    )
    .await?;

    // detected_by CHECK: only the indexer's scan or a hand-made link. A closed
    // two-value provenance set (not enum-backed because it is internal-only and
    // never grows from a Rust vocabulary the way link_type does).
    super::v4_work_items::install_check(
        pool,
        "work_item_git_links",
        "work_item_git_links_detected_by_check",
        "detected_by IN ('manual','auto_scan')",
    )
    .await?;

    // Lookup indexes: by item (the item's link list) and by commit (the
    // reverse — "which items does this commit close?").
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_wi_git_links_item ON work_item_git_links(item_id)",
        "CREATE INDEX IF NOT EXISTS idx_wi_git_links_commit \
            ON work_item_git_links(commit_id) WHERE commit_id IS NOT NULL",
        // Branch / PR resolution for the REST pr_event handler (resolve an item
        // from a branch or PR ref it was previously linked to).
        "CREATE INDEX IF NOT EXISTS idx_wi_git_links_ref ON work_item_git_links(link_type, ref_value)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }

    // 2. The auto-promotion idempotency ledger. provenance_key is UNIQUE, so a
    //    second cron pass over the same finding upserts (no dup item). item_id
    //    points at the promoted work item; first/last_seen_at bracket the
    //    finding's observed lifetime across cron runs.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS work_item_finding_provenance (
            id             BIGSERIAL PRIMARY KEY,
            provenance_key TEXT NOT NULL UNIQUE,
            item_id        BIGINT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
            finding_source TEXT NOT NULL,
            first_seen_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            last_seen_at   TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .execute(pool)
    .await?;

    // finding_source CHECK, built from the closed FindingSource enum.
    super::v4_work_items::install_check(
        pool,
        "work_item_finding_provenance",
        "work_item_finding_provenance_source_check",
        &format!(
            "finding_source IN ({})",
            crate::tracker::git_link::finding_source_sql_in_list()
        ),
    )
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_wi_finding_prov_item \
            ON work_item_finding_provenance(item_id)",
    )
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        // Pinning the constant — changing it is a schema-breaking event.
        assert_eq!(GIT_LINKS_V1, 17);
        assert_eq!(GIT_LINKS_V1_NAME, "git_links_v1");
    }
}
