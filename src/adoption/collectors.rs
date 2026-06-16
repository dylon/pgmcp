//! SQL collection over `mcp_tool_calls` plus the family classification.

use sqlx::{PgPool, Row};

use crate::adoption::report::{
    AdoptionReport, ClientStat, ConversionStat, CsmConformance, FamilyStat,
};

/// Connecting clients whose calls count as real adoption. pgmcp's own CLI
/// dispatch records `client_name = "cli"`, `extract_caller` falls back to
/// `"unknown"`, and smoke/test harnesses use `"smoke"`/`"test"`. Restricting to
/// this allowlist keeps the plan's own `cargo run -- tool` verification steps
/// (which log `cli` rows) from contaminating the measurement.
pub const REAL_CLIENTS: [&str; 3] = ["claude-code", "codex-mcp-client", "claude-cli"];

/// Explanatory note attached to every report (documents the known caveats
/// rather than silently presenting partial signals as complete).
const NOTE: &str = "Restricted to real clients (claude-code, codex-mcp-client, claude-cli); \
pgmcp's own CLI self-calls (client_name='cli'), 'unknown', and smoke/test rows are excluded. \
Per-session counts only populate for calls recorded after the mcp_session_id telemetry fix \
(the column was historically empty), so session rates ramp from zero while call counts are \
complete. RLM (a2a_pattern_recursive) is a subset of A2A, so a2a counts include rlm counts.";

/// The five under-adopted tool families this collector tracks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    A2a,
    Csm,
    Memory,
    Rlm,
    WorkItem,
}

impl Family {
    /// All families, in display order.
    pub const ALL: [Family; 5] = [
        Family::A2a,
        Family::Csm,
        Family::Memory,
        Family::Rlm,
        Family::WorkItem,
    ];

    /// Stable lowercase key used for JSON fields and SQL column aliases.
    pub fn key(self) -> &'static str {
        match self {
            Family::A2a => "a2a",
            Family::Csm => "csm",
            Family::Memory => "memory",
            Family::Rlm => "rlm",
            Family::WorkItem => "workitem",
        }
    }

    /// Human-readable label for rendered reports.
    pub fn label(self) -> &'static str {
        match self {
            Family::A2a => "A2A collaboration",
            Family::Csm => "CSM coordination-conformance",
            Family::Memory => "Memory server",
            Family::Rlm => "RLM (recursive)",
            Family::WorkItem => "Work-item tracker",
        }
    }

    /// SQL boolean predicate over a `tool` column. These are static strings
    /// (no user input), so interpolating them into the query is injection-safe.
    /// Raw strings keep the LIKE escape (`\_` = literal underscore) intact.
    fn sql_predicate(self) -> &'static str {
        match self {
            Family::A2a => r"tool LIKE 'a2a\_%'",
            Family::Csm => r"tool LIKE 'csm\_%'",
            Family::Memory => {
                r"(tool LIKE 'memory\_%' OR tool IN ('recall_prompts','search_mandates','graph_neighbors'))"
            }
            Family::Rlm => "tool = 'a2a_pattern_recursive'",
            Family::WorkItem => r"(tool LIKE 'work\_item%' OR tool LIKE 'tag\_%')",
        }
    }

    /// Rust mirror of [`Family::sql_predicate`], used by unit tests to guarantee
    /// the classification matches the live tool namespace. A tool may belong to
    /// more than one family (RLM ⊂ A2A). Test-scoped: production classification
    /// happens in SQL via [`Family::sql_predicate`].
    #[cfg(test)]
    pub fn classify(tool: &str) -> Vec<Family> {
        let mut families = Vec::new();
        if tool.starts_with("a2a_") {
            families.push(Family::A2a);
        }
        if tool.starts_with("csm_") {
            families.push(Family::Csm);
        }
        if tool.starts_with("memory_")
            || matches!(
                tool,
                "recall_prompts" | "search_mandates" | "graph_neighbors"
            )
        {
            families.push(Family::Memory);
        }
        if tool == "a2a_pattern_recursive" {
            families.push(Family::Rlm);
        }
        if tool.starts_with("work_item") || tool.starts_with("tag_") {
            families.push(Family::WorkItem);
        }
        families
    }
}

/// Build the per-client aggregation query. Columns: client_name, total calls,
/// total distinct (non-empty) sessions, then per-family call + session counts.
fn build_query() -> String {
    let mut sql = String::from(
        "SELECT client_name, \
         COUNT(*) AS total_calls, \
         COUNT(DISTINCT NULLIF(mcp_session_id, '')) AS total_sessions",
    );
    for family in Family::ALL {
        let pred = family.sql_predicate();
        let key = family.key();
        sql.push_str(&format!(
            ", COUNT(*) FILTER (WHERE {pred}) AS {key}_calls\
             , COUNT(DISTINCT NULLIF(mcp_session_id, '')) FILTER (WHERE {pred}) AS {key}_sessions"
        ));
    }
    sql.push_str(
        " FROM mcp_tool_calls \
         WHERE ts > now() - ($1::int * interval '1 minute') \
           AND client_name = ANY($2::text[]) \
         GROUP BY client_name \
         ORDER BY total_calls DESC",
    );
    sql
}

/// Collect adoption stats over the last `window_minutes` for the real-client
/// allowlist.
pub async fn collect(pool: &PgPool, window_minutes: i64) -> Result<AdoptionReport, sqlx::Error> {
    let allowlist: Vec<String> = REAL_CLIENTS.iter().map(|s| s.to_string()).collect();
    let sql = build_query();
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(window_minutes as i32)
        .bind(&allowlist)
        .fetch_all(pool)
        .await?;

    let mut clients = Vec::with_capacity(rows.len());
    // Overall accumulators. A session id belongs to exactly one client, so
    // summing distinct per-client session counts yields the correct distinct
    // total without double-counting.
    let mut overall_total_calls: i64 = 0;
    let mut overall_calls = [0i64; Family::ALL.len()];
    let mut overall_sessions = [0i64; Family::ALL.len()];

    for row in &rows {
        let client_name: String = row.get("client_name");
        let total_calls: i64 = row.get("total_calls");
        let total_sessions: i64 = row.get("total_sessions");
        overall_total_calls += total_calls;

        let mut families = Vec::with_capacity(Family::ALL.len());
        for (idx, family) in Family::ALL.into_iter().enumerate() {
            let calls_col = format!("{}_calls", family.key());
            let sessions_col = format!("{}_sessions", family.key());
            let calls: i64 = row.get(calls_col.as_str());
            let sessions: i64 = row.get(sessions_col.as_str());
            overall_calls[idx] += calls;
            overall_sessions[idx] += sessions;
            families.push(FamilyStat {
                family: family.label().to_string(),
                calls,
                sessions,
                call_share_pct: share_pct(calls, total_calls),
            });
        }
        clients.push(ClientStat {
            client_name,
            total_calls,
            total_sessions,
            families,
        });
    }

    let overall = Family::ALL
        .into_iter()
        .enumerate()
        .map(|(idx, family)| FamilyStat {
            family: family.label().to_string(),
            calls: overall_calls[idx],
            sessions: overall_sessions[idx],
            call_share_pct: share_pct(overall_calls[idx], overall_total_calls),
        })
        .collect();

    // Conversion + CSM conformance are best-effort: on a DB where the v11
    // nudge_emissions table hasn't been applied yet (pre-restart) the conversion
    // query errors → empty, never failing the whole report.
    let conversion = conversion(pool, window_minutes).await.unwrap_or_default();
    let csm_conformance = csm_conformance(pool, window_minutes)
        .await
        .unwrap_or_default();

    Ok(AdoptionReport {
        window_minutes,
        allowlist,
        clients,
        overall,
        overall_total_calls,
        conversion,
        csm_conformance,
        note: NOTE.to_string(),
    })
}

fn share_pct(part: i64, whole: i64) -> f64 {
    if whole <= 0 {
        0.0
    } else {
        (part as f64) * 100.0 / (whole as f64)
    }
}

/// Correlation window: a nudge "converts" if the same client calls a tool in
/// the nudged family within this many minutes after the nudge.
const CONVERSION_WINDOW_MINUTES: i64 = 10;

/// Nudge→adoption conversion per (family, channel), correlated by `client_name`
/// and a time-window — the only viable correlation, since the observe-hook
/// `session_id` and the MCP transport `mcp_session_id` are different id spaces.
/// Restricted to the real-client allowlist. The CASE maps each nudge family key
/// to the tool predicate for the family it steers toward (collaboration→a2a,
/// large_context→RLM, memory_*→memory, work_item→work-items).
pub async fn conversion(
    pool: &PgPool,
    window_minutes: i64,
) -> Result<Vec<ConversionStat>, sqlx::Error> {
    let allowlist: Vec<String> = REAL_CLIENTS.iter().map(|s| s.to_string()).collect();
    let rows = sqlx::query(
        r"SELECT n.family, n.channel, COUNT(*) AS nudges,
            COUNT(*) FILTER (WHERE EXISTS (
              SELECT 1 FROM mcp_tool_calls c
              WHERE c.client_name = n.client_name
                AND c.ts > n.ts
                AND c.ts <= n.ts + ($3::int * interval '1 minute')
                AND (CASE n.family
                      WHEN 'collaboration' THEN c.tool LIKE 'a2a\_%'
                      WHEN 'large_context' THEN c.tool = 'a2a_pattern_recursive'
                      WHEN 'memory_write'  THEN (c.tool LIKE 'memory\_%' OR c.tool IN ('recall_prompts','search_mandates','graph_neighbors'))
                      WHEN 'memory_read'   THEN (c.tool LIKE 'memory\_%' OR c.tool IN ('recall_prompts','search_mandates','graph_neighbors'))
                      WHEN 'work_item'     THEN (c.tool LIKE 'work\_item%' OR c.tool LIKE 'tag\_%')
                      ELSE false END)
            )) AS converted
          FROM nudge_emissions n
          WHERE n.ts > now() - ($1::int * interval '1 minute')
            AND n.client_name = ANY($2::text[])
          GROUP BY n.family, n.channel
          ORDER BY n.family, n.channel",
    )
    .bind(window_minutes as i32)
    .bind(&allowlist)
    .bind(CONVERSION_WINDOW_MINUTES as i32)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| {
            let nudges: i64 = r.get("nudges");
            let converted: i64 = r.get("converted");
            ConversionStat {
                family: r.get("family"),
                channel: r.get("channel"),
                nudges,
                converted,
                conversion_pct: share_pct(converted, nudges),
            }
        })
        .collect())
}

/// CSM run conformance over the window (bonus signal from `csm_run_traces`).
pub async fn csm_conformance(
    pool: &PgPool,
    window_minutes: i64,
) -> Result<CsmConformance, sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS total, COUNT(*) FILTER (WHERE conformant) AS conformant
         FROM csm_run_traces
         WHERE created_at > now() - ($1::int * interval '1 minute')",
    )
    .bind(window_minutes as i32)
    .fetch_one(pool)
    .await?;
    let total: i64 = row.get("total");
    let conformant: i64 = row.get("conformant");
    Ok(CsmConformance {
        total,
        conformant,
        conformant_pct: share_pct(conformant, total),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Rust classifier must match the live MCP tool namespace, family by
    /// family, so the SQL predicates (which mirror it) measure the right tools.
    #[test]
    fn classify_matches_live_tool_names() {
        assert_eq!(
            Family::classify("a2a_pattern_sequential"),
            vec![Family::A2a]
        );
        assert_eq!(Family::classify("a2a_send_task"), vec![Family::A2a]);
        // RLM is a strict subset of A2A.
        assert_eq!(
            Family::classify("a2a_pattern_recursive"),
            vec![Family::A2a, Family::Rlm]
        );
        assert_eq!(Family::classify("csm_validate_run"), vec![Family::Csm]);
        assert_eq!(
            Family::classify("memory_unified_search"),
            vec![Family::Memory]
        );
        // memory-family tools that lack the `memory_` prefix.
        assert_eq!(Family::classify("graph_neighbors"), vec![Family::Memory]);
        assert_eq!(Family::classify("recall_prompts"), vec![Family::Memory]);
        assert_eq!(Family::classify("search_mandates"), vec![Family::Memory]);
        assert_eq!(Family::classify("work_item_create"), vec![Family::WorkItem]);
        assert_eq!(Family::classify("work_item_claim"), vec![Family::WorkItem]);
        assert_eq!(Family::classify("tag_create"), vec![Family::WorkItem]);
        // Non-family tools classify to nothing.
        assert!(Family::classify("semantic_search").is_empty());
        assert!(Family::classify("orient").is_empty());
        assert!(Family::classify("grep").is_empty());
    }

    #[test]
    fn share_pct_guards_zero() {
        assert_eq!(share_pct(5, 0), 0.0);
        assert_eq!(share_pct(0, 0), 0.0);
        assert!((share_pct(1, 4) - 25.0).abs() < 1e-9);
    }

    #[test]
    fn query_mentions_every_family_alias() {
        let q = build_query();
        for family in Family::ALL {
            assert!(q.contains(&format!("{}_calls", family.key())));
            assert!(q.contains(&format!("{}_sessions", family.key())));
        }
        assert!(q.contains("client_name = ANY($2::text[])"));
    }

    /// The conversion CASE must handle every nudge family key the observe
    /// pipeline can emit, so no nudged family silently falls into `ELSE false`.
    #[test]
    fn conversion_case_covers_all_classifier_families() {
        use crate::sessions::{ToolFamily, tool_family_key};
        let handled = [
            "collaboration",
            "large_context",
            "memory_write",
            "memory_read",
            "work_item",
        ];
        for fam in [
            ToolFamily::Collaboration,
            ToolFamily::LargeContext,
            ToolFamily::MemoryWrite,
            ToolFamily::MemoryRead,
            ToolFamily::WorkItem,
        ] {
            assert!(
                handled.contains(&tool_family_key(fam)),
                "family {fam:?} not handled in the conversion CASE"
            );
        }
    }
}
