//! Built-in developer-tool catalog metadata ("tool cards").
//!
//! This is the local, repo-bundled "card" layer for the user's installed
//! toolbox — formal-verification tools, profiling/benchmarking/debugging tools,
//! and security tooling — mirroring `src/patterns/` exactly in spirit. Each [`ToolSeed`] is a
//! compact, LLM-oriented description of one installed tool: what it does, when
//! to reach for it, how to invoke it *on this machine*, its strengths and
//! limitations, and cross-links to alternatives.
//!
//! Entries are grouped into per-family submodules; [`tool_seeds`] assembles
//! them at call time. Adding a tool family is a matter of creating a new file
//! with a `pub(super) fn seeds() -> Vec<ToolSeed>` and extending
//! [`tool_seeds`] below.
//!
//! The cards are seeded into the `tool_cards` table (migration v32,
//! `src/db/migrations/v32_toolbox_catalog.rs`) and surfaced through the
//! `toolbox_*` MCP tools (`src/mcp/tools/tool_toolbox.rs`). Embeddings are NOT
//! produced on the seed/write path — the embedding-migration cron
//! (`src/cron/embedding_migration.rs`) backfills the `embedding` column, the
//! established 1024d-direct pattern (see `durable_mandates` / `data_tables` /
//! the v31 graph-RAG tables).

mod auto_active;
mod debuggers_sanitizers;
mod diagramming_graph;
mod diagramming_plotting;
mod diagramming_render;
mod model_checkers;
mod profilers_cpu;
mod profilers_memory_cache;
mod proof_assistants;
mod rewriting_termination;
mod security_binary;
mod security_gpu_algebra;
mod security_scanning;
mod security_static;
mod smt_sat_atp;
mod system_monitors;
mod tracers_bench;

/// One installed developer tool, described as an LLM-retrievable "card".
///
/// All fields are compile-time string constants except `alternatives`
/// (a slice of sibling slugs). The field set deliberately differs from
/// `patterns::PatternSeed`: a *tool* card answers "what is this, when do I
/// reach for it, and how do I run it here", not "what design tension does this
/// resolve".
#[derive(Debug, Clone, Copy)]
pub struct ToolSeed {
    /// kebab-case unique id (`z3`, `cargo-flamegraph`, `valgrind-massif`).
    pub slug: &'static str,
    /// Display name (`Z3`, `TLA⁺ / TLC`).
    pub name: &'static str,
    /// `formal_verification` | `developer_tooling` | `security` (see [`ToolDomain`]).
    pub domain: &'static str,
    /// Tool class — references a seeded [`ToolCategorySeed`] slug.
    pub category: &'static str,
    /// One-line elevator pitch.
    pub summary: &'static str,
    /// Capability description.
    pub what_it_does: &'static str,
    /// The LLM-actionable field — "reach for this when…".
    pub when_to_use: &'static str,
    /// What it consumes / produces.
    pub inputs_outputs: &'static str,
    /// Canonical command(s), grounded on this machine.
    pub invocation: &'static str,
    /// Where it wins.
    pub strengths: &'static str,
    /// When NOT to use it / known gaps.
    pub limitations: &'static str,
    /// Related/competing tool slugs (cross-links; validated by a referential test).
    pub alternatives: &'static [&'static str],
    /// How it is installed here (pacman pkg / opam / elan / PATH).
    pub availability: &'static str,
    /// Canonical reference URL.
    pub docs_url: &'static str,
}

/// A tool category — the closed-but-growing taxonomy axis, mirroring
/// `patterns::ParadigmSeed`. Seeded into `tool_categories`; every
/// [`ToolSeed::category`] must reference one (referential test below).
#[derive(Debug, Clone, Copy)]
pub struct ToolCategorySeed {
    pub slug: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub domain: &'static str,
}

/// The three top-level domains. Closed set: the `tool_cards.domain` CHECK
/// constraint is built from [`ToolDomain::sql_in_list`] (created by the v32
/// migration, widened to admit `security` by the v33 migration), and
/// [`tests::tool_domains_are_valid`] pins every seed to this set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDomain {
    FormalVerification,
    DeveloperTooling,
    Security,
    Diagramming,
}

impl ToolDomain {
    pub const ALL: [ToolDomain; 4] = [
        ToolDomain::FormalVerification,
        ToolDomain::DeveloperTooling,
        ToolDomain::Security,
        ToolDomain::Diagramming,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ToolDomain::FormalVerification => "formal_verification",
            ToolDomain::DeveloperTooling => "developer_tooling",
            ToolDomain::Security => "security",
            ToolDomain::Diagramming => "diagramming",
        }
    }

    /// SQL `IN (...)` value list for the `tool_cards.domain` CHECK constraint
    /// (ADR-003 idiom: the Rust enum is the single source of truth for the
    /// closed vocabulary). Consumed by `v32_toolbox_catalog::apply`.
    pub fn sql_in_list() -> String {
        Self::ALL
            .iter()
            .map(|d| format!("'{}'", d.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) const fn tool(
    slug: &'static str,
    name: &'static str,
    domain: &'static str,
    category: &'static str,
    summary: &'static str,
    what_it_does: &'static str,
    when_to_use: &'static str,
    inputs_outputs: &'static str,
    invocation: &'static str,
    strengths: &'static str,
    limitations: &'static str,
    alternatives: &'static [&'static str],
    availability: &'static str,
    docs_url: &'static str,
) -> ToolSeed {
    ToolSeed {
        slug,
        name,
        domain,
        category,
        summary,
        what_it_does,
        when_to_use,
        inputs_outputs,
        invocation,
        strengths,
        limitations,
        alternatives,
        availability,
        docs_url,
    }
}

/// Convenience constructors for the three domain string literals, so per-family
/// files read `FV` / `DEV` / `SEC` instead of repeating the slug.
pub(crate) const FV: &str = ToolDomain::FormalVerification.as_str();
pub(crate) const DEV: &str = ToolDomain::DeveloperTooling.as_str();
pub(crate) const SEC: &str = ToolDomain::Security.as_str();
pub(crate) const DIA: &str = ToolDomain::Diagramming.as_str();

pub fn tool_category_seeds() -> Vec<ToolCategorySeed> {
    let fv = ToolDomain::FormalVerification.as_str();
    let dev = ToolDomain::DeveloperTooling.as_str();
    let sec = ToolDomain::Security.as_str();
    let dia = ToolDomain::Diagramming.as_str();
    vec![
        // ---- formal_verification ----
        ToolCategorySeed {
            slug: "proof_assistant",
            name: "Proof assistant",
            description: "Interactive / dependently-typed theorem prover producing machine-checked proofs.",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "coq_library",
            name: "Rocq/Coq library",
            description: "Reusable Rocq/Coq library providing reasoning infrastructure (interaction trees, separation logic, …).",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "auto_active_verifier",
            name: "Auto-active verifier",
            description: "Source + annotation (pre/post/invariant) verifier discharged largely automatically via SMT.",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "smt_solver",
            name: "SMT solver",
            description: "Satisfiability-modulo-theories solver for decidable first-order fragments.",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "sat_solver",
            name: "SAT solver",
            description: "Boolean satisfiability (CNF/DIMACS) solver.",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "first_order_atp",
            name: "First-order ATP",
            description: "Automated first-order theorem prover (resolution / superposition / saturation).",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "model_checker",
            name: "Model checker",
            description: "Explicit-state, symbolic, or temporal-logic checker for concurrent / reactive systems.",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "timed_model_checker",
            name: "Timed model checker",
            description: "Model checker for real-time systems with clocks / timed automata.",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "process_algebra",
            name: "Process-algebra toolset",
            description: "Process-algebraic specification + behavioural-equivalence verification toolset.",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "probabilistic_model_checker",
            name: "Probabilistic model checker",
            description: "Model checker for stochastic systems (Markov chains / MDPs).",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "termination_complexity",
            name: "Termination / complexity / confluence prover",
            description: "Automated termination, complexity, and confluence analysis for term-rewriting systems and programs.",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "rewriting_semantics",
            name: "Rewriting / semantics framework",
            description: "Rewriting-logic / semantics framework for defining and executing formal language semantics.",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "security_protocol_verifier",
            name: "Security-protocol verifier",
            description: "Symbolic (Dolev-Yao) cryptographic-protocol verifier.",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "gpu_verifier",
            name: "GPU kernel verifier",
            description: "Verifier for GPU kernels (data-race / barrier-divergence freedom).",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "computer_algebra",
            name: "Computer-algebra system",
            description: "Symbolic-mathematics / computer-algebra system (verification-adjacent).",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "logic_programming",
            name: "Logic-programming system",
            description: "Prolog-family logic-programming system for relational / constraint reasoning.",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "optimization_solver",
            name: "Optimization solver",
            description: "Mathematical-optimization solver (LP / SDP / MIP).",
            domain: fv,
        },
        ToolCategorySeed {
            slug: "system_modeling",
            name: "System modeling & simulation",
            description: "Dynamical-system modeling and simulation: equation-based (Modelica) \
                           component models, state-space / transfer-function control models, \
                           ODE/DAE integration, and stochastic (Markov) processes.",
            domain: fv,
        },
        // ---- developer_tooling ----
        ToolCategorySeed {
            slug: "cpu_profiler",
            name: "CPU profiler",
            description: "Sampling / instrumenting profiler attributing on-CPU time to functions.",
            domain: dev,
        },
        ToolCategorySeed {
            slug: "profile_visualization",
            name: "Profile visualization",
            description: "Renders / visualizes profiler output (flame graphs, call graphs, annotated source).",
            domain: dev,
        },
        ToolCategorySeed {
            slug: "memory_profiler",
            name: "Memory profiler",
            description: "Heap / allocation profiler attributing memory use and detecting leaks.",
            domain: dev,
        },
        ToolCategorySeed {
            slug: "cache_call_profiler",
            name: "Cache / call-graph profiler",
            description: "Instruction / cache / branch / call-graph profiler via CPU simulation.",
            domain: dev,
        },
        ToolCategorySeed {
            slug: "ebpf_tracer",
            name: "eBPF tracer",
            description: "eBPF-based dynamic tracer for kernel / user events, latency, and stacks.",
            domain: dev,
        },
        ToolCategorySeed {
            slug: "syscall_tracer",
            name: "Syscall / library tracer",
            description: "Traces system calls, signals, and library calls of a running process.",
            domain: dev,
        },
        ToolCategorySeed {
            slug: "cli_benchmark",
            name: "CLI benchmarking",
            description: "Command-line wall-clock benchmarking harness.",
            domain: dev,
        },
        ToolCategorySeed {
            slug: "benchmark_library",
            name: "Benchmark library",
            description: "In-code microbenchmark framework / library.",
            domain: dev,
        },
        ToolCategorySeed {
            slug: "debugger",
            name: "Debugger",
            description: "Interactive source-level debugger.",
            domain: dev,
        },
        ToolCategorySeed {
            slug: "sanitizer",
            name: "Sanitizer / dynamic error detector",
            description: "Runtime error detector — compiler-instrumentation sanitizers (ASan/TSan/…) or Valgrind tools — catching memory / UB / data-race / leak bugs at run time.",
            domain: dev,
        },
        ToolCategorySeed {
            slug: "async_debugger",
            name: "Async-runtime debugger",
            description: "Runtime introspection for async executors (task stalls, busy/idle, poll times).",
            domain: dev,
        },
        ToolCategorySeed {
            slug: "jvm_profiler",
            name: "JVM profiler",
            description: "Profiler / monitor for the Java Virtual Machine.",
            domain: dev,
        },
        ToolCategorySeed {
            slug: "system_monitor",
            name: "System monitor",
            description: "Live system / resource monitor (CPU, memory, I/O, network, hardware counters).",
            domain: dev,
        },
        // ---- security ----
        ToolCategorySeed {
            slug: "secret_scanner",
            name: "Secret / credential scanner",
            description: "Detects committed secrets, API keys, and credential leaks in source, git history, or filesystems.",
            domain: sec,
        },
        ToolCategorySeed {
            slug: "secrets_management",
            name: "Secrets management / encryption",
            description: "Encrypts secrets at rest (age/PGP/KMS) so they are never committed in cleartext.",
            domain: sec,
        },
        ToolCategorySeed {
            slug: "sast",
            name: "Static application security testing (SAST)",
            description: "Source-level static analyzer flagging security-relevant defects (memory/injection/CERT/bugprone classes).",
            domain: sec,
        },
        ToolCategorySeed {
            slug: "supply_chain_audit",
            name: "Dependency / supply-chain audit",
            description: "Scans dependency graphs / SBOMs for known advisories, banned crates, and license/source policy violations.",
            domain: sec,
        },
        ToolCategorySeed {
            slug: "vulnerability_scanner",
            name: "Vulnerability scanner",
            description: "Scans images, filesystems, repos, or endpoints for known CVEs and misconfigurations.",
            domain: sec,
        },
        ToolCategorySeed {
            slug: "iac_container_security",
            name: "IaC / container config security",
            description: "Lints infrastructure & container definitions (Dockerfiles, IaC) for security best practice.",
            domain: sec,
        },
        ToolCategorySeed {
            slug: "tls_ssh_audit",
            name: "TLS / SSH configuration audit",
            description: "Audits transport crypto configuration (cipher suites, protocol versions, host-key/cert posture).",
            domain: sec,
        },
        ToolCategorySeed {
            slug: "network_recon",
            name: "Network scanner / recon utility",
            description: "Host/port/service discovery and raw socket connection/relay utilities for network reconnaissance.",
            domain: sec,
        },
        ToolCategorySeed {
            slug: "fuzzer",
            name: "Coverage-guided fuzzer",
            description: "Mutates inputs under coverage feedback to drive a target into crashes / sanitizer trips.",
            domain: sec,
        },
        ToolCategorySeed {
            slug: "malware_scanner",
            name: "Malware / antivirus scanner",
            description: "Signature/pattern malware detection for files and directories.",
            domain: sec,
        },
        ToolCategorySeed {
            slug: "binary_analysis",
            name: "Binary / ELF inspection & hardening",
            description: "Disassembly, symbol/ELF inspection, hardening checks, and binary patching.",
            domain: sec,
        },
        ToolCategorySeed {
            slug: "reverse_engineering",
            name: "Reverse-engineering framework",
            description: "Interactive disassembler / analysis platform for binaries.",
            domain: sec,
        },
        ToolCategorySeed {
            slug: "forensics",
            name: "Digital forensics",
            description: "File carving, disk-image forensics, and metadata extraction.",
            domain: sec,
        },
        // ---- diagramming ----
        ToolCategorySeed {
            slug: "graph_layout",
            name: "Graph / network layout",
            description: "Automatic node-edge graph drawing — force-directed, hierarchical, radial, and circular layout of relationships.",
            domain: dia,
        },
        ToolCategorySeed {
            slug: "uml_architecture",
            name: "UML / architecture / sequence (diagrams-as-code)",
            description: "Semantic software diagrams generated from text — UML, sequence, C4/architecture, ER, and flowcharts.",
            domain: dia,
        },
        ToolCategorySeed {
            slug: "scientific_plotting",
            name: "Scientific plotting & charting",
            description: "Data- and function-plotting / charting for scientific, statistical, and engineering visualization.",
            domain: dia,
        },
        ToolCategorySeed {
            slug: "diagram_language",
            name: "Vector picture / diagram language",
            description: "Programmatic vector-graphics / picture languages that draw from coordinates, paths, and macros.",
            domain: dia,
        },
        ToolCategorySeed {
            slug: "ascii_diagram",
            name: "ASCII-art → diagram",
            description: "Converts ASCII / Unicode line-art into rendered SVG / PNG diagrams.",
            domain: dia,
        },
        ToolCategorySeed {
            slug: "diagram_conversion",
            name: "Diagram format conversion & rendering",
            description: "Renders and converts diagram / vector formats (SVG / PDF / PNG / EPS, .fig / .odg, DVI) for headless pipelines.",
            domain: dia,
        },
        ToolCategorySeed {
            slug: "circuit_diagram",
            name: "Circuit / EDA schematics",
            description: "Electronic-circuit schematic capture and EDA export — schematic / PCB, netlists, ERC / DRC.",
            domain: dia,
        },
        ToolCategorySeed {
            slug: "protocol_data_diagram",
            name: "Protocol / packet / timing diagrams",
            description: "Byte-field / packet-layout and digital-timing / waveform diagrams for protocol and hardware work.",
            domain: dia,
        },
    ]
}

pub fn tool_seeds() -> Vec<ToolSeed> {
    let mut v = Vec::with_capacity(210);
    v.extend(proof_assistants::seeds());
    v.extend(auto_active::seeds());
    v.extend(smt_sat_atp::seeds());
    v.extend(model_checkers::seeds());
    v.extend(rewriting_termination::seeds());
    v.extend(security_gpu_algebra::seeds());
    v.extend(profilers_cpu::seeds());
    v.extend(profilers_memory_cache::seeds());
    v.extend(tracers_bench::seeds());
    v.extend(debuggers_sanitizers::seeds());
    v.extend(system_monitors::seeds());
    v.extend(security_static::seeds());
    v.extend(security_scanning::seeds());
    v.extend(security_binary::seeds());
    v.extend(diagramming_graph::seeds());
    v.extend(diagramming_render::seeds());
    v.extend(diagramming_plotting::seeds());
    v
}

/// The text embedded for semantic retrieval and hashed for staleness
/// detection. Covers exactly the prose fields the embedding-migration cron's
/// `text_select` concatenates for `tool_cards` (see
/// `src/cron/embedding_migration.rs`) — keep the two field sets in sync so a
/// prose edit both re-hashes (NULL-ing the stale vector in `upsert_tool_card`)
/// and re-embeds to the same content.
pub fn card_content(t: &ToolSeed) -> String {
    format!(
        "Tool: {name}\nSummary: {summary}\nWhat it does: {what_it_does}\nWhen to use: {when_to_use}\nInputs and outputs: {inputs_outputs}\nInvocation: {invocation}\nStrengths: {strengths}\nLimitations: {limitations}\nAvailability: {availability}\n",
        name = t.name,
        summary = t.summary,
        what_it_does = t.what_it_does,
        when_to_use = t.when_to_use,
        inputs_outputs = t.inputs_outputs,
        invocation = t.invocation,
        strengths = t.strengths,
        limitations = t.limitations,
        availability = t.availability,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn tool_slugs_are_unique() {
        let mut seen = HashSet::new();
        for t in tool_seeds() {
            assert!(seen.insert(t.slug), "duplicate tool slug: {}", t.slug);
        }
    }

    #[test]
    fn tool_category_slugs_are_unique() {
        let mut seen = HashSet::new();
        for c in tool_category_seeds() {
            assert!(seen.insert(c.slug), "duplicate category slug: {}", c.slug);
        }
    }

    #[test]
    fn tool_domains_are_valid() {
        let valid = ToolDomain::ALL
            .iter()
            .map(|d| d.as_str())
            .collect::<HashSet<_>>();
        for t in tool_seeds() {
            assert!(
                valid.contains(t.domain),
                "tool {} has invalid domain {}",
                t.slug,
                t.domain
            );
        }
        for c in tool_category_seeds() {
            assert!(
                valid.contains(c.domain),
                "category {} has invalid domain {}",
                c.slug,
                c.domain
            );
        }
    }

    #[test]
    fn tools_reference_seeded_categories() {
        let categories = tool_category_seeds()
            .into_iter()
            .map(|c| c.slug)
            .collect::<HashSet<_>>();
        for t in tool_seeds() {
            assert!(
                categories.contains(t.category),
                "tool {} references unknown category {}",
                t.slug,
                t.category
            );
        }
    }

    #[test]
    fn tools_match_their_category_domain() {
        let cat_domain = tool_category_seeds()
            .into_iter()
            .map(|c| (c.slug, c.domain))
            .collect::<std::collections::HashMap<_, _>>();
        for t in tool_seeds() {
            if let Some(&dom) = cat_domain.get(t.category) {
                assert_eq!(
                    dom, t.domain,
                    "tool {} domain {} disagrees with category {} domain {}",
                    t.slug, t.domain, t.category, dom
                );
            }
        }
    }

    #[test]
    fn tool_alternatives_reference_seeded_tools() {
        let slugs = tool_seeds()
            .into_iter()
            .map(|t| t.slug)
            .collect::<HashSet<_>>();
        for t in tool_seeds() {
            for alt in t.alternatives {
                assert!(
                    slugs.contains(alt),
                    "tool {} lists unknown alternative {}",
                    t.slug,
                    alt
                );
                assert_ne!(
                    *alt, t.slug,
                    "tool {} lists itself as an alternative",
                    t.slug
                );
            }
        }
    }

    #[test]
    fn domain_sql_in_list_is_pinned() {
        // ADR-003: the v32 migration builds the CHECK from this; pin it so a new
        // domain can't silently pass the Rust side while the DB rejects it.
        assert_eq!(
            ToolDomain::sql_in_list(),
            "'formal_verification', 'developer_tooling', 'security', 'diagramming'"
        );
    }
}
