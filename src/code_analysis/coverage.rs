//! Test-coverage report parsers (graph-roadmap Phase 4.4).
//!
//! Parses the three dominant coverage-report formats into per-file line
//! counts, with no XML dependency (the reports are regular enough for
//! line/regex extraction):
//!
//! - **lcov** (`.info`; Rust `grcov`/`tarpaulin`, JS `istanbul`, C `gcov`):
//!   `SF:<path>` … `DA:<line>,<hits>` … `end_of_record`.
//! - **Cobertura** XML (Python `coverage.py xml`, many CI tools):
//!   `<class filename="…">` with `<line number=".." hits=".."/>`.
//! - **JaCoCo** XML (JVM): `<sourcefile name="…">` with
//!   `<counter type="LINE" missed=".." covered=".."/>`.
//!
//! Pure + dependency-free; the tool reads the (already-indexed) report content
//! and feeds it here. `detect_and_parse` sniffs the format from the content.

/// Per-file line coverage parsed from a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCoverage {
    /// File path exactly as written in the report (caller normalizes/matches).
    pub path: String,
    pub lines_total: u32,
    pub lines_covered: u32,
}

/// Which report format produced a parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageFormat {
    Lcov,
    Cobertura,
    Jacoco,
}

impl CoverageFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            CoverageFormat::Lcov => "lcov",
            CoverageFormat::Cobertura => "cobertura",
            CoverageFormat::Jacoco => "jacoco",
        }
    }
}

/// Sniff the format from `content` and parse it. Returns the detected format +
/// per-file coverage, or `None` if nothing recognizable was found.
pub fn detect_and_parse(content: &str) -> Option<(CoverageFormat, Vec<FileCoverage>)> {
    // lcov: line-oriented, has `SF:` records.
    if content.contains("\nSF:") || content.starts_with("SF:") || content.starts_with("TN:") {
        let rows = parse_lcov(content);
        if !rows.is_empty() {
            return Some((CoverageFormat::Lcov, rows));
        }
    }
    // JaCoCo: `<sourcefile` + LINE counters.
    if content.contains("<sourcefile") && content.contains("type=\"LINE\"") {
        let rows = parse_jacoco(content);
        if !rows.is_empty() {
            return Some((CoverageFormat::Jacoco, rows));
        }
    }
    // Cobertura: `<class filename=` + `<line ... hits=`.
    if content.contains("<class") && content.contains("filename=") {
        let rows = parse_cobertura(content);
        if !rows.is_empty() {
            return Some((CoverageFormat::Cobertura, rows));
        }
    }
    None
}

/// Parse an lcov `.info` report. Multiple records for the same file are merged.
pub fn parse_lcov(content: &str) -> Vec<FileCoverage> {
    let mut out: Vec<FileCoverage> = Vec::new();
    let mut cur_path: Option<String> = None;
    let mut total = 0u32;
    let mut covered = 0u32;
    for line in content.lines() {
        let line = line.trim();
        if let Some(p) = line.strip_prefix("SF:") {
            cur_path = Some(p.to_string());
            total = 0;
            covered = 0;
        } else if let Some(rest) = line.strip_prefix("DA:") {
            // DA:<line>,<hits>[,<checksum>]
            let mut parts = rest.split(',');
            let _ln = parts.next();
            if let Some(hits) = parts.next().and_then(|h| h.parse::<i64>().ok()) {
                total += 1;
                if hits > 0 {
                    covered += 1;
                }
            }
        } else if line == "end_of_record" {
            if let Some(p) = cur_path.take() {
                push_merge(&mut out, p, total, covered);
            }
            total = 0;
            covered = 0;
        }
    }
    // A trailing record with no end_of_record marker.
    if let Some(p) = cur_path.take()
        && total > 0
    {
        push_merge(&mut out, p, total, covered);
    }
    out
}

/// Parse a Cobertura XML report (regex-free, attribute-scan based).
pub fn parse_cobertura(content: &str) -> Vec<FileCoverage> {
    let mut out: Vec<FileCoverage> = Vec::new();
    // Split on `<class` so each chunk holds one class's lines until the next.
    for chunk in content.split("<class").skip(1) {
        let Some(path) = attr_value(chunk, "filename=") else {
            continue;
        };
        // Only the lines before this class's `</class>` belong to it.
        let body = chunk.split("</class>").next().unwrap_or(chunk);
        let mut total = 0u32;
        let mut covered = 0u32;
        for line_el in body.split("<line").skip(1) {
            if let Some(hits) = attr_value(line_el, "hits=").and_then(|h| h.parse::<i64>().ok()) {
                total += 1;
                if hits > 0 {
                    covered += 1;
                }
            }
        }
        if total > 0 {
            push_merge(&mut out, path, total, covered);
        }
    }
    out
}

/// Parse a JaCoCo XML report using the per-sourcefile LINE counter.
pub fn parse_jacoco(content: &str) -> Vec<FileCoverage> {
    let mut out: Vec<FileCoverage> = Vec::new();
    for chunk in content.split("<sourcefile").skip(1) {
        let Some(name) = attr_value(chunk, "name=") else {
            continue;
        };
        let body = chunk.split("</sourcefile>").next().unwrap_or(chunk);
        // Find the `<counter type="LINE" missed=".." covered=".."/>`.
        for counter in body.split("<counter").skip(1) {
            if attr_value(counter, "type=").as_deref() == Some("LINE") {
                let missed = attr_value(counter, "missed=")
                    .and_then(|m| m.parse::<u32>().ok())
                    .unwrap_or(0);
                let cov = attr_value(counter, "covered=")
                    .and_then(|c| c.parse::<u32>().ok())
                    .unwrap_or(0);
                if missed + cov > 0 {
                    push_merge(&mut out, name, missed + cov, cov);
                }
                break;
            }
        }
    }
    out
}

/// Extract the value of `attr` (e.g. `"filename="`) from the start of an XML
/// fragment: the text between the first `"`…`"` following `attr`.
fn attr_value(fragment: &str, attr: &str) -> Option<String> {
    let i = fragment.find(attr)? + attr.len();
    let rest = &fragment[i..];
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn push_merge(out: &mut Vec<FileCoverage>, path: String, total: u32, covered: u32) {
    if let Some(existing) = out.iter_mut().find(|f| f.path == path) {
        existing.lines_total += total;
        existing.lines_covered += covered;
    } else {
        out.push(FileCoverage {
            path,
            lines_total: total,
            lines_covered: covered,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcov_counts_hits() {
        let src = "TN:\nSF:src/foo.rs\nDA:1,5\nDA:2,0\nDA:3,1\nLF:3\nLH:2\nend_of_record\n";
        let (fmt, rows) = detect_and_parse(src).expect("lcov");
        assert_eq!(fmt, CoverageFormat::Lcov);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "src/foo.rs");
        assert_eq!(rows[0].lines_total, 3);
        assert_eq!(rows[0].lines_covered, 2);
    }

    #[test]
    fn cobertura_counts_line_hits() {
        let src = r#"<coverage line-rate="0.5">
          <packages><package><classes>
            <class filename="app/x.py">
              <lines><line number="1" hits="3"/><line number="2" hits="0"/></lines>
            </class>
          </classes></package></packages>
        </coverage>"#;
        let (fmt, rows) = detect_and_parse(src).expect("cobertura");
        assert_eq!(fmt, CoverageFormat::Cobertura);
        assert_eq!(rows[0].path, "app/x.py");
        assert_eq!(rows[0].lines_total, 2);
        assert_eq!(rows[0].lines_covered, 1);
    }

    #[test]
    fn jacoco_uses_line_counter() {
        let src = r#"<report><package name="com/x">
          <sourcefile name="Foo.java">
            <counter type="INSTRUCTION" missed="10" covered="20"/>
            <counter type="LINE" missed="4" covered="6"/>
          </sourcefile>
        </package></report>"#;
        let (fmt, rows) = detect_and_parse(src).expect("jacoco");
        assert_eq!(fmt, CoverageFormat::Jacoco);
        assert_eq!(rows[0].path, "Foo.java");
        assert_eq!(rows[0].lines_total, 10);
        assert_eq!(rows[0].lines_covered, 6);
    }

    #[test]
    fn unrecognized_is_none() {
        assert!(detect_and_parse("just some text").is_none());
    }
}
