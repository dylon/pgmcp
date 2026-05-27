//! Metric extraction from command output and benchmark artifacts.
//!
//! Pure parsers shared by the runner (per-replicate extraction) and the
//! `experiment_log_artifact` / `pgmcp experiment ingest` import paths
//! (hyperfine `--export-json`, criterion `sample.json`, `/usr/bin/time -v`).

use regex::Regex;

/// First capture group of `pattern` over `text`, parsed as f64.
pub fn extract_regex(text: &str, pattern: &str) -> Result<f64, String> {
    let re = Regex::new(pattern).map_err(|e| format!("bad regex: {e}"))?;
    let caps = re
        .captures(text)
        .ok_or_else(|| "regex did not match output".to_string())?;
    let g = caps
        .get(1)
        .ok_or_else(|| "regex has no capture group 1".to_string())?;
    g.as_str()
        .trim()
        .parse::<f64>()
        .map_err(|e| format!("capture '{}' is not a number: {e}", g.as_str()))
}

/// f64 at an RFC-6901 JSON pointer within `json_text`.
pub fn extract_json_pointer(json_text: &str, pointer: &str) -> Result<f64, String> {
    let v: serde_json::Value =
        serde_json::from_str(json_text).map_err(|e| format!("output is not JSON: {e}"))?;
    let node = v
        .pointer(pointer)
        .ok_or_else(|| format!("JSON pointer '{pointer}' not found"))?;
    node.as_f64()
        .ok_or_else(|| format!("value at '{pointer}' is not a number"))
}

/// `Maximum resident set size (kbytes): N` from `/usr/bin/time -v` stderr.
pub fn parse_time_v_max_rss(stderr: &str) -> Result<f64, String> {
    for line in stderr.lines() {
        if let Some(idx) = line.find("Maximum resident set size")
            && let Some((_, rest)) = line[idx..].split_once(':')
        {
            return rest
                .trim()
                .parse::<f64>()
                .map_err(|e| format!("max RSS value parse: {e}"));
        }
    }
    Err("`/usr/bin/time -v` max-RSS line not found in stderr".to_string())
}

/// hyperfine `--export-json`: the first result's per-run `times` vector
/// (seconds). hyperfine does its own warm-up/repetition, so this single file
/// IS the sample vector.
pub fn parse_hyperfine_times(json_text: &str) -> Result<Vec<f64>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json_text).map_err(|e| format!("hyperfine JSON: {e}"))?;
    let results = v
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or("hyperfine JSON has no results[]")?;
    let first = results.first().ok_or("hyperfine results[] is empty")?;
    let times = first
        .get("times")
        .and_then(|t| t.as_array())
        .ok_or("hyperfine result has no times[]")?;
    let samples: Vec<f64> = times.iter().filter_map(|x| x.as_f64()).collect();
    if samples.is_empty() {
        return Err("hyperfine times[] held no numbers".to_string());
    }
    Ok(samples)
}

/// criterion `new/sample.json`: per-iteration time (ns) = `times[i] / iters[i]`.
pub fn parse_criterion_samples(sample_json: &str) -> Result<Vec<f64>, String> {
    let v: serde_json::Value =
        serde_json::from_str(sample_json).map_err(|e| format!("criterion sample.json: {e}"))?;
    let iters = v
        .get("iters")
        .and_then(|x| x.as_array())
        .ok_or("criterion sample.json has no iters[]")?;
    let times = v
        .get("times")
        .and_then(|x| x.as_array())
        .ok_or("criterion sample.json has no times[]")?;
    if iters.len() != times.len() || iters.is_empty() {
        return Err("criterion iters[]/times[] length mismatch or empty".to_string());
    }
    let mut out = Vec::with_capacity(iters.len());
    for (it, tm) in iters.iter().zip(times) {
        let i = it.as_f64().unwrap_or(0.0);
        let t = tm.as_f64().unwrap_or(0.0);
        if i > 0.0 {
            out.push(t / i);
        }
    }
    if out.is_empty() {
        return Err("criterion samples reduced to empty".to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_capture() {
        let v = extract_regex("throughput: 1234.5 qps", r"throughput:\s*([0-9.]+)").expect("rx");
        assert!((v - 1234.5).abs() < 1e-9);
    }

    #[test]
    fn json_pointer() {
        let v =
            extract_json_pointer(r#"{"result":{"rss_mb":612.0}}"#, "/result/rss_mb").expect("jp");
        assert!((v - 612.0).abs() < 1e-9);
    }

    #[test]
    fn time_v_max_rss() {
        let stderr =
            "\tCommand being timed: \"foo\"\n\tMaximum resident set size (kbytes): 624800\n";
        let v = parse_time_v_max_rss(stderr).expect("rss");
        assert!((v - 624800.0).abs() < 1e-9);
    }

    #[test]
    fn hyperfine_times() {
        let json = r#"{"results":[{"command":"x","times":[0.0101,0.0099,0.0100]}]}"#;
        let s = parse_hyperfine_times(json).expect("hf");
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn criterion_samples_per_iter() {
        let json = r#"{"iters":[10.0,10.0],"times":[1000.0,1200.0]}"#;
        let s = parse_criterion_samples(json).expect("cr");
        assert_eq!(s, vec![100.0, 120.0]);
    }
}
