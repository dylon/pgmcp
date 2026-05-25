//! Deterministic contextual-retrieval prefix builder (graph-roadmap Phase 2.4).
//!
//! Anthropic's "Contextual Retrieval" (2024) prepends a short situating blurb
//! to each chunk *before embedding* (~35% fewer retrieval failures; ~49% with
//! contextual BM25). Anthropic generates that blurb with an LLM; for **code**
//! pgmcp already materializes everything needed to write it **deterministically**
//! — exact, free, reproducible, no external model:
//!
//! - enclosing symbol (kind + name + signature) from `file_symbols`,
//! - file path + language from `indexed_files`,
//! - module role (importer count) from `code_graph_edges`,
//! - topic labels (c-TF-IDF) from `chunk_topic_assignments` / `code_topics`.
//!
//! The prefix is prepended to the chunk content for embedding only; the raw
//! `content` returned to the agent is unchanged.

/// Inputs assembled (per chunk) by the contextual re-embed cron from the DB.
#[derive(Debug, Clone, Default)]
pub struct ChunkContext {
    pub relative_path: String,
    pub language: String,
    /// Enclosing symbol kind (e.g. `function`, `class`), if any.
    pub symbol_kind: Option<String>,
    /// Enclosing symbol name, if any.
    pub symbol_name: Option<String>,
    /// Enclosing symbol signature, if recorded.
    pub symbol_signature: Option<String>,
    /// Top topic labels for the chunk (c-TF-IDF), best first.
    pub topics: Vec<String>,
    /// Number of files that import this file (module centrality proxy).
    pub importer_count: i64,
}

/// Build the deterministic situating prefix. Returns a compact, single-line-ish
/// blurb terminated by a newline so it reads as a header above the chunk. Empty
/// only in the degenerate case of no path (never expected).
pub fn build_context_prefix(ctx: &ChunkContext) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(5);

    if !ctx.relative_path.is_empty() {
        parts.push(format!("File: {}", ctx.relative_path));
    }
    if !ctx.language.is_empty() {
        parts.push(format!("Lang: {}", ctx.language));
    }
    match (&ctx.symbol_kind, &ctx.symbol_name) {
        (Some(kind), Some(name)) => {
            // Prefer the signature when present (richer), else kind+name.
            match &ctx.symbol_signature {
                Some(sig) if !sig.trim().is_empty() => {
                    parts.push(format!("{kind}: {}", sig.trim()));
                }
                _ => parts.push(format!("{kind}: {name}")),
            }
        }
        (None, Some(name)) => parts.push(format!("Symbol: {name}")),
        _ => {}
    }
    if !ctx.topics.is_empty() {
        let labels: Vec<&str> = ctx.topics.iter().take(3).map(|s| s.as_str()).collect();
        parts.push(format!("Topics: {}", labels.join(", ")));
    }
    if ctx.importer_count > 0 {
        parts.push(format!("Imported by: {}", ctx.importer_count));
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!("[{}]\n", parts.join(" | "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ChunkContext {
        ChunkContext {
            relative_path: "src/db/queries.rs".into(),
            language: "rust".into(),
            symbol_kind: Some("function".into()),
            symbol_name: Some("hybrid_search_chunks".into()),
            symbol_signature: Some("pub async fn hybrid_search_chunks(...)".into()),
            topics: vec![
                "retrieval".into(),
                "sql".into(),
                "embeddings".into(),
                "extra".into(),
            ],
            importer_count: 7,
        }
    }

    #[test]
    fn prefix_includes_path_symbol_topics_importers() {
        let p = build_context_prefix(&ctx());
        assert!(p.starts_with("[File: src/db/queries.rs"), "got {p}");
        assert!(p.contains("Lang: rust"));
        assert!(p.contains("hybrid_search_chunks"));
        assert!(
            p.contains("Topics: retrieval, sql, embeddings"),
            "top-3 topics: {p}"
        );
        assert!(!p.contains("extra"), "topics capped at 3");
        assert!(p.contains("Imported by: 7"));
        assert!(p.ends_with("]\n"));
    }

    #[test]
    fn falls_back_to_name_without_signature() {
        let mut c = ctx();
        c.symbol_signature = None;
        let p = build_context_prefix(&c);
        assert!(
            p.contains("function: hybrid_search_chunks"),
            "kind: name fallback: {p}"
        );
    }

    #[test]
    fn empty_context_yields_no_prefix() {
        let c = ChunkContext::default();
        assert_eq!(build_context_prefix(&c), "");
    }
}
