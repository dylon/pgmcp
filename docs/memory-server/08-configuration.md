# 08 — Configuration surface

All new TOML keys. Everything defaults to safe/off so a stock pgmcp
install behaves identically until the user opts in.

This file is the **as-built** config reference — update it in the
same PR that adds the keys to `Config` struct in `src/config.rs`.

> Original planning artifact:
> `~/.claude/plans/what-is-a-memory-idempotent-lovelace.md` §17.

---

## Full TOML surface

```toml
[memory]
enabled = true                  # master switch for Phase 2+ tools

[memory.embedder]
backend = "bge-m3"              # "bge-m3" | "minilm" (legacy)
matryoshka_query_dim = 256
matryoshka_rerank_dim = 1024

[memory.extractor]
backend = "qwen3-8b"            # "qwen3-8b" | "qwen3-4b" | "cloud"
inline_debounce_secs = 30
auto_promote_threshold = 0.6    # importance ≥ this auto-writes to memory_*
schema_validation = "strict"    # "strict" | "lenient"

[memory.reranker]
backend = "bge-v2-m3"
idle_unload_secs = 300

[memory.retention]
enabled = true
window_days = 90
importance_threshold = 0.3

[memory.reflection]
agent_enabled = true            # MCP tool always available; harness flag
cron_enabled = false            # default off
cron_interval_secs = 86400
min_new_observations = 50       # don't reflect on near-empty scopes

[memory.eval]
enabled = false
cron_interval_secs = 86400
sandbox_db_url = "postgres://..."

[memory.latent_pipeline]
enabled = false                       # default off; opt-in after training
backbone = "qwen3-8b"                 # must match memory.extractor.backend for inner-link path
link_weights_path = "models/recursive_link_qwen3_8b.safetensors"
link_signature = "rlv1"
quality_regression_threshold = 0.05   # auto-downgrade if delta > this
fallback_on_schema_fail = true        # always-on safety net
vram_probe_at_startup = true

[memory.latent_pipeline.train]
enabled = false                       # one-shot when the user is ready
samples_from_session_prompts = 10000
epochs = 3
batch_size = 1
seq_len_cap = 1024
gradient_checkpointing = true
learning_rate = 5e-4
adamw_betas = [0.9, 0.999]
output_path = "models/recursive_link_qwen3_8b.safetensors"

[memory.graph_rag]
enabled = true                        # tools registered; opt-in per call
max_latency_ms = 500                  # hard cap per Phase 6.5
unified_view_refresh_secs = 21600     # 6h, aligned with similarity-scan
path_search_default_max_hops = 3
path_search_prune_jaccard = 0.7       # drop paths with > this overlap
auto_disable_underperform_window = 7  # days; if graph variant strictly worse for N runs in a row, demote
auto_disable_underperform_runs = 5

[cron]
embedding_migration_interval_secs = 600
memory_raptor_interval_secs = 43200
memory_consolidate_interval_secs = 86400
memory_retention_interval_secs = 86400
memory_reflect_interval_secs = 86400
memory_eval_interval_secs = 86400
latent_pipeline_quality_interval_secs = 86400  # daily quality probe (Phase 11.4)
```

---

## Default-off rationale

Several flags default to `enabled = false`:

- `[memory.reflection] cron_enabled` — reflection costs LLM time and
  VRAM. Agent-driven via `memory_reflect` is always available; the
  cron is for users who want background consolidation.
- `[memory.eval] enabled` — eval cron writes to a sandbox DB; opt-in
  per user.
- `[memory.latent_pipeline] enabled` — gated on trained weights;
  fall back to text-mediated Phase 4 pipeline if disabled.
- `[memory.latent_pipeline.train] enabled` — one-shot training run;
  user explicitly triggers it.

`[memory.retention] enabled = true` is the only "destructive"
default-on, and only hard-deletes rows that are already
soft-deleted, low-importance, and outside the retention window
(default 90 days). Phase 8.2.

---

## Hot vs cold reload

Following the existing pgmcp config-watcher pattern
(`src/indexer/config_watcher.rs`):

**Hot (live-reloaded via ArcSwap):**

- `memory.retention.window_days`, `importance_threshold`
- `memory.reflection.cron_interval_secs`, `min_new_observations`
- `memory.graph_rag.max_latency_ms`, prune/refresh params
- All `[cron]` intervals
- Per-project overrides in `.pgmcp.toml`

**Cold (restart required):**

- `memory.embedder.backend`, `memory.extractor.backend`,
  `memory.reranker.backend` — model load is non-trivial.
- `memory.latent_pipeline.enabled`, `backbone`, `link_weights_path`
  — pipeline construction at startup.

The config-watcher logs a "cold change ignored" warning if a hot
reload hits a cold key.

---

## See also

- [`02-phases.md`](02-phases.md) — phase descriptions name the
  config keys that gate each feature.
- [`04-hardware.md`](04-hardware.md) — the VRAM budget motivates the
  extractor/reranker mutually-exclusive load policy that the
  `idle_unload_secs` and `vram_probe_at_startup` keys control.
- `src/config.rs` (once landed) — Serde-deserialized `Config` struct
  whose layout this file mirrors.
