//! Helper functions shared by the Symbol, Ref, and Complexity visitors
//! in the parent `rust.rs` — extracted as part of the D.2 god-file split.

use proc_macro2::Span;
use quote::ToTokens;
use syn::Visibility;

// ============================================================================
// Helpers
// ============================================================================

/// 1-based source line for a `proc_macro2::Span`. Requires the `span-locations`
/// feature on `proc-macro2` (set in `Cargo.toml`).
pub(super) fn span_line(span: Span) -> u32 {
    span.start().line as u32
}

/// Render a `syn::Type` (or any token stream) as a string.
pub(super) fn type_to_string<T: ToTokens>(ty: &T) -> String {
    ty.to_token_stream().to_string()
}

/// Map `syn::Visibility` to the canonical visibility strings used in
/// `file_symbols.visibility` (`public` / `module` / `private`).
pub(super) fn vis_str(v: &Visibility) -> Option<String> {
    Some(
        match v {
            Visibility::Public(_) => "public",
            // `pub(crate)`, `pub(super)`, `pub(in path::to::module)` — all
            // collapse to "module" since pgmcp's vocabulary does not yet
            // distinguish them.
            Visibility::Restricted(_) => "module",
            Visibility::Inherited => "private",
        }
        .to_string(),
    )
}
