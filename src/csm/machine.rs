//! Compiling a [`LocalType`] to a dense-state [`LocalMachine`], and assembling a
//! [`Network`] (one machine per role + the channel topology) from a
//! [`GlobalType`].
//!
//! Compilation linearises the (possibly recursive) local type into integer
//! states with explicit edges. `Rec`/`Var` become back-edges: a `Rec` binder
//! reuses the state allocated for its body's head, and a `Var` resolves to that
//! state — so there are no epsilon transitions.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::csm::mpst::global::{GlobalType, ProtocolEnv, ProtocolRef, TypeVar};
use crate::csm::mpst::local::LocalType;
use crate::csm::mpst::project::{ProjectionError, project};
use crate::csm::role::{Action, Channel, Label, Role};

/// A state index within a [`LocalMachine`].
pub type LocalState = usize;

/// The visibly-pushdown class of an edge — what it does to the conformance stack
/// (Alur–Madhusudan). `Internal` is an ordinary peer communication (the stack is
/// unchanged); `Call` is a frame-entry boundary (push, the `to` state is the
/// callee/box entry and the return address is supplied by the pushdown engine);
/// `Return` is a frame-exit boundary (pop — the real target is the popped return
/// address, so a `Return` edge's static `to` is a placeholder). Call-free
/// protocols have only `Internal` edges, so the flat machine is byte-identical to
/// the pre-pushdown CSM; the `Call`/`Return` boundary edges are read by the
/// pushdown conformance engine (`crate::csm::conformance`), never by the
/// call-free driver/`check_step` paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// An ordinary peer communication; the conformance stack is unchanged.
    Internal,
    /// A frame-entry boundary: push `return_state` (where to resume once the
    /// callee/box returns), then move to this edge's `to` (the callee/box entry).
    Call { return_state: LocalState },
    /// A frame-exit boundary: pop the return context and resume at the popped
    /// state. A `Return` edge's static `to` is a placeholder — the real target is
    /// the popped return address supplied by the pushdown engine.
    Return,
}

/// A labelled transition: from `from`, performing `action`, to `to`, with its
/// visibly-pushdown `kind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEdge {
    pub from: LocalState,
    pub action: Action,
    pub to: LocalState,
    pub kind: EdgeKind,
}

/// One role's communicating finite-state machine.
#[derive(Debug, Clone)]
pub struct LocalMachine {
    pub role: Role,
    pub n_states: usize,
    pub initial: LocalState,
    pub edges: Vec<LocalEdge>,
    pub terminals: BTreeSet<LocalState>,
}

impl LocalMachine {
    /// Edges leaving `state`.
    pub fn edges_from(&self, state: LocalState) -> impl Iterator<Item = &LocalEdge> {
        self.edges.iter().filter(move |e| e.from == state)
    }

    pub fn is_terminal(&self, state: LocalState) -> bool {
        self.terminals.contains(&state)
    }
}

/// Compile a **call-free** role's local type into its machine. A convenience
/// wrapper over [`compile_in`] with an empty environment; it panics if `lt`
/// contains a `LocalCall` (which needs an environment to resolve the callee — use
/// [`compile_in`] for protocols with calls). Every existing call-free caller and
/// test relies on this infallible form.
pub fn compile(role: &Role, lt: &LocalType) -> LocalMachine {
    compile_in(role, lt, &ProtocolEnv::new())
        .expect("call-free local type compiles (use compile_in for protocols with calls)")
}

/// Compile `lt` for `role` into a [`LocalMachine`], resolving any `LocalCall`
/// against `penv`. Calls/boxes compile to genuine `Call`/`Return` boundary edges
/// over a single global state space: a `Call` edge pushes the call site's return
/// state and enters the callee/box; the callee/box `End` becomes a `Return` (pop).
/// A recursive callee is compiled once per `(callee, callee-role)` and reused (the
/// RSM back-edge), so finite syntax yields unbounded nesting that the conformance
/// stack bounds. Call-free protocols produce only `Internal` edges, so the flat
/// view is byte-identical to the pre-pushdown CSM.
pub fn compile_in(
    role: &Role,
    lt: &LocalType,
    penv: &ProtocolEnv,
) -> Result<LocalMachine, ProjectionError> {
    let mut c = Compiler {
        edges: Vec::new(),
        n: 0,
        terminals: BTreeSet::new(),
        rec_env: HashMap::new(),
        role: role.clone(),
        box_exit_labels: Vec::new(),
        penv,
        box_memo: HashMap::new(),
    };
    let initial = c.go(lt, None)?;
    Ok(LocalMachine {
        role: role.clone(),
        n_states: c.n,
        initial,
        edges: c.edges,
        terminals: c.terminals,
    })
}

struct Compiler<'a> {
    edges: Vec<LocalEdge>,
    n: usize,
    terminals: BTreeSet<LocalState>,
    /// Recursion-variable scope: `μ var` binder state for `Var` back-edges.
    rec_env: HashMap<TypeVar, LocalState>,
    /// The role this machine belongs to (the "self" peer of a stack-boundary edge).
    role: Role,
    /// Stack of active box/callee exit labels. While compiling a box or callee
    /// body, its `End` emits a `Return` edge carrying the top label (the real
    /// return target is the popped address, not a static `to`); a stack handles
    /// nested frames.
    box_exit_labels: Vec<Label>,
    /// The environment resolving named callees ([`LocalType::LocalCall`]).
    penv: &'a ProtocolEnv,
    /// Memoized box entry per `(callee-name, callee-role)` — the RSM back-edge that
    /// makes a recursive callee reuse its own box instead of unrolling forever.
    box_memo: HashMap<(String, Role), LocalState>,
}

impl Compiler<'_> {
    fn fresh(&mut self) -> LocalState {
        let s = self.n;
        self.n += 1;
        s
    }

    /// Compile a named callee, projected onto the callee-role `cr` this machine's
    /// role plays, into a memoized box; return the box entry state. Allocating and
    /// memoizing the entry BEFORE compiling the body is what lets a recursive
    /// callee back-edge to itself (finite syntax → unbounded nesting bounded only
    /// by the conformance stack).
    fn compile_callee_box(
        &mut self,
        callee_name: &str,
        cr: &Role,
    ) -> Result<LocalState, ProjectionError> {
        let key = (callee_name.to_string(), cr.clone());
        if let Some(&e) = self.box_memo.get(&key) {
            return Ok(e);
        }
        let g = self
            .penv
            .resolve(&ProtocolRef::new(callee_name))
            .ok_or_else(|| ProjectionError::UnresolvedCallee {
                name: callee_name.to_string(),
            })?
            .clone();
        let callee_lt = project(&g, cr)?;
        let entry = self.fresh();
        self.box_memo.insert(key, entry);
        self.box_exit_labels
            .push(Label::text(format!("ret:{callee_name}")));
        let got = self.go(&callee_lt, Some(entry))?;
        self.box_exit_labels.pop();
        debug_assert_eq!(got, entry, "callee box head must reuse the allocated entry");
        Ok(entry)
    }

    /// Compile `lt`, returning its entry state. If `entry` is supplied (by a `Rec`
    /// binder or a box/callee entry), that state is the node's own state so the
    /// binder/entry and its body's head coincide and `Var`/recursive calls can
    /// back-edge to it.
    fn go(
        &mut self,
        lt: &LocalType,
        entry: Option<LocalState>,
    ) -> Result<LocalState, ProjectionError> {
        match lt {
            LocalType::End => {
                let s = entry.unwrap_or_else(|| self.fresh());
                match self.box_exit_labels.last() {
                    // Inside a box/callee body: `End` is the frame's pop point — emit
                    // a `Return` edge carrying the exit label. Its static `to` is a
                    // placeholder; the real target is the popped return address.
                    Some(exit_label) => {
                        let exit_label = exit_label.clone();
                        self.edges.push(LocalEdge {
                            from: s,
                            action: Action::Send {
                                to: self.role.clone(),
                                label: exit_label,
                            },
                            to: s,
                            kind: EdgeKind::Return,
                        });
                    }
                    None => {
                        self.terminals.insert(s);
                    }
                }
                Ok(s)
            }
            LocalType::Var { var } => Ok(*self
                .rec_env
                .get(var)
                .expect("unbound recursion variable in compile (well-formedness should prevent)")),
            LocalType::Rec { var, body } => {
                let s = entry.unwrap_or_else(|| self.fresh());
                let prev = self.rec_env.insert(var.clone(), s);
                let got = self.go(body, Some(s))?;
                debug_assert_eq!(got, s, "rec body head must reuse the binder state");
                match prev {
                    Some(p) => {
                        self.rec_env.insert(var.clone(), p);
                    }
                    None => {
                        self.rec_env.remove(var);
                    }
                }
                Ok(s)
            }
            LocalType::Send { to, label, cont } => {
                let s = entry.unwrap_or_else(|| self.fresh());
                let t = self.go(cont, None)?;
                self.edges.push(LocalEdge {
                    from: s,
                    action: Action::Send {
                        to: to.clone(),
                        label: label.clone(),
                    },
                    to: t,
                    kind: EdgeKind::Internal,
                });
                Ok(s)
            }
            LocalType::Recv { from, label, cont } => {
                let s = entry.unwrap_or_else(|| self.fresh());
                let t = self.go(cont, None)?;
                self.edges.push(LocalEdge {
                    from: s,
                    action: Action::Recv {
                        from: from.clone(),
                        label: label.clone(),
                    },
                    to: t,
                    kind: EdgeKind::Internal,
                });
                Ok(s)
            }
            LocalType::Select { to, branches } => {
                let s = entry.unwrap_or_else(|| self.fresh());
                for br in branches {
                    let t = self.go(&br.cont, None)?;
                    self.edges.push(LocalEdge {
                        from: s,
                        action: Action::Send {
                            to: to.clone(),
                            label: br.label.clone(),
                        },
                        to: t,
                        kind: EdgeKind::Internal,
                    });
                }
                Ok(s)
            }
            LocalType::Branch { from, branches } => {
                let s = entry.unwrap_or_else(|| self.fresh());
                for br in branches {
                    let t = self.go(&br.cont, None)?;
                    self.edges.push(LocalEdge {
                        from: s,
                        action: Action::Recv {
                            from: from.clone(),
                            label: br.label.clone(),
                        },
                        to: t,
                        kind: EdgeKind::Internal,
                    });
                }
                Ok(s)
            }
            LocalType::LocalCall {
                callee,
                subst,
                cont,
            } => {
                // A call pushes the return state (this site's continuation) and
                // enters the callee's box (compiled/memoized, so recursion is a
                // back-edge). `self.role` plays callee-role `cr = subst⁻¹(role)`.
                let s = entry.unwrap_or_else(|| self.fresh());
                let ret = self.go(cont, None)?;
                let cr = subst
                    .iter()
                    .find(|(_, v)| **v == self.role)
                    .map(|(k, _)| k.clone())
                    .expect("projection only emits LocalCall when the role participates");
                let box_entry = self.compile_callee_box(&callee.name, &cr)?;
                self.edges.push(LocalEdge {
                    from: s,
                    action: Action::Send {
                        to: self.role.clone(),
                        label: Label::text(format!("call:{}", callee.name)),
                    },
                    to: box_entry,
                    kind: EdgeKind::Call { return_state: ret },
                });
                Ok(s)
            }
            LocalType::LocalBox {
                enter,
                body,
                exit,
                cont,
            } => {
                // An inline box pushes the return state and enters the compiled
                // body; the body's `End` pops (a `Return` carrying `exit`).
                let s = entry.unwrap_or_else(|| self.fresh());
                let ret = self.go(cont, None)?;
                let body_entry = self.fresh();
                self.box_exit_labels.push(exit.clone());
                let got = self.go(body, Some(body_entry))?;
                self.box_exit_labels.pop();
                debug_assert_eq!(got, body_entry, "box body head must reuse its entry");
                self.edges.push(LocalEdge {
                    from: s,
                    action: Action::Send {
                        to: self.role.clone(),
                        label: enter.clone(),
                    },
                    to: body_entry,
                    kind: EdgeKind::Call { return_state: ret },
                });
                Ok(s)
            }
        }
    }
}

/// A network of communicating machines: one per role, plus the directed channel
/// topology induced by the protocol.
#[derive(Debug, Clone)]
pub struct Network {
    pub protocol: String,
    pub machines: BTreeMap<Role, LocalMachine>,
    pub channels: BTreeSet<Channel>,
}

impl Network {
    /// Build the network for a **call-free** global type against an empty
    /// environment. A convenience wrapper over [`Network::build_in`]; protocols
    /// containing a `GlobalCall` must use `build_in` with the environment that
    /// defines their callees.
    pub fn build(protocol: impl Into<String>, g: &GlobalType) -> Result<Network, ProjectionError> {
        Network::build_in(protocol, g, &ProtocolEnv::new())
    }

    /// Build the network for a global type against `penv`: project onto every
    /// participant, compile each projection (resolving any callee through `penv`),
    /// and collect the channel topology.
    pub fn build_in(
        protocol: impl Into<String>,
        g: &GlobalType,
        penv: &ProtocolEnv,
    ) -> Result<Network, ProjectionError> {
        let roles = g.participants();
        let mut machines = BTreeMap::new();
        for r in &roles {
            let lt = project(g, r)?;
            machines.insert(r.clone(), compile_in(r, &lt, penv)?);
        }
        let mut channels = BTreeSet::new();
        collect_channels(g, &mut channels);
        Ok(Network {
            protocol: protocol.into(),
            machines,
            channels,
        })
    }

    pub fn machine(&self, role: &Role) -> Option<&LocalMachine> {
        self.machines.get(role)
    }
}

/// Collect the directed channels (`from → to`) used by a global type.
fn collect_channels(g: &GlobalType, acc: &mut BTreeSet<Channel>) {
    match g {
        GlobalType::Interaction { from, to, cont, .. } => {
            acc.insert(Channel::new(from.clone(), to.clone()));
            collect_channels(cont, acc);
        }
        GlobalType::Choice { from, to, branches } => {
            acc.insert(Channel::new(from.clone(), to.clone()));
            for b in branches {
                collect_channels(&b.cont, acc);
            }
        }
        GlobalType::Rec { body, .. } => collect_channels(body, acc),
        // The callee's channels belong to the callee (collected when it is built);
        // here only the return continuation is local to this protocol.
        GlobalType::GlobalCall { cont, .. } => collect_channels(cont, acc),
        // A box's inline body channels ARE part of this protocol.
        GlobalType::GlobalBox { body, cont, .. } => {
            collect_channels(body, acc);
            collect_channels(cont, acc);
        }
        GlobalType::Var { .. } | GlobalType::End => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::mpst::global::{end, interaction};
    use crate::csm::mpst::local::LocalType;
    use crate::csm::role::Label;

    fn r(name: &str) -> Role {
        Role::new(name)
    }

    #[test]
    fn compile_linear_send_recv_chain() {
        // !P⟨plan⟩ . ?P⟨ans⟩ . end  → 3 states, 2 edges, 1 terminal
        let lt = LocalType::send(
            "P",
            Label::text("plan"),
            LocalType::recv("P", Label::text("ans"), LocalType::End),
        );
        let m = compile(&r("O"), &lt);
        assert_eq!(m.n_states, 3);
        assert_eq!(m.edges.len(), 2);
        assert_eq!(m.terminals.len(), 1);
        // The initial state has exactly one outgoing Send edge.
        let out: Vec<_> = m.edges_from(m.initial).collect();
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].action, Action::Send { .. }));
    }

    #[test]
    fn compile_recursion_creates_a_back_edge() {
        // μt. ?O⟨ping⟩ . t  → 1 state, 1 self-edge, no terminal
        let lt = LocalType::rec(
            "t",
            LocalType::recv("O", Label::text("ping"), LocalType::var("t")),
        );
        let m = compile(&r("R"), &lt);
        assert_eq!(m.n_states, 1);
        assert_eq!(m.edges.len(), 1);
        assert_eq!(m.edges[0].from, m.edges[0].to, "recursion must self-loop");
        assert!(m.terminals.is_empty());
    }

    #[test]
    fn build_network_projects_every_role_and_collects_channels() {
        // O → R : q . R → O : a . end
        let g = interaction(
            "O",
            "R",
            Label::text("q"),
            interaction("R", "O", Label::text("a"), end()),
        );
        let net = Network::build("ping_pong", &g).expect("projects");
        assert_eq!(net.machines.len(), 2);
        assert!(net.machine(&r("O")).is_some());
        assert!(net.machine(&r("R")).is_some());
        // Two directed channels: O→R and R→O.
        assert_eq!(net.channels.len(), 2);
        assert!(net.channels.contains(&Channel::new(r("O"), r("R"))));
        assert!(net.channels.contains(&Channel::new(r("R"), r("O"))));
    }

    #[test]
    fn build_network_carries_projection_errors() {
        // The divergent-bystander choice from the projector tests: T cannot project.
        use crate::csm::mpst::global::{choice, gbranch};
        let g = choice(
            "R",
            "O",
            vec![
                gbranch(
                    Label::text("a"),
                    interaction("T", "O", Label::text("x"), end()),
                ),
                gbranch(
                    Label::text("b"),
                    interaction("O", "T", Label::text("y"), end()),
                ),
            ],
        );
        assert!(Network::build("bad", &g).is_err());
    }
}
