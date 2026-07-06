//! `GET /api/logs/tail` + `GET /api/logs/grep` — a **bounded** log-tail and
//! fuzzy-grep surface for the webui *Logs* pane.
//!
//! Both endpoints read the daemon's own log file (`[logging] file` in
//! `src/config.rs`, tilde-expanded exactly as `crate::logging` expands it) from
//! the **tail** only: they `seek` to `max(0, len − 2 MiB)` and read a window
//! capped at [`MAX_TAIL_BYTES`]. A multi-gigabyte rotated log is therefore never
//! pulled into memory — the cost of either endpoint is `O(min(file_len, 2 MiB))`
//! regardless of on-disk size. All filesystem work runs on a
//! `tokio::task::spawn_blocking` thread so the async runtime is never blocked by
//! a synchronous read.
//!
//! # `GET /api/logs/tail?lines=<int>&level=<opt>`
//!
//! Returns the last `lines` (default [`DEFAULT_TAIL_LINES`], capped at
//! [`MAX_TAIL_LINES`]) lines of the window, optionally filtered to a log
//! `level`. Response:
//!
//! ```json
//! {
//!   "path": "/home/user/.local/share/pgmcp/pgmcp.log",
//!   "lines": [
//!     { "text": "invoked", "level": "INFO", "ts": "2026-07-05T00:00:00.9Z",
//!       "target": "pgmcp::mcp::tool" }
//!   ],
//!   "truncated": true
//! }
//! ```
//!
//! When `[logging] format = "json"` each line is parsed and `level` /
//! `timestamp` / `target` / `fields.message` are lifted into the structured
//! fields; a line that fails to parse (e.g. a raw panic backtrace interleaved
//! with the JSON stream) falls back to `{ "text": <raw>, "level": null, … }`.
//! For any non-JSON format every line is returned verbatim as `text` with null
//! metadata. `truncated` is `true` when older lines exist before the returned
//! slice (either the 2 MiB window dropped the head of the file, or more lines
//! matched than were returned).
//!
//! # `GET /api/logs/grep?q=<pattern>&distance=<0..3>&token=<0|1>&case_insensitive=<0|1>&limit=<int>`
//!
//! Fuzzy-greps the same tail window with `liblevenshtein`. Response:
//!
//! ```json
//! {
//!   "matches": [
//!     { "line_number": 42, "line": "…the raw line…",
//!       "matched": [ { "text": "hello", "start": 5, "end": 9, "distance": 1 } ] }
//!   ],
//!   "truncated": false
//! }
//! ```
//!
//! Two matching engines, selected by `token`:
//!
//! * `token=0` (default) — [`PhoneticGrep`] with the
//!   [`Algorithm::Transposition`] automaton: a per-line fuzzy regex grep. The
//!   `case_insensitive` flag (default on) is honored here. `start`/`end` are the
//!   1-indexed column span of the match within the line (liblevenshtein's own
//!   convention: `start = start_byte + 1`, `end = end_byte`).
//! * `token=1` — [`TokenGrep`] with its rich token-query language
//!   (`error:0 .* failed:1`, alternation, phrases). `scan` matches over the whole
//!   window, so each match's document byte offsets are mapped back to a 1-indexed
//!   line number + line text and per-token, line-relative columns so both engines
//!   share one response shape.
//!
//! `distance` (fuzzy edit distance) defaults to `2` and is capped at
//! [`MAX_GREP_DISTANCE`]; `limit` (max returned matches) defaults to
//! [`DEFAULT_GREP_LIMIT`], capped at [`MAX_GREP_LIMIT`]. `truncated` is `true`
//! when the window dropped the head of the file or the result cap was hit. An
//! un-compilable `q` is a *client* error, so it is surfaced in an extra
//! `"error"` field (never swallowed, therefore not logged).
//!
//! # Error posture (ADR-021)
//!
//! A missing log file — or an unconfigured `[logging] file` — is a *by-design*
//! empty result, not a failure: the endpoint returns the empty envelope and logs
//! nothing. A genuine swallowed IO failure (permission denied, mid-read error, a
//! panicked blocking task) logs at `tracing::error!` per ADR-021 and still
//! returns the empty envelope so the pane degrades gracefully.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use liblevenshtein::phonetic::grep::PhoneticGrep;
use liblevenshtein::phonetic::token_grep::TokenGrep;
use liblevenshtein::transducer::Algorithm;

use super::ApiState;

/// Hard cap on how many bytes are ever read from the tail of the log file
/// (2 MiB). Bounds both memory and CPU for `tail` and `grep` irrespective of the
/// on-disk file size.
const MAX_TAIL_BYTES: u64 = 2 * 1024 * 1024;

/// Default number of tail lines when the caller omits `lines`.
const DEFAULT_TAIL_LINES: usize = 200;
/// Upper bound on the `lines` parameter (defends the response size).
const MAX_TAIL_LINES: usize = 1000;

/// Default number of grep matches when the caller omits `limit`.
const DEFAULT_GREP_LIMIT: usize = 200;
/// Upper bound on the grep `limit` parameter.
const MAX_GREP_LIMIT: usize = 1000;

/// Upper bound on the fuzzy edit distance (`liblevenshtein` `max_distance`).
const MAX_GREP_DISTANCE: u8 = 3;

// ============================================================================
// Query parameters
// ============================================================================

/// Query parameters for `GET /api/logs/tail`.
#[derive(Debug, Deserialize)]
pub struct TailParams {
    /// Number of trailing lines to return (default [`DEFAULT_TAIL_LINES`],
    /// clamped to `1..=`[`MAX_TAIL_LINES`]).
    #[serde(default)]
    pub lines: Option<usize>,
    /// Optional log-level filter (`"error"`, `"warn"`, `"info"`, …), matched
    /// case-insensitively.
    #[serde(default)]
    pub level: Option<String>,
    /// Optional lower time bound (inclusive), RFC3339. Applies only in JSON log
    /// format (each line carries a timestamp); a line without a timestamp is
    /// excluded when a bound is set.
    #[serde(default)]
    pub since: Option<String>,
    /// Optional upper time bound (inclusive), RFC3339.
    #[serde(default)]
    pub until: Option<String>,
}

/// Query parameters for `GET /api/logs/grep`.
#[derive(Debug, Deserialize)]
pub struct GrepParams {
    /// The fuzzy pattern (a regex for `token=0`, a token-query for `token=1`).
    #[serde(default)]
    pub q: Option<String>,
    /// Maximum fuzzy edit distance (default `2`, capped at
    /// [`MAX_GREP_DISTANCE`]).
    #[serde(default)]
    pub distance: Option<u8>,
    /// `1` selects the [`TokenGrep`] engine; `0` (default) selects
    /// [`PhoneticGrep`].
    #[serde(default)]
    pub token: Option<u8>,
    /// `1` (default) matches case-insensitively; only honored by the
    /// [`PhoneticGrep`] engine.
    #[serde(default)]
    pub case_insensitive: Option<u8>,
    /// Maximum number of matches to return (default [`DEFAULT_GREP_LIMIT`],
    /// clamped to `1..=`[`MAX_GREP_LIMIT`]).
    #[serde(default)]
    pub limit: Option<usize>,
}

// ============================================================================
// Handlers
// ============================================================================

/// `GET /api/logs/tail` — see the module docs for the response shape.
pub async fn tail(State(state): State<ApiState>, Query(params): Query<TailParams>) -> Json<Value> {
    let cfg = state.config.load();
    let configured = cfg.logging.file.trim().to_string();
    let is_json = cfg.logging.format.eq_ignore_ascii_case("json");
    drop(cfg);

    // Unconfigured log file: by-design empty result, no log.
    if configured.is_empty() {
        return Json(json!({ "path": "", "lines": [], "truncated": false }));
    }

    let want = params
        .lines
        .unwrap_or(DEFAULT_TAIL_LINES)
        .clamp(1, MAX_TAIL_LINES);
    let level_filter = params.level.and_then(|s| {
        let trimmed = s.trim().to_string();
        (!trimmed.is_empty()).then_some(trimmed)
    });
    let since = parse_rfc3339_param(params.since.as_deref());
    let until = parse_rfc3339_param(params.until.as_deref());

    let result = tokio::task::spawn_blocking(move || {
        tail_blocking(
            &configured,
            is_json,
            want,
            level_filter.as_deref(),
            since,
            until,
        )
    })
    .await;

    match result {
        Ok(value) => Json(value),
        Err(e) => {
            // A panic inside the blocking task is a genuine swallowed failure.
            tracing::error!(error = %e, "logs tail: blocking read task panicked");
            Json(json!({ "path": "", "lines": [], "truncated": false }))
        }
    }
}

/// `GET /api/logs/grep` — see the module docs for the response shape.
pub async fn grep(State(state): State<ApiState>, Query(params): Query<GrepParams>) -> Json<Value> {
    let pattern = params.q.map(|s| s.trim().to_string()).unwrap_or_default();
    // No pattern → nothing to search; empty result, no log.
    if pattern.is_empty() {
        return Json(json!({ "matches": [], "truncated": false }));
    }

    let distance = params.distance.unwrap_or(2).min(MAX_GREP_DISTANCE);
    let token = params.token.unwrap_or(0) != 0;
    let case_insensitive = params.case_insensitive.unwrap_or(1) != 0;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_GREP_LIMIT)
        .clamp(1, MAX_GREP_LIMIT);

    let cfg = state.config.load();
    let configured = cfg.logging.file.trim().to_string();
    drop(cfg);
    if configured.is_empty() {
        return Json(json!({ "matches": [], "truncated": false }));
    }

    let result = tokio::task::spawn_blocking(move || {
        grep_blocking(
            configured,
            pattern,
            distance,
            token,
            case_insensitive,
            limit,
        )
    })
    .await;

    match result {
        Ok(value) => Json(value),
        Err(e) => {
            tracing::error!(error = %e, "logs grep: blocking read task panicked");
            Json(json!({ "matches": [], "truncated": false }))
        }
    }
}

// ============================================================================
// Blocking bodies (run on a spawn_blocking thread)
// ============================================================================

/// Synchronous body of [`tail`]. Reads the bounded tail window and builds the
/// `{ path, lines, truncated }` envelope.
fn tail_blocking(
    configured: &str,
    is_json: bool,
    want: usize,
    level_filter: Option<&str>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Value {
    let path = expand_tilde(configured);
    let (content, window_truncated) = match read_tail_window(&path) {
        Ok(window) => window,
        // A missing file is a by-design empty result (no log).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return json!({ "path": "", "lines": [], "truncated": false });
        }
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "logs tail: failed to read daemon log file"
            );
            return json!({ "path": "", "lines": [], "truncated": false });
        }
    };

    let window_lines: Vec<&str> = content.lines().collect();
    let mut truncated = window_truncated;

    let has_filter = level_filter.is_some() || since.is_some() || until.is_some();
    let lines_out: Vec<Value> = if has_filter {
        // Filter across the whole window (level AND time), keep the last `want`.
        let mut matched: Vec<Value> = Vec::new();
        for &raw in &window_lines {
            let entry = parse_log_line(raw, is_json);
            let level_ok = level_filter.is_none_or(|level| log_line_has_level(&entry, raw, level));
            if level_ok && log_line_in_time_range(&entry, since, until) {
                matched.push(entry.into_value());
            }
        }
        let start = matched.len().saturating_sub(want);
        truncated = truncated || start > 0;
        matched.split_off(start)
    } else {
        // No filter: only the last `want` lines need parsing.
        let start = window_lines.len().saturating_sub(want);
        truncated = truncated || start > 0;
        window_lines[start..]
            .iter()
            .map(|&raw| parse_log_line(raw, is_json).into_value())
            .collect()
    };

    json!({
        "path": path.to_string_lossy(),
        "lines": lines_out,
        "truncated": truncated,
    })
}

/// Synchronous body of [`grep`]. Reads the bounded tail window and dispatches to
/// the selected fuzzy engine.
fn grep_blocking(
    configured: String,
    pattern: String,
    distance: u8,
    token: bool,
    case_insensitive: bool,
    limit: usize,
) -> Value {
    let path = expand_tilde(&configured);
    let (content, window_truncated) = match read_tail_window(&path) {
        Ok(window) => window,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return json!({ "matches": [], "truncated": false });
        }
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "logs grep: failed to read daemon log file"
            );
            return json!({ "matches": [], "truncated": false });
        }
    };

    let outcome = if token {
        grep_token(&pattern, distance, limit, &content)
    } else {
        grep_phonetic(&pattern, distance, case_insensitive, limit, &content)
    };

    match outcome {
        Ok((matches, capped)) => json!({
            "matches": matches,
            "truncated": window_truncated || capped,
        }),
        // An un-compilable pattern is client input; surface it rather than
        // swallow it (so, per ADR-021, it needs no error log).
        Err(msg) => json!({ "matches": [], "truncated": false, "error": msg }),
    }
}

// ============================================================================
// Fuzzy engines
// ============================================================================

/// Per-line fuzzy grep via [`PhoneticGrep`] (Transposition automaton). Returns
/// the mapped match objects plus whether the `limit` cap was hit. An
/// un-compilable `pattern` returns `Err(message)` for the caller to surface.
fn grep_phonetic(
    pattern: &str,
    distance: u8,
    case_insensitive: bool,
    limit: usize,
    content: &str,
) -> Result<(Vec<Value>, bool), String> {
    let grep =
        PhoneticGrep::from_pattern_with_algorithm(pattern, distance, Algorithm::Transposition)
            .map_err(|e| e.to_string())?
            .case_insensitive(case_insensitive);

    let mut out: Vec<Value> = Vec::with_capacity(limit.min(64));
    let mut capped = false;
    for line_match in grep.grep_file(content) {
        if out.len() >= limit {
            capped = true;
            break;
        }
        let matched: Vec<Value> = line_match
            .matches
            .into_iter()
            .map(|m| {
                json!({
                    "text": m.matched_text,
                    "start": m.start_column,
                    "end": m.end_column,
                    "distance": m.distance,
                })
            })
            .collect();
        out.push(json!({
            "line_number": line_match.line_number,
            "line": line_match.line,
            "matched": matched,
        }));
    }
    Ok((out, capped))
}

/// Token-query fuzzy grep via [`TokenGrep`]. `scan` matches over the whole
/// window, so each match's document byte offsets are mapped back to a 1-indexed
/// line number + line text (mirroring [`PhoneticGrep`]'s per-line, 1-indexed
/// column convention) so both engines share a response shape.
fn grep_token(
    pattern: &str,
    distance: u8,
    limit: usize,
    content: &str,
) -> Result<(Vec<Value>, bool), String> {
    let grep = TokenGrep::new(pattern, distance).map_err(|e| e.to_string())?;
    let all = grep.scan(content);
    let capped = all.len() > limit;
    let table = LineTable::build(content);

    let mut out: Vec<Value> = Vec::with_capacity(all.len().min(limit));
    for token_match in all.into_iter().take(limit) {
        let line_idx = table.line_of(token_match.byte_range.0);
        let line_start = table.starts[line_idx];
        let matched: Vec<Value> = token_match
            .token_matches
            .into_iter()
            .map(|d| {
                // Convert document-relative byte offsets to 1-indexed columns
                // within the match's start line (PhoneticGrep's convention).
                json!({
                    "text": d.original_text,
                    "start": d.byte_range.0.saturating_sub(line_start) + 1,
                    "end": d.byte_range.1.saturating_sub(line_start),
                    "distance": d.distance,
                })
            })
            .collect();
        out.push(json!({
            "line_number": line_idx + 1,
            "line": table.lines[line_idx],
            "matched": matched,
        }));
    }
    Ok((out, capped))
}

// ============================================================================
// Bounded tail read
// ============================================================================

/// Read up to [`MAX_TAIL_BYTES`] from the **end** of `path`, returning the
/// decoded (lossy-UTF-8) window plus whether the head of the file was dropped
/// (the file was larger than the window). When the head is dropped the first
/// (partial) line is trimmed so callers only ever observe whole lines.
///
/// Never reads the whole file: seeks to `max(0, len − cap)` and reads a
/// `take`-guarded window, so a concurrent append cannot push the read past the
/// cap.
fn read_tail_window(path: &Path) -> std::io::Result<(String, bool)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let (start, truncated) = if len > MAX_TAIL_BYTES {
        (len - MAX_TAIL_BYTES, true)
    } else {
        (0, false)
    };
    file.seek(SeekFrom::Start(start))?;

    let cap = (len - start).min(MAX_TAIL_BYTES) as usize;
    let mut buf = Vec::with_capacity(cap);
    (&mut file).take(MAX_TAIL_BYTES).read_to_end(&mut buf)?;

    let mut content = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        // The window starts mid-line; drop up to and including the first newline
        // so only whole lines remain.
        match content.find('\n') {
            Some(nl) => {
                content.drain(..=nl);
            }
            // A single line longer than the window: nothing whole to return.
            None => content.clear(),
        }
    }
    Ok((content, truncated))
}

// ============================================================================
// Parsing helpers
// ============================================================================

/// A log line reduced to the four fields the webui pane renders.
struct ParsedLogLine {
    text: String,
    level: Option<String>,
    ts: Option<String>,
    target: Option<String>,
}

impl ParsedLogLine {
    fn into_value(self) -> Value {
        json!({
            "text": self.text,
            "level": self.level,
            "ts": self.ts,
            "target": self.target,
        })
    }
}

/// Parse one raw log line. In JSON mode, lift `level` / `timestamp` / `target` /
/// `fields.message`. On a parse failure (a non-JSON line interleaved with the
/// JSON stream — expected, by design) or in any non-JSON format, fall back to
/// the raw line as `text` with null metadata. This fallback is deliberate and
/// benign, so it is not logged.
fn parse_log_line(raw: &str, is_json: bool) -> ParsedLogLine {
    if is_json && let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw) {
        let level = map.get("level").and_then(Value::as_str).map(str::to_owned);
        let ts = map
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let target = map.get("target").and_then(Value::as_str).map(str::to_owned);
        let text = map
            .get("fields")
            .and_then(|fields| fields.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| raw.to_owned());
        return ParsedLogLine {
            text,
            level,
            ts,
            target,
        };
    }
    ParsedLogLine {
        text: raw.to_owned(),
        level: None,
        ts: None,
        target: None,
    }
}

/// Whether a parsed line matches the requested level filter. The structured
/// `level` (JSON mode) is compared case-insensitively; for a line without a
/// structured level (compact format, or a raw fallback) this is a best-effort
/// case-insensitive substring probe of the raw line.
fn log_line_has_level(entry: &ParsedLogLine, raw: &str, want: &str) -> bool {
    if let Some(level) = entry.level.as_deref() {
        return level.eq_ignore_ascii_case(want);
    }
    let want_upper = want.to_ascii_uppercase();
    raw.to_ascii_uppercase().contains(want_upper.as_str())
}

/// Parse an optional RFC3339 query param into a UTC instant; blank/invalid → None.
fn parse_rfc3339_param(raw: Option<&str>) -> Option<DateTime<Utc>> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// Whether a parsed line falls within the inclusive `[since, until]` window.
/// Only JSON-format lines carry a timestamp; when `ts` is absent and a bound was
/// requested, the line is excluded (it cannot be placed in the window).
fn log_line_in_time_range(
    entry: &ParsedLogLine,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> bool {
    if since.is_none() && until.is_none() {
        return true;
    }
    match entry
        .ts
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
    {
        Some(ts) => {
            let ts = ts.with_timezone(&Utc);
            since.is_none_or(|s| ts >= s) && until.is_none_or(|u| ts <= u)
        }
        None => false,
    }
}

/// Expand a leading `~/` to `$HOME` (mirrors `crate::logging`'s private helper
/// so this surface reads exactly the file the daemon writes). Any other path is
/// returned unchanged.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Byte-offset → line lookup over the scanned window, built once per token grep.
struct LineTable<'a> {
    /// Ascending start byte offset of each (0-indexed) line.
    starts: Vec<usize>,
    /// Display text of each line (trailing `\n` / `\r` trimmed).
    lines: Vec<&'a str>,
}

impl<'a> LineTable<'a> {
    fn build(content: &'a str) -> Self {
        // Preallocate: one slot per newline, plus the trailing segment.
        let line_count = content.bytes().filter(|&b| b == b'\n').count() + 1;
        let mut starts = Vec::with_capacity(line_count);
        let mut lines = Vec::with_capacity(line_count);

        let mut offset = 0usize;
        for segment in content.split_inclusive('\n') {
            starts.push(offset);
            let text = segment.strip_suffix('\n').unwrap_or(segment);
            let text = text.strip_suffix('\r').unwrap_or(text);
            lines.push(text);
            offset += segment.len();
        }
        // `split_inclusive` yields nothing for the empty string; guarantee at
        // least one line so `line_of` can never index an empty table.
        if starts.is_empty() {
            starts.push(0);
            lines.push("");
        }
        Self { starts, lines }
    }

    /// The 0-indexed line containing byte offset `byte` (clamped into range).
    fn line_of(&self, byte: usize) -> usize {
        match self.starts.partition_point(|&s| s <= byte) {
            0 => 0,
            n => n - 1,
        }
    }
}

// ============================================================================
// Tests (pure helpers only — no filesystem, DB, or ApiState needed)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_passthrough_and_home() {
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(expand_tilde("relative/x"), PathBuf::from("relative/x"));
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                expand_tilde("~/logs/pgmcp.log"),
                home.join("logs/pgmcp.log")
            );
        }
    }

    #[test]
    fn line_table_maps_offsets() {
        let table = LineTable::build("a\nbb\nccc");
        assert_eq!(table.starts, vec![0, 2, 5]);
        assert_eq!(table.lines, vec!["a", "bb", "ccc"]);
        assert_eq!(table.line_of(0), 0); // 'a'
        assert_eq!(table.line_of(1), 0); // '\n' after "a"
        assert_eq!(table.line_of(2), 1); // 'b'
        assert_eq!(table.line_of(5), 2); // 'c'
        assert_eq!(table.line_of(7), 2); // last 'c'
        assert_eq!(table.line_of(999), 2); // clamps past EOF
    }

    #[test]
    fn line_table_handles_crlf_and_empty() {
        let table = LineTable::build("x\r\ny\r\n");
        assert_eq!(table.lines, vec!["x", "y"]);
        let empty = LineTable::build("");
        assert_eq!(empty.starts, vec![0]);
        assert_eq!(empty.lines, vec![""]);
        assert_eq!(empty.line_of(0), 0);
    }

    #[test]
    fn parse_log_line_json_lifts_structured_fields() {
        let raw = r#"{"timestamp":"2026-07-05T00:00:00.9Z","level":"INFO","fields":{"message":"invoked","tool":"x"},"target":"pgmcp::mcp::tool"}"#;
        let parsed = parse_log_line(raw, true);
        assert_eq!(parsed.text, "invoked");
        assert_eq!(parsed.level.as_deref(), Some("INFO"));
        assert_eq!(parsed.ts.as_deref(), Some("2026-07-05T00:00:00.9Z"));
        assert_eq!(parsed.target.as_deref(), Some("pgmcp::mcp::tool"));
    }

    #[test]
    fn parse_log_line_plain_is_verbatim() {
        let parsed = parse_log_line("2026-07-05  INFO pgmcp: hello", false);
        assert_eq!(parsed.text, "2026-07-05  INFO pgmcp: hello");
        assert!(parsed.level.is_none());
        assert!(parsed.ts.is_none());
        assert!(parsed.target.is_none());
    }

    #[test]
    fn parse_log_line_json_invalid_falls_back_to_raw() {
        // Requested JSON mode, but the line is a raw (non-JSON) panic trace.
        let parsed = parse_log_line("thread 'main' panicked at foo.rs:1", true);
        assert_eq!(parsed.text, "thread 'main' panicked at foo.rs:1");
        assert!(parsed.level.is_none());
    }

    #[test]
    fn log_line_has_level_structured_and_substring() {
        let json_line = parse_log_line(r#"{"level":"ERROR","fields":{"message":"boom"}}"#, true);
        assert!(log_line_has_level(&json_line, "", "error"));
        assert!(log_line_has_level(&json_line, "", "ERROR"));
        assert!(!log_line_has_level(&json_line, "", "info"));

        // Compact/raw line: best-effort substring probe.
        let plain = parse_log_line("2026  WARN pgmcp: careful", false);
        assert!(log_line_has_level(
            &plain,
            "2026  WARN pgmcp: careful",
            "warn"
        ));
        assert!(!log_line_has_level(
            &plain,
            "2026  WARN pgmcp: careful",
            "trace"
        ));
    }

    #[test]
    fn grep_phonetic_finds_fuzzy_match_on_correct_line() {
        let content = "alpha hello world\nbeta goodbye\n";
        let (matches, capped) =
            grep_phonetic("helo", 2, true, 200, content).expect("valid pattern compiles");
        assert!(!capped);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["line_number"].as_u64(), Some(1));
        let matched = matches[0]["matched"].as_array().expect("matched array");
        assert!(!matched.is_empty());
        assert!(matched[0]["text"].as_str().is_some());
        assert!(matched[0]["start"].as_u64().unwrap() >= 1); // 1-indexed
    }

    #[test]
    fn grep_phonetic_caps_at_limit() {
        let content = "hello\nhello\nhello\nhello\nhello\n";
        let (matches, capped) =
            grep_phonetic("hello", 0, false, 2, content).expect("valid pattern");
        assert_eq!(matches.len(), 2);
        assert!(capped);
    }

    #[test]
    fn grep_phonetic_empty_content_is_empty() {
        let (matches, capped) = grep_phonetic("hello", 1, true, 10, "").expect("valid pattern");
        assert!(matches.is_empty());
        assert!(!capped);
    }

    #[test]
    fn grep_token_matches_adjacent_tokens() {
        let content = "start alpha beta end\nother line\n";
        let (matches, _capped) =
            grep_token("alpha beta", 1, 200, content).expect("valid token query");
        assert!(!matches.is_empty());
        assert_eq!(matches[0]["line_number"].as_u64(), Some(1));
        let matched = matches[0]["matched"].as_array().expect("matched array");
        assert!(!matched.is_empty());
        let joined: String = matched
            .iter()
            .filter_map(|m| m["text"].as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("alpha"), "matched texts: {joined:?}");
    }
}
