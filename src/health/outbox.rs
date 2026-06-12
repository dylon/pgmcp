//! Durable ephemeral-event outbox (store-and-forward).
//!
//! Captures the *fire-and-forget external writes* that have **no other durable
//! source** when the database is unreachable, and replays them once it
//! recovers. Scope is deliberately narrow: the session-observe and
//! client-file-event ingress (the Claude Code hooks POST these once and move
//! on). File-indexing is **not** spooled — the files on disk plus the
//! `rescan_workspace` reconciliation are a strictly better durable log.
//!
//! ## Why a *deferred local POST* and not a row spool
//!
//! Both writer endpoints resolve a project (longest-prefix cwd) and an indexed
//! file, and embed the prompt, *at request time* — none of which can be done
//! while the DB is down. Rather than extract and duplicate those hot pipelines
//! (and risk drift), an [`OutboxRecord`] stores the **raw request body + its
//! endpoint path**, and [`OutboxReplayer`] re-POSTs it to the *same* loopback
//! handler after recovery. Idempotency is inherited from the handlers
//! (`session_prompts` is sha256-deduped; `client_file_events` is consumed via
//! `DISTINCT ON (abs_path) … ORDER BY ts DESC`).
//!
//! ## ENOSPC self-defeat guards
//!
//! The outage that motivated this was *disk-full*, so an outbox on the same
//! full filesystem would fail identically — and an unbounded outbox during a
//! long outage would itself *become* the next ENOSPC. [`Outbox::append`]
//! therefore (a) refuses to write when its own filesystem is below
//! `self_floor` (bytes **or** inodes) and (b) caps total spool size at
//! `max_bytes` (drop-new or drop-oldest). Both are counted in `dropped`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::health::db_health::DbHealth;
use crate::health::fs::fs_avail;

/// When the spool reaches `max_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnFull {
    /// Drop the newest record (refuse the append).
    Stop,
    /// Trim the oldest records to make room, then append.
    DropOldest,
}

impl OnFull {
    pub fn parse(s: &str) -> OnFull {
        match s {
            "drop_oldest" => OnFull::DropOldest,
            _ => OnFull::Stop,
        }
    }
}

/// One spooled deferred POST. JSONL-serialized, one per line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutboxRecord {
    /// The local endpoint path to re-POST to, e.g. `/api/session/observe`.
    pub path: String,
    /// The raw request body (the typed request re-serialized to JSON).
    pub body: serde_json::Value,
    /// Epoch seconds the record was spooled (observability / ordering).
    pub ts: i64,
}

/// Append-and-replay spool. Cheap to clone via `Arc`; all mutation is through
/// the filesystem + atomics (no interior lock).
#[derive(Debug)]
pub struct Outbox {
    dir: PathBuf,
    max_bytes: u64,
    self_floor_bytes: u64,
    self_floor_inodes: u64,
    on_full: OnFull,
    /// Monotonic suffix for replay-segment uniqueness (avoids `SystemTime`
    /// collisions when many records rotate in the same nanosecond).
    seq: AtomicU64,
    /// Records refused by a self-floor or cap guard, or a write error.
    dropped: AtomicU64,
}

const ACTIVE_FILE: &str = "outbox.jsonl";
const REPLAY_PREFIX: &str = "outbox.replaying.";

impl Outbox {
    /// Create the outbox, ensuring `dir` exists. Returns `None` (disabled) if
    /// the directory cannot be created — the daemon logs and runs without it
    /// rather than failing startup.
    pub fn new(
        dir: PathBuf,
        max_bytes: u64,
        self_floor_bytes: u64,
        self_floor_inodes: u64,
        on_full: OnFull,
    ) -> Option<Self> {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(error = %e, dir = %dir.display(), "outbox: could not create spool dir; outbox disabled");
            return None;
        }
        Some(Self {
            dir,
            max_bytes,
            self_floor_bytes,
            self_floor_inodes,
            on_full,
            seq: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        })
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn active_path(&self) -> PathBuf {
        self.dir.join(ACTIVE_FILE)
    }

    /// Spool one deferred POST. Best-effort: any guard or IO failure increments
    /// `dropped` and returns without erroring (the caller's request still
    /// succeeds from the hook's perspective).
    pub fn append(&self, path: &str, body: serde_json::Value) {
        let rec = OutboxRecord {
            path: path.to_string(),
            body,
            ts: now_epoch_secs(),
        };
        let mut line = match serde_json::to_string(&rec) {
            Ok(s) => s,
            Err(e) => {
                debug!(error = %e, "outbox: record serialize failed; dropping");
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        line.push('\n');

        // Guard 1 — never become the next ENOSPC: refuse if our own filesystem
        // is below the self-floor on bytes OR inodes.
        if let Some(avail) = fs_avail(&self.dir)
            && (avail.avail_bytes < self.self_floor_bytes
                || avail.avail_inodes < self.self_floor_inodes)
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            debug!(
                avail_bytes = avail.avail_bytes,
                avail_inodes = avail.avail_inodes,
                "outbox: filesystem at/below self-floor; dropping spool record"
            );
            return;
        }

        // Guard 2 — cap total spool size.
        let active = self.active_path();
        let cur = std::fs::metadata(&active).map(|m| m.len()).unwrap_or(0);
        if cur.saturating_add(line.len() as u64) > self.max_bytes {
            match self.on_full {
                OnFull::Stop => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    debug!(
                        max_bytes = self.max_bytes,
                        "outbox: at cap (stop); dropping record"
                    );
                    return;
                }
                OnFull::DropOldest => self.trim_oldest(&active),
            }
        }

        if let Err(e) = append_line(&active, &line) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            debug!(error = %e, "outbox: append failed; dropping record");
        }
    }

    /// Drop roughly the oldest quarter of the active file's lines to make room.
    fn trim_oldest(&self, active: &Path) {
        let Ok(contents) = std::fs::read_to_string(active) else {
            return;
        };
        let lines: Vec<&str> = contents.lines().collect();
        if lines.is_empty() {
            return;
        }
        let drop = (lines.len() / 4).max(1);
        self.dropped.fetch_add(drop as u64, Ordering::Relaxed);
        let kept = lines[drop..].join("\n");
        let kept = if kept.is_empty() {
            String::new()
        } else {
            format!("{kept}\n")
        };
        if let Err(e) = std::fs::write(active, kept) {
            warn!(error = %e, "outbox: trim rewrite failed");
        }
    }

    /// Rotate the active file (if any non-empty) into a uniquely-named replay
    /// segment, and return all replay segments oldest-first (including any left
    /// by a prior partial replay).
    fn rotate_and_list_segments(&self) -> Vec<PathBuf> {
        let active = self.active_path();
        if let Ok(md) = std::fs::metadata(&active)
            && md.len() > 0
        {
            let n = self.seq.fetch_add(1, Ordering::Relaxed);
            let stamp = now_epoch_nanos();
            let seg = self
                .dir
                .join(format!("{REPLAY_PREFIX}{stamp:020}.{n:06}.jsonl"));
            if let Err(e) = std::fs::rename(&active, &seg) {
                warn!(error = %e, "outbox: rotate failed; replay deferred");
            }
        }
        let mut segs: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with(REPLAY_PREFIX) && n.ends_with(".jsonl"))
                    .unwrap_or(false)
                {
                    segs.push(p);
                }
            }
        }
        segs.sort(); // names are zero-padded stamp+seq → chronological
        segs
    }

    /// Parse a replay segment into records (skipping malformed lines).
    fn read_segment(seg: &Path) -> Vec<OutboxRecord> {
        let Ok(contents) = std::fs::read_to_string(seg) else {
            return Vec::new();
        };
        contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| match serde_json::from_str::<OutboxRecord>(l) {
                Ok(r) => Some(r),
                Err(e) => {
                    debug!(error = %e, "outbox: skipping malformed replay line");
                    None
                }
            })
            .collect()
    }

    /// Rewrite a segment with the unprocessed remainder, or delete it when the
    /// remainder is empty (segment fully replayed).
    fn finish_segment(seg: &Path, remainder: &[OutboxRecord]) {
        if remainder.is_empty() {
            let _ = std::fs::remove_file(seg);
            return;
        }
        let body: String = remainder
            .iter()
            .filter_map(|r| serde_json::to_string(r).ok())
            .collect::<Vec<_>>()
            .join("\n");
        let body = format!("{body}\n");
        if let Err(e) = std::fs::write(seg, body) {
            warn!(error = %e, "outbox: remainder rewrite failed");
        }
    }
}

/// Drives replay of an [`Outbox`] by re-POSTing each spooled record to the
/// loopback REST handler it came from. Constructed once in the daemon; the
/// prober fires `replay()` on the DB Down→Up edge.
pub struct OutboxReplayer {
    outbox: std::sync::Arc<Outbox>,
    base_url: String,
    client: reqwest::Client,
    db_health: std::sync::Arc<DbHealth>,
}

/// Outcome of one replay sweep.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReplayStats {
    pub replayed: u64,
    pub failed: u64,
    pub segments: u64,
}

impl OutboxReplayer {
    pub fn new(
        outbox: std::sync::Arc<Outbox>,
        host: &str,
        port: u16,
        db_health: std::sync::Arc<DbHealth>,
    ) -> Self {
        // Replay always targets loopback: the hooks POST to localhost and the
        // endpoints' threat model assumes a same-host bind. A `0.0.0.0` bind
        // still serves on 127.0.0.1.
        let host = if host == "0.0.0.0" || host.is_empty() {
            "127.0.0.1"
        } else {
            host
        };
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            outbox,
            base_url: format!("http://{host}:{port}"),
            client,
            db_health,
        }
    }

    /// Replay all spooled records. Re-POSTs each through its original handler;
    /// stops a segment at the first failure (keeping the remainder for the next
    /// recovery) and aborts entirely if the DB drops again mid-replay.
    pub async fn replay(&self) -> ReplayStats {
        let mut stats = ReplayStats::default();
        let segments = self.outbox.rotate_and_list_segments();
        for seg in segments {
            stats.segments += 1;
            let records = Outbox::read_segment(&seg);
            let mut idx = 0usize;
            let mut aborted = false;
            for (i, rec) in records.iter().enumerate() {
                // The DB recovered to trigger this replay; if it drops again,
                // stop and keep the remainder rather than hammering it.
                if !self.db_health.is_up() {
                    aborted = true;
                    idx = i;
                    break;
                }
                if self.post_one(rec).await {
                    stats.replayed += 1;
                    idx = i + 1;
                } else {
                    stats.failed += 1;
                    idx = i;
                    aborted = true;
                    break;
                }
            }
            let remainder = if aborted { &records[idx..] } else { &[][..] };
            Outbox::finish_segment(&seg, remainder);
            if aborted {
                break; // process older-or-equal segments next time, in order
            }
        }
        if stats.replayed > 0 || stats.failed > 0 {
            info!(
                replayed = stats.replayed,
                failed = stats.failed,
                segments = stats.segments,
                dropped_total = self.outbox.dropped(),
                "outbox: replay sweep complete"
            );
        }
        stats
    }

    /// POST one record; `true` on a 2xx response.
    async fn post_one(&self, rec: &OutboxRecord) -> bool {
        let url = format!("{}{}", self.base_url, rec.path);
        match self.client.post(&url).json(&rec.body).send().await {
            Ok(resp) if resp.status().is_success() => true,
            Ok(resp) => {
                debug!(status = %resp.status(), path = %rec.path, "outbox: replay POST non-2xx");
                false
            }
            Err(e) => {
                debug!(error = %e, path = %rec.path, "outbox: replay POST failed");
                false
            }
        }
    }
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())?;
    f.flush()
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_epoch_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!(
                "pgmcp-outbox-test-{}-{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&p).expect("mkdir");
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn body(prompt: &str) -> serde_json::Value {
        serde_json::json!({"session_id": "00000000-0000-0000-0000-000000000000", "cwd": "/x", "prompt": prompt})
    }

    #[test]
    fn append_then_segment_roundtrip() {
        let td = TempDir::new();
        let ob = Outbox::new(td.0.clone(), 1 << 20, 0, 0, OnFull::Stop).expect("outbox");
        ob.append("/api/session/observe", body("hello"));
        ob.append("/api/session/observe", body("world"));
        let segs = ob.rotate_and_list_segments();
        assert_eq!(segs.len(), 1, "one rotated segment");
        let recs = Outbox::read_segment(&segs[0]);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].path, "/api/session/observe");
        assert_eq!(recs[0].body["prompt"], "hello");
        assert_eq!(recs[1].body["prompt"], "world");
    }

    #[test]
    fn self_floor_drops_when_below() {
        let td = TempDir::new();
        // self_floor_bytes = u64::MAX ⇒ always below ⇒ always drop.
        let ob = Outbox::new(td.0.clone(), 1 << 20, u64::MAX, 0, OnFull::Stop).expect("outbox");
        ob.append("/api/session/observe", body("hello"));
        assert_eq!(ob.dropped(), 1);
        assert!(ob.rotate_and_list_segments().is_empty(), "nothing spooled");
    }

    #[test]
    fn cap_stop_drops_when_full() {
        let td = TempDir::new();
        // max_bytes tiny ⇒ second append exceeds and is dropped.
        let ob = Outbox::new(td.0.clone(), 80, 0, 0, OnFull::Stop).expect("outbox");
        ob.append("/api/session/observe", body("aaaaaaaaaaaaaaaaaaaa"));
        let dropped_before = ob.dropped();
        ob.append("/api/session/observe", body("bbbbbbbbbbbbbbbbbbbb"));
        assert!(ob.dropped() > dropped_before, "second append hit the cap");
    }

    #[test]
    fn finish_segment_deletes_when_remainder_empty() {
        let td = TempDir::new();
        let ob = Outbox::new(td.0.clone(), 1 << 20, 0, 0, OnFull::Stop).expect("outbox");
        ob.append("/p", body("x"));
        let segs = ob.rotate_and_list_segments();
        Outbox::finish_segment(&segs[0], &[]);
        assert!(!segs[0].exists(), "fully-replayed segment deleted");
    }

    #[test]
    fn finish_segment_keeps_remainder() {
        let td = TempDir::new();
        let ob = Outbox::new(td.0.clone(), 1 << 20, 0, 0, OnFull::Stop).expect("outbox");
        ob.append("/p", body("a"));
        ob.append("/p", body("b"));
        let segs = ob.rotate_and_list_segments();
        let recs = Outbox::read_segment(&segs[0]);
        Outbox::finish_segment(&segs[0], &recs[1..]);
        let after = Outbox::read_segment(&segs[0]);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].body["prompt"], "b");
    }
}
