//! The four RecursiveMAS collaboration patterns (Yang et al., arXiv:2604.25917,
//! Table 1) as homogeneous latent-loop topologies — the Track-B (latent)
//! realization of the Track-A protocols in [`crate::csm::registry`]. Same role
//! structure as the text patterns, but every role is a white-box latent
//! participant on one shared backbone (`W₃ = I`) and the medium is hidden-state
//! hand-off rather than text.
//!
//! **Pattern ↔ medium note.** Sequential, Mixture, and Distillation are pure
//! reasoning loops and run end-to-end in latent space. *Deliberation* includes a
//! Tool-Caller role; real tool execution is a black-box action and therefore a
//! `Text` edge (the `csm::media` discipline, R1) — so the latent topology here
//! models only the Reflector's latent refinement, and a deployment that needs
//! live tools should drive Deliberation via the Tier-2 text path
//! (`a2a::recursion`). This is documented, not silently ignored.

use crate::rmas::topology::RmasTopology;

/// The four collaboration patterns (the recursive/RLM pattern is single-role
/// self-recursion and is handled by `a2a::rlm`, not this multi-role loop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RmasPattern {
    Sequential,
    Mixture,
    Distillation,
    Deliberation,
}

impl RmasPattern {
    pub fn as_str(&self) -> &'static str {
        match self {
            RmasPattern::Sequential => "sequential",
            RmasPattern::Mixture => "mixture",
            RmasPattern::Distillation => "distillation",
            RmasPattern::Deliberation => "deliberation",
        }
    }

    /// Parse a config / tool string (default-free; `None` for unknown).
    pub fn parse(s: &str) -> Option<RmasPattern> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sequential" => Some(RmasPattern::Sequential),
            "mixture" => Some(RmasPattern::Mixture),
            "distillation" => Some(RmasPattern::Distillation),
            "deliberation" => Some(RmasPattern::Deliberation),
            _ => None,
        }
    }
}

const PLANNER: &str = "You are the Planner. Decompose the query into a concise, ordered plan of steps. Do not solve it yet — produce only the plan.";
const CRITIC: &str = "You are the Critic. Review the plan handed to you for gaps, errors, and unstated assumptions. Produce a sharpened, corrected plan.";
const SOLVER: &str = "You are the Solver. Execute the (critiqued) plan and produce the final, complete answer to the original query.";
const SUMMARIZER: &str = "You are the Summarizer. Combine the specialists' answers into one coherent, non-redundant final answer, resolving any disagreements.";
const EXPERT: &str = "You are the Expert. Produce a thorough, authoritative answer with full reasoning — optimize for correctness over brevity.";
const LEARNER: &str = "You are the Learner. Distill the Expert's reasoning into a compact, self-contained answer that preserves correctness.";
const REFLECTOR: &str = "You are the Reflector. Examine the current solution state, identify what remains uncertain, and decide the next refinement (or that it is complete).";
const TOOL_CALLER: &str = "You are the Tool-Caller. Carry out the Reflector's directive and report the result. (In a tool-enabled deployment this role acts over a Text edge to a black-box tool agent.)";

/// Build the homogeneous latent topology for a pattern.
///
/// * `rounds` — recursion depth (the A₁→…→Aₙ→A₁ loop repeated `rounds` times;
///   only the final round's last role decodes).
/// * `n_specialists` — number of specialist roles for `Mixture` (clamped to
///   1..=8, matching the live `a2a_pattern_mixture` cap); ignored otherwise.
pub fn rmas_topology(pattern: RmasPattern, rounds: usize, n_specialists: usize) -> RmasTopology {
    match pattern {
        RmasPattern::Sequential => RmasTopology::homogeneous(
            vec![
                ("Planner".into(), PLANNER.into()),
                ("Critic".into(), CRITIC.into()),
                ("Solver".into(), SOLVER.into()),
            ],
            rounds,
        ),
        RmasPattern::Mixture => {
            let n = n_specialists.clamp(1, 8);
            let mut roles: Vec<(String, String)> = Vec::with_capacity(n + 1);
            for i in 1..=n {
                roles.push((
                    format!("Specialist-{i}"),
                    format!(
                        "You are Specialist {i} of {n}. Answer the query from your distinct angle, independently of the other specialists."
                    ),
                ));
            }
            roles.push(("Summarizer".into(), SUMMARIZER.into()));
            RmasTopology::homogeneous(roles, rounds)
        }
        RmasPattern::Distillation => RmasTopology::homogeneous(
            vec![
                ("Expert".into(), EXPERT.into()),
                ("Learner".into(), LEARNER.into()),
            ],
            rounds,
        ),
        RmasPattern::Deliberation => RmasTopology::homogeneous(
            vec![
                ("Reflector".into(), REFLECTOR.into()),
                ("Tool-Caller".into(), TOOL_CALLER.into()),
            ],
            rounds,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequential_has_three_reasoning_roles() {
        let t = rmas_topology(RmasPattern::Sequential, 2, 0);
        assert_eq!(t.n_roles(), 3);
        assert!(t.all_white_box());
        assert_eq!(t.roles[0].role.as_str(), "Planner");
        assert_eq!(t.roles[2].role.as_str(), "Solver");
    }

    #[test]
    fn mixture_scales_specialists_plus_summarizer() {
        let t = rmas_topology(RmasPattern::Mixture, 1, 3);
        assert_eq!(t.n_roles(), 4); // 3 specialists + summarizer
        assert_eq!(
            t.roles.last().expect("nonempty").role.as_str(),
            "Summarizer"
        );
        // Cap enforced.
        let capped = rmas_topology(RmasPattern::Mixture, 1, 50);
        assert_eq!(capped.n_roles(), 9); // 8 + summarizer
        let floored = rmas_topology(RmasPattern::Mixture, 1, 0);
        assert_eq!(floored.n_roles(), 2); // 1 + summarizer
    }

    #[test]
    fn distillation_and_deliberation_are_two_roles() {
        assert_eq!(rmas_topology(RmasPattern::Distillation, 1, 0).n_roles(), 2);
        assert_eq!(rmas_topology(RmasPattern::Deliberation, 1, 0).n_roles(), 2);
    }

    #[test]
    fn pattern_string_round_trips() {
        for p in [
            RmasPattern::Sequential,
            RmasPattern::Mixture,
            RmasPattern::Distillation,
            RmasPattern::Deliberation,
        ] {
            assert_eq!(RmasPattern::parse(p.as_str()), Some(p));
        }
        assert_eq!(RmasPattern::parse("nope"), None);
    }
}
