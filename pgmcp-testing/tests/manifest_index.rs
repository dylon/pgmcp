//! Real-Postgres + filesystem test for the Cargo-manifest dependency parser: a
//! `consumer` crate with a `path = "../dependency"` dep yields a `cargo`/`path`
//! `project_dependencies` edge to the `dependency` project.

use pgmcp::deps::manifest::index_project_manifests;
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn manifest_indexes_path_dependency() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // Temp workspace: consumer/ (path-deps on ../dependency) + dependency/.
    let base = std::env::temp_dir().join(format!("pgmcp_manifest_{}", std::process::id()));
    let consumer_dir = base.join("consumer");
    let dependency_dir = base.join("dependency");
    std::fs::create_dir_all(&consumer_dir).expect("mkdir consumer");
    std::fs::create_dir_all(&dependency_dir).expect("mkdir dependency");
    std::fs::write(
        consumer_dir.join("Cargo.toml"),
        "[package]\nname = \"consumer\"\nversion = \"0.1.0\"\n\
         [dependencies]\ndependency = { path = \"../dependency\" }\n",
    )
    .expect("write consumer manifest");
    std::fs::write(
        dependency_dir.join("Cargo.toml"),
        "[package]\nname = \"dependency\"\nversion = \"0.1.0\"\n",
    )
    .expect("write dependency manifest");

    let consumer_canon = consumer_dir.canonicalize().expect("canon consumer");
    let dependency_canon = dependency_dir.canonicalize().expect("canon dependency");

    let consumer_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1,$2,$3) RETURNING id",
    )
    .bind("/ws")
    .bind(consumer_canon.to_string_lossy().as_ref())
    .bind("consumer-m")
    .fetch_one(&pool)
    .await
    .expect("seed consumer project");
    let dependency_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1,$2,$3) RETURNING id",
    )
    .bind("/ws")
    .bind(dependency_canon.to_string_lossy().as_ref())
    .bind("dependency-m")
    .fetch_one(&pool)
    .await
    .expect("seed dependency project");

    let (up, _closed) = index_project_manifests(
        &pool,
        consumer_id,
        consumer_canon.to_string_lossy().as_ref(),
    )
    .await;
    std::fs::remove_dir_all(&base).ok();
    assert!(up >= 1, "should upsert the path dependency edge");

    let edge: Option<(i32, String, String)> = sqlx::query_as(
        "SELECT dependency_project_id, kind, source
           FROM project_dependencies
          WHERE dependent_project_id = $1 AND valid_to IS NULL",
    )
    .bind(consumer_id)
    .fetch_optional(&pool)
    .await
    .expect("query edge");
    let (dep_id, kind, source) = edge.expect("path-dep edge exists");
    assert_eq!(dep_id, dependency_id);
    assert_eq!(kind, "path");
    assert_eq!(source, "cargo");
}
