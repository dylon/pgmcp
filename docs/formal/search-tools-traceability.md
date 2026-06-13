# Search Tool Formal Verification Traceability

Status: traceability artifact for the first search-tool verification
milestone. It records proof obligations, inherited proof corpora, local formal
models, and regression gates.

## Scope

Initial usage telemetry pointed to search and read tools as the right starting
surface. The 31-day `mcp_tool_telemetry` snapshot used for this milestone
showed `grep` (3,315 calls) and `read_file` (1,769 calls) as the highest-volume
tools, followed by `semantic_search` (437), `orient` (349), and `text_search`
(312). The same snapshot also exposed a telemetry issue: tool-call rows were
not retaining the `project` parameter, so per-project usage rankings could not
be trusted until the telemetry fixes landed.

Search and retrieval tools in scope:

| Tool | Local correctness obligations | Existing local evidence |
| --- | --- | --- |
| `semantic_search` | Query embedding has the active 1024-d BGE-M3 shape; optional `project` and `language` filters are honored; scores are ordered by pgvector cosine similarity; worktree dedupe never invents hits. | `pgmcp-testing/tests/oracle_semantic_search.rs`; `docs/formal/tla/SearchToolScoping.tla`; telemetry project-attribution regression in `src/mcp/server.rs`. |
| `text_search` | PostgreSQL FTS matches only chunks satisfying optional `project` and `language` filters; one best chunk per file is returned; bounded variant applies the same filter semantics. | `pgmcp-testing/tests/query_smoke_queries.rs::queries_text_search_filters_project`; `docs/formal/tla/SearchToolScoping.tla`; bounded timeout smoke test. |
| `grep` | Regex or fuzzy mode does not cross a requested project boundary; glob/language filters only remove hits; fuzzy mode delegates approximate matching semantics to liblevenshtein/libdictenstein. | `pgmcp-testing/tests/query_smoke_mcp_tools.rs::tool_grep_project_filter_does_not_leak_other_projects`; `docs/formal/tla/SearchToolScoping.tla`; query smoke tests. |
| `hybrid_search` | BM25 and semantic legs use the same project/language scope before Reciprocal Rank Fusion; leg failure degrades rather than corrupting fused results; numeric request bounds are normalized before leg fetches and truncation. | `pgmcp-testing/tests/oracle_hybrid_search_rrf.rs`; `docs/formal/tla/SearchToolScoping.tla`; `docs/formal/tla/HybridSearchBoundary.tla`; `text_search_bounded` now accepts the project filter used by the hybrid BM25 leg. |
| `orient` | Required project parameter scopes file lists, key entry points, recent files, language summary, and tree output; explicitly workspace-global social/effect summaries do not masquerade as project file results. | `pgmcp-testing/tests/query_smoke_mcp_tools.rs::tool_orient_project_snapshot_does_not_leak_other_projects`; `docs/formal/tla/SearchToolScoping.tla`; existing populated-corpus orient smoke test. |
| `read_file` | Reads exactly one indexed absolute path and does not claim project attribution unless project resolution is explicit. | `pgmcp-testing/tests/query_smoke_queries.rs::queries_read_file_is_exact_absolute_path_only`; `docs/formal/tla/SearchToolScoping.tla`; query smoke tests. |
| `search_commits` | Commit-vector search respects optional project filter and returns only indexed history chunks. | `docs/formal/tla/SearchToolScoping.tla`; query smoke tests. |

Fuzzy and phonetic adapters in scope:

| Tool family | Local adapter obligation | Inherited proof source |
| --- | --- | --- |
| Phonetic normalization and symbol search | Preserve caller-selected project rule set; pass strings, limits, and max-distance bounds to the dependency without weakening them; do not assert order independence. | `../liblevenshtein-rust/docs/verification/INDEX.md`; `../liblevenshtein-rust/docs/verification/README_FORMAL_GATES.md`. |
| Token/fuzzy grep and candidate generation | Preserve the dependency candidate set and distance budget; pgmcp may filter or rank results but must not invent in-budget matches not returned by the dependency. | `../libdictenstein/formal-verification/README.md`; `SubstringSearchSpec.v` and `FuzzyCandidateCoverageSpec.v`. |
| WFST/lattice-backed correction paths | Treat WFST semiring and language-equivalence proofs as dependency contracts; pgmcp verifies only adapter scoping, serialization, and error handling. | `../lling-llang/proofs/README.md`; `../lling-llang/proofs/doc/proof-status.md`. |
| libgrammstein-backed query wrappers | Import only the dependency-bridge contracts libgrammstein marks as trusted or local to its formal gate; pgmcp adapter tests must cover wrapper-level project and result-shape obligations. | `../libgrammstein/formal/README.md`; `../libgrammstein/formal/dependencies/*.md`. |

## Inherited Proof Corpora

`liblevenshtein-rust/`:

- `docs/verification/INDEX.md` records the five completed phonetic rewrite
  theorems: rule well-formedness, bounded expansion, non-confluence,
  termination, and idempotence.
- `docs/verification/README_FORMAL_GATES.md` defines manifest-driven gates:
  `scripts/verify-formal.sh audit`, `trusted`, `coq-trusted`, and `tla`.
- `docs/verification/tla/README.md` documents bounded TLA+ models for
  online scanning, subsumption, product automata, priority queries, and
  value-yielding queries, with explicit abstraction limits.

`libdictenstein/`:

- `formal-verification/README.md` records the ARTrie proof surface, including
  Rocq specs for exact substring search, SCDAWG occurrence construction, and
  fuzzy candidate coverage.
- `formal-verification/VERIFICATION_RESULTS.md` records correspondence tests
  tying those specs to Rust APIs, including substring candidate, SCDAWG
  occurrence, and fuzzy candidate coverage checks.
- `scripts/verify-formal-correspondence.sh` is the dependency refresh gate
  before pgmcp relies on those contracts.

`lling-llang/`:

- `proofs/README.md` and `proofs/doc/proof-status.md` record checked semiring
  foundations, WFST language semantics, partial-correctness algorithm models,
  and finite TLA+ specs for RRWM, lazy composition, and cascade ordering.
- `make verify-proofs` runs the Rocq checks plus TLC configs and expected
  failure mutants from the repository root.

`libgrammstein/`:

- `formal/README.md` records the local TLA+/Rocq gate for importer lifecycle,
  checkpoint state, async shard sync, worker shutdown, persistent storage
  bridge, query semantics bridge, and MKN arithmetic.
- `formal/dependencies/liblevenshtein-contracts.md` and
  `formal/dependencies/libdictenstein-contracts.md` define which dependency
  contracts libgrammstein imports and which remain supporting evidence only.
- `make -C formal dependency-contracts` refreshes imported dependency
  contracts; `make -C formal complete` runs the local formal gate.

## Local Issues Found And Corrected

1. Durable telemetry rows lost project attribution.

   Correction: `instrumented_tool_run` now accepts a normalized project hint.
   CLI dispatch extracts `project` structurally from JSON args, and typed MCP
   handlers with a project parameter pass it through explicitly.

2. The telemetry writer encoded missing optional fields as empty strings.

   Correction: optional telemetry arrays now bind SQL NULL elements instead of
   `""`, and telemetry queries normalize legacy empty project strings with
   `NULLIF(project, '')`.

3. Telemetry aggregations applied filters inconsistently.

   Correction: `summary`, `top_tools`, `top_callers`, `top_projects`,
   `error_rate`, `histogram`, and `raw` now all honor the same `tool`,
   `client_name`, and `project` filter semantics.

4. `text_search` advertised project filtering but did not implement it.

   Correction: `TextSearchParams` now includes `project`; the FTS query left
   joins `projects` and applies nullable project/language filters without
   dropping files whose `project_id` is NULL; `hybrid_search` passes the same
   project filter to the bounded BM25 leg.

5. `docs/search-modes.md` still described the retired MiniLM 384-d search
   path.

   Correction: the doc now states the active 1024-d BGE-M3 vector space.

6. `hybrid_search` accepted a signed `limit` without bounding it before
   computing leg fetch windows or truncating fused output.

   Correction: `limit` now clamps to `1..=100`, non-finite weights reject before
   legs run, negative weights clamp to zero-weight skipped legs, the third-leg
   edit distance uses the shared fuzzy cap, and optional project/language filters
   are trimmed consistently. See `hybrid-search-boundary-traceability.md`.

## Verification Run 2026-06-04

Focused local tests:

```bash
cargo nextest run -p pgmcp telemetry_tests
cargo nextest run -p pgmcp-testing --test query_smoke_queries text_search
cargo nextest run -p pgmcp-testing --test query_smoke_mcp_tools tool_mcp_tool_telemetry_filters_project_across_aggregations
cargo nextest run -p pgmcp-testing --test mcp_tool_smoke text_search_returns_ranked_results_from_mock_db
cargo nextest run -p pgmcp-testing --test query_smoke_misc telemetry_writer_flushes_null_optional_text_and_project
cargo fmt --check
git diff --check
```

Dependency formal checks:

```bash
cd ../liblevenshtein-rust
scripts/verify-formal.sh trusted
scripts/verify-formal.sh coq-file light docs/verification/phonetic/rewrite_rules.v
scripts/verify-formal.sh coq-file light docs/verification/phonetic/zompist_rules.v

cd ../libdictenstein
cargo test --test fuzzy_candidate_coverage_correspondence
cargo test --features persistent-artrie --test persistent_artrie_formal_correspondence

cd ../lling-llang
make -C proofs/coq proof-check
tlc -workers 1 -metadir /tmp/lling-llang-tlc-rrwm-pgmcp \
  -config proofs/tla/MC/RRWM.cfg proofs/tla/RRWM.tla

cd ../libgrammstein
make -C formal rocq
make -C formal apalache
```

Local pgmcp formal checks:

```bash
for v in docs/formal/rocq/*.v; do coqc "$v"; done
cd docs/formal/tla
for spec in *.tla; do timeout 90s tlc -workers 1 -metadir "/tmp/pgmcp-tlc-${spec%.tla}" "$spec"; done
```

The new `SearchToolScoping.tla` model was checked with TLC after it was added,
then extended to include the required-project `orient` snapshot case: 63
distinct states, 4,032 generated states, no invariant violations.

The first direct `zompist_rules.v` compile failed because
`rewrite_rules.vo` had been built against an older Coq/Corelib assumption set.
Cleaning the phonetic proof directory and recompiling `rewrite_rules.v` before
`zompist_rules.v` fixed the proof artifact drift; no source proof change was
required.

Full `./scripts/verify.sh` was run after the focused gates. Gates 1-6 passed:
format, all-target build, clippy with `-D warnings`, release `pgmcp` binary
tests, release `pgmcp-testing`, and the ignored GPU fail-closed fallback smoke.
Gate 7 (`cargo smoke`) failed because the host NVIDIA stack was not usable:
`nvidia-smi` failed outside the sandbox with `Failed to initialize NVML:
Driver/library version mismatch` and `NVML library version: 610.43`. Gate 8
was then run independently as `cargo test --release --tests` and passed. The
local formal gates were also rerun independently after the full-gate attempt.

## Additional Verification Run 2026-06-04

After extending `SearchToolScoping.tla` with the required-project `orient`
snapshot case, TLC checked the model again: 63 distinct states, 4,032 generated
states, no invariant violations.

The following nextest partitions were rerun after the model/test update:

```bash
cargo nextest run -p pgmcp telemetry_tests --build-jobs 1
cargo nextest run -p pgmcp-testing --test query_smoke_mcp_tools --build-jobs 1
cargo nextest run -p pgmcp-testing --test query_smoke_queries text_search --build-jobs 1
cargo nextest run -p pgmcp-testing --test query_smoke_misc telemetry_writer --build-jobs 1
cargo nextest run -p pgmcp-testing --test mcp_tool_smoke \
  text_search_returns_ranked_results_from_mock_db --build-jobs 1
```

Results: 4/4 pgmcp telemetry unit tests passed; 32/32
`query_smoke_mcp_tools` tests passed, including
`tool_grep_project_filter_does_not_leak_other_projects`,
`tool_orient_project_snapshot_does_not_leak_other_projects`, and
`tool_mcp_tool_telemetry_filters_project_across_aggregations`; 3/3
text-search query smoke tests passed; 2/2 telemetry writer tests passed; the
handler-level `text_search_returns_ranked_results_from_mock_db` smoke passed.

The high-volume `read_file` obligation was tightened with a DB-backed exact
absolute-path regression:

```bash
cargo nextest run -p pgmcp-testing --test query_smoke_queries read_file --build-jobs 1
```

Results: 3/3 read-file query tests passed, including a duplicate-relative-path
case proving `read_file` returns only the requested absolute path and does not
reinterpret a relative-looking path.

An attempted broad `cargo nextest run --workspace --all-targets` was not a
useful gate in this environment: it generated hundreds of GiB of debug test
artifacts and the linker crashed with `ld terminated with signal 7 [Bus error]`
once the workspace filesystem reached 100% usage. `cargo clean` removed 425.1
GiB of generated artifacts; the successful commands above were then run as
smaller partitions with serialized build jobs.

## Additional Verification Run 2026-06-05

The `semantic_search` obligation was rechecked directly after auditing the
existing oracle target:

```bash
timeout 300 cargo nextest run -p pgmcp-testing --test oracle_semantic_search --build-jobs 1
```

Results: 5/5 semantic-search oracle tests passed, including
`project_filter_isolates_results`, `language_filter_isolates_results`,
`semantic_search_returns_correct_rank_order_on_pinned_corpus`,
`hnsw_recall_matches_brute_force_within_recall_floor`, and
`ef_search_set_local_does_not_leak_across_pooled_connections`. Exact process
inspection before the run found no active `cargo`, `rustc`, `nextest`, or
formal-verifier process, so there was no live compiler/proc-macro deadlock to
triage for this target.

The current `SearchToolScoping.tla` model was also rerun:

```bash
timeout 120 tlc -workers 1 -metadir /tmp/pgmcp-tlc-SearchToolScoping-current \
  -config docs/formal/tla/SearchToolScoping.cfg docs/formal/tla/SearchToolScoping.tla
```

Results: 63 distinct states, 4,032 generated states, no invariant violations.

## Inventory And Concurrency Follow-Up 2026-06-05

The high-use-tool slice exposed adjacent project-inventory and background-worker
obligations that are tracked separately in
`inventory-and-concurrency-traceability.md`. That follow-up adds
`ProjectInventoryScoping.tla` for duplicate project-name fail-closed behavior,
exact-path file metadata, and boundary-safe cwd resolution, and it records the
libdictenstein ARTrie eviction-thread self-join fix for the observed
`Resource deadlock avoided (os error 35)` crash.

## Manual Verification Commands

Run these only when refreshing the formal traceability evidence:

```bash
cd ../liblevenshtein-rust
scripts/verify-formal.sh trusted
scripts/verify-formal.sh tla

cd ../libdictenstein
scripts/verify-formal-correspondence.sh

cd ../lling-llang
make verify-proofs

cd ../libgrammstein
make -C formal dependency-contracts
make -C formal complete
```

Routine pgmcp code changes still use the project gate:

```bash
./scripts/verify.sh
```
