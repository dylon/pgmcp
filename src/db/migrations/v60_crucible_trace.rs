//! Migration step 60: `crucible_trace` — the unified run-tracing store (ADR-020,
//! E10). Sibling tables that ANNOTATE `csm_run_traces` (which stays the sole source
//! of position, ADR-011) so a run is reconstructable + replayable to its first
//! divergence across every run-shape:
//!
//! - `crucible_trace_spans` — the OTel-shaped span tree; each span references an
//!   event slice `[event_lo, event_hi)` of `csm_run_traces.events` and binds to the
//!   machine via `(orch_state, frame_depth, gtype_cursor, critic_iteration)`.
//! - `crucible_trace_span_closure` — transitive parent→descendant closure so the
//!   `_why`/`_diff`/subtree reads are one indexed query.
//! - `crucible_trace_events` — point-in-time annotations within a span.
//! - `crucible_control_journal` — the append-only halt/resume/cancel history beside
//!   the mutable `system_control` singleton (ADR-016).
//! - `crucible_trace_counterexamples` — TLC/SMT/Rocq counterexamples as durable,
//!   replayable witnesses (ADR-012/017), idempotent on `content_sha256`.
//!
//! All closed vocabularies are sourced from the Rust enums in
//! [`crate::csm::trace_store`] (ADR-003 idiom: the CHECK list is built from the
//! enum's `sql_in_list()` so the DB and Rust source-of-truth cannot drift; golden
//! tests pin each). Additive + `IF NOT EXISTS`, so idempotent and version-gated.
//!
//! ## Boundary
//!
//! Pure coordination/MEMORY state in pgmcp's OWN tables — pgmcp never runs a shell
//! or writes the user's files. The agent (pi) supplies every value; pgmcp persists,
//! queries, and replays them.

use sqlx::PgPool;

use crate::csm::trace_store::{
    AnnotationKind, AnnotationSeverity, CexSource, CexVerdict, ControlAction, ControlScope,
    SpanKind, SpanStatus, WitnessKind,
};

pub(super) const CRUCIBLE_TRACE: i32 = 60;
pub(super) const CRUCIBLE_TRACE_NAME: &str = "crucible_trace";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // ---- 1. spans (the OTel-shaped tree) -----------------------------------
    let spans = format!(
        "CREATE TABLE IF NOT EXISTS crucible_trace_spans (
            span_id              BIGSERIAL PRIMARY KEY,
            trace_id             UUID NOT NULL,
            parent_span_id       BIGINT REFERENCES crucible_trace_spans(span_id) ON DELETE CASCADE,
            kind                 TEXT NOT NULL CHECK (kind IN ({span_kind})),
            name                 TEXT NOT NULL,
            status               TEXT NOT NULL DEFAULT 'unset' CHECK (status IN ({span_status})),
            status_message       TEXT,
            started_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            ended_at             TIMESTAMPTZ,
            session_key          TEXT REFERENCES orchestration_sessions(session_key) ON DELETE SET NULL,
            task_id              UUID REFERENCES a2a_tasks(id) ON DELETE SET NULL,
            run_trace_id         BIGINT REFERENCES csm_run_traces(id) ON DELETE SET NULL,
            work_item_public_id  TEXT,
            experiment_id        BIGINT REFERENCES experiments(id) ON DELETE SET NULL,
            pi_session_id        TEXT,
            role                 TEXT,
            peer                 TEXT,
            model                TEXT,
            event_lo             INT,
            event_hi             INT,
            gtype_cursor         INT,
            frame_depth          INT NOT NULL DEFAULT 0,
            orch_state           INT,
            critic_iteration     INT,
            critic_phase         TEXT,
            attributes           JSONB NOT NULL DEFAULT '{{}}'::jsonb,
            links                JSONB NOT NULL DEFAULT '[]'::jsonb,
            created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
        span_kind = SpanKind::sql_in_list(),
        span_status = SpanStatus::sql_in_list(),
    );
    sqlx::query(sqlx::AssertSqlSafe(spans.as_str()))
        .execute(pool)
        .await?;
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_cts_trace      ON crucible_trace_spans (trace_id, span_id)",
        "CREATE INDEX IF NOT EXISTS idx_cts_parent     ON crucible_trace_spans (parent_span_id)",
        "CREATE INDEX IF NOT EXISTS idx_cts_session    ON crucible_trace_spans (session_key)",
        "CREATE INDEX IF NOT EXISTS idx_cts_task       ON crucible_trace_spans (task_id)",
        "CREATE INDEX IF NOT EXISTS idx_cts_runtrace   ON crucible_trace_spans (run_trace_id)",
        "CREATE INDEX IF NOT EXISTS idx_cts_workitem   ON crucible_trace_spans (work_item_public_id)",
        "CREATE INDEX IF NOT EXISTS idx_cts_experiment ON crucible_trace_spans (experiment_id)",
        "CREATE INDEX IF NOT EXISTS idx_cts_kind_status ON crucible_trace_spans (kind, status)",
        "CREATE INDEX IF NOT EXISTS idx_cts_started    ON crucible_trace_spans (started_at)",
        "CREATE INDEX IF NOT EXISTS idx_cts_open       ON crucible_trace_spans (trace_id) WHERE ended_at IS NULL",
        "CREATE INDEX IF NOT EXISTS idx_cts_attrs_gin  ON crucible_trace_spans USING gin (attributes)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }

    // ---- 2. counterexample witnesses (referenced by annotations) -----------
    let cex = format!(
        "CREATE TABLE IF NOT EXISTS crucible_trace_counterexamples (
            id                  BIGSERIAL PRIMARY KEY,
            trace_id            UUID,
            span_id             BIGINT REFERENCES crucible_trace_spans(span_id) ON DELETE SET NULL,
            experiment_id       BIGINT REFERENCES experiments(id) ON DELETE SET NULL,
            work_item_public_id TEXT,
            source              TEXT NOT NULL CHECK (source IN ({cex_source})),
            verdict             TEXT NOT NULL DEFAULT 'violated' CHECK (verdict IN ({cex_verdict})),
            property            TEXT,
            witness_kind        TEXT NOT NULL CHECK (witness_kind IN ({witness_kind})),
            witness             JSONB NOT NULL,
            content             TEXT,
            content_sha256      CHAR(64) NOT NULL UNIQUE,
            metrics             JSONB NOT NULL DEFAULT '{{}}'::jsonb,
            created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
        cex_source = CexSource::sql_in_list(),
        cex_verdict = CexVerdict::sql_in_list(),
        witness_kind = WitnessKind::sql_in_list(),
    );
    sqlx::query(sqlx::AssertSqlSafe(cex.as_str()))
        .execute(pool)
        .await?;
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_ctcx_trace  ON crucible_trace_counterexamples (trace_id)",
        "CREATE INDEX IF NOT EXISTS idx_ctcx_source ON crucible_trace_counterexamples (source, verdict)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }

    // ---- 3. annotations (point-in-time facts within a span) ----------------
    let events = format!(
        "CREATE TABLE IF NOT EXISTS crucible_trace_events (
            id                 BIGSERIAL PRIMARY KEY,
            span_id            BIGINT NOT NULL REFERENCES crucible_trace_spans(span_id) ON DELETE CASCADE,
            trace_id           UUID NOT NULL,
            at                 TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            event_kind         TEXT NOT NULL CHECK (event_kind IN ({annotation_kind})),
            severity           TEXT NOT NULL DEFAULT 'info' CHECK (severity IN ({severity})),
            message            TEXT,
            event_ord          INT,
            counterexample_id  BIGINT REFERENCES crucible_trace_counterexamples(id) ON DELETE SET NULL,
            attributes         JSONB NOT NULL DEFAULT '{{}}'::jsonb
        )",
        annotation_kind = AnnotationKind::sql_in_list(),
        severity = AnnotationSeverity::sql_in_list(),
    );
    sqlx::query(sqlx::AssertSqlSafe(events.as_str()))
        .execute(pool)
        .await?;
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_cte_span  ON crucible_trace_events (span_id)",
        "CREATE INDEX IF NOT EXISTS idx_cte_trace ON crucible_trace_events (trace_id, at)",
        "CREATE INDEX IF NOT EXISTS idx_cte_kind  ON crucible_trace_events (event_kind)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }

    // ---- 4. span closure (fast subtree / ancestor queries) -----------------
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS crucible_trace_span_closure (
            ancestor_id   BIGINT NOT NULL REFERENCES crucible_trace_spans(span_id) ON DELETE CASCADE,
            descendant_id BIGINT NOT NULL REFERENCES crucible_trace_spans(span_id) ON DELETE CASCADE,
            depth         INT NOT NULL,
            PRIMARY KEY (ancestor_id, descendant_id)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_ctsc_desc ON crucible_trace_span_closure (descendant_id)",
    )
    .execute(pool)
    .await?;

    // ---- 5. control-plane audit journal (append-only) ----------------------
    let journal = format!(
        "CREATE TABLE IF NOT EXISTS crucible_control_journal (
            id                  BIGSERIAL PRIMARY KEY,
            action              TEXT NOT NULL CHECK (action IN ({action})),
            scope               TEXT NOT NULL DEFAULT 'fleet' CHECK (scope IN ({scope})),
            session_key         TEXT,
            task_id             UUID REFERENCES a2a_tasks(id) ON DELETE SET NULL,
            work_item_public_id TEXT,
            trace_id            UUID,
            span_id             BIGINT REFERENCES crucible_trace_spans(span_id) ON DELETE SET NULL,
            reason              TEXT,
            actor               TEXT,
            at                  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            attributes          JSONB NOT NULL DEFAULT '{{}}'::jsonb
        )",
        action = ControlAction::sql_in_list(),
        scope = ControlScope::sql_in_list(),
    );
    sqlx::query(sqlx::AssertSqlSafe(journal.as_str()))
        .execute(pool)
        .await?;
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_ccj_at      ON crucible_control_journal (at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_ccj_action  ON crucible_control_journal (action, at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_ccj_session ON crucible_control_journal (session_key)",
        "CREATE INDEX IF NOT EXISTS idx_ccj_trace   ON crucible_control_journal (trace_id)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(CRUCIBLE_TRACE, 60);
        assert_eq!(CRUCIBLE_TRACE_NAME, "crucible_trace");
    }

    /// The CHECK constraints are built from the Rust enums; pin that the
    /// interpolation is non-empty and quoted (the per-enum vocabulary is pinned in
    /// `trace_store`'s golden tests).
    #[test]
    fn check_vocabularies_are_sourced_from_enums() {
        assert!(SpanKind::sql_in_list().contains("'planned_step'"));
        assert!(SpanStatus::sql_in_list().contains("'error'"));
        assert!(AnnotationKind::sql_in_list().contains("'off_protocol'"));
        assert!(ControlAction::sql_in_list().contains("'halt'"));
        assert!(CexSource::sql_in_list().contains("'tlc'"));
        assert!(WitnessKind::sql_in_list().contains("'tla_trace'"));
    }
}

#[cfg(test)]
mod db_roundtrip {
    //! A real-Postgres round-trip for the v60 schema + the `trace_store` query layer.
    //! GATED on `CRUCIBLE_TRACE_IT_DB` (a URL to a THROWAWAY db); unset ⇒ the test
    //! no-ops, so normal `cargo test` and CI are unaffected. NOT part of pgmcp's
    //! verify.sh sweep — a targeted, opt-in validation that the generated DDL and
    //! every (runtime-checked) sqlx query is valid against Postgres.
    use crate::csm::trace_store as ts;
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;

    async fn pool() -> Option<PgPool> {
        let url = std::env::var("CRUCIBLE_TRACE_IT_DB").ok()?;
        PgPool::connect(&url).await.ok()
    }

    fn span(
        tid: Uuid,
        kind: ts::SpanKind,
        name: &str,
        parent: Option<i64>,
        status: ts::SpanStatus,
    ) -> ts::SpanInput {
        ts::SpanInput {
            trace_id: tid,
            parent_span_id: parent,
            kind,
            name: name.into(),
            status,
            status_message: None,
            ended_at: None,
            session_key: None,
            task_id: None,
            run_trace_id: None,
            work_item_public_id: None,
            experiment_id: None,
            pi_session_id: None,
            role: None,
            peer: None,
            model: None,
            event_lo: None,
            event_hi: None,
            gtype_cursor: None,
            frame_depth: 0,
            orch_state: None,
            critic_iteration: None,
            critic_phase: None,
            attributes: json!({}),
            links: json!([]),
        }
    }

    #[tokio::test]
    async fn v60_schema_and_queries_roundtrip_on_real_postgres() {
        let Some(pool) = pool().await else {
            eprintln!("SKIP: set CRUCIBLE_TRACE_IT_DB to a throwaway postgres url");
            return;
        };
        // Minimal parent tables the v60 FKs reference (a real deploy runs the full chain).
        for ddl in [
            "CREATE TABLE IF NOT EXISTS orchestration_sessions (session_key TEXT PRIMARY KEY)",
            "CREATE TABLE IF NOT EXISTS a2a_tasks (id UUID PRIMARY KEY)",
            "CREATE TABLE IF NOT EXISTS csm_run_traces (id BIGSERIAL PRIMARY KEY, task_id UUID, events JSONB)",
            "CREATE TABLE IF NOT EXISTS experiments (id BIGSERIAL PRIMARY KEY)",
        ] {
            sqlx::query(ddl).execute(&pool).await.expect("parent ddl");
        }
        // The migration under test — and idempotent on re-apply.
        super::apply(&pool).await.expect("v60 apply");
        super::apply(&pool).await.expect("v60 apply is idempotent");

        let tid = Uuid::new_v4();
        let root = ts::record_span(&pool, &span(tid, ts::SpanKind::Run, "run", None, ts::SpanStatus::Unset))
            .await
            .expect("root span");
        let child = ts::record_span(
            &pool,
            &span(tid, ts::SpanKind::PlannedStep, "step:0", Some(root), ts::SpanStatus::Ok),
        )
        .await
        .expect("child span");

        // closure: the child reaches itself (depth 0) and its root ancestor (depth 1).
        let closure_cnt: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM crucible_trace_span_closure WHERE descendant_id = $1",
        )
        .bind(child)
        .fetch_one(&pool)
        .await
        .expect("closure count");
        assert_eq!(closure_cnt, 2, "child closure must be {{self, root}}");

        ts::record_annotation(
            &pool,
            &ts::AnnotationInput {
                span_id: child,
                trace_id: tid,
                event_kind: ts::AnnotationKind::OffProtocol,
                severity: ts::AnnotationSeverity::Error,
                message: Some("t1_req before t0_done".into()),
                event_ord: Some(1),
                counterexample_id: None,
                attributes: json!({}),
            },
        )
        .await
        .expect("annotation");

        let cex_in = ts::CounterexampleInput {
            trace_id: Some(tid),
            span_id: Some(child),
            experiment_id: None,
            work_item_public_id: None,
            source: ts::CexSource::Tlc,
            verdict: ts::CexVerdict::Violated,
            property: Some("DepOrder_T0_T1".into()),
            witness_kind: ts::WitnessKind::TlaTrace,
            witness: json!({ "trace": [{ "g": "s_t0done" }] }),
            content: None,
            content_sha256: "a".repeat(64),
            metrics: json!({}),
        };
        let c1 = ts::record_counterexample(&pool, &cex_in).await.expect("cex 1");
        let c2 = ts::record_counterexample(&pool, &cex_in).await.expect("cex 2");
        assert_eq!(c1, c2, "record_counterexample is idempotent on content_sha256");

        ts::record_control(
            &pool,
            &ts::ControlInput {
                action: ts::ControlAction::Halt,
                scope: ts::ControlScope::Fleet,
                session_key: None,
                task_id: None,
                work_item_public_id: None,
                trace_id: Some(tid),
                span_id: None,
                reason: Some("test halt".into()),
                actor: Some("it".into()),
                attributes: json!({}),
            },
        )
        .await
        .expect("control");

        // reads
        let spans = ts::load_spans(&pool, tid).await.expect("load_spans");
        assert_eq!(spans.len(), 2);
        let q = ts::query_spans(
            &pool,
            &ts::SpanQuery { kind: Some("planned_step".into()), limit: 10, ..Default::default() },
        )
        .await
        .expect("query_spans");
        assert!(q.iter().any(|s| s.span_id == child));
        let header = ts::trace_header(&pool, tid).await.expect("header").expect("a header");
        assert_eq!(header.n_spans, 2);
        let journal = ts::load_control_journal(
            &pool,
            &ts::ControlQuery { limit: 10, ..Default::default() },
        )
        .await
        .expect("journal");
        assert!(journal.iter().any(|e| e.action == "halt"));
        let cex = ts::load_counterexample(&pool, None, Some(tid), Some("tlc"))
            .await
            .expect("cex read")
            .expect("a cex");
        assert_eq!(cex.property.as_deref(), Some("DepOrder_T0_T1"));
        let closed = ts::close_span(&pool, root, ts::SpanStatus::Ok, Some("done"), None)
            .await
            .expect("close")
            .expect("a row");
        assert_eq!(closed.status, "ok");

        eprintln!("v60 round-trip OK: 2 spans, closure ok, cex idempotent, journal+header+reads ok");
    }
}
