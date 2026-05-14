//! Built-in software pattern catalog metadata.
//!
//! The text in this module is intentionally compact: it is the local,
//! repo-bundled "card" layer that gives the agent stable labels,
//! paradigms, and source associations before any opted-in full-text source
//! imports run. Full article bodies are fetched into the local database at
//! runtime and are not committed to the repository.
//!
//! Entries are grouped into per-family submodules; `pattern_seeds()`
//! assembles them at call time. Adding a new pattern family is a matter of
//! creating a new file with a `pub(super) fn seeds() -> Vec<PatternSeed>`
//! and extending `pattern_seeds()` below.

mod anti_patterns;
mod aop;
mod api_design;
mod architecture;
mod automata;
mod code_smells;
mod concurrency;
mod data_engineering;
mod declarative;
mod deployment;
mod distributed_data;
mod functional;
mod gof;
mod idioms;
mod kubernetes;
mod ml_ai;
mod observability;
mod principles;
mod security;
mod solid_grasp;
mod sources;
mod testing;

#[derive(Debug, Clone, Copy)]
pub struct ParadigmSeed {
    pub slug: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub wikipedia_url: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct PatternSeed {
    pub slug: &'static str,
    pub name: &'static str,
    pub kind: &'static str,
    pub category: &'static str,
    pub summary: &'static str,
    pub intent: &'static str,
    pub problem: &'static str,
    pub solution: &'static str,
    pub consequences: &'static str,
    pub paradigms: &'static [&'static str],
    pub tags: &'static [&'static str],
    pub canonical_url: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct SourceDescriptor {
    pub source_family: &'static str,
    pub title: &'static str,
    pub url: &'static str,
    /// Fallback URLs tried in order if `url` fails. Supports `https://`,
    /// `http://`, and `file://` schemes. The `file://` scheme reads from
    /// the local filesystem and is the recommended way to point at
    /// `~/Papers/` PDFs when a paywall/SPA/dead-domain primary URL has no
    /// open web alternate.
    pub mirrors: &'static [&'static str],
    pub license_label: &'static str,
    pub source_type: &'static str,
    pub ingest_policy: &'static str,
    pub pattern_slugs: &'static [&'static str],
    pub tags: &'static [&'static str],
}

#[allow(clippy::too_many_arguments)]
pub(crate) const fn pat(
    slug: &'static str,
    name: &'static str,
    kind: &'static str,
    category: &'static str,
    summary: &'static str,
    intent: &'static str,
    problem: &'static str,
    solution: &'static str,
    consequences: &'static str,
    paradigms: &'static [&'static str],
    tags: &'static [&'static str],
    canonical_url: &'static str,
) -> PatternSeed {
    PatternSeed {
        slug,
        name,
        kind,
        category,
        summary,
        intent,
        problem,
        solution,
        consequences,
        paradigms,
        tags,
        canonical_url,
    }
}

pub fn paradigm_seeds() -> Vec<ParadigmSeed> {
    vec![
        ParadigmSeed {
            slug: "procedural_programming",
            name: "Procedural programming",
            description: "Procedure/module-oriented design with explicit control flow and state.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Procedural_programming",
        },
        ParadigmSeed {
            slug: "object_oriented_programming",
            name: "Object-oriented programming",
            description: "Design around objects, encapsulation, polymorphism, and responsibility assignment.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Object-oriented_programming",
        },
        ParadigmSeed {
            slug: "functional_programming",
            name: "Functional programming",
            description: "Design with pure functions, immutable data, composition, and algebraic abstractions.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Functional_programming",
        },
        ParadigmSeed {
            slug: "logic_programming",
            name: "Logic programming",
            description: "Declarative design with facts, rules, relations, unification, and search.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Logic_programming",
        },
        ParadigmSeed {
            slug: "event_driven_programming",
            name: "Event-driven programming",
            description: "Design around event production, delivery, handlers, streams, and reactions.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Event-driven_programming",
        },
        ParadigmSeed {
            slug: "concurrent_programming",
            name: "Concurrent programming",
            description: "Design for overlapping tasks, coordination, cancellation, and shared-resource safety.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Concurrent_computing",
        },
        ParadigmSeed {
            slug: "parallel_programming",
            name: "Parallel programming",
            description: "Design for decomposing computation across cores, workers, and data partitions.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Parallel_computing",
        },
        ParadigmSeed {
            slug: "aspect_oriented_programming",
            name: "Aspect-oriented programming",
            description: "Design for modularizing cross-cutting concerns with join points, pointcuts, and advice.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Aspect-oriented_programming",
        },
        ParadigmSeed {
            slug: "distributed_systems",
            name: "Distributed systems",
            description: "Design for networked services, partial failure, consistency, resilience, and operations.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Distributed_computing",
        },
        ParadigmSeed {
            slug: "reactive_programming",
            name: "Reactive programming",
            description: "Design around asynchronous data streams, observers, and propagation of change.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Reactive_programming",
        },
        ParadigmSeed {
            slug: "dataflow_programming",
            name: "Dataflow programming",
            description: "Computation as a directed graph of operators acting on flowing data.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Dataflow_programming",
        },
        ParadigmSeed {
            slug: "declarative_programming",
            name: "Declarative programming",
            description: "Express what the program should accomplish rather than how, via rules, queries, or descriptions.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Declarative_programming",
        },
        ParadigmSeed {
            slug: "actor_model",
            name: "Actor model",
            description: "Concurrent computation as isolated actors that communicate exclusively via asynchronous messages.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Actor_model",
        },
        ParadigmSeed {
            slug: "machine_learning_engineering",
            name: "Machine learning engineering",
            description: "Engineering practice of building, deploying, monitoring, and iterating on ML/AI systems in production.",
            wikipedia_url: "https://en.wikipedia.org/wiki/MLOps",
        },
        ParadigmSeed {
            slug: "formal_languages_and_automata",
            name: "Formal languages and automata",
            description: "Theory and practice of recognizers, generators, transducers, parsers, lexers, and string-index automata; covers finite/pushdown/tree/ω-automata, parser families, string-index data structures, edit-distance and approximate-matching automata, phonetic encoders, abstract machines, term rewriting, and probabilistic grammars.",
            wikipedia_url: "https://en.wikipedia.org/wiki/Automata_theory",
        },
    ]
}

pub fn pattern_seeds() -> Vec<PatternSeed> {
    let mut v = Vec::with_capacity(900);
    v.extend(gof::seeds());
    v.extend(solid_grasp::seeds());
    v.extend(principles::seeds());
    v.extend(functional::seeds());
    v.extend(concurrency::seeds());
    v.extend(architecture::seeds());
    v.extend(declarative::seeds());
    v.extend(anti_patterns::seeds());
    v.extend(code_smells::seeds());
    v.extend(security::seeds());
    v.extend(testing::seeds());
    v.extend(idioms::seeds());
    v.extend(aop::seeds());
    v.extend(observability::seeds());
    v.extend(deployment::seeds());
    v.extend(data_engineering::seeds());
    v.extend(api_design::seeds());
    v.extend(ml_ai::seeds());
    v.extend(distributed_data::seeds());
    v.extend(kubernetes::seeds());
    v.extend(automata::seeds());
    v
}

pub fn source_registry() -> Vec<SourceDescriptor> {
    sources::descriptors()
}

pub fn card_content(pattern: &PatternSeed) -> String {
    format!(
        "Pattern: {name}\nKind: {kind}\nCategory: {category}\nParadigms: {paradigms}\nTags: {tags}\nSummary: {summary}\nIntent: {intent}\nProblem: {problem}\nSolution: {solution}\nConsequences and tradeoffs: {consequences}\nCanonical URL: {url}\n",
        name = pattern.name,
        kind = pattern.kind,
        category = pattern.category,
        paradigms = pattern.paradigms.join(", "),
        tags = pattern.tags.join(", "),
        summary = pattern.summary,
        intent = pattern.intent,
        problem = pattern.problem,
        solution = pattern.solution,
        consequences = pattern.consequences,
        url = pattern.canonical_url
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn source_registry_references_seeded_patterns() {
        let slugs = pattern_seeds()
            .into_iter()
            .map(|p| p.slug)
            .collect::<HashSet<_>>();

        for source in source_registry() {
            for slug in source.pattern_slugs {
                assert!(
                    slugs.contains(slug),
                    "source {} references unknown pattern slug {}",
                    source.title,
                    slug
                );
            }
        }
    }

    #[test]
    fn patterns_reference_seeded_paradigms() {
        let paradigms = paradigm_seeds()
            .into_iter()
            .map(|p| p.slug)
            .collect::<HashSet<_>>();

        for pattern in pattern_seeds() {
            for paradigm in pattern.paradigms {
                assert!(
                    paradigms.contains(paradigm),
                    "pattern {} references unknown paradigm {}",
                    pattern.slug,
                    paradigm
                );
            }
        }
    }

    #[test]
    fn pattern_slugs_are_unique() {
        let mut seen = HashSet::new();
        for p in pattern_seeds() {
            assert!(seen.insert(p.slug), "duplicate pattern slug: {}", p.slug);
        }
    }

    #[test]
    fn paradigm_slugs_are_unique() {
        let mut seen = HashSet::new();
        for p in paradigm_seeds() {
            assert!(seen.insert(p.slug), "duplicate paradigm slug: {}", p.slug);
        }
    }

    #[test]
    fn pattern_kinds_are_valid() {
        let valid = HashSet::from(["pattern", "anti_pattern", "principle", "code_smell"]);
        for p in pattern_seeds() {
            assert!(
                valid.contains(p.kind),
                "pattern {} has invalid kind {}",
                p.slug,
                p.kind
            );
        }
    }
}
