# pgmcp Integration with AI Agents

How pgmcp wires into Claude Code, Codex CLI, the `~/Papers`/`~/Documents`
document corpus, per-project `.claude/` scanning, and project `.pgmcp.toml`
overrides. For the bare tool surface see [tool-catalog.md](tool-catalog.md).


### Auto-Discovery of Agent Homes

On startup, pgmcp checks whether `~/.claude/` exists and, if so, registers it as
a synthetic **"claude"** project. All indexable files within are scanned and indexed
just like any workspace project. This includes:

- Memory files (`CLAUDE.md`, project memory files)
- Plans and design documents
- Session transcript JSONL files (`projects/*/` session logs)
- Hook scripts and configuration files

A hardcoded `CLAUDE_DIR_EXCLUDES` list filters out noise directories (telemetry,
debug logs, cache, binary snapshots).

pgmcp also checks whether `~/.codex/` exists and registers it as a synthetic
**"codex"** project. Codex stores credentials, sqlite state, shell snapshots,
plugin checkouts, and caches in the same directory, so pgmcp uses an allow-list:
`config.toml`, `history.jsonl`, `memories/**`, and `sessions/**/*.jsonl`.

### Document Indexing — `~/Papers/` and `~/Documents/`

Beyond source code, pgmcp can index **personal document corpora**: academic
papers, notes, invoices, manuals — the kinds of things you'd want to grep or
semantic-search even though they aren't in a git repo.

On startup, pgmcp checks for two well-known document directories and registers
them as synthetic projects when they exist:

| Directory       | Project name | Typical contents                              |
|-----------------|--------------|-----------------------------------------------|
| `~/Papers/`     | `Papers`     | Academic PDFs, LaTeX source, EPUB textbooks   |
| `~/Documents/` | `Documents`  | Notes (ORG/MD/RST), invoices, DOCX, ODT, RTF  |

No `.git/` is required; the directory's mere existence enables it. Users
without these directories pay no cost (the daemon `is_dir()`-guards both).

#### Supported formats

| Extension      | Language       | Storage form                                 |
|----------------|----------------|----------------------------------------------|
| `pdf`          | `pdf`          | `pdftotext -layout`, NFKC + dehyphenated     |
| `ps` / `eps`   | `postscript`   | `ps2ascii`, NFKC                             |
| `docx`         | `docx`         | `pandoc --to plain`                          |
| `doc`          | `doc`          | `pandoc --to plain` (needs antiword/catdoc)  |
| `rtf`          | `rtf`          | `pandoc --to plain`                          |
| `odt`          | `odt`          | `pandoc --to plain`                          |
| `epub`         | `epub`         | `pandoc --to plain`                          |
| `tex`/`latex`  | `latex`        | `pandoc --to plain` (strips markup)          |
| `org`          | `org`          | `pandoc --to plain` (strips markup)          |
| `rst`          | `rst`          | UTF-8 passthrough + normalization            |
| `bib`          | `bibtex`       | UTF-8 passthrough + normalization            |
| `txt`          | `text`         | UTF-8 passthrough + normalization            |

The extraction layer routes binary formats through system tools (see
Prerequisites) and applies a single **normalization pass** to all outputs:
NFKC Unicode, dehyphenation of line-break-split words, page-number-line
stripping, control-character removal, whitespace collapse. The result is
stored verbatim in `file_chunks.content` so MCP tool responses are
already token-efficient — no separate wire format needed.

#### Source-form preference

When several forms of the same document coexist in one directory — e.g.
`invoice.org`, `invoice.tex`, `invoice.pdf` — pgmcp indexes **only the source
form**, not the build output. The default priority (configurable per project
in `.pgmcp.toml`) is:

```
org > rst > md > tex > latex > docx > epub > odt > rtf > pdf > ps > eps > doc
```

Files whose extension isn't in the priority list (e.g. `.csv`) are kept
unconditionally — they can't be deduplicated against anything.

#### Content-based dedup and rename detection

For document corpora, the same PDF often ends up in two places (download
folder + organized archive) or gets moved as the library is reorganized.
pgmcp detects both cases via content hashing **before** extraction:

- **Rename** — same content, different path, old path is gone on disk:
  the existing canonical row's path is updated in place; chunks and
  embeddings are reused.
- **Cross-path duplicate** — same content, different path, old path still
  present: insert a metadata-only row pointing at the canonical via
  `duplicate_of_file_id`. Chunk-bearing queries follow the pointer
  transparently via `COALESCE(duplicate_of_file_id, id)`.

The savings are large: moving or duplicating a 50-page PDF is now O(stat)
instead of triggering subprocess extraction + GPU embedding. Counters
`documents_renamed` and `documents_deduplicated` in `index_stats` surface
the impact.

#### Recommended agent workflow (token-efficient)

For documents projects, prefer chunk-level retrieval over file-level:

- `semantic_search project=Papers query="attention mechanism" limit=5` —
  ~5 chunks (~2-3k tokens) targeted at the question.
- `grep pattern="error budget" project=Documents before_context=2 after_context=2`
  — chunk-anchored regex matches with surrounding lines, ~500 tokens per
  hit instead of whole-file (~20-50k tokens).
- `read_file path=~/Papers/<sample>.pdf start_line=100 end_line=150` —
  pulls only the requested line range from indexed chunks, stitched and
  trimmed. Works even when `indexed_files.content` is NULL (Level-1
  oversized files): chunks are stitched on demand.
- `read_file path=~/Papers/<sample>.pdf chunk_index_start=5 chunk_index_end=6`
  — alternate chunk-indexed addressing for paging through long documents.

`file_info` reports `chunk_count`, `first/last_chunk_line`, and
`extracted_kind` (e.g. `pdf_text`, `docx_text`, `latex_plain`) so the agent
can plan further reads in one round-trip.

#### Per-project `.pgmcp.toml`

Drop a `.pgmcp.toml` into `~/Papers/` or `~/Documents/` to override defaults:

```toml
[indexer]
# Override the 1 MiB default for binary docs (default 100 MiB):
max_document_source_bytes = 209715200   # 200 MiB

# Per-project priority replacement (note: replace semantics, not merge):
source_priority = ["org", "tex", "latex", "rst", "md", "epub", "pdf", "ps", "eps"]

# Exclude LaTeX build artifacts in addition to the hardcoded defaults:
exclude_patterns = [
    "*.aux", "*.log", "*.out", "*.toc",
    "*.synctex.gz", "*.fls", "*.fdb_latexmk",
    "supplementary/", "submissions-archive/",
]

# Documents typically aren't in git; turn history indexing off explicitly.
[git]
index_history = false
```

### Project-Level `.claude/` Scanning

For each discovered project, pgmcp also scans its `.claude/` subdirectory (if
present). Files found there -- memory files, plans, session transcripts -- are
indexed as part of the parent project, so searches against that project include
its Claude Code context.

### Claude JSONL Session Transcript Parsing

Claude Code session transcripts are stored as JSONL files. pgmcp includes a
dedicated parser (`claude_chunker`) that extracts meaningful messages:

- **User messages** -- the prompts you sent
- **Assistant messages** -- Claude's responses (text content)
- **Tool results** -- output from tool calls

Each extracted message becomes a separate chunk with its own embedding, making
session history semantically searchable. Generic (non-Claude) JSONL files are
chunked one line per chunk.

### Codex JSONL Session and History Parsing

Codex session rollouts live under `~/.codex/sessions/YYYY/MM/DD/*.jsonl`, and
prompt history lives at `~/.codex/history.jsonl`. pgmcp extracts user messages,
assistant responses, tool calls, and bounded tool outputs while skipping
developer/system instructions, reasoning records, encrypted payloads, token
counts, invalid JSON lines, and oversized tool output.

Both synthetic projects live in the same PostgreSQL index. Claude can search
Codex history with `project: "codex"`, and Codex can search Claude history with
`project: "claude"`.

### Auto-RAG Hooks

pgmcp can automatically inject relevant context into every Claude Code session
and prompt via two hooks. No manual tool calls needed.

Codex CLI supports MCP server registration, so it can query pgmcp tools directly.
It does not currently expose Claude-style prompt hooks in the local CLI surface,
so automatic prompt-time injection is Claude-specific.

**SessionStart Hook** -- runs `pgmcp context` when a Claude Code session begins.
Injects a markdown summary containing the project name, root path, file count,
language breakdown, and file tree.

**UserPromptSubmit Hook** -- runs `~/.claude/hooks/pgmcp-rag.sh` on every user
prompt. Queries the daemon's `POST /api/search` endpoint with the prompt text and
injects up to 5 semantically relevant code snippets. Short prompts (< 30 chars)
are skipped. 2-second timeout with graceful fallback.

**Configuration** -- add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "/usr/local/bin/pgmcp context",
            "timeout": 10000
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.claude/hooks/pgmcp-rag.sh",
            "timeout": 3000
          }
        ]
      }
    ]
  }
}
```

**Hook Script** -- place at `~/.claude/hooks/pgmcp-rag.sh` (`chmod +x`):

```bash
#!/bin/bash
# pgmcp RAG hook -- injects relevant indexed code into Claude's context
# Reads user prompt from stdin JSON, queries pgmcp daemon for relevant snippets

set -e

INPUT=$(cat)
PROMPT=$(echo "$INPUT" | jq -r '.prompt // empty')

# Skip short prompts (commands like "yes", "continue", "ok")
if [ ${#PROMPT} -lt 30 ]; then
    exit 0
fi

# Query pgmcp daemon for semantically relevant code
RESULTS=$(curl -s -m 2 "http://localhost:3100/api/search" \
    -H 'Content-Type: application/json' \
    -d "{\"query\": $(echo "$PROMPT" | jq -Rs .), \"limit\": 5}" 2>/dev/null) || exit 0

# Check if we got results
RESULT_COUNT=$(echo "$RESULTS" | jq -r '.results | length // 0' 2>/dev/null) || exit 0
if [ "$RESULT_COUNT" -eq 0 ]; then
    exit 0
fi

# Format results as context
echo "## pgmcp: Relevant indexed code"
echo ""
echo "$RESULTS" | jq -r '.results[] | "### \(.file_path) (similarity: \(.similarity | tostring | .[0:4]))\n```\(.language)\n\(.chunk)\n```\n"' 2>/dev/null || exit 0

exit 0
```

Requires `jq` and `curl` on the system PATH.

---

## pgmcp Utilization (Claude Code Integration)

The auto-RAG hook above enriches *every* prompt, but Claude Code still defaults
to built-in `Read`/`Grep`/`Glob` for many exploration steps where pgmcp tools
would produce better results (cross-project semantic queries, graph-aware
analysis, topic clustering). To bias Claude toward pgmcp's tools, pgmcp ships
three complementary mechanisms:

1. **Tool-call proxy via `PreToolUse` hooks** — augment or selectively deny
   `Read`/`Grep`/`Glob` calls at the harness level.
2. **Subagent containment via `~/.claude/agents/` overrides** — drop `Grep`/`Glob`
   from spawned-subagent tool catalogs entirely.
3. **Per-tool invocation counters** in `/api/status` — measure utilization to
   A/B-test whether the above are working.

The full design rationale (including why an HTTP-level proxy was rejected) lives
at `~/.claude/plans/thoroughly-examine-home-dylon-workspace-melodic-cake.md`.
The user-side reference implementation lives in `~/.claude/hooks/` and
`~/.claude/agents/`.

### `PreToolUse` Hooks (Layer A: Augment + Layer B: Enforce)

Six hook scripts ship at `~/.claude/hooks/`, plus a shared library at
`~/.claude/hooks/lib/pgmcp-common.sh`. All are non-blocking: they exit 0
silently when the daemon is down (verified via 300 ms `GET /health`) so a
pgmcp outage never blocks the user.

**Layer A — augmenting hooks (always on, model-discretionary):**

| Hook                              | Matcher  | Behavior                                                                                                                                                               |
|-----------------------------------|----------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `pgmcp-read-context.sh`           | `Read`   | Calls `POST /api/file_envelope` and injects a 5-line context block (language, size, indexed_at, etc.) alongside the file content.                                      |
| `pgmcp-grep-companion.sh`         | `Grep`   | When the path is broad (whole repo or no specific path), calls `POST /api/grep` and injects up to 10 cross-project hits alongside the native Grep result.             |
| `pgmcp-glob-suggestion.sh`        | `Glob`   | When the pattern is broad (`**/*.rs` from project root), emits a one-line suggestion to use `mcp__pgmcp__orient`/`semantic_search`/`project_tree` instead.            |

Augmenting hooks emit `additionalContext` and never block tool execution. They
are model-discretionary — the model decides whether to act on the injected
context.

**Layer B — enforce hooks (opt-in, harness-enforced):**

| Hook                              | Matcher  | Behavior                                                                                                                                                                     |
|-----------------------------------|----------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `pgmcp-grep-enforce.sh`           | `Grep`   | When `PGMCP_HOOK_MODE=enforce` AND path is broad AND pattern length ≥ 3 chars, returns `permissionDecision: "deny"` and tells the model to use `mcp__pgmcp__grep` instead.   |
| `pgmcp-glob-enforce.sh`           | `Glob`   | When `PGMCP_HOOK_MODE=enforce` AND pattern is broad, returns `permissionDecision: "deny"` and tells the model to use `mcp__pgmcp__project_tree` or `mcp__pgmcp__orient`.    |

Enforce hooks use the same `permissionDecision: "deny"` primitive as
`~/.claude/git-guard.sh` — the harness refuses the tool call regardless of
model intent. There is **no** enforce hook for `Read` (too disruptive for
read-after-write and `.gitignore`'d files).

**Mode summary:**

| Mode (`PGMCP_HOOK_MODE`)  | Activation                  | What happens                                                                                                              |
|---------------------------|-----------------------------|---------------------------------------------------------------------------------------------------------------------------|
| `augment-only` (default)  | Always on                   | Layer A injects context; Layer B short-circuits. Soft nudging.                                                            |
| `enforce`                 | `PGMCP_HOOK_MODE=enforce …` | Layer B returns `permissionDecision: "deny"` for broad `Grep`/`Glob`. Native tool still allowed for narrow patterns.       |
| `permissive`              | `PGMCP_HOOK_MODE=permissive`| Same as `augment-only`; explicit override for sessions where enforce is configured per-project but the user wants out.    |

**Configuration** -- add to `~/.claude/settings.json` `PreToolUse` array
(alongside any existing `Bash`/etc. entries):

```json
{ "matcher": "Read",  "hooks": [
  { "type": "command", "command": "~/.claude/hooks/pgmcp-read-context.sh",   "timeout": 2000 }
]},
{ "matcher": "Grep",  "hooks": [
  { "type": "command", "command": "~/.claude/hooks/pgmcp-grep-companion.sh", "timeout": 3000 },
  { "type": "command", "command": "~/.claude/hooks/pgmcp-grep-enforce.sh",   "timeout": 1500 }
]},
{ "matcher": "Glob",  "hooks": [
  { "type": "command", "command": "~/.claude/hooks/pgmcp-glob-suggestion.sh","timeout": 1000 },
  { "type": "command", "command": "~/.claude/hooks/pgmcp-glob-enforce.sh",   "timeout": 1000 }
]}
```

The two `Grep` and two `Glob` matchers chain — both run for each tool call.
The enforce hook short-circuits unless `PGMCP_HOOK_MODE=enforce` and conditions
match, so the chain is harmless when enforce is off.

**Shared library** at `~/.claude/hooks/lib/pgmcp-common.sh` provides:

- `pgmcp_health_ok` — 300 ms `GET /health` probe; daemon down → fail-fast
- `pgmcp_emit_context` — shape `additionalContext` JSON for augmenting
- `pgmcp_emit_deny` — shape `permissionDecision: "deny"` JSON for enforce
- `pgmcp_dedup_check` — TTL-based dedup keyed on `~/.claude/hooks/.pgmcp-cache/`
  to prevent the same pattern from re-injecting context multiple times within
  3 minutes (avoids context bloat)

Requires `jq` and `curl` on the system PATH.

### Subagent Tool-Catalog Overrides (`~/.claude/agents/`)

Spawned subagents (via the `Agent` tool — `Explore`, `general-purpose`, etc.)
run as independent Claude instances and **do not invoke the parent session's
`PreToolUse` hooks**. The hooks above only constrain the main session.

To constrain subagents, override the built-in agent definitions to drop
`Grep`/`Glob` from their tool catalog. The harness will not surface those tools
to the subagent — it literally cannot call them.

**Setup** -- create `~/.claude/agents/Explore.md` (and similarly for
`general-purpose.md`) with YAML frontmatter:

```markdown
---
name: Explore
description: Fast read-only search agent for locating code...
model: inherit
tools: Bash, Read, WebFetch, WebSearch, mcp__pgmcp__semantic_search, mcp__pgmcp__text_search, mcp__pgmcp__grep, mcp__pgmcp__hybrid_search, mcp__pgmcp__read_file, mcp__pgmcp__list_projects, mcp__pgmcp__project_tree, mcp__pgmcp__file_info, mcp__pgmcp__orient, ...
---

ALWAYS prefer pgmcp tools when available. The built-in Grep, Glob,
NotebookEdit, Edit, and Write tools have been removed from your
tool catalog — this is intentional. For exploration use
mcp__pgmcp__grep, mcp__pgmcp__semantic_search, mcp__pgmcp__hybrid_search.
```

Resolution order: user-level overrides at `~/.claude/agents/<Name>.md` win
over Claude Code's built-in agent definitions for the same name.

`Bash` and `Read` are kept because some legitimate cases (read-after-write,
ungit'd files) need them. Edit/Write/NotebookEdit are kept on `general-purpose`
(it does write code) but dropped from the read-only `Explore`.

### Measuring Utilization

`StatsTracker::tool_invocations` (a `DashMap<String, AtomicU64>`) records every
MCP tool call by name. Surface in the `/api/status` response under
`counters.tool_invocations`:

```bash
curl -s http://localhost:3100/api/status | jq '.counters.tool_invocations'
# {
#   "semantic_search": 142,
#   "grep": 23,
#   "orient": 8,
#   "centrality_analysis": 4,
#   ...
# }
```

Compare with the count of `Read`/`Grep`/`Glob` invocations in
`~/.claude/projects/*/...jsonl` transcripts (which pgmcp itself indexes as the
`claude` project) to compute a utilization ratio. Recommended baselines:

- Capture one week before installing the hooks/overrides (no measurement
  changes, just a snapshot).
- Capture another week after each layer ships (Stage 3 server-side rewrites,
  Stage 5a agent overrides, Stage 1 hooks).
- Track ratio `mcp__pgmcp__* / (Read + Grep + Glob)` per session and the
  count of `mcp__pgmcp__orient` in the first 3 tool calls of each session.

See `docs/scientific-ledger/recovery-times-2026-04-28.md` for related
empirical-baseline methodology.

---

