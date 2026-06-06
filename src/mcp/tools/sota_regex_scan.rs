//! Shared regex-based file-content scanner used by Phase 5/6/9 tools.

#![allow(dead_code)]

use regex::Regex;
use sqlx::PgPool;

use futures::TryStreamExt;

/// One match found by `scan_files_for_pattern`.
#[derive(Debug, Clone)]
pub struct ScanHit {
    pub relative_path: String,
    pub language: String,
    pub line: u32,
    pub snippet: String,
}

/// Stream over all files in a project; for each file's content, run `pattern`
/// and collect one hit per match (line + snippet trimmed to 200 chars).
///
/// `language_filter` restricts which languages to scan (None = all).
pub async fn scan_files_for_pattern(
    pool: &PgPool,
    project_id: i32,
    pattern: &Regex,
    language_filter: Option<&[&str]>,
    limit: usize,
) -> Result<Vec<ScanHit>, sqlx::Error> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let mut q = String::from(
        "SELECT relative_path, language, content
         FROM indexed_files
         WHERE project_id = $1 AND content IS NOT NULL",
    );
    if language_filter.is_some() {
        q.push_str(" AND language = ANY($2::text[])");
    }
    q.push_str(" ORDER BY id");

    let mut hits: Vec<ScanHit> = Vec::new();
    if let Some(langs) = language_filter {
        let v: Vec<String> = langs.iter().map(|s| s.to_string()).collect();
        let mut rows = sqlx::query_as::<_, (String, String, Option<String>)>(&q)
            .bind(project_id)
            .bind(v)
            .fetch(pool);
        while let Some((path, lang, content)) = rows.try_next().await? {
            push_pattern_hits(&mut hits, path, lang, content, pattern, limit);
            if hits.len() >= limit {
                return Ok(hits);
            }
        }
    } else {
        let mut rows = sqlx::query_as::<_, (String, String, Option<String>)>(&q)
            .bind(project_id)
            .fetch(pool);
        while let Some((path, lang, content)) = rows.try_next().await? {
            push_pattern_hits(&mut hits, path, lang, content, pattern, limit);
            if hits.len() >= limit {
                return Ok(hits);
            }
        }
    }
    Ok(hits)
}

fn push_pattern_hits(
    hits: &mut Vec<ScanHit>,
    path: String,
    lang: String,
    content: Option<String>,
    pattern: &Regex,
    limit: usize,
) {
    let Some(c) = content else { return };
    for m in pattern.find_iter(&c) {
        let start = m.start();
        // Compute 1-based line number by counting newlines before m.start().
        let line = c[..start].bytes().filter(|b| *b == b'\n').count() + 1;
        let line_start = c[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = c[start..]
            .find('\n')
            .map(|i| start + i)
            .unwrap_or_else(|| c.len());
        let snip = c[line_start..line_end].trim();
        let snippet: String = snip.chars().take(200).collect();
        hits.push(ScanHit {
            relative_path: path.clone(),
            language: lang.clone(),
            line: line as u32,
            snippet,
        });
        if hits.len() >= limit {
            return;
        }
    }
}
