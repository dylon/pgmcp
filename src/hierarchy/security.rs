//! Cross-project CVE exposure (ADR-027 E6). Computes each project's DIRECT
//! supply-chain exposure by matching its manifest package names against
//! `vuln_advisories`, then the tool propagates exposure across the project
//! dependency graph (a vuln in D exposes D's transitive dependents) with a
//! confidence-decayed severity. Name-level matching here is the cross-project
//! propagation substrate; precise per-manifest SemVer matching is
//! `cve_supply_chain` (per project).

use std::collections::HashMap;

use sqlx::PgPool;

/// Coarse severity rank from an advisory's free-text severity.
pub fn severity_rank(s: Option<&str>) -> i32 {
    match s.map(|x| x.to_ascii_lowercase()).as_deref() {
        Some("critical") => 4,
        Some("high") => 3,
        Some("medium") | Some("moderate") => 2,
        Some("low") => 1,
        _ => 1,
    }
}

/// Per-project direct exposure: `project_id → (vulnerable package names, max severity rank)`.
pub async fn direct_exposure(
    pool: &PgPool,
) -> Result<HashMap<i32, (Vec<String>, i32)>, sqlx::Error> {
    // Advisory package → max severity rank.
    let adv: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT package, severity FROM vuln_advisories")
            .fetch_all(pool)
            .await?;
    let mut adv_sev: HashMap<String, i32> = HashMap::with_capacity(adv.len());
    for (pkg, sev) in adv {
        let r = severity_rank(sev.as_deref());
        adv_sev
            .entry(pkg)
            .and_modify(|x| *x = (*x).max(r))
            .or_insert(r);
    }
    if adv_sev.is_empty() {
        return Ok(HashMap::new());
    }

    // Each project's manifest files.
    let manifests: Vec<(i32, String, Option<String>)> = sqlx::query_as(
        "SELECT project_id, relative_path, content FROM indexed_files
          WHERE relative_path ~ '(^|/)(Cargo\\.toml|package\\.json|requirements\\.txt|pyproject\\.toml|go\\.mod|pom\\.xml|lake-manifest\\.json|lakefile\\.lean)$'",
    )
    .fetch_all(pool)
    .await?;

    let mut out: HashMap<i32, (Vec<String>, i32)> = HashMap::new();
    for (pid, path, content) in manifests {
        let Some(c) = content else {
            continue;
        };
        let fname = path.rsplit('/').next().unwrap_or(path.as_str());
        for name in crate::deps::ecosystems::package_names(fname, &c) {
            if let Some(&r) = adv_sev.get(&name) {
                let e = out.entry(pid).or_insert_with(|| (Vec::new(), 0));
                if !e.0.contains(&name) {
                    e.0.push(name);
                }
                e.1 = e.1.max(r);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_rank_orders() {
        assert!(severity_rank(Some("critical")) > severity_rank(Some("high")));
        assert!(severity_rank(Some("high")) > severity_rank(Some("medium")));
        assert!(severity_rank(Some("medium")) > severity_rank(Some("low")));
        assert_eq!(severity_rank(None), 1);
        assert_eq!(
            severity_rank(Some("moderate")),
            severity_rank(Some("medium"))
        );
    }
}
