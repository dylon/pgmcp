//! Runtime↔static deadlock reconciler (Dbg-1).
//!
//! Pure module (no DB, no I/O): given a runtime trace string captured by the
//! AGENT (an off-CPU folded stack from BCC `offcputime -f` / `offwaketime`, a
//! `perf script` dump, or a `gdb` `thread apply all bt`), extract the
//! blocked-on-lock wait relation and reconcile it against the static
//! lock-order graph ([`crate::graph::lock_order::LockEdge`]).
//!
//! ## What a runtime trace tells us
//!
//! When a thread is blocked acquiring a lock, its stack contains a kernel/libc
//! lock-wait leaf (`futex_wait`, `__lll_lock_wait`, `pthread_mutex_lock`,
//! `pthread_rwlock_*lock`, …) and, above it, the application frame that issued
//! the acquire. We model one observation as the pair
//! `(holder_context, wanted_lock)`:
//! - `holder_context` — the nearest application frame that was *holding* /
//!   *waiting from* (the function on whose behalf the lock is wanted). For a
//!   folded stack this is the frame directly under the lock-wait leaf.
//! - `wanted_lock` — the lock the thread is blocked on. Traces rarely name the
//!   mutex *variable*; we use the application frame that calls the lock
//!   primitive as the lock's identity proxy (the same `resource_key`-by-callsite
//!   convention the static `sync_ops` extractor uses when it can't resolve a
//!   field path). The reconciler therefore matches on *frame/symbol identity*.
//!
//! ## Reconciliation
//!
//! [`reconcile`] classifies into three buckets:
//! - `confirmed` — an observed wait-for `(a, b)` whose pair `(a, b)` (or its
//!   constituent resources) corresponds to a static `LockEdge a→b`. The static
//!   analysis predicted this ordering AND it was observed at runtime.
//! - `static_missed` — an observed wait-for with NO matching static edge. The
//!   runtime saw an ordering the static analysis missed (a precision gap —
//!   unresolved callee, dynamic dispatch, FFI). High-signal: the static graph
//!   is incomplete here.
//! - `static_only` — a static edge never witnessed at runtime in this trace.
//!   Either a true ordering this workload didn't exercise, or a static
//!   false-positive. Lower-signal, but worth surfacing for cycle triage.

use std::collections::HashSet;

use crate::graph::lock_order::LockEdge;

/// One observed runtime wait-for relation: `holder`/waiter context acquired (or
/// is blocked acquiring) `wanted`. Both are application-frame / symbol names
/// harvested from the trace.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObservedWait {
    /// Application frame holding / on whose behalf the wait happens.
    pub holder: String,
    /// The lock the thread is blocked acquiring (callsite-frame identity).
    pub wanted: String,
    /// The blocking primitive leaf that proved this was a lock wait
    /// (`futex_wait`, `pthread_mutex_lock`, …). Diagnostic only.
    pub primitive: String,
}

/// The reconciliation result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reconciliation {
    /// Observed waits corroborated by a static lock-order edge.
    pub confirmed: Vec<ConfirmedWait>,
    /// Observed waits with no static counterpart (static analysis missed them).
    pub static_missed: Vec<ObservedWait>,
    /// Static edges never observed at runtime in this trace.
    pub static_only: Vec<StaticOnlyEdge>,
}

/// An observed wait that matched a static edge.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfirmedWait {
    pub observed: ObservedWait,
    /// The matched static edge's resource pair (`from`, `to`).
    pub static_from: String,
    pub static_to: String,
    pub interprocedural: bool,
}

/// A static edge with no runtime witness.
#[derive(Clone, Debug, PartialEq)]
pub struct StaticOnlyEdge {
    pub from: String,
    pub to: String,
    pub interprocedural: bool,
}

/// Lock-wait primitive leaves: a stack frame matching one of these substrings
/// proves the thread above it is blocked acquiring a lock.
const LOCK_WAIT_PRIMITIVES: &[&str] = &[
    "futex_wait",
    "__lll_lock_wait",
    "__lll_lock_wait_private",
    "pthread_mutex_lock",
    "pthread_rwlock_rdlock",
    "pthread_rwlock_wrlock",
    "pthread_cond_wait",
    "pthread_cond_timedwait",
    "do_futex",
    "futex_wait_queue",
    "__futex_abstimed_wait",
    // Rust std parking_lot / std::sync lock waits often surface as these.
    "parking_lot",
    "RawMutex",
    "sys_futex",
];

/// True when a frame name indicates a lock-wait primitive.
fn is_lock_wait_primitive(frame: &str) -> bool {
    LOCK_WAIT_PRIMITIVES.iter().any(|p| frame.contains(p))
}

/// Strip an address / offset suffix and module decoration from a frame token,
/// returning the bare symbol. Handles:
///  - `func+0x1a` → `func`
///  - `func (/usr/lib/libc.so)` → `func`
///  - `0x00007f...` (pure address) → "" (unnamed)
fn clean_frame(frame: &str) -> String {
    let f = frame.trim();
    // Drop a trailing ` (module)` annotation (perf / folded sometimes append).
    let f = f.split(" (").next().unwrap_or(f);
    // Drop a `+0x..` offset.
    let f = f.split("+0x").next().unwrap_or(f);
    let f = f.trim();
    // A pure hex address is unnamed.
    if f.starts_with("0x") || (f.starts_with("0X")) {
        return String::new();
    }
    f.to_string()
}

/// Reduce a (possibly path-qualified / mangled) symbol to its final identifier
/// segment so trace frames match static `resource_key`s by base name.
fn bare_symbol(name: &str) -> String {
    let after_path = name.rsplit("::").next().unwrap_or(name);
    after_path
        .split(['<', '(', ' ', '@'])
        .next()
        .unwrap_or(after_path)
        .trim()
        .to_string()
}

/// Parse a BCC off-CPU folded stack (`offcputime -f` / `offwaketime`):
/// `thread;frameN;…;frame1;leaf count`. Each line whose stack contains a
/// lock-wait primitive yields one [`ObservedWait`]: `wanted` = the application
/// frame directly *below* (called by) the lock primitive, `holder` = the next
/// application frame below that.
pub fn parse_offcpu_folded(text: &str) -> Vec<ObservedWait> {
    let mut out: Vec<ObservedWait> = Vec::new();
    let mut seen: HashSet<ObservedWait> = HashSet::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // Drop the trailing count.
        let stack = match line.rsplit_once(char::is_whitespace) {
            Some((s, c)) if c.trim().parse::<u64>().is_ok() => s,
            _ => line, // no count — treat whole line as the stack
        };
        // Frames are leaf-last in folded format: `thread;deep;...;leaf`.
        let frames: Vec<String> = stack.split(';').map(clean_frame).collect();
        if let Some(wait) = wait_from_leaf_last(&frames)
            && seen.insert(wait.clone())
        {
            out.push(wait);
        }
    }
    out
}

/// Parse a `perf script` dump. perf-script groups a sample as a header line
/// followed by indented frames (leaf FIRST), blank-line separated. We treat
/// each sample-block's frames as one stack and apply the same lock-wait
/// extraction (leaf-first orientation).
pub fn parse_perf_script(text: &str) -> Vec<ObservedWait> {
    let mut out: Vec<ObservedWait> = Vec::new();
    let mut seen: HashSet<ObservedWait> = HashSet::new();
    let mut current: Vec<String> = Vec::new();

    let flush = |frames: &mut Vec<String>,
                 out: &mut Vec<ObservedWait>,
                 seen: &mut HashSet<ObservedWait>| {
        if frames.is_empty() {
            return;
        }
        // perf-script frames are leaf-FIRST.
        if let Some(wait) = wait_from_leaf_first(frames)
            && seen.insert(wait.clone())
        {
            out.push(wait);
        }
        frames.clear();
    };

    for raw in text.lines() {
        let line = raw;
        if line.trim().is_empty() {
            flush(&mut current, &mut out, &mut seen);
            continue;
        }
        // A non-indented line that isn't a frame is a sample header
        // (`comm pid [cpu] ts: ... event:`). Frame lines are indented and have
        // the shape `<addr> <symbol> (<dso>)`.
        let is_frame = line.starts_with(['\t', ' ']) && line.trim().starts_with("0x")
            || (line.starts_with(['\t', ' ']) && line.contains("0x"));
        if !is_frame {
            // New sample header — flush the previous block.
            flush(&mut current, &mut out, &mut seen);
            continue;
        }
        // Frame line: drop the leading address token, keep the symbol.
        let trimmed = line.trim();
        // shape: `<hexaddr> symbol (dso)` OR `<hexaddr> in symbol (dso)`
        // (perf prints `in` when it has the DSO+symbol) OR `symbol+0x.. (dso)`.
        let after_addr = trimmed
            .split_once(' ')
            .map(|(_, rest)| rest)
            .unwrap_or(trimmed)
            .trim();
        // Strip a leading `in ` keyword (perf-script with symbol resolution).
        let after_in = after_addr.strip_prefix("in ").unwrap_or(after_addr);
        let sym = clean_frame(after_in);
        if !sym.is_empty() {
            current.push(sym);
        }
    }
    flush(&mut current, &mut out, &mut seen);
    out
}

/// Parse a `gdb` backtrace (`thread apply all bt` or a single `bt`). Frames are
/// `#N  0xADDR in symbol (...) at file:line` (or without the address for the
/// innermost), leaf FIRST (`#0` is the leaf). Each `Thread`/backtrace block is
/// one stack.
pub fn parse_gdb_bt(text: &str) -> Vec<ObservedWait> {
    let mut out: Vec<ObservedWait> = Vec::new();
    let mut seen: HashSet<ObservedWait> = HashSet::new();
    let mut current: Vec<String> = Vec::new();

    let flush = |frames: &mut Vec<String>,
                 out: &mut Vec<ObservedWait>,
                 seen: &mut HashSet<ObservedWait>| {
        if frames.is_empty() {
            return;
        }
        if let Some(wait) = wait_from_leaf_first(frames)
            && seen.insert(wait.clone())
        {
            out.push(wait);
        }
        frames.clear();
    };

    for raw in text.lines() {
        let line = raw.trim();
        // A new thread header starts a fresh backtrace block.
        if line.starts_with("Thread ") || line.starts_with("[Switching to Thread") {
            flush(&mut current, &mut out, &mut seen);
            continue;
        }
        // Frame line: `#N  [0xADDR in] symbol (args) [at file:line]`.
        if let Some(rest) = line.strip_prefix('#') {
            // rest = `N  0xADDR in symbol (...)` or `N  symbol (...)`.
            let sym = gdb_frame_symbol(rest);
            if !sym.is_empty() {
                current.push(sym);
            }
        }
    }
    flush(&mut current, &mut out, &mut seen);
    out
}

/// Extract the symbol from a gdb frame body (the part after `#`).
/// `0  0x00007f.. in pthread_mutex_lock ()` → `pthread_mutex_lock`;
/// `2  my_func (x=1) at f.c:10` → `my_func`.
fn gdb_frame_symbol(rest: &str) -> String {
    // Skip the frame number.
    let after_num = rest
        .trim_start()
        .split_once(char::is_whitespace)
        .map(|(_, r)| r)
        .unwrap_or(rest)
        .trim();
    // If there's an ` in `, the symbol follows it; else the symbol is first.
    let candidate = if let Some(idx) = after_num.find(" in ") {
        &after_num[idx + 4..]
    } else {
        after_num
    };
    clean_frame(candidate)
}

/// Build an [`ObservedWait`] from a LEAF-LAST frame list (folded stacks).
/// The lock-wait primitive is near the end; the app frame just before it is the
/// `wanted` lock callsite, the frame before that is the `holder`.
fn wait_from_leaf_last(frames: &[String]) -> Option<ObservedWait> {
    let prim_pos = frames.iter().position(|f| is_lock_wait_primitive(f))?;
    let primitive = frames[prim_pos].clone();
    // Walk DOWN-stack (toward the root, i.e. lower indices) collecting the first
    // two non-empty, non-primitive application frames.
    let mut app: Vec<String> = Vec::new();
    for f in frames[..prim_pos].iter().rev() {
        if f.is_empty() || is_lock_wait_primitive(f) {
            continue;
        }
        app.push(f.clone());
        if app.len() == 2 {
            break;
        }
    }
    finish_wait(app, primitive)
}

/// Build an [`ObservedWait`] from a LEAF-FIRST frame list (perf script / gdb).
fn wait_from_leaf_first(frames: &[String]) -> Option<ObservedWait> {
    let prim_pos = frames.iter().position(|f| is_lock_wait_primitive(f))?;
    let primitive = frames[prim_pos].clone();
    // App frames are AFTER the primitive (higher indices = up-stack callers).
    let mut app: Vec<String> = Vec::new();
    for f in frames[prim_pos + 1..].iter() {
        if f.is_empty() || is_lock_wait_primitive(f) {
            continue;
        }
        app.push(f.clone());
        if app.len() == 2 {
            break;
        }
    }
    finish_wait(app, primitive)
}

/// Assemble the final [`ObservedWait`] from the collected application frames.
/// `app[0]` = innermost app frame (the `wanted` lock callsite),
/// `app[1]` = its caller (the `holder` context). When only one app frame is
/// present, `holder` falls back to that same frame.
fn finish_wait(app: Vec<String>, primitive: String) -> Option<ObservedWait> {
    let wanted = bare_symbol(app.first()?);
    if wanted.is_empty() {
        return None;
    }
    let holder = app
        .get(1)
        .map(|h| bare_symbol(h))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| wanted.clone());
    Some(ObservedWait {
        holder,
        wanted,
        primitive,
    })
}

/// Reconcile observed runtime waits against the static lock-order edges.
///
/// Matching is by *resource/frame identity* (bare symbol). An observed wait
/// `(holder, wanted)` matches a static edge `from→to` when the observed pair's
/// constituents correspond to the edge's resources — i.e. the static graph
/// ordered `holder`-side before `wanted`-side. Because traces and the static
/// `sync_ops` extractor both identify locks by callsite frame when the variable
/// is unnameable, we match on bare-symbol equality of `{holder,wanted}` against
/// `{from,to}`.
pub fn reconcile(
    observed_wait_for: &[(String, String)],
    static_edges: &[LockEdge],
) -> Reconciliation {
    // Normalize the observed pairs.
    let observed: Vec<ObservedWait> = observed_wait_for
        .iter()
        .filter(|(h, w)| !h.trim().is_empty() && !w.trim().is_empty())
        .map(|(h, w)| ObservedWait {
            holder: bare_symbol(h),
            wanted: bare_symbol(w),
            primitive: String::new(),
        })
        .collect();
    reconcile_observed(&observed, static_edges)
}

/// Reconcile pre-parsed [`ObservedWait`]s (preserves the `primitive` witness).
pub fn reconcile_observed(observed: &[ObservedWait], static_edges: &[LockEdge]) -> Reconciliation {
    // Index static edges by their bare-resource pair (from, to).
    let static_pairs: Vec<(String, String, bool)> = static_edges
        .iter()
        .map(|e| (bare_symbol(&e.from), bare_symbol(&e.to), e.interprocedural))
        .collect();

    let mut confirmed: Vec<ConfirmedWait> = Vec::new();
    let mut static_missed: Vec<ObservedWait> = Vec::new();
    // Track which static edges got witnessed.
    let mut witnessed: HashSet<usize> = HashSet::new();
    let mut seen_missed: HashSet<(String, String)> = HashSet::new();

    for obs in observed {
        let h = &obs.holder;
        let w = &obs.wanted;
        if h.is_empty() || w.is_empty() {
            continue;
        }
        // Find a static edge whose endpoints match the observed pair, in either
        // assignment (holder↔from/wanted↔to OR the resource pair matches either
        // way — a lock cycle may name them in either rotation).
        let mut matched: Option<usize> = None;
        for (i, (from, to, _interproc)) in static_pairs.iter().enumerate() {
            let direct = from == h && to == w;
            let rotated = from == w && to == h;
            // Also accept a match where the *wanted* lock equals the edge target
            // and the holder equals the edge source by either resource name (the
            // frame-identity proxy can collapse holder→callsite).
            let resource_match = (from == h || from == w) && (to == h || to == w) && from != to;
            if direct || rotated || resource_match {
                matched = Some(i);
                break;
            }
        }
        match matched {
            Some(i) => {
                witnessed.insert(i);
                let (from, to, interproc) = &static_pairs[i];
                confirmed.push(ConfirmedWait {
                    observed: obs.clone(),
                    static_from: from.clone(),
                    static_to: to.clone(),
                    interprocedural: *interproc,
                });
            }
            None => {
                if seen_missed.insert((h.clone(), w.clone())) {
                    static_missed.push(obs.clone());
                }
            }
        }
    }

    // Static edges with no runtime witness. Dedup by (from,to).
    let mut static_only: Vec<StaticOnlyEdge> = Vec::new();
    let mut seen_only: HashSet<(String, String)> = HashSet::new();
    for (i, (from, to, interproc)) in static_pairs.iter().enumerate() {
        if witnessed.contains(&i) {
            continue;
        }
        if seen_only.insert((from.clone(), to.clone())) {
            static_only.push(StaticOnlyEdge {
                from: from.clone(),
                to: to.clone(),
                interprocedural: *interproc,
            });
        }
    }

    Reconciliation {
        confirmed,
        static_missed,
        static_only,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::lock_order::AcqMode;

    fn edge(from: &str, to: &str, interproc: bool) -> LockEdge {
        LockEdge {
            from: from.into(),
            to: to.into(),
            from_mode: AcqMode::Write,
            to_mode: AcqMode::Write,
            min_confidence: 0.9,
            interprocedural: interproc,
            held_symbol: 1,
            held_line: 1,
            acquired_symbol: 2,
            acquired_line: 2,
            via_callee: None,
        }
    }

    #[test]
    fn offcpu_folded_extracts_lock_wait() {
        // leaf-last: thread;...;holder_fn;wanted_fn;pthread_mutex_lock;futex_wait
        let text = "\
worker;start_loop;handle_request;acquire_b;pthread_mutex_lock;futex_wait 4200
idle;epoll_wait 999
";
        let waits = parse_offcpu_folded(text);
        assert_eq!(waits.len(), 1, "waits: {:?}", waits);
        assert_eq!(waits[0].wanted, "acquire_b");
        assert_eq!(waits[0].holder, "handle_request");
        assert!(waits[0].primitive.contains("futex_wait") || waits[0].primitive.contains("mutex"));
    }

    #[test]
    fn perf_script_extracts_lock_wait() {
        let text = "\
myapp 1234 [001] 99.1: cycles:
\t0x00007fff in futex_wait (libc.so)
\t0x00007fff in pthread_mutex_lock (libc.so)
\t0x000055aa in acquire_a (myapp)
\t0x000055ab in worker_main (myapp)

myapp 1235 [002] 99.2: cycles:
\t0x00007fff in epoll_wait (libc.so)
";
        let waits = parse_perf_script(text);
        assert_eq!(waits.len(), 1, "waits: {:?}", waits);
        assert_eq!(waits[0].wanted, "acquire_a");
        assert_eq!(waits[0].holder, "worker_main");
    }

    #[test]
    fn gdb_bt_extracts_lock_wait() {
        let text = "\
Thread 2 (Thread 0x7f blah):
#0  0x00007f in __lll_lock_wait () from /lib/libpthread.so
#1  0x00007f in pthread_mutex_lock () from /lib/libpthread.so
#2  0x000055 in lock_resource_x (self=0x1) at src/db.rs:42
#3  0x000055 in handle_txn (req=...) at src/db.rs:90
Thread 1 (Thread 0x7e blah):
#0  0x00007f in epoll_wait () from /lib/libc.so
";
        let waits = parse_gdb_bt(text);
        assert_eq!(waits.len(), 1, "waits: {:?}", waits);
        assert_eq!(waits[0].wanted, "lock_resource_x");
        assert_eq!(waits[0].holder, "handle_txn");
    }

    #[test]
    fn reconcile_confirms_matching_edge() {
        let observed = vec![("handle_request".to_string(), "acquire_b".to_string())];
        let edges = vec![edge("handle_request", "acquire_b", false)];
        let rec = reconcile(&observed, &edges);
        assert_eq!(rec.confirmed.len(), 1, "rec: {:?}", rec);
        assert!(rec.static_missed.is_empty());
        assert!(rec.static_only.is_empty());
    }

    #[test]
    fn reconcile_flags_static_missed() {
        // Observed a wait the static graph never predicted.
        let observed = vec![("foo".to_string(), "bar".to_string())];
        let edges = vec![edge("alpha", "beta", false)];
        let rec = reconcile(&observed, &edges);
        assert_eq!(rec.static_missed.len(), 1);
        assert_eq!(rec.static_missed[0].wanted, "bar");
        // The unrelated static edge is reported static_only.
        assert_eq!(rec.static_only.len(), 1);
        assert!(rec.confirmed.is_empty());
    }

    #[test]
    fn reconcile_flags_static_only() {
        // No runtime observation; one static edge remains static_only.
        let rec = reconcile(&[], &[edge("a", "b", true)]);
        assert_eq!(rec.static_only.len(), 1);
        assert!(rec.static_only[0].interprocedural);
        assert!(rec.confirmed.is_empty());
        assert!(rec.static_missed.is_empty());
    }

    #[test]
    fn reconcile_matches_rotated_cycle() {
        // A B↔A runtime observation matches an A→B static edge (cycle rotation).
        let observed = vec![("lock_b".to_string(), "lock_a".to_string())];
        let edges = vec![edge("lock_a", "lock_b", false)];
        let rec = reconcile(&observed, &edges);
        assert_eq!(rec.confirmed.len(), 1, "rec: {:?}", rec);
    }

    #[test]
    fn bare_symbol_strips_path_and_generics() {
        assert_eq!(bare_symbol("myapp::db::lock_x"), "lock_x");
        assert_eq!(bare_symbol("Foo<T>::method"), "method");
        assert_eq!(bare_symbol("plain"), "plain");
    }

    #[test]
    fn clean_frame_strips_offset_and_module() {
        assert_eq!(clean_frame("pthread_mutex_lock+0x1a"), "pthread_mutex_lock");
        assert_eq!(clean_frame("func (/usr/lib/libc.so)"), "func");
        assert_eq!(clean_frame("0x00007fff"), "");
    }

    #[test]
    fn parsers_no_panic_on_garbage() {
        for s in ["", "   ", ";;;", "#", "Thread", "0x 0x 0x"] {
            let _ = parse_offcpu_folded(s);
            let _ = parse_perf_script(s);
            let _ = parse_gdb_bt(s);
        }
    }
}
