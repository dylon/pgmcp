//! Declarative taint specification (graph-roadmap Phase 2.1): classifies callee
//! names into taint **sources** (attacker-controllable input), **sinks**
//! (dangerous consumption), and **sanitizers** (taint-clearing). Used by
//! per-language `extract_dataflow` implementations to tag flow nodes; the
//! reachability rigor comes from the engine (`taint_dataflow`), which only
//! reports when a source actually *flows into* a sink. Patterns are substring
//! matches on the callee path, chosen to work across Rust/Python/JS/Go/Java.

/// Classify a callee path as a taint source, returning the source category.
pub fn source_kind(callee: &str) -> Option<&'static str> {
    let c = callee;
    if c.contains("env::var")
        || c.contains("getenv")
        || c.contains("os.environ")
        || c.contains("process.env")
    {
        return Some("env");
    }
    if c.contains("env::args")
        || c.contains("sys.argv")
        || c.contains("process.argv")
        || c.contains("os.Args")
    {
        return Some("argv");
    }
    if c.contains("io::stdin")
        || c.contains("sys.stdin")
        || c.contains("read_line")
        || c.contains("readLine")
    {
        return Some("stdin");
    }
    // Web-framework request inputs (body/query/params/headers/form).
    if c.contains("req.body")
        || c.contains("req.query")
        || c.contains("req.params")
        || c.contains("request.body")
        || c.contains("request.form")
        || c.contains("request.args")
        || c.contains("query_string")
        || c.contains("form_data")
        || c.contains("FormData")
    {
        return Some("request");
    }
    None
}

/// Classify a callee path as a dangerous sink, returning the sink category.
pub fn sink_kind(callee: &str) -> Option<&'static str> {
    let c = callee;
    if c.contains("Command::new")
        || c.contains("process::Command")
        || c.contains("os.system")
        || c.contains("subprocess.")
        || c.contains("Popen")
        || c.contains("Runtime.exec")
        || c.contains("child_process")
        || c == "system"
        || c == "execvp"
        || c == "execve"
        || c.ends_with("::exec")
    {
        return Some("command");
    }
    if c.contains("sqlx::query")
        || c.contains("cursor.execute")
        || c.contains("executeQuery")
        || c.contains("rawQuery")
        || c.ends_with(".execute")
        || c.ends_with(".query")
        || c.contains("db.query")
    {
        return Some("sql");
    }
    if c == "eval"
        || c.ends_with("::eval")
        || c.ends_with(".eval")
        || c.contains("Function(")
        || c.contains("vm.run")
    {
        return Some("eval");
    }
    if c.contains("pickle.load")
        || c.contains("yaml.load")
        || c.contains("ObjectInputStream")
        || c.contains("Marshal.load")
        || c.contains("unserialize")
    {
        return Some("deserialize");
    }
    if c.contains("File::open")
        || c.contains("fs::read")
        || c.contains("fs::write")
        || c.contains("fopen")
        || c.contains("readFile")
        || c.contains("sendfile")
    {
        return Some("path");
    }
    None
}

/// `true` when the callee clears taint (escaping, validation, parameterization,
/// numeric parsing, canonicalization). Taint never propagates out of a value
/// passed through one of these.
pub fn is_sanitizer(callee: &str) -> bool {
    let c = callee;
    c.contains("escape")
        || c.contains("sanitize")
        || c.contains("quote")
        || c.contains("htmlspecialchars")
        || c.contains("parameterize")
        || c.contains("canonicalize")
        || c.contains("validate")
        || c.contains("parse::<")
        || c.contains("parseInt")
        || c.contains("parseFloat")
        || c.contains("to_int")
        || c.contains(".bind(")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_classified() {
        assert_eq!(source_kind("std::env::var"), Some("env"));
        assert_eq!(source_kind("os.environ.get"), Some("env"));
        assert_eq!(source_kind("std::env::args"), Some("argv"));
        assert_eq!(source_kind("sys.argv"), Some("argv"));
        assert_eq!(source_kind("req.query"), Some("request"));
        assert_eq!(source_kind("compute_sum"), None);
    }

    #[test]
    fn sinks_classified() {
        assert_eq!(sink_kind("std::process::Command::new"), Some("command"));
        assert_eq!(sink_kind("os.system"), Some("command"));
        assert_eq!(sink_kind("subprocess.run"), Some("command"));
        assert_eq!(sink_kind("sqlx::query"), Some("sql"));
        assert_eq!(sink_kind("cursor.execute"), Some("sql"));
        assert_eq!(sink_kind("eval"), Some("eval"));
        assert_eq!(sink_kind("pickle.loads"), Some("deserialize"));
        assert_eq!(sink_kind("std::fs::read_to_string"), Some("path"));
        assert_eq!(sink_kind("println"), None);
    }

    #[test]
    fn sanitizers_classified() {
        assert!(is_sanitizer("shell_escape"));
        assert!(is_sanitizer("html.escape"));
        assert!(is_sanitizer("input.parse::<i64>"));
        assert!(is_sanitizer("validate_path"));
        assert!(!is_sanitizer("format"));
    }
}
