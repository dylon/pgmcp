//! Concurrency analysis — the DB glue between the `sync_ops` skeleton and the
//! pure graph algorithms.
//!
//! - Shared-memory deadlock: [`analyze_lock_order`] builds the interprocedural
//!   lock-order graph (via [`crate::graph::lock_order`]) and reports cycles
//!   with severity + witnesses; [`lock_order_edges`] exposes the raw graph for
//!   the inspection tool.
//! - Message-passing deadlock + bottlenecks live in sibling phases (`petri`,
//!   bottleneck queries) and reuse the same skeleton fetch.

pub mod findings;
pub mod severity;

use std::collections::{HashMap, HashSet, VecDeque};

use sqlx::PgPool;

use crate::db::queries::{self, SymbolMeta, SyncOpRow};
use crate::graph::lock_order::{self, AcqMode, LockCycle, LockEdge, LockEvent, ReachAcq};
use crate::graph::petri::{self, ChannelEvent, ChannelFinding, MsgKind};
use crate::parsing::sync_ops::SyncOpKind;
use crate::tracker::severity::Severity;

/// Knobs for the lock-order analysis.
#[derive(Clone, Copy, Debug)]
pub struct LockOrderOptions {
    /// Max call hops for interprocedural lock inlining (held-while-call reach).
    pub max_call_depth: u32,
    /// Drop edges whose weakest resource-key confidence is below this.
    pub confidence_floor: f32,
    /// Bound on simple-cycle enumeration length.
    pub max_cycle_len: usize,
    /// Minimum resolution confidence for a call edge to be followed.
    pub call_confidence: f32,
}

impl Default for LockOrderOptions {
    fn default() -> Self {
        Self {
            max_call_depth: 5,
            confidence_floor: 0.3,
            max_cycle_len: 6,
            call_confidence: 0.5,
        }
    }
}

/// A confirmed lock-order cycle with severity and the symbol metadata needed to
/// render its witness.
pub struct LockCycleFinding {
    pub cycle: LockCycle,
    pub severity: Severity,
    pub score: f32,
    pub public_api_reachable: bool,
    pub meta: HashMap<i64, SymbolMeta>,
}

/// Map a lock-paradigm `sync_ops` row to a [`LockEvent`] (skips non-lock ops and
/// keyless acquires/releases — they can't form identifiable graph nodes).
fn row_to_event(r: &SyncOpRow) -> Option<LockEvent> {
    let kind = SyncOpKind::from_db_str(&r.op_kind)?;
    match kind {
        SyncOpKind::Acquire | SyncOpKind::AcquireWrite => Some(LockEvent::Acquire {
            key: r.resource_key.clone()?,
            mode: AcqMode::Write,
            conf: r.resource_confidence,
            line: r.line as u32,
        }),
        SyncOpKind::AcquireRead => Some(LockEvent::Acquire {
            key: r.resource_key.clone()?,
            mode: AcqMode::Read,
            conf: r.resource_confidence,
            line: r.line as u32,
        }),
        SyncOpKind::Release => Some(LockEvent::Release {
            key: r.resource_key.clone()?,
        }),
        _ => None,
    }
}

/// Build the interprocedural lock-order edges for a project (no cycle
/// detection). Shared by [`analyze_lock_order`] and the `lock_order_graph`
/// inspection tool.
pub async fn lock_order_edges(
    pool: &PgPool,
    project_id: i32,
    opts: LockOrderOptions,
) -> Result<Vec<LockEdge>, sqlx::Error> {
    let rows = queries::sync_skeleton_for_project(pool, project_id, Some("lock")).await?;
    let call_edges =
        queries::resolved_call_edges_for_project(pool, project_id, opts.call_confidence).await?;

    // Per-symbol event stream (acquire/release ops) + each symbol's direct
    // acquire sites. `rows` arrive ordered by (symbol_id, seq). Tuple sort key:
    // (line, kind-tiebreak [ops<calls], seq).
    type Keyed = (i64, u8, i32, LockEvent);
    let mut events_by_symbol: HashMap<i64, Vec<Keyed>> = HashMap::new();
    let mut direct_acq_site: HashMap<i64, HashMap<String, ReachAcq>> = HashMap::new();
    for r in &rows {
        let Some(ev) = row_to_event(r) else {
            continue;
        };
        if let LockEvent::Acquire {
            key,
            mode,
            conf,
            line,
        } = &ev
        {
            let racq = ReachAcq {
                mode: *mode,
                conf: *conf,
                symbol_id: r.symbol_id,
                line: *line,
            };
            direct_acq_site
                .entry(r.symbol_id)
                .or_default()
                .entry(key.clone())
                .and_modify(|e| {
                    if racq.conf > e.conf {
                        *e = racq.clone();
                    }
                })
                .or_insert(racq);
        }
        events_by_symbol
            .entry(r.symbol_id)
            .or_default()
            .push((r.line as i64, 0, r.seq, ev));
    }

    // Interleave resolved call sites; build the reverse call graph for inlining.
    let mut rev: HashMap<i64, Vec<i64>> = HashMap::new();
    for (src, tgt, line) in &call_edges {
        rev.entry(*tgt).or_default().push(*src);
        events_by_symbol.entry(*src).or_default().push((
            *line as i64,
            1,
            0,
            LockEvent::Call { callee: *tgt },
        ));
    }

    let mut ordered: HashMap<i64, Vec<LockEvent>> = HashMap::with_capacity(events_by_symbol.len());
    for (sym, mut evs) in events_by_symbol {
        evs.sort_by_key(|e| (e.0, e.1, e.2));
        ordered.insert(sym, evs.into_iter().map(|(_, _, _, e)| e).collect());
    }

    // reachable_acq[s] = locks acquirable within K hops FROM s. Reverse-BFS from
    // each acquirer up to K hops, attributing its acquire sites to every
    // ancestor (the symbols that reach it).
    let mut reachable_acq: HashMap<i64, HashMap<String, ReachAcq>> = HashMap::new();
    for (&acq_sym, a_acq) in &direct_acq_site {
        let mut visited: HashSet<i64> = HashSet::new();
        let mut q: VecDeque<(i64, u32)> = VecDeque::new();
        q.push_back((acq_sym, 0));
        visited.insert(acq_sym);
        while let Some((node, d)) = q.pop_front() {
            let entry = reachable_acq.entry(node).or_default();
            for (key, racq) in a_acq {
                entry
                    .entry(key.clone())
                    .and_modify(|e| {
                        if racq.conf > e.conf {
                            *e = racq.clone();
                        }
                    })
                    .or_insert_with(|| racq.clone());
            }
            if d < opts.max_call_depth
                && let Some(callers) = rev.get(&node)
            {
                for &c in callers {
                    if visited.insert(c) {
                        q.push_back((c, d + 1));
                    }
                }
            }
        }
    }

    Ok(lock_order::build_lock_order(
        &ordered,
        &reachable_acq,
        opts.confidence_floor,
    ))
}

/// Detect shared-memory deadlock candidates (lock-order cycles) for a project.
pub async fn analyze_lock_order(
    pool: &PgPool,
    project_id: i32,
    opts: LockOrderOptions,
) -> Result<Vec<LockCycleFinding>, sqlx::Error> {
    let edges = lock_order_edges(pool, project_id, opts).await?;
    let cycles = lock_order::find_lock_cycles(&edges, opts.max_cycle_len);
    if cycles.is_empty() {
        return Ok(Vec::new());
    }

    // Witness metadata for every symbol named in a cycle.
    let mut sym_ids: HashSet<i64> = HashSet::new();
    for c in &cycles {
        for e in &c.edges {
            sym_ids.insert(e.held_symbol);
            sym_ids.insert(e.acquired_symbol);
            if let Some(callee) = e.via_callee {
                sym_ids.insert(callee);
            }
        }
    }
    let ids: Vec<i64> = sym_ids.into_iter().collect();
    let meta_map: HashMap<i64, SymbolMeta> = queries::symbol_meta_for_ids(pool, &ids)
        .await?
        .into_iter()
        .map(|m| (m.id, m))
        .collect();

    let mut out = Vec::with_capacity(cycles.len());
    for cycle in cycles {
        let public = cycle.edges.iter().any(|e| {
            [e.held_symbol, e.acquired_symbol].iter().any(|id| {
                meta_map
                    .get(id)
                    .map(|m| m.visibility.as_deref() == Some("public"))
                    .unwrap_or(false)
            })
        });
        let (severity, score) = severity::cycle_severity(&cycle, public);
        let mut cyc_meta: HashMap<i64, SymbolMeta> = HashMap::new();
        for e in &cycle.edges {
            for id in [e.held_symbol, e.acquired_symbol]
                .into_iter()
                .chain(e.via_callee)
            {
                if let Some(m) = meta_map.get(&id) {
                    cyc_meta.insert(id, m.clone());
                }
            }
        }
        out.push(LockCycleFinding {
            cycle,
            severity,
            score,
            public_api_reachable: public,
            meta: cyc_meta,
        });
    }
    out.sort_by(|a, b| {
        b.severity.rank().cmp(&a.severity.rank()).then(
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    Ok(out)
}

/// Map a message-paradigm `sync_ops` op_kind to a Petri [`MsgKind`].
fn msg_kind_from_str(s: &str) -> Option<MsgKind> {
    match s {
        "send" => Some(MsgKind::Send),
        "send_persistent" => Some(MsgKind::SendPersistent),
        "recv" => Some(MsgKind::Recv),
        "recv_persistent" => Some(MsgKind::RecvPersistent),
        "select" => Some(MsgKind::Select),
        "spawn" => Some(MsgKind::Spawn),
        _ => None,
    }
}

/// Detect message-passing (channel) deadlock candidates for a project, plus the
/// symbol metadata for the involved processes. Returns
/// `(findings, symbol_id → metadata)`.
pub async fn analyze_channels(
    pool: &PgPool,
    project_id: i32,
) -> Result<(Vec<ChannelFinding>, HashMap<i64, SymbolMeta>), sqlx::Error> {
    let rows = queries::sync_skeleton_for_project(pool, project_id, Some("message")).await?;
    // rows arrive ordered by (symbol_id, seq), so each process's events are in
    // program order — the cyclic-wait analysis relies on "first op".
    let mut by_proc: HashMap<i64, Vec<ChannelEvent>> = HashMap::new();
    for r in &rows {
        let Some(kind) = msg_kind_from_str(&r.op_kind) else {
            continue;
        };
        by_proc.entry(r.symbol_id).or_default().push(ChannelEvent {
            kind,
            channel: r.resource_key.clone(),
        });
    }

    let findings = petri::analyze_channels(&by_proc);

    let mut ids: HashSet<i64> = HashSet::new();
    for f in &findings {
        for &p in &f.processes {
            ids.insert(p);
        }
    }
    let meta: HashMap<i64, SymbolMeta> =
        queries::symbol_meta_for_ids(pool, &ids.into_iter().collect::<Vec<_>>())
            .await?
            .into_iter()
            .map(|m| (m.id, m))
            .collect();
    Ok((findings, meta))
}
