# Secret Detection Formal Verification Traceability

Status: focused security scan slice for the direct `secret_detection` MCP tool.

## Scope

The 31-day `mcp_tool_telemetry` snapshot placed `secret_detection` in the
2-call cluster. The tool scans indexed file contents for known secret prefixes
and high-entropy string literals, then appends a crypto-effect enrichment
channel. This slice verifies the direct MCP tool boundary; the quality-report
collector path remains covered by the aggregate tool timeout and is a candidate
for a later shared collector-memory slice.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `secret_detection` | Normalize project names; reject non-finite entropy before lookup; clamp entropy to `0..=8`; clamp finding limits to `1..=500` before scanning; stream file contents one row at a time; stop at the effective limit; drop the content stream before crypto-symbol enrichment; return normalized effective parameters; execute read-only. | `tla/SecretDetectionScan.tla`; `tool_sota_phase6` once sibling dependency compilation is restored. |

## Issues Found And Corrected

The direct tool treated `limit` as an after-push check. With `limit=0` or a
negative limit, the first matching secret was still pushed before the scan
stopped. The tool now computes an effective limit before scanning and clamps it
to `1..=500`.

The tool fetched all project file contents into a `Vec` before scanning. It now
uses the SQLx row stream, keeping at most one file body resident in the direct
scan path, and explicitly drops that stream before running the crypto-symbol
enrichment query.

`min_entropy` is now checked for finiteness and clamped to `0..=8`, and the
response reports the normalized project, effective entropy threshold, and
effective limit.

## Formal Model

`tla/SecretDetectionScan.tla` models the direct tool as local numeric
validation, normalized project lookup, streaming scan, stream drop, enrichment
query, and response construction.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `InvalidInputsDoNotScan` | Non-finite entropy and invalid projects do not scan files or run enrichment. |
| `ProjectRejectsBeforeScan` | Blank, missing, and duplicate projects reject after lookup and before scan. |
| `EffectiveBoundsHold` | Entropy/limit values are bounded and findings never exceed the effective limit. |
| `StreamingMemoryBound` | At most one file body is resident during the direct scan. |
| `CryptoAfterStreamDrop` | Crypto-symbol enrichment cannot run while the content stream is held. |
| `ReadOnly` | The tool performs no writes. |

## Verification Run 2026-06-07

```bash
env PGMCP_TLC_JAVA_XMX=256m \
    PGMCP_TLC_MEMORY_MAX=1536M \
    PGMCP_TLC_METASPACE=64m \
    PGMCP_TLC_CLASS_SPACE=32m \
    PGMCP_TLC_CODE_CACHE=128m \
    ../../../scripts/tlc-capped.sh SecretDetectionScan.tla
```

Result: 8 distinct states, 15 generated states, no invariant violations.

```bash
cargo nextest run -p pgmcp-testing --test tool_sota_phase6 --build-jobs 1
```

Result: pending until the sibling `libdictenstein` refactor is ready for Rust
workspace builds.
