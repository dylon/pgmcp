//! P13.3 — articulatory near-name merge ranking in find_similar_modules.
//!
//! The cross_language_signature_clones materialized view is populated
//! by a separate cron, so this test asserts on the deterministic
//! ranking shape:
//!   - Pairs with `articulatory_distance ≤ [fuzzy].phonetic_merge_threshold`
//!     should rank above pairs that share only the type signature.
//!
//! Without seeding the cron output (which requires a full graph
//! pipeline), we exercise the ordering function directly via the
//! same scoring formula the tool uses inline.

use pgmcp::config::Config;
use pgmcp::fuzzy::phonetic::articulatory_distance_score;

fn rank_score(similarity: f64, articulatory_distance: f64, merge_threshold: f64) -> f64 {
    let denom = merge_threshold.max(1e-9);
    let bump = 0.2 * (1.0 - (articulatory_distance / denom).clamp(0.0, 1.0));
    similarity + bump
}

#[test]
fn near_name_pair_outranks_distant_name_pair_at_equal_similarity() {
    let cfg = Config::default();
    let threshold = cfg.fuzzy.phonetic_merge_threshold;

    // "foo" vs "boo" is a single voicing-only edit (b↔f is partly
    // voicing + manner) so the articulatory cost is sub-1.0 — well
    // under the default 2.0 threshold. "foo" vs a totally different
    // string blows past it.
    let near_dist = articulatory_distance_score("foo", "boo");
    let far_dist = articulatory_distance_score("foo", "xyzzy");
    assert!(
        near_dist < threshold,
        "test premise: near_dist ({near_dist}) must be < threshold ({threshold}) to get a bump"
    );

    let same_sim = 0.85_f64;
    let near_score = rank_score(same_sim, near_dist, threshold);
    let far_score = rank_score(same_sim, far_dist, threshold);

    assert!(
        near_score > far_score,
        "near-name pair (dist={near_dist}, score={near_score}) must outrank \
         far-name pair (dist={far_dist}, score={far_score}) when type-similarity is equal"
    );
}

#[test]
fn high_similarity_with_far_names_still_outranks_low_similarity_with_close_names() {
    let cfg = Config::default();
    let threshold = cfg.fuzzy.phonetic_merge_threshold;

    let close_dist = articulatory_distance_score("foo", "boo");
    let far_dist = articulatory_distance_score("foo", "completely_different_xyzzy");

    // Articulatory bump is capped at +0.2; type-similarity dominates
    // beyond that gap. This invariant prevents pure-name matches
    // from displacing type-signature matches at the top of the list.
    let high = rank_score(0.99_f64, far_dist, threshold);
    let low = rank_score(0.50_f64, close_dist, threshold);
    assert!(
        high > low,
        "type-similarity gap of 0.49 must dominate articulatory bump (≤ 0.2)"
    );
}
