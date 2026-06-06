# Correct Query Formal Verification Traceability

Status: focused high-use fuzzy/WFST adapter slice for `correct_query`.

## Scope

The refreshed 31-day `mcp_tool_calls` snapshot showed `correct_query` at 7
calls with no non-ok outcomes, making it the next uncovered telemetry-ranked
tool after the existing fuzzy, memory, experiment, graph, and concurrency
slices.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `correct_query` | Trim and reject blank projects and queries; cap query size; reject non-finite LM weights; clamp edit distance and LM weight before WFST use; resolve exactly one project id before opening a trie or model; key persistent symbol tries and HybridLM models by `slugified-name-p<project_id>`; keep slug-colliding project names isolated; remain read-only and lock-free. | `tla/CorrectQueryBoundary.tla`; `pgmcp-testing/tests/phonetic_search_and_correct_query.rs`; fuzzy/LM path tests. |

## Issues Found And Corrected

`correct_query` passed raw request parameters into the correction path:

| Issue | Correction |
| --- | --- |
| `max_distance` was cast directly to `usize`, so `u32::MAX` could request a huge edit-distance traversal. | Reuse `src/fuzzy/limits.rs::bounded_max_distance`, capping the effective distance at 64. |
| `lm_weight` accepted out-of-range or non-finite values in direct Rust callers. | Reject non-finite values and clamp finite values into `0.0..=1.0`. |
| Blank/whitespace queries reached the trie/WFST path and returned identity responses. | Trim and reject blank queries before correction. |
| The response did not expose normalized project, distance, or LM weight values. | Return normalized `project`, `max_distance`, and `lm_weight`. |
| Per-project fuzzy trie files were keyed only by `slugify(project_name)`. Distinct names such as `correct/slug` and `correct_slug` could address the same trie file. | Introduce `project_artifact_key(project_id, name) = slugified-name-p<project_id>` and use it for cron-written and lazy-opened symbol/path/commit tries. |
| HybridLM model paths used raw project names. A malformed project name could create nested/path-traversing model paths, and slug-colliding projects could share a model namespace. | Sanitize the legacy helper and add production `model_path_for_project(data_dir, project_id, name)` using the same project artifact key. Cron training, `correct_query`, and the hybrid-search WFST third leg now agree on the keyed model path. |

## Inherited Contracts

This slice does not re-prove Damerau-Levenshtein distance, ARTrie traversal, or
WFST Viterbi semantics inside pgmcp. It relies on the same dependency proof
corpora used by the fuzzy-search slice:

| Dependency | Imported guarantee used here |
| --- | --- |
| `liblevenshtein-rust` | Edit-distance transducer query soundness and bounded candidate generation. |
| `libdictenstein` | Persistent ARTrie publication/recovery and lock-free read traversal for symbol tries. |
| `lling-llang` | WFST semiring/path reasoning and lattice composition contracts. |
| `libgrammstein` | Hybrid language-model serialization/scoring contract used by `PgmcpHybridLm`. |

## Concurrency Boundary

`correct_query` performs no writes and adds no mutexes. The only synchronization
it depends on is existing read-only project resolution plus the persistent trie
cache's mtime coherence. The project-id artifact key prevents two concurrent
requests for slug-colliding project names from racing through the same trie or
model file. The cache remains bounded and non-blocking at the pgmcp layer; the
underlying ARTrie lifecycle is covered by the prior libdictenstein evidence.

## Formal Model

`tla/CorrectQueryBoundary.tla` abstracts the WFST internals and models
representative valid, blank-project, duplicate-project, blank-query,
oversized-query, non-finite-LM, unknown-project, and slug-collision requests.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidRequestsReject` | Invalid project/query/LM requests reject before artifact access. |
| `AcceptedRequestsHaveUniqueProject` | Successful calls have one resolved project id. |
| `BoundsApplied` | Edit distance and LM weight are normalized into finite bounds. |
| `TrieAndModelUseSameResolvedKey` | Trie and HybridLM paths use the same resolved project key. |
| `SlugCollisionSeparatedByProjectId` | Slug-colliding names cannot share artifact keys. |
| `CorrectionIsProjectLocal` | Returned correction candidates belong to the resolved project. |
| `ReadOnlyNoLocks` | The tool writes nothing and acquires no locks. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && \
  env PGMCP_TLC_JAVA_XMX=256m PGMCP_TLC_MEMORY_MAX=1536M \
      PGMCP_TLC_METASPACE=64m PGMCP_TLC_CLASS_SPACE=32m \
      PGMCP_TLC_CODE_CACHE=128m \
      ../../../scripts/tlc-capped.sh CorrectQueryBoundary.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 9 distinct states, 18 generated.
All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test phonetic_search_and_correct_query --build-jobs 1 correct_query
```

Result: 7/7 `correct_query`-filtered tests passed, with 1 unrelated phonetic
test skipped by the filter. The run covers normal correction, over-correction
guarding, mixed-case symbols, input normalization/bounds, blank/non-finite
rejection, duplicate project display-name rejection, and slug-collision
isolation.

```bash
cargo nextest run -p pgmcp-testing --test tool_fuzzy_search_uses_persistent_trie --build-jobs 1
```

Result: included in a 3-binary fuzzy regression run; 7/7 passed across
`tool_fuzzy_search_uses_persistent_trie`,
`fuzzy_sync_handles_null_visibility_and_commits`, and
`tool_fuzzy_search_project_filter`.

```bash
cargo nextest run -p pgmcp-testing --test hybrid_lm_train_smoke --build-jobs 1
cargo nextest run -p pgmcp-testing --test hybrid_lm_train_resume --build-jobs 1
cargo nextest run -p pgmcp-testing --test hybrid_search_three_leg --build-jobs 1
cargo nextest run -p pgmcp-testing --test hybrid_search_third_leg_uses_persistent_trie --build-jobs 1
```

Result: run together as four focused nextest binaries; 4/4 passed.

```bash
cargo test -p pgmcp --lib project_artifact_key_disambiguates_slug_collisions
cargo test -p pgmcp --lib model_paths_sanitize_names_and_project_paths_include_id
```

Result: both helper unit tests passed.
