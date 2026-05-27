//! Plan-definition tools: author a reusable plan template + its dictated
//! structural rules (`plan_define`), and validate a concrete plan instance
//! against a definition (`plan_validate`). The rule-checking logic is the pure
//! `crate::tracker::validate`; here we only marshal params ↔ DB.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use serde::{Deserialize, Serialize};

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::{
    PlanDefineParams, PlanDefinitionExportParams, PlanDefinitionImportParams, PlanValidateParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::{id_of_public, map_db_err};
use crate::mcp::tools::work_items::slugify;

/// `plan_define` — create/update a plan-definition template and its rules.
/// Re-running with the same `(slug, version)` replaces the rule set (a clean
/// edit). An invalid `rule_kind`/`severity` is rejected by the DB CHECK and
/// surfaced as `invalid_params`.
pub async fn tool_plan_define(
    ctx: &SystemContext,
    params: PlanDefineParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let title = params.title.trim();
    if title.is_empty() {
        return Err(McpError::invalid_params("title must be non-empty", None));
    }
    let slug = params
        .slug
        .as_deref()
        .map(slugify)
        .unwrap_or_else(|| slugify(title));
    let version = params.version.unwrap_or(1).max(1);
    let status = params.status.as_deref().unwrap_or("active");

    let extends_id = match params.extends_slug.as_deref() {
        None => None,
        Some(es) => {
            let parent = queries::get_plan_definition(pool, &slugify(es), None)
                .await
                .map_err(map_db_err)?
                .ok_or_else(|| {
                    McpError::invalid_params(format!("extends_slug '{es}' not found"), None)
                })?;
            Some(parent.id)
        }
    };

    let def_id = queries::upsert_plan_definition(
        pool,
        &slug,
        version,
        title,
        params.description.as_deref(),
        extends_id,
        status,
        None,
    )
    .await
    .map_err(map_db_err)?;

    queries::clear_definition_rules(pool, def_id)
        .await
        .map_err(map_db_err)?;
    let mut n_rules = 0_usize;
    for r in &params.rules {
        queries::insert_definition_rule(
            pool,
            def_id,
            &r.rule_kind,
            r.applies_to_kind.as_deref(),
            r.child_kind.as_deref(),
            r.min_count,
            r.max_count,
            r.field_name.as_deref(),
            r.pattern.as_deref(),
            r.severity.as_deref().unwrap_or("error"),
        )
        .await
        .map_err(|e| {
            McpError::invalid_params(format!("rule '{}' rejected: {e}", r.rule_kind), None)
        })?;
        n_rules += 1;
    }

    json_result(&json!({
        "slug": slug,
        "version": version,
        "definition_id": def_id,
        "status": status,
        "rules": n_rules,
    }))
}

/// `plan_validate` — validate a plan instance (the subtree rooted at
/// `root_public_id`) against a definition's rules. Returns a severity-sorted
/// violations report; `valid` is true when there are no `error`-severity
/// violations. Advisory — it reports; it does not block.
pub async fn tool_plan_validate(
    ctx: &SystemContext,
    params: PlanValidateParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().plan_validations.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let root_id = id_of_public(pool, &params.root_public_id).await?;
    let def = queries::get_plan_definition(
        pool,
        &slugify(&params.definition_slug),
        params.definition_version,
    )
    .await
    .map_err(map_db_err)?
    .ok_or_else(|| {
        McpError::invalid_params(
            format!("plan definition '{}' not found", params.definition_slug),
            None,
        )
    })?;

    let violations = queries::validate_plan(pool, root_id, def.id)
        .await
        .map_err(map_db_err)?;
    let error_count = violations.iter().filter(|v| v.severity == "error").count();
    let warn_count = violations.iter().filter(|v| v.severity == "warn").count();

    json_result(&json!({
        "root": params.root_public_id,
        "definition": format!("{}@v{}", def.slug, def.version),
        "valid": error_count == 0,
        "summary": {
            "error_count": error_count,
            "warn_count": warn_count,
            "total": violations.len(),
        },
        "violations": violations,
    }))
}

// ============================================================================
// TOML round-trip (Phase 9c) — `plan_definition_export` / `_import`.
//
// The DB is the source of truth; the TOML file is a portable, inspectable,
// serene-eclipse-shaped artifact. The schema is `[definition]` (metadata) +
// optional `[scope]` (free-form passthrough preserved verbatim across
// round-trips) + `[[rule]]` array-of-tables (the dictated structural rules).
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
struct DefinitionDoc {
    definition: DefinitionMeta,
    /// Free-form scope block (serene-eclipse `[scope]`); preserved verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<toml::Value>,
    #[serde(default, rename = "rule", skip_serializing_if = "Vec::is_empty")]
    rules: Vec<RuleDoc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DefinitionMeta {
    slug: String,
    #[serde(default = "one")]
    version: i32,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default = "active_status")]
    status: String,
    /// Parent definition slug (inheritance); resolved on import if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extends: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RuleDoc {
    rule_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    applies_to_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    child_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_count: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_count: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    field_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pattern: Option<String>,
    #[serde(default = "error_severity")]
    severity: String,
}

fn one() -> i32 {
    1
}
fn active_status() -> String {
    "active".to_string()
}
fn error_severity() -> String {
    "error".to_string()
}

/// `plan_definition_export` — serialize a stored plan definition (metadata +
/// `[scope]` passthrough + rules) to serene-eclipse-shaped TOML. The TOML
/// string is always returned; if `path` is given it is also written there
/// (parent directories are created).
pub async fn tool_plan_definition_export(
    ctx: &SystemContext,
    params: PlanDefinitionExportParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let def = queries::get_plan_definition(pool, &slugify(&params.slug), params.version)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| {
            McpError::invalid_params(format!("plan definition '{}' not found", params.slug), None)
        })?;
    let rule_rows = queries::list_definition_rules(pool, def.id)
        .await
        .map_err(map_db_err)?;

    // Resolve the parent slug (for `extends`) if this definition inherits.
    let extends = match def.extends_id {
        None => None,
        Some(pid) => queries::get_plan_definition_by_id(pool, pid)
            .await
            .map_err(map_db_err)?
            .map(|p| p.slug),
    };

    // Preserve any `[scope]` block previously stored in body_toml.
    let scope = def
        .body_toml
        .as_deref()
        .and_then(|s| toml::from_str::<toml::Value>(s).ok())
        .and_then(|v| v.get("scope").cloned());

    let doc = DefinitionDoc {
        definition: DefinitionMeta {
            slug: def.slug.clone(),
            version: def.version,
            title: def.title.clone(),
            description: def.description.clone(),
            status: def.status.clone(),
            extends,
        },
        scope,
        rules: rule_rows
            .into_iter()
            .map(|r| RuleDoc {
                rule_kind: r.rule_kind,
                applies_to_kind: r.applies_to_kind,
                child_kind: r.child_kind,
                min_count: r.min_count,
                max_count: r.max_count,
                field_name: r.field_name,
                pattern: r.pattern,
                severity: r.severity,
            })
            .collect(),
    };

    let toml_str = toml::to_string_pretty(&doc)
        .map_err(|e| McpError::internal_error(format!("TOML serialize failed: {e}"), None))?;

    let written_path = match params.path.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(p) => {
            let path = std::path::PathBuf::from(p);
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    McpError::internal_error(format!("mkdir {}: {e}", parent.display()), None)
                })?;
            }
            tokio::fs::write(&path, &toml_str).await.map_err(|e| {
                McpError::internal_error(format!("write {}: {e}", path.display()), None)
            })?;
            Some(path.display().to_string())
        }
    };

    json_result(&json!({
        "slug": def.slug,
        "version": def.version,
        "rules": doc.rules.len(),
        "path": written_path,
        "toml": toml_str,
    }))
}

/// `plan_definition_import` — parse a serene-eclipse-shaped TOML document
/// (inline `toml` string or a `path` to read) into a plan definition + its
/// rules. The raw TOML is stored in `body_toml` (preserving the `[scope]`
/// block), then the rule set is replaced. Idempotent on `(slug, version)`.
pub async fn tool_plan_definition_import(
    ctx: &SystemContext,
    params: PlanDefinitionImportParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    // Source the TOML text from the inline string or the file path.
    let raw = match (params.toml.as_deref(), params.path.as_deref()) {
        (Some(s), _) if !s.trim().is_empty() => s.to_string(),
        (_, Some(p)) if !p.is_empty() => tokio::fs::read_to_string(p)
            .await
            .map_err(|e| McpError::invalid_params(format!("read {p}: {e}"), None))?,
        _ => {
            return Err(McpError::invalid_params(
                "provide either 'toml' (inline) or 'path' (file to read)",
                None,
            ));
        }
    };

    let doc: DefinitionDoc = toml::from_str(&raw)
        .map_err(|e| McpError::invalid_params(format!("invalid definition TOML: {e}"), None))?;

    let slug = slugify(&doc.definition.slug);
    let title = doc.definition.title.trim();
    if title.is_empty() {
        return Err(McpError::invalid_params(
            "[definition].title must be non-empty",
            None,
        ));
    }
    let version = doc.definition.version.max(1);

    let extends_id = match doc.definition.extends.as_deref() {
        None => None,
        Some(es) => Some(
            queries::get_plan_definition(pool, &slugify(es), None)
                .await
                .map_err(map_db_err)?
                .ok_or_else(|| McpError::invalid_params(format!("extends '{es}' not found"), None))?
                .id,
        ),
    };

    let def_id = queries::upsert_plan_definition(
        pool,
        &slug,
        version,
        title,
        doc.definition.description.as_deref(),
        extends_id,
        &doc.definition.status,
        Some(&raw),
    )
    .await
    .map_err(map_db_err)?;

    queries::clear_definition_rules(pool, def_id)
        .await
        .map_err(map_db_err)?;
    let mut n_rules = 0usize;
    for r in &doc.rules {
        queries::insert_definition_rule(
            pool,
            def_id,
            &r.rule_kind,
            r.applies_to_kind.as_deref(),
            r.child_kind.as_deref(),
            r.min_count,
            r.max_count,
            r.field_name.as_deref(),
            r.pattern.as_deref(),
            &r.severity,
        )
        .await
        .map_err(|e| {
            McpError::invalid_params(format!("rule '{}' rejected: {e}", r.rule_kind), None)
        })?;
        n_rules += 1;
    }

    json_result(&json!({
        "imported": true,
        "slug": slug,
        "version": version,
        "definition_id": def_id,
        "rules": n_rules,
    }))
}
