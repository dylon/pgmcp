# 05 — Schema (full SQL)

All new tables for Phase 1 (embedding columns) and Phase 2 (memory
tables), plus the Phase 6.3 unified-graph views. Coordinated into
the existing append-only `src/db/migrations.rs`.

This file is the **as-built** SQL reference once each migration
lands. Until then, treat it as a target schema that may be refined
during implementation; update this file in the same PR that adds
the migration.

> Original planning artifact:
> `~/.claude/plans/what-is-a-memory-idempotent-lovelace.md` §12.

---

## 12.1 Phase 1 — embedding migration columns

```sql
-- file_chunks: add v2 embedding alongside existing 384d
ALTER TABLE file_chunks ADD COLUMN embedding_v2 VECTOR(1024);
ALTER TABLE file_chunks ADD COLUMN embedding_signature TEXT;
CREATE INDEX idx_file_chunks_embedding_v2 ON file_chunks
  USING hnsw (embedding_v2 vector_cosine_ops) WITH (m=24, ef_construction=200);

-- session_prompts: same
ALTER TABLE session_prompts ADD COLUMN embedding_v2 VECTOR(1024);
ALTER TABLE session_prompts ADD COLUMN embedding_signature TEXT;
CREATE INDEX idx_session_prompts_embedding_v2 ON session_prompts
  USING hnsw (embedding_v2 vector_cosine_ops) WITH (m=24, ef_construction=200);

-- pgmcp_metadata extended to track multiple embedding signatures
-- (existing rows continue to track HNSW params for the old column).
INSERT INTO pgmcp_metadata (key, value)
VALUES ('active_embedding_signature', 'minilm-l6-v2');  -- flipped at cutover
```

---

## 12.2 Phase 2 — memory tables

```sql
-- Cognitive tier enum (decision 1).
CREATE TYPE memory_tier AS ENUM (
  'working',     -- transient, in-prompt
  'episodic',    -- "this happened at time T"
  'semantic',    -- "X is a Y"
  'procedural',  -- "to do X, follow these steps"
  'reflective'   -- higher-order summaries
);

-- Source provenance (where a fact came from).
CREATE TYPE memory_source AS ENUM (
  'user_explicit',   -- agent tool call from a user-stated fact
  'llm_extraction',  -- Phase 4 batch extractor
  'reflection',      -- Phase 5 reflection cycle
  'consolidation',   -- Phase 8 near-dup merge
  'agent_write',     -- agent-asserted, no LLM in the loop
  'migration'        -- imported from session_mandates / durable_mandates
);

-- Scope tuple (decisions 1, 8). Each dimension nullable → "any".
CREATE TABLE memory_scope (
  id          BIGSERIAL PRIMARY KEY,
  user_id     TEXT,
  agent_id    TEXT,   -- "claude-code", "codex", ...
  session_id  UUID REFERENCES sessions(id) ON DELETE CASCADE,
  project_id  INT  REFERENCES projects(id) ON DELETE CASCADE,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  UNIQUE (user_id, agent_id, session_id, project_id)
);

-- Entities (bi-temporal — decision 3).
CREATE TABLE memory_entities (
  id               BIGSERIAL PRIMARY KEY,
  name             TEXT NOT NULL,
  entity_type      TEXT NOT NULL,
  canonical_name   TEXT,    -- normalized for dedupe
  importance       REAL NOT NULL DEFAULT 0.5,
  source           memory_source NOT NULL,
  created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  valid_from       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  valid_to         TIMESTAMPTZ,
  superseded_by    BIGINT REFERENCES memory_entities(id),
  UNIQUE (name, entity_type, valid_from)
);
CREATE INDEX idx_memory_entities_active
  ON memory_entities (name, entity_type) WHERE valid_to IS NULL;
CREATE INDEX idx_memory_entities_temporal
  ON memory_entities (valid_from, valid_to);
CREATE INDEX idx_memory_entities_canonical
  ON memory_entities (canonical_name) WHERE valid_to IS NULL;

-- Scope join (decisions 1, 8). M:N for shared memory.
CREATE TABLE memory_entity_scope (
  entity_id  BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
  scope_id   BIGINT NOT NULL REFERENCES memory_scope(id) ON DELETE CASCADE,
  PRIMARY KEY (entity_id, scope_id)
);
CREATE INDEX idx_memory_entity_scope_scope ON memory_entity_scope (scope_id);

-- Tier join (decision 1). Fuzzy weights.
CREATE TABLE memory_entity_tier (
  entity_id  BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
  tier       memory_tier NOT NULL,
  weight     REAL NOT NULL DEFAULT 1.0,
  PRIMARY KEY (entity_id, tier),
  CHECK (weight >= 0.0 AND weight <= 1.0)
);
CREATE INDEX idx_memory_entity_tier_tier ON memory_entity_tier (tier);

-- Observations (facts attached to entities, embedded).
CREATE TABLE memory_observations (
  id                  BIGSERIAL PRIMARY KEY,
  entity_id           BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
  content             TEXT NOT NULL,
  content_sha256      CHAR(64) NOT NULL,
  embedding           VECTOR(1024),  -- BGE-M3 dense
  embedding_signature TEXT NOT NULL DEFAULT 'bge-m3-v1',
  importance          REAL NOT NULL DEFAULT 0.5,
  source              memory_source NOT NULL,
  source_session_id   UUID REFERENCES sessions(id),
  source_prompt_id    BIGINT REFERENCES session_prompts(id),
  derived_from        BIGINT[],  -- for reflection / consolidation provenance
  created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  valid_from          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  valid_to            TIMESTAMPTZ,
  superseded_by       BIGINT REFERENCES memory_observations(id),
  UNIQUE (entity_id, content_sha256, valid_from)
);
CREATE INDEX idx_memory_observations_active
  ON memory_observations (entity_id) WHERE valid_to IS NULL;
CREATE INDEX idx_memory_observations_temporal
  ON memory_observations (valid_from, valid_to);
CREATE INDEX idx_memory_observations_embedding
  ON memory_observations USING hnsw (embedding vector_cosine_ops)
  WITH (m=24, ef_construction=200);
-- FTS for hybrid search
CREATE INDEX idx_memory_observations_fts
  ON memory_observations USING gin (to_tsvector('english', content));

-- Relations (typed edges, bi-temporal).
CREATE TABLE memory_relations (
  id              BIGSERIAL PRIMARY KEY,
  from_entity_id  BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
  to_entity_id    BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
  relation_type   TEXT NOT NULL,
  importance      REAL NOT NULL DEFAULT 0.5,
  source          memory_source NOT NULL,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  valid_from      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  valid_to        TIMESTAMPTZ,
  superseded_by   BIGINT REFERENCES memory_relations(id),
  UNIQUE (from_entity_id, to_entity_id, relation_type, valid_from),
  CHECK (from_entity_id <> to_entity_id)
);
CREATE INDEX idx_memory_relations_from
  ON memory_relations (from_entity_id) WHERE valid_to IS NULL;
CREATE INDEX idx_memory_relations_to
  ON memory_relations (to_entity_id) WHERE valid_to IS NULL;
CREATE INDEX idx_memory_relations_type
  ON memory_relations (relation_type) WHERE valid_to IS NULL;
CREATE INDEX idx_memory_relations_temporal
  ON memory_relations (valid_from, valid_to);

-- Code-graph cross-linking (decision 9).
CREATE TABLE memory_code_anchor (
  id           BIGSERIAL PRIMARY KEY,
  entity_id    BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
  file_id      INT  REFERENCES indexed_files(id) ON DELETE CASCADE,
  chunk_id     BIGINT REFERENCES file_chunks(id) ON DELETE CASCADE,
  topic_id     BIGINT REFERENCES code_topics(id) ON DELETE CASCADE,
  anchor_type  TEXT NOT NULL,  -- 'implements'|'tested-by'|'documented-in'|'caused-by'|'applies-to'
  created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  CHECK (file_id IS NOT NULL OR chunk_id IS NOT NULL OR topic_id IS NOT NULL)
);
CREATE INDEX idx_memory_code_anchor_entity ON memory_code_anchor (entity_id);
CREATE INDEX idx_memory_code_anchor_file   ON memory_code_anchor (file_id)   WHERE file_id   IS NOT NULL;
CREATE INDEX idx_memory_code_anchor_chunk  ON memory_code_anchor (chunk_id)  WHERE chunk_id  IS NOT NULL;
CREATE INDEX idx_memory_code_anchor_topic  ON memory_code_anchor (topic_id)  WHERE topic_id  IS NOT NULL;

-- RAPTOR summary tree (Phase 6.1).
CREATE TABLE memory_summary_tree (
  id              BIGSERIAL PRIMARY KEY,
  scope_id        BIGINT NOT NULL REFERENCES memory_scope(id) ON DELETE CASCADE,
  level           INT NOT NULL,    -- 0 = leaf (observation), 1+ = summary
  parent_id       BIGINT REFERENCES memory_summary_tree(id),
  observation_id  BIGINT REFERENCES memory_observations(id),  -- NULL except at level 0
  summary_text    TEXT,
  summary_embedding VECTOR(1024),
  child_count     INT,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  CHECK ((level = 0 AND observation_id IS NOT NULL AND summary_text IS NULL)
      OR (level > 0 AND observation_id IS NULL     AND summary_text IS NOT NULL))
);
CREATE INDEX idx_memory_summary_tree_level
  ON memory_summary_tree (scope_id, level);
CREATE INDEX idx_memory_summary_tree_embedding
  ON memory_summary_tree USING hnsw (summary_embedding vector_cosine_ops)
  WITH (m=24, ef_construction=200);

-- Forget audit log (Phase 8).
CREATE TABLE memory_forget_log (
  id             BIGSERIAL PRIMARY KEY,
  actor          TEXT NOT NULL,         -- agent / user / cron
  target_type    TEXT NOT NULL,         -- 'entity'|'observation'|'relation'|'anchor'
  target_id      BIGINT NOT NULL,
  cascade        BOOLEAN NOT NULL,
  rows_affected  INT NOT NULL,
  manifest_json  JSONB,                 -- full list of deleted dependents
  forgotten_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Reflection bookkeeping (Phase 5).
CREATE TABLE memory_reflection_runs (
  id                BIGSERIAL PRIMARY KEY,
  scope_id          BIGINT REFERENCES memory_scope(id) ON DELETE SET NULL,
  started_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  finished_at       TIMESTAMPTZ,
  observation_count INT,
  facts_emitted     INT,
  trigger           TEXT NOT NULL  -- 'agent'|'cron'
);
```

---

## 12.3 Phase 6.3 — heterogeneous-node unified graph view

NodeRAG-inspired. **No new base tables** — views over existing
tables only.

```sql
CREATE MATERIALIZED VIEW memory_unified_nodes AS
  SELECT id::TEXT AS node_id, 'memory_entity'::TEXT AS node_type,
         name AS label, NULL::VECTOR(1024) AS embedding,
         NULL::BIGINT AS scope_id, importance
    FROM memory_entities WHERE valid_to IS NULL
  UNION ALL
  SELECT id::TEXT, 'observation', LEFT(content, 80), embedding,
         NULL::BIGINT, importance
    FROM memory_observations WHERE valid_to IS NULL
  UNION ALL
  SELECT id::TEXT, 'chunk', LEFT(content, 80), embedding_v2,
         NULL::BIGINT, 0.5
    FROM file_chunks
  UNION ALL
  SELECT id::TEXT, 'topic', topic_label, centroid, NULL::BIGINT, 0.5
    FROM code_topics
  UNION ALL
  SELECT id::TEXT, 'durable_mandate', imperative, embedding,
         NULL::BIGINT, 0.7
    FROM durable_mandates
  UNION ALL
  SELECT id::TEXT, 'commit', message, NULL::VECTOR(1024),
         NULL::BIGINT, 0.5
    FROM git_commits;

CREATE INDEX idx_memory_unified_nodes_embedding
  ON memory_unified_nodes USING hnsw (embedding vector_cosine_ops)
  WITH (m=24, ef_construction=200);

CREATE VIEW memory_unified_edges AS
  SELECT from_entity_id::TEXT AS from_id, 'memory_entity' AS from_type,
         to_entity_id::TEXT   AS to_id,   'memory_entity' AS to_type,
         relation_type AS edge_type, importance AS weight
    FROM memory_relations WHERE valid_to IS NULL
  UNION ALL
  SELECT entity_id::TEXT, 'memory_entity',
         COALESCE(file_id::TEXT, chunk_id::TEXT, topic_id::TEXT),
         CASE WHEN file_id IS NOT NULL THEN 'file'
              WHEN chunk_id IS NOT NULL THEN 'chunk'
              ELSE 'topic' END,
         anchor_type, 1.0
    FROM memory_code_anchor
  UNION ALL
  SELECT from_file_id::TEXT, 'file', to_file_id::TEXT, 'file',
         edge_type::TEXT, weight
    FROM code_graph_edges
  UNION ALL
  SELECT chunk_id::TEXT, 'chunk', topic_id::TEXT, 'topic',
         'belongs_to', membership_score
    FROM chunk_topic_assignments WHERE membership_score >= 0.05;
```

Materialized view refresh on the same cadence as the existing
`similarity-scan` cron (cheap; UNION ALL of indexed tables).

---

## 12.4 Phase 0 — mandate-embedding promotion (deferred to Phase 2 cutover)

```sql
-- Once BGE-M3 is live (post-Phase 1 cutover):
ALTER TABLE durable_mandates ADD COLUMN embedding VECTOR(1024);
ALTER TABLE durable_mandates ADD COLUMN embedding_signature TEXT;
CREATE INDEX idx_durable_mandates_embedding
  ON durable_mandates USING hnsw (embedding vector_cosine_ops)
  WITH (m=24, ef_construction=200);

-- Same for session_mandates (so search_mandates can hit either layer).
ALTER TABLE session_mandates ADD COLUMN embedding VECTOR(1024);
```

---

## Index strategy notes

- **Partial indices on `WHERE valid_to IS NULL`** for the hot path —
  default queries hit only the active subset.
- **Composite `(valid_from, valid_to)`** indices for explicit
  `as_of` queries on bi-temporal tables.
- **HNSW params** (m=24, ef_construction=200) match the existing
  `file_chunks` HNSW per ADR-001
  (`docs/decisions/001-no-pgvectorscale-migration.md`).
- **FTS gin index** on `memory_observations.content` for hybrid
  search; matches the pattern used by `file_chunks_text_search`.

---

## See also

- [`02-phases.md`](02-phases.md) Phases 1 (12.1), 2 (12.2), 6.3
  (12.3), 0 (12.4) — when each batch of SQL lands.
- [`01-decisions.md`](01-decisions.md) — decisions 1, 3, 8, 9
  motivate the bi-temporal columns, scope tuple, and code anchor.
- [`07-risks-and-verification.md`](07-risks-and-verification.md) —
  proptest invariants on bi-temporal correctness.
