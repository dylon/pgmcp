//! Shared regex-based file-content scanner used by Phase 5/6/9 tools.

#![allow(dead_code)]

use regex::Regex;
use sqlx::PgPool;

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
    let mut q = String::from(
        "SELECT relative_path, language, content
         FROM indexed_files
         WHERE project_id = $1 AND content IS NOT NULL",
    );
    if language_filter.is_some() {
        q.push_str(" AND language = ANY($2::text[])");
    }

    let rows: Vec<(String, String, Option<String>)> = if let Some(langs) = language_filter {
        let v: Vec<String> = langs.iter().map(|s| s.to_string()).collect();
        sqlx::query_as::<_, (String, String, Option<String>)>(&q)
            .bind(project_id)
            .bind(v)
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query_as::<_, (String, String, Option<String>)>(&q)
            .bind(project_id)
            .fetch_all(pool)
            .await?
    };

    let mut hits: Vec<ScanHit> = Vec::new();
    for (path, lang, content) in rows {
        let Some(c) = content else { continue };
        for m in pattern.find_iter(&c) {
            let start = m.start();
            // Compute 1-based line number by counting newlines before m.start()
            let line = c[..start].bytes().filter(|b| *b == b'\n').count() + 1;
            let line_start = c[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let line_end = c[start..]
                .find('\n')
                .map(|i| start + i)
                .unwrap_or_else(|| c.len());
            let snip = &c[line_start..line_end];
            let snip = if snip.len() > 200 { &snip[..200] } else { snip };
            hits.push(ScanHit {
                relative_path: path.clone(),
                language: lang.clone(),
                line: line as u32,
                snippet: snip.trim().to_string(),
            });
            if hits.len() >= limit {
                return Ok(hits);
            }
        }
    }
    Ok(hits)
}
