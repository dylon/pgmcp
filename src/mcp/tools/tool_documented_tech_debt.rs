//! `tool_documented_tech_debt` — unified developer-authored debt surface.
//!
//! Complements `tool_technical_debt_analysis` (composite score) and
//! `tool_panic_paths` (per-function Rust/Python stub-macro count) by
//! surfacing every documented debt marker across the project — comment
//! markers, code-stub macros, deprecation annotations, GitHub issue
//! references — with severity tiers and git-blame attribution.

#![allow(unused_imports)]

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Instant;

use chrono::{DateTime, Utc};
use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::DocumentedTechDebtParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

/// 17 comment markers, severity-classified. Delegates to the shared catalog in
/// `crate::code_analysis::findings` so the `documented_tech_debt` tool and the
/// `findings-promotion` cron never disagree about the marker set or tiers.
fn comment_markers() -> Vec<(&'static str, &'static str)> {
    crate::code_analysis::findings::comment_markers()
}

/// Code-stub macro patterns per language.
struct StubPattern {
    language: &'static str,
    pattern: &'static str,
    label: &'static str,
    severity: &'static str,
}

fn stub_patterns() -> &'static [StubPattern] {
    &[
        StubPattern {
            language: "rust",
            pattern: r#"(?m)\b(?:panic|todo|unimplemented|unreachable)!\s*\(\s*"(?i)(?:not\s*implemented|todo|fixme|unimplemented)"#,
            label: "rust_stub_panic",
            severity: "high",
        },
        StubPattern {
            language: "rust",
            pattern: r"(?m)\b(?:todo|unimplemented)!\s*\(",
            label: "rust_todo_macro",
            severity: "high",
        },
        StubPattern {
            language: "python",
            pattern: r"(?m)\braise\s+NotImplementedError\b",
            label: "python_not_implemented",
            severity: "high",
        },
        StubPattern {
            language: "python",
            pattern: r"(?m)pass\s*#\s*TODO",
            label: "python_pass_todo",
            severity: "medium",
        },
        StubPattern {
            language: "javascript",
            pattern: r#"(?m)throw\s+new\s+(?:Error|TypeError)\s*\(\s*['"](?i)(?:not\s*implemented|todo|unimplemented)"#,
            label: "js_throw_not_implemented",
            severity: "high",
        },
        StubPattern {
            language: "typescript",
            pattern: r#"(?m)throw\s+new\s+(?:Error|TypeError)\s*\(\s*['"](?i)(?:not\s*implemented|todo|unimplemented)"#,
            label: "ts_throw_not_implemented",
            severity: "high",
        },
        StubPattern {
            language: "go",
            pattern: r#"(?m)panic\s*\(\s*['"](?i)(?:todo|not\s*implemented|unimplemented)"#,
            label: "go_panic_todo",
            severity: "high",
        },
        StubPattern {
            language: "java",
            pattern: r"(?m)throw\s+new\s+UnsupportedOperationException\s*\(",
            label: "java_unsupported_op",
            severity: "high",
        },
        StubPattern {
            language: "c",
            pattern: r"(?m)__builtin_unreachable\s*\(\s*\)|assert\s*\(\s*0\s*&&",
            label: "c_unreachable_assert",
            severity: "high",
        },
        StubPattern {
            language: "cpp",
            pattern: r"(?m)__builtin_unreachable\s*\(\s*\)|assert\s*\(\s*0\s*&&",
            label: "cpp_unreachable_assert",
            severity: "high",
        },
        // Clojure stub idiom: `(throw (ex-info "not implemented" ...))` or
        // `(throw (UnsupportedOperationException. ...))`.
        StubPattern {
            language: "clojure",
            pattern: r#"(?im)\(\s*throw\s+\(\s*(?:ex-info\s+["'](?:not\s*implemented|todo|unimplemented)|UnsupportedOperationException)"#,
            label: "clojure_throw_not_implemented",
            severity: "high",
        },
        StubPattern {
            language: "clojurescript",
            pattern: r#"(?im)\(\s*throw\s+\(\s*(?:ex-info\s+["'](?:not\s*implemented|todo|unimplemented)|js/Error\.\s*["'](?:not\s*implemented|todo|unimplemented))"#,
            label: "cljs_throw_not_implemented",
            severity: "high",
        },
    ]
}

/// Deprecation-attribute patterns per language.
struct DeprecatedPattern {
    language: &'static str,
    pattern: &'static str,
}

fn deprecation_patterns() -> &'static [DeprecatedPattern] {
    &[
        DeprecatedPattern {
            language: "rust",
            pattern: r"(?m)#\[deprecated[^\]]*\]",
        },
        DeprecatedPattern {
            language: "java",
            pattern: r"(?m)@Deprecated\b",
        },
        DeprecatedPattern {
            language: "javascript",
            pattern: r"(?m)@deprecated\b",
        },
        DeprecatedPattern {
            language: "typescript",
            pattern: r"(?m)@deprecated\b",
        },
        DeprecatedPattern {
            language: "python",
            pattern: r"(?m)warnings\.warn\([^)]*DeprecationWarning|@deprecated\b",
        },
        // Clojure marks deprecation via `^:deprecated` metadata or a
        // `{:deprecated "..."}` attr-map on a var.
        DeprecatedPattern {
            language: "clojure",
            pattern: r#"(?m)\^:deprecated\b|:deprecated\s+(?:true|"[^"]*")"#,
        },
        DeprecatedPattern {
            language: "clojurescript",
            pattern: r#"(?m)\^:deprecated\b|:deprecated\s+(?:true|"[^"]*")"#,
        },
    ]
}

/// Issue-ref regex: matches `#1234` and `owner/repo#42` after a marker.
fn issue_ref_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([\w./-]+)?#(\d+)").expect("issue ref regex"))
}

/// One blame row used for attribution.
#[derive(sqlx::FromRow, Debug, Clone)]
struct BlameRow {
    start_line: i32,
    end_line: i32,
    blame_author: Option<String>,
    blame_date: Option<DateTime<Utc>>,
}

/// Build a per-line lookup of `(author, date)` from the file's chunk rows.
/// Falls back to None for lines not covered by any chunk.
fn blame_at(blame: &[BlameRow], line: u32) -> (Option<String>, Option<DateTime<Utc>>) {
    let mut best: Option<&BlameRow> = None;
    let mut best_span = u32::MAX;
    for row in blame {
        let s = row.start_line as u32;
        let e = row.end_line as u32;
        if s <= line && line <= e {
            let span = e.saturating_sub(s);
            if span < best_span {
                best_span = span;
                best = Some(row);
            }
        }
    }
    match best {
        Some(b) => (b.blame_author.clone(), b.blame_date),
        None => (None, None),
    }
}

#[derive(Debug, Clone)]
struct Finding {
    file: String,
    language: String,
    line: u32,
    kind: String,
    severity: &'static str,
    category: &'static str,
    snippet: String,
    issue_refs: Vec<String>,
    author: Option<String>,
    age_days: Option<i64>,
}

pub async fn tool_documented_tech_debt(
    ctx: &SystemContext,
    params: DocumentedTechDebtParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .documented_debt_scans
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let project = params.project.trim();
    let project_id = project_id_or_err(ctx, project).await?;

    let limit = params.limit.unwrap_or(100).clamp(1, 1000) as usize;
    let format = params
        .format
        .as_deref()
        .map(str::trim)
        .filter(|format| !format.is_empty())
        .unwrap_or("summary");
    if !matches!(format, "summary" | "full") {
        return Err(McpError::invalid_params(
            format!("unknown format '{format}'; expected summary | full"),
            None,
        ));
    }
    let category_filter = params
        .category
        .as_deref()
        .map(str::trim)
        .filter(|category| !category.is_empty())
        .unwrap_or("all");
    if !matches!(
        category_filter,
        "all" | "comments" | "stub_macros" | "deprecated"
    ) {
        return Err(McpError::invalid_params(
            format!(
                "unknown category '{category_filter}'; expected all | comments | stub_macros | deprecated"
            ),
            None,
        ));
    }
    let kind_filter = params
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .map(str::to_uppercase);
    let severity_filter = params
        .severity
        .as_deref()
        .map(str::trim)
        .filter(|severity| !severity.is_empty())
        .map(str::to_lowercase);
    if let Some(severity) = severity_filter.as_deref()
        && !matches!(severity, "high" | "medium" | "low")
    {
        return Err(McpError::invalid_params(
            format!("unknown severity '{severity}'; expected high | medium | low"),
            None,
        ));
    }
    let min_age_days = match params.min_age_days {
        Some(days) if days < 0 => {
            return Err(McpError::invalid_params(
                "min_age_days must be non-negative",
                None,
            ));
        }
        other => other,
    };
    let language_filter = params
        .language
        .as_deref()
        .map(str::trim)
        .filter(|language| !language.is_empty());

    // Canonical defaults when the caller omits `exclude_paths`: skip the
    // curated pattern catalog and the marker-detector's own regex test
    // fixtures, since those are seed prose and self-test inputs rather
    // than real debt. `Some(vec![])` opts out entirely.
    const DEFAULT_EXCLUDE_PATHS: &[&str] = &[
        "src/patterns/**",
        "src/mcp/tools/tool_technical_debt_analysis.rs",
        "src/mcp/tools/tool_documented_tech_debt.rs",
    ];
    let exclude_glob_patterns: Vec<String> = params
        .exclude_paths
        .clone()
        .unwrap_or_else(|| {
            DEFAULT_EXCLUDE_PATHS
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        })
        .into_iter()
        .map(|pat| pat.trim().to_string())
        .filter(|pat| !pat.is_empty())
        .collect();
    let exclude_matcher: Option<globset::GlobSet> =
        if exclude_glob_patterns.is_empty() {
            None
        } else {
            let mut builder = globset::GlobSetBuilder::new();
            for pat in &exclude_glob_patterns {
                match globset::Glob::new(pat) {
                    Ok(g) => {
                        builder.add(g);
                    }
                    Err(e) => {
                        return Err(McpError::invalid_params(
                            format!("Invalid glob pattern {pat:?}: {e}"),
                            None,
                        ));
                    }
                }
            }
            Some(builder.build().map_err(|e| {
                McpError::internal_error(format!("Glob set build failed: {e}"), None)
            })?)
        };

    debug!(
        tool = "documented_tech_debt",
        project, limit, format, category_filter, "MCP tool invoked"
    );

    // Fetch project files.
    let mut files: Vec<(i64, String, String, Option<String>)> =
        sqlx::query_as::<_, (i64, String, String, Option<String>)>(
            "SELECT f.id, f.relative_path, f.language, f.content
             FROM indexed_files f
             WHERE f.project_id = $1
               AND f.content IS NOT NULL
               AND ($2::text IS NULL OR f.language = $2)",
        )
        .bind(project_id)
        .bind(language_filter)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("File query failed: {}", e), None))?;

    // Apply the glob-based path exclusions client-side (the glob library
    // gives us full ** / ? / [class] semantics that are awkward in SQL).
    if let Some(matcher) = &exclude_matcher {
        files.retain(|(_, relative_path, _, _)| !matcher.is_match(relative_path));
    }

    if files.is_empty() {
        // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
        // for the project. Universal enrichment — every tool benefits from
        // surfacing the effect distribution alongside its primary output.
        // Gracefully degrades to empty when the project lookup or
        // shadow-ASR data isn't populated.
        let effect_breakdown: Vec<serde_json::Value> =
            crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                .collect();

        return json_result(&json!({
        "effect_breakdown": effect_breakdown,
            "project": project,
            "total_markers": 0,
            "findings": [],
            "guidance": "No files matched the filter (project not indexed, no content, or language filter excludes all).",
        }));
    }

    let file_ids: Vec<i64> = files.iter().map(|f| f.0).collect();
    let blame_rows: Vec<(i64, BlameRow)> =
        sqlx::query_as::<_, (i64, i32, i32, Option<String>, Option<DateTime<Utc>>)>(
            "SELECT file_id, start_line, end_line, blame_author, blame_date
         FROM file_chunks
         WHERE file_id = ANY($1::bigint[])",
        )
        .bind(&file_ids)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Blame query failed: {}", e), None))?
        .into_iter()
        .map(|(fid, sl, el, author, date)| {
            (
                fid,
                BlameRow {
                    start_line: sl,
                    end_line: el,
                    blame_author: author,
                    blame_date: date,
                },
            )
        })
        .collect();
    let mut blame_by_file: HashMap<i64, Vec<BlameRow>> = HashMap::new();
    for (fid, row) in blame_rows {
        blame_by_file.entry(fid).or_default().push(row);
    }

    // Compile comment regex once (case-insensitive, with optional `(issue-ref)`).
    let markers = comment_markers();
    let allowlist: HashMap<String, &'static str> =
        markers.iter().map(|(t, s)| (t.to_string(), *s)).collect();
    let alt = markers
        .iter()
        .map(|(t, _)| *t)
        .collect::<Vec<_>>()
        .join("|");
    // Match the marker after `//`, `/*`, `#`, or `--` (SQL/Lua). Capture
    // the kind and the trailing text on the same line.
    let comment_re = Regex::new(&format!(
        r"(?im)(?:(?://|/\*|#|--)\s*|^|\s)({})(?:\([^)]*\))?:?\s*([^\n]*)",
        alt
    ))
    .expect("comment marker regex");

    let stubs = stub_patterns();
    let stub_compiled: Vec<(StubPatternCompiled, &'static StubPattern)> = stubs
        .iter()
        .map(|sp| {
            (
                StubPatternCompiled {
                    re: Regex::new(sp.pattern).expect("stub regex"),
                },
                sp,
            )
        })
        .collect();

    let depr = deprecation_patterns();
    let depr_compiled: Vec<(StubPatternCompiled, &'static DeprecatedPattern)> = depr
        .iter()
        .map(|dp| {
            (
                StubPatternCompiled {
                    re: Regex::new(dp.pattern).expect("deprecated regex"),
                },
                dp,
            )
        })
        .collect();

    let now = Utc::now();
    let mut findings: Vec<Finding> = Vec::new();

    let scan_comments = matches!(category_filter, "all" | "comments");
    let scan_stubs = matches!(category_filter, "all" | "stub_macros");
    let scan_depr = matches!(category_filter, "all" | "deprecated");

    for (fid, path, lang, content_opt) in &files {
        let Some(content) = content_opt else { continue };
        let blame_rows_for_file = blame_by_file.get(fid).cloned().unwrap_or_default();

        // Comment markers.
        if scan_comments {
            for cap in comment_re.captures_iter(content) {
                let kind_match = match cap.get(1) {
                    Some(m) => m,
                    None => continue,
                };
                let kind_upper = kind_match.as_str().to_uppercase();
                let severity = match allowlist.get(&kind_upper) {
                    Some(s) => *s,
                    None => continue,
                };
                let line_no = content[..kind_match.start()]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count() as u32
                    + 1;
                let trail = cap.get(2).map(|m| m.as_str()).unwrap_or("");
                let issue_refs: Vec<String> = issue_ref_re()
                    .captures_iter(trail)
                    .map(|c| {
                        let owner = c.get(1).map(|m| m.as_str()).unwrap_or("");
                        let num = c.get(2).map(|m| m.as_str()).unwrap_or("");
                        if owner.is_empty() {
                            format!("#{}", num)
                        } else {
                            format!("{}#{}", owner.trim_end_matches('/'), num)
                        }
                    })
                    .collect();
                let line_start = content[..kind_match.start()]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let line_end = content[kind_match.start()..]
                    .find('\n')
                    .map(|i| kind_match.start() + i)
                    .unwrap_or_else(|| content.len());
                let snippet = content[line_start..line_end].trim().to_string();
                let (author, date) = blame_at(&blame_rows_for_file, line_no);
                let age_days = date.map(|d| (now - d).num_days());
                findings.push(Finding {
                    file: path.clone(),
                    language: lang.clone(),
                    line: line_no,
                    kind: kind_upper,
                    severity,
                    category: "comment",
                    snippet: truncate(&snippet, 200),
                    issue_refs,
                    author,
                    age_days,
                });
            }
        }

        // Stub macros.
        if scan_stubs {
            for (compiled, sp) in &stub_compiled {
                if sp.language != lang {
                    continue;
                }
                for m in compiled.re.find_iter(content) {
                    let line_no =
                        content[..m.start()].bytes().filter(|b| *b == b'\n').count() as u32 + 1;
                    let line_start = content[..m.start()].rfind('\n').map(|i| i + 1).unwrap_or(0);
                    let line_end = content[m.start()..]
                        .find('\n')
                        .map(|i| m.start() + i)
                        .unwrap_or_else(|| content.len());
                    let snippet = content[line_start..line_end].trim().to_string();
                    let (author, date) = blame_at(&blame_rows_for_file, line_no);
                    let age_days = date.map(|d| (now - d).num_days());
                    findings.push(Finding {
                        file: path.clone(),
                        language: lang.clone(),
                        line: line_no,
                        kind: sp.label.to_uppercase(),
                        severity: sp.severity,
                        category: "stub_macro",
                        snippet: truncate(&snippet, 200),
                        issue_refs: Vec::new(),
                        author,
                        age_days,
                    });
                }
            }
        }

        // Deprecation annotations.
        if scan_depr {
            for (compiled, dp) in &depr_compiled {
                if dp.language != lang {
                    continue;
                }
                for m in compiled.re.find_iter(content) {
                    let line_no =
                        content[..m.start()].bytes().filter(|b| *b == b'\n').count() as u32 + 1;
                    let line_start = content[..m.start()].rfind('\n').map(|i| i + 1).unwrap_or(0);
                    let line_end = content[m.start()..]
                        .find('\n')
                        .map(|i| m.start() + i)
                        .unwrap_or_else(|| content.len());
                    let snippet = content[line_start..line_end].trim().to_string();
                    let (author, date) = blame_at(&blame_rows_for_file, line_no);
                    let age_days = date.map(|d| (now - d).num_days());
                    findings.push(Finding {
                        file: path.clone(),
                        language: lang.clone(),
                        line: line_no,
                        kind: "DEPRECATION_ATTR".to_string(),
                        severity: "medium",
                        category: "deprecated",
                        snippet: truncate(&snippet, 200),
                        issue_refs: Vec::new(),
                        author,
                        age_days,
                    });
                }
            }
        }
    }

    // Apply post-filters.
    findings.retain(|f| {
        if let Some(ref k) = kind_filter
            && !f.kind.eq_ignore_ascii_case(k)
        {
            return false;
        }
        if let Some(ref sev) = severity_filter
            && !f.severity.eq_ignore_ascii_case(sev)
        {
            return false;
        }
        if let Some(min) = min_age_days {
            match f.age_days {
                Some(d) if d >= min as i64 => {}
                _ => return false,
            }
        }
        true
    });

    // Aggregate stats.
    let total_markers = findings.len();
    let mut by_kind: HashMap<String, usize> = HashMap::new();
    let mut by_severity: HashMap<&'static str, usize> = HashMap::new();
    let mut by_category: HashMap<&'static str, usize> = HashMap::new();
    let mut by_language: HashMap<String, usize> = HashMap::new();
    let mut oldest_marker_days: Option<i64> = None;
    for f in &findings {
        *by_kind.entry(f.kind.clone()).or_insert(0) += 1;
        *by_severity.entry(f.severity).or_insert(0) += 1;
        *by_category.entry(f.category).or_insert(0) += 1;
        *by_language.entry(f.language.clone()).or_insert(0) += 1;
        if let Some(d) = f.age_days {
            oldest_marker_days = Some(oldest_marker_days.map(|o| o.max(d)).unwrap_or(d));
        }
    }

    let summary = json!({
        "project": project,
        "filters": {
            "format": format,
            "category": category_filter,
            "kind": kind_filter,
            "severity": severity_filter,
            "min_age_days": min_age_days,
            "language": language_filter,
            "limit": limit,
        },
        "total_markers": total_markers,
        "by_kind": by_kind,
        "by_severity": by_severity,
        "by_category": by_category,
        "by_language": by_language,
        "oldest_marker_days": oldest_marker_days,
    });

    let result = if format == "full" {
        // Sort by severity (high > medium > low), then age descending.
        let sev_order = |s: &str| match s {
            "high" => 0,
            "medium" => 1,
            _ => 2,
        };
        findings.sort_by(|a, b| {
            sev_order(a.severity)
                .cmp(&sev_order(b.severity))
                .then_with(|| b.age_days.unwrap_or(-1).cmp(&a.age_days.unwrap_or(-1)))
        });
        findings.truncate(limit);
        let arr: Vec<_> = findings
            .iter()
            .map(|f| {
                json!({
                    "file": f.file,
                    "language": f.language,
                    "line": f.line,
                    "kind": f.kind,
                    "severity": f.severity,
                    "category": f.category,
                    "snippet": f.snippet,
                    "issue_refs": f.issue_refs,
                    "author": f.author,
                    "age_days": f.age_days,
                })
            })
            .collect();
        json!({
            "summary": summary,
            "findings": arr,
            "guidance": guidance(),
        })
    } else {
        json!({
            "summary": summary,
            "guidance": guidance(),
        })
    };

    debug!(
        tool = "documented_tech_debt",
        total = total_markers,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "tool complete"
    );

    json_result(&result)
}

fn guidance() -> &'static str {
    "Severity tiers: high = FIXME/BUG/HACK/KLUDGE/WTF/XXX + stub macros; medium = TODO/TBD/WORKAROUND/REVIEW/SMELL/REFACTOR/DEPRECATED + deprecation attrs; low = NOTE/OPTIMIZE/TEMP/DEBUG. Author and age_days come from file_chunks.blame_*; null when the project has no git history indexed. Issue refs extract `#1234` and `owner/repo#42` from the line tail."
}

fn truncate(s: &str, max: usize) -> String {
    crate::code_analysis::findings::truncate(s, max)
}

/// Wrapper holding a compiled regex; needed so we don't repeatedly compile in
/// the inner scan loops.
struct StubPatternCompiled {
    re: Regex,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_has_17_markers() {
        let markers = comment_markers();
        assert_eq!(markers.len(), 17, "marker count drifted");
    }

    #[test]
    fn severity_partition_is_complete() {
        let markers = comment_markers();
        let high: Vec<_> = markers.iter().filter(|(_, s)| *s == "high").collect();
        let medium: Vec<_> = markers.iter().filter(|(_, s)| *s == "medium").collect();
        let low: Vec<_> = markers.iter().filter(|(_, s)| *s == "low").collect();
        assert_eq!(high.len() + medium.len() + low.len(), 17);
        assert_eq!(high.len(), 6);
        assert_eq!(medium.len(), 7);
        assert_eq!(low.len(), 4);
    }

    #[test]
    fn debug_is_in_low_tier() {
        let markers = comment_markers();
        let debug = markers.iter().find(|(t, _)| *t == "DEBUG");
        assert!(debug.is_some());
        assert_eq!(debug.expect("DEBUG marker").1, "low");
    }

    #[test]
    fn issue_ref_extraction_handles_simple_and_qualified() {
        let re = issue_ref_re();
        let line = "TODO(#1234) and also owner/repo#42";
        let caps: Vec<_> = re.captures_iter(line).collect();
        assert_eq!(caps.len(), 2);
    }

    #[test]
    fn blame_at_picks_narrowest_chunk() {
        let blame = vec![
            BlameRow {
                start_line: 1,
                end_line: 100,
                blame_author: Some("alice".into()),
                blame_date: Some(Utc::now()),
            },
            BlameRow {
                start_line: 40,
                end_line: 60,
                blame_author: Some("bob".into()),
                blame_date: Some(Utc::now()),
            },
        ];
        let (author, _) = blame_at(&blame, 50);
        assert_eq!(author.as_deref(), Some("bob"));
    }

    #[test]
    fn blame_at_returns_none_outside_any_chunk() {
        let blame = vec![BlameRow {
            start_line: 10,
            end_line: 20,
            blame_author: Some("alice".into()),
            blame_date: None,
        }];
        let (author, _) = blame_at(&blame, 5);
        assert!(author.is_none());
    }

    /// Every stub/deprecation pattern must compile; the inner loops use
    /// `.expect()` so a malformed pattern would panic in production.
    #[test]
    fn all_stub_and_deprecation_patterns_compile() {
        for sp in stub_patterns() {
            Regex::new(sp.pattern).unwrap_or_else(|e| panic!("stub pattern {} bad: {e}", sp.label));
        }
        for dp in deprecation_patterns() {
            Regex::new(dp.pattern)
                .unwrap_or_else(|e| panic!("deprecation pattern for {} bad: {e}", dp.language));
        }
    }

    #[test]
    fn clojure_stub_pattern_matches_throw_not_implemented() {
        let sp = stub_patterns()
            .iter()
            .find(|sp| sp.language == "clojure")
            .expect("clojure stub pattern present");
        let re = Regex::new(sp.pattern).expect("compile");
        assert!(re.is_match("(defn foo [] (throw (ex-info \"not implemented\" {})))"));
        assert!(re.is_match("(throw (UnsupportedOperationException. \"x\"))"));
        assert!(!re.is_match("(defn foo [] (println \"ok\"))"));
    }

    #[test]
    fn clojure_deprecation_pattern_matches_metadata() {
        let dp = deprecation_patterns()
            .iter()
            .find(|dp| dp.language == "clojure")
            .expect("clojure deprecation pattern present");
        let re = Regex::new(dp.pattern).expect("compile");
        assert!(re.is_match("(defn ^:deprecated old-api [] nil)"));
        assert!(re.is_match("(def x {:deprecated \"use y\"})"));
        assert!(!re.is_match("(defn current-api [] nil)"));
    }
}
