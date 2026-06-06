//! Request-bound normalization for fuzzy MCP tools.

pub const DEFAULT_FUZZY_DISTANCE: u32 = 2;
pub const MAX_FUZZY_DISTANCE: u32 = 64;
pub const DEFAULT_FUZZY_LIMIT: u32 = 20;
pub const MAX_FUZZY_LIMIT: u32 = 100;

pub fn bounded_max_distance(raw: Option<u32>) -> usize {
    raw.unwrap_or(DEFAULT_FUZZY_DISTANCE)
        .min(MAX_FUZZY_DISTANCE) as usize
}

pub fn bounded_limit(raw: Option<u32>) -> usize {
    raw.unwrap_or(DEFAULT_FUZZY_LIMIT).clamp(1, MAX_FUZZY_LIMIT) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_distance_is_bounded_but_allows_exact_match_mode() {
        assert_eq!(bounded_max_distance(Some(0)), 0);
        assert_eq!(bounded_max_distance(None), DEFAULT_FUZZY_DISTANCE as usize);
        assert_eq!(
            bounded_max_distance(Some(u32::MAX)),
            MAX_FUZZY_DISTANCE as usize
        );
    }

    #[test]
    fn limit_is_clamped_to_non_empty_finite_window() {
        assert_eq!(bounded_limit(Some(0)), 1);
        assert_eq!(bounded_limit(None), DEFAULT_FUZZY_LIMIT as usize);
        assert_eq!(bounded_limit(Some(u32::MAX)), MAX_FUZZY_LIMIT as usize);
    }
}
