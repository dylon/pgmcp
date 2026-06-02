# ADR-012 — Ontology trie acceleration

**Status:** Accepted · 2026-06-02
**Context branch:** `feat/work-item-tracker`
**Supersedes/relates:** the trie-accelerator section of the ontology design
(`~/.claude/plans/what-are-the-state-of-the-art-wise-willow.md`), ADR-003 (closed
vocab idiom), the libdictenstein persistent-trie seam (`src/fuzzy/`).

## Context

The hierarchical-ontology design enumerated nine possible uses of libdictenstein's
persistent tries to accelerate ontology operations. Before wiring them, each was
evaluated against the **actual** schema and access patterns (not the planning-time
assumptions). The investigation is recorded in
`~/.claude/plans/explore-trie-accel-api.md`; the load-bearing findings:

1. `FuzzyIndex<V>` (over `PersistentARTrieChar`, mmap + WAL, mtime-coherent
   `FuzzyCache`, rebuilt by the `fuzzy-sync` cron) is the proven persistent-trie
   seam already powering `fuzzy_symbol_search` / `fuzzy_path_search`.
2. `SuffixAutomatonChar` (`SubstringIndex`) is **in-memory only** — it has no
   persistence and is rebuilt per call; it is already wired into `substring_search`
   / `fuzzy_grep` for ad-hoc (request-supplied) corpora.
3. `ontology_tree` is **not** a recursive CTE — it returns a flat per-facet edge
   list. The recursive traversals are `concept_ancestors` (in `ontology_query`) and
   `detect_is_a_cycles` (in `ontology_check`).
4. The FCA `is_a` cover is a **DAG** (FCA produces multi-parent "diamond" concepts).
5. The FCA build interns effect strings with a **per-run, per-facet
   `HashMap<String,u32>`** whose ids never escape one `is_a_cover` call.

These facts decide which trie uses are genuine accelerators, which are best served
by a capability delivered another way, and which would be net-negative.

## Decision

### Wired — the genuine persistent-trie accelerator

**Accel A — concept fuzzy + prefix index.** A workspace-global
`FuzzyIndex<ConceptValue>` (`ConceptValue { entity_id, facet, status, project_id }`)
is materialized from `ontology_concept_meta ⨝ memory_entities` by the `fuzzy-sync`
cron (`rebuild_concepts`), cached in `FuzzyCache` (a new `concepts` kind), and
opened by `open_concept_trie`. `ontology_search` unions three legs and dedups by
`entity_id`:

- **fuzzy** — Damerau-Levenshtein `FuzzyIndex::query` (typo tolerance SQL cannot
  give: "concurency" → "Concurrency Control");
- **prefix** — `FuzzyIndex::prefix` (new method over `iter_prefix_with_values`);
- **ILIKE** — the original SQL substring scan (always-correct fallback).

Crucially the trie proposes only **names**; PG resolves them to live rows
(`resolve_concepts_by_names`, `WHERE name = ANY(...) AND valid_to IS NULL`). So a
stale or cross-project-duplicate trie entry can never produce an incorrect result,
and a cold/absent trie degrades cleanly to ILIKE-only. This is the irreplaceable
win — typo tolerance — and it mirrors the proven `fuzzy_symbol_search` pattern.

### Delivered as capabilities (correct tool, not an ill-fitting trie)

**Accel B → subtree queries via recursive CTE.** `ontology_tree` gains
`root_concept` + `depth`, returning a concept's bounded **descendant** subtree
(`concept_descendants`). A *materialized-path trie* (the plan's original Accel B)
was rejected: it is well-defined only for trees, but the `is_a` hierarchy is a DAG
— a diamond concept has multiple root-paths, so a single materialized path is
incorrect and storing all paths is combinatorial. A recursive transitive closure is
the correct primitive for DAG reachability; the per-facet hierarchy is bounded, so
the CTE is cheap.

**Accel C → invariant-body search via SQL.** `search_concepts_by_name` now also
matches `constraint_text`, so an invariant surfaces by a word in its constraint
sentence ("find the invariant about *ambiguity*"), not only its name. A persistent
`SuffixAutomatonChar` index was rejected: the automaton is in-memory only (it does
not fit the persistent `fuzzy-sync`/`FuzzyCache` model), a per-call rebuild over the
whole concept corpus costs more than the SQL scan at ontology scale, and ad-hoc
substring search is already provided by `substring_search` / `fuzzy_grep`.

### Not built — with evidence

**Accel D — persistent FCA attribute interning.** Rejected as a **pessimization
with no consumer.** `build_facet_isa` interns effect strings with a per-run
`HashMap<String,u32>`; the ids exist only to drive set-inclusion inside one
`is_a_cover` call and never escape it, so the map is already correct and optimal.
Replacing it with a `PersistentVocabARTrie` would add WAL I/O to a pure in-memory
computation for **zero** functional gain — there is no cross-run lattice-diff or
attribute-display feature that needs stable, persisted attribute ids. Per the
data-driven-optimization mandate (benchmark before optimizing; never add machinery
without a measured need), it stays a `HashMap`. Trigger to revisit: a feature that
diffs FCA lattices across runs or displays a concept's attributes by name.

**egglog reasoning engine.** Documented as a future enhancement in
`src/ontology/reason.rs` (the recursive-CTE deduction + embedding-cosine EDC
canonicalization fully cover today's needs; the `ontology_rule` table and the
`ontology_check`/`ontology_query`/`ontology_export` surface are shaped so egglog can
slot in behind them without a migration).

**`DoubleArrayTrieChar` (static read-only dictionary), phonetic dedup,
canonical-form / rule-symbol interning.** Future, low-priority: the static DAT is a
read-throughput variant of Accel A's already-persistent index; phonetic dedup is an
optional extra EDC candidate source (the phonetic automata in `src/fuzzy/phonetic.rs`
remain available); the canonical-form cache and rule-symbol interning are coupled to
the (future) egglog engine.

## Consequences

- `ontology_search` is typo-tolerant and prefix-aware, and searches invariant
  bodies — directly serving the anti-mistake goal (surfacing the right invariant).
- `ontology_tree(root_concept, depth)` answers subtree questions, completing the
  planned tool signature, correctly over the DAG.
- One new persistent trie kind (`concepts`, global) is rebuilt by `fuzzy-sync`
  alongside symbols/paths/commits/mandates; `FuzzySyncReport.concepts_synced` and
  the `fuzzy-sync` log/`trigger_cron` output report it.
- No speculative or incorrect machinery was added: the materialized-path trie
  (DAG-incorrect), the persistent substring automaton (model-mismatched), and the
  persistent FCA vocab (pessimizing, consumer-less) were each rejected on concrete
  evidence rather than built to pad the count.

## Verification

- `src/fuzzy/persistent_artrie.rs` — `prefix_returns_terms_with_values` unit test.
- `pgmcp-testing/tests/oracle_ontology_trie_accel.rs` — real-SQL oracles for the
  concept fuzzy/prefix index (Accel A), invariant-body search (Accel C), and bounded
  DAG subtree descendants (Accel B).
- Full `./scripts/verify.sh` (all 8 gates) including the pgmcp-testing oracle suite.
