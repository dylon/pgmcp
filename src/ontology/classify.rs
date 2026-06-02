//! Deterministic-first **facet classification** for concepts.
//!
//! Phase-1 classifier for the topic-seeded concept layer. Two tiers, both
//! deterministic (no LLM — the LLM fallback for the `domain_concept` residue is
//! opt-in and lives in the concept cron, not here):
//!
//! 1. **Naming/keyword cues** ([`facet_from_text`]) — a priority-ordered rule
//!    table over the concept label's tokens. Pure + exhaustively testable. This
//!    is the "path/naming tokens" signal: a topic label is its c-TF-IDF keywords.
//! 2. **Code-signal refinement** ([`dominant_effect_facet`]) — when the cues are
//!    silent, the dominant shadow-ASR *effect* of the topic's symbols upgrades
//!    `unsafe`→`security` / `async`→`concurrency`. Best-effort; DB errors fall
//!    through to the cue result.
//!
//! Unresolved ⇒ [`Facet::DomainConcept`] (the honest catch-all; Phase 4 FCA and
//! the Phase 10 pattern-catalog migration refine structure later).

use sqlx::PgPool;

use crate::ontology::facet::Facet;

/// One cue rule: a facet and the lowercase substrings that select it. Order in
/// [`CUE_RULES`] is the priority — the first rule with a matching token wins, so
/// more-specific facets precede broader ones.
struct CueRule {
    facet: Facet,
    needles: &'static [&'static str],
}

/// Priority-ordered cue table. Matched against whitespace/punctuation-split
/// lowercase tokens of the concept label (substring match per token).
const CUE_RULES: &[CueRule] = &[
    CueRule {
        facet: Facet::Concurrency,
        needles: &[
            "lock",
            "mutex",
            "rwlock",
            "async",
            "await",
            "channel",
            "thread",
            "atomic",
            "concurren",
            "parallel",
            "semaphore",
            "deadlock",
            "spawn",
            "tokio",
            "rayon",
            "barrier",
            "condvar",
        ],
    },
    CueRule {
        facet: Facet::Security,
        needles: &[
            "auth",
            "crypto",
            "secret",
            "passwd",
            "password",
            "sanitiz",
            "inject",
            "vulnerab",
            "exploit",
            "encrypt",
            "decrypt",
            "tls",
            "ssl",
            "permission",
            "sandbox",
            "taint",
            "credential",
            "oauth",
            "jwt",
            "cve",
        ],
    },
    CueRule {
        facet: Facet::Protocol,
        needles: &[
            "protocol",
            "handshake",
            "grpc",
            "rpc",
            "http",
            "mpst",
            "cfsm",
            "session_type",
            "wire_format",
            "codec",
        ],
    },
    CueRule {
        facet: Facet::DataStructure,
        needles: &[
            "trie",
            "dawg",
            "automaton",
            "hashmap",
            "btree",
            "queue",
            "deque",
            "stack",
            "heap",
            "ringbuffer",
            "buffer",
            "bitset",
            "bloom",
            "lattice",
            "linkedlist",
            "hashset",
            "dictionary",
        ],
    },
    CueRule {
        facet: Facet::Algorithm,
        needles: &[
            "sort",
            "dijkstra",
            "levenshtein",
            "kmeans",
            "clustering",
            "traversal",
            "greedy",
            "backtrack",
            "memoiz",
            "pagerank",
            "viterbi",
            "knapsack",
            "fft",
            "gradient",
            "heuristic",
            "search_algo",
        ],
    },
    CueRule {
        facet: Facet::DesignPattern,
        needles: &[
            "visitor",
            "singleton",
            "observer",
            "decorator",
            "factory_pattern",
            "facade",
            "adapter_pattern",
            "memento",
            "flyweight",
            "proxy_pattern",
        ],
    },
    CueRule {
        facet: Facet::EngineeringPractice,
        needles: &[
            "unittest",
            "integration_test",
            "benchmark",
            "lint",
            "telemetry",
            "logging",
            "tracing",
            "profiling",
            "fuzz",
            "ci_pipeline",
            "coverage",
        ],
    },
    CueRule {
        facet: Facet::Architecture,
        needles: &[
            "architecture",
            "subsystem",
            "boundary",
            "topology",
            "layering",
        ],
    },
    CueRule {
        facet: Facet::Component,
        needles: &[
            "daemon",
            "service",
            "controller",
            "repository",
            "gateway",
            "scheduler",
            "dispatcher",
            "registry",
            "orchestrator",
            "supervisor",
        ],
    },
];

/// Tokenize a label into lowercase alphanumeric/underscore tokens, plus the
/// whole lowercased string (so multi-word needles like `session_type` match
/// against `wire_format` joins). Preallocated to the token count.
fn tokens(label: &str) -> Vec<String> {
    let lower = label.to_lowercase();
    let mut out: Vec<String> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();
    out.push(lower);
    out
}

/// Tier 1 — naming/keyword cues. `None` ⇒ no rule matched (caller falls through
/// to the code signal, then `DomainConcept`).
pub fn facet_from_text(label: &str) -> Option<Facet> {
    let toks = tokens(label);
    for rule in CUE_RULES {
        for needle in rule.needles {
            if toks.iter().any(|t| t.contains(needle)) {
                return Some(rule.facet);
            }
        }
    }
    None
}

/// Tier 2 — dominant shadow-ASR effect of the topic's symbols. Best-effort: any
/// DB error (or no effects) yields `None`. Maps `unsafe`→`Security`,
/// `async`→`Concurrency`; other effects don't pin a facet.
async fn dominant_effect_facet(pool: &PgPool, topic_id: i64) -> Option<Facet> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT se.effect, COUNT(*) AS c
         FROM chunk_topic_assignments cta
         JOIN file_chunks fc   ON fc.id = cta.chunk_id
         JOIN file_symbols fs  ON fs.file_id = fc.file_id
         JOIN symbol_effects se ON se.symbol_id = fs.id
         WHERE cta.topic_id = $1 AND cta.membership_score >= 0.05
         GROUP BY se.effect
         ORDER BY c DESC
         LIMIT 3",
    )
    .bind(topic_id)
    .fetch_all(pool)
    .await
    .ok()?;
    for (effect, _) in rows {
        let e = effect.to_lowercase();
        if e.contains("unsafe") {
            return Some(Facet::Security);
        }
        if e.contains("async") || e.contains("await") {
            return Some(Facet::Concurrency);
        }
    }
    None
}

/// Classify a topic-seeded concept: cues first, then the code signal, then the
/// `DomainConcept` catch-all. Always returns a facet (never errors — the signal
/// query is best-effort).
pub async fn classify_topic_concept(pool: &PgPool, topic_id: i64, label: &str) -> Facet {
    if let Some(f) = facet_from_text(label) {
        return f;
    }
    if let Some(f) = dominant_effect_facet(pool, topic_id).await {
        return f;
    }
    Facet::DomainConcept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cues_classify_obvious_labels() {
        assert_eq!(
            facet_from_text("mutex lock guard"),
            Some(Facet::Concurrency)
        );
        assert_eq!(
            facet_from_text("async runtime channel"),
            Some(Facet::Concurrency)
        );
        assert_eq!(facet_from_text("oauth token crypto"), Some(Facet::Security));
        assert_eq!(
            facet_from_text("persistent trie dawg"),
            Some(Facet::DataStructure)
        );
        assert_eq!(
            facet_from_text("levenshtein automaton"),
            Some(Facet::DataStructure)
        ); // 'automaton' (DS) before 'levenshtein' only if DS precedes algo
        assert_eq!(
            facet_from_text("dijkstra shortest path"),
            Some(Facet::Algorithm)
        );
        assert_eq!(
            facet_from_text("grpc handshake codec"),
            Some(Facet::Protocol)
        );
        assert_eq!(
            facet_from_text("background daemon service"),
            Some(Facet::Component)
        );
    }

    #[test]
    fn unmatched_label_is_none() {
        assert_eq!(facet_from_text("invoice billing ledger"), None);
        assert_eq!(facet_from_text(""), None);
    }

    #[test]
    fn priority_concurrency_beats_later_rules() {
        // "lock" (Concurrency) precedes any data-structure cue.
        assert_eq!(
            facet_from_text("lock-free queue"),
            Some(Facet::Concurrency),
            "Concurrency rule must win over DataStructure by priority"
        );
    }

    #[test]
    fn tokenizer_splits_and_lowercases() {
        let t = tokens("Async-Runtime.Channel");
        assert!(t.iter().any(|x| x == "async"));
        assert!(t.iter().any(|x| x == "runtime"));
        assert!(t.iter().any(|x| x == "channel"));
    }
}
