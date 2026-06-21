//! Parameter types for the **tape verbs** — the agent-facing MCP surface over
//! the context-tape paging substrate (Phase 4).
//!
//! These nine verbs (`tape_get` / `tape_put` / `tape_peek` / `tape_slice` /
//! `tape_grep` / `tape_fuzzy` / `tape_semantic` / `tape_list` / `tape_stat`)
//! are the **black-box-legal**, safe addressable surface any agent (Claude,
//! Codex, …) may call: they are *analytical* (no shell, no code execution),
//! they never write the user's source files, and the durable corpus is
//! READ-ONLY (reads hydrate from it; writes target the per-tree `TapeStore`
//! only). Every verb is scoped to a `tree` — the recursion-tree id
//! (`RlmFrame.root_task_id`, or a fresh UUID for standalone use) — so two
//! concurrent runs never collide in the backing store.
//!
//! Extracted to its own file per the `params/mod.rs` per-domain split; the
//! structs are re-exported by `params/mod.rs` (and transitively by `server.rs`)
//! so `crate::mcp::server::Tape*Params` resolves for the tool body files and
//! the `dispatch_tool!` / CLI paths.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

/// `tape_get` — fetch one page's situated bytes (resident hot/OOC cascade, else
/// hydrate the READ-ONLY corpus and admit it clean).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TapeGetParams {
    #[schemars(
        description = "Recursion-tree scope (the per-tree TapeStore namespace == RlmFrame.root_task_id). \
                       Required; pass a fresh UUID for standalone use."
    )]
    pub tree: String,
    #[schemars(
        description = "Page address — the data-plane path string (== PageAddress::to_path()), e.g. \
                       'corpus/chunk/42', 'memory/obs/7', 'scratch/<tree-uuid>/<hex-slot>'."
    )]
    pub address: String,
}

/// `tape_put` — stage bytes into the per-tree store as DIRTY. Omitting
/// `address` mints a fresh `Scratch` slot. Never writes the user's files; the
/// corpus is read-only (write-back promotion is gated off by default).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TapePutParams {
    #[schemars(description = "Recursion-tree scope (the per-tree TapeStore namespace). Required.")]
    pub tree: String,
    #[schemars(
        description = "Target page address (== PageAddress::to_path()). OMIT to mint a fresh \
                       tree-local Scratch slot; the new address is returned in the response."
    )]
    pub address: Option<String>,
    #[schemars(description = "The page content (UTF-8 text) to stage.")]
    pub content: String,
    #[schemars(
        description = "Request write-back promotion into durable memory (memory_observations only). \
                       Honored ONLY if the daemon's [tape] allow_promotion is enabled AND the address \
                       is an existing observation; otherwise the bytes stay staged in the tree store. \
                       Default false."
    )]
    pub promote: Option<bool>,
}

/// `tape_peek` — a cheap head/size probe over a page WITHOUT materializing its
/// full content (mirrors the RLM `peek`).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TapePeekParams {
    #[schemars(description = "Recursion-tree scope (the per-tree TapeStore namespace). Required.")]
    pub tree: String,
    #[schemars(description = "Page address (== PageAddress::to_path()).")]
    pub address: String,
    #[schemars(
        description = "Number of leading bytes to return as the head preview (default 256). \
                       Clamped to the page size; truncated on a UTF-8 char boundary."
    )]
    pub bytes: Option<usize>,
}

/// `tape_slice` — positional range scan over the per-tree store, in address
/// (key) order, between two address paths.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TapeSliceParams {
    #[schemars(description = "Recursion-tree scope (the per-tree TapeStore namespace). Required.")]
    pub tree: String,
    #[schemars(
        description = "Inclusive low bound — a page address path (== PageAddress::to_path()). \
                       The scan visits resident pages with key >= lo."
    )]
    pub lo: String,
    #[schemars(
        description = "Inclusive high bound — a page address path. The scan stops at key > hi. \
                       If lo > hi the range is empty."
    )]
    pub hi: String,
    #[schemars(
        description = "Cap on the number of pages returned (default 64). A scan that hits the cap \
                       sets truncated=true."
    )]
    pub max_pages: Option<usize>,
}

/// `tape_grep` — substring search. `tree` scope uses the per-tree store's
/// substring index; `corpus` scope resolves matching chunks from the READ-ONLY
/// corpus; `both` unions them.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TapeGrepParams {
    #[schemars(description = "Recursion-tree scope (the per-tree TapeStore namespace). Required.")]
    pub tree: String,
    #[schemars(description = "The substring / pattern to match against page content.")]
    pub pattern: String,
    #[schemars(
        description = "Where to search: 'tree' (resident pages, default), 'corpus' (the READ-ONLY \
                       indexed corpus), or 'both'."
    )]
    pub scope: Option<String>,
    #[schemars(
        description = "Optional project name to scope a corpus-scope grep (ignored for tree scope)."
    )]
    pub project: Option<String>,
    #[schemars(description = "Cap on hits returned (default 64).")]
    pub limit: Option<usize>,
}

/// `tape_fuzzy` — Levenshtein fuzzy-path search over the per-tree store's path
/// index (error-correct an address path within `max_distance` edits).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TapeFuzzyParams {
    #[schemars(description = "Recursion-tree scope (the per-tree TapeStore namespace). Required.")]
    pub tree: String,
    #[schemars(description = "The query path to fuzzy-match against resident page address paths.")]
    pub query: String,
    #[schemars(
        description = "Maximum Levenshtein edit distance (default 2). Larger values match more \
                       loosely at higher cost."
    )]
    pub max_distance: Option<usize>,
    #[schemars(
        description = "Optional path prefix the matched addresses must start with (post-filter), \
                       e.g. 'corpus/chunk/' or 'scratch/'."
    )]
    pub filter: Option<String>,
}

/// `tape_semantic` — top-`k` semantic retrieval over the READ-ONLY corpus
/// (embeds the natural-language query host-side).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TapeSemanticParams {
    #[schemars(description = "Recursion-tree scope (the per-tree TapeStore namespace). Required.")]
    pub tree: String,
    #[schemars(description = "The natural-language query to embed and retrieve against.")]
    pub query: String,
    #[schemars(description = "Number of nearest hits to return (default 8).")]
    pub k: Option<usize>,
    #[schemars(description = "Optional project name to scope the corpus retrieval.")]
    pub project: Option<String>,
}

/// `tape_list` — enumerate resident page addresses (optionally under a path
/// prefix), in address order, via the per-tree path index.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TapeListParams {
    #[schemars(description = "Recursion-tree scope (the per-tree TapeStore namespace). Required.")]
    pub tree: String,
    #[schemars(
        description = "Optional address-path prefix to filter by (e.g. 'corpus/chunk/', 'scratch/'). \
                       Omit to list every resident address."
    )]
    pub prefix: Option<String>,
    #[schemars(description = "Cap on the number of addresses returned (default 256).")]
    pub limit: Option<usize>,
}

/// `tape_stat` — residency statistics for the per-tree store (bytes / page
/// count / dirty count / out-of-core overlay segments).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TapeStatParams {
    #[schemars(description = "Recursion-tree scope (the per-tree TapeStore namespace). Required.")]
    pub tree: String,
}

/// Serde-friendly mirror of [`context_tape::repl::ReplLimits`] — the hard,
/// deterministic resource ceilings one REPL run executes under. Every field is
/// optional; an omitted field takes the conservative context-tape default
/// (`ReplLimits::default()` — ~100k ops, 256 pages, 1 MiB touched bytes, 64 KiB
/// strings, 16 call levels). See [`TapeReplParams`].
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ReplLimitsParams {
    #[schemars(
        description = "rhai per-script bytecode-operation ceiling (default 100000). Bounds CPU."
    )]
    pub max_operations: Option<u64>,
    #[schemars(
        description = "Host pages-touched ceiling (default 256). Bounds paging pressure: the run \
                       cannot fault in unbounded pages."
    )]
    pub max_pages: Option<u64>,
    #[schemars(
        description = "Host bytes-touched ceiling (default 1048576 = 1 MiB). Bounds the working set."
    )]
    pub max_bytes: Option<u64>,
    #[schemars(
        description = "rhai max string size in bytes any operation may build (default 65536 = 64 KiB)."
    )]
    pub max_string_size: Option<usize>,
    #[schemars(description = "rhai max function-call nesting depth (default 16).")]
    pub max_call_levels: Option<usize>,
}

impl ReplLimitsParams {
    /// Map onto [`context_tape::repl::ReplLimits`], substituting the context-tape
    /// default for every omitted field. Total — never fails.
    pub fn into_limits(self) -> context_tape::repl::ReplLimits {
        let d = context_tape::repl::ReplLimits::default();
        context_tape::repl::ReplLimits {
            max_operations: self.max_operations.unwrap_or(d.max_operations),
            max_pages: self.max_pages.unwrap_or(d.max_pages),
            max_bytes: self.max_bytes.unwrap_or(d.max_bytes),
            max_string_size: self.max_string_size.unwrap_or(d.max_string_size),
            max_call_levels: self.max_call_levels.unwrap_or(d.max_call_levels),
        }
    }
}

/// `tape_repl` — run a **sandboxed white-box REPL** script against the per-tree
/// tape store, gated by a structural admission check.
///
/// This is the *white-box / latent-tier* counterpart of the nine black-box-legal
/// tape verbs: it scripts the tape through context-tape's deny-by-default `rhai`
/// engine (only the nine verbs `peek`/`slice`/`grep`/`get`/`put`/`fuzzy`/
/// `semantic`/`list`/`stat`; no filesystem/network/process; `eval` disabled) under
/// hard, deterministic [`ReplLimitsParams`]. Admission requires **both** a
/// white-box caller (a black-box agent is structurally refused — white-box status
/// is a host-side fact, never a self-reported claim) **and** that the named
/// `experiment_slug` resolves to an **Open** experiment. The durable corpus is
/// never written (`put` targets only tree-local `Scratch`); pgmcp itself runs no
/// shell and never writes the user's source files.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TapeReplParams {
    #[schemars(
        description = "Recursion-tree scope (the per-tree TapeStore namespace == RlmFrame.root_task_id). \
                       Required; pass a fresh UUID for standalone use."
    )]
    pub tree: String,
    #[schemars(
        description = "The tape-DSL script to run: nested calls to the nine tape verbs \
                       (peek/slice/grep/get/put/fuzzy/semantic/list/stat) under the deny-by-default \
                       rhai sandbox. Example: 'put(\"scratch/<tid>/01\", \"hi\"); get(\"scratch/<tid>/01\")'."
    )]
    pub script: String,
    #[schemars(
        description = "Slug of the experiment that authorizes this REPL session. Admission requires \
                       this experiment to exist with status 'open' (DB-backed). Decouples the REPL \
                       from any specific experiment definition while keeping the gate real."
    )]
    pub experiment_slug: String,
    #[serde(default)]
    #[schemars(
        description = "Optional resource ceilings for this run (operations / pages / bytes / string \
                       size / call levels). Each omitted field uses the conservative context-tape \
                       default."
    )]
    pub limits: Option<ReplLimitsParams>,
}
