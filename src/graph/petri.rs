//! Message-passing (channel) deadlock analysis — the polynomial structural
//! signals over per-process channel skeletons (Petri-net semantics without
//! state-space enumeration).
//!
//! Pure algorithm (no DB). Three signals, all polynomial:
//! 1. **Matching** — `orphan_send` (a channel sent-to but never received) and
//!    `blocked_recv` (a linear receive whose channel has no producer anywhere →
//!    the receiver blocks forever).
//! 2. **Cyclic wait** — a set of processes each *initially blocked* on a receive
//!    that only another (also-initially-blocked) member produces; a cycle in
//!    this wait-for relation is a communication deadlock
//!    ([`crate::graph::algorithms::find_cycles`]).
//!
//! These map onto the Petri net the formal model reasons about (places =
//! channel buffers + control points; transitions = send/recv): a `blocked_recv`
//! is a transition whose sole input place lies in an unmarked siphon; a channel
//! cycle is a reachable dead marking. Soundness is proved in
//! `docs/formal/rocq/ChannelDeadlock.v`. Rholang's persistent receive (`<=`)
//! never starves (it does not consume its control token), so it is excluded
//! from "initially blocked".

use std::collections::{HashMap, HashSet};

use petgraph::graph::{DiGraph, NodeIndex};

use crate::graph::algorithms::find_cycles;

/// A channel operation in a process's ordered message skeleton.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MsgKind {
    Send,
    SendPersistent,
    Recv,
    RecvPersistent,
    Select,
    Spawn,
}

impl MsgKind {
    /// A linear (consuming, blocking-if-empty) receive — the kind that can
    /// starve. Persistent receive stays armed and does not block progress.
    pub fn is_blocking_recv(self) -> bool {
        matches!(self, MsgKind::Recv)
    }
    pub fn is_send(self) -> bool {
        matches!(self, MsgKind::Send | MsgKind::SendPersistent)
    }
}

/// One ordered channel event in a process (symbol) body.
#[derive(Clone, Debug)]
pub struct ChannelEvent {
    pub kind: MsgKind,
    pub channel: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelFindingKind {
    OrphanSend,
    BlockedRecv,
    ChannelCycle,
}

impl ChannelFindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OrphanSend => "orphan_send",
            Self::BlockedRecv => "blocked_recv",
            Self::ChannelCycle => "channel_cycle",
        }
    }
}

/// A channel-deadlock finding.
#[derive(Clone, Debug)]
pub struct ChannelFinding {
    pub kind: ChannelFindingKind,
    /// The channel involved (for matching findings); `None` for a multi-channel cycle.
    pub channel: Option<String>,
    /// Processes (symbol ids) participating.
    pub processes: Vec<i64>,
    /// For a cycle: each process's `(symbol, waits_on_channel)`.
    pub waits: Vec<(i64, String)>,
    pub detail: String,
}

/// Per-channel sender / receiver tallies across all processes.
struct ChannelTally {
    senders: HashSet<i64>,
    linear_receivers: HashSet<i64>,
    persistent_receivers: HashSet<i64>,
}

impl ChannelTally {
    fn new() -> Self {
        Self {
            senders: HashSet::new(),
            linear_receivers: HashSet::new(),
            persistent_receivers: HashSet::new(),
        }
    }
    fn any_receiver(&self) -> bool {
        !self.linear_receivers.is_empty() || !self.persistent_receivers.is_empty()
    }
}

fn tally_channels(
    events_by_process: &HashMap<i64, Vec<ChannelEvent>>,
) -> HashMap<String, ChannelTally> {
    let mut tallies: HashMap<String, ChannelTally> = HashMap::new();
    for (&proc, events) in events_by_process {
        for ev in events {
            let Some(ch) = &ev.channel else { continue };
            let t = tallies.entry(ch.clone()).or_insert_with(ChannelTally::new);
            match ev.kind {
                MsgKind::Send | MsgKind::SendPersistent => {
                    t.senders.insert(proc);
                }
                MsgKind::Recv => {
                    t.linear_receivers.insert(proc);
                }
                MsgKind::RecvPersistent => {
                    t.persistent_receivers.insert(proc);
                }
                MsgKind::Select | MsgKind::Spawn => {}
            }
        }
    }
    tallies
}

/// Matching analysis: orphan sends and producer-less blocked receives.
pub fn channel_matching(
    events_by_process: &HashMap<i64, Vec<ChannelEvent>>,
) -> Vec<ChannelFinding> {
    let tallies = tally_channels(events_by_process);
    let mut out = Vec::new();
    for (ch, t) in &tallies {
        if !t.senders.is_empty() && !t.any_receiver() {
            let mut procs: Vec<i64> = t.senders.iter().copied().collect();
            procs.sort_unstable();
            out.push(ChannelFinding {
                kind: ChannelFindingKind::OrphanSend,
                channel: Some(ch.clone()),
                processes: procs,
                waits: Vec::new(),
                detail: format!(
                    "channel `{ch}` is sent to but never received (orphan send / dropped channel)"
                ),
            });
        }
        if !t.linear_receivers.is_empty() && t.senders.is_empty() {
            let mut procs: Vec<i64> = t.linear_receivers.iter().copied().collect();
            procs.sort_unstable();
            out.push(ChannelFinding {
                kind: ChannelFindingKind::BlockedRecv,
                channel: Some(ch.clone()),
                processes: procs,
                waits: Vec::new(),
                detail: format!("linear receive on `{ch}` has no producer anywhere — the receiver blocks forever"),
            });
        }
    }
    out.sort_by(|a, b| a.channel.cmp(&b.channel));
    out
}

/// Cyclic-wait analysis: processes that are *initially blocked* on a receive
/// only another initially-blocked process produces, forming a wait-for cycle.
pub fn channel_cycles(events_by_process: &HashMap<i64, Vec<ChannelEvent>>) -> Vec<ChannelFinding> {
    // first_recv[p] = channel of p's FIRST linear receive, if p's first channel
    // op is such a receive (i.e. p blocks immediately).
    let mut first_recv: HashMap<i64, String> = HashMap::new();
    // producers[c] = processes that send on c.
    let mut producers: HashMap<String, Vec<i64>> = HashMap::new();

    for (&proc, events) in events_by_process {
        for ev in events {
            if ev.kind.is_send()
                && let Some(ch) = &ev.channel
            {
                producers.entry(ch.clone()).or_default().push(proc);
            }
        }
        // first message op that is a blocking recv on a named channel
        if let Some(ev) = events
            .iter()
            .find(|e| e.kind.is_send() || e.kind.is_blocking_recv())
            && ev.kind.is_blocking_recv()
            && let Some(ch) = &ev.channel
        {
            first_recv.insert(proc, ch.clone());
        }
    }

    // Wait-for graph: P → Q iff P is initially blocked on channel C and Q (also
    // initially blocked) produces C.
    let mut idx: HashMap<i64, NodeIndex> = HashMap::new();
    let mut g: DiGraph<i64, ()> = DiGraph::new();
    let mut waits_on: HashMap<(i64, i64), String> = HashMap::new();
    for (&p, ch) in &first_recv {
        let Some(prods) = producers.get(ch) else {
            continue;
        };
        for &q in prods {
            if q != p && first_recv.contains_key(&q) {
                let pa = *idx.entry(p).or_insert_with(|| g.add_node(p));
                let qa = *idx.entry(q).or_insert_with(|| g.add_node(q));
                g.add_edge(pa, qa, ());
                waits_on.insert((p, q), ch.clone());
            }
        }
    }

    let mut out = Vec::new();
    let mut seen: HashSet<Vec<i64>> = HashSet::new();
    for scc in find_cycles(&g) {
        let mut procs: Vec<i64> = scc.iter().map(|n| g[*n]).collect();
        procs.sort_unstable();
        if !seen.insert(procs.clone()) {
            continue;
        }
        // Witness: each member and the channel it waits on (to the next member).
        let mut waits = Vec::new();
        for n in &scc {
            let p = g[*n];
            // find the channel p waits on toward any other member in the SCC
            if let Some(((_, _), ch)) = waits_on
                .iter()
                .find(|((pp, qq), _)| *pp == p && scc.iter().any(|m| g[*m] == *qq))
            {
                waits.push((p, ch.clone()));
            }
        }
        out.push(ChannelFinding {
            kind: ChannelFindingKind::ChannelCycle,
            channel: None,
            processes: procs,
            waits,
            detail: "communication deadlock: each process blocks on a receive that only the next \
                     (also-blocked) process in the cycle would produce"
                .to_string(),
        });
    }
    out
}

/// Run all channel-deadlock signals.
pub fn analyze_channels(
    events_by_process: &HashMap<i64, Vec<ChannelEvent>>,
) -> Vec<ChannelFinding> {
    let mut out = channel_cycles(events_by_process);
    out.extend(channel_matching(events_by_process));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: MsgKind, ch: &str) -> ChannelEvent {
        ChannelEvent {
            kind,
            channel: Some(ch.into()),
        }
    }

    #[test]
    fn blocked_recv_no_producer() {
        let mut m = HashMap::new();
        m.insert(1i64, vec![ev(MsgKind::Recv, "cmd")]);
        let f = channel_matching(&m);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, ChannelFindingKind::BlockedRecv);
        assert_eq!(f[0].channel.as_deref(), Some("cmd"));
    }

    #[test]
    fn orphan_send_no_receiver() {
        let mut m = HashMap::new();
        m.insert(1i64, vec![ev(MsgKind::Send, "metrics")]);
        let f = channel_matching(&m);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, ChannelFindingKind::OrphanSend);
    }

    #[test]
    fn matched_channel_is_clean() {
        let mut m = HashMap::new();
        m.insert(1i64, vec![ev(MsgKind::Send, "c")]);
        m.insert(2i64, vec![ev(MsgKind::Recv, "c")]);
        assert!(channel_matching(&m).is_empty());
    }

    #[test]
    fn mutual_initial_recv_is_a_cycle() {
        // P1: recv(c1) then send(c2)   P2: recv(c2) then send(c1)  → deadlock.
        let mut m = HashMap::new();
        m.insert(1i64, vec![ev(MsgKind::Recv, "c1"), ev(MsgKind::Send, "c2")]);
        m.insert(2i64, vec![ev(MsgKind::Recv, "c2"), ev(MsgKind::Send, "c1")]);
        let cycles = channel_cycles(&m);
        assert_eq!(cycles.len(), 1, "one mutual-wait cycle: {cycles:?}");
        assert_eq!(cycles[0].kind, ChannelFindingKind::ChannelCycle);
        assert_eq!(cycles[0].processes, vec![1, 2]);
    }

    #[test]
    fn send_first_no_cycle() {
        // P1 sends first (not initially blocked) → no deadlock even with P2 recv.
        let mut m = HashMap::new();
        m.insert(1i64, vec![ev(MsgKind::Send, "c1"), ev(MsgKind::Recv, "c2")]);
        m.insert(2i64, vec![ev(MsgKind::Recv, "c1"), ev(MsgKind::Send, "c2")]);
        assert!(
            channel_cycles(&m).is_empty(),
            "P1 sends first, so the cycle is broken"
        );
    }

    #[test]
    fn persistent_recv_does_not_starve() {
        // A persistent receiver (contract) on `c` with no sender is not a
        // blocked_recv (it stays armed; nothing is waiting on it to finish).
        let mut m = HashMap::new();
        m.insert(1i64, vec![ev(MsgKind::RecvPersistent, "c")]);
        assert!(channel_matching(&m).is_empty());
    }
}
