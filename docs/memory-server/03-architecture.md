# 03 — Architecture sketch & backend traits

Architectural overview, plus the Rust trait surfaces that act as
swap seams between compute backends. Every new backend follows the
**trait + closed-set enum + factory** pattern established by
`src/fcm/mod.rs:144-169` (the existing `FcmBackend`).

> Original planning artifact:
> `~/.claude/plans/what-is-a-memory-idempotent-lovelace.md` §8 + §13.

---

## Topology

```
┌──────────────────────────────────────────────────────────────────┐
│                        Claude Code / Codex                       │
└────────────────┬─────────────────────────────────┬───────────────┘
                 │ MCP tool calls                  │ UserPromptSubmit hook
                 ▼                                 ▼
┌────────────────────────────┐    ┌──────────────────────────────┐
│  pgmcp MCP tools           │    │  POST /api/session/observe   │
│  (existing + new memory_*) │    │  (existing endpoint)         │
└────┬──────────────┬────────┘    └──────────────┬───────────────┘
     │              │                            │
     │              │             ┌──────────────▼────────────────┐
     │              │             │ Phase 4: LLM salience worker  │
     │              │             │  Qwen3-8B → entities/relations │
     │              │             │  bi-temporal invalidation      │
     │              │             └──────────────┬────────────────┘
     │              │                            │
     ▼              ▼                            ▼
┌──────────────────────────────────────────────────────────────────┐
│                    PostgreSQL + pgvector                         │
├──────────────────────────────────────────────────────────────────┤
│  EXISTING                                                        │
│  file_chunks · session_prompts · session_mandates                │
│  durable_mandates · code_topics · code_graph_edges               │
│  file_metrics · cross_project_similarities · ...                 │
├──────────────────────────────────────────────────────────────────┤
│  NEW (Phase 2)                                                   │
│  memory_entities · memory_observations · memory_relations        │
│  memory_scope · memory_entity_scope · memory_entity_tier         │
│  memory_code_anchor · memory_forget_log · memory_reflection_runs │
│  (HNSW on memory_observations.embedding)                         │
├──────────────────────────────────────────────────────────────────┤
│  NEW (Phase 6.1)  memory_summary_tree (RAPTOR)                   │
│  NEW (Phase 6.3)  memory_unified_nodes (matview), edges (view)   │
└──────────────────────────────────────────────────────────────────┘
            ▲                                       ▲
            │ Phase 1 embedding upgrade             │ Phase 7 rerank
            │ (BGE-M3 + Matryoshka)                 │ (BGE-reranker-v2-m3)
            │                                       │
        candle-rs + CUDA backend ───────────────────┘
        (existing FcmBackend trait extends here)
                  │
                  │ Phase 11 latent-space pipeline
                  ▼
        Qwen3-8B + RecursiveLink (gated on VRAM)
```

### Key continuity points

- **No new feature flags.** `[features]`-free per pgmcp policy
  (`feedback_feature_gated_build_verification.md`); new backends
  slot in via the existing `FcmBackend`-style trait pattern.
- **CUDA reused.** The reranker, the new embedding model, and the
  latent-space pipeline all ride the existing candle+CUDA path;
  nothing CPU-only.
- **The `claude` project stays the same.** Phase 2 is *additive* —
  the filesystem-based memory (Claude Code's auto-memory + plan
  files + CLAUDE.md) continues to be indexed and searchable. The
  entity/relation surface is a *second* memory channel for
  agent-initiated writes that don't naturally land in files.

---

## Backend trait surfaces

All swap-seam traits follow the `FcmBackend` pattern from
`src/fcm/mod.rs:144-169`: a trait + closed-set enum + factory.

### `Embedder` (`src/embeddings/mod.rs`) — Phase 1

```rust
use anyhow::Result;

pub trait Embedder: Send + Sync {
    fn name(&self) -> &'static str;
    fn signature(&self) -> &'static str;
    fn full_dim(&self) -> usize;
    fn matryoshka_dims(&self) -> &'static [usize];

    /// Embed for indexing (no instruction prefix).
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Embed for query (BGE-M3 documentation recommends an
    /// instruction prefix on queries for retrieval).
    fn embed_query(&self, text: &str) -> Result<Vec<f32>>;

    /// Truncate a full-dim vector to a Matryoshka prefix dim,
    /// re-normalizing if the model requires it. Default impl
    /// truncates + L2-normalizes.
    fn truncate(&self, full: &[f32], target_dim: usize) -> Vec<f32> {
        /* default */
    }
}

#[derive(Debug, Clone, Copy)]
pub enum EmbedderChoice { Bgem3, MiniLm }

pub fn make_embedder(choice: EmbedderChoice) -> Result<Box<dyn Embedder>>;
```

### `LlmExtractor` (`src/llm/mod.rs`) — Phase 4

```rust
pub struct EntityRef<'a> {
    pub id: i64,
    pub name: &'a str,
    pub entity_type: &'a str,
    pub key_observations: &'a [&'a str],  // top-K by importance, for grounding
}

pub struct ExtractionRequest<'a> {
    pub text: &'a str,
    pub existing_entities: &'a [EntityRef<'a>],
    pub scope: &'a ScopeRef,
    pub max_extractions: usize,
}

pub struct NewEntity {
    pub name: String,
    pub entity_type: String,
    pub initial_observations: Vec<String>,
    pub importance: f32,
}

pub struct NewRelation {
    pub from_name: String,
    pub to_name: String,
    pub relation_type: String,
    pub importance: f32,
}

pub struct ContradictionSignal {
    pub conflicting_with: i64,  // existing observation/relation id
    pub kind: ContradictionKind,  // Observation | Relation
    pub reason: String,
}

pub struct ExtractionResult {
    pub entities: Vec<NewEntity>,
    pub relations: Vec<NewRelation>,
    pub contradictions: Vec<ContradictionSignal>,
}

pub trait LlmExtractor: Send + Sync {
    fn name(&self) -> &'static str;
    fn model_signature(&self) -> &'static str;
    fn extract(&self, req: ExtractionRequest<'_>) -> Result<ExtractionResult>;
    fn reflect(&self, observations: &[&str]) -> Result<Vec<NewEntity>>;
}

#[derive(Debug, Clone, Copy)]
pub enum LlmBackendChoice { Qwen38b, Qwen34b, Cloud(CloudProvider) }

#[derive(Debug, Clone, Copy)]
pub enum CloudProvider { Anthropic, Openai }

pub fn make_extractor(choice: LlmBackendChoice) -> Result<Box<dyn LlmExtractor>>;
```

### `Reranker` (`src/reranker/mod.rs`) — Phase 7

```rust
pub trait Reranker: Send + Sync {
    fn name(&self) -> &'static str;
    /// Returns (original_index, score) sorted descending.
    fn rerank(&self, query: &str, candidates: &[&str]) -> Result<Vec<(usize, f32)>>;
}

#[derive(Debug, Clone, Copy)]
pub enum RerankerChoice { BgeV2M3 }

pub fn make_reranker(choice: RerankerChoice) -> Result<Box<dyn Reranker>>;
```

### `LatentPipeline` (`src/llm/latent_pipeline.rs`) — Phase 11

```rust
pub trait LatentPipeline: Send + Sync {
    fn name(&self) -> &'static str;
    fn backbone_signature(&self) -> &'static str;
    fn link_signature(&self) -> &'static str;   // version of trained W_1,W_2

    /// Run extract→reflect as a single fused latent pipeline.
    fn extract_then_reflect(
        &mut self,
        req: &ExtractionRequest<'_>,
    ) -> Result<(ExtractionResult, Vec<NewEntity>)>;

    /// Run extract→consolidate (called from Phase 8's near-dup merger).
    fn extract_then_consolidate(
        &mut self,
        req: &ExtractionRequest<'_>,
        scope: &ScopeRef,
    ) -> Result<(ExtractionResult, ConsolidationResult)>;
}

pub enum LatentPipelineChoice { Qwen38bRlv1, Disabled }
pub fn make_latent_pipeline(choice: LatentPipelineChoice)
    -> Result<Box<dyn LatentPipeline>>;
```

### VRAM-aware dispatcher (`src/llm/dispatcher.rs`)

```rust
/// Owns mutually-exclusive LLM/reranker loading per the VRAM budget.
pub struct GpuDispatcher {
    embedder: Box<dyn Embedder>,        // always resident
    extractor: ResidentSlot<Box<dyn LlmExtractor>>,
    reranker:  ResidentSlot<Box<dyn Reranker>>,
}

pub enum ResidentSlot<T> { Loaded(T), Unloaded(LoaderFn<T>) }

impl GpuDispatcher {
    pub fn with_extractor<R>(&mut self, f: impl FnOnce(&dyn LlmExtractor) -> R) -> R;
    pub fn with_reranker <R>(&mut self, f: impl FnOnce(&dyn Reranker)   -> R) -> R;
    pub fn embedder(&self) -> &dyn Embedder;
}
```

`with_extractor` / `with_reranker` unload the other slot if needed
before calling `f`, then leave the chosen model resident until the
next swap. See [`04-hardware.md`](04-hardware.md) for why this
mutually-exclusive policy is necessary.

---

## File-system layout (planned)

```
src/
├── embeddings/
│   ├── mod.rs              # Embedder trait + factory
│   ├── bge_m3.rs           # BGE-M3 impl via candle
│   └── minilm.rs           # legacy MiniLM-L6 (kept for migration window)
├── llm/
│   ├── mod.rs              # LlmExtractor trait + factory
│   ├── qwen3.rs            # Qwen3-8B / Qwen3-4B impl via candle
│   ├── cloud.rs            # optional cloud (Anthropic / OpenAI) impl
│   ├── dispatcher.rs       # GpuDispatcher (mutually-exclusive loading)
│   └── latent_pipeline.rs  # Phase 11 LatentPipeline trait + Qwen3+RL impl
├── reranker/
│   ├── mod.rs              # Reranker trait + factory
│   └── bge_v2_m3.rs        # BGE-reranker-v2-m3 impl via candle
├── memory/
│   ├── mod.rs              # MemoryStore facade
│   ├── crud.rs             # entity/observation/relation CRUD
│   ├── search.rs           # semantic/hybrid/facts_at queries
│   ├── reflect.rs          # Phase 5 reflection
│   ├── raptor.rs           # Phase 6.1 summary tree
│   ├── ppr.rs              # Phase 6.2 personalized pagerank
│   ├── unified.rs          # Phase 6.3 heterogeneous-node view
│   ├── path_search.rs      # Phase 6.4 path-based retrieval
│   ├── anchor.rs           # Phase 2 code-graph cross-link
│   ├── retention.rs        # Phase 8 retention/consolidation
│   └── forget.rs           # Phase 8 explicit forget
├── cron/
│   ├── embedding_migration.rs       # Phase 1
│   ├── memory_raptor.rs             # Phase 6.1
│   ├── memory_reflect.rs            # Phase 5.2
│   ├── memory_consolidate.rs        # Phase 8.3
│   ├── memory_retention.rs          # Phase 8.2
│   ├── memory_eval.rs               # Phase 9.2
│   └── latent_pipeline_quality.rs   # Phase 11.4
├── mcp/
│   ├── client_profile.rs   # Phase 10 per-client output format
│   └── tools/
│       ├── tool_recall_prompts.rs       # Phase 0
│       ├── tool_search_mandates.rs      # Phase 0
│       ├── tool_memory_*.rs             # Phases 3, 5, 6, 8
│       └── ...
└── sessions/
    └── extractor_worker.rs # Phase 4 background salience pipeline

assets/
└── client_profiles.toml    # Phase 10 per-client overrides
```

`memory_unified_nodes` is a materialized view, not a Rust module;
refresh logic lives in `src/memory/unified.rs`.

---

## See also

- [`02-phases.md`](02-phases.md) — the phase-by-phase plan that
  introduces each of these traits.
- [`04-hardware.md`](04-hardware.md) — VRAM budget motivating the
  `GpuDispatcher` mutually-exclusive load policy.
- [`05-schema.md`](05-schema.md) — the SQL behind the storage layer.
- [`06-tools.md`](06-tools.md) — MCP tool surface that calls into
  these backends.
