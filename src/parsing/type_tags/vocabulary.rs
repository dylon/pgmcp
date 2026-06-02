//! Canonical type-tag and effect vocabulary for the unified semantic
//! representation (shadow ASR).
//!
//! Every tag and effect name a backend emits must be present here. The
//! migration `shadow_asr_v1` seeds `type_tag_catalog` and `effect_catalog`
//! from `SEED_TYPE_TAGS` and `SEED_EFFECTS`; a regression test
//! (`vocabulary_constants_appear_in_seed_lists`) below proves the two
//! representations stay in sync.
//!
//! Backends import the `&'static str` constants (e.g.
//! `type_tags::vocabulary::OWNED`) rather than typing string literals — this
//! makes typos a compile-time error on the Rust side.
//!
//! Vocabulary growth happens here: add a `type_tag!` or `effect!` line, run
//! `verify.sh`, and the migration test catches stale catalog rows.

// Same idiom as `src/parsing/symbols.rs`: the vocabulary types/constants are
// consumed by the `shadow_asr_v1` migration (which seeds `type_tag_catalog`
// + `effect_catalog`) and by every language backend during Phase B+. Until
// every consumer has landed they trip dead-code warnings; the file-level
// allow keeps `clippy -D warnings` green without per-item annotations.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Where a tag was first introduced. `Universal` tags apply to multiple
/// languages; `Language("rholang")` etc. mark tags that exist *only*
/// because that language surfaces a semantic the polyglot baseline does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagOrigin {
    Universal,
    Language(&'static str),
}

impl TagOrigin {
    /// Stable string used in the `type_tag_catalog.language_origin` column.
    pub fn as_db_str(self) -> &'static str {
        match self {
            TagOrigin::Universal => "universal",
            TagOrigin::Language(name) => name,
        }
    }
}

/// One vocabulary entry: name + description + language of origin.
#[derive(Debug, Clone, Copy)]
pub struct TagDef {
    pub name: &'static str,
    pub description: &'static str,
    pub origin: TagOrigin,
}

/// Defines both the public `&'static str` constant and an entry in the
/// matching seed list. Each line is `CONST_NAME = ("string", "description",
/// TagOrigin::...)`.
macro_rules! define_vocabulary {
    (
        $list_const:ident,
        $($const_name:ident = ($literal:literal, $desc:literal, $origin:expr));* $(;)?
    ) => {
        $(
            pub const $const_name: &str = $literal;
        )*
        pub const $list_const: &[TagDef] = &[
            $(
                TagDef {
                    name: $literal,
                    description: $desc,
                    origin: $origin,
                },
            )*
        ];
    };
}

// Type tags. Backends emit these on `symbol_parameters.type_tags` and
// `file_symbols.return_type_tags`. Tags compose freely — a Rust
// `Arc<Mutex<HashMap<K,V>>>` is `{owned, smart_pointer, shared, mutable_ref,
// concurrency, mutex, keyed, container, unordered}`.
define_vocabulary! {
    SEED_TYPE_TAGS,

    // ── Primitive shape ─────────────────────────────────────────────
    TAG_INT          = ("int",          "Signed integer of any width.",                                   TagOrigin::Universal);
    TAG_UINT         = ("uint",         "Unsigned integer of any width.",                                 TagOrigin::Universal);
    TAG_FLOAT        = ("float",        "Floating-point number of any precision.",                        TagOrigin::Universal);
    TAG_BOOL         = ("bool",         "Boolean / logical truth value.",                                 TagOrigin::Universal);
    TAG_CHAR         = ("char",         "Single character / code point.",                                 TagOrigin::Universal);
    TAG_STRING       = ("string",       "Textual string of arbitrary length.",                            TagOrigin::Universal);
    TAG_BYTES        = ("bytes",        "Byte sequence (octet string).",                                  TagOrigin::Universal);
    TAG_UNIT         = ("unit",         "Unit / void / no value.",                                        TagOrigin::Universal);
    TAG_NEVER        = ("never",        "Divergent / bottom / never-returns.",                            TagOrigin::Universal);
    TAG_NULL_LIKE    = ("null_like",    "Language's null/None/nil/undefined sentinel.",                   TagOrigin::Universal);

    // ── Container shape ─────────────────────────────────────────────
    TAG_CONTAINER    = ("container",    "Holds zero or more elements.",                                   TagOrigin::Universal);
    TAG_ORDERED      = ("ordered",      "Element order is preserved.",                                    TagOrigin::Universal);
    TAG_UNORDERED    = ("unordered",    "Element order is not guaranteed.",                               TagOrigin::Universal);
    TAG_KEYED        = ("keyed",        "Indexed by arbitrary key (map-like).",                           TagOrigin::Universal);
    TAG_INDEXED      = ("indexed",      "Indexed by integer position (sequence-like).",                   TagOrigin::Universal);
    TAG_FIXED_SIZE   = ("fixed_size",   "Capacity fixed at construction.",                                TagOrigin::Universal);
    TAG_DYNAMIC      = ("dynamic",      "Resizable at runtime.",                                          TagOrigin::Universal);

    // ── Reference shape ─────────────────────────────────────────────
    TAG_REFERENCE    = ("reference",    "Borrowed / non-owning reference.",                               TagOrigin::Universal);
    TAG_MUTABLE_REF  = ("mutable_ref",  "Reference granting mutation rights.",                            TagOrigin::Universal);
    TAG_POINTER      = ("pointer",      "Raw / unmanaged pointer.",                                       TagOrigin::Universal);
    TAG_OWNED        = ("owned",        "Owning value (caller controls lifetime).",                       TagOrigin::Universal);
    TAG_BORROWED     = ("borrowed",     "Borrowed (caller retains ownership).",                           TagOrigin::Universal);
    TAG_SMART_POINTER = ("smart_pointer", "Managed pointer (Arc/Rc/Box/shared_ptr/Gc).",                  TagOrigin::Universal);
    TAG_WEAK         = ("weak",         "Weak (non-keeping) reference.",                                  TagOrigin::Universal);
    TAG_UNIQUE       = ("unique",       "Unique ownership (move semantics).",                             TagOrigin::Universal);
    TAG_SHARED       = ("shared",       "Shared ownership (refcounted).",                                 TagOrigin::Universal);

    // ── Algebraic shape ─────────────────────────────────────────────
    TAG_OPTION       = ("option",       "Optional / Maybe (None | Some<T>).",                             TagOrigin::Universal);
    TAG_RESULT       = ("result",       "Result / Either (Ok<T> | Err<E>).",                              TagOrigin::Universal);
    TAG_UNION        = ("union",        "Untagged union of multiple types.",                              TagOrigin::Universal);
    TAG_SUM_TYPE     = ("sum_type",     "Tagged sum / variant (discriminated union).",                    TagOrigin::Universal);
    TAG_PRODUCT_TYPE = ("product_type", "Product type / record / tuple-shape.",                           TagOrigin::Universal);

    // ── Computation shape ───────────────────────────────────────────
    TAG_FUNCTION     = ("function",     "Function type (callable).",                                      TagOrigin::Universal);
    TAG_CLOSURE      = ("closure",      "Closure / lambda capturing environment.",                        TagOrigin::Universal);
    TAG_FUTURE       = ("future",       "Future / promise / awaitable result.",                           TagOrigin::Universal);
    TAG_ASYNC        = ("async",        "Async / coroutine context.",                                     TagOrigin::Universal);
    TAG_ITERATOR     = ("iterator",     "Synchronous iterator.",                                          TagOrigin::Universal);
    TAG_STREAM       = ("stream",       "Asynchronous stream / async iterator.",                          TagOrigin::Universal);
    TAG_GENERATOR    = ("generator",    "Generator (yield-able function).",                               TagOrigin::Universal);

    // ── Object / nominal shape ──────────────────────────────────────
    TAG_CLASS        = ("class",        "Class (OO).",                                                    TagOrigin::Universal);
    TAG_INTERFACE    = ("interface",    "Interface / protocol.",                                          TagOrigin::Universal);
    TAG_TRAIT        = ("trait",        "Trait / typeclass.",                                             TagOrigin::Universal);
    TAG_STRUCT       = ("struct",       "Struct / data record.",                                          TagOrigin::Universal);
    TAG_RECORD       = ("record",       "Record / data class (immutable struct).",                        TagOrigin::Universal);
    TAG_ENUM_TYPE    = ("enum_type",    "Enumeration type.",                                              TagOrigin::Universal);

    // ── Generic shape ───────────────────────────────────────────────
    TAG_TYPE_PARAMETER = ("type_parameter", "Bound generic type parameter (T, U).",                       TagOrigin::Universal);
    TAG_ASSOCIATED_TYPE = ("associated_type", "Associated type on a trait/typeclass.",                    TagOrigin::Universal);
    TAG_EXISTENTIAL  = ("existential",  "Existential type (impl Trait, exists T).",                       TagOrigin::Universal);

    // ── Concurrency ─────────────────────────────────────────────────
    TAG_CONCURRENCY  = ("concurrency",  "Concurrency primitive (broad family).",                          TagOrigin::Universal);
    TAG_CHANNEL      = ("channel",      "Channel (Go/Rust mpsc/Clojure core.async/Rholang).",             TagOrigin::Universal);
    TAG_MUTEX        = ("mutex",        "Mutex / lock.",                                                  TagOrigin::Universal);
    TAG_ATOMIC       = ("atomic",       "Atomic primitive.",                                              TagOrigin::Universal);
    TAG_LOCK_FREE    = ("lock_free",    "Lock-free data structure.",                                      TagOrigin::Universal);

    // ── IO / effect surface (carried as type tags when type-encoded) ─
    TAG_IO           = ("io",           "I/O-bound value / handle.",                                      TagOrigin::Universal);
    TAG_NETWORK      = ("network",      "Network-related value (socket, request, etc.).",                 TagOrigin::Universal);
    TAG_FILESYSTEM   = ("filesystem",   "Filesystem-related value (path, file handle).",                  TagOrigin::Universal);
    TAG_GPU          = ("gpu",          "GPU-related value (buffer, kernel, stream).",                    TagOrigin::Universal);
    TAG_DATABASE     = ("database",     "Database-related value (connection, query, row).",               TagOrigin::Universal);

    // ── Special ─────────────────────────────────────────────────────
    TAG_ERROR_TYPE   = ("error_type",   "Error / exception type.",                                        TagOrigin::Universal);
    TAG_PHANTOM      = ("phantom",      "Phantom data (zero-sized type-level marker).",                   TagOrigin::Universal);
    TAG_OPAQUE       = ("opaque",       "Opaque type (hidden representation).",                           TagOrigin::Universal);
    TAG_UNKNOWN      = ("unknown",      "Mapping unavailable; raw text preserved.",                       TagOrigin::Universal);

    // ── Rholang-specific (process calculus) ─────────────────────────
    TAG_PROCESS      = ("process",      "Rholang process (computation, not value).",                      TagOrigin::Language("rholang"));
    TAG_NAME         = ("name",         "Rholang name (quoted process used as channel).",                 TagOrigin::Language("rholang"));
    TAG_QUOTED_PROCESS = ("quoted_process", "@P — process quoted to become a name.",                      TagOrigin::Language("rholang"));
    TAG_LINEAR       = ("linear",       "Linear channel receive binding (`<-`).",                         TagOrigin::Language("rholang"));
    TAG_PERSISTENT   = ("persistent",   "Persistent channel binding (`<=`).",                             TagOrigin::Language("rholang"));
    TAG_SYNCHRONOUS  = ("synchronous",  "Synchronous send/receive.",                                      TagOrigin::Language("rholang"));
    TAG_REGISTRY_URI = ("registry_uri", "Registry URI lookup (`rho:registry:...`).",                      TagOrigin::Language("rholang"));
    TAG_PAR          = ("par",          "Parallel process composition (`|`).",                            TagOrigin::Language("rholang"));

    // ── MeTTa-specific (atom-space / term-rewriting) ────────────────
    TAG_ATOM         = ("atom",         "Atomic MeTTa expression (leaf).",                                TagOrigin::Language("metta"));
    TAG_EXPRESSION   = ("expression",   "Compound MeTTa expression (list).",                              TagOrigin::Language("metta"));
    TAG_SPACE        = ("space",        "Mutable atom-space (e.g. `&self`, `&kb`).",                      TagOrigin::Language("metta"));
    TAG_PATTERN_VARIABLE = ("pattern_variable", "`$x` pattern variable.",                                 TagOrigin::Language("metta"));
    TAG_METTA_TYPED  = ("metta_typed",  "Symbol with explicit `(: name Type)` annotation.",               TagOrigin::Language("metta"));
    TAG_RULE_HEAD    = ("rule_head",    "Rule LHS — pattern matched by the rewriter.",                    TagOrigin::Language("metta"));
    TAG_RULE_BODY    = ("rule_body",    "Rule RHS — produced expression on a match.",                     TagOrigin::Language("metta"));
    TAG_NONDETERMINISTIC = ("nondeterministic", "Multiple rule bindings share a head.",                   TagOrigin::Language("metta"));
}

// Effects. Backends emit these on `symbol_effects`. Effect = "something this
// symbol does", as opposed to type tags = "something this symbol is".
define_vocabulary! {
    SEED_EFFECTS,

    // ── Universal effects ───────────────────────────────────────────
    EFFECT_ASYNC        = ("async",        "Asynchronous function or coroutine.",                        TagOrigin::Universal);
    EFFECT_UNSAFE       = ("unsafe",       "Bypasses language safety (Rust `unsafe`, C++ UB-prone).",    TagOrigin::Universal);
    EFFECT_PURE         = ("pure",         "Declared / inferred pure (no side effects).",                TagOrigin::Universal);
    EFFECT_EXTERN       = ("extern",       "External / FFI / foreign-language interop.",                 TagOrigin::Universal);
    EFFECT_THROWS       = ("throws",       "May throw a checked exception.",                             TagOrigin::Universal);
    EFFECT_MAY_PANIC    = ("may_panic",    "May panic / abort / fatal-error.",                           TagOrigin::Universal);
    EFFECT_DEPRECATED   = ("deprecated",   "Marked deprecated by annotation or attribute.",              TagOrigin::Universal);
    EFFECT_TEST         = ("test",         "Test / spec / example function.",                            TagOrigin::Universal);
    EFFECT_MAIN         = ("main",         "Entry point (`main`, `init`, `_start`).",                    TagOrigin::Universal);
    EFFECT_GENERATOR    = ("generator",    "Generator / yields values.",                                 TagOrigin::Universal);
    EFFECT_OPERATOR     = ("operator",     "Operator overload / built-in operator implementation.",      TagOrigin::Universal);
    EFFECT_VIRTUAL      = ("virtual",      "Virtual / overridable in subclass.",                         TagOrigin::Universal);
    EFFECT_OVERRIDE     = ("override",     "Overrides a parent method.",                                 TagOrigin::Universal);
    EFFECT_INLINE       = ("inline",       "Inline-required (e.g. `#[inline]`, `inline`).",              TagOrigin::Universal);
    EFFECT_CONST_EVAL   = ("const_eval",   "Const-evaluable (`const fn`, `constexpr`).",                 TagOrigin::Universal);
    EFFECT_VOLATILE     = ("volatile",     "Volatile / barrier-required access.",                        TagOrigin::Universal);
    EFFECT_IO           = ("io",           "Performs I/O.",                                              TagOrigin::Universal);
    EFFECT_NETWORK      = ("network",      "Performs network I/O.",                                      TagOrigin::Universal);
    EFFECT_FILESYSTEM   = ("filesystem",   "Performs filesystem I/O.",                                   TagOrigin::Universal);
    EFFECT_DATABASE     = ("database",     "Performs database I/O.",                                     TagOrigin::Universal);
    EFFECT_CRYPTO       = ("crypto",       "Performs cryptographic operations.",                         TagOrigin::Universal);
    EFFECT_CRYPTO_WEAK  = ("crypto_weak",  "Uses weak / broken cryptography (MD5, DES, ECB).",           TagOrigin::Universal);
    EFFECT_BLOCKING_IO  = ("blocking_io",  "Blocks the calling thread on I/O.",                          TagOrigin::Universal);
    EFFECT_GPU_KERNEL   = ("gpu_kernel",   "Launches a GPU kernel.",                                     TagOrigin::Universal);
    EFFECT_CLONE_CALL   = ("clone_call",   "Calls a clone / copy constructor explicitly.",               TagOrigin::Universal);
    EFFECT_VIRTUAL_DISPATCH = ("virtual_dispatch", "Performs virtual / dynamic dispatch.",               TagOrigin::Universal);
    EFFECT_HTTP_HANDLER = ("http_handler", "Handles an HTTP request (route handler).",                   TagOrigin::Universal);
    EFFECT_AUTH_REQUIRED = ("auth_required", "Requires authenticated/authorized caller.",                TagOrigin::Universal);

    // ── Concurrency (coarse membership mirror of the ordered `sync_ops`
    //    skeleton, v21). `sync_ops` carries the ordered detail + resource
    //    identity; these effects give per-symbol "does it touch locks/spawn/
    //    await at all" membership for `symbol_effects`, the effect-drift ledger,
    //    and search facets. Folded into `Symbol.effects` by the symbol-extraction
    //    cron from `extract_sync_ops`. ────────────────────────────────
    EFFECT_LOCK_ACQUIRE  = ("lock_acquire",  "Acquires a mutex / rwlock / lock guard.",                    TagOrigin::Universal);
    EFFECT_LOCK_RELEASE  = ("lock_release",  "Releases a lock guard (explicit or scope-end).",             TagOrigin::Universal);
    EFFECT_THREAD_SPAWN  = ("thread_spawn",  "Spawns an OS thread / async task (thread::spawn, tokio::spawn).", TagOrigin::Universal);
    EFFECT_AWAIT_POINT   = ("await_point",   "Contains an await suspension point.",                        TagOrigin::Universal);
    EFFECT_CHANNEL_SELECT = ("channel_select", "Non-deterministic channel choice (select! / tokio::select!).", TagOrigin::Universal);

    // ── Rholang-specific effects ────────────────────────────────────
    EFFECT_CHANNEL_SEND               = ("channel_send",               "Sends on a channel (`!`).",                     TagOrigin::Language("rholang"));
    EFFECT_CHANNEL_SEND_PERSISTENT    = ("channel_send_persistent",    "Persistent send on a channel (`!!`).",          TagOrigin::Language("rholang"));
    EFFECT_CHANNEL_SEND_SYNC          = ("channel_send_sync",          "Synchronous send (`!?`).",                      TagOrigin::Language("rholang"));
    EFFECT_CHANNEL_RECEIVE_LINEAR     = ("channel_receive_linear",     "Linear receive (`for(@m <- chan)`).",           TagOrigin::Language("rholang"));
    EFFECT_CHANNEL_RECEIVE_PERSISTENT = ("channel_receive_persistent", "Persistent receive (`for(@m <= chan)`).",       TagOrigin::Language("rholang"));
    EFFECT_CHANNEL_RECEIVE_PEEK       = ("channel_receive_peek",       "Peek receive (`for(@m <<- chan)`).",            TagOrigin::Language("rholang"));
    EFFECT_CHANNEL_EVAL               = ("channel_eval",               "Channel dereference (`*x`).",                   TagOrigin::Language("rholang"));
    EFFECT_CONTRACT_DEFINE            = ("contract_define",            "Defines a Rholang contract.",                   TagOrigin::Language("rholang"));
    EFFECT_PROCESS_SPAWN              = ("process_spawn",              "Spawns a parallel process (`|`).",              TagOrigin::Language("rholang"));
    EFFECT_REGISTRY_LOOKUP            = ("registry_lookup",            "Looks up a name via the registry URI.",         TagOrigin::Language("rholang"));

    // ── MeTTa-specific effects ──────────────────────────────────────
    EFFECT_TERM_REWRITE   = ("term_rewrite",   "Defines a term-rewrite rule (`(= LHS RHS)`).",                          TagOrigin::Language("metta"));
    EFFECT_PATTERN_MATCH  = ("pattern_match",  "Rule LHS contains a non-trivial pattern.",                              TagOrigin::Language("metta"));
    EFFECT_METTA_EXECUTE  = ("metta_execute",  "Top-level execution (`!(expr)`).",                                      TagOrigin::Language("metta"));
    EFFECT_SPACE_MODIFY   = ("space_modify",   "Mutates an atom-space.",                                                TagOrigin::Language("metta"));
    EFFECT_SPACE_IMPORT   = ("space_import",   "Imports a space (`(import! &space file)`).",                            TagOrigin::Language("metta"));
}

/// Look up a type-tag definition by name. Returns `None` for unknown tags.
pub fn type_tag(name: &str) -> Option<&'static TagDef> {
    SEED_TYPE_TAGS.iter().find(|t| t.name == name)
}

/// Look up an effect definition by name. Returns `None` for unknown effects.
pub fn effect(name: &str) -> Option<&'static TagDef> {
    SEED_EFFECTS.iter().find(|t| t.name == name)
}

/// True when `name` exists in either the type-tag or effect vocabulary.
/// Used by tests and the persistence layer to validate backend emissions.
pub fn is_known_tag_or_effect(name: &str) -> bool {
    type_tag(name).is_some() || effect(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_type_tags_have_unique_names() {
        let mut seen = std::collections::HashSet::with_capacity(SEED_TYPE_TAGS.len());
        for t in SEED_TYPE_TAGS {
            assert!(seen.insert(t.name), "duplicate type tag name: {}", t.name);
        }
    }

    #[test]
    fn seed_effects_have_unique_names() {
        let mut seen = std::collections::HashSet::with_capacity(SEED_EFFECTS.len());
        for t in SEED_EFFECTS {
            assert!(seen.insert(t.name), "duplicate effect name: {}", t.name);
        }
    }

    #[test]
    fn type_tag_and_effect_overlap_is_documented_not_accidental() {
        // Type tags and effects live in *separate* catalog tables
        // (`type_tag_catalog` vs `effect_catalog`) so the same name can
        // legitimately appear in both — `async` is both a type-tag facet
        // ("this value participates in async context", attached to `Future<T>`
        // parameters) and an effect ("this function is `async fn`"). Allowing
        // the overlap keeps each vocabulary terse and natural. This test
        // exists to keep the overlap *intentional*: when a new collision
        // appears, the developer must add it to ALLOWED_OVERLAPS with a
        // one-liner reason or rename one side.
        const ALLOWED_OVERLAPS: &[(&str, &str)] = &[
            (
                "async",
                "type-tag = 'value participates in async context'; effect = 'function is `async fn`'",
            ),
            (
                "io",
                "type-tag = 'value handles I/O'; effect = 'function performs I/O'",
            ),
            (
                "network",
                "type-tag = 'value is network-related (socket, request)'; effect = 'function performs network I/O'",
            ),
            (
                "filesystem",
                "type-tag = 'value is filesystem-related (path, file handle)'; effect = 'function performs filesystem I/O'",
            ),
            (
                "database",
                "type-tag = 'value is database-related (connection, query, row)'; effect = 'function performs database I/O'",
            ),
            (
                "generator",
                "type-tag = 'generator type'; effect = 'function is a generator'",
            ),
        ];

        let tag_names: std::collections::HashSet<_> =
            SEED_TYPE_TAGS.iter().map(|t| t.name).collect();
        let allowed: std::collections::HashSet<&str> =
            ALLOWED_OVERLAPS.iter().map(|(n, _)| *n).collect();
        for e in SEED_EFFECTS {
            if tag_names.contains(e.name) {
                assert!(
                    allowed.contains(e.name),
                    "effect '{}' collides with a type tag of the same name and is not in \
                     ALLOWED_OVERLAPS — either rename one side or add a one-liner reason \
                     to the allow-list in vocabulary::tests",
                    e.name
                );
            }
        }
    }

    #[test]
    fn every_constant_is_a_well_formed_lowercase_identifier() {
        for t in SEED_TYPE_TAGS.iter().chain(SEED_EFFECTS.iter()) {
            assert!(!t.name.is_empty(), "empty tag name");
            assert!(
                t.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "tag name '{}' has non-lowercase-snake_case chars",
                t.name
            );
            assert!(
                !t.description.is_empty(),
                "empty description for '{}'",
                t.name
            );
        }
    }

    #[test]
    fn lookup_round_trips_for_known_entries() {
        let int = type_tag(TAG_INT).expect("int is a known type tag");
        assert_eq!(int.name, "int");
        assert_eq!(int.origin, TagOrigin::Universal);

        let async_eff = effect(EFFECT_ASYNC).expect("async is a known effect");
        assert_eq!(async_eff.name, "async");
        assert_eq!(async_eff.origin, TagOrigin::Universal);

        let linear = type_tag(TAG_LINEAR).expect("linear is a Rholang type tag");
        assert_eq!(linear.origin, TagOrigin::Language("rholang"));

        let term_rewrite = effect(EFFECT_TERM_REWRITE).expect("term_rewrite is a MeTTa effect");
        assert_eq!(term_rewrite.origin, TagOrigin::Language("metta"));
    }

    #[test]
    fn lookup_returns_none_for_unknown() {
        assert!(type_tag("does_not_exist").is_none());
        assert!(effect("does_not_exist").is_none());
        assert!(!is_known_tag_or_effect("nonsense"));
    }

    #[test]
    fn tag_origin_db_str_is_stable() {
        assert_eq!(TagOrigin::Universal.as_db_str(), "universal");
        assert_eq!(TagOrigin::Language("rholang").as_db_str(), "rholang");
        assert_eq!(TagOrigin::Language("metta").as_db_str(), "metta");
    }

    #[test]
    fn seed_lists_are_nonempty_and_within_reasonable_size() {
        // Documents the intended vocabulary scale. If these explode, an ADR
        // amendment is required — the closed/open trade-off depends on the
        // vocabulary remaining human-curatable.
        assert!(SEED_TYPE_TAGS.len() >= 50, "type tag vocabulary too small");
        assert!(
            SEED_TYPE_TAGS.len() <= 120,
            "type tag vocabulary too large; consider ADR amendment"
        );
        assert!(SEED_EFFECTS.len() >= 30, "effect vocabulary too small");
        assert!(
            SEED_EFFECTS.len() <= 80,
            "effect vocabulary too large; consider ADR amendment"
        );
    }

    #[test]
    fn rholang_and_metta_have_their_signature_tags_and_effects() {
        // Boy Scout — these are the language-specific tags that the plan
        // names explicitly. If any go missing the goldens (Phase C) will
        // fail, but this fails faster and with a clearer message.
        for name in [
            TAG_PROCESS,
            TAG_NAME,
            TAG_QUOTED_PROCESS,
            TAG_LINEAR,
            TAG_PERSISTENT,
            TAG_SYNCHRONOUS,
            TAG_REGISTRY_URI,
            TAG_PAR,
        ] {
            assert!(
                type_tag(name).is_some(),
                "missing Rholang type tag: {}",
                name
            );
        }
        for name in [
            TAG_ATOM,
            TAG_EXPRESSION,
            TAG_SPACE,
            TAG_PATTERN_VARIABLE,
            TAG_METTA_TYPED,
            TAG_RULE_HEAD,
            TAG_RULE_BODY,
            TAG_NONDETERMINISTIC,
        ] {
            assert!(type_tag(name).is_some(), "missing MeTTa type tag: {}", name);
        }
        for name in [
            EFFECT_CHANNEL_SEND,
            EFFECT_CHANNEL_SEND_PERSISTENT,
            EFFECT_CHANNEL_SEND_SYNC,
            EFFECT_CHANNEL_RECEIVE_LINEAR,
            EFFECT_CHANNEL_RECEIVE_PERSISTENT,
            EFFECT_CHANNEL_RECEIVE_PEEK,
            EFFECT_CHANNEL_EVAL,
            EFFECT_CONTRACT_DEFINE,
            EFFECT_PROCESS_SPAWN,
            EFFECT_REGISTRY_LOOKUP,
        ] {
            assert!(effect(name).is_some(), "missing Rholang effect: {}", name);
        }
        for name in [
            EFFECT_TERM_REWRITE,
            EFFECT_PATTERN_MATCH,
            EFFECT_METTA_EXECUTE,
            EFFECT_SPACE_MODIFY,
            EFFECT_SPACE_IMPORT,
        ] {
            assert!(effect(name).is_some(), "missing MeTTa effect: {}", name);
        }
    }

    #[test]
    fn concurrency_effects_present_in_seed() {
        // RC1 tripwire (no DB): the v21 concurrency effects must stay in
        // SEED_EFFECTS. They drifted out of `effect_catalog` once — the
        // every-boot `reconcile_vocabulary_catalogs` in `db::migrations` now
        // heals that — and dropping one here would silently FK-skip every
        // symbol that emits it during extraction. Pairs with the real-DB
        // `vocabulary_catalog_parity` test (catalog ⊇ vocabulary).
        for name in [
            EFFECT_LOCK_ACQUIRE,
            EFFECT_LOCK_RELEASE,
            EFFECT_THREAD_SPAWN,
            EFFECT_AWAIT_POINT,
            EFFECT_CHANNEL_SELECT,
        ] {
            assert!(
                effect(name).is_some(),
                "concurrency effect {name} dropped from SEED_EFFECTS"
            );
        }
    }
}
