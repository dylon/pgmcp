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

// ============================================================================
// Profile-artifact parsers (Opt-2). Pure: agent-provided text → structured
// metrics. pgmcp never runs perf/valgrind itself.
// ============================================================================

/// One ranked symbol from a `perf report` table.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PerfReportEntry {
    /// Resolved symbol name (the `[.] foo::bar` tail).
    pub symbol: String,
    /// Self (exclusive) percentage — time in this symbol's own code.
    pub self_pct: f64,
    /// Children (inclusive) percentage when the report carries it; else equal
    /// to `self_pct` (a self-only report).
    pub children_pct: f64,
    /// DSO / module column (binary or shared object), when present.
    pub module: Option<String>,
}

/// Parse `perf report` text (the default stdio table, optionally with
/// `--stdio`). Handles both the self-only layout
/// (`  12.34%  binary  [.] symbol`) and the `--children` layout
/// (`  98.7%  12.3%  binary  [.] symbol`). Lines that are headers, comments
/// (`#`), or don't start with a percentage are ignored. Returns entries sorted
/// by `self_pct` descending.
pub fn parse_perf_report(text: &str) -> Vec<PerfReportEntry> {
    let mut out: Vec<PerfReportEntry> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Collect leading percentage tokens (1 = self-only, 2 = children+self).
        let mut fields = trimmed.split_whitespace();
        let first = match fields.next() {
            Some(f) => f,
            None => continue,
        };
        let Some(p1) = parse_percent(first) else {
            continue;
        };
        // Peek the second token: another percentage means children+self order.
        let rest_after_first: Vec<&str> = fields.collect();
        let (children_pct, self_pct, rest): (f64, f64, &[&str]) =
            if let Some(second) = rest_after_first.first()
                && let Some(p2) = parse_percent(second)
            {
                // `--children`: first is children (inclusive), second is self.
                (p1, p2, &rest_after_first[1..])
            } else {
                // Self-only: the single percentage is self == children.
                (p1, p1, &rest_after_first[..])
            };

        // The remainder is `[<dso> ...] [.] <symbol>` — find the `[.]`/`[k]`
        // symbol-kind marker; everything after it is the symbol, the token(s)
        // before it (minus command) are the module.
        let marker_pos = rest
            .iter()
            .position(|t| t.starts_with('[') && t.ends_with(']') && t.len() <= 4);
        let (module, symbol) = match marker_pos {
            Some(pos) => {
                // Module is the token just before the marker (the DSO column),
                // when present.
                let module = if pos >= 1 {
                    Some(rest[pos - 1].to_string())
                } else {
                    None
                };
                let symbol = rest[pos + 1..].join(" ");
                (module, symbol)
            }
            None => {
                // No marker — treat the last token as the symbol.
                let symbol = rest.last().map(|s| s.to_string()).unwrap_or_default();
                (None, symbol)
            }
        };
        if symbol.is_empty() {
            continue;
        }
        out.push(PerfReportEntry {
            symbol,
            self_pct,
            children_pct,
            module,
        });
    }
    out.sort_by(|a, b| {
        b.self_pct
            .partial_cmp(&a.self_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// Parse a token like `12.34%` or `12.34` into an f64 percentage. Returns
/// `None` when the token is not a percentage-shaped number.
fn parse_percent(token: &str) -> Option<f64> {
    let t = token.strip_suffix('%').unwrap_or(token);
    // Must look numeric (avoid treating a symbol like `0x1234` as a percent).
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    t.parse::<f64>().ok()
}

/// Parse Brendan-Gregg "folded stacks" (`stackcollapse-*` output): each line is
/// `frame1;frame2;…;leaf <count>`. Returns a map from the LEAF symbol to the
/// summed sample count (a symbol may appear as the leaf of several distinct
/// stacks). Lines without a trailing integer count are skipped.
pub fn parse_folded_stacks(text: &str) -> std::collections::HashMap<String, u64> {
    let mut out: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // Split off the trailing count (last whitespace-separated token).
        let Some((stack, count_str)) = line.rsplit_once(char::is_whitespace) else {
            continue;
        };
        let Ok(count) = count_str.trim().parse::<u64>() else {
            continue;
        };
        // Leaf = last `;`-separated frame.
        let leaf = stack.rsplit(';').next().unwrap_or(stack).trim();
        if leaf.is_empty() {
            continue;
        }
        *out.entry(leaf.to_string()).or_insert(0) += count;
    }
    out
}

/// Peak-heap summary + top allocating frames from a `massif` text dump
/// (`ms_print` output OR a raw `massif.out.*` file).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MassifSummary {
    /// Peak total heap bytes (max `mem_heap_B` across snapshots, or the peak
    /// snapshot's value in `ms_print` output).
    pub peak_heap_bytes: u64,
    /// Top allocating function frames seen in detailed snapshots, by the
    /// largest byte attribution observed, descending.
    pub top_frames: Vec<MassifFrame>,
}

/// One allocation-site frame from a massif detailed snapshot.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MassifFrame {
    pub function: String,
    pub bytes: u64,
}

/// Parse a `massif.out.*` (raw) or `ms_print` dump. For the raw format we read
/// `mem_heap_B=` lines for the peak and the `n…: bytes function` tree lines of
/// detailed snapshots for frames. For `ms_print` output we read the snapshot
/// table's peak and the `->NN.NN% (X,XXX,XXXB) 0xADDR: func (file)` lines.
pub fn parse_massif(text: &str) -> MassifSummary {
    let mut peak: u64 = 0;
    let mut frames: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

    // Raw format: `mem_heap_B=NNN`.
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("mem_heap_B=")
            && let Ok(v) = rest.trim().parse::<u64>()
        {
            peak = peak.max(v);
        }
    }

    // Frame lines. Two shapes:
    //  raw:      `nK: BYTES 0xADDR: func (file:line)`  (the tree under a snapshot)
    //  ms_print: `->NN.NN% (BYTESB) 0xADDR: func (file:line)`
    for line in text.lines() {
        let l = line.trim();
        // ms_print percentage tree line.
        if let Some(frame) = parse_ms_print_frame(l) {
            let entry = frames.entry(frame.function).or_insert(0);
            *entry = (*entry).max(frame.bytes);
            continue;
        }
        // raw snapshot tree line: `<n>: <bytes> 0x...: <func> (...)`.
        if let Some(frame) = parse_raw_massif_frame(l) {
            // Track peak too — the raw detailed tree's root equals heap bytes.
            peak = peak.max(frame.bytes);
            let entry = frames.entry(frame.function).or_insert(0);
            *entry = (*entry).max(frame.bytes);
        }
    }

    let mut top_frames: Vec<MassifFrame> = frames
        .into_iter()
        .map(|(function, bytes)| MassifFrame { function, bytes })
        .collect();
    top_frames.sort_by_key(|f| std::cmp::Reverse(f.bytes));
    top_frames.truncate(25);

    MassifSummary {
        peak_heap_bytes: peak,
        top_frames,
    }
}

/// `->NN.NN% (1,234,567B) 0xADDR: func_name (file.c:line)` → frame.
fn parse_ms_print_frame(line: &str) -> Option<MassifFrame> {
    let after_arrow = line.strip_prefix("->").or_else(|| line.strip_prefix("| ->"))?;
    // Bytes live in the first `(...B)` group.
    let open = after_arrow.find('(')?;
    let close = after_arrow[open..].find("B)")? + open;
    let bytes_str: String = after_arrow[open + 1..close]
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    let bytes = bytes_str.parse::<u64>().ok()?;
    // Function name: after the `0xADDR:` marker, up to ` (`.
    let colon = after_arrow.find(": ")?;
    let tail = &after_arrow[colon + 2..];
    let func_end = tail.find(" (").unwrap_or(tail.len());
    let function = tail[..func_end].trim().to_string();
    if function.is_empty() {
        return None;
    }
    Some(MassifFrame { function, bytes })
}

/// raw massif tree line: `<n>: <bytes> 0xADDR: func (file:line)` → frame.
fn parse_raw_massif_frame(line: &str) -> Option<MassifFrame> {
    // Must contain a `0x...:` marker to be an allocation tree frame.
    let marker = line.find("0x")?;
    let colon = line[marker..].find(": ")? + marker;
    // Bytes are the integer token immediately before the `nXX:` / address.
    // Strip a leading `nNN:` index if present, then read the next integer.
    let head = &line[..marker];
    let bytes = head
        .split_whitespace()
        .filter_map(|t| t.replace(',', "").parse::<u64>().ok())
        .next_back()?;
    let tail = &line[colon + 2..];
    let func_end = tail.find(" (").unwrap_or(tail.len());
    let function = tail[..func_end].trim().to_string();
    if function.is_empty() || function.starts_with("0x") {
        return None;
    }
    Some(MassifFrame { function, bytes })
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

    // ------------------------------------------------------------------
    // Profile parsers (Opt-2)
    // ------------------------------------------------------------------

    #[test]
    fn perf_report_self_only_layout() {
        let text = "\
# Samples: 10K of event 'cycles'
#
# Overhead  Command  Shared Object      Symbol
# ........  .......  .................  ......
#
    42.10%  myapp    myapp              [.] compute_hash
    17.55%  myapp    libc-2.31.so       [.] memcpy
     3.20%  myapp    [kernel.kallsyms]  [k] do_syscall
";
        let entries = parse_perf_report(text);
        assert_eq!(entries.len(), 3, "entries: {:?}", entries);
        assert_eq!(entries[0].symbol, "compute_hash");
        assert!((entries[0].self_pct - 42.10).abs() < 1e-9);
        assert_eq!(entries[0].module.as_deref(), Some("myapp"));
        assert_eq!(entries[1].symbol, "memcpy");
        // Sorted by self_pct descending.
        assert!(entries[0].self_pct >= entries[1].self_pct);
    }

    #[test]
    fn perf_report_children_layout() {
        let text = "\
# Children      Self  Command  Shared Object  Symbol
    98.70%    42.10%  myapp    myapp          [.] compute_hash
    50.00%    17.55%  myapp    myapp          [.] inner
";
        let entries = parse_perf_report(text);
        assert_eq!(entries.len(), 2, "entries: {:?}", entries);
        let ch = entries.iter().find(|e| e.symbol == "compute_hash").expect("ch");
        assert!((ch.children_pct - 98.70).abs() < 1e-9);
        assert!((ch.self_pct - 42.10).abs() < 1e-9);
    }

    #[test]
    fn perf_report_ignores_non_data_lines() {
        let text = "random prose\n# a comment\n   \n  not_a_percent foo [.] bar\n";
        let entries = parse_perf_report(text);
        assert!(entries.is_empty(), "entries: {:?}", entries);
    }

    #[test]
    fn folded_stacks_sums_leaf_counts() {
        let text = "\
main;compute;hash_block 120
main;compute;hash_block 30
main;io;read 45
main;io 5
";
        let map = parse_folded_stacks(text);
        assert_eq!(map.get("hash_block").copied(), Some(150));
        assert_eq!(map.get("read").copied(), Some(45));
        assert_eq!(map.get("io").copied(), Some(5));
    }

    #[test]
    fn folded_stacks_skips_malformed() {
        let text = "no_count_here\nframe;leaf notanumber\nframe;ok 7\n";
        let map = parse_folded_stacks(text);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("ok").copied(), Some(7));
    }

    #[test]
    fn massif_raw_peak_and_frames() {
        let text = "\
snapshot=10
mem_heap_B=1048576
mem_heap_extra_B=4096
n2: 1048576 0x4011AA: build_index (index.rs:42)
 n1: 524288 0x4022BB: alloc_buffer (buf.rs:10)
mem_heap_B=2097152
n1: 2097152 0x4033CC: grow_table (table.rs:88)
";
        let s = parse_massif(text);
        assert_eq!(s.peak_heap_bytes, 2_097_152);
        assert!(
            s.top_frames.iter().any(|f| f.function == "grow_table" && f.bytes == 2_097_152),
            "frames: {:?}",
            s.top_frames
        );
        // Sorted descending by bytes.
        assert!(s.top_frames.windows(2).all(|w| w[0].bytes >= w[1].bytes));
    }

    #[test]
    fn massif_ms_print_frames() {
        let text = "\
->50.00% (1,048,576B) 0x4011AA: build_index (index.rs:42)
| ->25.00% (524,288B) 0x4022BB: alloc_buffer (buf.rs:10)
";
        let s = parse_massif(text);
        assert!(
            s.top_frames.iter().any(|f| f.function == "build_index" && f.bytes == 1_048_576),
            "frames: {:?}",
            s.top_frames
        );
    }
}
