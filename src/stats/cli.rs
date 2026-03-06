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
            // Parse Prometheus text format and display as table
            for line in body.lines() {
                if line.starts_with('#') || line.is_empty() {
                    continue;
                }
                if let Some((key, value)) = line.split_once(' ') {
                    let display_key = key
                        .strip_prefix("pgmcp_")
                        .unwrap_or(key)
                        .replace('_', " ");
                    println!("  {:<30} {}", display_key, value);
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
    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or("")
        .to_string();

    Ok(body)
}
