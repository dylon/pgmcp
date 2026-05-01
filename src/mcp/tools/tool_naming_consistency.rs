//! `tool_naming_consistency` — flag identifiers whose case convention diverges
//! from the dominant convention in their containing directory.
//!
//! Powered by `file_symbols` (Tier-0e tree-sitter pass). The cron job
//! `symbol-extraction` populates that table; if it hasn't run yet for the
//! project, this tool soft-fails with `health.symbols_present:false` and a
//! guidance message — never an error.
//!
//! Per-(directory, kind) dominance is the unit of analysis: snake-case
//! functions and PascalCase structs in the same directory are NOT mutually
//! flagged, because they're the idiomatic mix in most languages. A symbol is
//! flagged only when, within its directory and kind, the dominant convention
//! reaches `min_dominance` (default 0.7) AND the symbol's own convention
//! differs.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde::Serialize;
use serde_json::json;
use tracing::{debug, info};

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::fix_actions::{
    EstimatedEffort, FixAction, LocationRef, PathRange, RecommendedFix, TargetPath, TargetRef,
};
use crate::mcp::tools::fix_helpers::{lookup_project_id, pool_or_err};

/// Recognized identifier conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NamingConvention {
    SnakeCase,
    CamelCase,
    PascalCase,
    ScreamingSnake,
    KebabCase,
    /// Anything not classifiable — single character, mixed punctuation,
    /// numeric-only, etc. Treated as a soft "skip" — never flagged and never
    /// counted toward dominance.
    Other,
}

impl NamingConvention {
    fn as_str(self) -> &'static str {
        match self {
            NamingConvention::SnakeCase => "snake_case",
            NamingConvention::CamelCase => "camelCase",
            NamingConvention::PascalCase => "PascalCase",
            NamingConvention::ScreamingSnake => "SCREAMING_SNAKE",
            NamingConvention::KebabCase => "kebab-case",
            NamingConvention::Other => "other",
        }
    }
}

/// Classify an identifier into one of the canonical conventions.
///
/// Heuristic rules (evaluated in order):
/// - Contains underscore + only uppercase letters/digits → `SCREAMING_SNAKE`.
/// - Contains underscore (any other case) → `snake_case`.
/// - Contains dash → `kebab-case`.
/// - No separators, mixed case, starts uppercase → `PascalCase`.
/// - No separators, mixed case, starts lowercase → `camelCase`.
/// - No separators, all lowercase → `snake_case` (single-word convention).
/// - Anything else (single char, all uppercase no underscore, mixed punct) → `Other`.
pub(crate) fn classify_convention(name: &str) -> NamingConvention {
    if name.is_empty() {
        return NamingConvention::Other;
    }

    let alphabetic_chars: Vec<char> = name.chars().filter(|c| c.is_alphabetic()).collect();
    if alphabetic_chars.is_empty() {
        return NamingConvention::Other;
    }

    let has_underscore = name.contains('_');
    let has_dash = name.contains('-');
    let has_upper = alphabetic_chars.iter().any(|c| c.is_uppercase());
    let has_lower = alphabetic_chars.iter().any(|c| c.is_lowercase());
    let starts_upper = name
        .chars()
        .find(|c| c.is_alphabetic())
        .map(|c| c.is_uppercase())
        .unwrap_or(false);

    if has_underscore && has_upper && !has_lower {
        return NamingConvention::ScreamingSnake;
    }
    if has_underscore {
        return NamingConvention::SnakeCase;
    }
    if has_dash {
        return NamingConvention::KebabCase;
    }
    if has_upper && has_lower {
        return if starts_upper {
            NamingConvention::PascalCase
        } else {
            NamingConvention::CamelCase
        };
    }
    if !has_upper && has_lower {
        // Single lowercase word → snake_case in spirit.
        return NamingConvention::SnakeCase;
    }
    NamingConvention::Other
}

/// Convert an identifier to a target convention. Splits the name into words
/// (via underscore/dash boundaries + camelCase humps), then re-joins per
/// target. `Other` returns the input unchanged.
pub(crate) fn convert_to_convention(name: &str, target: NamingConvention) -> String {
    let words = split_into_words(name);
    if words.is_empty() {
        return name.to_string();
    }
    match target {
        NamingConvention::SnakeCase => words
            .iter()
            .map(|w| w.to_lowercase())
            .collect::<Vec<_>>()
            .join("_"),
        NamingConvention::CamelCase => {
            let mut out = String::new();
            for (i, w) in words.iter().enumerate() {
                if i == 0 {
                    out.push_str(&w.to_lowercase());
                } else {
                    out.push_str(&capitalize(w));
                }
            }
            out
        }
        NamingConvention::PascalCase => words.iter().map(|w| capitalize(w)).collect::<String>(),
        NamingConvention::ScreamingSnake => words
            .iter()
            .map(|w| w.to_uppercase())
            .collect::<Vec<_>>()
            .join("_"),
        NamingConvention::KebabCase => words
            .iter()
            .map(|w| w.to_lowercase())
            .collect::<Vec<_>>()
            .join("-"),
        NamingConvention::Other => name.to_string(),
    }
}

fn split_into_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut prev_was_lower = false;
    for c in name.chars() {
        if c == '_' || c == '-' {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            prev_was_lower = false;
            continue;
        }
        // camelCase / PascalCase boundary: lowercase → uppercase
        if c.is_uppercase() && prev_was_lower && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        current.push(c);
        prev_was_lower = c.is_lowercase();
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}

fn directory_of(relative_path: &str) -> &str {
    relative_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

pub async fn tool_naming_consistency(
    ctx: &SystemContext,
    params: NamingConsistencyParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .naming_consistency_scans
        .fetch_add(1, Ordering::Relaxed);

    let min_dominance = params.min_dominance.unwrap_or(0.7).clamp(0.0, 1.0);
    let limit = params.limit.unwrap_or(50).max(1) as usize;
    let include_fixes = params.include_fixes.unwrap_or(true);

    info!(
        tool = "naming_consistency",
        project = %params.project,
        min_dominance,
        limit,
        include_fixes,
        "MCP tool invoked",
    );

    let project_id = match lookup_project_id(ctx, &params.project).await? {
        Some(id) => id,
        None => {
            return soft_fail_unknown_project(&params, min_dominance, limit);
        }
    };

    let pool = pool_or_err(ctx)?;
    let rows =
        crate::db::queries::get_naming_distribution(pool, project_id, params.language.as_deref())
            .await
            .map_err(|e| {
                McpError::internal_error(format!("get_naming_distribution failed: {}", e), None)
            })?;

    if rows.is_empty() {
        return soft_fail_no_symbols(&params, min_dominance, limit);
    }

    // Group by (directory, kind) and tally conventions.
    let mut groups: HashMap<(String, String), Vec<&crate::db::queries::NamingDistributionRow>> =
        HashMap::new();
    for row in &rows {
        let dir = directory_of(&row.relative_path).to_string();
        groups.entry((dir, row.kind.clone())).or_default().push(row);
    }

    let mut divergences: Vec<serde_json::Value> = Vec::new();
    for ((module_path, kind), members) in &groups {
        let mut tally: HashMap<NamingConvention, u32> = HashMap::new();
        for m in members {
            let conv = classify_convention(&m.symbol_name);
            if matches!(conv, NamingConvention::Other) {
                continue;
            }
            *tally.entry(conv).or_insert(0) += 1;
        }
        let total: u32 = tally.values().sum();
        if total == 0 {
            continue;
        }
        let (dominant, dominant_count) = match tally.iter().max_by_key(|&(_, c)| *c) {
            Some((conv, count)) => (conv, *count),
            None => continue,
        };
        let dominance = dominant_count as f64 / total as f64;
        if dominance < min_dominance {
            continue;
        }

        for m in members {
            let conv = classify_convention(&m.symbol_name);
            if matches!(conv, NamingConvention::Other) || conv == *dominant {
                continue;
            }
            let suggested = convert_to_convention(&m.symbol_name, *dominant);
            let mut entry = json!({
                "symbol": m.symbol_name,
                "file": m.relative_path,
                "line": m.start_line,
                "kind": kind,
                "language": m.language,
                "detected_convention": conv.as_str(),
                "dominant_in_module": dominant.as_str(),
                "module_dominance": dominance,
                "module_path": module_path,
                "module_kind_total": total,
                "recommendation": format!(
                    "Rename `{}` to `{}` to match the dominant {} convention in {} ({:.0}% {} for {}s).",
                    m.symbol_name,
                    suggested,
                    dominant.as_str(),
                    if module_path.is_empty() { "<root>" } else { module_path.as_str() },
                    dominance * 100.0,
                    dominant.as_str(),
                    kind
                ),
            });

            if include_fixes {
                let fix = build_rename_fix(
                    &params.project,
                    &m.relative_path,
                    m.start_line as u32,
                    &m.symbol_name,
                    &suggested,
                    dominance,
                );
                entry["recommended_fix"] = serde_json::to_value(&fix).map_err(|e| {
                    McpError::internal_error(
                        format!("recommended_fix serialization failed: {}", e),
                        None,
                    )
                })?;
            }
            divergences.push(entry);
        }
    }

    // Stable sort: dominance descending, then file path / line.
    divergences.sort_by(|a, b| {
        let da = a["module_dominance"].as_f64().unwrap_or(0.0);
        let db = b["module_dominance"].as_f64().unwrap_or(0.0);
        db.partial_cmp(&da)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let pa = a["file"].as_str().unwrap_or("");
                let pb = b["file"].as_str().unwrap_or("");
                pa.cmp(pb)
            })
            .then_with(|| {
                let la = a["line"].as_i64().unwrap_or(0);
                let lb = b["line"].as_i64().unwrap_or(0);
                la.cmp(&lb)
            })
    });

    let total = divergences.len();
    if divergences.len() > limit {
        divergences.truncate(limit);
    }

    let result = json!({
        "scope": {
            "project": params.project,
            "language": params.language,
        },
        "divergences": divergences,
        "total_divergences": total,
        "parameters": {
            "project": params.project,
            "language": params.language,
            "min_dominance": min_dominance,
            "limit": limit,
            "include_fixes": include_fixes,
        },
        "health": {
            "symbols_present": true,
        },
    });

    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "naming_consistency",
        duration_ms = start.elapsed().as_millis() as u64,
        total_divergences = total,
        returned = divergences.len(),
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

fn build_rename_fix(
    project: &str,
    file: &str,
    line: u32,
    original: &str,
    suggested: &str,
    confidence: f64,
) -> RecommendedFix {
    RecommendedFix {
        action: FixAction::MoveFunction,
        location: LocationRef {
            project: project.to_string(),
            paths: vec![PathRange {
                path: file.to_string(),
                start_line: line,
                end_line: line,
            }],
        },
        target: TargetRef {
            paths: vec![TargetPath {
                path: Some(file.to_string()),
                start_line: Some(line),
                end_line: Some(line),
                suggested_new_path: None,
                suggested_name: Some(suggested.to_string()),
                line_ranges: None,
            }],
        },
        steps: vec![
            format!("Rename symbol `{}` to `{}` at {}:{}.", original, suggested, file, line),
            "Update all call sites referencing the old name (run `pattern_search` or grep for the old identifier)."
                .to_string(),
        ],
        references: Vec::new(),
        confidence: confidence.clamp(0.0, 1.0),
        estimated_effort: EstimatedEffort::Small,
    }
}

fn soft_fail_unknown_project(
    params: &NamingConsistencyParams,
    min_dominance: f64,
    limit: usize,
) -> Result<CallToolResult, McpError> {
    let result = json!({
        "scope": {
            "project": params.project,
            "language": params.language,
        },
        "divergences": [],
        "total_divergences": 0,
        "parameters": {
            "project": params.project,
            "language": params.language,
            "min_dominance": min_dominance,
            "limit": limit,
        },
        "guidance": format!(
            "Project `{}` is not indexed. Run `pgmcp scan` or check `list_projects`.",
            params.project
        ),
        "health": {
            "symbols_present": false,
        },
    });
    let s = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;
    Ok(CallToolResult::success(vec![Content::text(s)]))
}

fn soft_fail_no_symbols(
    params: &NamingConsistencyParams,
    min_dominance: f64,
    limit: usize,
) -> Result<CallToolResult, McpError> {
    let result = json!({
        "scope": {
            "project": params.project,
            "language": params.language,
        },
        "divergences": [],
        "total_divergences": 0,
        "parameters": {
            "project": params.project,
            "language": params.language,
            "min_dominance": min_dominance,
            "limit": limit,
        },
        "guidance": "No symbols extracted for this project yet. Wait for the symbol-extraction \
                     cron to run (default: every 2h, 30 min after Ready), or trigger it manually \
                     via the daemon's heavy-cron path.",
        "health": {
            "symbols_present": false,
        },
    });
    let s = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;
    Ok(CallToolResult::success(vec![Content::text(s)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_snake_case() {
        assert_eq!(classify_convention("foo_bar"), NamingConvention::SnakeCase);
        assert_eq!(classify_convention("hello"), NamingConvention::SnakeCase);
        assert_eq!(
            classify_convention("a_long_name"),
            NamingConvention::SnakeCase
        );
    }

    #[test]
    fn classify_camel_case() {
        assert_eq!(classify_convention("fooBar"), NamingConvention::CamelCase);
        assert_eq!(
            classify_convention("buildHttpClient"),
            NamingConvention::CamelCase
        );
    }

    #[test]
    fn classify_pascal_case() {
        assert_eq!(classify_convention("FooBar"), NamingConvention::PascalCase);
        assert_eq!(
            classify_convention("HttpClient"),
            NamingConvention::PascalCase
        );
    }

    #[test]
    fn classify_screaming_snake() {
        assert_eq!(
            classify_convention("MAX_SIZE"),
            NamingConvention::ScreamingSnake
        );
        assert_eq!(
            classify_convention("API_KEY_V2"),
            NamingConvention::ScreamingSnake
        );
    }

    #[test]
    fn classify_kebab_case() {
        assert_eq!(classify_convention("foo-bar"), NamingConvention::KebabCase);
    }

    #[test]
    fn classify_other_for_edge_cases() {
        assert_eq!(classify_convention(""), NamingConvention::Other);
        assert_eq!(classify_convention("123"), NamingConvention::Other);
        assert_eq!(classify_convention("_"), NamingConvention::Other);
        // Single uppercase letter — no separator, no lowercase. Other.
        assert_eq!(classify_convention("X"), NamingConvention::Other);
    }

    #[test]
    fn convert_camel_to_snake() {
        assert_eq!(
            convert_to_convention("fooBar", NamingConvention::SnakeCase),
            "foo_bar"
        );
        assert_eq!(
            convert_to_convention("buildHttpClient", NamingConvention::SnakeCase),
            "build_http_client"
        );
    }

    #[test]
    fn convert_pascal_to_snake() {
        assert_eq!(
            convert_to_convention("FooBar", NamingConvention::SnakeCase),
            "foo_bar"
        );
    }

    #[test]
    fn convert_snake_to_pascal() {
        assert_eq!(
            convert_to_convention("foo_bar", NamingConvention::PascalCase),
            "FooBar"
        );
    }

    #[test]
    fn convert_snake_to_screaming() {
        assert_eq!(
            convert_to_convention("foo_bar", NamingConvention::ScreamingSnake),
            "FOO_BAR"
        );
    }

    #[test]
    fn convert_to_kebab() {
        assert_eq!(
            convert_to_convention("fooBar", NamingConvention::KebabCase),
            "foo-bar"
        );
    }

    #[test]
    fn directory_of_returns_parent() {
        assert_eq!(directory_of("src/api/handlers.rs"), "src/api");
        assert_eq!(directory_of("foo.rs"), "");
        assert_eq!(directory_of("a/b/c/d.rs"), "a/b/c");
    }
}
