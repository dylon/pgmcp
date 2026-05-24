//! P13.3 — `.pgmcp/rules.llev` hot-reload test.
//!
//! Writes a `.llev` file, opens PgmcpPhonetics against it, replaces
//! the file, calls reload_rules manually (mirrors what the
//! filesystem watcher does on a file event), asserts the active
//! rule set changes.

use std::sync::Arc;

use pgmcp::fuzzy::phonetic::PgmcpPhonetics;

// `.llev` files are line-oriented `pattern -> replacement;` rules.
// See `liblevenshtein-rust/data/rules/english/base.llev` for the full
// reference; we keep these tiny so the test exercises the parser
// without depending on the larger rule sets' semantics.
const RULESET_A: &str = "@name \"test-a\"\n@version \"1\"\nx -> y;\n";

const RULESET_B: &str = "@name \"test-b\"\n@version \"1\"\nx -> y;\ny -> z;\n";

#[test]
fn reload_rules_swaps_active_set() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let rules_path = tmp.path().join("rules.llev");
    std::fs::write(&rules_path, RULESET_A).expect("write A");

    let phon = PgmcpPhonetics::open(&rules_path, "en-us").expect("open");
    let count_a = phon.rules().len();
    assert!(count_a >= 1, "A must have at least one rule");

    std::fs::write(&rules_path, RULESET_B).expect("write B");
    phon.reload_rules(&rules_path).expect("reload");
    let count_b = phon.rules().len();
    assert!(
        count_b > count_a,
        "B has more rules than A: a={count_a} b={count_b}"
    );
}

#[test]
fn reset_to_default_falls_back_to_english_base() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let rules_path = tmp.path().join("rules.llev");
    std::fs::write(&rules_path, RULESET_A).expect("write A");

    let phon = PgmcpPhonetics::open(&rules_path, "en-us").expect("open");
    let count_override = phon.rules().len();
    phon.reset_to_default();
    let count_default = phon.rules().len();
    assert_ne!(
        count_override, count_default,
        "embedded English base has a different rule count than the 1-rule test override"
    );
}

#[test]
fn watch_handle_can_be_installed() {
    // We can't reliably trigger a watcher event in a test (notify's
    // backend buffering varies per platform), but we can verify
    // that `watch()` installs cleanly without erroring.
    let tmp = tempfile::tempdir().expect("tempdir");
    let rules_path = tmp.path().join("rules.llev");
    std::fs::write(&rules_path, RULESET_A).expect("write");
    let phon = Arc::new(PgmcpPhonetics::open(&rules_path, "en-us").expect("open"));
    phon.watch(rules_path.clone()).expect("watch install");
    // Watcher kept alive by the Arc; dropping the Arc tears it down.
}
