//! P14.4 — per-project `PgmcpPhonetics` registry is honored by
//! `tool_phonetic_normalize`.
//!
//! Pre-P14.4 the phonetic tools called `PgmcpPhonetics::default_english()`
//! per request, so per-project `.pgmcp/rules.llev` overrides were
//! inert. Post-P14.4 `ctx.phonetics_for(project)` consults the
//! registry first; this test pre-populates the registry with a
//! single-rule `.llev` (`a -> z`), calls the tool twice (with and
//! without `project`), and asserts only the per-project call applies
//! the rule.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::fuzzy::phonetic::install_phonetics_for_project;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::PhoneticNormalizeParams;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::tool_phonetic_normalize;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

/// One-rule `.llev` that maps every `a` → `z`. Tiny on purpose; we
/// just need a rule that materially changes the output so the test
/// can prove the per-project rule set is being consulted.
const A_TO_Z_RULES: &str = "@name \"a-to-z-test\"\n@version \"1\"\na -> z;\n";

#[tokio::test(flavor = "multi_thread")]
async fn per_project_rule_overrides_default_english() {
    let testdb = require_test_db!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root: PathBuf = tmp.path().join("my_project_a2z");
    std::fs::create_dir_all(&project_root).expect("mkdir project root");
    let rules_path = project_root.join(".pgmcp").join("rules.llev");
    std::fs::create_dir_all(rules_path.parent().expect("parent")).expect("mkdir .pgmcp");
    std::fs::write(&rules_path, A_TO_Z_RULES).expect("write rules");

    let ctx = build_ctx(Arc::new(testdb.pool().clone()));
    install_phonetics_for_project(
        &project_root,
        &rules_path,
        Some("en-us"),
        ctx.phonetics_registry(),
    )
    .expect("install");

    // With project set, the rule should fire (a→z).
    let with_project = tool_phonetic_normalize::run(
        &ctx,
        PhoneticNormalizeParams {
            term: "alphabet".to_string(),
            project: Some("my_project_a2z".to_string()),
        },
    )
    .await
    .expect("with-project call");
    let with_text = with_project
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text");
    let with_val: serde_json::Value = serde_json::from_str(&with_text).expect("with json");
    let with_normalized = with_val["normalized"].as_str().unwrap_or("");
    assert!(
        with_normalized.contains('z'),
        "per-project a→z rule must produce z in the output; got {with_normalized:?}"
    );
    assert!(
        !with_normalized.contains('a'),
        "per-project a→z rule must remove all `a` chars; got {with_normalized:?}"
    );

    // Without project the embedded English default is used; the
    // a→z mapping does NOT exist in English base rules.
    let without_project = tool_phonetic_normalize::run(
        &ctx,
        PhoneticNormalizeParams {
            term: "alphabet".to_string(),
            project: None,
        },
    )
    .await
    .expect("no-project call");
    let without_text = without_project
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text");
    let without_val: serde_json::Value = serde_json::from_str(&without_text).expect("no json");
    let without_normalized = without_val["normalized"].as_str().unwrap_or("");
    assert!(
        !without_normalized.contains('z') || without_normalized.contains('a'),
        "default English normalization must not match the per-project a→z transform: got \
         {without_normalized:?}"
    );
}

fn build_ctx(db: Arc<dyn DbClient>) -> SystemContext {
    let config = Arc::new(ArcSwap::from_pointee(Config::default()));
    let stats = Arc::new(StatsTracker::new());
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
    SystemContext::production(
        db,
        EmbedSource::backend(embed_backend),
        stats,
        config,
        log_broadcaster,
        task_store,
        DaemonLifecycle::new(),
    )
}

// Suppress unused import — DashMap is referenced via the phonetics
// registry's concrete type but the test doesn't construct one
// directly.
#[allow(dead_code)]
fn _touch_dashmap() -> Arc<DashMap<PathBuf, ()>> {
    Arc::new(DashMap::new())
}
