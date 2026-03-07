# ADR-001: Stay with pgvector HNSW, do not migrate to pgvectorscale

**Status:** Accepted
**Date:** 2026-03-07

## Context

Evaluated whether pgvectorscale (Timescale's disk-based StreamingDiskANN extension) would improve performance over pgmcp's pgvector HNSW index.

## Decision

**Do not migrate to pgvectorscale.** Instead, tune pgvector's HNSW parameters for better recall.

## Rationale

### Scale mismatch

pgvectorscale targets billion-vector datasets that exceed RAM. pgmcp's worst case (500K files × ~20 chunks × 384 dims × 4 bytes ≈ 15 GB) fits in RAM. HNSW is faster than DiskANN when the index fits in memory.

### Query pattern favors HNSW

pgmcp does simple k-NN searches with optional language/project filtering. HNSW handles this with sub-millisecond latency at pgmcp's scale.

### Repeated query advantage

As a local dev tool querying the same codebase repeatedly, warm caches give pgvector HNSW an inherent advantage over disk-based approaches.

### Deployment friction

pgvectorscale requires building from source (Rust toolchain, cargo-pgrx, AVX2/FMA CPU) or TimescaleDB Docker images. pgvector is a simple `CREATE EXTENSION` on most PostgreSQL installations.

### No cost pressure

pgvectorscale's main value is cost reduction (SSD vs RAM) for cloud-hosted services. For a local dev tool, there's no hosting bill to optimize.

## pgvector tunings applied instead

| Parameter | Before | After | Effect |
|-----------|--------|-------|--------|
| `hnsw_m` | 16 | 24 | More bidirectional links per node → better recall |
| `hnsw_ef_construction` | 64 | 200 | Larger candidate list during build → higher quality graph |
| `ef_search` | 40 (default) | 100 (configurable) | Larger search candidate list → better recall at query time |

Additional improvements:
- **Project filtering** added to semantic search (pre-filters by project name via JOIN)
- **Transaction-scoped `SET LOCAL`** for `ef_search` to avoid leaking session state across pooled connections
- **Metadata-driven index migration** tracks HNSW params in `pgmcp_metadata` table; index is rebuilt only when params change

## When to reconsider

- If pgmcp pivots to a shared/hosted service indexing hundreds of codebases
- If embedding dimensions increase dramatically (e.g., 1536-dim or 3072-dim model)
- If the dataset regularly exceeds 100M+ vectors

## Further tuning options (not yet implemented)

- **Quantization**: pgvector 0.7+ halfvec (float16) halves memory with minimal quality loss for 384-dim vectors
- **Per-project partitioning**: separate tables/indexes per project for smaller, faster HNSW graphs
- **Partial indexes**: per-language HNSW indexes for filtered queries
