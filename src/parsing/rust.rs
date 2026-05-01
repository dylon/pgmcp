//! Rust language backend — uses the native `syn` parser, not tree-sitter.
//!
//! Mirrors the visitor pattern used by `MeTTa-Compiler/tools/gc-root-audit/`
//! (`scanner.rs:1368-1611`): one `Visit<'ast>` impl per concern. Parse errors
//! are silent — the file simply yields empty Vecs.
//!
//! Line numbers come from `proc_macro2::Span::start().line` (requires the
//! `span-locations` feature on `proc-macro2`, set in `Cargo.toml`).

use proc_macro2::Span;
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::{
    self, Visit, visit_expr_call, visit_expr_method_call, visit_item_impl, visit_item_mod,
    visit_type_path,
};
use syn::{
    Ident, ImplItem, Item, ItemConst, ItemEnum, ItemFn, ItemImpl, ItemMod, ItemStatic, ItemStruct,
    ItemTrait, ItemType, UseTree, Visibility,
};

use crate::parsing::LanguageBackend;
use crate::parsing::symbols::{Import, Symbol, SymbolKind, SymbolRefKind, SymbolReference};

/// Static instance returned by `LanguageRegistry::for_language("rust")`.
pub static RUST_BACKEND: RustBackend = RustBackend;

/// Stateless backend; each call constructs a fresh visitor.
pub struct RustBackend;

impl LanguageBackend for RustBackend {
    fn language_name(&self) -> &'static str {
        "rust"
    }

    fn extract_symbols(&self, content: &str) -> Vec<Symbol> {
        let file = match syn::parse_file(content) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let mut v = SymbolVisitor::default();
        v.visit_file(&file);
        v.out
    }

    fn extract_imports(&self, content: &str) -> Vec<Import> {
        let file = match syn::parse_file(content) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let mut out: Vec<Import> = Vec::new();
        for item in &file.items {
            match item {
                Item::Use(item_use) => {
                    let line = span_line(item_use.use_token.span());
                    walk_use_tree(&item_use.tree, Vec::new(), line, &mut out);
                }
                // `mod foo;` and `pub mod foo;` — emit the bare name to match
                // the regex extractor's `RUST_MOD` shape (`raw_path = "foo"`).
                // Skip inline `mod foo { ... }` (`content.is_some()`) since
                // those are inline definitions, not imports of an external file.
                Item::Mod(item_mod) if item_mod.content.is_none() => {
                    let line = span_line(item_mod.mod_token.span());
                    out.push(Import {
                        target_raw: item_mod.ident.to_string(),
                        source_line: line,
                        alias: None,
                    });
                }
                // `extern crate foo;` and `extern crate foo as bar;` — match
                // the regex extractor's `RUST_EXTERN` shape (`raw_path = "foo"`).
                Item::ExternCrate(item_ec) => {
                    let line = span_line(item_ec.extern_token.span());
                    let alias = item_ec.rename.as_ref().map(|(_, name)| name.to_string());
                    out.push(Import {
                        target_raw: item_ec.ident.to_string(),
                        source_line: line,
                        alias,
                    });
                }
                _ => {}
            }
        }
        out
    }

    fn extract_references(&self, content: &str) -> Vec<SymbolReference> {
        let file = match syn::parse_file(content) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let mut v = RefVisitor::default();
        v.visit_file(&file);
        v.out
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// 1-based source line for a `proc_macro2::Span`. Requires the `span-locations`
/// feature on `proc-macro2` (set in `Cargo.toml`).
fn span_line(span: Span) -> u32 {
    span.start().line as u32
}

/// Render a `syn::Type` (or any token stream) as a string.
fn type_to_string<T: ToTokens>(ty: &T) -> String {
    ty.to_token_stream().to_string()
}

/// Map `syn::Visibility` to the canonical visibility strings used in
/// `file_symbols.visibility` (`public` / `module` / `private`).
fn vis_str(v: &Visibility) -> Option<String> {
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

// ============================================================================
// extract_symbols — SymbolVisitor
// ============================================================================

#[derive(Default)]
struct SymbolVisitor {
    out: Vec<Symbol>,
    /// Stack of enclosing `mod` names (for context; not currently used to
    /// build qualified names — `parent_id` resolution is the cron's job).
    mod_stack: Vec<String>,
    /// When inside an `impl Foo { ... }` block, the rendered self type.
    /// Methods emitted inside that block stash this so the cron can resolve
    /// `parent_id` by joining `parent_self_ty == file_symbols.name`.
    current_impl_self: Option<String>,
}

impl SymbolVisitor {
    fn push_symbol(
        &mut self,
        name: String,
        kind: SymbolKind,
        ident_span: Span,
        end_span: Span,
        visibility: Option<String>,
        signature: Option<String>,
    ) {
        self.out.push(Symbol {
            file_id: 0,
            name,
            kind,
            start_line: span_line(ident_span),
            end_line: span_line(end_span).max(span_line(ident_span)),
            parent_id: None,
            visibility,
            signature,
        });
    }
}

impl<'ast> Visit<'ast> for SymbolVisitor {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        let signature = Some(type_to_string(&node.sig));
        self.push_symbol(
            node.sig.ident.to_string(),
            SymbolKind::Function,
            node.sig.ident.span(),
            node.block.brace_token.span.close().span(),
            vis_str(&node.vis),
            signature,
        );
        visit::visit_item_fn(self, node);
    }

    fn visit_item_struct(&mut self, node: &'ast ItemStruct) {
        let header = type_to_string(&node.ident);
        self.push_symbol(
            node.ident.to_string(),
            SymbolKind::Struct,
            node.ident.span(),
            node.span(),
            vis_str(&node.vis),
            Some(header),
        );
        visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast ItemEnum) {
        let header = type_to_string(&node.ident);
        self.push_symbol(
            node.ident.to_string(),
            SymbolKind::Enum,
            node.ident.span(),
            node.brace_token.span.close().span(),
            vis_str(&node.vis),
            Some(header),
        );
        visit::visit_item_enum(self, node);
    }

    fn visit_item_trait(&mut self, node: &'ast ItemTrait) {
        let header = type_to_string(&node.ident);
        self.push_symbol(
            node.ident.to_string(),
            SymbolKind::Trait,
            node.ident.span(),
            node.brace_token.span.close().span(),
            vis_str(&node.vis),
            Some(header),
        );
        visit::visit_item_trait(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        // Stash the receiver type so methods inside the impl block carry
        // it as their parent context. We don't emit a Symbol for the impl
        // block itself.
        let saved = self.current_impl_self.take();
        self.current_impl_self = Some(type_to_string(&*node.self_ty));

        // Emit each method as Function. Visibility on impl items is the
        // method's visibility (defaulted to inherited if absent).
        for item in &node.items {
            if let ImplItem::Fn(method) = item {
                let signature = Some(type_to_string(&method.sig));
                self.push_symbol(
                    method.sig.ident.to_string(),
                    SymbolKind::Function,
                    method.sig.ident.span(),
                    method.block.brace_token.span.close().span(),
                    vis_str(&method.vis),
                    signature,
                );
            }
        }
        // Restore the previous impl context (handles nested-via-mod cases).
        visit_item_impl(self, node);
        self.current_impl_self = saved;
    }

    fn visit_item_const(&mut self, node: &'ast ItemConst) {
        self.push_symbol(
            node.ident.to_string(),
            SymbolKind::Const,
            node.ident.span(),
            node.span(),
            vis_str(&node.vis),
            Some(type_to_string(&node.ty)),
        );
        visit::visit_item_const(self, node);
    }

    fn visit_item_static(&mut self, node: &'ast ItemStatic) {
        self.push_symbol(
            node.ident.to_string(),
            SymbolKind::Const,
            node.ident.span(),
            node.span(),
            vis_str(&node.vis),
            Some(type_to_string(&node.ty)),
        );
        visit::visit_item_static(self, node);
    }

    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        let name = node.ident.to_string();
        self.push_symbol(
            name.clone(),
            SymbolKind::Module,
            node.ident.span(),
            node.span(),
            vis_str(&node.vis),
            None,
        );
        self.mod_stack.push(name);
        visit_item_mod(self, node);
        self.mod_stack.pop();
    }

    fn visit_item_type(&mut self, node: &'ast ItemType) {
        self.push_symbol(
            node.ident.to_string(),
            SymbolKind::Other,
            node.ident.span(),
            node.span(),
            vis_str(&node.vis),
            Some(type_to_string(&node.ty)),
        );
        visit::visit_item_type(self, node);
    }

    // Use, ExternCrate, Macro: skip — imports are handled separately, macros
    // are opaque to our extractor.
}

// ============================================================================
// extract_imports — UseTree walker
// ============================================================================

fn ident_to_string(ident: &Ident) -> String {
    ident.to_string()
}

fn walk_use_tree(tree: &UseTree, prefix: Vec<String>, line: u32, out: &mut Vec<Import>) {
    match tree {
        UseTree::Path(p) => {
            let mut next = prefix;
            next.push(ident_to_string(&p.ident));
            walk_use_tree(&p.tree, next, line, out);
        }
        UseTree::Name(n) => {
            let mut path = prefix;
            path.push(ident_to_string(&n.ident));
            out.push(Import {
                target_raw: path.join("::"),
                source_line: line,
                alias: None,
            });
        }
        UseTree::Rename(r) => {
            let mut path = prefix;
            path.push(ident_to_string(&r.ident));
            out.push(Import {
                target_raw: path.join("::"),
                source_line: line,
                alias: Some(ident_to_string(&r.rename)),
            });
        }
        UseTree::Glob(_) => {
            let mut path = prefix;
            path.push("*".to_string());
            out.push(Import {
                target_raw: path.join("::"),
                source_line: line,
                alias: None,
            });
        }
        UseTree::Group(g) => {
            for child in &g.items {
                walk_use_tree(child, prefix.clone(), line, out);
            }
        }
    }
}

// ============================================================================
// extract_references — RefVisitor
// ============================================================================

#[derive(Default)]
struct RefVisitor {
    out: Vec<SymbolReference>,
}

impl RefVisitor {
    fn push(&mut self, target_raw: String, ref_kind: SymbolRefKind, span: Span) {
        self.out.push(SymbolReference {
            source_file_id: 0,
            source_symbol_id: None,
            target_file_id: None,
            target_symbol_id: None,
            target_raw,
            ref_kind,
            source_line: span_line(span),
        });
    }
}

impl<'ast> Visit<'ast> for RefVisitor {
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        // The function expression of an ExprCall is the call target.
        if let syn::Expr::Path(p) = &*node.func
            && let Some(seg) = p.path.segments.last()
        {
            self.push(seg.ident.to_string(), SymbolRefKind::Call, seg.ident.span());
        }
        visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.push(
            node.method.to_string(),
            SymbolRefKind::Call,
            node.method.span(),
        );
        visit_expr_method_call(self, node);
    }

    fn visit_type_path(&mut self, node: &'ast syn::TypePath) {
        if let Some(seg) = node.path.segments.last() {
            self.push(
                seg.ident.to_string(),
                SymbolRefKind::TypeUse,
                seg.ident.span(),
            );
        }
        visit_type_path(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        // For `impl Trait for Type`, emit Inherit on Trait and Impl on Type.
        // For `impl Type`, emit only Impl on Type.
        if let Some((_, trait_path, _)) = &node.trait_
            && let Some(seg) = trait_path.segments.last()
        {
            self.push(
                seg.ident.to_string(),
                SymbolRefKind::Inherit,
                seg.ident.span(),
            );
        }
        // Self type
        if let syn::Type::Path(tp) = &*node.self_ty
            && let Some(seg) = tp.path.segments.last()
        {
            self.push(seg.ident.to_string(), SymbolRefKind::Impl, seg.ident.span());
        }
        visit_item_impl(self, node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsing::symbols::SymbolKind;

    const SAMPLE: &str = r#"
use std::collections::{HashMap, HashSet};
use crate::foo::bar as renamed;
use other::*;

pub fn free_function(x: i32) -> bool {
    HashMap::<String, i32>::new();
    helper(x)
}

pub(crate) struct MyStruct {
    field: i32,
}

pub enum Status { Ok, Err }

pub trait Processor {
    fn process(&self) -> i32;
}

impl Processor for MyStruct {
    fn process(&self) -> i32 {
        self.field
    }
}

pub const MAX_SIZE: usize = 1024;

mod inner {
    fn helper(x: i32) -> bool {
        x > 0
    }
}
"#;

    #[test]
    fn extract_symbols_returns_expected_kinds() {
        let syms = RUST_BACKEND.extract_symbols(SAMPLE);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"free_function"),
            "free_function: {:?}",
            names
        );
        assert!(names.contains(&"MyStruct"));
        assert!(names.contains(&"Status"));
        assert!(names.contains(&"Processor"));
        assert!(names.contains(&"process"), "method process: {:?}", names);
        assert!(names.contains(&"MAX_SIZE"));
        assert!(names.contains(&"inner"), "module inner: {:?}", names);
        assert!(names.contains(&"helper"), "nested helper: {:?}", names);
        // At least 8 symbols.
        assert!(
            syms.len() >= 8,
            "symbol count: {} ({:?})",
            syms.len(),
            names
        );
    }

    #[test]
    fn extract_symbols_visibility_mapping() {
        let syms = RUST_BACKEND.extract_symbols(SAMPLE);
        let by_name = |n: &str| syms.iter().find(|s| s.name == n).cloned();
        assert_eq!(
            by_name("free_function").and_then(|s| s.visibility),
            Some("public".into())
        );
        assert_eq!(
            by_name("MyStruct").and_then(|s| s.visibility),
            Some("module".into())
        );
        assert_eq!(
            by_name("helper").and_then(|s| s.visibility),
            Some("private".into())
        );
    }

    #[test]
    fn extract_symbols_emits_kinds_for_const_and_module() {
        let syms = RUST_BACKEND.extract_symbols(SAMPLE);
        let max_size = syms.iter().find(|s| s.name == "MAX_SIZE").expect("const");
        assert_eq!(max_size.kind, SymbolKind::Const);
        let inner = syms.iter().find(|s| s.name == "inner").expect("module");
        assert_eq!(inner.kind, SymbolKind::Module);
    }

    #[test]
    fn extract_imports_flattens_groups() {
        let imports = RUST_BACKEND.extract_imports(SAMPLE);
        let targets: Vec<&str> = imports.iter().map(|i| i.target_raw.as_str()).collect();
        // `use std::collections::{HashMap, HashSet};` flattens to two imports.
        assert!(targets.contains(&"std::collections::HashMap"));
        assert!(targets.contains(&"std::collections::HashSet"));
        // `use crate::foo::bar as renamed;` → `crate::foo::bar` with alias.
        let renamed = imports
            .iter()
            .find(|i| i.target_raw == "crate::foo::bar")
            .expect("renamed import");
        assert_eq!(renamed.alias.as_deref(), Some("renamed"));
        // Glob: `use other::*;`
        assert!(targets.contains(&"other::*"));
    }

    #[test]
    fn extract_imports_emits_mod_declarations() {
        // `mod inner { ... }` (inline, has content) is skipped.
        // `mod sibling;` (file-bound, no content) is emitted.
        let src = r#"
mod sibling;
pub mod public_sibling;
mod inline_module {
    fn helper() {}
}
"#;
        let imports = RUST_BACKEND.extract_imports(src);
        let targets: Vec<&str> = imports.iter().map(|i| i.target_raw.as_str()).collect();
        assert!(
            targets.contains(&"sibling"),
            "missing `sibling`: {:?}",
            targets
        );
        assert!(
            targets.contains(&"public_sibling"),
            "missing `public_sibling`: {:?}",
            targets
        );
        // Inline mod is NOT emitted as an import (it's a definition, not a file ref).
        assert!(
            !targets.contains(&"inline_module"),
            "inline_module should be skipped: {:?}",
            targets
        );
    }

    #[test]
    fn extract_imports_emits_extern_crate() {
        let src = r#"
extern crate serde;
extern crate proc_macro2 as pm2;
"#;
        let imports = RUST_BACKEND.extract_imports(src);
        let serde = imports
            .iter()
            .find(|i| i.target_raw == "serde")
            .expect("serde import");
        assert!(serde.alias.is_none());
        let pm2 = imports
            .iter()
            .find(|i| i.target_raw == "proc_macro2")
            .expect("proc_macro2 import");
        assert_eq!(pm2.alias.as_deref(), Some("pm2"));
    }

    #[test]
    fn extract_references_includes_type_and_calls() {
        let refs = RUST_BACKEND.extract_references(SAMPLE);
        let kinds_targets: Vec<(&str, SymbolRefKind)> = refs
            .iter()
            .map(|r| (r.target_raw.as_str(), r.ref_kind))
            .collect();
        // helper() call inside free_function
        assert!(
            kinds_targets
                .iter()
                .any(|(t, k)| *t == "helper" && *k == SymbolRefKind::Call),
            "helper call missing: {:?}",
            kinds_targets
        );
        // impl Processor for MyStruct → Inherit + Impl
        assert!(kinds_targets.contains(&("Processor", SymbolRefKind::Inherit)));
        assert!(kinds_targets.contains(&("MyStruct", SymbolRefKind::Impl)));
    }

    #[test]
    fn parse_error_yields_empty_vecs() {
        let bogus = "this is not valid Rust { syntax";
        assert!(RUST_BACKEND.extract_symbols(bogus).is_empty());
        assert!(RUST_BACKEND.extract_imports(bogus).is_empty());
        assert!(RUST_BACKEND.extract_references(bogus).is_empty());
    }

    #[test]
    fn language_name_is_rust() {
        assert_eq!(RUST_BACKEND.language_name(), "rust");
    }
}
