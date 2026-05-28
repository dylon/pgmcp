//! Tier-2 **Recursive-TextMAS** engine (ADR-009; Yang et al. arXiv:2604.25917
//! §5). Loop any collaboration pattern for n rounds, threading round r's output
//! into round r+1's input, with an optional convergence marker for early stop.
//! This unifies the recursion the sequential/deliberation tools do bespoke and
//! lets the otherwise single-shot patterns (mixture, distillation) recurse too.
//!
//! Pure orchestration, generic over an async `one_round` closure that performs
//! the actual peer calls — so the engine is unit-tested with a mock and adds no
//! GPU/latent dependency: black-box Claude/Codex peers participate here (the
//! latent speed/token savings of true RecursiveMAS need white-box models and
//! live only in Track-B Tier-3, `src/rmas`).
//!
//! Staged-engine `#![allow(dead_code)]` (the same posture as `src/csm` pre-Phase-2
//! and the `fuzzy`/`wfst` modules): the engine is complete + unit-tested + config-
//! plumbed (`[a2a.recursion]`), and is adopted per-pattern when multi-round is
//! enabled. `sequential`/`deliberation` already recurse via their own bespoke
//! loops; this unifies the primitive and extends it to the single-shot patterns.

#![allow(dead_code)]

use std::future::Future;

/// Hard cap on rounds (mirrors `dispatcher::MAX_RECURSION_ROUNDS`).
pub const MAX_RECURSION_ROUNDS: u32 = 10;

/// How round r's output is threaded into round r+1's input context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarryPolicy {
    /// Carry the concatenated per-turn outputs so far.
    FullTranscript,
    /// Carry only the latest round's final answer (default).
    FinalAnswerOnly,
    /// Carry a truncated summary of the latest final answer.
    Summarized,
}

impl CarryPolicy {
    pub fn parse(s: &str) -> CarryPolicy {
        match s {
            "full_transcript" => CarryPolicy::FullTranscript,
            "summarized" => CarryPolicy::Summarized,
            _ => CarryPolicy::FinalAnswerOnly,
        }
    }
}

/// Per-call recursion settings.
pub struct RecursionConfig {
    pub rounds: u32,
    pub carry: CarryPolicy,
    /// If the round's final answer contains this marker, stop early (converged).
    pub converge_marker: Option<String>,
}

/// What one round produced.
pub struct RoundResult {
    pub final_answer: String,
    pub converged: bool,
    pub turns: Vec<serde_json::Value>,
}

/// The whole recursive run.
pub struct RecursiveOutcome {
    pub rounds_executed: u32,
    pub converged: bool,
    pub final_answer: String,
    pub transcript: Vec<serde_json::Value>,
}

const SUMMARY_CAP: usize = 512;

/// Run `one_round` up to `cfg.rounds` times (clamped to [1, MAX]), threading the
/// carry-over context per [`CarryPolicy`] and stopping early on convergence.
pub async fn run_recursive<F, Fut>(
    cfg: &RecursionConfig,
    base: &str,
    mut one_round: F,
) -> RecursiveOutcome
where
    F: FnMut(u32, String) -> Fut,
    Fut: Future<Output = RoundResult>,
{
    let rounds = cfg.rounds.clamp(1, MAX_RECURSION_ROUNDS);
    let mut carried = base.to_string();
    let mut transcript: Vec<serde_json::Value> = Vec::new();
    let mut final_answer = String::new();
    let mut converged = false;
    let mut executed = 0u32;

    for r in 0..rounds {
        let res = one_round(r, carried.clone()).await;
        executed += 1;
        transcript.extend(res.turns);
        final_answer = res.final_answer;

        let marker_hit = cfg
            .converge_marker
            .as_deref()
            .is_some_and(|m| !m.is_empty() && final_answer.contains(m));
        if res.converged || marker_hit {
            converged = true;
            break;
        }

        if r + 1 < rounds {
            carried = match cfg.carry {
                CarryPolicy::FinalAnswerOnly => {
                    format!("{base}\n\n[Prior round result]\n{final_answer}")
                }
                CarryPolicy::Summarized => {
                    let s: String = final_answer.chars().take(SUMMARY_CAP).collect();
                    format!("{base}\n\n[Prior round summary]\n{s}")
                }
                CarryPolicy::FullTranscript => {
                    let rendered = transcript
                        .iter()
                        .filter_map(|t| t.get("output").and_then(|v| v.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n---\n");
                    format!("{base}\n\n[Transcript so far]\n{rendered}")
                }
            };
        }
    }

    RecursiveOutcome {
        rounds_executed: executed,
        converged,
        final_answer,
        transcript,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn converges_early_and_threads_carryover() {
        let cfg = RecursionConfig {
            rounds: 5,
            carry: CarryPolicy::FinalAnswerOnly,
            converge_marker: None,
        };
        let seen = std::cell::RefCell::new(Vec::<String>::new());
        let out = run_recursive(&cfg, "Q", |r, carried| {
            seen.borrow_mut().push(carried);
            async move {
                RoundResult {
                    final_answer: format!("ans{r}"),
                    converged: r == 2,
                    turns: vec![json!({ "round": r, "output": format!("ans{r}") })],
                }
            }
        })
        .await;
        assert_eq!(out.rounds_executed, 3); // rounds 0,1,2 then converge
        assert!(out.converged);
        assert_eq!(out.final_answer, "ans2");
        let s = seen.borrow();
        assert_eq!(s[0], "Q");
        assert!(s[1].contains("ans0"), "round 1 carries round 0's answer");
        assert!(s[2].contains("ans1"));
        assert_eq!(out.transcript.len(), 3);
    }

    #[tokio::test]
    async fn runs_all_rounds_without_convergence() {
        let cfg = RecursionConfig {
            rounds: 3,
            carry: CarryPolicy::Summarized,
            converge_marker: Some("CONVERGED".to_string()),
        };
        let out = run_recursive(&cfg, "Q", |r, _| async move {
            RoundResult {
                final_answer: format!("r{r}"),
                converged: false,
                turns: vec![],
            }
        })
        .await;
        assert_eq!(out.rounds_executed, 3);
        assert!(!out.converged);
        assert_eq!(out.final_answer, "r2");
    }

    #[tokio::test]
    async fn marker_triggers_convergence() {
        let cfg = RecursionConfig {
            rounds: 5,
            carry: CarryPolicy::FinalAnswerOnly,
            converge_marker: Some("DONE".to_string()),
        };
        let out = run_recursive(&cfg, "Q", |r, _| async move {
            RoundResult {
                final_answer: if r == 1 {
                    "all DONE".into()
                } else {
                    "more".into()
                },
                converged: false,
                turns: vec![],
            }
        })
        .await;
        assert_eq!(out.rounds_executed, 2);
        assert!(out.converged);
    }
}
