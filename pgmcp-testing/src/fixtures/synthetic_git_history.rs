//! Synthetic git-commit history with planted co-change patterns.
//!
//! Used by `oracle_find_coupled_files.rs` to assert the Jaccard
//! formula in `find_coupled_files` returns the expected pairs on a
//! known input. The pattern:
//!
//!   files A and B always change together → Jaccard 1.0
//!   files C and D share half their commits → Jaccard 0.5
//!   file E never co-changes with anyone → Jaccard 0
//!
//! Five commits total:
//!
//!   commit 1: A, B
//!   commit 2: A, B
//!   commit 3: A, B, C, D
//!   commit 4: C
//!   commit 5: E
//!
//! Hand-derived expected co-change pairs:
//!
//!   (A, B): co_commits = 3, commits_a = 3, commits_b = 3 → J = 3/3 = 1.0
//!   (A, C): co_commits = 1, commits_a = 3, commits_b = 2 → J = 1/4 = 0.25
//!   (A, D): co_commits = 1, commits_a = 3, commits_b = 1 → J = 1/3 ≈ 0.333
//!   (B, C): co_commits = 1, commits_a = 3, commits_b = 2 → J = 1/4 = 0.25
//!   (B, D): co_commits = 1, commits_a = 3, commits_b = 1 → J = 1/3 ≈ 0.333
//!   (C, D): co_commits = 1, commits_a = 2, commits_b = 1 → J = 1/2 = 0.5
//!   E never appears in any pair.
//!
//! At threshold 0.4 only (A, B) and (C, D) survive — exactly the two
//! "planted" coupled pairs.

use sqlx::PgPool;

pub struct GitHistoryHandles {
    pub project_id: i32,
    pub commit_ids: Vec<i64>,
}

/// Insert a project, 5 commits, and 5 file paths with the planted
/// co-change pattern. The project is named `git-coupled` so tests
/// reference it consistently.
pub async fn seed_git_history(pool: &PgPool) -> GitHistoryHandles {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws/coupled")
    .bind("/ws/coupled/git-coupled")
    .bind("git-coupled")
    .fetch_one(pool)
    .await
    .expect("project");

    // Commits — order matters for chronology but not for the
    // Jaccard formula. Use distinct hashes.
    let plan: &[(&str, &str, &str, &str, &[&str])] = &[
        // (commit_hash, author_name, author_email, subject, file_paths)
        (
            "aaa1",
            "alice",
            "a@x",
            "co AB #1",
            &["src/a.rs", "src/b.rs"],
        ),
        (
            "aaa2",
            "alice",
            "a@x",
            "co AB #2",
            &["src/a.rs", "src/b.rs"],
        ),
        (
            "aaa3",
            "alice",
            "a@x",
            "co ABCD",
            &["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"],
        ),
        ("aaa4", "bob", "b@x", "solo C", &["src/c.rs"]),
        ("aaa5", "bob", "b@x", "solo E", &["src/e.rs"]),
    ];

    let mut commit_ids = Vec::with_capacity(plan.len());
    for (hash, author_name, _author_email, subject, files) in plan {
        let commit_id: i64 = sqlx::query_scalar(
            "INSERT INTO git_commits \
             (project_id, commit_hash, author, author_date, subject, body) \
             VALUES ($1, $2, $3, NOW(), $4, $5) RETURNING id",
        )
        .bind(project_id)
        .bind(hash)
        .bind(author_name)
        .bind(subject)
        .bind("")
        .fetch_one(pool)
        .await
        .expect("git_commit");

        for file_path in *files {
            sqlx::query(
                "INSERT INTO git_commit_files (commit_id, file_path, change_type) \
                 VALUES ($1, $2, $3)",
            )
            .bind(commit_id)
            .bind(*file_path)
            .bind("M")
            .execute(pool)
            .await
            .expect("git_commit_files");
        }

        commit_ids.push(commit_id);
    }

    GitHistoryHandles {
        project_id,
        commit_ids,
    }
}
