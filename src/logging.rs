use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use chrono::Local;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

use crate::config::Config;

// ---------------------------------------------------------------------------
// RotatingFileAppender
// ---------------------------------------------------------------------------

/// Rotation period for log files.
enum RotationPeriod {
    Daily,
    Hourly,
    Never,
}

/// State protected by the RwLock: the open file handle and the period string
/// that was current when the file was opened.
struct AppenderState {
    file: File,
    /// `None` when rotation is `Never`; otherwise the period string
    /// (e.g. `"2026-03-09"` or `"2026-03-09-14"`) at the time the file was opened.
    current_period: Option<String>,
}

/// A rotating file appender that always writes to `{dir}/{filename}`.
///
/// On rotation boundary (daily/hourly) the current file is renamed to
/// `{filename}.{period}` and a fresh `{filename}` is opened.  Old rotated
/// files beyond `max_files` are pruned.
///
/// Implements [`MakeWriter`] so it can be used directly with
/// `tracing_subscriber`.
struct RotatingFileAppender {
    dir: PathBuf,
    filename: String,
    rotation: RotationPeriod,
    max_files: u32,
    state: RwLock<AppenderState>,
}

/// Writer guard returned by [`MakeWriter::make_writer`].
///
/// Holds the `RwLock` write‐guard for the duration of a single log event so
/// that the entire event is written atomically (no interleaving from other
/// threads).
struct AppenderGuard<'a>(std::sync::RwLockWriteGuard<'a, AppenderState>);

impl RotatingFileAppender {
    /// Create a new appender that writes to `{dir}/{filename}`.
    fn new(
        dir: PathBuf,
        filename: String,
        rotation: RotationPeriod,
        max_files: u32,
    ) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(&filename))?;

        let current_period = Self::period_string_for(&rotation);

        Ok(Self {
            dir,
            filename,
            rotation,
            max_files,
            state: RwLock::new(AppenderState {
                file,
                current_period,
            }),
        })
    }

    /// Return the period string for the current local time, or `None` for
    /// `Never` rotation.
    fn period_string_for(rotation: &RotationPeriod) -> Option<String> {
        match rotation {
            RotationPeriod::Daily => Some(Local::now().format("%Y-%m-%d").to_string()),
            RotationPeriod::Hourly => Some(Local::now().format("%Y-%m-%d-%H").to_string()),
            RotationPeriod::Never => None,
        }
    }

    /// If the rotation period has elapsed, rename the current log file to
    /// `{filename}.{old_period}` and open a fresh `{filename}`.
    ///
    /// **Must** be called while holding the write lock on `state`.
    fn maybe_rotate(&self, state: &mut AppenderState) -> io::Result<()> {
        let new_period = Self::period_string_for(&self.rotation);

        // Nothing to do when rotation is Never or the period hasn't changed.
        match (&state.current_period, &new_period) {
            (Some(old), Some(new)) if old != new => {
                // Period changed — rotate.
            }
            _ => return Ok(()),
        }

        // Flush the current file before renaming.
        state.file.flush()?;
        // Explicitly sync to disk so no buffered data is lost across the rename.
        state.file.sync_all()?;

        // Rename current file → {filename}.{old_period}
        let old_period = state.current_period.as_ref().expect("checked above");
        let rotated_name = format!("{}.{}", self.filename, old_period);
        let current_path = self.dir.join(&self.filename);
        let rotated_path = self.dir.join(&rotated_name);
        fs::rename(&current_path, &rotated_path)?;

        // Open a fresh log file.
        state.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&current_path)?;
        state.current_period = new_period;

        // Best‐effort cleanup of old rotated files.
        self.cleanup_old_files();

        Ok(())
    }

    /// Remove the oldest rotated files when their count exceeds `max_files`.
    ///
    /// Rotated files are named `{filename}.{period}` where the period is a
    /// lexicographically sortable date string, so sorting by name gives
    /// chronological order.
    fn cleanup_old_files(&self) {
        let prefix = format!("{}.", self.filename);
        let mut rotated: Vec<PathBuf> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(&prefix))
            })
            .map(|entry| entry.path())
            .collect();

        // Sort ascending (oldest first).
        rotated.sort();

        // Remove oldest files beyond the limit.
        let limit = self.max_files as usize;
        if rotated.len() > limit {
            let to_remove = rotated.len() - limit;
            for path in &rotated[..to_remove] {
                let _ = fs::remove_file(path);
            }
        }
    }
}

impl<'a> MakeWriter<'a> for RotatingFileAppender {
    type Writer = AppenderGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        let mut state = self
            .state
            .write()
            .expect("RotatingFileAppender lock poisoned");

        if let Err(e) = self.maybe_rotate(&mut state) {
            eprintln!("pgmcp: log rotation failed: {e}");
        }

        AppenderGuard(state)
    }
}

impl<'a> io::Write for AppenderGuard<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.file.flush()
    }
}

// ---------------------------------------------------------------------------
// Public initializers
// ---------------------------------------------------------------------------

/// Initialize tracing for one-shot CLI subcommands (`analyze`, `reindex`,
/// `tool`, `context`, `statistics`, `status`, `results`).
///
/// Without this, every `info!`/`warn!`/`error!` call inside the
/// subsystems these CLIs invoke is silently dropped — the user sees only
/// `println!` output. That has bitten debugging at least once
/// (`pgmcp analyze topics` reporting `0 topics, 0 noise chunks` after 214 s
/// of work, with the actual error invisible). See
/// `~/.claude/plans/thoroughly-examine-home-dylon-workspace-melodic-cake.md`.
///
/// - Writes to stderr (stdout is reserved for the CLI's structured
///   `println!` output, e.g. JSON tool results).
/// - Default level `info`; overridable via `RUST_LOG`.
/// - `try_init` so it's safe to call multiple times in tests / re-entrant
///   harness paths.
/// - ANSI colours only when stderr is a TTY.
pub fn init_cli() {
    use std::io::IsTerminal;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(std::io::stderr().is_terminal())
                .with_target(false)
                .with_thread_ids(false),
        )
        .try_init();
}

/// Initialize tracing for foreground (serve) mode.
/// Logs to stderr so stdout remains clean for MCP stdio transport.
pub fn init_foreground(config: &Config) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.logging.level));

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
/// Logs to a rotating file appender that always writes to `{dir}/{filename}`.
/// On rotation, the current file is renamed with a date suffix and a fresh
/// file is opened.
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

    let rotation = match config.logging.rotation.as_str() {
        "daily" => RotationPeriod::Daily,
        "hourly" => RotationPeriod::Hourly,
        "never" => RotationPeriod::Never,
        _ => RotationPeriod::Daily,
    };

    let file_appender = RotatingFileAppender::new(
        log_dir.to_path_buf(),
        log_filename.to_string(),
        rotation,
        config.logging.max_log_files,
    )
    .expect("Failed to create log file appender");

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.logging.level));

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
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}
