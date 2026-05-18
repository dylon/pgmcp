# 04 — Hardware constraints & model selection

Every model choice in this plan is sized for the user's actual
hardware. The 8 GiB VRAM ceiling is the binding constraint and the
reason the dispatcher (see [`03-architecture.md`](03-architecture.md))
enforces a mutually-exclusive load policy between the LLM extractor
and the reranker.

> Original planning artifact:
> `~/.claude/plans/what-is-a-memory-idempotent-lovelace.md` §11.

---

## The host

Per `/home/dylon/.claude/hardware-specifications.md`:

- CPU: **AMD Threadripper PRO 5975WX**, 32 cores / 64 threads (Zen 3).
- RAM: **128 GiB** DDR4 ECC.
- **GPU: NVIDIA RTX 4060 Ti, 8 GiB VRAM** (Ada Lovelace, CC 8.9).
- CUDA 12.x; driver 595.x.
- Primary storage: Samsung 990 PRO 4 TB NVMe.

**The 8 GiB VRAM is the binding constraint.** Every model choice
below is sized to fit on this device.

---

## VRAM budget (steady state vs. peak)

| Phase | Resident | Loaded on demand | Peak free |
|---|---|---|---|
| Today | MiniLM-L6 (350 MB) | — | ~7.6 GB |
| Phase 1+ | BGE-M3 fp16 (1.2 GB) | — | ~6.8 GB |
| Phase 4+ | BGE-M3 (1.2 GB) | Qwen3-8B Q4 (5.3 GB) during extraction | ~1.5 GB at peak |
| Phase 7+ | BGE-M3 (1.2 GB) | Either reranker (0.6 GB) **or** Qwen3-8B (5.3 GB), never both | ~6.2 / ~1.5 GB |
| Phase 11 inference | BGE-M3 (1.2 GB) | Qwen3-8B Q4 + RecursiveLink (~5.35 GB) | ~1.45 GB |
| Phase 11 training (one-shot) | — | Qwen3-8B frozen + activations (gradient ckpt, batch=1, seq ≤ 1024) ≈ 7.0 GB | ~1.0 GB |

**Mutually-exclusive load policy** — the LLM extractor and reranker
are never resident together. The dispatcher unloads one to make
room for the other. The `Embedder` stays resident because it's on
the hot path of nearly every tool.

**Phase 11 training is the tightest scenario** at ~7.0 GB peak. The
training run is one-shot, scheduled during low-traffic windows, with
a documented cloud-burst fallback (rent one A100 hour for ~$2–5 if
local training proves marginal — inference stays local either way).

---

## Why these specific models

### Embedder: BGE-M3 (1024d, Matryoshka)

- 568M params, fp16 footprint ~1.2 GB.
- Matryoshka-truncatable to 64/128/256/512/1024 — query at 256d for
  ANN, retrieve full for rerank.
- Dense + sparse + multi-vector in one model (we use dense in
  Phase 1; sparse can become a Phase 8 rerank input later).
- 100+ languages; strong on code and prose.
- Leaves ~6.8 GB headroom under the 8 GB ceiling, which is exactly
  what the Qwen3-8B Q4 extractor needs on top.

### LLM extractor: Qwen3-8B-Instruct Q4_K_M

- Q4_K_M quantized footprint: ~4.8 GB (model) + ~0.5 GB (KV cache
  for a 4 K context window) = ~5.3 GB.
- BGE-M3 (1.2 GB) + Qwen3-8B Q4 (5.3 GB) ≈ 6.5 GB resident, leaving
  ~1.5 GB headroom under the 8 GB ceiling.
- Strong instruction-following at the 8B class; SOTA on extraction
  benchmarks for its size as of late 2025.

### Fallback extractor: Qwen3-4B-Instruct Q4_K_M

- ~2.5 GB Q4. Selected when VRAM probe at startup shows < 6 GB free
  or config sets `[memory.extractor] backend = "qwen3-4b"`.
- Useful when another process holds VRAM, when the reranker is
  configured to stay resident, or for development on tighter
  hardware.

### Reranker: BGE-reranker-v2-m3

- 568M params, ~600 MB fp16.
- Pairs naturally with BGE-M3 (same family, same tokenizer).
- Mutually exclusive with Qwen3-8B per VRAM budget — dispatcher
  unloads the LLM during rerank windows.

### Latent pipeline: Qwen3-8B + RecursiveLink (~50 MB extra)

- Inference: BGE-M3 (1.2 GB) + Qwen3-8B Q4 + RecursiveLink (~5.35 GB)
  = ~6.55 GB. Fits.
- Training: Qwen3-8B Q4 frozen (5.3 GB) + activations with gradient
  checkpointing, batch=1, seq ≤ 1024 (~1.5 GB) + RecursiveLink
  gradients (~50 MB) + AdamW moments for (W_1, W_2) (~100 MB) =
  ~7.0 GB peak. Tight but fits with the checkpointing discipline.

---

## Why not larger models

- **Qwen3-Embedding-8B** — would bring +2–3 MTEB points but is 7 GB
  fp16; can't coexist with the LLM. BGE-M3 is the right point on
  the Pareto curve given the hardware.
- **Qwen3-14B / Qwen3-32B extractors** — Q4 footprints of ~9 GB /
  ~20 GB exceed the device.
- **70B-class models** — would require partial CPU offload (10×
  slower) or a second GPU.

---

## If the user upgrades

If a future GPU upgrade lifts the VRAM ceiling (e.g. 24 GB → run
Qwen3-32B-Instruct Q4, or 48 GB → resident reranker + 14B extractor
+ 1024d embeddings simultaneously), only the `*BackendChoice` enums
and a config-file change are needed. No schema migration; no tool
changes.

The trait-based architecture (see
[`03-architecture.md`](03-architecture.md)) makes "swap in a bigger
model" a config edit, not a refactor.

---

## See also

- [`02-phases.md`](02-phases.md) Phase 1, Phase 4, Phase 7, Phase 11
  — the phases this hardware budget gates.
- [`03-architecture.md`](03-architecture.md) — the `GpuDispatcher`
  that enforces the mutually-exclusive load policy.
- [`07-risks-and-verification.md`](07-risks-and-verification.md) —
  VRAM-related risks and their mitigations.
