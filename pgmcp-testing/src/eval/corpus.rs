//! DB-backed query generation (strategy **B**) and the M1 leakage-controlled
//! test corpus.
//!
//! [`collect_docstring_candidates`] pulls real chunks that begin with a
//! doc-comment from a live corpus; [`seed_test_corpus`] builds a fresh,
//! internally-consistent corpus in a *separate* pool where each target chunk is
//! re-embedded **with its doc-comment removed** (the M1 control — the stored
//! vector never saw the query), surrounded by distractor chunks embedded in
//! full. Querying that corpus with the doc-comment is then a genuine retrieval
//! task, not an echo.
//!
//! All embedding goes through one [`EmbeddingBackend`] so the target and
//! distractor vectors share a precision/ model — internal cosine comparisons
//! are exact regardless of whether the backend is GPU-BF16 or CPU-F32.

use std::collections::HashSet;
use std::sync::Arc;

use pgmcp::embed::EmbeddingBackend;
use sqlx::PgPool;
use sqlx::Row;

use crate::eval::docstring::{extract_leading_docstring, first_paragraph, redact_identifiers};
use crate::eval::query::{EvalQuery, GoldTarget, QueryStrategy};

/// A doc-comment-bearing chunk pulled from a live corpus.
#[derive(Debug, Clone)]
pub struct DocstringCandidate {
    pub relative_path: String,
    pub language: String,
    pub start_line: i64,
    pub end_line: i64,
    /// The extracted natural-language doc-comment (the query source).
    pub doc_text: String,
    /// The chunk body with the leading doc-comment removed (re-embedded for M1).
    pub body_without_doc: String,
}

/// Pull up to `max_candidates` Rust chunks from `project` that begin with a
/// doc-comment and have code after it. Deterministic order (by path, line) for
/// reproducibility. A `doc_min_chars`/`doc_max_chars` window keeps the query a
/// usable natural-language sentence rather than a one-word stub or a giant
/// `# Examples` block.
pub async fn collect_docstring_candidates(
    pool: &PgPool,
    project: &str,
    max_candidates: usize,
) -> Result<Vec<DocstringCandidate>, sqlx::Error> {
    // Fetch a generous superset; Rust-side extraction + length filtering whittle
    // it down to `max_candidates`.
    let fetch = (max_candidates * 4).max(64) as i64;
    let rows = sqlx::query(
        "SELECT f.relative_path, f.language, c.start_line, c.end_line, c.content \
         FROM file_chunks c \
         JOIN indexed_files f ON f.id = c.file_id \
         JOIN projects p ON p.id = f.project_id \
         WHERE p.name = $1 AND f.language = 'rust' \
           AND c.content ~ '^[[:space:]]*//[/!]' \
         ORDER BY f.relative_path, c.start_line \
         LIMIT $2",
    )
    .bind(project)
    .bind(fetch)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(max_candidates);
    for row in rows {
        if out.len() >= max_candidates {
            break;
        }
        let relative_path: String = row.get("relative_path");
        let language: String = row.get("language");
        let start_line: i32 = row.get("start_line");
        let end_line: i32 = row.get("end_line");
        let content: String = row.get("content");

        let Some(ext) = extract_leading_docstring(&content, &language) else {
            continue;
        };
        let query = first_paragraph(&ext.doc_text);
        // Usable-query window: a real sentence, not a stub or a giant block.
        if query.chars().count() < 25 || query.chars().count() > 400 {
            continue;
        }
        // The body must retain meaningful code after doc removal.
        if ext.body_without_doc.trim().chars().count() < 40 {
            continue;
        }
        out.push(DocstringCandidate {
            relative_path,
            language,
            start_line: start_line as i64,
            end_line: end_line as i64,
            doc_text: query, // store the first-paragraph query
            body_without_doc: ext.body_without_doc,
        });
    }
    Ok(out)
}

/// Sample up to `n` distractor chunk contents from `project`, excluding any
/// chunk whose path is in `exclude_paths`. Deterministic order.
pub async fn sample_distractor_texts(
    pool: &PgPool,
    project: &str,
    exclude_paths: &HashSet<String>,
    n: usize,
) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT f.relative_path, c.content \
         FROM file_chunks c \
         JOIN indexed_files f ON f.id = c.file_id \
         JOIN projects p ON p.id = f.project_id \
         WHERE p.name = $1 AND c.content IS NOT NULL AND length(c.content) > 40 \
         ORDER BY f.relative_path, c.start_line \
         LIMIT $2",
    )
    .bind(project)
    .bind((n * 2).max(64) as i64)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(n);
    for row in rows {
        if out.len() >= n {
            break;
        }
        let path: String = row.get("relative_path");
        if exclude_paths.contains(&path) {
            continue;
        }
        let content: String = row.get("content");
        out.push(content);
    }
    Ok(out)
}

/// Collect the identifier names (function / type / etc.) declared in a Rust
/// source body, for the M3 redaction variant. Best-effort via pgmcp's symbol
/// extractor; an empty set just means no redaction happens.
fn rust_identifiers(body: &str) -> HashSet<String> {
    use pgmcp::parsing::registry::LanguageRegistry;
    let mut idents = HashSet::new();
    if let Some(backend) = LanguageRegistry::for_language("rust") {
        for sym in backend.extract_symbols(body) {
            // Split paths like `Foo::bar` into component identifiers.
            for part in sym.name.split(|c: char| !c.is_alphanumeric() && c != '_') {
                if part.len() >= 3 {
                    idents.insert(part.to_string());
                }
            }
        }
    }
    idents
}

/// Insert a project, returning its id.
async fn insert_project(pool: &PgPool, name: &str) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) \
         ON CONFLICT (path) DO UPDATE SET name = EXCLUDED.name RETURNING id",
    )
    .bind("/m1-eval")
    .bind(format!("/m1-eval/{name}"))
    .bind(name)
    .fetch_one(pool)
    .await
}

/// Insert a file, returning its id.
async fn insert_file(pool: &PgPool, project_id: i32, rel: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO indexed_files \
            (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, 'rust', 64, 'x', 0, 1, NOW()) RETURNING id",
    )
    .bind(project_id)
    .bind(format!("/m1-eval/{rel}"))
    .bind(rel)
    .fetch_one(pool)
    .await
}

/// Insert a chunk with a precomputed embedding.
async fn insert_chunk(
    pool: &PgPool,
    file_id: i64,
    idx: i32,
    content: &str,
    start_line: i32,
    end_line: i32,
    embedding: &[f32],
) -> Result<(), sqlx::Error> {
    let v = pgvector::Vector::from(embedding.to_vec());
    sqlx::query(
        "INSERT INTO file_chunks \
            (file_id, chunk_index, content, start_line, end_line, embedding_v2, embedding_signature) \
         VALUES ($1, $2, $3, $4, $5, $6, 'bge-m3-v1')",
    )
    .bind(file_id)
    .bind(idx)
    .bind(content)
    .bind(start_line)
    .bind(end_line)
    .bind(v)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Build the M1 leakage-controlled corpus in `test_pool`: each candidate's
/// body (doc removed) is re-embedded as the target chunk, distractors are
/// embedded in full, and the returned [`EvalQuery`]s query with the doc-comment
/// and point their gold at the target's synthetic path. `redact` enables the M3
/// variant (identifiers stripped from both query and body before embedding).
///
/// Returns the query set; the test corpus is left populated in `test_pool` (the
/// caller scopes searches to project `"m1-eval"`).
pub async fn seed_test_corpus(
    test_pool: &PgPool,
    embedder: &Arc<dyn EmbeddingBackend>,
    candidates: &[DocstringCandidate],
    distractors: &[String],
    redact: bool,
) -> Result<Vec<EvalQuery>, String> {
    let project_id = insert_project(test_pool, "m1-eval")
        .await
        .map_err(|e| format!("insert project: {e}"))?;

    // Prepare target query/body text (with optional redaction).
    let mut target_query: Vec<String> = Vec::with_capacity(candidates.len());
    let mut target_body: Vec<String> = Vec::with_capacity(candidates.len());
    for c in candidates {
        if redact {
            let idents = rust_identifiers(&c.body_without_doc);
            target_query.push(redact_identifiers(&c.doc_text, &idents));
            target_body.push(redact_identifiers(&c.body_without_doc, &idents));
        } else {
            target_query.push(c.doc_text.clone());
            target_body.push(c.body_without_doc.clone());
        }
    }

    // Embed all bodies + distractors on the single backend (internal consistency).
    let body_refs: Vec<&str> = target_body.iter().map(|s| s.as_str()).collect();
    let body_vecs = embed_all(embedder, &body_refs).await?;
    let distractor_refs: Vec<&str> = distractors.iter().map(|s| s.as_str()).collect();
    let distractor_vecs = embed_all(embedder, &distractor_refs).await?;

    // Insert targets.
    let mut queries = Vec::with_capacity(candidates.len());
    for (i, (c, emb)) in candidates.iter().zip(&body_vecs).enumerate() {
        let rel = format!("t{i:04}.rs");
        let file_id = insert_file(test_pool, project_id, &rel)
            .await
            .map_err(|e| format!("insert target file: {e}"))?;
        insert_chunk(
            test_pool,
            file_id,
            0,
            &target_body[i],
            1,
            (c.end_line - c.start_line + 1).max(1) as i32,
            emb,
        )
        .await
        .map_err(|e| format!("insert target chunk: {e}"))?;

        queries.push(EvalQuery {
            id: format!("m1_{i:04}"),
            strategy: if redact {
                QueryStrategy::DocstringRedacted
            } else {
                QueryStrategy::Docstring
            },
            query: target_query[i].clone(),
            project: Some("m1-eval".to_string()),
            gold: vec![GoldTarget {
                path: rel,
                project: "m1-eval".to_string(),
                start_line: None,
                end_line: None,
                relevance: 1.0,
            }],
            notes: Some(format!("origin: {}:{}", c.relative_path, c.start_line)),
        });
    }

    // Insert distractors.
    for (j, (text, emb)) in distractors.iter().zip(&distractor_vecs).enumerate() {
        let rel = format!("d{j:05}.rs");
        let file_id = insert_file(test_pool, project_id, &rel)
            .await
            .map_err(|e| format!("insert distractor file: {e}"))?;
        insert_chunk(test_pool, file_id, 0, text, 1, 1, emb)
            .await
            .map_err(|e| format!("insert distractor chunk: {e}"))?;
    }

    Ok(queries)
}

/// Embed a list of texts in bounded sub-batches (the backend sub-batches
/// internally, but we chunk here too to bound peak memory on large lists).
async fn embed_all(
    embedder: &Arc<dyn EmbeddingBackend>,
    texts: &[&str],
) -> Result<Vec<Vec<f32>>, String> {
    const BATCH: usize = 64;
    let mut out = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(BATCH) {
        let v = embedder
            .embed_batch(chunk)
            .await
            .map_err(|e| format!("embed_batch: {e}"))?;
        out.extend(v);
    }
    Ok(out)
}

// ============================================================================
// Strategy B-realism (M2) — token-position hold-out on the LIVE corpus
// ============================================================================

/// A chunk whose tokenized length exceeds the embedding window. The `query` is
/// drawn from the text *beyond* the window (tokens > `window`) — content the
/// stored embedding never encoded (BGE-M3 truncates at `max_length`), yet the
/// full text lives in the lexical `content_tsv`. Querying the live corpus with
/// it is therefore (a) leak-free by construction against the full 644k-chunk
/// distractor set and (b) a direct measurement of truncation cost: semantic
/// retrieval lacks this text, lexical retrieval has it.
#[derive(Debug, Clone)]
pub struct HoldoutCandidate {
    pub relative_path: String,
    pub start_line: i64,
    pub end_line: i64,
    /// NL/text snippet decoded from the chunk's tokens beyond `window`.
    pub query: String,
}

/// Build a query snippet from the decoded tail: collapse whitespace, take up to
/// `max_chars` cut at a word boundary. Returns `None` if too short to be a
/// usable query.
fn tail_snippet(tail: &str, max_chars: usize) -> Option<String> {
    let collapsed = tail.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    if trimmed.chars().count() < 40 {
        return None;
    }
    if trimmed.chars().count() <= max_chars {
        return Some(trimmed.to_string());
    }
    let mut cut: String = trimmed.chars().take(max_chars).collect();
    if let Some(idx) = cut.rfind(' ') {
        cut.truncate(idx);
    }
    if cut.chars().count() < 40 {
        None
    } else {
        Some(cut)
    }
}

/// Collect up to `max_candidates` chunks from `project` whose tokenized length
/// exceeds `window`, drawing each query from the text beyond the window.
///
/// `tokenizer` MUST have truncation disabled (load via
/// [`pgmcp::embed::model::bge_m3_model_dir`] + `Tokenizer::from_file` and
/// `with_truncation(None)`) so the full token sequence is visible.
pub async fn collect_holdout_candidates(
    pool: &PgPool,
    project: &str,
    tokenizer: &tokenizers::Tokenizer,
    window: usize,
    max_candidates: usize,
) -> Result<Vec<HoldoutCandidate>, sqlx::Error> {
    // Substantial chunks (likely > `window` tokens) but not giant transcripts.
    // Longest-first so the hold-out tail is reliably non-empty; deterministic.
    let fetch = (max_candidates * 6).max(96) as i64;
    let rows = sqlx::query(
        "SELECT f.relative_path, c.start_line, c.end_line, c.content \
         FROM file_chunks c \
         JOIN indexed_files f ON f.id = c.file_id \
         JOIN projects p ON p.id = f.project_id \
         WHERE p.name = $1 AND c.content IS NOT NULL \
           AND length(c.content) BETWEEN 2600 AND 16000 \
         ORDER BY length(c.content) DESC, f.relative_path, c.start_line \
         LIMIT $2",
    )
    .bind(project)
    .bind(fetch)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(max_candidates);
    for row in rows {
        if out.len() >= max_candidates {
            break;
        }
        let relative_path: String = row.get("relative_path");
        let start_line: i32 = row.get("start_line");
        let end_line: i32 = row.get("end_line");
        let content: String = row.get("content");

        let Ok(encoding) = tokenizer.encode(content.as_str(), true) else {
            continue;
        };
        let ids = encoding.get_ids();
        if ids.len() <= window + 16 {
            continue; // not enough beyond the window to form a query
        }
        let Ok(tail) = tokenizer.decode(&ids[window..], true) else {
            continue;
        };
        let Some(query) = tail_snippet(&tail, 400) else {
            continue;
        };
        out.push(HoldoutCandidate {
            relative_path,
            start_line: start_line as i64,
            end_line: end_line as i64,
            query,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_snippet_rejects_short_and_cuts_at_word_boundary() {
        assert!(tail_snippet("too short", 400).is_none());
        let long = "word ".repeat(200); // 1000 chars
        let s = tail_snippet(&long, 100).expect("snippet");
        assert!(s.chars().count() <= 100);
        assert!(!s.ends_with(' '));
        assert!(!s.is_empty());
    }

    #[test]
    fn tail_snippet_collapses_whitespace() {
        let s = tail_snippet("alpha   beta\n\n  gamma delta epsilon zeta eta theta", 400)
            .expect("snippet");
        assert!(s.starts_with("alpha beta gamma"));
        assert!(!s.contains('\n'));
    }
}
