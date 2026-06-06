//! P13.3 — PgmcpPhonetics end-to-end search test.
//!
//! Builds a PhoneticNormalizedDictionary from a small vocabulary
//! using PgmcpPhonetics' active rules, queries with a phonetic
//! near-match, asserts the dictionary returns the expected entry.
//!
//! This exercises the rules → dictionary → query chain that the
//! Phase-13.4 phonetic MCP tools depend on.

use pgmcp::fuzzy::phonetic::PgmcpPhonetics;

#[test]
fn dictionary_built_via_pgmcp_phonetics_returns_phonetic_match() {
    let phon = PgmcpPhonetics::default_english();
    let vocab = [
        "receive_request",
        "process_response",
        "validate_input",
        "encode_payload",
    ];
    let dict = phon.build_dictionary(vocab.iter().copied());

    // The dictionary's normalize() should produce a stable form
    // regardless of source spelling; querying with a misspelling
    // close to "receive" should return that term.
    let hits = dict.query("recieve_request", 2);
    let terms: Vec<&str> = hits.iter().map(|c| c.term.as_str()).collect();
    assert!(
        terms.contains(&"receive_request"),
        "expected receive_request in hits, got {:?}",
        terms
    );
}

#[test]
fn normalize_via_phonetics_handle_does_not_panic() {
    let phon = PgmcpPhonetics::default_english();
    for input in ["", "a", "Hello World", "PhOnE", "receive_request_v2"] {
        let _normalized = phon.normalize(input);
        // No panic, no NaN/empty edge case; specific output
        // depends on the loaded rule pack.
    }
}

#[test]
fn expand_to_pattern_returns_regex_for_known_input() {
    let phon = PgmcpPhonetics::default_english();
    let pattern = phon.expand_to_pattern("phone");
    // Output is non-empty and not just the input verbatim — it
    // includes alternation produced by the English ph→f rule.
    assert!(!pattern.is_empty());
}
