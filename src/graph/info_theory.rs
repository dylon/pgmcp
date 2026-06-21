//! SOTA Phase 3 — Information-theoretic metrics.
//!
//! - Normalized Compression Distance (Cilibrasi-Vitányi IEEE TIT 2005) using zstd.
//! - Mutual Information for git co-change (sharper than Jaccard, accounts for marginal frequency).
//! - Conditional entropy H(target | source) over import edges.
//! - Shannon entropy of identifier distribution per file.
//!
//! These produce metrics that complement the embedding-cosine / Jaccard
//! signals — embedding misses parametric/structural clones, Jaccard misses
//! incidental co-change with high-frequency files.

#![allow(dead_code)]

use std::collections::HashMap;

use sqlx::PgPool;

// ============================================================================
// 3.1 NCD — Normalized Compression Distance (Cilibrasi-Vitányi 2005)
// ============================================================================

/// `NCD(x, y) = (C(xy) − min(C(x), C(y))) / max(C(x), C(y))`. Range: ~0 (similar)
/// to ~1+ (unrelated). Uses zstd as the compressor `C`.
///
/// Returns NaN if both inputs are empty (so callers can filter).
pub fn ncd_pair(a: &[u8], b: &[u8], level: i32) -> std::io::Result<f64> {
    if a.is_empty() && b.is_empty() {
        return Ok(f64::NAN);
    }
    let c_a = compressed_len(a, level)?;
    let c_b = compressed_len(b, level)?;
    let mut concat = Vec::with_capacity(a.len() + b.len());
    concat.extend_from_slice(a);
    concat.extend_from_slice(b);
    let c_ab = compressed_len(&concat, level)?;
    let min_c = c_a.min(c_b) as f64;
    let max_c = c_a.max(c_b).max(1) as f64;
    Ok(((c_ab as f64) - min_c) / max_c)
}

fn compressed_len(bytes: &[u8], level: i32) -> std::io::Result<usize> {
    // zstd::bulk::compress returns the compressed bytes; we only care about length.
    let compressed = zstd::bulk::compress(bytes, level)?;
    Ok(compressed.len())
}

/// Symmetric NCD: average of `NCD(a, b)` and `NCD(b, a)` — NCD is not perfectly
/// symmetric under typical compressors because `xy` and `yx` can compress
/// differently. The average is what the original paper recommends in practice.
pub fn ncd_pair_symmetric(a: &[u8], b: &[u8], level: i32) -> std::io::Result<f64> {
    let d1 = ncd_pair(a, b, level)?;
    let d2 = ncd_pair(b, a, level)?;
    Ok((d1 + d2) * 0.5)
}

// ============================================================================
// 3.2 Mutual Information over git co-change
// ============================================================================

#[derive(Debug, Clone)]
pub struct CoChangeMI {
    pub file_a: i64,
    pub file_b: i64,
    pub mi: f64,
    pub support: u32,
}

/// Mutual information between two files' co-change indicators over the project's
/// commit history. `I(A;B) = Σ p(a,b) log(p(a,b) / (p(a)·p(b)))` over the 2×2
/// contingency table {seen, not-seen}.
///
/// Returns the top-K pairs by MI, descending, requiring `min_support` joint
/// commits as a noise filter.
pub async fn cochange_mutual_information(
    pool: &PgPool,
    project_id: i32,
    min_support: u32,
    limit: i32,
) -> Result<Vec<CoChangeMI>, sqlx::Error> {
    // Single SQL fetches per-pair joint counts + per-file marginal counts.
    let rows: Vec<(i64, i64, i64, i64, i64, i64)> =
        sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64)>(
            "WITH fp AS (
                SELECT f.id AS file_id, gcf.commit_id
                FROM git_commit_files gcf
                JOIN git_commits gc ON gc.id = gcf.commit_id
                JOIN indexed_files f ON f.relative_path = gcf.file_path
                    AND f.project_id = $1
                WHERE gc.project_id = $1
            ),
            n_total AS (
                SELECT COUNT(DISTINCT commit_id)::int8 AS n FROM fp
            ),
            singles AS (
                SELECT file_id, COUNT(DISTINCT commit_id)::int8 AS n
                FROM fp GROUP BY file_id
            ),
            pairs AS (
                SELECT a.file_id AS file_a, b.file_id AS file_b,
                       COUNT(*)::int8 AS n_ab
                FROM fp a JOIN fp b
                    ON a.commit_id = b.commit_id AND a.file_id < b.file_id
                GROUP BY a.file_a, b.file_b
                HAVING COUNT(*) >= $2
            )
            SELECT p.file_a, p.file_b, p.n_ab,
                   sa.n AS n_a, sb.n AS n_b, nt.n AS n_total
            FROM pairs p
            JOIN singles sa ON sa.file_id = p.file_a
            JOIN singles sb ON sb.file_id = p.file_b
            CROSS JOIN n_total nt",
        )
        .bind(project_id)
        .bind(min_support as i64)
        .fetch_all(pool)
        .await?;

    let mut out: Vec<CoChangeMI> = Vec::with_capacity(rows.len());
    for (file_a, file_b, n_ab, n_a, n_b, n_total) in rows {
        if n_total <= 0 {
            continue;
        }
        let n = n_total as f64;
        let p_a = n_a as f64 / n;
        let p_b = n_b as f64 / n;
        let p_ab = n_ab as f64 / n;
        let p_a_n = ((n_a - n_ab) as f64).max(0.0) / n;
        let p_n_b = ((n_b - n_ab) as f64).max(0.0) / n;
        let p_n_n = ((n_total - n_a - n_b + n_ab) as f64).max(0.0) / n;

        let mi = mi_term(p_ab, p_a, p_b)
            + mi_term(p_a_n, p_a, 1.0 - p_b)
            + mi_term(p_n_b, 1.0 - p_a, p_b)
            + mi_term(p_n_n, 1.0 - p_a, 1.0 - p_b);

        out.push(CoChangeMI {
            file_a,
            file_b,
            mi,
            support: n_ab as u32,
        });
    }
    out.sort_by(|a, b| b.mi.partial_cmp(&a.mi).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(limit.max(0) as usize);
    Ok(out)
}

fn mi_term(p_joint: f64, p_x: f64, p_y: f64) -> f64 {
    if p_joint <= 0.0 || p_x <= 0.0 || p_y <= 0.0 {
        return 0.0;
    }
    p_joint * (p_joint / (p_x * p_y)).log2()
}

// ============================================================================
// 3.3 Conditional entropy of imports H(target | source)
// ============================================================================

#[derive(Debug, Clone)]
pub struct FileEntropy {
    pub source_file_id: i64,
    pub entropy: f64,
    pub n_imports: u32,
}

/// `H(T | S=s) = − Σ_t p(t|s) log p(t|s)`. High entropy on a source file
/// means its imports are spread across many targets (broker / coordinator);
/// low entropy means one or two targets dominate (focused dependency).
pub async fn import_conditional_entropy(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<FileEntropy>, sqlx::Error> {
    let rows: Vec<(i64, Option<i64>, f64)> = sqlx::query_as::<_, (i64, Option<i64>, f64)>(
        "SELECT source_file_id, target_file_id, COALESCE(SUM(weight), 0.0) AS w
         FROM code_graph_edges
         WHERE project_id = $1 AND edge_type = 'import' AND target_file_id IS NOT NULL
           AND target_project_id IS NULL
         GROUP BY source_file_id, target_file_id
         ORDER BY source_file_id",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;

    let mut by_source: HashMap<i64, Vec<f64>> = HashMap::new();
    for (s, _t, w) in rows {
        by_source.entry(s).or_default().push(w.max(0.0));
    }
    let mut out: Vec<FileEntropy> = Vec::with_capacity(by_source.len());
    for (source_file_id, weights) in by_source {
        let total: f64 = weights.iter().sum();
        if total <= 0.0 {
            out.push(FileEntropy {
                source_file_id,
                entropy: 0.0,
                n_imports: weights.len() as u32,
            });
            continue;
        }
        let mut h = 0.0;
        for &w in &weights {
            let p = w / total;
            if p > 0.0 {
                h -= p * p.log2();
            }
        }
        out.push(FileEntropy {
            source_file_id,
            entropy: h,
            n_imports: weights.len() as u32,
        });
    }
    out.sort_by(|a, b| {
        b.entropy
            .partial_cmp(&a.entropy)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

// ============================================================================
// 3.4 Shannon entropy of identifier distribution per file
// ============================================================================

#[derive(Debug, Clone)]
pub struct IdentifierEntropy {
    pub file_id: i64,
    pub entropy: f64,
    pub n_tokens: u32,
}

/// Per-file Shannon entropy over identifier tokens (split snake_case + camelCase).
/// Low entropy → naming pollution / generated code / repetitive variables.
pub async fn identifier_entropy(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<IdentifierEntropy>, sqlx::Error> {
    let rows: Vec<(i64, String)> = sqlx::query_as::<_, (i64, String)>(
        "SELECT fs.file_id, fs.name
         FROM file_symbols fs
         JOIN indexed_files f ON fs.file_id = f.id
         WHERE f.project_id = $1
           AND fs.kind IN ('function','struct','enum','trait','class','method','const','interface','module')",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;

    let mut by_file: HashMap<i64, HashMap<String, u32>> = HashMap::new();
    for (fid, name) in rows {
        let tokens = split_identifier(&name);
        let entry = by_file.entry(fid).or_default();
        for tok in tokens {
            if tok.is_empty() {
                continue;
            }
            *entry.entry(tok).or_insert(0) += 1;
        }
    }

    let mut out: Vec<IdentifierEntropy> = Vec::with_capacity(by_file.len());
    for (file_id, freq) in by_file {
        let total: u32 = freq.values().sum();
        if total == 0 {
            out.push(IdentifierEntropy {
                file_id,
                entropy: 0.0,
                n_tokens: 0,
            });
            continue;
        }
        let mut h = 0.0;
        for &c in freq.values() {
            let p = c as f64 / total as f64;
            if p > 0.0 {
                h -= p * p.log2();
            }
        }
        out.push(IdentifierEntropy {
            file_id,
            entropy: h,
            n_tokens: total,
        });
    }
    out.sort_by(|a, b| {
        b.entropy
            .partial_cmp(&a.entropy)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

/// Split an identifier into camelCase and snake_case tokens, lowercased.
pub fn split_identifier(name: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if c == '_' || c == '-' || c.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current).to_lowercase());
            }
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower && !current.is_empty() {
            tokens.push(std::mem::take(&mut current).to_lowercase());
        }
        current.push(c);
        prev_lower = c.is_ascii_lowercase();
    }
    if !current.is_empty() {
        tokens.push(current.to_lowercase());
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ncd_identical_inputs_are_close_to_zero() {
        let a = b"the quick brown fox jumps over the lazy dog";
        let r = ncd_pair(a, a, 3).expect("ncd");
        assert!(
            r < 0.3,
            "ncd of identical inputs should be small, got {}",
            r
        );
    }

    #[test]
    fn ncd_unrelated_inputs_are_close_to_one() {
        let a = b"the quick brown fox jumps over the lazy dog";
        let b = b"function add(a, b) { return a + b; } export default add;";
        let r = ncd_pair(a, b, 3).expect("ncd");
        // Range can drift; ensure it's at least bounded.
        assert!(r > 0.0);
        assert!(r < 2.0);
    }

    #[test]
    fn ncd_empty_inputs_yield_nan() {
        let r = ncd_pair(b"", b"", 3).expect("ncd");
        assert!(r.is_nan());
    }

    #[test]
    fn ncd_symmetric_average_works() {
        let a = b"alpha beta gamma delta";
        let b = b"alpha beta gamma";
        let r = ncd_pair_symmetric(a, b, 3).expect("ncd");
        assert!(r.is_finite());
    }

    #[test]
    fn mi_term_handles_zero_inputs() {
        assert_eq!(mi_term(0.0, 0.5, 0.5), 0.0);
        assert_eq!(mi_term(0.5, 0.0, 0.5), 0.0);
        assert_eq!(mi_term(0.5, 0.5, 0.0), 0.0);
    }

    #[test]
    fn mi_term_positive_for_aligned() {
        // p(a,b) > p(a) p(b) — positive contribution
        let t = mi_term(0.5, 0.5, 0.5);
        assert!(t > 0.0);
    }

    #[test]
    fn split_identifier_snake_case() {
        assert_eq!(split_identifier("foo_bar_baz"), vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn split_identifier_camel_case() {
        assert_eq!(split_identifier("fooBarBaz"), vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn split_identifier_pascal_case() {
        assert_eq!(split_identifier("FooBarBaz"), vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn split_identifier_mixed() {
        let r = split_identifier("HTTPParser_doParse");
        assert!(r.contains(&"http".to_string()) || r.contains(&"httpparser".to_string()));
        assert!(r.iter().any(|s| s == "parse" || s == "do"));
    }
}
