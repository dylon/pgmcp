//! Vulnerability-advisory queries (clear/bulk-insert/load offline-CVE rows).
//! Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// One `vuln_advisories` row: an advisory's vulnerable range for one package.
/// (graph-roadmap Phase 4.5)
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct VulnAdvisoryRow {
    pub advisory_id: String,
    pub ecosystem: String,
    pub package: String,
    pub introduced: Option<String>,
    pub fixed: Option<String>,
    pub last_affected: Option<String>,
    pub severity: Option<String>,
    pub summary: Option<String>,
}

/// Delete all advisories (a re-import replaces the dump wholesale).
pub async fn clear_vuln_advisories(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("DELETE FROM vuln_advisories")
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Bulk-insert advisory rows (UNNEST). Called by the offline import.
pub async fn bulk_insert_vuln_advisories(
    pool: &PgPool,
    rows: &[VulnAdvisoryRow],
) -> Result<u64, sqlx::Error> {
    if rows.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = rows.iter().map(|r| r.advisory_id.clone()).collect();
    let ecos: Vec<String> = rows.iter().map(|r| r.ecosystem.clone()).collect();
    let pkgs: Vec<String> = rows.iter().map(|r| r.package.clone()).collect();
    let intro: Vec<Option<String>> = rows.iter().map(|r| r.introduced.clone()).collect();
    let fixed: Vec<Option<String>> = rows.iter().map(|r| r.fixed.clone()).collect();
    let last: Vec<Option<String>> = rows.iter().map(|r| r.last_affected.clone()).collect();
    let sev: Vec<Option<String>> = rows.iter().map(|r| r.severity.clone()).collect();
    let summ: Vec<Option<String>> = rows.iter().map(|r| r.summary.clone()).collect();
    let res = sqlx::query(
        "INSERT INTO vuln_advisories
            (advisory_id, ecosystem, package, introduced, fixed, last_affected, severity, summary)
         SELECT * FROM UNNEST(
             $1::text[], $2::text[], $3::text[], $4::text[],
             $5::text[], $6::text[], $7::text[], $8::text[]
         )",
    )
    .bind(&ids)
    .bind(&ecos)
    .bind(&pkgs)
    .bind(&intro)
    .bind(&fixed)
    .bind(&last)
    .bind(&sev)
    .bind(&summ)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Load advisories for the given ecosystem + package names (the dependency
/// inventory), for SemVer matching in `cve_supply_chain`. Empty `packages` ⇒
/// empty result.
pub async fn load_vuln_advisories(
    pool: &PgPool,
    ecosystem: &str,
    packages: &[String],
) -> Result<Vec<VulnAdvisoryRow>, sqlx::Error> {
    if packages.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, VulnAdvisoryRow>(
        "SELECT advisory_id, ecosystem, package, introduced, fixed, last_affected, severity, summary
         FROM vuln_advisories
         WHERE ecosystem = $1 AND package = ANY($2)",
    )
    .bind(ecosystem)
    .bind(packages)
    .fetch_all(pool)
    .await
}
