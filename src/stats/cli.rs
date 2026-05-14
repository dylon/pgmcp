//! CLI stats display.

use crate::config::Config;

/// Print statistics from the running instance.
pub async fn print_stats(config: &Config) -> anyhow::Result<()> {
    let url = format!(
        "http://{}:{}/metrics",
        config.metrics.http_bind, config.metrics.http_port
    );

    println!("Fetching stats from {}", url);

    // Try to connect to the metrics endpoint
    match reqwest_get(&url).await {
        Ok(body) => {
            println!("\npgmcp Statistics");
            println!("{}", "=".repeat(50));
            println!(
                "Three-pool architecture: InferencePool (GPU embed),\n\
                 CronPool (cron tasks), GeneralPool (CPU misc).\n\
                 The work-pool counters below reflect the GeneralPool —\n\
                 the InferencePool's activity is in the Embedding Pool group."
            );

            // Parse Prometheus text format into (key, value) pairs
            let metrics: Vec<(&str, &str)> = body
                .lines()
                .filter(|l| !l.starts_with('#') && !l.is_empty())
                .filter_map(|l| l.split_once(' '))
                .collect();

            // Group definitions: (section_name, prefix_match)
            let groups: &[(&str, &[&str])] = &[
                (
                    "Indexing",
                    &[
                        "pgmcp_files_indexed",
                        "pgmcp_files_failed",
                        "pgmcp_files_submitted",
                        "pgmcp_files_aborted_fk",
                        "pgmcp_chunks_embedded",
                        "pgmcp_bytes_processed",
                        "pgmcp_index_duration",
                        "pgmcp_embedding_duration",
                        "pgmcp_last_index",
                    ],
                ),
                (
                    "MCP",
                    &[
                        "pgmcp_mcp_",
                        "pgmcp_semantic_",
                        "pgmcp_text_",
                        "pgmcp_grep_",
                    ],
                ),
                (
                    "Scan",
                    &[
                        "pgmcp_files_scanned",
                        "pgmcp_files_skipped",
                        "pgmcp_files_stale",
                    ],
                ),
                (
                    "GeneralPool (CPU misc)",
                    &[
                        "pgmcp_active_threads",
                        "pgmcp_queue_depth",
                        "pgmcp_work_pool_",
                    ],
                ),
                ("InferencePool (GPU embed)", &["pgmcp_embed_"]),
                ("Cron", &["pgmcp_cron_"]),
                ("Git History", &["pgmcp_git_"]),
                ("Config Watcher", &["pgmcp_config_"]),
                ("File Watcher", &["pgmcp_watcher_"]),
                (
                    "Content storage",
                    &[
                        "pgmcp_files_with_null_bytes_stripped",
                        "pgmcp_files_with_content_omitted",
                        "pgmcp_documents_extraction_oom",
                        "pgmcp_read_file_disk_hits",
                        "pgmcp_read_file_disk_hash_mismatches",
                        "pgmcp_read_file_disk_io_errors",
                        "pgmcp_read_file_chunk_stitches",
                    ],
                ),
                ("System", &["pgmcp_uptime_"]),
            ];

            for (section, prefixes) in groups {
                let section_metrics: Vec<_> = metrics
                    .iter()
                    .filter(|(key, _)| prefixes.iter().any(|p| key.starts_with(p)))
                    .collect();

                if section_metrics.is_empty() {
                    continue;
                }

                println!("\n  {}:", section);
                for (key, value) in section_metrics {
                    let display_key = key.strip_prefix("pgmcp_").unwrap_or(key).replace('_', " ");
                    println!("    {:<34} {}", display_key, value);
                }
            }
        }
        Err(e) => {
            println!("Failed to connect to pgmcp: {}", e);
            println!("Is pgmcp running? Try: pgmcp serve");
        }
    }

    Ok(())
}

/// Simple HTTP GET using tokio's TCP stream (no external HTTP client dependency).
async fn reqwest_get(url: &str) -> anyhow::Result<String> {
    // Parse URL
    let url = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("Only http:// URLs supported"))?;

    let (host_port, path) = url.split_once('/').unwrap_or((url, "metrics"));

    let stream = tokio::net::TcpStream::connect(host_port).await?;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut reader, mut writer) = stream.into_split();

    let request = format!(
        "GET /{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, host_port
    );
    writer.write_all(request.as_bytes()).await?;

    let mut response = String::new();
    reader.read_to_string(&mut response).await?;

    // Extract body (after double CRLF)
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();

    Ok(body)
}
