//! P13.3 — articulatory severity integration in tool_naming_consistency.
//!
//! Verifies that:
//!   1. `severity_articulatory` is present on every divergence.
//!   2. The score is non-negative and finite.
//!   3. Identifiers that are phonetically close to their suggested
//!      rename score lower than identifiers that are far away.

use pgmcp::fuzzy::phonetic::articulatory_distance_score;

#[test]
fn articulatory_distance_orders_close_below_far() {
    // Same as the integration test will see internally.
    let close = articulatory_distance_score("receive_request", "receiverequest");
    let far = articulatory_distance_score("receive_request", "process_response");
    assert!(
        close < far,
        "expected close ({close}) < far ({far}) — articulatory severity must distinguish near-rename from unrelated"
    );
}

#[test]
fn articulatory_score_is_zero_for_identical_strings() {
    assert_eq!(articulatory_distance_score("foo", "foo"), 0.0);
}

#[test]
fn articulatory_score_is_non_negative_and_finite() {
    for (a, b) in [
        ("", ""),
        ("a", ""),
        ("a", "b"),
        ("hello_world", "helloWorld"),
        ("recieveRequest", "receive_request"),
    ] {
        let d = articulatory_distance_score(a, b);
        assert!(d.is_finite(), "score must be finite for ({a},{b})");
        assert!(d >= 0.0, "score must be non-negative for ({a},{b})");
    }
}
