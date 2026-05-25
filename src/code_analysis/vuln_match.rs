//! Offline OSV/GHSA advisory matching (graph-roadmap Phase 4.5).
//!
//! Matches a parsed dependency inventory against a locally-imported OSV dump —
//! **no network**. OSV ranges express vulnerability as event boundaries
//! (`introduced` / `fixed` / `last_affected`) over SemVer, so matching needs
//! only version *comparison* (introduced ≤ v < fixed), not the full range DSL —
//! hand-rolled here to avoid a `semver` crate dependency.
//!
//! Pure: `parse_osv` turns one OSV JSON document into [`Advisory`] rows; the
//! cron/CLI persists them and the `cve_supply_chain` tool matches the inventory.

use std::cmp::Ordering;

use serde_json::Value;

/// A parsed SemVer (major.minor.patch + optional prerelease). Build metadata is
/// ignored (per SemVer §10 it does not affect precedence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

/// Parse `1.2.3`, `v1.2`, `1`, `1.2.3-rc.1` (+build stripped). Lenient: missing
/// minor/patch default to 0.
pub fn parse_version(s: &str) -> Option<SemVer> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let s = s.split('+').next().unwrap_or(s); // drop build metadata
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, Some(p.to_string())),
        None => (s, None),
    };
    let mut it = core.split('.');
    let major = it.next()?.parse::<u64>().ok()?;
    let minor = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let patch = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    Some(SemVer {
        major,
        minor,
        patch,
        pre,
    })
}

impl SemVer {
    fn cmp_precedence(&self, other: &SemVer) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                // A version with no prerelease > one with a prerelease (1.0.0 > 1.0.0-rc).
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b), // simplified dotted-id compare
            })
    }
}

/// One OSV-style version range: vulnerable for `introduced ≤ v < fixed`
/// (and `v ≤ last_affected` when given). `None` introduced = from 0.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VulnRange {
    pub introduced: Option<String>,
    pub fixed: Option<String>,
    pub last_affected: Option<String>,
}

impl VulnRange {
    /// Is `version` within this vulnerable range?
    pub fn contains(&self, version: &SemVer) -> bool {
        if let Some(intro) = self.introduced.as_deref().and_then(parse_version)
            && version.cmp_precedence(&intro) == Ordering::Less
        {
            return false;
        }
        if let Some(fixed) = self.fixed.as_deref().and_then(parse_version)
            && version.cmp_precedence(&fixed) != Ordering::Less
        {
            return false; // fixed is the first NON-vulnerable version
        }
        if let Some(last) = self.last_affected.as_deref().and_then(parse_version)
            && version.cmp_precedence(&last) == Ordering::Greater
        {
            return false;
        }
        true
    }
}

/// A vulnerability advisory affecting one (ecosystem, package).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advisory {
    pub id: String,
    pub ecosystem: String,
    pub package: String,
    pub ranges: Vec<VulnRange>,
    pub severity: Option<String>,
    pub summary: Option<String>,
}

impl Advisory {
    pub fn affects(&self, version: &SemVer) -> bool {
        self.ranges.iter().any(|r| r.contains(version))
    }
}

/// Parse one OSV JSON document (`{id, summary, severity, affected:[...]}`) into
/// one [`Advisory`] per affected (ecosystem, package). Returns empty on a
/// non-OSV / malformed doc.
pub fn parse_osv(doc: &Value) -> Vec<Advisory> {
    let id = doc
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if id.is_empty() {
        return Vec::new();
    }
    let summary = doc
        .get("summary")
        .or_else(|| doc.get("details"))
        .and_then(|v| v.as_str())
        .map(|s| s.chars().take(300).collect());
    // Severity: OSV `database_specific.severity` or the `severity[].type`.
    let severity = doc
        .get("database_specific")
        .and_then(|d| d.get("severity"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let mut out = Vec::new();
    let Some(affected) = doc.get("affected").and_then(|a| a.as_array()) else {
        return out;
    };
    for aff in affected {
        let ecosystem = aff
            .get("package")
            .and_then(|p| p.get("ecosystem"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let package = aff
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if package.is_empty() {
            continue;
        }
        let mut ranges: Vec<VulnRange> = Vec::new();
        if let Some(rs) = aff.get("ranges").and_then(|r| r.as_array()) {
            for r in rs {
                // Each range has events: [{introduced}, {fixed}|{last_affected}].
                let mut cur = VulnRange::default();
                if let Some(events) = r.get("events").and_then(|e| e.as_array()) {
                    for ev in events {
                        if let Some(i) = ev.get("introduced").and_then(|v| v.as_str()) {
                            // A new `introduced` starts a fresh range.
                            if cur.introduced.is_some() || cur.fixed.is_some() {
                                ranges.push(std::mem::take(&mut cur));
                            }
                            cur.introduced = Some(i.to_string());
                        }
                        if let Some(f) = ev.get("fixed").and_then(|v| v.as_str()) {
                            cur.fixed = Some(f.to_string());
                        }
                        if let Some(l) = ev.get("last_affected").and_then(|v| v.as_str()) {
                            cur.last_affected = Some(l.to_string());
                        }
                    }
                }
                if cur != VulnRange::default() {
                    ranges.push(cur);
                }
            }
        }
        if ranges.is_empty() {
            continue;
        }
        out.push(Advisory {
            id: id.clone(),
            ecosystem,
            package,
            ranges,
            severity: severity.clone(),
            summary: summary.clone(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        let a = parse_version("1.2.3").unwrap();
        let b = parse_version("1.10.0").unwrap();
        assert_eq!(a.cmp_precedence(&b), Ordering::Less, "1.2.3 < 1.10.0");
        // release > prerelease
        let rc = parse_version("1.0.0-rc.1").unwrap();
        let rel = parse_version("1.0.0").unwrap();
        assert_eq!(rel.cmp_precedence(&rc), Ordering::Greater);
    }

    #[test]
    fn range_contains_introduced_to_fixed() {
        let r = VulnRange {
            introduced: Some("1.0.0".into()),
            fixed: Some("1.5.0".into()),
            last_affected: None,
        };
        assert!(
            r.contains(&parse_version("1.0.0").unwrap()),
            "introduced is vulnerable"
        );
        assert!(r.contains(&parse_version("1.4.9").unwrap()));
        assert!(
            !r.contains(&parse_version("1.5.0").unwrap()),
            "fixed is NOT vulnerable"
        );
        assert!(
            !r.contains(&parse_version("0.9.0").unwrap()),
            "before introduced"
        );
    }

    #[test]
    fn parses_osv_affected_ranges() {
        let doc: Value = serde_json::from_str(
            r#"{
              "id": "GHSA-xxxx",
              "summary": "Bad bug",
              "affected": [{
                "package": {"ecosystem": "crates.io", "name": "foo"},
                "ranges": [{"type": "SEMVER", "events": [{"introduced": "1.0.0"}, {"fixed": "1.2.0"}]}]
              }]
            }"#,
        )
        .unwrap();
        let advs = parse_osv(&doc);
        assert_eq!(advs.len(), 1);
        assert_eq!(advs[0].package, "foo");
        assert_eq!(advs[0].ecosystem, "crates.io");
        assert!(advs[0].affects(&parse_version("1.1.0").unwrap()));
        assert!(!advs[0].affects(&parse_version("1.2.0").unwrap()));
    }

    #[test]
    fn non_osv_doc_is_empty() {
        assert!(parse_osv(&serde_json::json!({"foo": "bar"})).is_empty());
    }
}
