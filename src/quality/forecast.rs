//! Pure trend/forecast math over a metric time-series — no DB, no
//! [`SystemContext`](crate::context::SystemContext), so it is trivially
//! unit-testable and reused by both the trend tools
//! (`tool_quality_trend.rs` / `tool_quality_forecast.rs`) and the proactive
//! digest (`src/digest`). All inputs are plain `f64` series; callers pull them
//! from `quality_report_history` (`src/quality/history.rs`) or the tracker's
//! burndown.
//!
//! The "trajectory" half of the snapshots→trajectories upgrade: a snapshot says
//! "engineering GPA is 2.4"; these functions say "GPA is falling 0.1/week and
//! hits the C-grade boundary (2.0) in ~4 weeks".

/// Values whose magnitude is below this are treated as zero (a flat trend / a
/// zero baseline) to avoid dividing by a hair.
const EPS: f64 = 1e-9;

/// Ordinary-least-squares slope of `y` against `x` over `points = [(x, y), …]`,
/// in *units of y per unit of x* (callers pass `x` in days, so the result is
/// "per day"). Returns `None` when there are fewer than two points or the `x`
/// values are all equal (a vertical/degenerate fit has no finite slope).
pub fn ols_slope(points: &[(f64, f64)]) -> Option<f64> {
    let n = points.len();
    if n < 2 {
        return None;
    }
    let n_f = n as f64;
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    let mut sum_xy = 0.0;
    let mut sum_xx = 0.0;
    for &(x, y) in points {
        sum_x += x;
        sum_y += y;
        sum_xy += x * y;
        sum_xx += x * x;
    }
    let denom = n_f * sum_xx - sum_x * sum_x;
    if denom.abs() < EPS {
        return None; // all x equal — no spread to regress against
    }
    Some((n_f * sum_xy - sum_x * sum_y) / denom)
}

/// Weeks until a metric currently at `latest`, moving at `slope_per_day`,
/// reaches `threshold` — e.g. "engineering GPA (2.4) falling 0.014/day hits the
/// C-grade floor (2.0) in ~4 weeks". Returns `None` when the metric is flat
/// (`slope_per_day ≈ 0`) or moving *away* from the threshold (it never crosses),
/// or is already at/past it.
pub fn weeks_to_threshold(latest: f64, slope_per_day: f64, threshold: f64) -> Option<f64> {
    if slope_per_day.abs() < EPS {
        return None; // flat — never crosses
    }
    let days = (threshold - latest) / slope_per_day;
    if days <= 0.0 {
        return None; // already at/past the threshold, or moving away from it
    }
    Some(days / 7.0)
}

/// Signed percent change from `prev` to `latest`, relative to `|prev|`
/// (e.g. `prev=10, latest=12 → +20.0`). Returns `None` when `prev ≈ 0` (percent
/// change off a zero baseline is undefined). The sign reflects direction:
/// positive = grew, negative = shrank.
pub fn pct_change(prev: f64, latest: f64) -> Option<f64> {
    if prev.abs() < EPS {
        return None;
    }
    Some((latest - prev) / prev.abs() * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn ols_slope_recovers_a_known_line() {
        // y = 2x + 1 → slope 2.0.
        let pts = [(0.0, 1.0), (1.0, 3.0), (2.0, 5.0), (3.0, 7.0)];
        assert!(approx(ols_slope(&pts).expect("slope"), 2.0));
    }

    #[test]
    fn ols_slope_is_negative_for_a_falling_series() {
        // A declining GPA: 3.0 → 2.7 → 2.4 over days 0,7,14.
        let pts = [(0.0, 3.0), (7.0, 2.7), (14.0, 2.4)];
        let s = ols_slope(&pts).expect("slope");
        assert!(s < 0.0, "declining series has negative slope, got {s}");
        assert!(approx(s, -0.3 / 7.0)); // -0.3 GPA per 7 days
    }

    #[test]
    fn ols_slope_none_for_degenerate_inputs() {
        assert_eq!(ols_slope(&[]), None);
        assert_eq!(ols_slope(&[(1.0, 1.0)]), None); // one point
        assert_eq!(ols_slope(&[(5.0, 1.0), (5.0, 9.0)]), None); // all x equal
    }

    #[test]
    fn weeks_to_threshold_projects_a_falling_metric() {
        // GPA 2.4, falling 0.05/day, threshold 2.0 → 8 days → ~1.143 weeks.
        let w = weeks_to_threshold(2.4, -0.05, 2.0).expect("crosses");
        assert!(approx(w, 8.0 / 7.0), "got {w}");
    }

    #[test]
    fn weeks_to_threshold_none_when_flat_or_diverging() {
        assert_eq!(
            weeks_to_threshold(2.4, 0.0, 2.0),
            None,
            "flat never crosses"
        );
        // Rising metric never reaches a lower threshold.
        assert_eq!(weeks_to_threshold(2.4, 0.05, 2.0), None, "moving away");
        // Already at/below the threshold.
        assert_eq!(weeks_to_threshold(1.9, -0.05, 2.0), None, "already past");
    }

    #[test]
    fn pct_change_signs_and_zero_baseline() {
        assert!(approx(pct_change(10.0, 12.0).expect("pct"), 20.0));
        assert!(approx(pct_change(10.0, 8.0).expect("pct"), -20.0));
        // Off a negative baseline, growth toward zero is positive.
        assert!(approx(pct_change(-10.0, -8.0).expect("pct"), 20.0));
        assert_eq!(pct_change(0.0, 5.0), None, "zero baseline is undefined");
    }
}
