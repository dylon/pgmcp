//! MCP tools for the dedicated software pattern / anti-pattern knowledge index.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

use chrono::Utc;
use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::{debug, error, info};

use crate::context::SystemContext;
use crate::db::patterns::{
    self, PatternListOptions, PatternSearchOptions, PatternSearchRow, SourceStateRow, SourceUpsert,
};
use crate::mcp::server::*;
use crate::patterns::{self as pattern_catalog, SourceDescriptor};

const DEFAULT_SEARCH_LIMIT: i32 = 10;
const DEFAULT_LIST_LIMIT: i32 = 50;
const DEFAULT_EXCERPT_CHARS: usize = 700;
const PATTERN_EMBEDDING_SCHEMA_VERSION: &str = "pgmcp-pattern-embedding-v1";

#[derive(Debug, Default)]
struct ImportSummary {
    sources_seen: i32,
    sources_imported: i32,
    sources_skipped: i32,
    chunks_embedded: i32,
    failed_sources: Vec<serde_json::Value>,
}

pub async fn tool_software_pattern_search(
    ctx: &SystemContext,
    params: SoftwarePatternSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    ensure_seeded_if_empty(ctx, pool).await?;

    let limit = params.limit.unwrap_or(DEFAULT_SEARCH_LIMIT).clamp(1, 50);
    info!(
        tool = "software_pattern_search",
        query = %truncate(&params.query, 200),
        limit,
        "MCP tool invoked"
    );

    let rows = search_rows(
        ctx,
        pool,
        &params.query,
        limit * 5,
        PatternSearchOptions {
            kind: params.kind,
            paradigms: normalize_paradigms(params.paradigms),
            category: params.category,
            source_family: params.source_family,
            source_type: params.source_type,
        },
    )
    .await?;
    let results = aggregate_matches(rows, params.include_sources.unwrap_or(true), limit);

    json_result(json!({
        "query": params.query,
        "result_count": results.len(),
        "results": results,
        "guidance": "These results come from the separate software-pattern knowledge index, not indexed source files.",
    }))
}

pub async fn tool_recommend_design_patterns(
    ctx: &SystemContext,
    params: RecommendDesignPatternsParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    ensure_seeded_if_empty(ctx, pool).await?;

    let limit = params.limit.unwrap_or(8).clamp(1, 30);
    let paradigms = infer_paradigms(
        ctx,
        params.paradigms.clone(),
        params.language.as_deref(),
        params.project.as_deref(),
    )
    .await;
    let query = recommendation_query(&params.task, params.constraints.as_deref());

    let pattern_rows = search_rows(
        ctx,
        pool,
        &query,
        limit * 5,
        PatternSearchOptions {
            kind: Some("pattern".to_string()),
            paradigms: Some(paradigms.clone()),
            category: None,
            source_family: None,
            source_type: None,
        },
    )
    .await?;

    let include_antipatterns = params.include_antipatterns.unwrap_or(true);
    let anti_patterns = if include_antipatterns {
        let rows = search_rows(
            ctx,
            pool,
            &query,
            limit * 3,
            PatternSearchOptions {
                kind: Some("anti_pattern".to_string()),
                paradigms: Some(paradigms.clone()),
                category: None,
                source_family: None,
                source_type: None,
            },
        )
        .await?;
        aggregate_matches(rows, true, (limit / 2).max(3))
    } else {
        Vec::new()
    };

    let recommended = aggregate_matches(pattern_rows, true, limit);
    json_result(json!({
        "task": params.task,
        "project": params.project,
        "language": params.language,
        "paradigms": paradigms,
        "constraints": params.constraints.unwrap_or_default(),
        "recommended_patterns": recommended,
        "anti_patterns_to_avoid": anti_patterns,
        "planning_guidance": [
            "Prefer the smallest pattern that directly addresses the task forces.",
            "Treat anti-pattern matches as review prompts, not automatic rejections.",
            "Use source citations to inspect the rationale before committing to a design."
        ],
    }))
}

pub async fn tool_review_design_patterns(
    ctx: &SystemContext,
    params: ReviewDesignPatternsParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    ensure_seeded_if_empty(ctx, pool).await?;

    let limit = params.limit.unwrap_or(8).clamp(1, 30);
    let paradigms = infer_paradigms(
        ctx,
        params.paradigms.clone(),
        params.language.as_deref(),
        params.project.as_deref(),
    )
    .await;

    let risk_rows = search_rows(
        ctx,
        pool,
        &params.design,
        limit * 4,
        PatternSearchOptions {
            kind: Some("anti_pattern".to_string()),
            paradigms: Some(paradigms.clone()),
            category: None,
            source_family: None,
            source_type: None,
        },
    )
    .await?;
    let alternative_rows = search_rows(
        ctx,
        pool,
        &params.design,
        limit * 4,
        PatternSearchOptions {
            kind: Some("pattern".to_string()),
            paradigms: Some(paradigms.clone()),
            category: None,
            source_family: None,
            source_type: None,
        },
    )
    .await?;

    json_result(json!({
        "project": params.project,
        "language": params.language,
        "paradigms": paradigms,
        "anti_pattern_risks": aggregate_matches(risk_rows, true, limit),
        "pattern_alternatives": aggregate_matches(alternative_rows, true, limit),
        "review_guidance": "Use high-scoring anti-patterns as targeted questions for the design review; confirm with code/context before changing the plan.",
    }))
}

pub async fn tool_get_software_pattern(
    ctx: &SystemContext,
    params: GetSoftwarePatternParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    ensure_seeded_if_empty(ctx, pool).await?;

    let Some(pattern) = patterns::get_pattern(pool, &params.slug_or_id)
        .await
        .map_err(sql_error("get_software_pattern"))?
    else {
        return json_result(json!({
            "found": false,
            "slug_or_id": params.slug_or_id,
            "message": "No software pattern found for slug_or_id"
        }));
    };

    let include_sources = params.include_sources.unwrap_or(true);
    let include_excerpts = params.include_excerpts.unwrap_or(false);
    let sources = if include_sources {
        let mut out = Vec::new();
        for source in patterns::get_pattern_sources(pool, pattern.id)
            .await
            .map_err(sql_error("get_software_pattern"))?
        {
            let excerpts = if include_excerpts {
                patterns::get_source_excerpts(pool, source.id, 2)
                    .await
                    .map_err(sql_error("get_software_pattern"))?
                    .into_iter()
                    .map(|s| truncate_owned(&s, DEFAULT_EXCERPT_CHARS))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            out.push(json!({
                "id": source.id,
                "source_family": source.source_family,
                "title": source.title,
                "url": source.url,
                "license_label": source.license_label,
                "source_type": source.source_type,
                "ingest_policy": source.ingest_policy,
                "status": source.status,
                "fetched_at": source.fetched_at,
                "imported_at": source.imported_at,
                "chunk_count": source.chunk_count,
                "excerpts": excerpts,
            }));
        }
        out
    } else {
        Vec::new()
    };

    json_result(json!({
        "found": true,
        "pattern": pattern,
        "sources": sources,
    }))
}

pub async fn tool_list_software_patterns(
    ctx: &SystemContext,
    params: ListSoftwarePatternsParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    ensure_seeded_if_empty(ctx, pool).await?;

    let limit = params.limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);
    let rows = patterns::list_patterns(
        pool,
        PatternListOptions {
            kind: params.kind,
            paradigm: params.paradigm,
            category: params.category,
            source_family: params.source_family,
            limit,
            offset,
        },
    )
    .await
    .map_err(sql_error("list_software_patterns"))?;

    json_result(json!({
        "count": rows.len(),
        "limit": limit,
        "offset": offset,
        "patterns": rows,
    }))
}

pub async fn tool_pattern_catalog_stats(ctx: &SystemContext) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let stats = patterns::catalog_stats(pool)
        .await
        .map_err(sql_error("pattern_catalog_stats"))?;

    json_result(json!({
        "stats": stats,
        "registered_source_families": registered_source_families(),
        "registered_sources": pattern_catalog::source_registry().len(),
    }))
}

pub async fn tool_refresh_pattern_catalog(
    ctx: &SystemContext,
    params: RefreshPatternCatalogParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let mode = params.mode.unwrap_or_else(|| "seed_only".to_string());
    let dry_run = params.dry_run.unwrap_or(false);
    let source_family = params.source_family.as_deref();
    let limit = params.limit.map(|v| v.max(0) as usize);

    let sources = select_sources(&mode, source_family, limit)?;
    if dry_run {
        return json_result(json!({
            "dry_run": true,
            "mode": mode,
            "source_family": source_family,
            "sources_seen": sources.len(),
            "sources": sources.iter().map(source_descriptor_json).collect::<Vec<_>>(),
        }));
    }

    let run_id = patterns::start_import_run(pool, &mode, source_family)
        .await
        .map_err(sql_error("refresh_pattern_catalog"))?;

    let mut summary = ImportSummary::default();
    let result = async {
        let seed_summary = seed_catalog(ctx, pool).await?;
        summary.sources_imported += seed_summary.sources_imported;
        summary.sources_skipped += seed_summary.sources_skipped;
        summary.chunks_embedded += seed_summary.chunks_embedded;

        if mode != "seed_only" {
            let imported = import_registered_sources(ctx, pool, &sources).await?;
            summary.sources_seen += imported.sources_seen;
            summary.sources_imported += imported.sources_imported;
            summary.sources_skipped += imported.sources_skipped;
            summary.chunks_embedded += imported.chunks_embedded;
            summary.failed_sources = imported.failed_sources;
        }

        Ok::<(), McpError>(())
    }
    .await;

    match result {
        Ok(()) => {
            patterns::finish_import_run(
                pool,
                run_id,
                "succeeded",
                summary.sources_seen,
                summary.sources_imported,
                summary.chunks_embedded,
                None,
            )
            .await
            .map_err(sql_error("refresh_pattern_catalog"))?;
            json_result(json!({
                "run_id": run_id,
                "mode": mode,
                "source_family": source_family,
                "summary": {
                    "sources_seen": summary.sources_seen,
                    "sources_imported": summary.sources_imported,
                    "sources_skipped": summary.sources_skipped,
                    "chunks_embedded": summary.chunks_embedded,
                    "failed_sources": summary.failed_sources,
                }
            }))
        }
        Err(e) => {
            let err_msg = e.to_string();
            let _ = patterns::finish_import_run(
                pool,
                run_id,
                "failed",
                summary.sources_seen,
                summary.sources_imported,
                summary.chunks_embedded,
                Some(&err_msg),
            )
            .await;
            Err(e)
        }
    }
}

pub async fn tool_upsert_pattern_source(
    ctx: &SystemContext,
    params: UpsertPatternSourceParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    ensure_seeded_if_empty(ctx, pool).await?;

    let pattern_id = patterns::find_pattern_id_by_slug(pool, &params.pattern_slug)
        .await
        .map_err(sql_error("upsert_pattern_source"))?
        .ok_or_else(|| {
            McpError::invalid_params(
                format!("Unknown software pattern slug: {}", params.pattern_slug),
                None,
            )
        })?;

    let source_id = patterns::upsert_source(
        pool,
        SourceUpsert {
            source_family: &params.source_family,
            title: &params.title,
            url: params.url.as_deref(),
            license_label: params.license_label.as_deref(),
            source_type: &params.source_type,
            ingest_policy: "manual_local",
            content: Some(&params.content),
            status: "imported",
            error: None,
            metadata: json!({"manual": true}),
            fetched_at: Some(Utc::now()),
        },
    )
    .await
    .map_err(sql_error("upsert_pattern_source"))?;

    patterns::link_source_pattern(pool, source_id, pattern_id, "documents")
        .await
        .map_err(sql_error("upsert_pattern_source"))?;

    let chunks_embedded = if params.reembed.unwrap_or(true) {
        embed_source_content(ctx, pool, source_id, &params.content).await?
    } else {
        0
    };

    json_result(json!({
        "pattern_slug": params.pattern_slug,
        "source_id": source_id,
        "chunks_embedded": chunks_embedded,
    }))
}

async fn search_rows(
    ctx: &SystemContext,
    pool: &PgPool,
    query: &str,
    limit: i32,
    options: PatternSearchOptions,
) -> Result<Vec<PatternSearchRow>, McpError> {
    let embedding = ctx.embed().embed_query(query).await.map_err(|e| {
        error!(tool = "software_patterns", error = %e, "Embedding failed");
        McpError::internal_error(format!("Embedding failed: {}", e), None)
    })?;
    let ef_search = ctx.config().load().vector.ef_search;
    patterns::semantic_search_patterns(pool, &embedding, limit, ef_search, options)
        .await
        .map_err(sql_error("software_pattern_search"))
}

async fn ensure_seeded_if_empty(ctx: &SystemContext, pool: &PgPool) -> Result<(), McpError> {
    let count = patterns::count_patterns(pool)
        .await
        .map_err(sql_error("software_patterns"))?;
    if count == 0 {
        debug!("Software pattern catalog is empty; seeding bundled cards");
        seed_catalog(ctx, pool).await?;
    }
    Ok(())
}

async fn seed_catalog(ctx: &SystemContext, pool: &PgPool) -> Result<ImportSummary, McpError> {
    let mut summary = ImportSummary::default();

    for paradigm in pattern_catalog::paradigm_seeds() {
        patterns::upsert_paradigm(pool, &paradigm)
            .await
            .map_err(sql_error("refresh_pattern_catalog"))?;
    }

    for seed in pattern_catalog::pattern_seeds() {
        let pattern_id = patterns::upsert_pattern(pool, &seed)
            .await
            .map_err(sql_error("refresh_pattern_catalog"))?;
        for paradigm_slug in seed.paradigms {
            patterns::link_pattern_paradigm(pool, pattern_id, paradigm_slug)
                .await
                .map_err(sql_error("refresh_pattern_catalog"))?;
        }

        let content = pattern_catalog::card_content(&seed);
        let content_hash = patterns::content_hash(&content);
        let content_sha256 = patterns::content_sha256(&content);
        let signature = embedding_signature(ctx);
        let metadata = source_metadata(
            json!({"seed_slug": seed.slug}),
            Some(&content_sha256),
            Some(&signature),
            None,
            None,
            Some("local_seed"),
        );
        let existing =
            patterns::find_source_state(pool, "pgmcp_seed", seed.name, Some(seed.canonical_url))
                .await
                .map_err(sql_error("refresh_pattern_catalog"))?;

        let source_id = if existing
            .as_ref()
            .is_some_and(|state| source_is_current(state, content_hash, &signature))
        {
            let id = existing.as_ref().expect("checked existing source").id;
            patterns::update_source_status(pool, id, "imported", None, metadata, Some(Utc::now()))
                .await
                .map_err(sql_error("refresh_pattern_catalog"))?;
            summary.sources_skipped += 1;
            id
        } else {
            let source_id = patterns::upsert_source(
                pool,
                SourceUpsert {
                    source_family: "pgmcp_seed",
                    title: seed.name,
                    url: Some(seed.canonical_url),
                    license_label: Some("pgmcp curated card"),
                    source_type: "curated_card",
                    ingest_policy: "bundled_metadata",
                    content: Some(&content),
                    status: "imported",
                    error: None,
                    metadata,
                    fetched_at: Some(Utc::now()),
                },
            )
            .await
            .map_err(sql_error("refresh_pattern_catalog"))?;

            summary.chunks_embedded += embed_source_content(ctx, pool, source_id, &content).await?;
            summary.sources_imported += 1;
            source_id
        };

        patterns::link_source_pattern(pool, source_id, pattern_id, "curated_card")
            .await
            .map_err(sql_error("refresh_pattern_catalog"))?;
    }

    Ok(summary)
}

async fn import_registered_sources(
    ctx: &SystemContext,
    pool: &PgPool,
    sources: &[SourceDescriptor],
) -> Result<ImportSummary, McpError> {
    let mut summary = ImportSummary {
        sources_seen: sources.len() as i32,
        ..ImportSummary::default()
    };

    for source in sources {
        let existing =
            patterns::find_source_state(pool, source.source_family, source.title, Some(source.url))
                .await
                .map_err(sql_error("refresh_pattern_catalog"))?;
        let validators = existing
            .as_ref()
            .map(|state| http_validators_from_metadata(&state.metadata))
            .unwrap_or_default();

        match fetch_source_text(source, &validators).await {
            Ok(FetchedSource::NotModified {
                etag,
                last_modified,
            }) => {
                let Some(state) = existing.as_ref() else {
                    summary.failed_sources.push(json!({
                        "source_family": source.source_family,
                        "title": source.title,
                        "url": source.url,
                        "error": "server returned 304 Not Modified but no cached source exists",
                    }));
                    continue;
                };
                let Some(content) = state.content.as_deref() else {
                    summary.failed_sources.push(json!({
                        "source_family": source.source_family,
                        "title": source.title,
                        "url": source.url,
                        "error": "server returned 304 Not Modified but cached source has no content",
                    }));
                    continue;
                };

                let content_hash = patterns::content_hash(content);
                let content_sha256 = patterns::content_sha256(content);
                let signature = embedding_signature(ctx);
                let metadata = source_metadata(
                    json!({"tags": source.tags, "http_status": 304}),
                    Some(&content_sha256),
                    Some(&signature),
                    etag.as_deref().or(validators.etag.as_deref()),
                    last_modified
                        .as_deref()
                        .or(validators.last_modified.as_deref()),
                    Some("not_modified"),
                );

                if source_is_current(state, content_hash, &signature) {
                    patterns::update_source_status(
                        pool,
                        state.id,
                        "imported",
                        None,
                        metadata,
                        Some(Utc::now()),
                    )
                    .await
                    .map_err(sql_error("refresh_pattern_catalog"))?;
                    link_registered_source_patterns(pool, state.id, source).await?;
                    summary.sources_skipped += 1;
                } else {
                    patterns::update_source_status(
                        pool,
                        state.id,
                        "imported",
                        None,
                        metadata,
                        Some(Utc::now()),
                    )
                    .await
                    .map_err(sql_error("refresh_pattern_catalog"))?;
                    link_registered_source_patterns(pool, state.id, source).await?;
                    summary.chunks_embedded +=
                        embed_source_content(ctx, pool, state.id, content).await?;
                    summary.sources_imported += 1;
                }
            }
            Ok(FetchedSource::Modified {
                content,
                etag,
                last_modified,
            }) => {
                let content_hash = patterns::content_hash(&content);
                let content_sha256 = patterns::content_sha256(&content);
                let signature = embedding_signature(ctx);
                let metadata = source_metadata(
                    json!({"tags": source.tags, "http_status": 200}),
                    Some(&content_sha256),
                    Some(&signature),
                    etag.as_deref(),
                    last_modified.as_deref(),
                    Some("modified"),
                );
                let source_id = patterns::upsert_source(
                    pool,
                    SourceUpsert {
                        source_family: source.source_family,
                        title: source.title,
                        url: Some(source.url),
                        license_label: Some(source.license_label),
                        source_type: source.source_type,
                        ingest_policy: source.ingest_policy,
                        content: Some(&content),
                        status: "imported",
                        error: None,
                        metadata,
                        fetched_at: Some(Utc::now()),
                    },
                )
                .await
                .map_err(sql_error("refresh_pattern_catalog"))?;

                link_registered_source_patterns(pool, source_id, source).await?;

                if existing
                    .as_ref()
                    .is_some_and(|state| source_is_current(state, content_hash, &signature))
                {
                    summary.sources_skipped += 1;
                } else {
                    summary.chunks_embedded +=
                        embed_source_content(ctx, pool, source_id, &content).await?;
                    summary.sources_imported += 1;
                }
            }
            Err(e) => {
                let source_id = patterns::upsert_source(
                    pool,
                    SourceUpsert {
                        source_family: source.source_family,
                        title: source.title,
                        url: Some(source.url),
                        license_label: Some(source.license_label),
                        source_type: source.source_type,
                        ingest_policy: source.ingest_policy,
                        content: None,
                        status: "failed",
                        error: Some(&e),
                        metadata: source_metadata(
                            json!({"tags": source.tags, "last_error_at": Utc::now().to_rfc3339()}),
                            None,
                            None,
                            validators.etag.as_deref(),
                            validators.last_modified.as_deref(),
                            Some("fetch_failed"),
                        ),
                        fetched_at: Some(Utc::now()),
                    },
                )
                .await
                .map_err(sql_error("refresh_pattern_catalog"))?;

                link_registered_source_patterns(pool, source_id, source).await?;

                summary.failed_sources.push(json!({
                    "source_family": source.source_family,
                    "title": source.title,
                    "url": source.url,
                    "error": e,
                }));
            }
        }
    }

    Ok(summary)
}

async fn link_registered_source_patterns(
    pool: &PgPool,
    source_id: i64,
    source: &SourceDescriptor,
) -> Result<(), McpError> {
    for slug in source.pattern_slugs {
        if let Some(pattern_id) = patterns::find_pattern_id_by_slug(pool, slug)
            .await
            .map_err(sql_error("refresh_pattern_catalog"))?
        {
            patterns::link_source_pattern(pool, source_id, pattern_id, "documents")
                .await
                .map_err(sql_error("refresh_pattern_catalog"))?;
        }
    }
    Ok(())
}

fn source_is_current(state: &SourceStateRow, content_hash: i64, embedding_signature: &str) -> bool {
    state.content_hash == Some(content_hash)
        && state.chunk_count > 0
        && state
            .metadata
            .get("embedding_signature")
            .and_then(Value::as_str)
            == Some(embedding_signature)
}

fn embedding_signature(ctx: &SystemContext) -> String {
    let cfg = ctx.config().load();
    format!(
        "{};model={};dimensions={};max_length={};chunk_size_lines={};chunk_overlap_lines={}",
        PATTERN_EMBEDDING_SCHEMA_VERSION,
        cfg.embeddings.model,
        cfg.embeddings.dimensions,
        cfg.embeddings.max_length,
        cfg.embeddings.chunk_size_lines,
        cfg.embeddings.chunk_overlap_lines,
    )
}

fn source_metadata(
    mut metadata: Value,
    content_sha256: Option<&str>,
    embedding_signature: Option<&str>,
    etag: Option<&str>,
    last_modified: Option<&str>,
    refresh_status: Option<&str>,
) -> Value {
    let obj = metadata
        .as_object_mut()
        .expect("source metadata roots are JSON objects");
    obj.insert("content_hash_algorithm".to_string(), json!("xxh3_64"));
    if let Some(sha) = content_sha256 {
        obj.insert("content_sha256".to_string(), json!(sha));
        obj.insert("content_sha256_algorithm".to_string(), json!("sha256"));
    }
    if let Some(signature) = embedding_signature {
        obj.insert("embedding_signature".to_string(), json!(signature));
    }
    if let Some(value) = etag {
        obj.insert("http_etag".to_string(), json!(value));
    }
    if let Some(value) = last_modified {
        obj.insert("http_last_modified".to_string(), json!(value));
    }
    if let Some(status) = refresh_status {
        obj.insert("last_refresh_status".to_string(), json!(status));
    }
    obj.insert(
        "last_checked_at".to_string(),
        json!(Utc::now().to_rfc3339()),
    );
    metadata
}

#[derive(Debug, Clone, Default)]
struct HttpValidators {
    etag: Option<String>,
    last_modified: Option<String>,
}

fn http_validators_from_metadata(metadata: &Value) -> HttpValidators {
    HttpValidators {
        etag: metadata
            .get("http_etag")
            .and_then(Value::as_str)
            .map(str::to_string),
        last_modified: metadata
            .get("http_last_modified")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

#[derive(Debug)]
enum FetchedSource {
    Modified {
        content: String,
        etag: Option<String>,
        last_modified: Option<String>,
    },
    NotModified {
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

async fn embed_source_content(
    ctx: &SystemContext,
    pool: &PgPool,
    source_id: i64,
    content: &str,
) -> Result<i32, McpError> {
    patterns::delete_source_chunks(pool, source_id)
        .await
        .map_err(sql_error("software_patterns"))?;

    let cfg = ctx.config().load();
    let chunks = patterns::chunk_text(
        content,
        cfg.embeddings.chunk_size_lines,
        cfg.embeddings.chunk_overlap_lines,
    );
    drop(cfg);

    let mut embedded = 0;
    for (idx, (start_line, end_line, chunk)) in chunks.iter().enumerate() {
        let embedding = ctx.embed().embed_query(chunk).await.map_err(|e| {
            McpError::internal_error(format!("Pattern source embedding failed: {}", e), None)
        })?;
        patterns::insert_source_chunk(
            pool,
            source_id,
            idx as i32,
            chunk,
            *start_line,
            *end_line,
            &embedding,
        )
        .await
        .map_err(sql_error("software_patterns"))?;
        embedded += 1;
    }

    Ok(embedded)
}

async fn fetch_source_text(
    source: &SourceDescriptor,
    validators: &HttpValidators,
) -> Result<FetchedSource, String> {
    let url = source.url.to_string();
    let title = source.title.to_string();
    let validators = validators.clone();
    tokio::task::spawn_blocking(move || {
        let mut request = ureq::get(&url).set("User-Agent", "pgmcp-pattern-indexer/0.1");
        if let Some(etag) = validators.etag.as_deref() {
            request = request.set("If-None-Match", etag);
        }
        if let Some(last_modified) = validators.last_modified.as_deref() {
            request = request.set("If-Modified-Since", last_modified);
        }

        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(304, response)) => {
                return Ok(FetchedSource::NotModified {
                    etag: response.header("ETag").map(str::to_string),
                    last_modified: response.header("Last-Modified").map(str::to_string),
                });
            }
            Err(e) => return Err(format!("fetch failed for {}: {}", url, e)),
        };
        let etag = response.header("ETag").map(str::to_string);
        let last_modified = response.header("Last-Modified").map(str::to_string);
        let body = response
            .into_string()
            .map_err(|e| format!("read failed for {}: {}", url, e))?;
        let text = html_to_text(&body);
        if text.trim().len() < 80 {
            Err(format!(
                "fetched source '{}' but extracted too little text",
                title
            ))
        } else {
            Ok(FetchedSource::Modified {
                content: format!("Title: {}\nURL: {}\n\n{}", title, url, text),
                etag,
                last_modified,
            })
        }
    })
    .await
    .map_err(|e| format!("fetch task failed: {}", e))?
}

fn html_to_text(input: &str) -> String {
    let mut text = input.to_string();
    for pat in [
        r"(?is)<script[^>]*>.*?</script>",
        r"(?is)<style[^>]*>.*?</style>",
        r"(?is)<noscript[^>]*>.*?</noscript>",
    ] {
        let re = Regex::new(pat).unwrap();
        text = re.replace_all(&text, "\n").into_owned();
    }
    let block =
        Regex::new(r"(?i)</?(p|div|section|article|h[1-6]|li|ul|ol|table|tr|br)[^>]*>").unwrap();
    text = block.replace_all(&text, "\n").into_owned();
    let tags = Regex::new(r"(?is)<[^>]+>").unwrap();
    text = tags.replace_all(&text, " ").into_owned();
    decode_entities(&text)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn aggregate_matches(
    rows: Vec<PatternSearchRow>,
    include_sources: bool,
    limit: i32,
) -> Vec<serde_json::Value> {
    let mut order = Vec::new();
    let mut grouped: HashMap<i64, serde_json::Value> = HashMap::new();
    let mut seen_sources: HashMap<i64, HashSet<i64>> = HashMap::new();

    for row in rows {
        if let Entry::Vacant(entry) = grouped.entry(row.pattern_id) {
            order.push(row.pattern_id);
            entry.insert(json!({
                "id": row.pattern_id,
                "slug": row.slug,
                "name": row.name,
                "kind": row.kind,
                "category": row.category,
                "summary": row.summary,
                "intent": row.intent,
                "canonical_url": row.canonical_url,
                "best_score": row.score,
                "sources": [],
                "excerpts": [],
            }));
        }

        if let Some(entry) = grouped.get_mut(&row.pattern_id) {
            let obj = entry.as_object_mut().expect("match entry is object");
            let excerpts = obj
                .get_mut("excerpts")
                .and_then(|v| v.as_array_mut())
                .expect("excerpts is array");
            if excerpts.len() < 3 {
                excerpts.push(json!({
                    "source_family": row.source_family,
                    "source_title": row.source_title,
                    "text": truncate_owned(&row.chunk_content, DEFAULT_EXCERPT_CHARS),
                    "score": row.score,
                }));
            }

            if include_sources {
                let source_seen = seen_sources.entry(row.pattern_id).or_default();
                if source_seen.insert(row.source_id) {
                    let sources = obj
                        .get_mut("sources")
                        .and_then(|v| v.as_array_mut())
                        .expect("sources is array");
                    sources.push(json!({
                        "id": row.source_id,
                        "source_family": row.source_family,
                        "title": row.source_title,
                        "url": row.source_url,
                        "license_label": row.license_label,
                    }));
                }
            }
        }
    }

    order
        .into_iter()
        .take(limit as usize)
        .filter_map(|id| grouped.remove(&id))
        .collect()
}

async fn infer_paradigms(
    ctx: &SystemContext,
    explicit: Option<Vec<String>>,
    language: Option<&str>,
    project: Option<&str>,
) -> Vec<String> {
    if let Some(paradigms) = normalize_paradigms(explicit)
        && !paradigms.is_empty()
    {
        return paradigms;
    }

    if let Some(lang) = language {
        return paradigms_for_language(lang);
    }

    if let Some(project_name) = project
        && let Ok(summary) = ctx.db().language_summary(project_name).await
        && let Some(top) = summary.into_iter().max_by_key(|r| r.count)
    {
        return paradigms_for_language(&top.language);
    }

    vec![
        "object_oriented_programming".to_string(),
        "functional_programming".to_string(),
        "event_driven_programming".to_string(),
    ]
}

fn paradigms_for_language(language: &str) -> Vec<String> {
    match language.to_ascii_lowercase().as_str() {
        "c" => vec!["procedural_programming"],
        "cpp" | "c++" => vec![
            "object_oriented_programming",
            "procedural_programming",
            "parallel_programming",
        ],
        "java" | "csharp" | "c#" | "kotlin" | "swift" | "ruby" => {
            vec!["object_oriented_programming", "event_driven_programming"]
        }
        "rust" => vec![
            "functional_programming",
            "concurrent_programming",
            "procedural_programming",
        ],
        "go" | "golang" => vec!["concurrent_programming", "procedural_programming"],
        "scala" | "fsharp" | "f#" | "haskell" | "ocaml" | "elm" | "purescript" => {
            vec!["functional_programming"]
        }
        "clojure" | "lisp" | "scheme" => vec!["functional_programming"],
        "prolog" | "datalog" => vec!["logic_programming"],
        "javascript" | "typescript" | "tsx" | "jsx" => vec![
            "event_driven_programming",
            "functional_programming",
            "object_oriented_programming",
        ],
        "aspectj" => vec!["aspect_oriented_programming", "object_oriented_programming"],
        _ => vec![
            "object_oriented_programming",
            "functional_programming",
            "event_driven_programming",
        ],
    }
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn normalize_paradigms(input: Option<Vec<String>>) -> Option<Vec<String>> {
    input.map(|values| {
        values
            .into_iter()
            .map(|v| v.trim().to_ascii_lowercase().replace([' ', '-'], "_"))
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>()
    })
}

fn recommendation_query(task: &str, constraints: Option<&[String]>) -> String {
    let mut query = format!("Feature or refactor task:\n{}", task);
    if let Some(constraints) = constraints
        && !constraints.is_empty()
    {
        query.push_str("\nConstraints:\n");
        query.push_str(&constraints.join("\n"));
    }
    query
}

fn select_sources(
    mode: &str,
    source_family: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<SourceDescriptor>, McpError> {
    let mut sources = match mode {
        "seed_only" => Vec::new(),
        "all" => pattern_catalog::source_registry(),
        "source_family" => {
            let family = source_family.ok_or_else(|| {
                McpError::invalid_params("source_family is required when mode=source_family", None)
            })?;
            pattern_catalog::source_registry()
                .into_iter()
                .filter(|s| s.source_family == family)
                .collect::<Vec<_>>()
        }
        other => {
            return Err(McpError::invalid_params(
                format!(
                    "Unknown refresh mode: {}. Use seed_only, source_family, or all.",
                    other
                ),
                None,
            ));
        }
    };
    if let Some(limit) = limit {
        sources.truncate(limit);
    }
    Ok(sources)
}

fn registered_source_families() -> Vec<String> {
    let mut families = pattern_catalog::source_registry()
        .into_iter()
        .map(|s| s.source_family.to_string())
        .collect::<Vec<_>>();
    families.sort();
    families.dedup();
    families
}

fn source_descriptor_json(source: &SourceDescriptor) -> serde_json::Value {
    json!({
        "source_family": source.source_family,
        "title": source.title,
        "url": source.url,
        "license_label": source.license_label,
        "source_type": source.source_type,
        "ingest_policy": source.ingest_policy,
        "pattern_slugs": source.pattern_slugs,
        "tags": source.tags,
    })
}

fn raw_pool(ctx: &SystemContext) -> Result<&PgPool, McpError> {
    ctx.db().pool().ok_or_else(|| {
        McpError::internal_error(
            "software pattern tools require a real Postgres pool".to_string(),
            None,
        )
    })
}

fn sql_error(tool: &'static str) -> impl Fn(sqlx::Error) -> McpError {
    move |e| {
        error!(tool, error = %e, "MCP tool failed");
        McpError::internal_error(format!("{} failed: {}", tool, e), None)
    }
}

fn json_result(value: serde_json::Value) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(&value)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

fn truncate_owned(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out = s.chars().take(max_chars).collect::<String>();
    out.push_str("...");
    out
}
