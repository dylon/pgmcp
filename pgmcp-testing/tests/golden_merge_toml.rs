//! Golden-file tests for `pgmcp::config::merge_toml_values`.
//!
//! The generator serialises the merged TOML back to a pretty-printed
//! string so the fixture is deterministic (no field-order drift from
//! `toml::Value` internals) and human-inspectable. Tests call the
//! same serialisation path so the comparison is exact.

use pgmcp::config::merge_toml_values;
use pgmcp_testing::golden::assert_match_exact;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MergeTomlInput {
    defaults: String,
    user: String,
}

fn run_merge(input: &MergeTomlInput) -> String {
    let defaults: toml::Value = toml::from_str(&input.defaults).expect("parse defaults");
    let user: toml::Value = toml::from_str(&input.user).expect("parse user");
    let merged = merge_toml_values(defaults, user);
    toml::to_string_pretty(&merged).expect("re-serialize merged")
}

#[test]
fn tables_user_wins_scalars_matches_golden() {
    assert_match_exact::<MergeTomlInput, String>("merge_toml/tables_user_wins_scalars", run_merge);
}

#[test]
fn arrays_union_matches_golden() {
    assert_match_exact::<MergeTomlInput, String>("merge_toml/arrays_union", run_merge);
}
