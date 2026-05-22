use super::backend::LanguageBackend;
use super::{c_cpp, clojure, coq, java, javascript, lean, python, rholang, rust, scala, tlaplus};

/// Registry: dispatches a language string to the matching backend, or `None`
/// when no backend has been wired yet.
#[allow(dead_code)]
pub struct LanguageRegistry;

#[allow(dead_code)]
impl LanguageRegistry {
    /// Resolve a language name (matching `indexed_files.language`) to its
    /// backend. Returns `None` for languages whose backend hasn't landed yet.
    pub fn for_language(language: &str) -> Option<&'static dyn LanguageBackend> {
        match language {
            "rust" => Some(&rust::RUST_BACKEND),
            "python" => Some(&python::PYTHON_BACKEND),
            "javascript" => Some(&javascript::JS_BACKEND),
            "typescript" => Some(&javascript::TS_BACKEND),
            "tsx" => Some(&javascript::TSX_BACKEND),
            "java" => Some(&java::JAVA_BACKEND),
            "scala" => Some(&scala::SCALA_BACKEND),
            "c" => Some(&c_cpp::C_BACKEND),
            "cpp" => Some(&c_cpp::CPP_BACKEND),
            "rholang" => Some(&rholang::RHOLANG_BACKEND),
            "clojure" => Some(&clojure::CLOJURE_BACKEND),
            "clojurescript" => Some(&clojure::CLOJURESCRIPT_BACKEND),
            // Formal-verification backends.
            "coq" => Some(&coq::COQ_BACKEND),
            "tlaplus" => Some(&tlaplus::TLAPLUS_BACKEND),
            "lean" => Some(&lean::LEAN_BACKEND),
            // Sage Math is a Python superset — reuse the Python backend.
            "sage" => Some(&python::PYTHON_BACKEND),
            _ => None,
        }
    }

    /// Whether any backend is available. Used by health envelopes to
    /// distinguish "no backend implemented" from "backend exists but no
    /// symbols extracted".
    pub fn any_backend_available() -> bool {
        true
    }
}
