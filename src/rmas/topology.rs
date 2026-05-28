//! The latent-loop topology (ADR-009 Tier-3 v1). For the *homogeneous* loop, N
//! roles all run on one resident backbone, each with its own RecursiveLink +
//! system prompt; the loop is A1→A2→…→AN→A1 for `rounds` rounds, and only the
//! final round's last role decodes to text (the rest stay latent). Pure data +
//! schedule logic — no GPU — so the orchestration is unit-tested independently
//! of the backbone.

use crate::csm::role::Role;

/// One role in the latent loop.
#[derive(Debug, Clone)]
pub struct RoleSpec {
    pub role: Role,
    pub system_prompt: String,
    /// White-box (latent-capable local backbone) vs black-box (text-only).
    /// In the homogeneous v1 every role is white-box; a black-box role would
    /// force a text-decode boundary (the `csm::media` discipline).
    pub white_box: bool,
}

/// The loop's roles + round count.
#[derive(Debug, Clone)]
pub struct RmasTopology {
    pub roles: Vec<RoleSpec>,
    pub rounds: usize,
}

impl RmasTopology {
    /// Build a homogeneous topology from `(role_name, system_prompt)` pairs.
    pub fn homogeneous(role_prompts: Vec<(String, String)>, rounds: usize) -> Self {
        let roles = role_prompts
            .into_iter()
            .map(|(name, prompt)| RoleSpec {
                role: Role::new(name),
                system_prompt: prompt,
                white_box: true,
            })
            .collect();
        RmasTopology {
            roles,
            rounds: rounds.max(1),
        }
    }

    pub fn n_roles(&self) -> usize {
        self.roles.len()
    }

    /// All white-box (latent loop fits entirely on the local backbone)?
    pub fn all_white_box(&self) -> bool {
        self.roles.iter().all(|r| r.white_box)
    }

    /// The ordered `(round, role_index)` hop schedule. The last entry is the
    /// decode-to-text hop; all earlier hops stay in latent space.
    pub fn schedule(&self) -> Vec<(usize, usize)> {
        let mut s = Vec::with_capacity(self.rounds * self.roles.len());
        for round in 0..self.rounds {
            for i in 0..self.roles.len() {
                s.push((round, i));
            }
        }
        s
    }

    /// Is `(round, role_idx)` the final hop (the one that decodes to text)?
    pub fn is_final_hop(&self, round: usize, role_idx: usize) -> bool {
        !self.roles.is_empty() && round + 1 == self.rounds && role_idx + 1 == self.roles.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topo() -> RmasTopology {
        RmasTopology::homogeneous(
            vec![
                ("Planner".into(), "You plan.".into()),
                ("Critic".into(), "You critique.".into()),
                ("Solver".into(), "You solve.".into()),
            ],
            2,
        )
    }

    #[test]
    fn schedule_visits_every_role_each_round() {
        let t = topo();
        let s = t.schedule();
        assert_eq!(s.len(), 6); // 2 rounds × 3 roles
        assert_eq!(s.first(), Some(&(0, 0)));
        assert_eq!(s.last(), Some(&(1, 2)));
    }

    #[test]
    fn only_the_last_hop_of_the_last_round_decodes() {
        let t = topo();
        assert!(t.is_final_hop(1, 2));
        assert!(!t.is_final_hop(0, 2)); // last role, but not last round
        assert!(!t.is_final_hop(1, 0)); // last round, but not last role
    }

    #[test]
    fn rounds_clamped_to_at_least_one() {
        let t = RmasTopology::homogeneous(vec![("A".into(), "p".into())], 0);
        assert_eq!(t.rounds, 1);
        assert!(t.all_white_box());
    }
}
