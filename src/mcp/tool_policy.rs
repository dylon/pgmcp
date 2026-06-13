//! Usage-adaptive, per-client tool surface (recency-decayed frequency).
//!
//! The adaptive tool surface gives each MCP client a default `tools/list` that
//! is *derived from its own usage* rather than hand-curated, and that grows on
//! demand (via the `enable_tools` meta-tool + `tools/list_changed`).
//!
//! **What the estimator is (and is not).** Each `(client, tool)` pair is scored
//! by an *exponentially time-decayed count of usage events*,
//! `w = Σ exp(-age_days / τ)`, and a tool is exposed when `w ≥ θ`. This is a
//! deterministic recency-weighted frequency estimator (an EWMA of the usage
//! impulse train) — **not** a trained/parametric ML model: nothing is fit by
//! minimizing a loss, and τ/θ are fixed constants, not estimated. "Learned"
//! here means *online adaptation to observed usage* (as in an LFU-with-aging
//! cache), not statistical learning. Full derivation — half-life, the
//! steady-state identity `E[w] = λτ`, the threshold-as-rate-gate `λ ≥ θ/τ`,
//! window-truncation error, and the relationship to EWMA / time-decayed stream
//! aggregates / Hawkes background intensity — is in
//! `docs/design/tool-policy-recency-decay.md`.
//!
//! Three moving parts live here:
//!
//! - [`ToolPolicySnapshot`] — the in-memory, O(1)-lookup view consulted by
//!   `list_tools`: per-client default sets plus a global cold-start prior. Held
//!   behind an `ArcSwap` on [`crate::context::SystemContext`] and hot-swapped by
//!   the `tool-policy-refresh` cron.
//! - [`recompute_and_persist`] / [`load_snapshot`] — the estimator. It scores
//!   every `(client, tool)` pair by the recency-decayed usage frequency above
//!   over the durable `mcp_tool_calls` telemetry, materializes the scores into
//!   `client_tool_policy`, and derives the snapshot. Because every `enable_tools`
//!   call and every native call is itself telemetry, frequently-used tools are
//!   promoted into the default set on the next pass and unused ones decay out —
//!   the surface converges to each client's real working set with zero manual
//!   curation.
//! - [`SessionToolState`] / [`retain_exposed`] — the per-connection overlay: the
//!   set a session has explicitly `enable_tools`-ed, unioned into that session's
//!   exposed surface on top of `mandatory_core ∪ learned_defaults`.
//!
//! The trust/scope boundary: gating only hides tools from `tools/list`; it never
//! makes a tool unreachable. A hidden tool is still dispatchable by name (the
//! `call_tool` meta-tool), so a client that ignores `tools/list_changed`
//! degrades gracefully rather than losing capability.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use sqlx::PgPool;

use crate::mcp::client_profile::{ClientProfile, ToolSurface};
use crate::mcp::tool_domains;

/// Upper bound on tracked per-session tool overlays (TTL/LRU-evicted in
/// `enable_tools`). Generous — each entry is a small `HashSet<String>`.
pub const MAX_TOOL_SESSIONS: usize = 4096;
/// TTL for a session overlay before opportunistic eviction (24 h).
pub const TOOL_SESSION_TTL_SECS: u64 = 24 * 60 * 60;

/// Tunable constants for the recency-decayed usage estimator (fixed *a priori*,
/// not fit). The `tool-policy-refresh` cron uses [`ToolPolicyConfig::default`];
/// only the cron *interval* is operator-configurable (`[cron]
/// tool_policy_interval_secs`). Semantics + tuning guidance:
/// `docs/design/tool-policy-recency-decay.md`.
#[derive(Debug, Clone, Copy)]
pub struct ToolPolicyConfig {
    /// Decay time-constant τ (days) in the score `w = Σ exp(-age_days / τ)`.
    /// Larger ⇒ longer memory. A use today contributes 1.0; a use τ days ago
    /// contributes e⁻¹ ≈ 0.37; the half-life is τ·ln2 ≈ 0.69 τ.
    pub decay_tau_days: f64,
    /// Only telemetry newer than this many days enters the sum (truncation
    /// horizon W). Keep W ≳ 5τ so the dropped tail e^(−W/τ) is negligible.
    pub lookback_days: i64,
    /// Inclusion threshold θ: a `(client, tool)` joins the client's default set
    /// when `w ≥ θ`. By the steady-state identity `E[w] = λτ`, this is a minimum
    /// sustained-usage-rate gate of θ/τ uses/day (defaults ≈ once / 28 days).
    pub weight_threshold: f64,
    /// Size of the global cold-start prior (most-used tools across all clients),
    /// exposed to a client with no usage history of its own.
    pub global_top_n: i64,
}

impl Default for ToolPolicyConfig {
    fn default() -> Self {
        Self {
            decay_tau_days: 14.0,
            lookback_days: 90,
            weight_threshold: 0.5,
            global_top_n: 25,
        }
    }
}

/// Per-connection tool overlay: the set a session has explicitly enabled via the
/// `enable_tools` meta-tool, plus a creation stamp for TTL-based eviction.
#[derive(Debug, Clone)]
pub struct SessionToolState {
    pub enabled: HashSet<String>,
    pub created_at: Instant,
}

impl Default for SessionToolState {
    fn default() -> Self {
        Self {
            enabled: HashSet::new(),
            created_at: Instant::now(),
        }
    }
}

/// The in-memory, O(1)-lookup policy view consulted by `list_tools`.
#[derive(Debug, Clone, Default)]
pub struct ToolPolicySnapshot {
    /// client_name (normalized lowercase) → tool names whose recency-decayed
    /// weight crossed the inclusion threshold.
    learned: HashMap<String, HashSet<String>>,
    /// Global most-used tools — the cold-start prior shown to a client that has
    /// no learned set of its own yet.
    global_top: HashSet<String>,
}

impl ToolPolicySnapshot {
    /// Construct directly (tests / cold start with no DB).
    pub fn new(learned: HashMap<String, HashSet<String>>, global_top: HashSet<String>) -> Self {
        Self {
            learned,
            global_top,
        }
    }

    /// Number of clients with a learned default set (for cron logging).
    pub fn client_count(&self) -> usize {
        self.learned.len()
    }

    /// Retain only the tools this `(client, session)` should see in `tools/list`,
    /// per the profile's [`ToolSurface`] strategy. `All` is a no-op (full
    /// catalog, byte-identical to the unfiltered router). `Learned` exposes
    /// `mandatory_core ∪ learned_defaults(client) ∪ session_enabled`, falling
    /// back to the global prior when the client has no history. `Fixed` exposes
    /// `mandatory_core ∪ (tools in the named domains) ∪ session_enabled`.
    pub fn retain_exposed(
        &self,
        tools: &mut Vec<rmcp::model::Tool>,
        profile: &ClientProfile,
        client_name: &str,
        session_enabled: &HashSet<String>,
    ) {
        if matches!(profile.tool_surface, ToolSurface::All) {
            return; // Full catalog — the common claude-code path.
        }
        let core: HashSet<String> = profile.effective_core().into_iter().collect();
        let learned = self.learned.get(client_name);
        tools.retain(|tool| {
            let name = tool.name.as_ref();
            if core.contains(name) || session_enabled.contains(name) {
                return true;
            }
            match &profile.tool_surface {
                ToolSurface::All => true, // unreachable (handled above)
                ToolSurface::Learned => match learned {
                    Some(set) => set.contains(name),
                    // Cold start: the client has no history → global prior.
                    None => self.global_top.contains(name),
                },
                ToolSurface::Fixed(domains) => tool_domains::domain_of(name)
                    .is_some_and(|d| domains.iter().any(|want| want == d)),
            }
        });
    }
}

/// Recompute every `(client, tool)` recency-decayed weight from `mcp_tool_calls`,
/// fully replace `client_tool_policy` with the fresh scores (in one transaction),
/// and return the derived in-memory snapshot. Called by the `tool-policy-refresh`
/// cron.
pub async fn recompute_and_persist(
    pool: &PgPool,
    cfg: &ToolPolicyConfig,
) -> Result<ToolPolicySnapshot, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Full recompute: clear, then re-aggregate the window. Rows that fell out of
    // the lookback window simply do not reappear, so a client that stopped using
    // a tool loses it (decay-to-zero) rather than retaining a stale weight.
    sqlx::query("DELETE FROM client_tool_policy")
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        "INSERT INTO client_tool_policy (client_name, tool_name, weight, last_used, updated_at)
         SELECT client_name,
                tool,
                SUM(exp(-EXTRACT(EPOCH FROM (now() - ts)) / ($1 * 86400.0))) AS weight,
                MAX(ts) AS last_used,
                now()
         FROM mcp_tool_calls
         WHERE ts > now() - make_interval(days => $2::int)
           AND outcome = 'ok'
           AND client_name IS NOT NULL
           AND client_name <> ''
         GROUP BY client_name, tool",
    )
    .bind(cfg.decay_tau_days)
    .bind(cfg.lookback_days)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    load_snapshot(pool, cfg).await
}

/// Build the in-memory snapshot from the persisted `client_tool_policy` table.
/// Used at daemon startup (before the first cron pass) and as the second half of
/// [`recompute_and_persist`].
pub async fn load_snapshot(
    pool: &PgPool,
    cfg: &ToolPolicyConfig,
) -> Result<ToolPolicySnapshot, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct PolicyRow {
        client_name: String,
        tool_name: String,
        weight: f64,
    }
    let rows: Vec<PolicyRow> =
        sqlx::query_as("SELECT client_name, tool_name, weight FROM client_tool_policy")
            .fetch_all(pool)
            .await?;

    let mut learned: HashMap<String, HashSet<String>> = HashMap::new();
    for row in &rows {
        if row.weight >= cfg.weight_threshold {
            learned
                .entry(row.client_name.to_lowercase())
                .or_default()
                .insert(row.tool_name.clone());
        }
    }

    let global_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT tool_name
         FROM client_tool_policy
         GROUP BY tool_name
         ORDER BY SUM(weight) DESC
         LIMIT $1",
    )
    .bind(cfg.global_top_n)
    .fetch_all(pool)
    .await?;
    let global_top: HashSet<String> = global_rows.into_iter().map(|(t,)| t).collect();

    Ok(ToolPolicySnapshot::new(learned, global_top))
}

/// Opportunistically evict expired / overflowing session overlays. Called from
/// `enable_tools` before inserting, so the per-connection map stays bounded
/// without a dedicated sweeper task: first drop entries older than `ttl_secs`,
/// then, if still over `max`, drop the oldest until at `max`.
pub fn prune_sessions(
    sessions: &dashmap::DashMap<String, SessionToolState>,
    max: usize,
    ttl_secs: u64,
) {
    let ttl = std::time::Duration::from_secs(ttl_secs);
    sessions.retain(|_, state| state.created_at.elapsed() < ttl);
    if sessions.len() <= max {
        return;
    }
    // Over capacity: collect (key, age) and evict oldest down to `max`.
    let mut ages: Vec<(String, std::time::Duration)> = sessions
        .iter()
        .map(|e| (e.key().clone(), e.value().created_at.elapsed()))
        .collect();
    // Oldest first (largest elapsed).
    ages.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    let evict = sessions.len().saturating_sub(max);
    for (key, _) in ages.into_iter().take(evict) {
        sessions.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::client_profile::ClientProfile;

    fn tool(name: &str) -> rmcp::model::Tool {
        use std::sync::Arc;
        let schema: Arc<serde_json::Map<String, serde_json::Value>> =
            Arc::new(serde_json::Map::new());
        rmcp::model::Tool::new(name.to_string(), "desc".to_string(), schema)
    }

    fn catalog() -> Vec<rmcp::model::Tool> {
        [
            "orient",
            "semantic_search",
            "central_functions",
            "lockset_races",
            "z3_smoke",
        ]
        .iter()
        .map(|n| tool(n))
        .collect()
    }

    #[test]
    fn all_surface_is_a_noop() {
        let snap = ToolPolicySnapshot::default();
        let profile = ClientProfile {
            tool_surface: ToolSurface::All,
            ..ClientProfile::default()
        };
        let mut tools = catalog();
        snap.retain_exposed(&mut tools, &profile, "claude-code", &HashSet::new());
        assert_eq!(tools.len(), 5, "All must expose the entire catalog");
    }

    #[test]
    fn learned_surface_is_core_plus_learned_plus_session() {
        // Learned set for the client includes one long-tail tool.
        let mut learned = HashMap::new();
        learned.insert(
            "codex-mcp-client".to_string(),
            HashSet::from(["central_functions".to_string()]),
        );
        let snap = ToolPolicySnapshot::new(learned, HashSet::new());
        // Custom small core so the assertion is precise.
        let profile = ClientProfile {
            tool_surface: ToolSurface::Learned,
            mandatory_core: vec!["orient".into()],
            ..ClientProfile::default()
        };
        let session = HashSet::from(["z3_smoke".to_string()]);
        let mut tools = catalog();
        snap.retain_exposed(&mut tools, &profile, "codex-mcp-client", &session);
        let names: HashSet<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(
            names,
            HashSet::from(["orient", "central_functions", "z3_smoke"]),
            "Learned = core ∪ learned ∪ session"
        );
    }

    #[test]
    fn learned_cold_start_uses_global_prior() {
        let snap = ToolPolicySnapshot::new(
            HashMap::new(),
            HashSet::from(["semantic_search".to_string()]),
        );
        let profile = ClientProfile {
            tool_surface: ToolSurface::Learned,
            mandatory_core: vec!["orient".into()],
            ..ClientProfile::default()
        };
        let mut tools = catalog();
        snap.retain_exposed(&mut tools, &profile, "brand-new-client", &HashSet::new());
        let names: HashSet<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(
            names,
            HashSet::from(["orient", "semantic_search"]),
            "an unseen client gets core ∪ global prior"
        );
    }

    #[test]
    fn default_core_keeps_a_learned_client_self_expanding() {
        // With the real DEFAULT_MANDATORY_CORE, a zero-history client must still
        // see the discovery + expansion meta-tools.
        let snap = ToolPolicySnapshot::default();
        let profile = ClientProfile {
            tool_surface: ToolSurface::Learned,
            ..ClientProfile::default()
        };
        let mut tools = vec![
            tool("enable_tools"),
            tool("tool_catalog"),
            tool("call_tool"),
            tool("central_functions"),
        ];
        snap.retain_exposed(&mut tools, &profile, "unseen", &HashSet::new());
        let names: HashSet<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(names.contains("enable_tools"));
        assert!(names.contains("tool_catalog"));
        assert!(names.contains("call_tool"));
        assert!(
            !names.contains("central_functions"),
            "a long-tail tool stays hidden until used or enabled"
        );
    }

    /// Headline token-regression test: over the REAL assembled catalog, a
    /// `Learned` client with no history sees only its mandatory core (a ~90%+
    /// byte reduction), while an `All` client (claude-code) sees the byte-for-byte
    /// full catalog. Guards both the savings and the no-regression-for-claude
    /// guarantee.
    #[test]
    fn learned_surface_shrinks_real_tools_list_while_all_stays_full() {
        use crate::mcp::server::McpServer;
        let catalog = McpServer::static_tool_catalog();
        let full_tokens = serialized_token_estimate(&catalog);
        assert!(catalog.len() > 200, "expected the full ~330-tool catalog");

        let snap = ToolPolicySnapshot::default(); // cold start: no learned history
        let empty = HashSet::new();

        // claude-code (All): byte-identical full catalog.
        let claude = ClientProfile {
            tool_surface: ToolSurface::All,
            ..ClientProfile::default()
        };
        let mut all_tools = catalog.clone();
        snap.retain_exposed(&mut all_tools, &claude, "claude-code", &empty);
        assert_eq!(
            all_tools.len(),
            catalog.len(),
            "All must expose the entire catalog unchanged"
        );

        // codex (Learned, cold): only the mandatory core survives.
        let codex = ClientProfile {
            tool_surface: ToolSurface::Learned,
            ..ClientProfile::default()
        };
        let mut lean = catalog.clone();
        snap.retain_exposed(&mut lean, &codex, "codex-mcp-client", &empty);

        // Every default-core tool that actually exists is present, and nothing else.
        let core: HashSet<&str> = crate::mcp::client_profile::DEFAULT_MANDATORY_CORE
            .iter()
            .copied()
            .collect();
        for tool in &lean {
            assert!(
                core.contains(tool.name.as_ref()),
                "lean surface leaked a non-core tool: {}",
                tool.name
            );
        }
        assert!(
            lean.len() >= 10 && lean.len() <= core.len(),
            "lean surface should be ~the core ({} tools), got {}",
            core.len(),
            lean.len()
        );

        let lean_tokens = serialized_token_estimate(&lean);
        assert!(
            lean_tokens * 5 < full_tokens,
            "Learned surface must cut tools/list by >80% (full≈{full_tokens} tok, \
             lean≈{lean_tokens} tok)"
        );
    }

    /// ~bytes/4 token estimate of a tool list's serialized `tools/list` payload.
    fn serialized_token_estimate(tools: &[rmcp::model::Tool]) -> usize {
        serde_json::to_string(tools)
            .map(|s| s.len() / 4)
            .unwrap_or(0)
    }

    #[test]
    fn fixed_surface_gates_by_domain() {
        let snap = ToolPolicySnapshot::default();
        let profile = ClientProfile {
            // `orient`/`semantic_search` live in domain "core"; gate to it.
            tool_surface: ToolSurface::Fixed(vec!["core".into()]),
            mandatory_core: vec![],
            ..ClientProfile::default()
        };
        let mut tools = vec![
            tool("orient"),
            tool("semantic_search"),
            tool("lockset_races"),
        ];
        snap.retain_exposed(&mut tools, &profile, "x", &HashSet::new());
        // `orient`/`semantic_search` are in the default core too, so they survive
        // regardless; the point is the non-core, non-"core"-domain tool is gone.
        let names: HashSet<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(!names.contains("lockset_races"));
    }
}
