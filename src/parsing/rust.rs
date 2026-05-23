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
    ItemTrait, ItemType, UseTree,
};

#[path = "rust/helpers.rs"]
mod helpers;
#[path = "rust/type_mapper.rs"]
mod type_mapper;
use helpers::*;

use crate::parsing::backend::LanguageBackend;
use crate::parsing::complexity;
use crate::parsing::function_metrics::{
    CognitiveIncrement, CognitiveKind, FunctionMetrics, ScoringInput,
};
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

    fn extract_function_metrics(&self, content: &str) -> Vec<FunctionMetrics> {
        let file = match syn::parse_file(content) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let mut v = ComplexityVisitor::default();
        v.visit_file(&file);
        v.into_metrics()
    }
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
    /// When inside a `trait Foo { ... }` block, the trait's name. Trait methods
    /// inherit the trait name as their scope parent.
    current_trait: Option<String>,
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
            name: name.clone(),
            kind,
            start_line: span_line(ident_span),
            end_line: span_line(end_span).max(span_line(ident_span)),
            parent_id: None,
            visibility,
            signature,
            scope_path: Some(self.compute_scope_path(&name)),
            scope_depth: Some(self.scope_depth()),
            ..Default::default()
        });
    }

    /// Build a fully-populated function-shaped `Symbol`. Used by
    /// `visit_item_fn` / `visit_impl_item_fn` / `visit_trait_item_fn` so the
    /// shadow-ASR fields (`parameters`, `return_type`, `generic_params`,
    /// `effects`, `scope_path`, `scope_depth`) land at the same time as the
    /// basic symbol row.
    #[allow(clippy::too_many_arguments)]
    fn push_function_symbol(
        &mut self,
        name: String,
        sig: &syn::Signature,
        attrs: &[syn::Attribute],
        body_tokens: Option<proc_macro2::TokenStream>,
        ident_span: Span,
        end_span: Span,
        visibility: Option<String>,
    ) {
        let parameters: Vec<crate::parsing::symbols::Parameter> = sig
            .inputs
            .iter()
            .enumerate()
            .map(|(i, arg)| type_mapper::fnarg_to_parameter(arg, i as u32))
            .collect();
        let return_type = Some(type_mapper::return_type_for(&sig.output));
        let generic_params = type_mapper::generics_for(&sig.generics);
        let panic_count = body_tokens
            .as_ref()
            .map(type_mapper::count_body_panics)
            .unwrap_or(0);
        let unsafe_blocks = body_tokens
            .as_ref()
            .map(count_body_unsafe_blocks)
            .unwrap_or(0);
        let effects = type_mapper::effects_for_sig(sig, attrs, panic_count, unsafe_blocks);
        let signature = Some(type_to_string(sig));

        self.out.push(Symbol {
            file_id: 0,
            name: name.clone(),
            kind: SymbolKind::Function,
            start_line: span_line(ident_span),
            end_line: span_line(end_span).max(span_line(ident_span)),
            parent_id: None,
            visibility,
            signature,
            parameters,
            return_type,
            generic_params,
            effects,
            scope_path: Some(self.compute_scope_path(&name)),
            scope_depth: Some(self.scope_depth()),
        });
    }

    /// Build `crate::a::b::name` from the current `mod_stack`, `current_impl_self`,
    /// and `current_trait`. Roots at `crate` because that's the canonical Rust
    /// path prefix and matches what `use` statements expect.
    fn compute_scope_path(&self, name: &str) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(self.mod_stack.len() + 3);
        parts.push("crate".to_string());
        for m in &self.mod_stack {
            parts.push(m.clone());
        }
        if let Some(self_ty) = &self.current_impl_self {
            parts.push(self_ty.clone());
        }
        if let Some(trait_name) = &self.current_trait {
            parts.push(trait_name.clone());
        }
        parts.push(name.to_string());
        parts.join("::")
    }

    /// Depth = number of containing scopes (0 = top-level under the crate root).
    fn scope_depth(&self) -> u32 {
        let mut d = self.mod_stack.len() as u32;
        if self.current_impl_self.is_some() {
            d += 1;
        }
        if self.current_trait.is_some() {
            d += 1;
        }
        d
    }
}

/// Walk a function body's token stream counting `unsafe { ... }` blocks.
/// Mirrors `ComplexityVisitor::visit_expr_unsafe`'s logic but operates on
/// raw tokens so we can derive the count outside of the complexity pass.
fn count_body_unsafe_blocks(stream: &proc_macro2::TokenStream) -> u32 {
    use proc_macro2::{Delimiter, TokenTree};
    let mut count = 0u32;
    let mut prev_was_unsafe = false;
    for tt in stream.clone() {
        match tt {
            TokenTree::Ident(id) if id == "unsafe" => {
                prev_was_unsafe = true;
            }
            TokenTree::Group(g) => {
                if prev_was_unsafe && matches!(g.delimiter(), Delimiter::Brace) {
                    count = count.saturating_add(1);
                }
                prev_was_unsafe = false;
                count = count.saturating_add(count_body_unsafe_blocks(&g.stream()));
            }
            _ => {
                prev_was_unsafe = false;
            }
        }
    }
    count
}

impl<'ast> Visit<'ast> for SymbolVisitor {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        self.push_function_symbol(
            node.sig.ident.to_string(),
            &node.sig,
            &node.attrs,
            Some(node.block.to_token_stream()),
            node.sig.ident.span(),
            node.block.brace_token.span.close().span(),
            vis_str(&node.vis),
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
        let name = node.ident.to_string();
        self.push_symbol(
            name.clone(),
            SymbolKind::Trait,
            node.ident.span(),
            node.brace_token.span.close().span(),
            vis_str(&node.vis),
            Some(header),
        );
        // Stash trait context so method symbols emitted via the recursive
        // visit get the trait's scope path. Trait methods without a default
        // body have `default = None`; we still emit them as Function rows
        // because they're part of the trait's surface (consumed by
        // public_api_surface, semver_break_audit, etc.).
        let saved_trait = self.current_trait.take();
        self.current_trait = Some(name);
        for item in &node.items {
            if let syn::TraitItem::Fn(method) = item {
                let end_span = method
                    .default
                    .as_ref()
                    .map(|b| b.brace_token.span.close().span())
                    .unwrap_or_else(|| method.sig.ident.span());
                let body_tokens = method.default.as_ref().map(|b| b.to_token_stream());
                self.push_function_symbol(
                    method.sig.ident.to_string(),
                    &method.sig,
                    &method.attrs,
                    body_tokens,
                    method.sig.ident.span(),
                    end_span,
                    None,
                );
            }
        }
        visit::visit_item_trait(self, node);
        self.current_trait = saved_trait;
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
                self.push_function_symbol(
                    method.sig.ident.to_string(),
                    &method.sig,
                    &method.attrs,
                    Some(method.block.to_token_stream()),
                    method.sig.ident.span(),
                    method.block.brace_token.span.close().span(),
                    vis_str(&method.vis),
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

// ============================================================================
// extract_function_metrics — ComplexityVisitor (CC / Cognitive / Halstead /
// NPath / panic-paths / unsafe-blocks)
// ============================================================================

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use std::collections::HashMap;

/// Static set of Rust operator/punctuation/keyword tokens (η1 universe).
/// Anything not in this set is classified as an operand.
const RUST_OPERATOR_TOKENS: &[&str] = &[
    // Arithmetic
    "+",
    "-",
    "*",
    "/",
    "%",
    // Comparison
    "==",
    "!=",
    "<",
    ">",
    "<=",
    ">=",
    // Logical
    "&&",
    "||",
    "!",
    // Bitwise
    "&",
    "|",
    "^",
    "<<",
    ">>",
    "~",
    // Assignment
    "=",
    "+=",
    "-=",
    "*=",
    "/=",
    "%=",
    "&=",
    "|=",
    "^=",
    "<<=",
    ">>=",
    // Path / member
    "::",
    ".",
    "->",
    "=>",
    // Range
    "..",
    "..=",
    "...",
    // Try
    "?",
    // Brackets (each pair counted as two operators)
    "(",
    ")",
    "{",
    "}",
    "[",
    "]",
    // Punctuation
    ",",
    ";",
    ":",
    "@",
    "#",
    "$",
    // Reserved keywords classified as operators (control-flow + binding)
    "if",
    "else",
    "match",
    "while",
    "loop",
    "for",
    "in",
    "return",
    "break",
    "continue",
    "let",
    "mut",
    "ref",
    "fn",
    "impl",
    "trait",
    "struct",
    "enum",
    "type",
    "const",
    "static",
    "use",
    "mod",
    "pub",
    "as",
    "self",
    "Self",
    "super",
    "crate",
    "where",
    "move",
    "async",
    "await",
    "dyn",
    "unsafe",
    "extern",
    "macro_rules",
    "yield",
];

#[derive(Default)]
struct ComplexityVisitor {
    /// Stack of function scopes. Nested `fn` items push/pop their own scope so
    /// each gets its own metrics row.
    scopes: Vec<FunctionScope>,
    /// Emitted rows (one per function).
    out: Vec<FunctionMetrics>,
}

struct FunctionScope {
    name: String,
    start_line: u32,
    end_line: u32,
    decision_points: u32,
    cognitive_increments: Vec<CognitiveIncrement>,
    operators: HashMap<&'static str, u32>,
    operands: HashMap<String, u32>,
    npath_factors: Vec<u64>,
    source_lines: u32,
    comment_lines: u32,
    panic_paths: u32,
    unsafe_blocks: u32,
    /// Current nesting depth (0 = function body top level).
    depth: u8,
}

impl ComplexityVisitor {
    fn enter_fn(
        &mut self,
        name: &Ident,
        body_open: Span,
        body_close: Span,
        body_tokens: TokenStream,
    ) {
        let mut scope = FunctionScope {
            name: name.to_string(),
            start_line: span_line(name.span()),
            end_line: span_line(body_close).max(span_line(body_open)),
            decision_points: 0,
            cognitive_increments: Vec::new(),
            operators: HashMap::new(),
            operands: HashMap::new(),
            npath_factors: Vec::new(),
            source_lines: span_line(body_close).saturating_sub(span_line(body_open)) + 1,
            comment_lines: 0,
            panic_paths: 0,
            unsafe_blocks: 0,
            depth: 0,
        };
        classify_tokens(body_tokens, &mut scope.operators, &mut scope.operands);
        self.scopes.push(scope);
    }

    fn exit_fn(&mut self) {
        if let Some(scope) = self.scopes.pop() {
            let input = ScoringInput {
                name: &scope.name,
                start_line: scope.start_line,
                end_line: scope.end_line,
                decision_points: scope.decision_points,
                cognitive_increments: scope.cognitive_increments,
                operators: scope.operators,
                operands: scope.operands,
                npath_factors: scope.npath_factors,
                source_lines: scope.source_lines,
                comment_lines: scope.comment_lines,
                panic_paths: scope.panic_paths,
                unsafe_blocks: scope.unsafe_blocks,
            };
            self.out.push(complexity::score(&input));
        }
    }

    /// Mutate the current scope (top of stack). No-op if not inside a function.
    fn cur(&mut self) -> Option<&mut FunctionScope> {
        self.scopes.last_mut()
    }

    fn into_metrics(self) -> Vec<FunctionMetrics> {
        self.out
    }
}

/// Classify a token stream into Halstead operator/operand buckets. Recurses
/// into delimiter groups so nested expressions contribute their tokens.
fn classify_tokens(
    stream: TokenStream,
    operators: &mut HashMap<&'static str, u32>,
    operands: &mut HashMap<String, u32>,
) {
    for tt in stream {
        match tt {
            TokenTree::Punct(p) => {
                let s = p.as_char().to_string();
                if let Some(op) = match_operator(&s) {
                    *operators.entry(op).or_insert(0) += 1;
                }
            }
            TokenTree::Ident(id) => {
                let name = id.to_string();
                if let Some(op) = match_operator(&name) {
                    *operators.entry(op).or_insert(0) += 1;
                } else {
                    *operands.entry(name).or_insert(0) += 1;
                }
            }
            TokenTree::Literal(lit) => {
                *operands.entry(lit.to_string()).or_insert(0) += 1;
            }
            TokenTree::Group(g) => {
                // Count the delimiter pair as two operator occurrences if
                // it's a recognized bracket; then recurse.
                let (open, close) = match g.delimiter() {
                    Delimiter::Parenthesis => (Some("("), Some(")")),
                    Delimiter::Brace => (Some("{"), Some("}")),
                    Delimiter::Bracket => (Some("["), Some("]")),
                    Delimiter::None => (None, None),
                };
                if let (Some(o), Some(c)) = (open, close) {
                    *operators.entry(o).or_insert(0) += 1;
                    *operators.entry(c).or_insert(0) += 1;
                }
                classify_tokens(g.stream(), operators, operands);
            }
        }
    }
}

/// Return the static-string equivalent of an operator token, or None if the
/// token is not a recognized operator/keyword.
fn match_operator(s: &str) -> Option<&'static str> {
    RUST_OPERATOR_TOKENS.iter().copied().find(|t| *t == s)
}

/// Recognize Rust's panic-leaf macro names.
fn is_panic_macro(name: &str) -> bool {
    matches!(
        name,
        "panic" | "assert" | "assert_eq" | "assert_ne" | "unreachable" | "todo" | "unimplemented"
    )
}

impl<'ast> Visit<'ast> for ComplexityVisitor {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        let body_tokens = node.block.to_token_stream();
        self.enter_fn(
            &node.sig.ident,
            node.block.brace_token.span.open().span(),
            node.block.brace_token.span.close().span(),
            body_tokens,
        );
        visit::visit_item_fn(self, node);
        self.exit_fn();
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let body_tokens = node.block.to_token_stream();
        self.enter_fn(
            &node.sig.ident,
            node.block.brace_token.span.open().span(),
            node.block.brace_token.span.close().span(),
            body_tokens,
        );
        visit::visit_impl_item_fn(self, node);
        self.exit_fn();
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        // Only score trait methods that have a default body.
        if let Some(block) = &node.default {
            let body_tokens = block.to_token_stream();
            self.enter_fn(
                &node.sig.ident,
                block.brace_token.span.open().span(),
                block.brace_token.span.close().span(),
                body_tokens,
            );
            visit::visit_trait_item_fn(self, node);
            self.exit_fn();
        } else {
            visit::visit_trait_item_fn(self, node);
        }
    }

    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        if let Some(s) = self.cur() {
            s.decision_points = s.decision_points.saturating_add(1);
            s.cognitive_increments.push(CognitiveIncrement {
                depth: s.depth,
                kind: CognitiveKind::NestedCondition,
            });
            s.npath_factors
                .push(if node.else_branch.is_some() { 2 } else { 1 });
            s.depth = s.depth.saturating_add(1);
        }
        visit::visit_expr_if(self, node);
        if let Some(s) = self.cur() {
            s.depth = s.depth.saturating_sub(1);
        }
    }

    fn visit_expr_match(&mut self, node: &'ast syn::ExprMatch) {
        if let Some(s) = self.cur() {
            // Each arm beyond the first is a decision point.
            let arms = node.arms.len() as u32;
            s.decision_points = s.decision_points.saturating_add(arms.saturating_sub(1));
            s.cognitive_increments.push(CognitiveIncrement {
                depth: s.depth,
                kind: CognitiveKind::NestedCondition,
            });
            s.npath_factors.push(arms.max(1) as u64);
            s.depth = s.depth.saturating_add(1);
        }
        visit::visit_expr_match(self, node);
        if let Some(s) = self.cur() {
            s.depth = s.depth.saturating_sub(1);
        }
    }

    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.bump_loop();
        visit::visit_expr_while(self, node);
        if let Some(s) = self.cur() {
            s.depth = s.depth.saturating_sub(1);
        }
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.bump_loop();
        visit::visit_expr_for_loop(self, node);
        if let Some(s) = self.cur() {
            s.depth = s.depth.saturating_sub(1);
        }
    }

    fn visit_expr_loop(&mut self, node: &'ast syn::ExprLoop) {
        self.bump_loop();
        visit::visit_expr_loop(self, node);
        if let Some(s) = self.cur() {
            s.depth = s.depth.saturating_sub(1);
        }
    }

    fn visit_expr_binary(&mut self, node: &'ast syn::ExprBinary) {
        if matches!(node.op, syn::BinOp::And(_) | syn::BinOp::Or(_))
            && let Some(s) = self.cur()
        {
            s.decision_points = s.decision_points.saturating_add(1);
            s.cognitive_increments.push(CognitiveIncrement {
                depth: s.depth,
                kind: CognitiveKind::LogicalSequence,
            });
            s.npath_factors.push(2);
        }
        visit::visit_expr_binary(self, node);
    }

    fn visit_expr_try(&mut self, node: &'ast syn::ExprTry) {
        if let Some(s) = self.cur() {
            s.decision_points = s.decision_points.saturating_add(1);
            s.npath_factors.push(2);
        }
        visit::visit_expr_try(self, node);
    }

    fn visit_expr_unsafe(&mut self, node: &'ast syn::ExprUnsafe) {
        if let Some(s) = self.cur() {
            s.unsafe_blocks = s.unsafe_blocks.saturating_add(1);
        }
        visit::visit_expr_unsafe(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let name = node.method.to_string();
        if matches!(name.as_str(), "unwrap" | "expect")
            && let Some(s) = self.cur()
        {
            s.panic_paths = s.panic_paths.saturating_add(1);
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_macro(&mut self, node: &'ast syn::ExprMacro) {
        if let Some(seg) = node.mac.path.segments.last() {
            let name = seg.ident.to_string();
            if is_panic_macro(&name)
                && let Some(s) = self.cur()
            {
                s.panic_paths = s.panic_paths.saturating_add(1);
            }
        }
        visit::visit_expr_macro(self, node);
    }

    fn visit_stmt_macro(&mut self, node: &'ast syn::StmtMacro) {
        if let Some(seg) = node.mac.path.segments.last() {
            let name = seg.ident.to_string();
            if is_panic_macro(&name)
                && let Some(s) = self.cur()
            {
                s.panic_paths = s.panic_paths.saturating_add(1);
            }
        }
        visit::visit_stmt_macro(self, node);
    }

    // Break and continue contribute cognitive +1 each (BreakInFlow).
    fn visit_expr_break(&mut self, node: &'ast syn::ExprBreak) {
        if let Some(s) = self.cur() {
            s.cognitive_increments.push(CognitiveIncrement {
                depth: s.depth,
                kind: CognitiveKind::BreakInFlow,
            });
        }
        visit::visit_expr_break(self, node);
    }

    fn visit_expr_continue(&mut self, node: &'ast syn::ExprContinue) {
        if let Some(s) = self.cur() {
            s.cognitive_increments.push(CognitiveIncrement {
                depth: s.depth,
                kind: CognitiveKind::BreakInFlow,
            });
        }
        visit::visit_expr_continue(self, node);
    }
}

impl ComplexityVisitor {
    fn bump_loop(&mut self) {
        if let Some(s) = self.cur() {
            s.decision_points = s.decision_points.saturating_add(1);
            s.cognitive_increments.push(CognitiveIncrement {
                depth: s.depth,
                kind: CognitiveKind::NestedCondition,
            });
            s.npath_factors.push(2);
            s.depth = s.depth.saturating_add(1);
        }
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

    // ========================================================================
    // Shadow-ASR integration tests (Phase B)
    //
    // These exercise the type_mapper through the SymbolVisitor end-to-end,
    // asserting that parameters / return_type / generic_params / effects /
    // scope_path / scope_depth land on the right symbols.
    // ========================================================================

    #[test]
    fn shadow_asr_fields_populate_for_free_fn() {
        let src = r#"
pub async fn authenticate(user: &User, password: &str) -> Result<Token, AuthError> {
    todo!()
}
"#;
        let syms = RUST_BACKEND.extract_symbols(src);
        let f = syms
            .iter()
            .find(|s| s.name == "authenticate")
            .expect("authenticate symbol");
        assert_eq!(f.kind, SymbolKind::Function);
        assert_eq!(f.parameters.len(), 2);
        assert_eq!(f.parameters[0].name.as_deref(), Some("user"));
        assert_eq!(
            f.parameters[0].modifier,
            Some(crate::parsing::symbols::ParamModifier::Ref)
        );
        assert_eq!(f.parameters[1].name.as_deref(), Some("password"));

        let rt = f.return_type.as_ref().expect("return type");
        assert!(
            rt.type_tags
                .contains(&crate::parsing::type_tags::vocabulary::TAG_RESULT.to_string())
        );
        let shape = rt.type_shape.as_ref().expect("return shape");
        assert_eq!(shape.constructor, "Result");

        assert!(
            f.effects
                .contains(&crate::parsing::type_tags::vocabulary::EFFECT_ASYNC.to_string())
        );
        assert!(
            f.effects
                .contains(&crate::parsing::type_tags::vocabulary::EFFECT_MAY_PANIC.to_string())
        );

        assert_eq!(f.scope_path.as_deref(), Some("crate::authenticate"));
        assert_eq!(f.scope_depth, Some(0));
    }

    #[test]
    fn shadow_asr_fields_populate_for_impl_method() {
        let src = r#"
struct Service;
impl Service {
    pub fn handle(&self, req: Request) -> Response {
        Response::default()
    }
}
"#;
        let syms = RUST_BACKEND.extract_symbols(src);
        let m = syms
            .iter()
            .find(|s| s.name == "handle")
            .expect("handle method");
        // Receiver + req = 2 params
        assert_eq!(m.parameters.len(), 2);
        assert!(m.parameters[0].is_self, "receiver should be is_self");
        assert_eq!(m.parameters[0].name.as_deref(), Some("self"));
        assert_eq!(m.parameters[1].name.as_deref(), Some("req"));
        assert_eq!(m.scope_path.as_deref(), Some("crate::Service::handle"));
        assert_eq!(m.scope_depth, Some(1));
    }

    #[test]
    fn shadow_asr_fields_populate_for_trait_method_with_default() {
        let src = r#"
pub trait Processor {
    fn process(&self, input: &str) -> String {
        String::from(input)
    }
    fn count(&self) -> usize;
}
"#;
        let syms = RUST_BACKEND.extract_symbols(src);
        let process = syms
            .iter()
            .find(|s| {
                s.name == "process"
                    && s.scope_path
                        .as_deref()
                        .is_some_and(|p| p.contains("Processor"))
            })
            .expect("trait process method");
        assert_eq!(
            process.scope_path.as_deref(),
            Some("crate::Processor::process")
        );
        assert_eq!(process.parameters.len(), 2);
        assert!(process.parameters[0].is_self);
        let count = syms
            .iter()
            .find(|s| {
                s.name == "count"
                    && s.scope_path
                        .as_deref()
                        .is_some_and(|p| p.contains("Processor"))
            })
            .expect("trait count method (no body)");
        assert_eq!(count.scope_path.as_deref(), Some("crate::Processor::count"));
        // Method without a body still surfaces parameters and return type.
        assert_eq!(count.parameters.len(), 1);
        let rt = count.return_type.as_ref().expect("return type");
        assert!(
            rt.type_tags
                .contains(&crate::parsing::type_tags::vocabulary::TAG_UINT.to_string())
        );
    }

    #[test]
    fn shadow_asr_effects_for_unsafe_fn() {
        let src = "unsafe fn read_raw(p: *const u8) -> u8 { unsafe { *p } }";
        let syms = RUST_BACKEND.extract_symbols(src);
        let f = syms
            .iter()
            .find(|s| s.name == "read_raw")
            .expect("read_raw");
        assert!(
            f.effects
                .contains(&crate::parsing::type_tags::vocabulary::EFFECT_UNSAFE.to_string())
        );
        // Deduped — even though both `unsafe fn` and an `unsafe { }` block
        // are present, only one effect entry remains.
        let unsafe_count = f
            .effects
            .iter()
            .filter(|e| e.as_str() == crate::parsing::type_tags::vocabulary::EFFECT_UNSAFE)
            .count();
        assert_eq!(unsafe_count, 1);
    }

    #[test]
    fn shadow_asr_generic_params_extracted() {
        let src = r#"
pub fn pick<T: Clone + Send, U>(a: T, b: U) -> T { a }
"#;
        let syms = RUST_BACKEND.extract_symbols(src);
        let f = syms.iter().find(|s| s.name == "pick").expect("pick");
        assert_eq!(f.generic_params.len(), 2);
        assert_eq!(f.generic_params[0].name, "T");
        assert!(
            f.generic_params[0]
                .bounds
                .iter()
                .any(|b| b.contains("Clone"))
        );
        assert!(
            f.generic_params[0]
                .bounds
                .iter()
                .any(|b| b.contains("Send"))
        );
        assert_eq!(f.generic_params[1].name, "U");
    }

    #[test]
    fn shadow_asr_scope_path_nests_through_mod() {
        let src = r#"
mod outer {
    pub mod inner {
        pub fn deep() {}
    }
}
"#;
        let syms = RUST_BACKEND.extract_symbols(src);
        let deep = syms.iter().find(|s| s.name == "deep").expect("deep fn");
        assert_eq!(
            deep.scope_path.as_deref(),
            Some("crate::outer::inner::deep")
        );
        assert_eq!(deep.scope_depth, Some(2));
    }

    #[test]
    fn shadow_asr_test_attr_marks_effect() {
        let src = r#"
#[test]
fn it_works() { assert!(true); }
"#;
        let syms = RUST_BACKEND.extract_symbols(src);
        let f = syms
            .iter()
            .find(|s| s.name == "it_works")
            .expect("it_works");
        assert!(
            f.effects
                .contains(&crate::parsing::type_tags::vocabulary::EFFECT_TEST.to_string())
        );
    }

    #[test]
    fn shadow_asr_deprecated_attr_marks_effect() {
        let src = r#"
#[deprecated]
pub fn old_api() {}
"#;
        let syms = RUST_BACKEND.extract_symbols(src);
        let f = syms.iter().find(|s| s.name == "old_api").expect("old_api");
        assert!(
            f.effects
                .contains(&crate::parsing::type_tags::vocabulary::EFFECT_DEPRECATED.to_string())
        );
    }

    #[test]
    fn language_name_is_rust() {
        assert_eq!(RUST_BACKEND.language_name(), "rust");
    }

    // ========================================================================
    // ComplexityVisitor tests (SOTA Phase 1, A1)
    // ========================================================================

    #[test]
    fn cc_for_empty_fn_is_one() {
        let src = "fn empty() {}";
        let metrics = RUST_BACKEND.extract_function_metrics(src);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "empty");
        assert_eq!(metrics[0].cyclomatic, 1);
    }

    #[test]
    fn cc_for_if_else_match() {
        let src = r#"
fn branchy(x: i32) -> i32 {
    if x > 0 {
        1
    } else {
        match x {
            -1 => -1,
            -2 => -2,
            _ => 0,
        }
    }
}
"#;
        let metrics = RUST_BACKEND.extract_function_metrics(src);
        assert_eq!(metrics.len(), 1);
        // 1 if + 2 extra match arms = 3 decision points → CC = 4
        assert_eq!(metrics[0].cyclomatic, 4);
    }

    #[test]
    fn cognitive_increments_with_nesting() {
        let src = r#"
fn deep(x: i32) -> i32 {
    if x > 0 {
        if x > 1 {
            return 2;
        }
    }
    0
}
"#;
        let metrics = RUST_BACKEND.extract_function_metrics(src);
        // outer if: +1, inner if: +1+1=2 → total cognitive=3
        assert!(
            metrics[0].cognitive >= 3,
            "got cognitive = {}",
            metrics[0].cognitive
        );
    }

    #[test]
    fn halstead_counts_operators_in_simple_fn() {
        let src = "fn add(a: i32, b: i32) -> i32 { a + b }";
        let metrics = RUST_BACKEND.extract_function_metrics(src);
        assert_eq!(metrics.len(), 1);
        // The body is `{ a + b }`. Operators include `{`, `}`, `+`. Operands
        // include `a` and `b`. Both η1 and η2 must be > 0.
        assert!(metrics[0].halstead.n1 > 0);
        assert!(metrics[0].halstead.n2 > 0);
    }

    #[test]
    fn unsafe_blocks_counted() {
        let src = r#"
fn dangerous() {
    unsafe {
        let _x: *const i32 = std::ptr::null();
    }
    unsafe {
        let _y: *const i32 = std::ptr::null();
    }
}
"#;
        let metrics = RUST_BACKEND.extract_function_metrics(src);
        assert_eq!(metrics[0].unsafe_blocks, 2);
    }

    #[test]
    fn panic_paths_counted_for_unwrap_and_macros() {
        let src = r#"
fn risky(x: Option<i32>) -> i32 {
    let v = x.unwrap();
    if v < 0 {
        panic!("negative");
    }
    assert!(v > 0);
    v
}
"#;
        let metrics = RUST_BACKEND.extract_function_metrics(src);
        // unwrap + panic + assert = 3 panic-leaves
        assert_eq!(metrics[0].panic_paths, 3);
    }

    #[test]
    fn impl_methods_score_independently() {
        let src = r#"
struct S;
impl S {
    fn method_a(&self) -> i32 {
        if true { 1 } else { 0 }
    }
    fn method_b(&self) {}
}
"#;
        let metrics = RUST_BACKEND.extract_function_metrics(src);
        let names: Vec<&str> = metrics.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"method_a"));
        assert!(names.contains(&"method_b"));
        let a = metrics
            .iter()
            .find(|m| m.name == "method_a")
            .expect("method_a");
        let b = metrics
            .iter()
            .find(|m| m.name == "method_b")
            .expect("method_b");
        assert_eq!(a.cyclomatic, 2); // one if
        assert_eq!(b.cyclomatic, 1); // empty
    }

    #[test]
    fn try_operator_counts_as_decision() {
        let src = r#"
fn parse(s: &str) -> Result<i32, String> {
    let v = s.parse::<i32>().map_err(|e| e.to_string())?;
    Ok(v)
}
"#;
        let metrics = RUST_BACKEND.extract_function_metrics(src);
        assert!(
            metrics[0].cyclomatic >= 2,
            "got CC = {}",
            metrics[0].cyclomatic
        );
    }

    #[test]
    fn loop_counts_as_decision() {
        let src = r#"
fn sum(xs: &[i32]) -> i32 {
    let mut s = 0;
    for x in xs {
        s += x;
    }
    s
}
"#;
        let metrics = RUST_BACKEND.extract_function_metrics(src);
        assert_eq!(metrics[0].cyclomatic, 2);
    }

    #[test]
    fn parse_error_yields_empty_fn_metrics() {
        let bogus = "this is not valid Rust { syntax";
        assert!(RUST_BACKEND.extract_function_metrics(bogus).is_empty());
    }

    #[test]
    fn boolean_and_or_count_as_decisions() {
        let src = r#"
fn check(a: bool, b: bool, c: bool) -> bool {
    a && b || c
}
"#;
        let metrics = RUST_BACKEND.extract_function_metrics(src);
        // a && b = +1, ... || c = +1 → CC = 3
        assert!(
            metrics[0].cyclomatic >= 3,
            "got CC = {}",
            metrics[0].cyclomatic
        );
    }

    #[test]
    fn npath_non_overflow_for_typical_fn() {
        let src = r#"
fn typical(x: i32) -> i32 {
    if x > 0 {
        x * 2
    } else {
        -x
    }
}
"#;
        let metrics = RUST_BACKEND.extract_function_metrics(src);
        assert!(matches!(
            metrics[0].npath,
            crate::parsing::function_metrics::NPathValue::Counted(2)
        ));
    }
}
