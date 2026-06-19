//! Worktree-family + singleton project grouping (ADR-027 Stage 1).
//!
//! Generalizes `crate::db::queries::projects::pick_main_worktree_ids` (which
//! returns only the main ids) into full group derivation, then upserts into
//! `project_groups` / `project_group_members`. Pure derivation (`derive_groups`)
//! is unit-tested; `rederive_groups` is the DB-writing wrapper.

use std::collections::HashMap;

use sqlx::PgPool;

use crate::hierarchy::{GroupKind, GroupRole};

/// A derived group: its key, label, and members (exactly one `Main`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedGroup {
    pub kind: GroupKind,
    pub group_key: String,
    pub label: String,
    pub members: Vec<(i32, GroupRole)>,
}

/// Pure: derive worktree-family + singleton groups from project git metadata.
/// `rows = (id, path, git_common_dir, git_root_commits)`. Projects sharing a git
/// common-dir + root-commits form a family (shortest basename = `Main`, the
/// existing worktree-main convention); a project with no git metadata becomes a
/// singleton group with itself as `Main`. Deterministic (sorted output).
pub fn derive_groups(rows: &[(i32, String, Option<String>, Option<String>)]) -> Vec<DerivedGroup> {
    let mut families: HashMap<(String, String), Vec<(i32, String)>> = HashMap::new();
    let mut singletons: Vec<(i32, String)> = Vec::new();
    for (id, path, cd, rc) in rows {
        let basename = path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(path)
            .to_string();
        match (cd.as_deref(), rc.as_deref()) {
            (None, None) => singletons.push((*id, basename)),
            (cd, rc) => families
                .entry((cd.unwrap_or("").to_string(), rc.unwrap_or("").to_string()))
                .or_default()
                .push((*id, basename)),
        }
    }
    let mut out = Vec::with_capacity(families.len() + singletons.len());
    for ((cd, rc), mut members) in families {
        // Shortest basename wins (worktree-main convention); stable tie-breaks.
        members.sort_by(|a, b| {
            a.1.len()
                .cmp(&b.1.len())
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.0.cmp(&b.0))
        });
        let label = members.first().map(|(_, b)| b.clone()).unwrap_or_default();
        let group_members = members
            .iter()
            .enumerate()
            .map(|(i, (id, _))| {
                (
                    *id,
                    if i == 0 {
                        GroupRole::Main
                    } else {
                        GroupRole::Member
                    },
                )
            })
            .collect();
        out.push(DerivedGroup {
            kind: GroupKind::WorktreeFamily,
            group_key: format!("{cd}|{rc}"),
            label,
            members: group_members,
        });
    }
    for (id, basename) in singletons {
        out.push(DerivedGroup {
            kind: GroupKind::WorktreeFamily,
            group_key: format!("singleton:{id}"),
            label: basename,
            members: vec![(id, GroupRole::Main)],
        });
    }
    out.sort_by(|a, b| a.group_key.cmp(&b.group_key));
    out
}

/// (Re)derive groups from `projects` git metadata and upsert into the v46 tables.
/// Idempotent: re-running converges (open memberships are replaced in-place).
/// Returns the number of groups written.
pub async fn rederive_groups(pool: &PgPool) -> Result<usize, sqlx::Error> {
    let rows: Vec<(i32, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, path, git_common_dir, git_root_commits FROM projects ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    let groups = derive_groups(&rows);
    let mut written = 0usize;
    for g in &groups {
        let group_id: i64 = sqlx::query_scalar(
            "INSERT INTO project_groups (kind, group_key, label) VALUES ($1, $2, $3)
             ON CONFLICT (kind, group_key) DO UPDATE SET label = EXCLUDED.label
             RETURNING id",
        )
        .bind(g.kind.as_str())
        .bind(&g.group_key)
        .bind(&g.label)
        .fetch_one(pool)
        .await?;
        for (pid, role) in &g.members {
            sqlx::query(
                "INSERT INTO project_group_members (group_id, project_id, role)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (group_id, project_id) WHERE valid_to IS NULL
                 DO UPDATE SET role = EXCLUDED.role",
            )
            .bind(group_id)
            .bind(pid)
            .bind(role.as_str())
            .execute(pool)
            .await?;
        }
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_picks_shortest_basename_as_main() {
        let rows = vec![
            (
                1,
                "/ws/f1r3node-feature".to_string(),
                Some("/g/.git".to_string()),
                Some("abc".to_string()),
            ),
            (
                2,
                "/ws/f1r3node".to_string(),
                Some("/g/.git".to_string()),
                Some("abc".to_string()),
            ),
        ];
        let groups = derive_groups(&rows);
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g.kind, GroupKind::WorktreeFamily);
        // id 2 ("f1r3node") is the shortest basename → Main.
        assert!(g.members.contains(&(2, GroupRole::Main)));
        assert!(g.members.contains(&(1, GroupRole::Member)));
    }

    #[test]
    fn no_git_metadata_becomes_singleton_main() {
        let rows = vec![(7, "/ws/solo".to_string(), None, None)];
        let groups = derive_groups(&rows);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members, vec![(7, GroupRole::Main)]);
        assert_eq!(groups[0].group_key, "singleton:7");
    }

    #[test]
    fn distinct_repos_are_distinct_groups() {
        let rows = vec![
            (
                1,
                "/ws/a".to_string(),
                Some("/a/.git".to_string()),
                Some("r1".to_string()),
            ),
            (
                2,
                "/ws/b".to_string(),
                Some("/b/.git".to_string()),
                Some("r2".to_string()),
            ),
        ];
        assert_eq!(derive_groups(&rows).len(), 2);
    }
}
