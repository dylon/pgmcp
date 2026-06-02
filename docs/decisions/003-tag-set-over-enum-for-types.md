# ADR-003: Tag-set over closed enum for the unified semantic type vocabulary

**Status:** Accepted
**Date:** 2026-05-22

## Context

The unified semantic representation plan (`/home/dylon/.claude/plans/would-translating-the-asts-cosmic-quill.md`) introduces a normalized type vocabulary so cross-language structural similarity, type-aware security analysis, and signature-precise API stability become tractable across pgmcp's 12 language backends (`rust`, `python`, `javascript`/`typescript`/`tsx`, `java`, `scala`, `c`/`cpp`, **`rholang`**, **`metta`**, `clojure`/`clojurescript`, `coq`, `tlaplus`, `lean`).

Two shapes for the vocabulary were considered:

- **Closed enum** — a `CREATE TYPE type_normalized AS ENUM (...)` with ~60 hand-chosen kinds; `symbol_parameters.type_normalized` is a single value of that type.
- **Open tag set** — a `TEXT[]` column validated against a `type_tag_catalog` table that lists the recognized tags.

Likewise for symbol effects (`async`, `unsafe`, `may_panic`, `blocking_io`, `deprecated`, plus language-specific effects like Rholang's `channel_send_persistent` and MeTTa's `term_rewrite`), two shapes were considered:

- **`BIT(32)` flags** column on `file_symbols` — bit positions correspond to a closed effect set.
- **`symbol_effects` table** — one row per `(symbol_id, effect)` membership; `effect` is `TEXT` validated against a catalog.

## Decision

**Use tag sets for both type kinds and effects.** Specifically:

1. `symbol_parameters.type_tags TEXT[]` + `file_symbols.return_type_tags TEXT[]`, indexed by GIN, validated against a `type_tag_catalog (name TEXT PRIMARY KEY, description TEXT, language_origin TEXT)` table.
2. `symbol_effects (symbol_id BIGINT, effect TEXT, PRIMARY KEY (symbol_id, effect))` with a btree index on `effect`. No `BIT(32)` column.

## Rationale

### Type kinds are orthogonal, not hierarchical

A Rust `Arc<Mutex<HashMap<K, V>>>` is *simultaneously*:

- `smart_pointer` (the `Arc` wrapper)
- `mutable_ref` (the `Mutex`'s interior mutability)
- `concurrency` (the `Mutex`'s semantic family)
- `map` (the `HashMap` shape)
- `owned` (the outermost ownership)

A closed enum forces "pick one" — invariably the outermost wrapper — and the inner structure has to go into JSONB or be lost. With a tag set, each facet is queryable directly via GIN: `WHERE type_tags @> ARRAY['concurrency','map']`. Subtype relations (`option<T>` is "nullable") become *derived tag rules*, not new enum variants.

### Closed sets can't ALTER away

PostgreSQL's `CREATE TYPE ... AS ENUM` supports `ADD VALUE` but **never** `DROP VALUE` or `RENAME VALUE` in a forward-compatible way. Every misnamed or obsoleted tag would survive forever as a graveyard variant. Tags are deleted by deleting a row in `type_tag_catalog`; misclassifications by per-row `array_remove`.

### Open-world reality

The polyglot ASR shadow has to absorb language families pgmcp's authors have not yet integrated. Rholang surfaces `channel`, `name`, `quoted_process`, `linear`, `persistent`, `synchronous`, `registry_uri`. MeTTa surfaces `atom`, `expression`, `space`, `pattern_variable`, `metta_typed`, `rule_head`, `rule_body`, `nondeterministic` — and exposes effects no other language has: `term_rewrite`, `pattern_match`, `metta_execute`, `space_modify`, `space_import`. Future backends (Coq's tactics? Lean's typeclasses-as-types? a hypothetical Idris dependent-type extractor?) will surface ~5 new categories per language. With a closed enum, every discovery is a `ALTER TYPE` migration step on a multi-million-row table. With a tag catalog, it's `INSERT INTO type_tag_catalog (name, ...) VALUES (...)`.

### Effects are unbounded and language-tinted

Rholang wants `channel_send`, `channel_receive_linear`, `process_spawn`. MeTTa wants `term_rewrite`, `space_modify`. Rust wants `unsafe`, `may_panic`. Python wants `async`, `generator`. A `BIT(32)` flags column:

- Silently caps at 32 effects forever (or requires schema bumps for every new effect).
- Queries read as opaque bit-masks (`WHERE effect_flags & B'00010000' = B'00010000'`).
- Can't model partial knowledge — there's no "we don't yet know whether this function has `may_panic`."
- Doesn't compose with backend-specific extension. Adding `nondeterministic_rewrite` for MeTTa would require coordinating with every other backend's bit assignments.

The `symbol_effects` table gives:

- `WHERE effect = 'async'` on a btree.
- Unbounded effect cardinality, language-local additions without coordination.
- Trivial `LEFT JOIN` for "give me the effect set per symbol" and `EXISTS` for membership.

### The closed-set instinct will resurface — pin the choice now

The `feedback_feature_gated_build_verification.md` discipline applies: a structurally enforced choice (this ADR + the migration's `type_tag_catalog` shape) survives turnover and refactoring better than an in-line comment. Every contributor proposing "let's just use an enum here" can be pointed back at this ADR rather than re-arguing the same case.

## Consequences

### Positive

- New tags ship as `type_tag_catalog` inserts, reviewed like data not architecture.
- Cross-cutting queries (`symbols whose return_type is concurrency-related AND error-typed`) are first-class GIN lookups.
- Backends introduce language-unique tags (`metta_typed`, `linear`, `persistent`) without coordinating bit positions.
- A future "tag deprecation" workflow is a simple row update with a `deprecated_at` column.

### Negative

- Validation is at write time (CHECK against catalog), not at compile time. A Rust enum would catch typos statically; a TEXT[] won't. Mitigated by: (a) backends emit `&'static str` literals from `src/parsing/type_tags/vocabulary.rs`, so typos *are* compile-time errors on the Rust side; (b) a CI test in `pgmcp-testing/tests/golden_type_tags_*.rs` asserts every emitted tag exists in `vocabulary::SEED_TAGS` and in the seeded catalog.
- GIN indexes on `TEXT[]` are larger than a single-column btree on an enum. At pgmcp's scale (low millions of `symbol_parameters` rows) this is not material; verified in similar schemas (`file_chunks.embedding` HNSW dominates the storage envelope).
- Multi-row joins for effect membership cost more than a single bitmask read. Mitigated by `effect_set_for(symbol_id)` helper in `src/mcp/tools/sema_helpers/effects.rs` that materializes once and caches per request.

## References

- Plan: `/home/dylon/.claude/plans/would-translating-the-asts-cosmic-quill.md` § "Schema (one migration step `apply_step(N, "shadow_asr_v1")`)"
- Vocabulary seed: `src/parsing/type_tags/vocabulary.rs` (the canonical name → description → language_origin table that seeds `type_tag_catalog`)
- LFortran ASR for comparison: `https://github.com/lfortran/lfortran/blob/main/src/libasr/ASR.asdl` — ASR is a closed structural grammar because LFortran processes one well-defined language family; pgmcp processes an open polyglot set, so the same shape would be wrong here.
- Discipline reference: `feedback_feature_gated_build_verification.md` (the after-action that established structural enforcement of architecture choices over commenting / convention).

## Amendment — catalog reconciliation & drift enforcement (2026-06-02)

The "Negative" mitigation (b) above promised a test asserting every emitted tag
exists "in `vocabulary::SEED_TAGS` **and in the seeded catalog**". The
catalog-superset half was never written — and its absence bit us: the v21
concurrency effects (`await_point`, `lock_acquire`, `lock_release`,
`thread_spawn`, `channel_select`) were added to `SEED_EFFECTS` but the catalogs
are seeded only inside the **version-gated** v2 migration step, so they never
reached already-migrated databases. The `symbol_effects.effect` → `effect_catalog`
FK then rejected every symbol carrying one and the whole file was skipped,
collapsing symbol coverage. (Full record:
`docs/scientific-ledger/symbol-coverage-rc1-rc2-2026-06-02.md`.)

This is now enforced structurally:

- **Every-boot reconcile.** `reconcile_vocabulary_catalogs` in
  `src/db/migrations.rs` runs **unconditionally** from `run_migrations`,
  idempotently upserting both `effect_catalog` (from `SEED_EFFECTS`) and
  `type_tag_catalog` (from `SEED_TYPE_TAGS`) and verifying catalog ⊇ vocabulary
  (a residual gap is logged at `error!`, never fatal). Adding a
  `define_vocabulary!` line now requires nothing else — the catalog follows on
  the next boot, fresh install or upgrade alike.
- **Regression tests** (the promised "and in the seeded catalog" half):
  `pgmcp-testing/tests/vocabulary_catalog_parity.rs` (real-DB: catalog ⊇ vocab,
  plus a delete-and-heal test proving the reconcile repairs drift) and the no-DB
  tripwire `vocabulary.rs::tests::concurrency_effects_present_in_seed`.

The original decision (TEXT + catalog over a Rust enum) is unchanged; this only
closes the enforcement gap it always intended.
