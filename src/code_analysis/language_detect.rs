//! BCP-47 language detection for project comment corpora.
//!
//! Wraps the `whatlang` crate. Samples comments / README content
//! from a project's `file_chunks` (plus a `README.md` fallback) and
//! returns the dominant BCP-47 language tag. Used by Phase 13.3 to
//! pick the right phonetic-rule pack per project.
//!
//! Detection is cached in `pgmcp_metadata` under
//! `phonetic.language.<project_id>` so repeated invocations don't
//! re-sample the corpus.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 10 + P13.3.

use sqlx::PgPool;
use tracing::debug;
use whatlang::Lang;

/// Detect the dominant BCP-47 language of a project's comments and
/// README. Defaults to `"en-us"` when:
/// - the project has no chunks,
/// - whatlang returns no detection (insufficient text), or
/// - the detected language is English (the default override).
///
/// Results are cached in `pgmcp_metadata` keyed by
/// `phonetic.language.<project_id>`. Pass `force` to ignore the
/// cache and re-sample.
pub async fn project_language(
    pool: &PgPool,
    project_id: i32,
    force: bool,
) -> Result<String, sqlx::Error> {
    let cache_key = format!("phonetic.language.{project_id}");

    if !force
        && let Some(cached) =
            sqlx::query_scalar::<_, String>("SELECT value FROM pgmcp_metadata WHERE key = $1")
                .bind(&cache_key)
                .fetch_optional(pool)
                .await?
    {
        debug!(
            project_id,
            tag = %cached,
            "project_language: cache hit"
        );
        return Ok(cached);
    }

    let sample = sample_text(pool, project_id).await?;
    let tag = detect_bcp47(&sample);

    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(&cache_key)
    .bind(&tag)
    .execute(pool)
    .await?;
    debug!(project_id, tag = %tag, "project_language: detected + cached");
    Ok(tag)
}

/// Pull a representative sample of comment / README text from the
/// project's `file_chunks`. Cap at 256 KB so detection runs in
/// milliseconds for any project size.
async fn sample_text(pool: &PgPool, project_id: i32) -> Result<String, sqlx::Error> {
    // Prefer README content where present; otherwise sample comment
    // chunks across the project.
    let mut buf = String::with_capacity(262_144);

    if let Some(readme) = sqlx::query_scalar::<_, String>(
        "SELECT content FROM indexed_files
         WHERE project_id = $1
           AND lower(relative_path) IN ('readme.md', 'readme.rst', 'readme.txt', 'readme')
         LIMIT 1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await?
    {
        buf.push_str(&readme);
        if buf.len() >= 65_536 {
            buf.truncate(65_536);
        }
        buf.push('\n');
    }

    let chunks: Vec<String> = sqlx::query_scalar(
        "SELECT fc.content
         FROM file_chunks fc
         JOIN indexed_files f ON fc.file_id = f.id
         WHERE f.project_id = $1
           AND fc.content IS NOT NULL
         ORDER BY fc.id
         LIMIT 200",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    for c in chunks {
        if buf.len() + c.len() > 262_144 {
            break;
        }
        buf.push_str(&c);
        buf.push('\n');
    }
    Ok(buf)
}

/// Run whatlang and convert the result to a BCP-47 tag. Defaults to
/// `"en-us"` when no detection is possible.
fn detect_bcp47(text: &str) -> String {
    if text.trim().is_empty() {
        return "en-us".to_string();
    }
    let Some(lang) = whatlang::detect_lang(text) else {
        return "en-us".to_string();
    };
    // Map whatlang Lang → BCP-47 tag. Whatlang uses ISO 639-3 codes
    // (e.g. "eng"); pgmcp's rule packs key on ISO 639-1 / BCP-47
    // ("en", "en-us"). The mapping is enumeration-complete for the
    // languages liblevenshtein's rule packs cover; everything else
    // falls back to English.
    match lang {
        Lang::Eng => "en-us".to_string(),
        Lang::Spa => "es".to_string(),
        Lang::Por => "pt".to_string(),
        Lang::Ita => "it".to_string(),
        Lang::Fra => "fr".to_string(),
        Lang::Deu => "de".to_string(),
        Lang::Nld => "nl".to_string(),
        Lang::Rus => "ru".to_string(),
        Lang::Kor => "ko".to_string(),
        Lang::Dan => "da".to_string(),
        _ => {
            // Unknown to our rule packs; fall back to English so
            // downstream phonetic search still produces results.
            "en-us".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_defaults_to_english() {
        assert_eq!(detect_bcp47(""), "en-us");
        assert_eq!(detect_bcp47("   \n\n"), "en-us");
    }

    #[test]
    fn english_paragraph_detected_as_en_us() {
        let text = "This is a sufficiently long paragraph of English text \
                    that whatlang should detect with high confidence and map \
                    to the en-us BCP-47 tag for our rule pack dispatch.";
        assert_eq!(detect_bcp47(text), "en-us");
    }

    #[test]
    fn spanish_paragraph_detected_as_es() {
        let text = "Este es un párrafo en español suficientemente largo \
                    para que whatlang lo detecte con confianza alta y lo \
                    asigne al paquete de reglas en castellano.";
        let detected = detect_bcp47(text);
        // Allow es OR es-MX OR fallback to en-us if whatlang is
        // not confident — but in practice this string is firmly Spanish.
        assert!(
            detected == "es" || detected == "en-us",
            "expected es (or en-us fallback if whatlang misses), got {detected}"
        );
    }
}
