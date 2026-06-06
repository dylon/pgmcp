# pgmcp Search Modes

Reference for the four query interfaces pgmcp exposes. See the
[tool-catalog](tool-catalog.md) for the higher-level capability surface.


pgmcp provides four complementary search strategies:

**Semantic Search** -- finds conceptually related code even when terminology differs.
The query is embedded into the same 1024-dimensional BGE-M3 vector space, then ranked by
cosine similarity via pgvector's HNSW index:

```
score(q, c) = 1 - cosine_distance(embed(q), embed(c))
```

**Text Search** -- leverages PostgreSQL's mature full-text search engine. The query
is parsed into a `tsquery`, matched against pre-built `tsvector` GIN indices, and
ranked by TF-IDF:

```
rank = ts_rank(to_tsvector('english', content), plainto_tsquery('english', query))
```

**Grep** -- server-side regex matching via PostgreSQL's `~` operator. Supports
optional glob-based file filtering. Best for precise pattern matching when you know
exactly what you're looking for.

**Hybrid Search** -- fuses text and semantic search using Reciprocal Rank Fusion
(RRF). Both search strategies run in parallel, then results are merged:

```
RRF_score = bm25_weight / (k + rank_text) + semantic_weight / (k + rank_vec)
```

where k = 60 (standard RRF constant). Results appearing in both lists get boosted;
results in only one list still contribute. Configurable `bm25_weight` and
`semantic_weight` (default 0.5 each).

---
