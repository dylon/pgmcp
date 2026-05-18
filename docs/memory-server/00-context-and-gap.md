# 00 — Context, landscape, SOTA, current state, gap

This document gives the rationale behind the memory-server initiative:
why it exists, what's out there today, what's state-of-the-art, what
pgmcp already has, and where the gap is. The roadmap that follows from
this analysis lives in [`02-phases.md`](02-phases.md); the resolved
design decisions are in [`01-decisions.md`](01-decisions.md).

> Original planning artifact:
> `~/.claude/plans/what-is-a-memory-idempotent-lovelace.md` §1–§6.

---

## 1. Context

### Why this exists

Claude Code, Codex, and similar coding agents have bounded context
windows (currently 200k–1M tokens). Long-running projects, repeated
user preferences, prior architectural decisions, and incident history
all live *outside* the window. A **memory server** is the
architectural answer: an external service the agent reads/writes via
tool calls so that effective memory is bounded by storage, not
context.

pgmcp already does much of what a memory server does — durable
storage, semantic search, session observation, mandate promotion —
but it was built as a **code indexer first** and a memory store
second. The question is what's needed to make it explicitly fill the
memory-server role at SOTA quality, while keeping the rest of its
functionality.

### Intended outcome

- A clear picture of the existing memory-server landscape.
- A SOTA map (academic + applied) of what advanced systems look like.
- A concrete gap analysis (pgmcp vs the field).
- A phased proposal for extending pgmcp toward SOTA.
- A canonical, in-repo design record so the rationale isn't trapped
  in `~/.claude/plans/`.

---

## 2. What is a memory server

An MCP **memory server** is an external service that lets an LLM agent
externalize state across sessions. The model writes facts via tool
calls and queries them back later; the *effective* memory is bounded
by storage, not by the context window. Three properties combine to
give "effectively infinite" memory:

1. **Externalized state.** Writes go to disk/DB, not the prompt.
2. **Lazy retrieval.** Only the 5–10 relevant fragments enter context
   per query.
3. **Cross-session durability.** Memories survive conversation
   boundaries and tool restarts.

Memory servers vary along several axes:

- **Schema** — knowledge graph (entities/relations/observations) vs.
  flat vectors vs. tiered memory blocks vs. temporal graph.
- **Retrieval** — keyword/substring vs. dense vector vs. graph
  traversal vs. hybrid.
- **Write path** — explicit user/agent writes vs. LLM-extracted
  salience vs. reflection-driven consolidation.
- **Persistence** — JSON file vs. SQLite vs. Postgres+pgvector vs.
  Neo4j vs. cloud-managed.
- **Scope** — per-user, per-session, per-agent, shared.

---

## 3. The existing MCP memory-server landscape

### 3.1 Official `@modelcontextprotocol/server-memory`

The Anthropic reference implementation. Deliberately minimal —
designed as a teaching example, not a scale target.

- **Persistence:** JSONL file on local disk. Path via
  `MEMORY_FILE_PATH` env var (default `memory.jsonl`). No DB, no
  index.
- **Schema:** knowledge-graph triple model.
  - **Entity** — `name` (unique id), `entityType`,
    `observations: string[]`
  - **Relation** — `from`, `to`, `relationType` (active-voice verb
    phrase)
  - **Observation** — `entityName`, `contents: string[]`
- **Tools (9):** `create_entities`, `create_relations`,
  `add_observations`, `delete_entities`, `delete_relations`,
  `delete_observations`, `read_graph`, `search_nodes`, `open_nodes`.
- **Retrieval:** substring/keyword match across entity names, types,
  and observations. No embeddings. No vector similarity. No graph
  traversal beyond `open_nodes` (single-hop).

This is the **operational baseline** any "memory server" is compared
against. Source:
<https://github.com/modelcontextprotocol/servers/tree/main/src/memory>.

### 3.2 mem0 (mem0ai/mem0)

Higher-scale, opinionated, LLM-driven salience.

- **Persistence:** vector store (default Qdrant; also
  Postgres+pgvector, Chroma, Weaviate, etc.) + optional graph backend
  + KV.
- **Memory model:** tiered by **scope** (User / Session / Agent), not
  by cognitive type.
- **Write path:** single-pass LLM extraction of facts/triplets from
  conversation; entity-linking boosts cross-memory relevance.
- **Retrieval:** hybrid — semantic + BM25 + entity matching, scored
  in parallel.
- **MCP surface:** the original "OpenMemory MCP" is deprecated as of
  2026 in favor of the self-hosted Mem0 server. Community MCP
  servers exist (`coleam00/mcp-mem0`, Postgres-backed, exposing
  `save_memory`, `get_all_memories`, `search_memories`).

### 3.3 Letta / MemGPT

OS-inspired tiered memory; primarily an agent runtime, secondarily an
MCP surface.

- **Memory tiers:**
  - **Core / in-context memory blocks** — directly in the prompt; the
    agent edits via tool calls; blocks attachable/detachable across
    agents.
  - **Archival memory** — DB-backed, retrieved on demand via tools.
  - **Recall memory** — message history accessible after eviction.
- **Eviction:** LLM decides when to swap content between core and
  archival (the OS-paging analogy).
- **MCP:** Letta can both host an MCP server and act as a client. It
  is *not* packaged as a drop-in memory MCP server the way the
  official server is — Letta wants you to use its full agent
  runtime.

### 3.4 Graphiti / Zep

Temporal knowledge graph; the "facts have validity intervals" thesis.

- **Persistence:** Neo4j (primary), FalkorDB, Kuzu, Neptune.
- **Schema:** Entities / Facts (edges with **bi-temporal** validity:
  `t_valid_from`, `t_valid_to`, plus ingestion time) / Episodes (raw
  provenance) / custom Pydantic types.
- **Distinguishing feature:** facts are **versioned over time, never
  destructively overwritten** — contradictions invalidate prior facts
  rather than overwriting them.
- **Retrieval:** hybrid — embeddings + BM25 + graph traversal.
- **MCP server:** ships natively.
- **Zep** is the managed/production layer above Graphiti with the
  same thesis.

### 3.5 Others worth naming

- **Supermemory** — MCP-first, coding-agent oriented, cloud-hosted.
- **Cognee, LangMem, MemoClaw, OMEGA, SuperLocalMemory** — appear in
  2026 comparison literature; lower-profile.

---

## 4. State of the art (2025–2026)

The field has moved well past "store and retrieve via dense vectors."
SOTA systems are characterized along nine axes.

### 4.1 Memory taxonomy

Two dominant decompositions coexist and the field has **not
converged**.

- **Cognitive-science taxonomy** — working / episodic / semantic /
  procedural / reflective. Operationalized for agents by *Generative
  Agents* (Park et al. 2023, arXiv:2304.03442) and formalized in
  **CoALA** (Cognitive Architectures for Language Agents).
- **Systems taxonomy** — message buffer / core memory blocks / recall
  / archival, à la **MemGPT** (Packer et al. 2023, arXiv:2310.08560)
  and Letta. Analogous to RAM/disk paging, not human memory regions.

mem0 sits in between — LLM-extracted "memories" + optional graph
variant. Most production systems pick a hybrid.

### 4.2 Hierarchical retrieval

Flat ANN fails on multi-hop and thematic queries. Three approaches
dominate, and the field has converged on "hierarchy beats flat for
non-trivial queries" — but **not** on which hierarchy.

- **RAPTOR** (Sarthi et al., ICLR 2024, arXiv:2401.18059) — recursive
  cluster-then-summarize tree; query at multiple abstraction levels.
  +20% on QuALITY over flat RAG. Indexing is O(n log n) embeddings
  plus per-level LLM summaries.
- **GraphRAG** (Microsoft, arXiv:2404.16130) — LLM extracts
  entities/relations, Leiden community detection, per-community
  summaries. Excels at "global" thematic queries (72–83%
  comprehensiveness vs RAG baselines). Expensive to build.
- **HippoRAG** (Gutierrez et al., NeurIPS 2024, arXiv:2405.14831) —
  schemaless KG + Personalized PageRank from query concepts. Matches
  iterative retrieval (IRCoT) at 10–30× lower cost on multi-hop QA.

### 4.3 Active / write-time memory operations

The write path is where 2024–2026 systems diverged from naive RAG.

- **Reflection** (Generative Agents) — when accumulated importance
  exceeds a threshold, the agent self-prompts for higher-order
  observations stored as new memories.
- **LLM-driven extraction** (mem0) — entity-relation triplets with
  explicit conflict detection/resolution on update.
- **Bi-temporal facts** (Zep/Graphiti, arXiv:2501.13956) — edges
  carry `t_valid_from`/`t_valid_to`; contradictions invalidate, not
  overwrite.
- **Three generations** of write-time operations are now described
  (arXiv:2603.07670): rule-based → LLM-judged → RL-trained policies
  (AgeMem is an early RL example).

**Field state:** LLM-judged extraction is de facto best practice for
personal/conversational memory; rule-based still dominates for code
indexes (which is what pgmcp is today).

### 4.4 Hybrid retrieval

The 2025 production recipe has **converged**: **dense + sparse +
(optional) graph, fused by RRF, reranked by cross-encoder** on the
top 50–100.

- **BM25** or learned-sparse **SPLADE++** handles exact lexical
  matches and rare tokens.
- **Dense vectors** handle paraphrase.
- **RRF** (Cormack 2009) — the boring, undefeated fusion baseline,
  typically +15–30% over pure vector.
- **ColBERTv2** (arXiv:2112.01488) late-interaction wins when
  token-level recall matters (long docs, code) at higher storage
  cost.

### 4.5 Embeddings frontier

MTEB late 2025: **instruction-tuned LLM-encoders dominate.**

- Leaders for English general: **E5-Mistral-7B-instruct**
  (arXiv:2401.00368), Qwen3-Embedding, BGE-en-icl.
- **BGE-M3** — dense + sparse + multi-vector in one model, 100+
  languages, Matryoshka-truncatable.
- **Matryoshka Representation Learning** (arXiv:2205.13147) — store
  1024d, query at 64–256d for ANN, retrieve at full for rerank.
  Standard in most 2025 releases (Nomic v1.5, ModernBERT,
  jina-emb-v3).
- **For code:** Voyage-code-3 and Jina-code-v2 lead public
  benchmarks.
- **pgmcp today:** `all-MiniLM-L6-v2`, 384d. **Two generations
  behind.**

### 4.6 Personalization & user modeling

Contested.

- **mem0** — implicit: LLM extracts preferences from conversation,
  stores as facts/triplets, surfaces on every relevant query.
- **Letta** — explicit: agents edit memory blocks via tool calls.
- **PEARL** (arXiv:2404.15269) — latent-preference learning from user
  edits to outputs.
- **Surprising finding:** plain RAG over raw conversation often beats
  mem0-style extraction for personalization while being cheaper.
  Extraction wins on latency/cost at scale, not necessarily accuracy.

### 4.7 Eviction & consolidation

Under-developed. Production systems use crude policies (time-decay,
size caps, recency-weighted, or nothing). Research is moving toward:

- LLM-judged importance scoring at write time.
- Semantic merge — cluster + summarize duplicates.
- ACT-R-style retrieval-strength decay (*Forgetful but Faithful*,
  arXiv:2512.12856).
- Zep's bi-temporal invalidation as a middle ground for replaceable
  facts.

"Crude works surprisingly well" is the honest summary. **No winner
yet.**

### 4.8 Evaluation

Two benchmarks dominate.

- **LoCoMo** — 1,540 questions across single-hop, multi-hop,
  open-domain, and temporal categories on multi-session
  conversations.
- **LongMemEval** (Wu et al., ICLR 2025) — 500 questions across 6
  categories; particularly hard on knowledge-update and
  multi-session.

Leaderboards (late 2025/2026): ByteRover 2.0 reports 92.2% on LoCoMo;
OMEGA 95.4% on LongMemEval; LiCoMemory 73.8% on LongMemEval-S
(GPT-4o-mini). **Vendor leaderboards should be treated skeptically**
— methodology varies, contamination is plausible. **BEAM** also
relevant for episodic recall.

### 4.9 Open challenges (unsolved as of 2026)

- **Long-horizon consistency** when reflections accumulate and drift.
- **Contradiction handling** beyond replaceable facts — semantic
  disagreements.
- **Multi-agent shared memory** — race conditions look like model
  errors; consistency models under-specified (arXiv:2603.10062).
- **Privacy / right-to-be-forgotten** — selective deletion with
  provenance is mostly unsolved.
- **Evaluation honesty** — benchmark contamination, vendor inflation.
- **Forgetting policies in general** — research gap.

---

## 5. What pgmcp already has

Verified by source audit. Citations are `path:line` against the
repository at the time of writing.

### 5.1 Persistent vector store (the "memory backbone")

- `file_chunks` table — chunk-level embedding, language, file path,
  line range, project linkage.
- pgvector HNSW index, m=24, ef_construction=200, ef_search=100;
  rebuilt only on `[vector]` param change
  (`src/db/migrations.rs:1034-1077`).
- Embedding model: `all-MiniLM-L6-v2`, 384d. **Two generations behind
  SOTA** — see §4.5.
- Reachable via `semantic_search`, `text_search`, `grep`,
  `hybrid_search` (already implements RRF: BM25 + vector),
  `read_file`, `file_info`.

### 5.2 Cross-session continuity: the `claude` synthetic project

- Auto-registered in `src/indexer/scanner.rs:166-180`. Noise excludes
  at `src/indexer/scanner.rs:18-33`.
- JSONL session transcripts parsed by
  `src/indexer/claude_chunker.rs`; generic JSONL by
  `chunker.rs::chunk_jsonl_content`; file-history cross-refs by
  `parse_file_history_map`.
- Past sessions, plans (`~/.claude/plans/*.md`), memory files,
  decisions — all searchable via
  `semantic_search(project: "claude", ...)`.
- The `pgmcp context` CLI explicitly recommends this path
  (`src/cli/context.rs:79-83, 110-114`).
- **This already gives pgmcp most of what a "remember across
  sessions" memory server provides** — writes happen via the
  filesystem (Claude Code's auto-memory + plan files), not via
  dedicated MCP tool calls.

### 5.3 Session mandates (`src/sessions.rs`)

- **Polarity taxonomy** — `enum MandatePolarity` at
  `src/sessions.rs:30-56`, 12 variants (snake_case):
  `always · never · prefer · avoid · remember · from_now_on ·
  correction · permission · constraint · mandate · process_rule ·
  project_rule`. DB CHECK at `src/db/migrations.rs:798-805` enforces
  the same set.
- **Auxiliary enums** — `CueTier { A..F }` (default D),
  `MandateStatus { Active, Superseded, Retired, Promoted }`.
- **Observation flow** — `~/.claude/hooks/pgmcp-rag.sh` POSTs
  `{session_id, cwd, prompt}` to `POST /api/session/observe` (route
  `src/cli/daemon.rs:457-460`; handler
  `src/api/handlers.rs:268-347`). Pipeline: resolve project by
  longest-cwd-prefix → `upsert_session` → sha256+embed →
  `insert_prompt` → tiered regex `extract_mandates` →
  `upsert_mandate` per hit → `list_active_mandates(20)` → optional
  `semantic_search` RAG → render `additional_context` Markdown
  (≤ 2 KB).
- **Table** `session_mandates` (`src/db/migrations.rs:741-756`):
  `id BIGSERIAL, session_id UUID FK, source_prompt_id BIGINT FK,
  polarity TEXT, imperative TEXT, target TEXT, cwd_prefix TEXT,
  cue_tier CHAR(1), salience REAL, status TEXT, created_at,
  last_reinforced_at, reinforcement_count INT`. UNIQUE on
  `(session_id, polarity, lower(imperative))` so re-extraction bumps
  `reinforcement_count` rather than duplicating.
- **Replay** — `additional_context` injected on every prompt; MCP
  `session_mandates` tool and `mandate_context(session_id=…)`
  re-expose to the agent.

### 5.4 Durable mandates

- **Table** (`src/db/migrations.rs:781-792`):
  `durable_mandates(id, scope TEXT, project_id INT FK, polarity,
  imperative, target, source_mandate_id BIGINT FK→session_mandates,
  promoted_at, file_path)`. CHECK
  `scope IN ('project','workspace')`
  (`src/db/migrations.rs:818-823`).
- **Promotion** — MCP `promote_session_mandate`
  (`src/mcp/server.rs:~1903`) calls `sessions::promote_mandate`
  (`src/sessions.rs:1098-1126`): tx-copies the row, flips source
  status to `'promoted'`, optionally appends to `target_file` under
  a `## Promoted session mandates (pgmcp)` marker (idempotent on
  re-run).
- **Retrieval** — exactly one reader:
  `list_durable_mandates_for_project`
  (`src/sessions.rs:1128-1141`). It dumps
  `WHERE project_id = $1 OR scope = 'workspace' ORDER BY promoted_at DESC`.
  **No search surface** — no semantic, no FTS, no polarity-filter
  query. Surfaced through `mandate_context` and the
  `pgmcp://project/{name}/mandates` resource.

### 5.5 Session prompts archive — and a sleeping asset

- **Table** (`src/db/migrations.rs:728-739`):
  `session_prompts(id, session_id FK, ts, prompt_text,
  prompt_sha256 CHAR(64), embedding vector(384),
  UNIQUE(session_id, prompt_sha256))`. HNSW index
  `idx_session_prompts_embedding` rebuilt only on `[vector]` change.
- **Writers only.** `insert_prompt` (`src/sessions.rs:944-967`) is
  the sole call site. A schema-wide grep for `SELECT … FROM
  session_prompts` returns **zero hits**. **The embedding column is
  populated on every prompt but never read.** The
  cross-session-retrieval claim describes design intent, not
  implementation.
- Only "query" against rows is the `ON CONFLICT (session_id,
  prompt_sha256) DO UPDATE SET ts = NOW()` dedupe.
- **Implication:** pgmcp already has a fully embedded, deduped
  cross-session prompt archive. Surfacing it as a tool is the
  cheapest possible memory-server feature.

### 5.6 Topic clustering (Fuzzy BERTopic = FCM + c-TF-IDF)

- Soft clustering of chunks into keyword-labeled topics.
- `code_topics` + `chunk_topic_assignments` tables.
- Cron job `topic-clustering` (default every 12h).
- Tools: `discover_topics`, `topic_hierarchy`, `topic_hierarchy_fcm`,
  `find_orphans`, `find_misplaced_code`.

### 5.7 Graph layer

- `code_graph_edges` table (`src/db/migrations.rs:389-407`) —
  file-to-file edges keyed by `indexed_files.id`. Edge types
  `Import | CoChange | Semantic` (`src/graph/mod.rs:35-50`).
  **Strictly derived from source code; not user-asserted facts.**
- `file_metrics` table — centrality, coupling, churn, ML scores.
- Tools: `dependency_graph`, `centrality_analysis`,
  `community_detection`, `circular_dependencies`,
  `change_impact_analysis`.

### 5.8 Cross-project similarity

- `cross_project_similarities` materialized table.
- Tools: `compare_files`, `find_similar_modules`, `find_duplicates`,
  `refactoring_report`.

### 5.9 Composite orientation

- `orient` MCP tool (`src/mcp/server.rs:~1849`) — bundles project
  metadata, language, tree, PageRank entry points, recent files, top
  topics, health envelope. The "wake up and remember what's going on
  here" tool.
- `mandate_context` (`src/mcp/server.rs:~1869`) — file-backed
  AGENTS.md/CLAUDE.md/.pgmcp.toml bundle + (when `session_id`)
  active session mandates + project-scoped durable mandates.

### 5.10 What is **not** there

A `CREATE TABLE` grep for `entit*`, `relation*`, `knowledge*`,
`observation*`, `fact*` returns **zero matches**. There are no tools
named `remember`, `recall`, `memory`, or `context_bundle`.
Concretely missing surfaces:

- No user-fact tables (entity / relation / observation).
- No write-side fact API. `POST /api/session/observe` accepts only a
  single `prompt` string and routes it through the regex extractor.
- No open-vocabulary recall — "what do I know about $TOPIC" has no
  endpoint. `session_mandates` is keyed by `session_id`/`cwd`;
  `durable_mandates` is per-project dump only.
- No entity disambiguation / merging across sessions. The closest is
  the `UNIQUE(session_id, polarity, lower(imperative))` dedupe —
  only within one session, and only on textual exact match after
  lower-casing.
- No relation traversal over user data. All graph tools traverse
  code edges, not user-asserted facts.

---

## 6. Gap analysis: pgmcp vs SOTA memory servers

> **Legend:** ✓ shipped · ◐ partial · ✗ missing

| Capability                                | Official MCP | mem0 | Letta | Zep/Graphiti | **pgmcp** | SOTA needed? |
|-------------------------------------------|:------------:|:----:|:-----:|:------------:|:---------:|:------------:|
| Persistent vector store                   | ✗            | ✓    | ✓     | ✓            | ✓         | yes          |
| Entity/relation graph (user facts)        | ✓            | ✓    | ◐     | ✓            | **✗**     | yes          |
| Knowledge-graph search tools (CRUD)       | ✓            | ◐    | ◐     | ✓            | **✗**     | yes          |
| Hybrid search (BM25 + dense)              | ✗            | ✓    | ◐     | ✓            | ✓         | yes          |
| Reranking (cross-encoder)                 | ✗            | ◐    | ◐     | ✓            | **✗**     | yes          |
| Hierarchical retrieval (RAPTOR-style)     | ✗            | ✗    | ✗     | ◐            | **✗**     | yes          |
| Community summaries (GraphRAG-style)      | ✗            | ✗    | ✗     | ◐            | ◐ (topics)| optional     |
| LLM-driven salience extraction            | ✗            | ✓    | ✗     | ✓            | ◐ (regex) | yes          |
| Reflection / consolidation                | ✗            | ◐    | ◐     | ◐            | **✗**     | yes          |
| Bi-temporal facts (validity intervals)    | ✗            | ✗    | ✗     | ✓            | **✗**     | optional     |
| Eviction / forgetting policy              | ✗            | ◐    | ✓     | ✓            | **✗**     | yes          |
| Per-user / per-agent memory scoping       | ✗            | ✓    | ✓     | ✓            | ◐ (session)| yes         |
| Cross-session retrieval                   | ✓            | ✓    | ✓     | ✓            | ✓         | yes          |
| Frontier embedding model                  | n/a          | ✓    | ✓     | ✓            | **✗ (MiniLM)** | yes     |
| Matryoshka / multi-vector                 | n/a          | ◐    | ◐     | ◐            | **✗**     | yes          |
| Cognitive-tier memory (working/episodic)  | ✗            | ◐    | ✓     | ◐            | **✗**     | contested    |
| Code-index integration                    | ✗            | ✗    | ✗     | ✗            | ✓ (unique)| pgmcp moat   |
| Graph analytics on code                   | ✗            | ✗    | ✗     | ✗            | ✓ (unique)| pgmcp moat   |
| Mandate observation from prompts          | ✗            | ◐    | ✗     | ✗            | ✓ (unique)| pgmcp moat   |

### Headline takeaways

1. **pgmcp's retrieval/storage foundation is strong** — vector store,
   HNSW, hybrid search, durable persistence, cross-session search
   via the `claude` project all exist.
2. **The big structural miss is a user-fact entity/relation graph.**
   The official memory server's core abstraction (entities +
   relations + observations) does not exist in pgmcp today. The
   closest analog — `durable_mandates` — is flat text by polarity,
   not a graph.
3. **The salience pipeline is rule-based** (regex over prompts). SOTA
   has moved to LLM-judged extraction; the rule-based approach
   misses nuance and contradicts harder cases. This is the biggest
   *quality* gap.
4. **The embedding model is two generations behind.** Switching to a
   Matryoshka-trained instruction-tuned encoder (BGE-M3,
   Qwen3-Embedding, or a code-specialist like Voyage-code-3) is the
   single highest-impact upgrade across *all* of pgmcp, not just the
   memory surface.
5. **No reranking layer.** Hybrid search returns the union; SOTA
   reranks the top 50–100 with a cross-encoder. Easy bolt-on.
6. **No eviction policy.** This will matter as memory volume grows.
   For now `file_chunks` deletion is FK-driven; user-facts would
   need their own policy.
7. **pgmcp's moat is the code-index integration** — no memory server
   in the field combines a graph-aware code index with
   conversational memory. This is the unique value proposition.

---

## See also

- [`01-decisions.md`](01-decisions.md) — the 13 committed design
  decisions that resolve the open questions raised by this gap
  analysis.
- [`02-phases.md`](02-phases.md) — phased implementation roadmap
  (Phase 0 quick wins → Phase 11 latent-space pipeline).
- [`10-alternatives.md`](10-alternatives.md) — alternatives considered
  but not adopted, with reasons (RecursiveMAS, LightRAG,
  LazyGraphRAG, NodeRAG, PathRAG, G-Retriever, KAG, GraphRAG-Bench
  cautionary notes).
