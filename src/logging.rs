use std::path::Path;

use tracing_appender::rolling;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

/// Initialize tracing for foreground (serve) mode.
/// Logs to stderr so stdout remains clean for MCP stdio transport.
pub fn init_foreground(config: &Config) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.logging.level));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(true)
                .with_target(true)
                .with_thread_ids(true),
        )
        .init();
}

/// Initialize tracing for daemon mode.
/// Logs to a rotating file appender.
pub fn init_daemon(config: &Config) {
    let log_path = expand_tilde(&config.logging.file);
    let log_dir = Path::new(&log_path)
        .parent()
        .expect("Log file path must have a parent directory");
    let log_filename = Path::new(&log_path)
        .file_name()
        .expect("Log file path must have a filename")
        .to_str()
        .expect("Log filename must be valid UTF-8");

    // Ensure log directory exists
    std::fs::create_dir_all(log_dir).expect("Failed to create log directory");

    let file_appender = match config.logging.rotation.as_str() {
        "daily" => rolling::daily(log_dir, log_filename),
        "hourly" => rolling::hourly(log_dir, log_filename),
        "never" => rolling::never(log_dir, log_filename),
        _ => rolling::daily(log_dir, log_filename),
    };

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.logging.level));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(file_appender)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(true)
                .json(),
        )
        .init();
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}
