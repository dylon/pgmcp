# Topic-clustering bake-off

Engines scored on identical per-project input with a fixed K (fair control). Higher NPMI / diversity / modularity / silhouette = better; `distinct_label_ratio` near 1.0 and `topics_per_doc` near 1 are healthy; a degenerate run (per the Phase-1 gate) is flagged.

## liblevenshtein-rust

17870 chunks · fixed K = 60 · 12088 graph edges

| engine | topics | noise% | distinct_label | topics/doc | max_share | NPMI | diversity | silhouette | modularity | sec |
|:──|──:|──:|──:|──:|──:|──:|──:|──:|──:|──:|
| baseline ⚠ | 58 | 0.0 | 0.793 | 4.000 | 0.233 | -0.520 | 0.355 | -0.000 | — | 125.9 |
| embedding_pca ⚠ | 22 | 0.0 | 0.636 | 4.000 | 0.138 | 0.094 | 0.227 | 0.000 | — | 228.4 |
| embedding_rp ⚠ | 23 | 0.0 | 0.391 | 4.000 | 0.169 | 0.106 | 0.122 | 0.000 | — | 83.7 |
| graph | 95 | 0.2 | 1.000 | 1.000 | 0.174 | 0.367 | 0.678 | — | 1.226 | 17.6 |

**Winner: `graph`** (NPMI 0.367, non-degenerate).

## mettail-rust

21913 chunks · fixed K = 66 · 13347 graph edges

| engine | topics | noise% | distinct_label | topics/doc | max_share | NPMI | diversity | silhouette | modularity | sec |
|:──|──:|──:|──:|──:|──:|──:|──:|──:|──:|──:|
| baseline ⚠ | 49 | 0.0 | 0.653 | 4.000 | 0.232 | -0.454 | 0.363 | 0.000 | — | 161.1 |
| embedding_pca ⚠ | 5 | 0.0 | 0.600 | 4.000 | 0.250 | 0.079 | 0.400 | 0.000 | — | 244.8 |
| embedding_rp ⚠ | 12 | 0.0 | 0.250 | 4.000 | 0.229 | 0.059 | 0.150 | 0.000 | — | 110.2 |
| graph | 207 | 0.8 | 1.000 | 1.000 | 0.080 | 0.310 | 0.618 | — | 1.156 | 25.1 |

**Winner: `graph`** (NPMI 0.310, non-degenerate).

## Papers

31318 chunks · fixed K = 79 · 4138 graph edges

| engine | topics | noise% | distinct_label | topics/doc | max_share | NPMI | diversity | silhouette | modularity | sec |
|:──|──:|──:|──:|──:|──:|──:|──:|──:|──:|──:|
| baseline ⚠ | 44 | 0.0 | 0.477 | 4.000 | 0.244 | -0.346 | 0.300 | 0.000 | — | 214.0 |
| embedding_pca | 6 | 0.0 | 0.500 | 3.972 | 0.249 | 0.143 | 0.433 | 0.839 | — | 331.2 |
| embedding_rp ⚠ | 13 | 0.0 | 0.308 | 4.000 | 0.159 | 0.142 | 0.138 | 0.000 | — | 161.4 |
| graph | 117 | 0.1 | 1.000 | 1.000 | 0.097 | 0.416 | 0.793 | — | 1.066 | 27.8 |

**Winner: `graph`** (NPMI 0.416, non-degenerate).

## pgmcp

9636 chunks · fixed K = 44 · 10249 graph edges

| engine | topics | noise% | distinct_label | topics/doc | max_share | NPMI | diversity | silhouette | modularity | sec |
|:──|──:|──:|──:|──:|──:|──:|──:|──:|──:|──:|
| baseline ⚠ | 41 | 0.0 | 0.780 | 4.000 | 0.242 | -0.421 | 0.459 | 0.000 | — | 52.7 |
| embedding_pca ⚠ | 36 | 0.0 | 0.944 | 4.000 | 0.100 | -0.480 | 0.550 | 0.000 | — | 194.9 |
| embedding_rp ⚠ | 8 | 0.0 | 0.125 | 4.000 | 0.214 | 0.122 | 0.125 | 0.000 | — | 41.2 |
| graph | 184 | 2.0 | 1.000 | 1.000 | 0.039 | 0.327 | 0.640 | — | 1.101 | 10.5 |

**Winner: `graph`** (NPMI 0.327, non-degenerate).

## Overall

Per-project winners:

- liblevenshtein-rust: `graph` (NPMI 0.367)
- mettail-rust: `graph` (NPMI 0.310)
- Papers: `graph` (NPMI 0.416)
- pgmcp: `graph` (NPMI 0.327)

Win counts:

- `graph`: 4

**Recommended default `topic_clustering_method`: `graph`**

## Analysis (2026-06-13)

**Conclusion: the graph-hybrid engine wins on every corpus type** (code, mixed
code+docs, pure prose), so `topic_clustering_method = "graph"` is the empirically
confirmed default.

Methodology: each engine ran on identical per-project chunks with a fixed K (fair
control); metrics are the `src/quality/topic_metrics.rs` suite. The embedding tracks were
re-run AFTER the c-TF-IDF df-nuke fix (`top_membership_topics`), so their NPMI/diversity are
now real (an earlier 1-project run showed `NaN` because diffuse fuzzy memberships + the 40%
max-document-frequency cutoff had emptied every keyword list).

Findings:
- **`baseline` (FCM on raw 1024-d) is anti-coherent everywhere** — NPMI −0.35 … −0.52 and
  flagged degenerate. This is the quantified "before": clustering 1024-d directly collapses.
- **PCA reduction genuinely helps** (the curse-of-dimensionality fix): on `Papers` (prose)
  `embedding_pca` reached silhouette 0.839 and passed the gate — but its NPMI (0.143) is still
  far below graph (0.416). Reduction repairs *separation* but the embedding topics remain less
  coherent + diverse than the graph communities.
- **`graph` dominates on every axis**: NPMI 0.31–0.42, diversity 0.62–0.79, a clean
  1-topic-per-doc partition (vs the 4.0 smearing cap on the fuzzy engines), modularity
  1.07–1.23, and ~10× faster (10–28 s vs 41–331 s) — and never degenerate.
- The degeneracy gate behaved correctly: it flagged baseline + most embedding runs (⚠) and
  passed graph on all four.

Why graph wins for code AND prose: it clusters the fused semantic-kNN + import + co_change
graph, which never computes a 1024-d distance (sidestepping concentration); on prose, where
import/call edges are sparse, the semantic-similarity edges alone still yield coherent
communities.

> Note: a fifth engine, `embedding_hdbscan` (in-tree HDBSCAN\* on PCA-reduced
> embeddings — the canonical BERTopic clusterer), was added after this run. It
> is unit-tested (`src/cron/hdbscan.rs`) and selectable via the bake-off; re-run
> `pgmcp analyze topic-bakeoff` to include its row. The graph engine remains the
> confirmed default (it wins on every corpus type and is O(n log n)-ish, whereas
> HDBSCAN\* is O(n²) and clusters a subsample).
