//! Runtime-derived tool-name → domain map.
//!
//! pgmcp composes its MCP tool surface from ~33 per-domain routers
//! (`McpServer::assembled_tool_router`). The adaptive per-client tool surface
//! (usage-adaptive recency-decayed defaults + dynamic enable/disable, see
//! [`crate::mcp::tool_policy`]) gates tools by *domain*, but `rmcp::model::Tool`
//! carries no domain tag. Rather than hand-maintain a 300-line name→domain table
//! that would silently drift from the routers, we derive the map at first use
//! from the *same* per-domain routers the server assembles, via
//! [`McpServer::domain_tool_names`]. This guarantees the map can never disagree
//! with the live tool set (a golden test asserts the two stay in lockstep).

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::mcp::server::McpServer;

static MAP: OnceLock<HashMap<String, &'static str>> = OnceLock::new();

/// The domain a tool belongs to (the base name of its per-domain router, e.g.
/// `"core"`, `"graph_func"`, `"work_items_a"`), or `None` for an unknown name.
pub fn domain_of(name: &str) -> Option<&'static str> {
    map().get(name).copied()
}

/// The full name→domain map, built once (lazily) from the per-domain routers.
pub fn map() -> &'static HashMap<String, &'static str> {
    MAP.get_or_init(build)
}

/// Every tool name in a domain (empty if the domain is unknown). Used by
/// `enable_tools(domain=…)` to expand a domain into its tool set.
pub fn tools_in_domain(domain: &str) -> Vec<String> {
    McpServer::domain_tool_names()
        .into_iter()
        .find(|(d, _)| *d == domain)
        .map(|(_, names)| names)
        .unwrap_or_default()
}

fn build() -> HashMap<String, &'static str> {
    let pairs = McpServer::domain_tool_names();
    let capacity = pairs.iter().map(|(_, names)| names.len()).sum();
    let mut map = HashMap::with_capacity(capacity);
    for (domain, names) in pairs {
        for name in names {
            map.insert(name, domain);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tool the server actually exposes resolves to exactly one domain,
    /// and the map carries no phantom entries. Guards against a new
    /// `router_<domain>` being summed into `assembled_tool_router()` without
    /// being mirrored into `domain_tool_names()` (a gated tool with no domain
    /// would be invisible to every `Learned` client).
    #[test]
    fn every_assembled_tool_has_exactly_one_domain() {
        let catalog = McpServer::static_tool_catalog();
        let map = map();
        for tool in &catalog {
            assert!(
                map.contains_key(tool.name.as_ref()),
                "assembled tool `{}` has no domain — add its router to domain_tool_names()",
                tool.name
            );
        }
        assert_eq!(
            map.len(),
            catalog.len(),
            "domain map size ({}) != catalog size ({}) — a tool name is shared across two \
             routers, or domain_tool_names() lists a tool the assembled router does not",
            map.len(),
            catalog.len()
        );
    }
}
