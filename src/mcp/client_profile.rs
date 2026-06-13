//! Memory-server Phase 10: per-client protocol customization.
//!
//! Three things live here:
//!
//! - `OutputFormat` — closed-set enum selecting the wire-encoding
//!   style for a tool's response (Markdown / CompactJson / Text).
//! - `ClientProfile` — what we know about one client (its preferred
//!   `OutputFormat`, whether it accepts handle-pattern outputs by
//!   default, whether to include full provenance, per-tool description
//!   overrides).
//! - `ClientProfileRegistry` — case-insensitive name → profile lookup
//!   loaded from `assets/client_profiles.toml` at daemon startup, with
//!   a baked-in fallback registry so a fresh checkout works without
//!   any external config file.
//!
//! Token-efficiency commitments (decision 11) materialize here:
//! Codex's profile sets `default_brief = true` and `include_provenance
//! = false`; Claude Code's is the opposite. Tool bodies that want to
//! honour these defaults call `OutputFormat::serialize_value`.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Pretty-printed JSON (the default surface today).
    #[default]
    Markdown,
    /// Whitespace-minimized JSON — best for downstream re-prompting.
    CompactJson,
    /// Plain text — for clients that don't render Markdown or JSON
    /// well. Used by `pgmcp context` CLI output.
    Text,
}

impl OutputFormat {
    /// Serialize an arbitrary JSON value according to this format.
    /// Tool bodies that care about per-client output use this instead
    /// of `serde_json::to_string_pretty`.
    pub fn serialize_value(self, v: &serde_json::Value) -> String {
        match self {
            Self::Markdown => serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()),
            Self::CompactJson => serde_json::to_string(v).unwrap_or_else(|_| v.to_string()),
            Self::Text => render_value_as_text(v),
        }
    }
}

fn render_value_as_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .map(render_value_as_text)
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{}: {}", k, render_value_as_text(v)))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Per-request rendering posture, resolved once at the MCP dispatch boundary
/// from the caller's [`ClientProfile`]. Copy-cheap and propagated to tool bodies
/// via a task-local (see [`with_render_ctx`]) so the ~88 tools that serialize
/// through `sota_helpers::json_result` honor the caller's `output_format`
/// without threading a parameter through 300+ tool signatures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderCtx {
    pub output_format: OutputFormat,
    pub default_brief: bool,
    pub include_provenance: bool,
}

impl Default for RenderCtx {
    /// Markdown (pretty JSON) + full content + provenance — the
    /// claude-code / generic posture. Returned when no per-request context has
    /// been installed (CLI dispatch, tests, or a body polled outside a
    /// [`with_render_ctx`] scope), so the fallback never compacts a rich
    /// client's output.
    fn default() -> Self {
        Self {
            output_format: OutputFormat::Markdown,
            default_brief: false,
            include_provenance: true,
        }
    }
}

impl RenderCtx {
    /// Resolve a render context from a client profile.
    pub fn from_profile(p: &ClientProfile) -> Self {
        Self {
            output_format: p.output_format,
            default_brief: p.default_brief,
            include_provenance: p.include_provenance,
        }
    }

    /// Serialize a JSON value per this context's `output_format`.
    pub fn serialize_value(self, v: &serde_json::Value) -> String {
        self.output_format.serialize_value(v)
    }
}

tokio::task_local! {
    /// Request-scoped rendering context. Installed once per MCP tool dispatch by
    /// `McpServer::call_tool` (and defaulted on the CLI path); read by
    /// `sota_helpers::json_result` and the search-tool bodies so each result is
    /// encoded in the caller's preferred `OutputFormat`. Reading outside a scope
    /// yields `RenderCtx::default()` (Markdown) — a safe degradation, never an
    /// error.
    static CURRENT_RENDER_CTX: RenderCtx;
}

/// Run `fut` with `rc` installed as the current request-scoped [`RenderCtx`].
/// The MCP dispatch entry points wrap the whole tool future in this so any body
/// polled within (rmcp awaits handlers inline, never on a fresh task) reads the
/// caller's posture via [`current_render_ctx`].
pub async fn with_render_ctx<F>(rc: RenderCtx, fut: F) -> F::Output
where
    F: std::future::Future,
{
    CURRENT_RENDER_CTX.scope(rc, fut).await
}

/// The current request-scoped [`RenderCtx`], or `RenderCtx::default()` when none
/// is installed (CLI dispatch, tests, or a body polled outside a scope).
pub fn current_render_ctx() -> RenderCtx {
    CURRENT_RENDER_CTX.try_with(|rc| *rc).unwrap_or_default()
}

/// How a client's `tools/list` surface is selected by the adaptive tool policy
/// (`crate::mcp::tool_policy`). A closed set of selection strategies (ADR-003
/// idiom: the Rust enum is the source of truth; no DB CHECK since it is never
/// persisted as a column).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurface {
    /// Expose the entire catalog (claude-code's default). The adaptive filter is
    /// a no-op, so `tools/list` is byte-identical to the unfiltered router.
    All,
    /// Expose `mandatory_core ∪ learned_defaults(client) ∪ session_enabled`,
    /// where `learned_defaults` is recomputed from usage telemetry by the
    /// `tool-policy-refresh` cron. The token-sensitive default for
    /// codex / generic / unknown clients.
    #[default]
    Learned,
    /// Expose `mandatory_core ∪ (every tool in these domains) ∪ session_enabled`.
    /// A deterministic override independent of telemetry; domain names are the
    /// `tool_domains` base names (e.g. "graph_core", "work_items_a"). In TOML:
    /// `tool_surface = { fixed = ["graph_core", "concurrency"] }`.
    Fixed(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientProfile {
    /// Matched case-insensitively against MCP `clientInfo.name`.
    pub name: String,
    #[serde(default)]
    pub output_format: OutputFormat,
    /// When true, retrieval tools default to handle pattern (compact
    /// {id, name, ...} payloads) and require explicit `expand=true`
    /// to inline full content.
    #[serde(default)]
    pub default_brief: bool,
    /// When true, observation/relation rows include source-session and
    /// source-prompt provenance. Disabled for token-efficient clients.
    #[serde(default = "default_true")]
    pub include_provenance: bool,
    /// Per-tool description overrides. The MCP layer renders these
    /// into tool annotations when the request's client matches.
    #[serde(default)]
    pub description_overrides: HashMap<String, String>,
    /// How this client's `tools/list` surface is selected. Defaults to
    /// `Learned` (lean, self-expanding); claude-code overrides to `All`.
    #[serde(default)]
    pub tool_surface: ToolSurface,
    /// Tools always exposed regardless of `tool_surface` gating (the discovery /
    /// dynamic-expansion meta-tools plus the first-reach core). Empty ⇒
    /// [`DEFAULT_MANDATORY_CORE`] (see [`ClientProfile::effective_core`]).
    #[serde(default)]
    pub mandatory_core: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// The always-exposed core tools for `Learned` / `Fixed` clients when a profile
/// does not specify its own `mandatory_core`. These are the first tools an agent
/// reaches for plus the discovery / dynamic-expansion meta-tools, so even a
/// client with zero usage history — and one that ignores `tools/list_changed` —
/// has a usable, self-expanding surface (it can always `tool_catalog` to browse,
/// `enable_tools` to add natively, or `call_tool` as a direct fallback).
pub const DEFAULT_MANDATORY_CORE: &[&str] = &[
    // Discovery + dynamic expansion (must always be present for a Learned client
    // to reach the rest of the catalog).
    "tool_catalog",
    "enable_tools",
    "disable_tools",
    "call_tool",
    // Orientation + search — the highest-frequency first reach.
    "orient",
    "semantic_search",
    "text_search",
    "grep",
    "hybrid_search",
    // Read / inventory.
    "read_file",
    "file_info",
    "project_tree",
    "list_projects",
    "index_stats",
    "mandate_context",
    // Memory entry point.
    "memory_unified_search",
];

impl Default for ClientProfile {
    fn default() -> Self {
        Self {
            name: "generic".into(),
            output_format: OutputFormat::Markdown,
            default_brief: false,
            include_provenance: true,
            description_overrides: HashMap::new(),
            tool_surface: ToolSurface::Learned,
            mandatory_core: Vec::new(),
        }
    }
}

impl ClientProfile {
    /// The effective always-exposed core for this profile: its own
    /// `mandatory_core` if set, else [`DEFAULT_MANDATORY_CORE`].
    pub fn effective_core(&self) -> Vec<String> {
        if self.mandatory_core.is_empty() {
            DEFAULT_MANDATORY_CORE
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            self.mandatory_core.clone()
        }
    }

    /// Apply this profile's per-tool description overrides to `tools` in place.
    /// No-op when the profile has no overrides (e.g. claude-code / generic).
    /// Extracted from the hand-written `ServerHandler::list_tools` so the rewrite
    /// is unit-testable without an rmcp peer.
    pub fn apply_description_overrides(&self, tools: &mut [rmcp::model::Tool]) {
        if self.description_overrides.is_empty() {
            return;
        }
        for tool in tools.iter_mut() {
            if let Some(ov) = self.description_overrides.get(tool.name.as_ref()) {
                tool.description = Some(ov.clone().into());
            }
        }
    }
}

/// Built-in fallback profiles. Always available; overridden by
/// `assets/client_profiles.toml` when present.
fn builtin_profiles() -> Vec<ClientProfile> {
    vec![
        // Claude Code (the primary interactive client) gets the FULL catalog by
        // default — `tool_surface = All` makes the adaptive filter a no-op, so
        // its `tools/list` is byte-identical to the unfiltered router.
        ClientProfile {
            name: "claude-code".into(),
            output_format: OutputFormat::Markdown,
            default_brief: false,
            include_provenance: true,
            description_overrides: HashMap::new(),
            tool_surface: ToolSurface::All,
            mandatory_core: Vec::new(),
        },
        // Claude Code's CLI also identifies as `claude-cli` (confirmed in
        // mcp_tool_calls telemetry); same posture as claude-code.
        ClientProfile {
            name: "claude-cli".into(),
            output_format: OutputFormat::Markdown,
            default_brief: false,
            include_provenance: true,
            description_overrides: HashMap::new(),
            tool_surface: ToolSurface::All,
            mandatory_core: Vec::new(),
        },
        // Codex's MCP client identifies as `codex-mcp-client` (confirmed in
        // telemetry, v0.133.0); `codex` is kept as an alias for older builds.
        // Token-sensitive: lean `Learned` surface that self-expands on demand.
        ClientProfile {
            name: "codex".into(),
            output_format: OutputFormat::CompactJson,
            default_brief: true,
            include_provenance: false,
            description_overrides: HashMap::new(),
            tool_surface: ToolSurface::Learned,
            mandatory_core: Vec::new(),
        },
        ClientProfile {
            name: "codex-mcp-client".into(),
            output_format: OutputFormat::CompactJson,
            default_brief: true,
            include_provenance: false,
            description_overrides: HashMap::new(),
            tool_surface: ToolSurface::Learned,
            mandatory_core: Vec::new(),
        },
        // Unknown clients fall back to `generic`: a lean, self-expanding surface.
        ClientProfile {
            name: "generic".into(),
            output_format: OutputFormat::Markdown,
            default_brief: false,
            include_provenance: true,
            description_overrides: HashMap::new(),
            tool_surface: ToolSurface::Learned,
            mandatory_core: Vec::new(),
        },
    ]
}

#[derive(Debug, Clone, Default)]
pub struct ClientProfileRegistry {
    /// Case-insensitive name → profile.
    by_name: HashMap<String, ClientProfile>,
    fallback: ClientProfile,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ClientProfileFile {
    #[serde(default)]
    profiles: Vec<ClientProfile>,
}

/// Normalize a client-supplied name so lookups don't depend on
/// capitalization or punctuation style. `Claude Code` ⇒ `claude-code`,
/// `claude_code` ⇒ `claude-code`.
fn normalize_client_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|c| match c {
            ' ' | '_' | '\t' => '-',
            _ => c,
        })
        .collect()
}

impl ClientProfileRegistry {
    /// Build the registry from the built-in fallback set, then layer
    /// `assets/client_profiles.toml` on top when present. Missing /
    /// malformed file → fall back to built-ins (logged via `tracing`).
    pub fn load_or_builtin(toml_path: &Path) -> Self {
        let mut by_name: HashMap<String, ClientProfile> = HashMap::new();
        for p in builtin_profiles() {
            by_name.insert(normalize_client_name(&p.name), p);
        }
        if toml_path.exists() {
            match std::fs::read_to_string(toml_path) {
                Ok(s) => match toml::from_str::<ClientProfileFile>(&s) {
                    Ok(file) => {
                        for p in file.profiles {
                            by_name.insert(normalize_client_name(&p.name), p);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %toml_path.display(),
                            error = %e,
                            "client_profiles.toml parse failed; using built-in profiles"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        path = %toml_path.display(),
                        error = %e,
                        "client_profiles.toml read failed; using built-in profiles"
                    );
                }
            }
        }
        let fallback = by_name
            .get("generic")
            .cloned()
            .unwrap_or_else(ClientProfile::default);
        Self { by_name, fallback }
    }

    /// Resolve a profile by `clientInfo.name`. Falls back to the
    /// `generic` profile when the name doesn't match. Case-insensitive
    /// and tolerant of whitespace/underscore vs hyphen ("Claude Code",
    /// "claude_code", and "claude-code" all resolve to the built-in
    /// `claude-code` profile).
    pub fn for_client(&self, name: &str) -> &ClientProfile {
        let key = normalize_client_name(name);
        self.by_name.get(&key).unwrap_or(&self.fallback)
    }

    /// All profiles, for introspection / debugging.
    pub fn all(&self) -> Vec<&ClientProfile> {
        self.by_name.values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_format_serializes_value_per_variant() {
        let v = serde_json::json!({"a": 1, "b": [2, 3]});
        let m = OutputFormat::Markdown.serialize_value(&v);
        let c = OutputFormat::CompactJson.serialize_value(&v);
        let t = OutputFormat::Text.serialize_value(&v);
        assert!(m.contains("  ")); // pretty indent
        assert!(c.starts_with("{") && !c.contains("\n"));
        assert!(t.contains("a: 1"));
    }

    #[test]
    fn registry_returns_baked_in_profiles() {
        let reg = ClientProfileRegistry::load_or_builtin(Path::new("/nonexistent/path.toml"));
        let p = reg.for_client("Claude Code");
        assert_eq!(p.name.to_lowercase(), "claude-code");
        assert_eq!(p.output_format, OutputFormat::Markdown);

        let p = reg.for_client("Codex");
        assert!(p.default_brief);
        assert!(!p.include_provenance);
        assert_eq!(p.output_format, OutputFormat::CompactJson);
    }

    #[test]
    fn registry_falls_back_to_generic_for_unknown_client() {
        let reg = ClientProfileRegistry::load_or_builtin(Path::new("/nonexistent/path.toml"));
        let p = reg.for_client("some-unknown-client");
        assert_eq!(p.name.to_lowercase(), "generic");
    }

    #[test]
    fn registry_loads_overrides_from_toml() {
        let toml_src = r#"
            [[profiles]]
            name = "custom-client"
            output_format = "compact_json"
            default_brief = true
            include_provenance = false
        "#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("profiles.toml");
        std::fs::write(&path, toml_src).expect("write");
        let reg = ClientProfileRegistry::load_or_builtin(&path);
        let p = reg.for_client("custom-client");
        assert_eq!(p.output_format, OutputFormat::CompactJson);
        assert!(p.default_brief);
    }

    #[test]
    fn apply_description_overrides_rewrites_only_matching_tools() {
        use std::sync::Arc;
        let mut overrides = HashMap::new();
        overrides.insert("grep".to_string(), "terse grep".to_string());
        let codex = ClientProfile {
            name: "codex-mcp-client".into(),
            output_format: OutputFormat::CompactJson,
            default_brief: true,
            include_provenance: false,
            description_overrides: overrides,
            tool_surface: ToolSurface::Learned,
            mandatory_core: Vec::new(),
        };
        let schema: Arc<serde_json::Map<String, serde_json::Value>> =
            Arc::new(serde_json::Map::new());
        let mut tools = vec![
            rmcp::model::Tool::new("grep", "original grep desc", schema.clone()),
            rmcp::model::Tool::new("semantic_search", "original ss desc", schema.clone()),
        ];
        codex.apply_description_overrides(&mut tools);
        assert_eq!(tools[0].description.as_deref(), Some("terse grep"));
        // Non-overridden tools keep their base description.
        assert_eq!(tools[1].description.as_deref(), Some("original ss desc"));

        // A profile with no overrides (claude-code / generic) is a no-op.
        let before = tools[0].description.clone();
        ClientProfile::default().apply_description_overrides(&mut tools);
        assert_eq!(tools[0].description, before);
    }

    #[test]
    fn builtin_tool_surfaces_match_policy() {
        let reg = ClientProfileRegistry::load_or_builtin(Path::new("/nonexistent/path.toml"));
        // Claude Code gets the full catalog by default.
        assert_eq!(reg.for_client("claude-code").tool_surface, ToolSurface::All);
        assert_eq!(reg.for_client("claude-cli").tool_surface, ToolSurface::All);
        // Token-sensitive / unknown clients lean.
        assert_eq!(
            reg.for_client("codex-mcp-client").tool_surface,
            ToolSurface::Learned
        );
        assert_eq!(
            reg.for_client("some-unknown-client").tool_surface,
            ToolSurface::Learned
        );
    }

    #[test]
    fn effective_core_falls_back_to_default_then_honors_override() {
        let p = ClientProfile::default();
        assert_eq!(p.effective_core().len(), DEFAULT_MANDATORY_CORE.len());
        // The discovery + dynamic-expansion tools must always be in the default
        // core, else a Learned client could never reach the rest of the catalog.
        for must in ["tool_catalog", "enable_tools", "disable_tools", "call_tool"] {
            assert!(
                p.effective_core().iter().any(|t| t == must),
                "default core missing {must}"
            );
        }
        let custom = ClientProfile {
            mandatory_core: vec!["orient".into()],
            ..ClientProfile::default()
        };
        assert_eq!(custom.effective_core(), vec!["orient".to_string()]);
    }

    #[test]
    fn tool_surface_round_trips_through_toml() {
        let toml_src = r#"
            [[profiles]]
            name = "all-client"
            tool_surface = "all"

            [[profiles]]
            name = "fixed-client"
            tool_surface = { fixed = ["graph_core", "concurrency"] }
            mandatory_core = ["orient"]
        "#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("profiles.toml");
        std::fs::write(&path, toml_src).expect("write");
        let reg = ClientProfileRegistry::load_or_builtin(&path);
        assert_eq!(reg.for_client("all-client").tool_surface, ToolSurface::All);
        assert_eq!(
            reg.for_client("fixed-client").tool_surface,
            ToolSurface::Fixed(vec!["graph_core".into(), "concurrency".into()])
        );
        assert_eq!(
            reg.for_client("fixed-client").effective_core(),
            vec!["orient".to_string()]
        );
    }

    #[test]
    fn render_ctx_from_profile_and_default() {
        let codex = ClientProfile {
            output_format: OutputFormat::CompactJson,
            default_brief: true,
            include_provenance: false,
            ..ClientProfile::default()
        };
        let rc = RenderCtx::from_profile(&codex);
        assert_eq!(rc.output_format, OutputFormat::CompactJson);
        assert!(rc.default_brief);
        assert!(!rc.include_provenance);

        // The default (used when no request scope is installed) is rich Markdown.
        let d = RenderCtx::default();
        assert_eq!(d.output_format, OutputFormat::Markdown);
        assert!(!d.default_brief);
        assert!(d.include_provenance);

        let v = serde_json::json!({"k": 1});
        assert!(d.serialize_value(&v).contains('\n')); // pretty
        assert!(!rc.serialize_value(&v).contains('\n')); // compact
    }

    #[tokio::test]
    async fn current_render_ctx_reflects_scope_and_falls_back() {
        // Outside any scope: the safe default.
        assert_eq!(current_render_ctx(), RenderCtx::default());
        // Inside a scope: the installed context.
        let rc = RenderCtx {
            output_format: OutputFormat::CompactJson,
            default_brief: true,
            include_provenance: false,
        };
        with_render_ctx(rc, async {
            assert_eq!(current_render_ctx(), rc);
        })
        .await;
        // After the scope: back to default.
        assert_eq!(current_render_ctx(), RenderCtx::default());
    }
}
