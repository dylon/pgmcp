# Inventory And Concurrency Formal Verification Traceability

Status: follow-up artifact for the high-use search/inventory verification
slice and the 2026-06-04 ARTrie eviction-thread crash.

## Scope

This slice covers two bug classes found during formal verification and focused
regression testing:

- Project-scoped inventory tools must not merge or leak results across
  ambiguous project identities.
- `list_projects` must preserve distinct indexed project identities, even when
  display names collide, so clients can see why name-scoped tools fail closed.
- `mandate_context` must apply the same duplicate-name fail-closed rule as
  other project-name-scoped tools before loading project-local instructions.
- libdictenstein ARTrie background workers must shut down without leaking,
  blocking forever, or attempting to join the current thread.

Sage is available on this host and remains useful for independent finite
algebraic checks, ranking models, graph arithmetic, or vector-space sanity
checks. It was not used for this slice because the obligations were transition
and SQL-snapshot properties better exercised by TLC plus PostgreSQL tests.

## Local Issues Found And Corrected

1. ARTrie eviction shutdown could self-join.

   Evidence: pgmcp daemon crashed with `failed to join thread: Resource
   deadlock avoided (os error 35)` on thread `artrie-eviction-char`.

   Correction: libdictenstein `EvictionCoordinator::shutdown` now compares the
   stored `JoinHandle` thread id with `thread::current().id()`. Owner-thread
   shutdown still joins after setting the shutdown flag; worker-thread teardown
   detaches the current-thread handle instead of invoking `join()`.

2. Background worker lifecycle formal model did not cover worker-side drop.

   Correction: libdictenstein `BackgroundWorkerLifecycle.tla` now includes
   owner-thread teardown and worker-thread last-strong-reference teardown, with
   `NoSelfJoin` in addition to `NoOrphan` and termination.

3. `find_project_by_cwd` used raw string-prefix matching.

   Correction: project resolution now matches only exact project paths or
   path-component children. `/ws/boundary/app` no longer matches
   `/ws/boundary/application/src`.

4. Name-scoped inventory tools could merge duplicate display names.

   Correction: `project_tree` resolves project-name matches in one SQL snapshot
   and fails closed on duplicate names. `orient` similarly detects duplicate
   display names before building project-scoped path lists.

5. `file_info` lacked durable project attribution in its metadata response.

   Correction: `file_info` now joins the owning project and returns
   `project_name` when a file row has a project owner.

6. `list_projects` was outside the project-inventory formal model even though
   it is a high-use identity-discovery tool.

   Correction: `ProjectInventoryScoping.tla` now models `list_projects`
   separately from name-scoped inventory tools. The invariant
   `ListProjectsPreservesProjectIdentity` requires duplicate display names to
   remain visible as distinct project rows, while `orient` and `project_tree`
   continue to fail closed when a caller supplies an ambiguous name.

7. `mandate_context` was corrected to fail closed on duplicate display names,
   but the inventory model and MCP smoke tests did not yet exercise that
   high-use path.

   Correction: `mandate_context` is now included in the `NameScopedTools` set in
   `ProjectInventoryScoping.tla`, and the MCP smoke suite covers duplicate-name
   rejection at the tool boundary.

## Formal Models

| Model | Obligations |
| --- | --- |
| `tla/ProjectInventoryScoping.tla` | `ListProjectsPreservesProjectIdentity`, `NameScopedNoCrossProjectLeak`, `AmbiguousNamesFailClosed` for `mandate_context`/`orient`/`project_tree`, `ExactPathOnly`, `BoundarySafeCwd`, `SiblingPrefixRejected`. |
| `tla/AtomicFileReplacement.tla` | `VisibleNeverPartial`, `AbortRollsBack`, `CommitPublishesCompleteReplacement`, `NullHashNeverVisible`. |
| `../libdictenstein/formal-verification/tla+/BackgroundWorkerLifecycle.tla` | `NoOrphan`, `NoSelfJoin`, and eventual teardown termination for owner-thread and worker-thread shutdown paths. |

## Verification Run 2026-06-05

TLC:

```bash
timeout 120 tlc -workers 1 \
  -metadir /tmp/pgmcp-tlc-ProjectInventoryScoping \
  -config docs/formal/tla/ProjectInventoryScoping.cfg \
  docs/formal/tla/ProjectInventoryScoping.tla
```

Result after adding the `list_projects` and `mandate_context` obligations: 19
distinct states, 28 generated states, no invariant violations.

```bash
timeout 120 tlc -workers 1 \
  -metadir /tmp/pgmcp-tlc-AtomicFileReplacement \
  -config docs/formal/tla/AtomicFileReplacement.cfg \
  docs/formal/tla/AtomicFileReplacement.tla
```

Result: 14 distinct states, 23 generated states, no invariant violations.

```bash
timeout 120 tlc -workers 1 \
  -metadir /tmp/libdictenstein-tlc-BackgroundWorkerLifecycle \
  -config ../libdictenstein/formal-verification/tla+/BackgroundWorkerLifecycle.cfg \
  ../libdictenstein/formal-verification/tla+/BackgroundWorkerLifecycle.tla
```

Result: 8 distinct states, 9 generated states, no invariant violations.

Rust regressions:

```bash
CARGO_TARGET_DIR=/tmp/libdictenstein-pgmcp-target cargo test \
  --manifest-path ../libdictenstein/Cargo.toml \
  --features persistent-artrie --lib \
  persistent_artrie_core::eviction::coordinator::tests::test_coordinator_does_not_join_current_thread \
  -- --nocapture

CARGO_TARGET_DIR=/tmp/libdictenstein-pgmcp-target cargo test \
  --manifest-path ../libdictenstein/Cargo.toml \
  --features persistent-artrie \
  --test persistent_char_thread_lifecycle -- --nocapture

cargo nextest run -p pgmcp-testing --test query_smoke_queries --build-jobs 1
cargo nextest run -p pgmcp-testing --test query_smoke_mcp_tools --build-jobs 1
cargo nextest run -p pgmcp-testing --test mcp_tool_smoke file_info --build-jobs 1
cargo nextest run -p pgmcp-testing --test mcp_tool_smoke project_tree --build-jobs 1
cargo nextest run -p pgmcp telemetry_tests --build-jobs 1
cargo nextest run -p pgmcp-testing --test query_smoke_misc telemetry_writer --build-jobs 1
cargo nextest run -p pgmcp-testing --test db_session_timeouts replace_indexed_file --build-jobs 1
cargo nextest run -p pgmcp-testing --test migrations_versioning work_item_code_anchor_chunk_fk_index_exists --build-jobs 1
cargo fmt --check
git diff --check
```

Results: libdictenstein self-join unit test passed; libdictenstein persistent
char lifecycle tests passed 2/2; pgmcp `query_smoke_queries` passed 113/113;
`query_smoke_mcp_tools` passed 32/32; `mcp_tool_smoke file_info` passed 4/4;
`mcp_tool_smoke project_tree` passed 4/4; pgmcp telemetry unit tests passed
4/4; telemetry writer smokes passed 2/2; the atomic replacement lock-timeout
rollback regression passed 1/1; the chunk-anchor FK-index migration regression
passed 1/1; formatting and diff whitespace checks passed.

The long quiet interval during `cargo nextest run -p pgmcp telemetry_tests` was
checked against the host process table: `rustc` was actively compiling at about
95-99% CPU, so it was not a macro or proc-macro deadlock. The target completed
successfully after the compile phase.

Additional non-`verify.sh` validation after the clippy/test-harness cleanup:

```bash
cargo clippy -p pgmcp --all-targets -- -D warnings
cargo clippy -p pgmcp-testing --all-targets -- -D warnings

cargo nextest run -p pgmcp-testing \
  --test golden_ctf_idf \
  --test golden_fcm \
  --test oracle_graph_algorithms \
  --test claude_chunker_nul_bytes \
  --test hybrid_lm_train_resume \
  --test hybrid_lm_train_smoke \
  --test phonetic_pipeline_search \
  --test memory_phase6_7 \
  --test semver_break_audit_levenshtein \
  --test oracle_symbol_resolution \
  --build-jobs 1

cargo nextest run -p pgmcp-testing \
  --test query_smoke_queries \
  --test query_smoke_mcp_tools \
  --test query_smoke_misc \
  --test mcp_tool_smoke \
  --test db_session_timeouts \
  --test migrations_versioning \
  --test query_inventory_vs_coverage \
  --test rholang_metta_indexing \
  --test topic_dendrogram_cron_registered \
  --test work_item_presence_cron_registered \
  --build-jobs 1

cargo nextest run -p pgmcp-testing \
  --test api_status_endpoint \
  --test config_watcher_e2e \
  --test cron_jobs_e2e \
  --test indexer_pipeline_e2e \
  --test embedder_bgem3_loads_from_pth \
  --build-jobs 1

cargo fmt --check
git diff --check
```

Results: pgmcp clippy passed; pgmcp-testing clippy passed; the fast patched-test
partition passed 43/43 with 1 skipped; the database/MCP smoke partition passed
263/263; the e2e-style partition passed 39/39, including
`process_file_concurrent_on_distinct_paths_no_deadlock`,
`process_file_burst_of_rewrites_converges_to_last_version`, and the BGE-M3
1024-d embedding smoke. Formatting and diff whitespace checks passed.

The `list_projects` identity-preservation regression was then run directly:

```bash
cargo nextest run -p pgmcp-testing --test query_smoke_queries queries_list_projects --build-jobs 1
```

Results: 2/2 passed, including
`queries_list_projects_preserves_duplicate_display_names`.

The `mandate_context` ambiguity regression was also run directly:

```bash
cargo nextest run -p pgmcp-testing --test mcp_tool_smoke mandate_context --build-jobs 1
```

Results: 2/2 passed, including
`mandate_context_rejects_ambiguous_project_name`.

Full `./scripts/verify.sh` remains deferred until explicit approval because it
is slow. The previous complete run had already isolated the remaining known
full-gate failure to an external CUDA/NVML host-driver mismatch in the smoke
gate; an interrupted later attempt is not counted as verification evidence.
