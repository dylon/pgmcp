//! The five RecursiveMAS collaboration patterns (Yang et al., arXiv:2604.25917,
//! Table 1) as well-formed [`GlobalType`]s — the canonical *contract* each
//! `a2a_pattern_*` tool is checked against. The protocols follow the paper's
//! role structure; where the current text implementation diverges (e.g. the
//! Deliberation Tool-Caller producing the final answer on convergence, per the
//! paper, vs. the impl reusing the Reflector's text), the observer
//! (`super::conformance`) surfaces it — that divergence is the point.

use crate::csm::examples::deliberation;
use crate::csm::mpst::global::{GlobalType, end, interaction};
use crate::csm::role::{Label, Role};

/// The five patterns. The join key `pattern_skill_id` matches `a2a_tasks.skill_id`
/// (the `a2a_pattern_*` tool name) so a recorded run can be matched to its protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolId {
    Sequential,
    Mixture,
    Distillation,
    Deliberation,
    Recursive,
}

impl ProtocolId {
    pub const ALL: [ProtocolId; 5] = [
        ProtocolId::Sequential,
        ProtocolId::Mixture,
        ProtocolId::Distillation,
        ProtocolId::Deliberation,
        ProtocolId::Recursive,
    ];

    /// Short stable name (`"sequential"`, …).
    pub fn name(self) -> &'static str {
        match self {
            ProtocolId::Sequential => "sequential",
            ProtocolId::Mixture => "mixture",
            ProtocolId::Distillation => "distillation",
            ProtocolId::Deliberation => "deliberation",
            ProtocolId::Recursive => "recursive",
        }
    }

    /// The `a2a_tasks.skill_id` of the tool that runs this pattern.
    pub fn pattern_skill_id(self) -> &'static str {
        match self {
            ProtocolId::Sequential => "a2a_pattern_sequential",
            ProtocolId::Mixture => "a2a_pattern_mixture",
            ProtocolId::Distillation => "a2a_pattern_distillation",
            ProtocolId::Deliberation => "a2a_pattern_deliberation",
            ProtocolId::Recursive => "a2a_pattern_recursive",
        }
    }

    pub fn from_name(s: &str) -> Option<ProtocolId> {
        ProtocolId::ALL.into_iter().find(|p| p.name() == s)
    }

    pub fn from_skill_id(s: &str) -> Option<ProtocolId> {
        ProtocolId::ALL
            .into_iter()
            .find(|p| p.pattern_skill_id() == s)
    }
}

/// Parameters that make a pattern's protocol concrete (specialist count, round
/// bounds, recursion depth). Defaults mirror the live tool clamps.
#[derive(Debug, Clone, Copy)]
pub struct ProtocolParams {
    /// Mixture specialist count (live cap 1..=8).
    pub n_specialists: usize,
    /// Sequential recursion rounds (live clamp 1..=5).
    pub recursion_rounds: usize,
    /// Recursive/RLM unroll depth (`MAX_RLM_DEPTH = 4`).
    pub rlm_depth: usize,
}

impl Default for ProtocolParams {
    fn default() -> Self {
        ProtocolParams {
            n_specialists: 3,
            recursion_rounds: 1,
            rlm_depth: 2,
        }
    }
}

/// Build the global type for a pattern under the given parameters.
pub fn global_of(id: ProtocolId, p: &ProtocolParams) -> GlobalType {
    match id {
        ProtocolId::Sequential => sequential(p.recursion_rounds.max(1)),
        ProtocolId::Mixture => mixture(p.n_specialists.clamp(1, 8)),
        ProtocolId::Distillation => distillation(),
        ProtocolId::Deliberation => deliberation(),
        // Depth comes from the actual run when validating (0 ⇒ a leaf RLM with
        // no decomposition, which is the empty protocol `end`).
        ProtocolId::Recursive => recursive(p.rlm_depth),
    }
}

fn lbl(s: &str) -> Label {
    Label::text(s)
}

/// Sequential: `O → P : plan_req . P → O : plan . O → C : critique_req .
/// C → O : critique . O → S : solve_req . S → O : solution`, unrolled `rounds`
/// times (each round's Solver output threads into the next Planner), then `end`.
fn sequential(rounds: usize) -> GlobalType {
    fn round(k: usize, rounds: usize) -> GlobalType {
        if k >= rounds {
            return end();
        }
        interaction(
            "O",
            "P",
            lbl("plan_req"),
            interaction(
                "P",
                "O",
                lbl("plan"),
                interaction(
                    "O",
                    "C",
                    lbl("critique_req"),
                    interaction(
                        "C",
                        "O",
                        lbl("critique"),
                        interaction(
                            "O",
                            "S",
                            lbl("solve_req"),
                            interaction("S", "O", lbl("solution"), round(k + 1, rounds)),
                        ),
                    ),
                ),
            ),
        )
    }
    round(0, rounds)
}

/// Mixture: `O → Spᵢ : query . Spᵢ → O : answer` for each of N specialists, then
/// `O → Sum : reduce_req . Sum → O : summary . end`. The fan-out is modelled as a
/// causal sequence; independent per-specialist channels mean any runtime
/// interleaving conforms.
fn mixture(n: usize) -> GlobalType {
    fn specialist(i: usize, n: usize) -> GlobalType {
        if i > n {
            return interaction(
                "O",
                "Sum",
                lbl("reduce_req"),
                interaction("Sum", "O", lbl("summary"), end()),
            );
        }
        let role = Role::new(format!("Sp{i}"));
        interaction(
            "O",
            role.clone(),
            lbl("query"),
            interaction(role, "O", lbl("answer"), specialist(i + 1, n)),
        )
    }
    specialist(1, n)
}

/// Distillation: `O → E : query . E → O : expert . O → L : distill_req .
/// L → O : learner . end`.
fn distillation() -> GlobalType {
    interaction(
        "O",
        "E",
        lbl("query"),
        interaction(
            "E",
            "O",
            lbl("expert"),
            interaction(
                "O",
                "L",
                lbl("distill_req"),
                interaction("L", "O", lbl("learner"), end()),
            ),
        ),
    )
}

/// Recursive (RLM): the depth-bounded self-recursion, unrolled to `depth` levels
/// `O → Subₖ : subcall . Subₖ → O : subresult`, innermost `end`. Each level is a
/// distinct role (the unrolled self-call tree); RLM's internal step kinds
/// (peek/filter/chunk/verify/stitch) are local computation, not communication,
/// so they are correctly absent from the protocol.
fn recursive(depth: usize) -> GlobalType {
    fn level(k: usize, depth: usize) -> GlobalType {
        if k > depth {
            return end();
        }
        let sub = Role::new(format!("Sub{k}"));
        interaction(
            "O",
            sub.clone(),
            lbl("subcall"),
            interaction(sub, "O", lbl("subresult"), level(k + 1, depth)),
        )
    }
    level(1, depth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::machine::Network;
    use crate::csm::mpst::wellformed::well_formed;

    #[test]
    fn every_protocol_is_well_formed_and_builds_a_network() {
        let p = ProtocolParams::default();
        for id in ProtocolId::ALL {
            let g = global_of(id, &p);
            well_formed(&g)
                .unwrap_or_else(|e| panic!("{} not well-formed: {}", id.name(), e.message()));
            Network::build(id.name(), &g)
                .unwrap_or_else(|e| panic!("{} does not project: {}", id.name(), e.message()));
        }
    }

    #[test]
    fn skill_id_round_trips() {
        for id in ProtocolId::ALL {
            assert_eq!(ProtocolId::from_skill_id(id.pattern_skill_id()), Some(id));
            assert_eq!(ProtocolId::from_name(id.name()), Some(id));
        }
    }

    #[test]
    fn sequential_unrolls_rounds() {
        // 1 round = 4 participants (O,P,C,S); 2 rounds reuses them, more messages.
        let g1 = sequential(1);
        let g2 = sequential(2);
        assert_eq!(g1.participants().len(), 4);
        assert_eq!(g2.participants().len(), 4);
        // The 2-round network still projects.
        assert!(Network::build("seq2", &g2).is_ok());
    }

    #[test]
    fn mixture_scales_with_specialists() {
        let g = mixture(3);
        // O, Sp1, Sp2, Sp3, Sum = 5 roles.
        assert_eq!(g.participants().len(), 5);
    }
}
