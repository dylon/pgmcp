# Public API Surface Formal Traceability

## Scope

This slice covers `public_api_surface`: request normalization, format
validation, project-id scoping, summary counts, full-format limits, and
read-only behavior.

## Verified Properties

- Project names are trimmed; blank and duplicate display names fail closed.
- `format` is restricted to `summary | full`.
- `limit` is clamped to `1..=2000` and applies only to `format="full"`.
- Summary counts cover all matching public symbols and are not capped by the
  full-format row limit.
- Optional language filters are trimmed and project scoped.
- The tool is read-only and takes no runtime locks.

## Implementation Links

- `src/mcp/tools/tool_public_api_surface.rs`
- `pgmcp-testing/tests/oracle_public_api_surface.rs`

## Mechanical Checks

- TLA+: `docs/formal/tla/PublicApiSurfaceScope.tla`
- Config: `docs/formal/tla/PublicApiSurfaceScope.cfg`
- Focused Rust regression:
  - `cargo nextest run -p pgmcp-testing --test oracle_public_api_surface --build-jobs 1`

## Concurrency Notes

The tool performs read-only SQL. Full-format descriptor enrichment is bounded by
the clamped row limit and performs no writes or advisory locking.
