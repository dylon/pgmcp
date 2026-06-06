# Adoption Report Formal Verification Traceability

Status: focused telemetry/reporting slice for `adoption_report`.

## Scope

`adoption_report` reads durable `mcp_tool_calls` rows independently of
`mcp_tool_telemetry` and reports family adoption for real clients only. It is a
follow-on telemetry slice after `mcp_tool_telemetry` because it shares the same
historical table but has a different correctness boundary: family
classification and allowlisted measurement.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `adoption_report` | Clamp the lookback window to `1..=44640`; normalize output format and reject unknown formats; include only real clients; exclude old rows outside the window; classify A2A, CSM, memory, RLM, and work-item families soundly; count distinct non-empty sessions per family; preserve RLM as a subset of A2A. | `tla/AdoptionReportTelemetry.tla`; `pgmcp-testing/tests/query_smoke_mcp_tools.rs`. |

## Issues Found And Corrected

The output `format` parameter was not normalized. A whitespace-padded
`" json "` request failed as an unknown format even though the rest of the
tool uses bounded, forgiving request handling.

Correction: `format` is now trimmed, blank values default to `json`, and the
accepted set remains `json | markdown | md`.

Existing tests only checked response shape on an empty table. Correction: the
focused smoke test now seeds durable telemetry rows for one real client, one
excluded CLI client, one excluded unknown client, and one old row outside the
window. It asserts family counts for A2A, RLM, memory, work-items, and CSM.

## Formal Model

`tla/AdoptionReportTelemetry.tla` models the real-client allowlist, excluded
clients, recent and old telemetry rows, blank/trimmed/invalid formats,
low/high lookback windows, family classification, distinct-session counting,
and the RLM-subset-of-A2A relation.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `FormatValidatedAndNormalized` / `InvalidFormatsRejected` | Accepted formats are normalized; unknown formats fail closed. |
| `WindowClamped` / `WindowFilterSound` | The lookback window is bounded and old rows do not contribute. |
| `AllowlistOnly` / `ExcludedClientsDoNotContribute` | CLI/unknown/test clients cannot affect adoption measurement. |
| `OverallTotalMatchesVisibleRows` | Overall call count equals visible allowlisted rows. |
| `FamilyCallsSound` | Per-family calls match the classifier. |
| `FamilySessionsDeduped` | Per-family sessions count distinct non-empty session ids. |
| `RlmSubsetOfA2a` | Recursive large-context usage remains a subset of A2A usage. |

## Verification Run 2026-06-05

```bash
(cd docs/formal/tla && ../../../scripts/tlc-capped.sh AdoptionReportTelemetry.tla)
```

Result: exit 0 under `scripts/tlc-capped.sh`; 4 distinct states, 8
generated. All listed invariants held.

```bash
cargo nextest run -p pgmcp-testing --test query_smoke_mcp_tools adoption_report --build-jobs 1
```

Result: 1/1 passed. The focused run covers seeded real-client rows, excluded
CLI rows, old rows outside the window, trimmed JSON format, family counts, and
RLM subset behavior.
