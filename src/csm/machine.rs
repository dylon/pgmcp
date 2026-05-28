//! Compiling a [`LocalType`] to a dense-state [`LocalMachine`], and assembling a
//! [`Network`] (one machine per role + the channel topology) from a
//! [`GlobalType`].
//!
//! Compilation linearises the (possibly recursive) local type into integer
//! states with explicit edges. `Rec`/`Var` become back-edges: a `Rec` binder
//! reuses the state allocated for its body's head, and a `Var` resolves to that
//! state — so there are no epsilon transitions.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::csm::mpst::global::{GlobalType, TypeVar};
use crate::csm::mpst::local::LocalType;
use crate::csm::mpst::project::{ProjectionError, project};
use crate::csm::role::{Action, Channel, Role};

/// A state index within a [`LocalMachine`].
pub type LocalState = usize;

/// A labelled transition: from `from`, performing `action`, to `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEdge {
    pub from: LocalState,
    pub action: Action,
    pub to: LocalState,
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

/// Compile a role's local type into its machine.
pub fn compile(role: &Role, lt: &LocalType) -> LocalMachine {
    let mut c = Compiler {
        edges: Vec::new(),
        n: 0,
        terminals: BTreeSet::new(),
        env: HashMap::new(),
    };
    let initial = c.go(lt, None);
    LocalMachine {
        role: role.clone(),
        n_states: c.n,
        initial,
        edges: c.edges,
        terminals: c.terminals,
    }
}

struct Compiler {
    edges: Vec<LocalEdge>,
    n: usize,
    terminals: BTreeSet<LocalState>,
    env: HashMap<TypeVar, LocalState>,
}

impl Compiler {
    fn fresh(&mut self) -> LocalState {
        let s = self.n;
        self.n += 1;
        s
    }

    /// Compile `lt`, returning its entry state. If `entry` is supplied (only by
    /// a `Rec` binder), that state is used as the node's own state so the
    /// binder and its body's head coincide and `Var` can back-edge to it.
    fn go(&mut self, lt: &LocalType, entry: Option<LocalState>) -> LocalState {
        match lt {
            LocalType::End => {
                let s = entry.unwrap_or_else(|| self.fresh());
                self.terminals.insert(s);
                s
            }
            LocalType::Var { var } => *self
                .env
                .get(var)
                .expect("unbound recursion variable in compile (well-formedness should prevent)"),
            LocalType::Rec { var, body } => {
                let s = entry.unwrap_or_else(|| self.fresh());
                let prev = self.env.insert(var.clone(), s);
                let got = self.go(body, Some(s));
                debug_assert_eq!(got, s, "rec body head must reuse the binder state");
                match prev {
                    Some(p) => {
                        self.env.insert(var.clone(), p);
                    }
                    None => {
                        self.env.remove(var);
                    }
                }
                s
            }
            LocalType::Send { to, label, cont } => {
                let s = entry.unwrap_or_else(|| self.fresh());
                let t = self.go(cont, None);
                self.edges.push(LocalEdge {
                    from: s,
                    action: Action::Send {
                        to: to.clone(),
                        label: label.clone(),
                    },
                    to: t,
                });
                s
            }
            LocalType::Recv { from, label, cont } => {
                let s = entry.unwrap_or_else(|| self.fresh());
                let t = self.go(cont, None);
                self.edges.push(LocalEdge {
                    from: s,
                    action: Action::Recv {
                        from: from.clone(),
                        label: label.clone(),
                    },
                    to: t,
                });
                s
            }
            LocalType::Select { to, branches } => {
                let s = entry.unwrap_or_else(|| self.fresh());
                for br in branches {
                    let t = self.go(&br.cont, None);
                    self.edges.push(LocalEdge {
                        from: s,
                        action: Action::Send {
                            to: to.clone(),
                            label: br.label.clone(),
                        },
                        to: t,
                    });
                }
                s
            }
            LocalType::Branch { from, branches } => {
                let s = entry.unwrap_or_else(|| self.fresh());
                for br in branches {
                    let t = self.go(&br.cont, None);
                    self.edges.push(LocalEdge {
                        from: s,
                        action: Action::Recv {
                            from: from.clone(),
                            label: br.label.clone(),
                        },
                        to: t,
                    });
                }
                s
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
    /// Build the network for a global type: project onto every participant,
    /// compile each projection, and collect the channel topology.
    pub fn build(protocol: impl Into<String>, g: &GlobalType) -> Result<Network, ProjectionError> {
        let roles = g.participants();
        let mut machines = BTreeMap::new();
        for r in &roles {
            let lt = project(g, r)?;
            machines.insert(r.clone(), compile(r, &lt));
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
