# ADR-017: Topic-clustering redesign — quality gate, dual engines, and a graph-hybrid default

- **Status:** Accepted & implemented 2026-06-13 (quality gate, dual engines, bake-off,
  c-TF-IDF fix, dendrogram fix, global-cron→graph dispatch, per-project scope, LLM labeling,
  digest — all wired/tested; global roll-up + RAPTOR persistence remain — see "Remaining
  work"). User runs `scripts/verify.sh`; activates on release rebuild + daemon restart.
- **Supersedes the FCM-only "Fuzzy BERTopic" topic engine** (`src/cron/topic_clustering.rs`).
- **Related:** ADR-004 (BGE-M3 1024-d embeddings), the K-selector (Phase 12), the
  scientific-ledger bake-off at `docs/scientific-ledger/topic-clustering-bakeoff-2026-06-13.md`.

## Context — what was broken

A review of the live workspace topic model (2026-06-13, verified by direct SQL) found it
**fully degenerate and silently so**:

- All 200 global topics shared one label — `the / and / dylon / home / workspace`.
- Stored centroids were **384-dimensional** — MiniLM-era artifacts; the topic scan had not
  successfully stored since the BGE-M3 (1024-d) cutover (~late May), i.e. **~3 weeks of
  stale degenerate output** served by `discover_topics` / `topic_hierarchy`.
- `chunk_topic_assignments`: **198.67 topics per chunk** (50M rows / 252k chunks); fuzzy
  memberships uniform at ≈ 1/K.
- The entire hierarchy/summary layer was dead: `code_summary_tree`, `memory_summary_tree`,
  `topic_dendrograms` all **0 rows**.
- **No topic-quality metric was ever computed or persisted** — which is *why* a 3-week
  breakage went unnoticed. The K-selector even computed Xie-Beni / fuzzy-silhouette during
  the K sweep and then discarded them.

### Root cause

FCM clustered **raw 1024-d** L2-normalized BGE-M3 embeddings with a Euclidean/cosine
distance. In ~1024-d, pairwise distances **concentrate** (curse of dimensionality): the
measured pairwise-cosine spread over the live corpus was σ ≈ 0.06 around a mean of 0.59, so
every `D² = 2(1−cos)` sat at 0.82 ± 0.12 and the fuzzy memberships flattened to a uniform
`1/K`. Uniform memberships ⇒ c-TF-IDF cannot separate topics ⇒ all labels collapse. The
`gpu_fcm_precision = fp16` default made it worse by rounding away the σ = 0.06 signal.

**Healthy and reused, not rebuilt:** the c-TF-IDF labeling + stopword tiers, the code graph
(`code_graph_edges`: ~422k semantic + ~130k import + ~8k co-change, all project-scoped),
Louvain community detection, and the cross-project similarity / coupling tools — none of
which depend on `code_topics`.

## Decision

Five changes, smallest-blast-radius first:

### 1. A topic-quality metric suite + a pre-overwrite degeneracy gate (`src/quality/topic_metrics.rs`)

`TopicMetrics` scores any clustering result on one set of axes: **NPMI** (OCTIS/gensim
`c_npmi`) + **UMass** coherence, **topic diversity**, **mean-max-membership** (the direct
FCM-collapse detector: → 1/K ⇒ collapsed), **topics-per-doc** + **max-topic-share** (smearing
/ mega-bucket), **distinct-label-ratio**, and the reused **Xie-Beni / fuzzy-silhouette** on
the *final* model. `degeneracy_reason()` is consulted **before** `clear_topics_for_scope` in
every overwrite path, so a degenerate cycle can never again replace good topics; refusals
bump `topic_degenerate_refusals`. Metrics persist to `pgmcp_metadata['topics_quality']`
(+ bounded history; no migration) and surface in `orient`'s health envelope. This is the
keystone — it makes every later change measurable and makes regressions visible.

### 2. Two interchangeable engines behind `topic_clustering_method`

- **`graph`** (`src/cron/topic_graph.rs`, **the novel engine, now default**) — fuse the
  semantic-kNN + import + co-change edges of `code_graph_edges` into one weighted file graph,
  run Louvain/Leiden, treat each community as a topic, label with the existing c-TF-IDF.
  Never computes a 1024-d distance, so it **sidesteps the concentration collapse entirely**;
  modularity is a free quality signal; assignment is a clean hard partition (no smearing).
- **`embedding_pca` / `embedding_rp`** (`src/cron/topic_reduce.rs`) — the BERTopic recipe:
  reduce 1024-d → ~30-d (in-tree PCA via subspace-iteration + Rayleigh–Ritz Jacobi, or a JL
  random projection — both dependency-free), then FCM. Restores distance contrast.
- **`baseline`** — the original FCM-on-1024-d, kept to quantify the collapse.

UMAP (`annembed`) + HDBSCAN remain a documented future option, gated on dependency vetting
(`ndarray-linalg`/LAPACK vs the `ndarray 0.16` pin); the in-tree PCA is the default reducer.

### 3. A reproducible bake-off (`src/cron/topic_bakeoff.rs`, `pgmcp analyze topic-bakeoff`)

Runs every engine on identical per-project input with a fixed K (fair control), scores them
with the same `TopicMetrics`, and writes a scientific-ledger comparison + a per-project /
overall winner. This is the experiment that picks the default.

### 4. c-TF-IDF df-nuke fix (`top_membership_topics`)

The bake-off exposed a real latent bug: with diffuse fuzzy memberships, c-TF-IDF distributed
every chunk's tokens to *every* topic (any `mu > 1e-8`), so every word landed in every topic
and the 40%-max-document-frequency cutoff **emptied all keyword lists**. Fixed by feeding each
chunk's tokens only to its top-`MAX_MEMBERSHIPS_PER_CHUNK` topics (mirrors the assignment
cap; no-op for the small-K golden fixtures). Regression-tested
(`test_ctf_idf_diffuse_membership_not_nuked`).

### 5. Hierarchy repair (`src/cron/topic_dendrogram.rs`)

The dendrogram cron OOMed on giant projects (libgrammstein's `extract` builds an O(n²)
distance matrix; `claude` @167k → ~114 GB) and the error was swallowed → `topic_dendrograms`
permanently empty. Fixed by bounding the input to `MAX_DENDROGRAM_CHUNKS` (6k) via a
deterministic strided subsample, so it populates for all projects.

## Bake-off result (2026-06-13, 4 projects, post df-nuke fix)

Ran on a representative set spanning code / mixed / prose with a fixed K per project (fair
control). **`graph` won every project on every axis** — full ledger:
`docs/scientific-ledger/topic-clustering-bakeoff-2026-06-13.md`.

| project | graph NPMI | best non-graph NPMI | graph diversity | graph modularity | graph sec |
|:──|──:|──:|──:|──:|──:|
| liblevenshtein-rust (code) | **0.367** | 0.106 | 0.678 | 1.226 | 17.6 |
| mettail-rust (code) | **0.310** | 0.079 | 0.618 | 1.156 | 25.1 |
| Papers (prose) | **0.416** | 0.143 | 0.793 | 1.066 | 27.8 |
| pgmcp (mixed) | **0.327** | 0.122 | 0.640 | 1.101 | 10.5 |

- `baseline` (FCM on raw 1024-d) is **anti-coherent** everywhere (NPMI −0.35 … −0.52) — the
  quantified collapse.
- PCA reduction genuinely helps separation (on prose, `embedding_pca` reached silhouette 0.839
  and passed the gate) but its coherence still trails graph by ~3×.
- `graph` is non-degenerate on all four, gives a clean 1-topic-per-doc partition (vs the 4.0
  smearing cap on the fuzzy engines), and is ~10× faster.
- The df-nuke fix is validated: embedding engines now produce real keywords/NPMI (the earlier
  1-project run showed `NaN` because the max-df cutoff had emptied every keyword list).

## Consequences

- `discover_topics` for a project (`refresh`) and the emergency per-project fallback now use
  the configured engine (graph by default) and pass the quality gate.
- Topic quality is measured, persisted, trended, and surfaced — silent collapse cannot recur.
- No schema migration (quality lives in the `pgmcp_metadata` key/value table).
- New config: `topic_clustering_method`, `topic_reduce_dim`, `topic_graph_edge_weights`,
  `topic_graph_resolution`, and the five `topic_min_*` / `topic_max_*` gate thresholds.

## Done in the follow-up pass (2026-06-13, same session)

- **Global cron now dispatches to the graph engine.** `run_global_topic_scan` routes to the
  new `run_graph_topic_scan` when `topic_clustering_method == "graph"` (the default): it
  clusters **each project independently** (bounded memory) over its fused graph, applies the
  degeneracy gate, persists quality, and stores under `scope='project:NAME'`. The FCM
  in-memory/mmap/online paths remain for the other methods (and stay gate-protected).
- **Per-project is the effective default scope** (`scope='project:NAME'`);
  `chunk_topic_assignments` is populated per-project so the global analysis tools keep working.
- **LLM-label-all wired** — `topic_label_llm::maybe_relabel` runs (via `spawn_blocking`,
  gated by `topic_llm_labels` default true / `topic_llm_backend` `qwen3-4b`, deterministic
  c-TF-IDF fallback) on BOTH the per-project graph cron AND the on-demand
  `run_project_topic_scan` path AND the global roll-up — every stored topic is LLM-labeled.
- **Global roll-up scope DONE** — `build_global_rollup` meta-clusters the per-project topic
  centroids (cosine-kNN + Louvain) into `scope='global'` topics (summed `chunk_count`, merged
  keywords, mean centroid, `parent_topic_ids` → members), stored via `store_global_rollup`
  WITHOUT duplicating `chunk_topic_assignments`; gated + quality-persisted + LLM-labeled. The
  hierarchy overlay (`scope='hierarchy'`) then runs on the fresh global centroids. So
  `discover_topics` with no project again returns a cross-project view.
- **Digest topic-health** — `collect_health` surfaces degenerate topic scopes (read-only
  SELECT; within the digest trust boundary).
- **Manual triggers** — `trigger_cron` gained `topic-clustering`, `code-raptor`,
  `topic-dendrogram`, and `memory-raptor` jobs (they were all absent), so topics / code summary
  tree / topic dendrogram / memory summary tree can be recomputed on demand (under the
  heavy-cron lock) instead of waiting for the interval — the live-verification path after a
  daemon restart.
- **HDBSCAN\* clusterer** (`src/cron/hdbscan.rs`, Phase 2 Track B) — in-tree
  (no fragile new dep) standard Campello/McInnes HDBSCAN\* (core-distance → mutual reachability
  → MST → condensed tree → Excess-of-Mass), unit-tested (recovers 3 separated blobs, single
  blob, noise). Wired as the `embedding_hdbscan` engine (`cluster_embeddings_hdbscan`: PCA-reduce
  → subsample-cluster → assign-all-to-nearest-centroid for O(n²) tractability) — selectable via
  `cluster_embeddings_engine`, the on-demand `run_project_topic_scan`, and the bake-off.

Verified: `cargo check` + `cargo clippy --bin pgmcp --all-targets` clean, 81 topic tests pass,
`cargo fmt --check` clean. (Full `scripts/verify.sh` is run by the user.)

## Remaining work (follow-up)

- **Workspace summary trees are operational, not defects.** `topic_dendrograms` (topic
  hierarchy) and `code_summary_tree` (code RAPTOR) are both **scheduled** crons
  (`topic_dendrogram::run_or_log` at scheduler.rs:2021, gated on `topic_dendrogram_interval_secs
  > 0`; `code_raptor::run_code_raptor` at scheduler.rs:1608) with correct code; the
  `topic_dendrogram` OOM that kept it empty is fixed here (input cap). Both populate on the next
  daemon restart + cron run. No code change needed beyond the dendrogram cap.
- **`memory_summary_tree` — now manually triggerable.** `memory_raptor` is the *memory-server's*
  RAPTOR (recursive LLM summarization over agent `memory_observations`). It was a dead module
  (`pub mod memory_raptor;` with no caller). It is now wired into `trigger_cron` as the
  `memory-raptor` job: it builds the configured local LLM extractor
  (`make_extractor(parse_backend_choice(topic_llm_backend))`) and rebuilds `memory_summary_tree`
  on demand, under the heavy-cron lock. It is deliberately **not** added as a recurring
  *scheduled* cron — it would be the daemon's only model-loading recurring job (a resource
  decision); the manual trigger fully exercises it. (Auto-scheduling remains a one-line config
  opt-in if desired.)
- **Activation** — none of this is live until a release rebuild + daemon restart. The
  comprehensive multi-project bake-off ledger and the live post-restart verification are run
  against that rebuild (`scripts/verify.sh` is run by the user, who controls deploy cadence).

## Alternatives considered

- **Keep FCM, just lower `m` / `K`** — partial mitigation of concentration but does not
  address the fundamental high-dim distance collapse; rejected as primary in favor of either
  reduction or graph clustering.
- **UMAP + HDBSCAN now** — SOTA-canonical but adds a heavy dependency surface
  (`ndarray-linalg`/LAPACK); deferred behind vetting since in-tree PCA + graph already repair
  the failure.

## Addendum A (2026-06-15): algorithm-signature staleness correctness across all engines

The "honest staleness detection" of Decision §1 / `grading-reliability.md` §6 keyed
`orient.health.topics_stale` and `architecture_quality`'s `separation_of_concerns`
dimension on a stored algorithm signature
(`pgmcp_metadata['topics_algo_signature']` vs `TOPICS_ALGO_SIGNATURE`). When `graph`
became the **default** engine (this ADR), the signature mechanism silently broke: the
graph engine never stamped the signature, so the model read as *permanently stale* even
seconds after a fully successful scan. Measured 2026-06-15: 5,882 fresh topics across 66
projects, yet `SELECT … WHERE key='topics_algo_signature'` returned **zero rows**.

### Defects (all fixed here)

| # | Defect |
|---|---|
| D1 | Graph engine (the default) never stamped: it stores `global` via `store_global_rollup` and per-project via `store_topics("project:…")`, both bypassing the lone stamp site (guarded by `scope=="global"` inside `store_topics`). |
| D2 | Online FCM path also never stamped: its keyword-less shell topics tripped an inline `with_kw`/`distinct_lead` heuristic, so even the FCM family did not stamp uniformly. |
| D3 | The signature was a bare const with **no engine identity** — switching `topic_clustering_method` was undetectable; stale cross-engine topics were trusted (graph's hard 1-topic/doc partition vs the FCM tracks' soft up-to-`MAX_MEMBERSHIPS_PER_CHUNK` partition are not interchangeable). |
| D4 | Two divergent degeneracy definitions: the canonical `topic_gate_rejects` gate vs the ad-hoc heuristic buried in the DB-layer `store_topics`; they disagreed (and did, for online). |
| D5 | `config.rs` doc-comment claimed default `"embedding_pca"`; the code returns `"graph"`. |
| D6 | The scheduler's `topic_cron_config` was a lossy field-by-field literal with `..CronConfig::default()` that **reset every un-listed topic knob** (engine method, gate thresholds, LLM-label toggle, reducer dims, graph weights) back to default — so operator TOML overrides were ignored by the cron, and the cron's `topic_clustering_method` (hence the stamped signature) could disagree with what the consumers compute. |

### Resolution

1. **Stamping is a cron-orchestration concern, not a storage-primitive one.** Removed the
   stamp block *and its ad-hoc degeneracy heuristic* from `db::queries::store_topics` (now a
   pure storage primitive — kills D4). Added one private helper
   `topic_clustering::stamp_topics_signature(db, config)`, called on the **success path** of
   each global-refresh strategy after the canonical gate passed and the global store
   succeeded: FCM in-memory, FCM mmap, FCM online, and the graph global roll-up
   (`build_global_rollup`, after `store_global_rollup` + quality persist). The invariant is
   uniform: **stamp ⟺ (gate passed, where the strategy has one) ∧ (global store succeeded)**.
   The per-project emergency fallback, the `hierarchy` overlay, and on-demand single-project
   `discover_topics` deliberately do **not** stamp (they do not refresh the authoritative
   `global` scope), so the model stays honestly stale until a real global refresh.
2. **Engine-aware signature (D3).** `topics_effective_signature(config) = "pgmcp-topics-v3+{method}"`.
   An engine switch yields a different string ⇒ correctly stale until recompute. The two
   consumers (`tool_orient.rs`, `tool_architecture_quality.rs`) compare against this effective
   value. The const is **not** bumped v3→v4 (the label pipeline is unchanged; the engine-suffix
   transition already forces exactly one free recompute on next cron).
3. **Full config threading (D6).** The scheduler's `topic_cron_config` is now `config.clone()`
   (mirroring `sem_cron_config`), threading every topic knob from TOML.
4. **Doc fixes (D5).** `config.rs` comment corrected; `grading-reliability.md` §6 updated.

### Success-path case matrix (graph engine)

```
run_global_topic_scan
  ├─ 0 chunks ──────────────────► NoOp return                  [no stamp]  ✓ stale
  └─ method ∈ {graph, embedding_hdbscan}
        └─ run_graph_topic_scan
              ├─ per project: gate? ──reject──► continue (keep prior)
              │                   └─accept──► store_topics("project:NAME")  [no stamp]
              └─ build_global_rollup
                    ├─ <4 candidates ──────────► return         [no stamp]  ✓ stale
                    ├─ labels degenerate ──────► return         [no stamp]  ✓ stale
                    └─ store_global_rollup OK → persist_quality
                          └─ ★ stamp_topics_signature(db, config)           ✓ fresh
```

Partial success (some per-project scans gate-rejected, ≥4 good candidates remain) **does**
stamp — the consumer-visible `global` scope is genuinely fresh and non-degenerate; rejected
projects keep their prior rows.

### Validation (this host; the real-DB test suite is dormant here)

After release rebuild + daemon restart: `trigger_cron{job:"topic-clustering"}` →
`psql -tAc "SELECT value FROM pgmcp_metadata WHERE key='topics_algo_signature'"` must return
`pgmcp-topics-v3+graph` → `orient{project:"pgmcp"}` must show `health.topics_stale:false`.
Pure unit tests (signature format) run here; `require_test_db!` integration tests
(`graph_scan_stamps`, `degenerate_rollup_unstamped`, `online_path_stamps`,
`engine_switch_invalidates`, `emergency_fallback_no_global_stamp`) run in CI.
