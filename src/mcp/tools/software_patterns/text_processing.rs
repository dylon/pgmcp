//! HTML stripping + Postgres-NUL sanitization + HTML-entity decoder —
//! extracted from `tool_software_patterns.rs` as part of the D.2
//! god-file split.

use regex::Regex;

/// PostgreSQL TEXT columns reject `\0` even though it's a valid UTF-8 code
/// point. Some upstream sources (corrupt HTML, content sniffed binary,
/// editor artefacts) ship NUL bytes inside otherwise-text bodies. Strip
/// them at every ingress so downstream `INSERT … (TEXT)` never fails with
/// `invalid byte sequence for encoding "UTF8": 0x00`.
pub(super) fn sanitize_text_for_postgres(input: &str) -> String {
    if input.contains('\0') {
        input.replace('\0', "")
    } else {
        input.to_string()
    }
}

pub(super) fn html_to_text(input: &str) -> String {
    let mut text = input.to_string();
    for pat in [
        r"(?is)<script[^>]*>.*?</script>",
        r"(?is)<style[^>]*>.*?</style>",
        r"(?is)<noscript[^>]*>.*?</noscript>",
    ] {
        let re = Regex::new(pat).unwrap();
        text = re.replace_all(&text, "\n").into_owned();
    }
    let block =
        Regex::new(r"(?i)</?(p|div|section|article|h[1-6]|li|ul|ol|table|tr|br)[^>]*>").unwrap();
    text = block.replace_all(&text, "\n").into_owned();
    let tags = Regex::new(r"(?is)<[^>]+>").unwrap();
    text = tags.replace_all(&text, " ").into_owned();
    decode_entities(&text)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn decode_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}
