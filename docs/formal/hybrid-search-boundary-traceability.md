# Hybrid Search Boundary Formal Verification Traceability

Status: focused addendum for the direct `hybrid_search` MCP boundary.

## Scope

`SearchToolScoping.tla` already covers `hybrid_search` project/language result
scoping. This addendum covers the request-local numeric and degradation boundary:
bounded limits, finite weights, bounded third-leg edit distance, normalized
optional filters, and independent leg failure handling.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `hybrid_search` | Clamp signed `limit` to `1..=100` before using it for leg fetch windows or `Vec::truncate`; reject non-finite BM25/semantic/WFST weights before any leg runs; treat negative weights as zero-weight skipped legs; bound third-leg edit distance to the shared fuzzy maximum; trim optional project/language filters and omit blank filters; degrade on text/semantic leg error or timeout instead of failing the whole response; perform no DB writes or retained locks. | `tla/HybridSearchBoundary.tla`; pure helper tests in `tool_hybrid_search.rs` once sibling dependency compilation is restored. |

## Issues Found And Corrected

`limit` was an `Option<i32>` used directly. A negative value could flow into
`limit * 2` and then into `fused.truncate(limit as usize)`, where the cast would
produce a very large `usize`. The tool now clamps `limit` through the shared
fuzzy result-window policy before any fetch or truncate.

Weights were accepted without finite checks. Non-finite values now reject with
`invalid_params` before any text, embedding, semantic, or WFST leg runs. Negative
weights clamp to zero, preserving the existing zero-weight "skip this leg"
semantics.

The third-leg edit distance was unbounded. It now uses the shared fuzzy maximum,
matching the bounds already verified for fuzzy symbol/path/phonetic search.

Optional project and language filters are now trimmed once and blank filters are
omitted consistently across BM25, semantic, and WFST-rewritten semantic legs.

Scope note: the optional WFST leg may lazy-warm a persistent symbol trie if the
artifact is absent. This addendum therefore claims no DB writes and no retained
locks, but does not claim the third leg is filesystem-read-only.

## Formal Model

`tla/HybridSearchBoundary.tla` models request normalization, finite-weight
validation, leg run/skip/error/timeout outcomes, bounded fetch windows, bounded
fused output, and third-leg activation requirements.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `BadWeightsRejectBeforeLegs` | NaN/Inf weights reject before any leg contributes. |
| `BoundsBeforeFetchAndTruncate` | Effective limit, fetch window, fused count, and edit distance are bounded. |
| `ZeroWeightsSkipLegs` | Zero-weight legs report `skipped`. |
| `LegFailuresDegradeOnly` | Text/semantic errors or timeouts set `degraded` without invalidating surviving legs. |
| `ThirdLegRequiresProjectAndModel` | WFST can run only with a normalized project, a model, and a positive weight. |
| `NoDbMutationOrRetainedLocks` | The modeled direct tool path performs no DB writes and retains no locks. |

## Verification Run 2026-06-07

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh HybridSearchBoundary.tla
```

Result: 7 distinct states, 13 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp --lib tool_hybrid_search
```

Result: pending until sibling `libdictenstein` / `libgrammstein` compilation is
restored; Rust workspace builds are intentionally not run during this formal-only
blocker window.
