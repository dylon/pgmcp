# Experiment Artifact Formal Verification Traceability

Status: focused high-use-tool slice for `experiment_log_artifact`, the next
durable telemetry-ranked tool after the search/inventory/config and tracker
progress slices.

## Scope

The 31-day `mcp_tool_telemetry` snapshot used for this slice showed
`experiment_log_artifact` at 40 calls. This tool records ad-hoc benchmark,
profiling, and debug artifacts, optionally parsing known benchmark formats into
metrics summaries.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `experiment_log_artifact` | Reject empty/whitespace-only artifact kinds; normalize the kind before parser dispatch, storage, summary embedding, and response; parse recognized `hyperfine`/`criterion` content only when requested; preserve caller metrics for unparsed artifacts; store a content SHA-256 iff content is supplied. | `tla/ExperimentArtifactCapture.tla`; `pgmcp-testing/tests/tool_experiments_integration.rs`. |

## Issue Found And Corrected

`experiment_log_artifact` checked `params.kind.trim().is_empty()` but then used
the untrimmed `params.kind` for parser dispatch, DB storage, embedding summary,
and response payload. A caller sending `" hyperfine "` would pass validation but
skip the hyperfine parser and store a whitespace-padded kind.

Correction: the tool now normalizes `kind` once and uses that normalized value
for parser dispatch, DB insert, summary text, and response JSON.

## Formal Model

`tla/ExperimentArtifactCapture.tla` models empty kind, whitespace-only kind,
trimmed hyperfine, criterion, unrecognized log, and parse-disabled hyperfine
requests.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `EmptyKindRejected` | Empty/whitespace-only kinds do not insert artifact rows. |
| `StoredKindNormalized` | Stored artifact kind equals the normalized non-empty kind. |
| `RecognizedParseReplacesMetrics` | Recognized parse requests replace caller metrics with parsed summaries. |
| `NoParsePreservesMetrics` | Parse-disabled or unrecognized artifacts preserve caller-supplied metrics. |
| `HashIffContentPresent` | Content hash presence exactly tracks content presence. |

## Verification Run 2026-06-05

```bash
timeout 120 tlc -workers 1 \
  -metadir /tmp/pgmcp-tlc-ExperimentArtifactCapture \
  -config docs/formal/tla/ExperimentArtifactCapture.cfg \
  docs/formal/tla/ExperimentArtifactCapture.tla
```

Result: 1,292 distinct states, 1,549 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test tool_experiments_integration --build-jobs 1
```

Result: 1/1 passed. The integration test now sends `" hyperfine "`, verifies
the response kind is `"hyperfine"`, verifies parsed sample count and metrics,
and checks the stored DB row has normalized kind plus a 64-character content
SHA-256.
