# ADR-026: Read-only `lsp_query` MCP API

- **Status:** Accepted
- **Date:** 2026-06-19
- **Relates to:** ADR-024 (symbol_occurrences), ADR-016 (adaptive tool surface). Files:
  `src/mcp/params/lsp.rs` (`LspOp`, `LspQueryParams`), `src/mcp/tools/tool_lsp_query.rs`,
  handler in `src/mcp/server/handlers/sema.rs`.

## Context

pgmcp held the data behind most Language-Server-Protocol features — `file_symbols`,
`symbol_references`, the resolution tiers, and (new in ADR-024) `symbol_occurrences` with
column offsets — but exposed no LSP-shaped surface. An agent wanting go-to-definition /
find-references / call-hierarchy / hover had to assemble them from a dozen lower-level tools.
The user asked for "an LSP MCP API for read-only analytical operations at the shadow-ASR
level (no file manipulation)."

## Decision

**One tool, `lsp_query`, dispatched on a closed `LspOp` vocab** (ADR-003). One tool rather
than fifteen keeps the adaptive tool catalog small (ADR-016) and is the Occam choice — the
ops share parameter shape and backing tables. Params: `{project, op, file_path?, symbol?,
scope?, limit}`.

Ops and their backing data:

| Op | Backing |
|---|---|
| `document_symbol`, `folding_range` | `file_symbols` for a file |
| `workspace_symbol` | `file_symbols` by fuzzy name across the project |
| `definition` | `file_symbols` defining rows for a name |
| `references`, `document_highlight` | `symbol_occurrences` (scope-aware via `enclosing_symbol_id`) |
| `hover`, `signature_help` | `file_symbols` + `symbol_parameters` + `symbol_effects` |
| `call_hierarchy_incoming` / `outgoing` | `symbol_references` (call edges) |
| `type_hierarchy_super` / `sub`, `implementation` | `symbol_references` (typed edges) |
| `type_definition` | declared `type_tags` → the type's `file_symbols` definition |
| `capabilities` | the op list + backing map (no project required) |

### Design rules

- **Read-only by construction.** No `rename` / `format` / `code_action` / `prepare*` — only
  analytical queries. There is no write path in the tool.
- **Empty ≠ error.** Ops over data a backend doesn't yet populate (e.g. `implementation` for
  a language with no `implements` edges) return an empty result **plus `guidance`** naming
  what would light it up — so an agent learns the coverage boundary instead of hitting a
  wall. `op=capabilities` documents the whole surface.
- **Scope-aware references.** With `scope`, `references` restricts to occurrences lexically
  inside that symbol (ADR-024's `occurrences_in_scope`).
- **Invalid `op` fails closed** with the vocab list.

## Consequences

- Agents get familiar LSP semantics in one call without opening files; coverage grows
  automatically as ADR-024 occurrence extraction and shadow-ASR data fill in.
- Honest degradation: `type_definition` / `implementation` / `type_hierarchy_*` depend on
  typed edges the regex/textual backends may not emit for every language — surfaced via
  `guidance`, not pretended.
- Tested: `lsp_query_lifecycle` real-DB test exercises capabilities / document_symbol /
  workspace_symbol / definition / references / document_highlight / hover + invalid-op
  rejection over seeded symbols and occurrences (also satisfying the Layer-D coverage gate).
