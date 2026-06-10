//! `tool_substring_search` (Phase 8).
use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::suffix_automaton::SubstringIndex;
use crate::mcp::server::SubstringSearchParams;
use crate::mcp::tools::sota_helpers::json_result;

const MAX_SUBSTRING_NEEDLE_BYTES: usize = 4_096;
const MAX_SUBSTRING_HAYSTACK_TERMS: usize = 5_000;
const MAX_SUBSTRING_TERM_BYTES: usize = 4_096;
const MAX_SUBSTRING_TOTAL_BYTES: usize = 1_048_576;

/// Alias so the `dispatch_tool!` CLI macro (which calls `tool_<name>`) can reach
/// this tool's `run` body — the MCP `#[tool_router]` path calls `run` directly.
pub use self::run as tool_substring_search;

pub async fn run(
    ctx: &SystemContext,
    params: SubstringSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let needle = validate_substring_part("needle", &params.needle, MAX_SUBSTRING_NEEDLE_BYTES)?;
    if params.haystack.len() > MAX_SUBSTRING_HAYSTACK_TERMS {
        return Err(McpError::invalid_params(
            format!("haystack must contain at most {MAX_SUBSTRING_HAYSTACK_TERMS} terms"),
            None,
        ));
    }

    let mut total_bytes = needle.len();
    let mut terms = BTreeSet::new();
    for term in &params.haystack {
        validate_substring_part("haystack entries", term, MAX_SUBSTRING_TERM_BYTES)?;
        total_bytes = total_bytes
            .checked_add(term.len())
            .filter(|bytes| *bytes <= MAX_SUBSTRING_TOTAL_BYTES)
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!(
                        "haystack total size must be at most {MAX_SUBSTRING_TOTAL_BYTES} bytes"
                    ),
                    None,
                )
            })?;
        terms.insert(term.as_str());
    }

    let found = if terms.is_empty() {
        false
    } else {
        let idx = SubstringIndex::from_terms(terms.iter().copied());
        idx.contains_substring(needle)
    };
    json_result(&json!({
        "needle": needle,
        "haystack_size": terms.len(),
        "contains_substring": found,
    }))
}

fn validate_substring_part<'a>(
    field: &str,
    raw: &'a str,
    max_bytes: usize,
) -> Result<&'a str, McpError> {
    if raw.is_empty() {
        return Err(McpError::invalid_params(
            format!("{field} must be non-empty"),
            None,
        ));
    }
    if raw.len() > max_bytes {
        return Err(McpError::invalid_params(
            format!("{field} must be at most {max_bytes} bytes"),
            None,
        ));
    }
    Ok(raw)
}
