//! Report structures and rendering for adoption telemetry.
//!
//! Rendered by hand (markdown / JSON) following the codebase's no-templating
//! idiom. The `src/render` module's `render()` is bound to `QualityReport`, so
//! the simpler adoption report carries its own thin renderers rather than
//! forcing its shape through that type.

use serde_json::{Value, json};

/// Per-family adoption within a single client (or the overall roll-up).
#[derive(Clone, Debug, PartialEq)]
pub struct FamilyStat {
    /// Human-readable family label (e.g. "A2A collaboration").
    pub family: String,
    /// Total calls to tools in this family over the window.
    pub calls: i64,
    /// Distinct (non-empty) MCP sessions that called ≥1 tool in this family.
    pub sessions: i64,
    /// `calls` as a percentage of the client's (or overall) total calls.
    pub call_share_pct: f64,
}

/// Per-client adoption roll-up.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientStat {
    pub client_name: String,
    pub total_calls: i64,
    pub total_sessions: i64,
    pub families: Vec<FamilyStat>,
}

/// The full adoption report over a time window.
#[derive(Clone, Debug, PartialEq)]
pub struct AdoptionReport {
    pub window_minutes: i64,
    pub allowlist: Vec<String>,
    pub clients: Vec<ClientStat>,
    pub overall: Vec<FamilyStat>,
    pub overall_total_calls: i64,
    pub note: String,
}

impl FamilyStat {
    fn to_json(&self) -> Value {
        json!({
            "family": self.family,
            "calls": self.calls,
            "sessions": self.sessions,
            "call_share_pct": round2(self.call_share_pct),
        })
    }
}

impl AdoptionReport {
    /// Structured JSON — the primary form, consumed by the experiment ledger.
    pub fn to_json(&self) -> Value {
        json!({
            "window_minutes": self.window_minutes,
            "allowlist": self.allowlist,
            "overall_total_calls": self.overall_total_calls,
            "overall": self.overall.iter().map(FamilyStat::to_json).collect::<Vec<_>>(),
            "clients": self.clients.iter().map(|c| json!({
                "client_name": c.client_name,
                "total_calls": c.total_calls,
                "total_sessions": c.total_sessions,
                "families": c.families.iter().map(FamilyStat::to_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "note": self.note,
        })
    }

    /// GitHub-flavored Markdown table view.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "## pgmcp tool-family adoption — last {} min\n\n",
            self.window_minutes
        ));
        out.push_str(&format!(
            "Overall calls (real clients): **{}**\n\n### Overall by family\n\n",
            self.overall_total_calls
        ));
        push_family_table(&mut out, &self.overall);
        for client in &self.clients {
            out.push_str(&format!(
                "\n### {} — {} calls, {} sessions\n\n",
                client.client_name, client.total_calls, client.total_sessions
            ));
            push_family_table(&mut out, &client.families);
        }
        out.push_str(&format!("\n> {}\n", self.note));
        out
    }
}

fn push_family_table(out: &mut String, families: &[FamilyStat]) {
    out.push_str("| Family | Calls | Sessions | Call share |\n|---|--:|--:|--:|\n");
    for f in families {
        out.push_str(&format!(
            "| {} | {} | {} | {:.1}% |\n",
            f.family, f.calls, f.sessions, f.call_share_pct
        ));
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}
