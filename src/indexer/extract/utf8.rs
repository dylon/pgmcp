use std::path::Path;

use super::{ExtractError, ExtractOptions, Extracted, normalize::normalize_extracted_text};

/// UTF-8 passthrough for plain-text document formats (rst, bibtex, txt).
/// Strips a UTF-8 BOM if present and applies the same normalization pass
/// used by the binary-document extractors so the storage layer sees a
/// uniform form regardless of source encoding details.
pub fn read(path: &Path, opts: &ExtractOptions) -> Result<Option<Extracted>, ExtractError> {
    let bytes = std::fs::read(path).map_err(ExtractError::Io)?;
    let source_size_bytes = bytes.len() as u64;
    let max = opts.max_extracted_bytes;
    let (data, truncated) = if bytes.len() > max {
        (&bytes[..max], true)
    } else {
        (&bytes[..], false)
    };
    let stripped = strip_utf8_bom(data);
    let lossy = String::from_utf8_lossy(stripped);
    let normalized = normalize_extracted_text(&lossy);
    Ok(Some(Extracted {
        text: normalized,
        truncated,
        source_size_bytes,
    }))
}

fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &bytes[3..]
    } else {
        bytes
    }
}
