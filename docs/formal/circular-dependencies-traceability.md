# Circular Dependencies Formal Verification Traceability

Status: focused high-use graph slice for `circular_dependencies`.

## Scope

The tool resolves a project name, loads project-local import edges, finds SCCs,
and reports simple cycles up to a caller-supplied maximum length.

Local correctness obligations:

| Tool | Obligations | Evidence |
| --- | --- | --- |
| `circular_dependencies` | Clamp signed max-cycle-length requests to a finite search cap; fail closed on duplicate project display names; query edges/files by resolved project id; emit only closed import cycles inside that project; attach the project hint to durable MCP telemetry. | `tla/CircularDependenciesScope.tla`; `pgmcp-testing/tests/oracle_circular_dependencies.rs`. |

## Issues Found And Corrected

`circular_dependencies` previously cast `params.max_cycle_length.unwrap_or(10)`
directly to `usize`. A negative JSON value could therefore become a huge search
bound instead of the intended small request.

Correction: max cycle length is clamped to `2..=64` before cycle extraction.

The tool also resolved `projects.name` with `fetch_optional`, which could pick an
arbitrary row when multiple indexed workspaces shared the same project basename.
The Shadow-ASR enrichment repeated that name lookup.

Correction: project lookup now collects all matching ids, rejects duplicate
display names with `invalid_params`, and reuses the resolved id for edges, file
metadata, and effect enrichment.

The tool body used `expect` when extracting a raw `PgPool`. Correction: it now
returns an MCP internal error instead of panicking if invoked with a non-pool DB
client.

The MCP handler used the generic instrumentation wrapper. Correction: it now
passes `params.project` through `instrumented_tool_wrap_with_project`.

## Formal Model

`tla/CircularDependenciesScope.tla` models unique, duplicate, and missing
project names; negative, zero, normal, and oversized max-cycle-length requests;
project-scoped files; and import edges that contain both valid and invalid
candidate cycles.

Checked invariants:

| Invariant | Meaning |
| --- | --- |
| `NonUniqueProjectRejected` | Missing or duplicate project display names never produce cycles. |
| `EffectiveMaxCycleLengthClamped` | Every response uses the clamped search bound. |
| `ReportedCyclesProjectScoped` | Every reported cycle node belongs to the resolved project id. |
| `ReportedCyclesWithinMax` | No reported cycle exceeds the effective max length. |
| `ReportedCyclesAreClosedImportCycles` | Every reported cycle is backed by closed project-local import edges. |
| `NoCyclesOnRejected` | Rejected requests return an empty cycle set. |

## Verification Run 2026-06-05

```bash
env PGMCP_TLC_MEMORY_MAX=768M PGMCP_TLC_JAVA_XMX=512m \
  PGMCP_TLC_WORKERS=1 timeout 60 scripts/tlc-capped.sh \
  -config docs/formal/tla/CircularDependenciesScope.cfg \
  docs/formal/tla/CircularDependenciesScope.tla
```

Result: 16 distinct states, 32 generated states, no invariant violations. The
model was refactored from call-sequence exploration to a one-shot arbitrary
request, preserving the per-call safety invariants while avoiding unnecessary
state-history growth.

```bash
cargo nextest run -p pgmcp-testing --test oracle_circular_dependencies --build-jobs 1
```

Result: 4/4 passed. The regression tests cover both planted cycles, negative
max-cycle-length clamping, oversized max-cycle-length capping, and duplicate
project display-name rejection.
