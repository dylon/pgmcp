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
}

fn default_true() -> bool {
    true
}

impl Default for ClientProfile {
    fn default() -> Self {
        Self {
            name: "generic".into(),
            output_format: OutputFormat::Markdown,
            default_brief: false,
            include_provenance: true,
            description_overrides: HashMap::new(),
        }
    }
}

impl ClientProfile {
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
        ClientProfile {
            name: "claude-code".into(),
            output_format: OutputFormat::Markdown,
            default_brief: false,
            include_provenance: true,
            description_overrides: HashMap::new(),
        },
        // Claude Code's CLI also identifies as `claude-cli` (confirmed in
        // mcp_tool_calls telemetry); same posture as claude-code.
        ClientProfile {
            name: "claude-cli".into(),
            output_format: OutputFormat::Markdown,
            default_brief: false,
            include_provenance: true,
            description_overrides: HashMap::new(),
        },
        // Codex's MCP client identifies as `codex-mcp-client` (confirmed in
        // telemetry, v0.133.0); `codex` is kept as an alias for older builds.
        ClientProfile {
            name: "codex".into(),
            output_format: OutputFormat::CompactJson,
            default_brief: true,
            include_provenance: false,
            description_overrides: HashMap::new(),
        },
        ClientProfile {
            name: "codex-mcp-client".into(),
            output_format: OutputFormat::CompactJson,
            default_brief: true,
            include_provenance: false,
            description_overrides: HashMap::new(),
        },
        ClientProfile {
            name: "generic".into(),
            output_format: OutputFormat::Markdown,
            default_brief: false,
            include_provenance: true,
            description_overrides: HashMap::new(),
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
}
