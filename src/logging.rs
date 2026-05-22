use std::collections::BTreeMap;
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

/// Compose an `EnvFilter` from the configured global level + optional
/// per-target overrides. `RUST_LOG` (when set) takes precedence over both;
/// per-target overrides extend whichever filter was chosen.
fn build_env_filter(level: &str, targets: &BTreeMap<String, String>) -> EnvFilter {
    let mut filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    for (target, lvl) in targets {
        let directive_str = format!("{}={}", target, lvl);
        match directive_str.parse() {
            Ok(d) => filter = filter.add_directive(d),
            Err(e) => eprintln!(
                "pgmcp: ignoring invalid [logging] targets directive `{}`: {}",
                directive_str, e,
            ),
        }
    }
    filter
}

// ---------------------------------------------------------------------------
// RotatingFileAppender
// ---------------------------------------------------------------------------

/// Rotation period for log files.
#[derive(Clone, Copy)]
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
/// Two layers when called with `Some(&config)`:
///
/// - Stderr (always): human-readable, ANSI when a TTY, level from
///   `RUST_LOG` else `config.logging.level` else `info`. Stdout is
///   reserved for structured CLI output (JSON, tables), so log lines
///   stay on stderr.
/// - File (only when `config` is `Some` and the file is writable):
///   JSON, no ANSI, no rotation. The daemon owns rotation; the CLI just
///   appends to the current file. If the daemon rotates mid-CLI, this
///   CLI's events continue landing in the now-rotated file for the rest
///   of the CLI's lifetime — acceptable for short-lived invocations.
///
/// Subcommands that load `Config::load()` should call this with
/// `Some(&config)` after the load succeeds so `pgmcp tool foo` lands
/// in `~/.local/share/pgmcp/pgmcp.log` and is visible to `tail -f`.
/// `try_init` so it's safe to call multiple times in tests.
pub fn init_cli_with_config(config: Option<&Config>) {
    use std::io::IsTerminal;
    use std::sync::Mutex;

    // The CLI has no config in the no-args case; default to `info` with no
    // per-target overrides. With a config, honor `level` + `targets`.
    let empty_targets: BTreeMap<String, String> = BTreeMap::new();
    let (level, targets) = match config {
        Some(c) => (c.logging.level.as_str(), &c.logging.targets),
        None => ("info", &empty_targets),
    };

    let try_with_file = || -> Option<()> {
        let cfg = config?;
        let log_path = expand_tilde(&cfg.logging.file);
        let parent = Path::new(&log_path).parent()?;
        let _ = fs::create_dir_all(parent);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok()?;
        let filter = build_env_filter(level, targets);
        let stderr_layer = fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(std::io::stderr().is_terminal())
            .with_target(false)
            .with_thread_ids(false);
        let file_layer = make_format_layer(&cfg.logging.format, Mutex::new(file));
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(stderr_layer)
            .with(file_layer)
            .try_init();
        Some(())
    };
    if try_with_file().is_some() {
        return;
    }

    let filter = build_env_filter(level, targets);
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
    let filter = build_env_filter(&config.logging.level, &config.logging.targets);

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

/// Build the configured file-writer rotation policy from
/// `[logging] rotation = "daily" | "hourly" | "never"`. Unknown values
/// fall through to `Daily`.
fn parse_rotation(rotation: &str) -> RotationPeriod {
    match rotation {
        "daily" => RotationPeriod::Daily,
        "hourly" => RotationPeriod::Hourly,
        "never" => RotationPeriod::Never,
        _ => RotationPeriod::Daily,
    }
}

/// Open a rotating file appender for the given path, creating any
/// missing parent directories.
fn make_rotating_appender(
    path: &str,
    rotation: RotationPeriod,
    max_files: u32,
) -> RotatingFileAppender {
    let log_path = expand_tilde(path);
    let log_dir = Path::new(&log_path)
        .parent()
        .expect("Log file path must have a parent directory");
    let log_filename = Path::new(&log_path)
        .file_name()
        .expect("Log file path must have a filename")
        .to_str()
        .expect("Log filename must be valid UTF-8");
    std::fs::create_dir_all(log_dir).expect("Failed to create log directory");
    RotatingFileAppender::new(
        log_dir.to_path_buf(),
        log_filename.to_string(),
        rotation,
        max_files,
    )
    .expect("Failed to create log file appender")
}

/// Build a fmt layer for the main log file with the configured output
/// format. Returns a boxed layer so the three format branches share a
/// uniform type. The layer always writes to a file appender (no ANSI,
/// with target and thread ids).
fn make_format_layer<S, W>(
    format: &str,
    writer: W,
) -> Box<dyn tracing_subscriber::Layer<S> + Send + Sync + 'static>
where
    S: tracing::Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let base = fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true);
    match format {
        "compact" => base.compact().boxed(),
        "pretty" => base.pretty().boxed(),
        // Default — and explicitly chosen `json` — both pick JSON output.
        _ => base.json().boxed(),
    }
}

/// Initialize tracing for daemon mode.
///
/// Always writes to a rotating file appender at `config.logging.file`.
/// Output format follows `config.logging.format` (`json` | `compact` |
/// `pretty`). If `config.logging.access_log` is set, a second layer
/// filtered to events from the `pgmcp::mcp::tool` target (i.e. the
/// `invoked` / `completed` / `failed` events from
/// `instrumented_tool_run`) writes to that path with the same rotation
/// policy — an nginx-style access log of MCP tool traffic, separate
/// from general daemon logs.
pub fn init_daemon(config: &Config) {
    let rotation = parse_rotation(&config.logging.rotation);
    let main_appender =
        make_rotating_appender(&config.logging.file, rotation, config.logging.max_log_files);
    let filter = build_env_filter(&config.logging.level, &config.logging.targets);
    let main_layer = make_format_layer(&config.logging.format, main_appender);

    let registry = tracing_subscriber::registry().with(filter).with(main_layer);

    if let Some(access_path) = config.logging.access_log.as_deref() {
        let access_appender =
            make_rotating_appender(access_path, rotation, config.logging.max_log_files);
        let access_layer = fmt::layer()
            .with_writer(access_appender)
            .with_ansi(false)
            .with_target(true)
            .with_thread_ids(true)
            .json()
            .with_filter(tracing_subscriber::filter::filter_fn(|m| {
                m.target() == "pgmcp::mcp::tool"
            }));
        registry.with(access_layer).init();
    } else {
        registry.init();
    }
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}
