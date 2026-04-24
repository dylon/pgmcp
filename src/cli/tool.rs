//! `pgmcp tool` subcommand: list / inspect / invoke any MCP tool from CLI.
//!
//! `parse_tool_args` converts space-separated `KEY=VALUE` pairs from the
//! shell into the JSON `Value::Object` that an MCP tool expects. Auto-types
//! values into i64 / f64 / bool / string. Repeated keys collapse into an
//! array (for tools that take `Vec<T>` params like `edge_types`).

use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::context::SystemContext;
use crate::db;
use crate::embed;
use crate::mcp;
use crate::stats;

pub async fn run(
    config_override: Option<&Path>,
    name: Option<String>,
    args: Vec<String>,
    json: bool,
    schema: bool,
) -> anyhow::Result<()> {
    // Tier 1: list / --schema — no DB, no embed model
    let catalog = mcp::server::McpServer::static_tool_catalog();
    match name {
        None => {
            list_tools(&catalog);
            Ok(())
        }
        Some(ref tool_name) if schema => {
            show_tool_schema(&catalog, tool_name)?;
            Ok(())
        }
        Some(ref tool_name) => {
            // Tier 2+3: tool execution — DB required, embed model lazy
            let config = Config::load(config_override)?;
            let pool = db::pool::create_pool(&config.database).await?;
            db::migrations::run_migrations(&pool, &config.vector).await?;
            let stats = Arc::new(stats::tracker::StatsTracker::new());
            let config_arc = Arc::new(ArcSwap::from_pointee(config));
            let log_broadcaster = Arc::new(mcp::logging::LogBroadcaster::new());
            let task_store = Arc::new(mcp::tasks::TaskStore::new());
            // Lazy embed: no pool running, model created on first embedding tool call
            let db: Arc<dyn db::DbClient> = Arc::new(pool);
            let cli_ctx = SystemContext::production(
                db,
                embed::EmbedSource::lazy(config_arc.load().embeddings.clone()),
                stats,
                config_arc,
                log_broadcaster,
                task_store,
            );
            let server = mcp::server::McpServer::new(cli_ctx);

            let tool_args = parse_tool_args(&args);
            match server.call_tool_cli(tool_name, tool_args).await {
                Ok(result) => {
                    print_tool_result(&result, json);
                    if result.is_error == Some(true) {
                        std::process::exit(1);
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("Error: {}", e.message);
                    std::process::exit(1);
                }
            }
        }
    }
}

/// Parse `KEY=VALUE` argv pairs into a JSON object.
///
/// - `i64` first, then `f64`, then `bool`, then `String`.
/// - Repeated keys collapse to a JSON array (in argv order).
/// - Args without `=` are skipped with a stderr warning.
pub fn parse_tool_args(args: &[String]) -> serde_json::Value {
    use serde_json::{Map, Value};

    let mut map = Map::new();

    for arg in args {
        let (key, val_str) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => {
                eprintln!("Warning: ignoring argument without '=': {}", arg);
                continue;
            }
        };

        // Auto-parse the value: try i64 → f64 → bool → string
        let value = if let Ok(n) = val_str.parse::<i64>() {
            Value::Number(n.into())
        } else if let Ok(f) = val_str.parse::<f64>() {
            Value::Number(serde_json::Number::from_f64(f).unwrap_or_else(|| 0.into()))
        } else if val_str == "true" {
            Value::Bool(true)
        } else if val_str == "false" {
            Value::Bool(false)
        } else {
            Value::String(val_str)
        };

        // Repeated keys → array (for Vec<String> params like edge_types, smells)
        if let Some(existing) = map.get_mut(&key) {
            match existing {
                Value::Array(arr) => arr.push(value),
                _ => {
                    let prev = existing.clone();
                    *existing = Value::Array(vec![prev, value]);
                }
            }
        } else {
            map.insert(key, value);
        }
    }

    Value::Object(map)
}

fn list_tools(tools: &[rmcp::model::Tool]) {
    println!("Available pgmcp tools ({} total):", tools.len());
    println!();

    // Group by category: infer from first word/prefix of tool name
    let categories: &[(&str, &[&str])] = &[
        (
            "Search",
            &[
                "semantic_search",
                "text_search",
                "grep",
                "hybrid_search",
                "search_commits",
            ],
        ),
        (
            "File Info",
            &[
                "read_file",
                "project_tree",
                "file_info",
                "list_projects",
                "index_stats",
                "reindex",
            ],
        ),
        (
            "Similarity",
            &[
                "compare_files",
                "find_similar_modules",
                "find_duplicates",
                "refactoring_report",
            ],
        ),
        (
            "Topics",
            &[
                "discover_topics",
                "find_orphans",
                "find_misplaced_code",
                "find_coupled_files",
                "test_coverage_gaps",
                "complexity_hotspots",
                "topic_hierarchy",
                "suggest_merges",
                "suggest_splits",
                "doc_coverage_gaps",
            ],
        ),
        (
            "Graph",
            &[
                "dependency_graph",
                "centrality_analysis",
                "community_detection",
                "circular_dependencies",
                "change_impact_analysis",
            ],
        ),
        (
            "Architecture",
            &[
                "coupling_cohesion_report",
                "architecture_violations",
                "design_smell_detection",
                "architecture_quality",
                "design_metrics",
            ],
        ),
        (
            "Prediction",
            &[
                "bug_prediction",
                "technical_debt_analysis",
                "anomaly_detection",
            ],
        ),
        ("Advanced", &["code_summarize", "engineering_scorecard"]),
    ];

    let tool_map: std::collections::HashMap<&str, &rmcp::model::Tool> =
        tools.iter().map(|t| (t.name.as_ref(), t)).collect();

    for (category, names) in categories {
        let mut found = false;
        for name in *names {
            if let Some(tool) = tool_map.get(name) {
                if !found {
                    println!("  {}:", category);
                    found = true;
                }
                let desc = tool.description.as_deref().unwrap_or("");
                // First sentence only
                let short = desc.split_once(". ").map(|(s, _)| s).unwrap_or(desc);
                let short = if short.len() > 70 {
                    &short[..70]
                } else {
                    short
                };
                println!("    {:<30} {}", name, short);
            }
        }
        if found {
            println!();
        }
    }

    // Show any uncategorized tools
    let categorized: std::collections::HashSet<&str> = categories
        .iter()
        .flat_map(|(_, names)| names.iter().copied())
        .collect();
    let mut uncategorized = false;
    for tool in tools {
        if !categorized.contains(tool.name.as_ref()) {
            if !uncategorized {
                println!("  Other:");
                uncategorized = true;
            }
            let desc = tool.description.as_deref().unwrap_or("");
            let short = desc.split_once(". ").map(|(s, _)| s).unwrap_or(desc);
            let short = if short.len() > 70 {
                &short[..70]
            } else {
                short
            };
            println!("    {:<30} {}", tool.name, short);
        }
    }
    if uncategorized {
        println!();
    }

    println!("Usage: pgmcp tool <name> [KEY=VALUE ...]");
    println!("       pgmcp tool <name> --schema    # show parameter schema");
    println!("       pgmcp tool <name> --json      # compact JSON output");
}

fn show_tool_schema(tools: &[rmcp::model::Tool], name: &str) -> anyhow::Result<()> {
    let tool = tools
        .iter()
        .find(|t| t.name.as_ref() == name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown tool: '{}'. Run `pgmcp tool` to list available tools.",
                name
            )
        })?;

    println!("Tool: {}", tool.name);
    if let Some(desc) = &tool.description {
        println!();
        println!("{}", desc);
    }
    println!();
    println!("Parameters:");
    let schema_json = serde_json::to_string_pretty(&*tool.input_schema)?;
    println!("{}", schema_json);

    Ok(())
}

fn print_tool_result(result: &rmcp::model::CallToolResult, compact: bool) {
    for content in &result.content {
        match &content.raw {
            rmcp::model::RawContent::Text(text_content) => {
                if compact {
                    println!("{}", text_content.text);
                } else {
                    // Try to pretty-print JSON, fallback to raw text
                    match serde_json::from_str::<serde_json::Value>(&text_content.text) {
                        Ok(json) => {
                            if let Ok(pretty) = serde_json::to_string_pretty(&json) {
                                println!("{}", pretty);
                            } else {
                                println!("{}", text_content.text);
                            }
                        }
                        Err(_) => {
                            println!("{}", text_content.text);
                        }
                    }
                }
            }
            _ => {
                eprintln!("[non-text content]");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn empty_args_returns_empty_object() {
        let v = parse_tool_args(&[]);
        assert_eq!(v, json!({}));
    }

    #[test]
    fn parses_typed_values() {
        let args = vec![
            "limit=10".to_string(),
            "threshold=0.85".to_string(),
            "json=true".to_string(),
            "project=foo".to_string(),
        ];
        let v = parse_tool_args(&args);
        assert_eq!(v["limit"], 10);
        assert_eq!(v["threshold"], 0.85);
        assert_eq!(v["json"], true);
        assert_eq!(v["project"], "foo");
    }

    #[test]
    fn repeated_keys_collapse_to_array() {
        let args = vec![
            "edge_types=import".to_string(),
            "edge_types=co_change".to_string(),
            "edge_types=semantic".to_string(),
        ];
        let v = parse_tool_args(&args);
        assert_eq!(v["edge_types"], json!(["import", "co_change", "semantic"]));
    }

    #[test]
    fn arg_without_equals_is_skipped() {
        let args = vec!["valid=1".to_string(), "garbage".to_string()];
        let v = parse_tool_args(&args);
        assert_eq!(v["valid"], 1);
        assert!(v.as_object().expect("object").len() == 1);
    }

    #[test]
    fn empty_value_parses_as_empty_string() {
        let args = vec!["key=".to_string()];
        let v = parse_tool_args(&args);
        assert_eq!(v["key"], "");
    }

    #[test]
    fn value_with_equals_preserves_rhs() {
        // `=` after the first one belongs to the value (split_once).
        let args = vec!["q=a=b=c".to_string()];
        let v = parse_tool_args(&args);
        assert_eq!(v["q"], "a=b=c");
    }

    proptest! {
        /// Any valid (key, i64) round-trips — stored as JSON number == original.
        #[test]
        fn prop_round_trips_i64_values(
            key in "[a-z][a-z0-9_]{0,10}",
            value in i32::MIN as i64..=i32::MAX as i64,
        ) {
            let args = vec![format!("{}={}", key, value)];
            let v = parse_tool_args(&args);
            prop_assert_eq!(v[&key].as_i64(), Some(value));
        }

        /// Any valid (key, bool) round-trips as JSON bool.
        #[test]
        fn prop_round_trips_bool_values(
            key in "[a-z][a-z0-9_]{0,10}",
            value: bool,
        ) {
            let args = vec![format!("{}={}", key, value)];
            let v = parse_tool_args(&args);
            prop_assert_eq!(v[&key].as_bool(), Some(value));
        }

        /// Repeated keys preserve argv order in the resulting array.
        /// The regex excludes strings that would auto-parse as numbers,
        /// bools, or special floats ("nan", "inf") — parse_tool_args
        /// intentionally type-coerces those, which would make the
        /// as_str() round-trip lose information.
        #[test]
        fn prop_repeated_keys_preserve_order(
            // Start with a letter so we can't accidentally match "true",
            // "false", "nan", "inf" (all lowercase).
            values in prop::collection::vec("[g-m][a-z]{1,6}", 2..8usize),
        ) {
            let args: Vec<String> = values.iter().map(|v| format!("k={}", v)).collect();
            let parsed = parse_tool_args(&args);
            let arr = parsed["k"].as_array().expect("array");
            prop_assert_eq!(arr.len(), values.len());
            for (actual, expected) in arr.iter().zip(values.iter()) {
                prop_assert_eq!(actual.as_str(), Some(expected.as_str()));
            }
        }

        /// Arguments without `=` are silently dropped.
        #[test]
        fn prop_args_without_equals_dropped(
            with in prop::collection::vec("[a-z]{1,5}", 0..5usize),
            without in prop::collection::vec("[a-z]{1,5}", 0..5usize),
        ) {
            let mut args: Vec<String> = with.iter().map(|k| format!("{}=1", k)).collect();
            args.extend(without.iter().cloned());
            let parsed = parse_tool_args(&args);
            let obj = parsed.as_object().expect("object");
            // Unique keys from `with` should appear; none from `without`.
            let unique: std::collections::HashSet<&String> = with.iter().collect();
            for key in &unique {
                prop_assert!(obj.contains_key(key.as_str()),
                    "missing key {}", key);
            }
            for key in &without {
                if !unique.contains(key) {
                    prop_assert!(!obj.contains_key(key.as_str()),
                        "unexpected key {} from non-equals argv", key);
                }
            }
        }

        /// Keys are case-sensitive — `Foo` and `foo` are distinct.
        #[test]
        fn prop_keys_case_sensitive(
            lower in "[a-z]{3,8}",
            _ in any::<u8>(),
        ) {
            let upper: String = lower.to_uppercase();
            prop_assume!(upper != lower);
            let args = vec![
                format!("{}=1", lower),
                format!("{}=2", upper),
            ];
            let parsed = parse_tool_args(&args);
            let obj = parsed.as_object().expect("object");
            prop_assert!(obj.contains_key(&lower));
            prop_assert!(obj.contains_key(&upper));
            prop_assert_eq!(obj[&lower].as_i64(), Some(1));
            prop_assert_eq!(obj[&upper].as_i64(), Some(2));
        }
    }
}
