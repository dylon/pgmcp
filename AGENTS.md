# pgmcp — Codex Working Rules

## Verification

Before declaring code changes complete, run:

```bash
./scripts/verify.sh
```

Focused `cargo test` or `cargo clippy` runs are useful during iteration, but
they do not replace the full verification gate.

## Project Notes

- CUDA is mandatory; there is no CPU-only cargo feature.
- The main binary and library expose the same module tree. Add new top-level
  modules to both `src/main.rs` and `src/lib.rs` when applicable.
- `pgmcp` serves one shared MCP index. Claude Code and Codex CLI can both query
  synthetic agent projects such as `claude` and `codex` when connected to the
  same daemon.
- Keep transcript parsers conservative: index useful user/assistant/tool text,
  and skip credentials, encrypted payloads, reasoning internals, cache/state,
  and oversized tool output.
