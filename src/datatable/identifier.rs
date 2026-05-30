//! Identifier safety for data-table and column names.
//!
//! Table names and column names are validated against a strict charset
//! **before** any SQL and are also enforced by DB CHECK constraints
//! (defense-in-depth). Note: names are stored and queried only as bound
//! *values* (a `data_tables.name` predicate, a `data_table_columns.name` row),
//! never spliced into SQL as identifiers — so this charset is hygiene + report-
//! header usability, not the injection defense (the parameter binding is). Row
//! *field* keys used in filters/aggregations are bound parameters and are not
//! subject to this charset.

use std::sync::OnceLock;

use regex::Regex;

/// Lowercase, starts with a letter, only `[a-z0-9_]`, 1..=63 chars.
fn ident_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[a-z][a-z0-9_]{0,62}$").expect("data-table identifier regex compiles")
    })
}

/// Whether `s` is a valid table/column identifier.
pub fn valid_identifier(s: &str) -> bool {
    ident_re().is_match(s)
}

/// Validate `s`, returning a human-readable reason on failure (for mapping to
/// `McpError::invalid_params`).
pub fn validate_identifier(s: &str) -> Result<(), String> {
    if valid_identifier(s) {
        Ok(())
    } else {
        Err(format!(
            "invalid name {s:?}: must be lowercase, start with a letter, \
             contain only [a-z0-9_], and be 1-63 characters"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_names() {
        for ok in ["obs", "bench_runs", "a", "x9", "review_findings_2026", "m"] {
            assert!(valid_identifier(ok), "{ok:?} should be valid");
            assert!(validate_identifier(ok).is_ok());
        }
    }

    #[test]
    fn rejects_malformed_names() {
        for bad in [
            "",
            "1x",            // starts with a digit
            "Obs",           // uppercase
            "with space",    // space
            "x-y",           // hyphen
            "x'y",           // quote (injection-shaped)
            "drop;table",    // semicolon
            "tab\tname",     // control char
            "naïve",         // non-ascii
            &"a".repeat(64), // 64 chars (> 63)
        ] {
            assert!(!valid_identifier(bad), "{bad:?} should be invalid");
            assert!(validate_identifier(bad).is_err());
        }
    }

    #[test]
    fn boundary_lengths() {
        assert!(valid_identifier(&format!("a{}", "0".repeat(62)))); // 63 chars
        assert!(!valid_identifier(&format!("a{}", "0".repeat(63)))); // 64 chars
    }
}
