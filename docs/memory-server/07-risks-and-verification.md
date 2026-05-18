# 07 — Risk register & verification approach

What can go wrong, how we'll know, and what we'll do about it.

> Original planning artifact:
> `~/.claude/plans/what-is-a-memory-idempotent-lovelace.md` §15 + §16.

---

## Risk register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| 8 GB VRAM insufficient when reranker + extractor both needed concurrently | High | Med | Mutually-exclusive load policy in `GpuDispatcher` (see [`03-architecture.md`](03-architecture.md)); batch queries to amortize swaps; document `Qwen3-4B` fallback. |
| candle missing Qwen3 inference path | Med | High | Verify support before Phase 4 starts; if missing, the `LlmBackendChoice::Cloud` path becomes Phase-4 primary, with Qwen3 deferred. Trait is unchanged either way. |
| BGE-M3 query latency dominates `semantic_search` SLO | Med | Med | Matryoshka — query at 256d, retrieve full for rerank. Cache embeddings of recent queries. Profile before assuming. |
| Migration from 384d → 1024d takes too long on large indexes | Med | Low | Embedding-migration cron is fully incremental; old column stays live throughout; cutover is instantaneous (flag flip). |
| LLM extraction produces hallucinated entities | High | Med | Strict JSON schema validation; reject on parse failure; importance < 0.3 facts gated behind `[memory.extractor] auto_promote = false` until eval confirms quality. |
| Bi-temporal query plans regress under load | Med | Med | Partial indices `WHERE valid_to IS NULL` for the hot path. Explicit `as_of` queries hit `(valid_from, valid_to)` index. Benchmark on synthetic ≥ 1M observations before declaring Phase 3 complete. |
| Multi-agent shared memory race conditions | Low | High | Insert-only writes; bi-temporal invalidation never destroys; Postgres transaction isolation. Test in §15 scenario "scope isolation under concurrent writes". |
| Reflection cron consumes Claude API quota (if Cloud backend used) | Med | Med | Cron defaults to off; when on, runs per-scope at most daily; cost cap in config. |
| `memory_code_anchor` becomes stale when files are renamed/deleted | High | Low | `ON DELETE CASCADE` from `indexed_files` / `file_chunks` / `code_topics`. A rename = delete + insert, so anchors auto-clean. Track count in stats. |
| User asks "forget X" but cascade misses something | Low | High | `memory_forget_log.manifest_json` is the audit trail; a follow-up `memory_audit_forget(log_id)` tool re-traverses to verify completeness. |
| Phase 11 RecursiveLink training OOMs on 8 GB | Med | Med | Gradient checkpointing + batch=1 + seq=1024; if still OOM, document cloud-burst (one A100 hour, ~$2–5) as the supported alternative. The latent pipeline is opt-in; failed training just leaves the text-mediated Phase-4 path active. |
| Phase 11 latent pipeline degrades extraction quality silently | High | High | Quality validation harness (Phase 11.4) runs daily; auto-downgrade to text path if regression > threshold; structured Prometheus alert. JSON schema validation rejects unparseable outputs on every call (defense in depth). |
| Phase 11 RecursiveLink weights drift as user's prompt distribution evolves | Med | Med | Re-train monthly cron (off by default); the `link_signature` versioning lets multiple link versions coexist for A/B testing. |
| Token-efficiency targets (cross-phase concerns) regress after a feature change | Med | Low | Prometheus counters tracked in `pgmcp_metadata`; CI gate optional (defer to a follow-up if the user wants it enforced). |
| Phase 6.3–6.5 graph-enhanced retrieval underperforms vanilla (per GraphRAG-Bench arXiv:2506.02404) | **High** | Med | Empirical gating per Phase 6.5: all graph tools opt-in, A/B vs vanilla in Phase 9 eval, weekly review of `pgmcp_graph_retrieval_underperformance_total`. Default retrieval stays vanilla `memory_semantic_search`. |
| `memory_unified_nodes` materialized view becomes expensive at scale | Med | Low | Refresh on same cadence as `similarity-scan` (configurable); partial-index per node_type for hot subsets; switch to incremental refresh if base tables exceed N rows (documented threshold, monitored). |
| `memory_path_search` returns redundant paths despite flow-pruning | Med | Low | Two-layer defense: prune step (Jaccard threshold) + per-call `k` cap. Logging captures top-3 path overlap stats so threshold can be tuned from real workload. |

---

## Verification approach (per phase)

Every phase ends with `./scripts/verify.sh` green (pre-push hook
enforced). In addition:

| Phase | New tests in `pgmcp-testing/tests/` | Notes |
|---|---|---|
| 0 | `memory_phase0.rs` | recall_prompts returns embedded historical prompts; search_mandates FTS hits; supersession marks duplicates. |
| 1 | `embedding_bge_m3.rs`, `embedding_migration_e2e.rs` | BGE-M3 dim matches; migration cron drains backlog; cutover flag flips reads cleanly. |
| 2 | `memory_schema.rs` | All constraints honored; bi-temporal `valid_to`-IS-NULL filter is the default; M:N scope/tier joins work; `memory_code_anchor` CHECK enforces ≥ 1 FK. |
| 3 | `memory_crud.rs`, `memory_search.rs`, `memory_code_anchor_e2e.rs` | Official-compat tool shapes match upstream JSON; semantic + hybrid + facts_at all return scoped results; anchor round-trip. |
| 4 | `memory_extractor.rs` | Qwen3-8B Q4 loads under VRAM ceiling; extraction returns schema-valid JSON; contradiction detection bi-temporal-invalidates. |
| 5 | `memory_reflect.rs` | Reflection emits at least one observation with `derived_from` populated; cron path also writes. |
| 6 | `memory_raptor.rs`, `memory_ppr.rs`, `memory_unified.rs`, `memory_path_search.rs`, `memory_graph_rag_ab.rs` | RAPTOR tree builds; PPR ordering correct; unified-node view spans all expected node types; path search returns ranked paths; A/B harness reports `(vanilla, graph, delta)` for ≥ 10 query scenarios with auto-disable threshold honored. |
| 7 | `memory_rerank.rs` | Rerank improves nDCG@10 over base on a held-out set; mutually-exclusive load with extractor confirmed. |
| 8 | `memory_forget.rs`, `memory_retention.rs` | Soft delete preserves history; cascade=true removes dependents; log entry written; retention cron respects window. |
| 9 | `memory_eval.rs` (the harness itself) | All 20+ scenarios pass; sandbox DB isolated from production. |
| 10 | `client_profile.rs` | Per-client OutputFormat selected correctly; tool description overrides resolve in expected order. |
| 11 | `latent_pipeline.rs`, `latent_pipeline_quality.rs` | VRAM probe passes on 4060 Ti; trained RecursiveLink loads from safetensors; latent output schema-validates; quality harness writes deltas. |

---

## Cross-cutting

- **New scenarios** in `pgmcp-testing/tests/db_sql_surface_integration.rs`
  for every new SQL query path.
- **Zero new clippy warnings.** `cargo verify-clippy` is part of
  `scripts/verify.sh`.
- **Property tests** (`proptest`) on bi-temporal invariants:
  - For every observation, exactly one of `{valid_to IS NULL,
    valid_to > valid_from}` holds.
  - `superseded_by` never forms a cycle.
- **`gpu_fallback_smoke`** extension confirms degraded-mode (CPU)
  behaviour for the new backends.
- **Telemetry counters** for every quantitative target in the plan
  (token reduction, latency, quality delta) — wired through the
  existing Prometheus / `pgmcp_metadata` surface.

---

## See also

- [`02-phases.md`](02-phases.md) — what each phase ships, in detail.
- [`04-hardware.md`](04-hardware.md) — VRAM-related risks have their
  budget here.
- [`09-milestones-and-as-built.md`](09-milestones-and-as-built.md) —
  per-milestone landing record (includes test results).
