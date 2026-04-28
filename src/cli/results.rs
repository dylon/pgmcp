//! `results [kind]` subcommand: print cached analysis results from the DB.
//!
//! Subcommands: `similarity`, `topics`. Without one, prints both.

use std::path::Path;

use clap::Subcommand;

use crate::config::Config;
use crate::db;

#[derive(Subcommand, Clone)]
pub enum ResultsKind {
    /// Show similarity analysis results
    Similarity,
    /// Show topic clustering results
    Topics,
}

pub async fn run(
    config_override: Option<&Path>,
    kind: Option<ResultsKind>,
    limit: i32,
) -> anyhow::Result<()> {
    crate::logging::init_cli();
    let config = Config::load(config_override)?;
    let pool = db::pool::create_pool(&config.database).await?;
    db::migrations::run_migrations(&pool, &config.vector).await?;

    match kind {
        Some(ResultsKind::Similarity) => {
            print_similarity_results(&pool, limit).await?;
        }
        Some(ResultsKind::Topics) => {
            print_topic_results(&pool, limit).await?;
        }
        None => {
            print_similarity_results(&pool, limit).await?;
            println!();
            print_topic_results(&pool, limit).await?;
        }
    }
    Ok(())
}

fn truncate_path(path: &str, max_len: usize) -> &str {
    if path.len() <= max_len {
        return path;
    }
    // Find a `/` boundary near the start of the tail
    let skip = path.len() - max_len;
    match path[skip..].find('/') {
        Some(pos) => &path[skip + pos..],
        None => &path[skip..],
    }
}

async fn print_similarity_results(pool: &sqlx::PgPool, limit: i32) -> anyhow::Result<()> {
    let total = db::queries::count_similarity_pairs(pool).await?;
    let pairs = db::queries::top_similar_file_pairs(pool, limit).await?;

    println!(
        "=== Cross-Project Similarity ({} total chunk pairs) ===",
        total
    );
    println!();

    if pairs.is_empty() {
        println!("No similarity data found.");
        println!("Run `pgmcp analyze similarity` to populate.");
        return Ok(());
    }

    // Header
    println!(
        "{:<40} {:<40} {:>6} {:>6} {:>6}",
        "File A", "File B", "Avg%", "Max%", "Chunks"
    );
    println!("{}", "-".repeat(100));

    for pair in &pairs {
        let path_a = format!(
            "{}:{}",
            pair.project_name_a,
            truncate_path(&pair.path_a, 30)
        );
        let path_b = format!(
            "{}:{}",
            pair.project_name_b,
            truncate_path(&pair.path_b, 30)
        );
        println!(
            "{:<40} {:<40} {:>5.1}% {:>5.1}% {:>6}",
            truncate_path(&path_a, 40),
            truncate_path(&path_b, 40),
            pair.avg_similarity * 100.0,
            pair.max_similarity * 100.0,
            pair.matching_chunks,
        );
    }

    Ok(())
}

async fn print_topic_results(pool: &sqlx::PgPool, limit: i32) -> anyhow::Result<()> {
    let topics = db::queries::load_cached_topics(pool, "global", limit).await?;

    println!("=== Topic Clustering (global) ===");
    println!();

    if topics.is_empty() {
        println!("No topic data found.");
        println!("Run `pgmcp analyze topics` to populate.");
        return Ok(());
    }

    for (i, topic) in topics.iter().enumerate() {
        let label = topic["label"].as_str().unwrap_or("unknown");
        let size = topic["size"].as_i64().unwrap_or(0);
        let files = topic["files"].as_i64().unwrap_or(0);
        let project_count = topic["project_count"].as_i64().unwrap_or(0);
        let cohesion = topic["avg_internal_similarity"]
            .as_f64()
            .map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "N/A".into());

        println!(
            "Topic {} — {} ({} chunks, {} files, {} projects, cohesion {})",
            i, label, size, files, project_count, cohesion,
        );

        // Keywords
        if let Some(keywords) = topic["keywords"].as_array() {
            let kw_list: Vec<&str> = keywords.iter().filter_map(|k| k.as_str()).collect();
            if !kw_list.is_empty() {
                println!("  Keywords: {}", kw_list.join(", "));
            }
        }

        // Representative snippet (first 3 lines)
        if let Some(snippet) = topic["representative_snippet"].as_str() {
            let preview: String = snippet
                .lines()
                .take(3)
                .map(|l| format!("  │ {}", l))
                .collect::<Vec<_>>()
                .join("\n");
            if !preview.is_empty() {
                println!("{}", preview);
            }
        }

        // Top files
        if let Some(top_files) = topic["representative_files"].as_array() {
            let file_list: Vec<&str> = top_files
                .iter()
                .filter_map(|f| f.as_str())
                .take(5)
                .collect();
            if !file_list.is_empty() {
                println!(
                    "  Files: {}",
                    file_list
                        .iter()
                        .map(|p| truncate_path(p, 50))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }

        if i < topics.len() - 1 {
            println!();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn short_path_passes_through() {
        assert_eq!(truncate_path("a/b", 10), "a/b");
    }

    #[test]
    fn truncates_to_slash_boundary_when_possible() {
        let truncated = truncate_path("very/long/path/file.rs", 10);
        assert!(truncated.starts_with('/'));
    }

    #[test]
    fn truncates_to_suffix_when_no_slash() {
        let truncated = truncate_path("longfilename_without_slashes", 10);
        assert_eq!(truncated.len(), 10);
    }

    proptest! {
        /// Every truncation output is at most `max_len` bytes (from the
        /// slash-boundary branch) or exactly `max_len` bytes (no-slash).
        #[test]
        fn prop_truncate_output_len_bounded(
            path in "[a-zA-Z0-9/_.-]{1,80}",
            max_len in 1usize..80,
        ) {
            let out = truncate_path(&path, max_len);
            prop_assert!(out.len() <= path.len(),
                "output ({} bytes) longer than input ({})", out.len(), path.len());
            if path.len() > max_len {
                prop_assert!(out.len() <= max_len,
                    "output ({} bytes) longer than max_len ({})", out.len(), max_len);
            }
        }

        /// Paths already within the limit are returned unchanged.
        #[test]
        fn prop_short_paths_unchanged(
            path in "[a-zA-Z0-9/_.-]{1,20}",
        ) {
            let max = path.len() + 10;
            prop_assert_eq!(truncate_path(&path, max), path.as_str());
        }

        /// When the input is truncated and the tail contains a `/`,
        /// the output starts with `/`.
        #[test]
        fn prop_truncated_output_starts_at_slash_boundary_when_possible(
            prefix in "[a-z]{1,10}",
            mid in "[a-z]{1,10}",
            suffix in "[a-z]{1,10}",
            max_len in 5usize..20,
        ) {
            let path = format!("{}/{}/{}", prefix, mid, suffix);
            prop_assume!(path.len() > max_len);
            let out = truncate_path(&path, max_len);
            let skip = path.len() - max_len;
            if path[skip..].contains('/') {
                prop_assert!(out.starts_with('/'),
                    "expected '/' prefix: path={}, max_len={}, out={}",
                    path, max_len, out);
            }
        }
    }
}
