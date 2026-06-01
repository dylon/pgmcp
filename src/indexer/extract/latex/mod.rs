//! In-process LaTeX → plain-text extraction, replacing the `pandoc` subprocess
//! for `.tex` files.
//!
//! pgmcp consumes `latex_parser`'s error-tolerant AST (`parse` + `ast` +
//! `lift_math`) and renders it to plain text here — the renderer is a pgmcp
//! concern; `latex-parser` stays a pure parser. Unlike pandoc, this never
//! hard-fails on imperfect LaTeX (so the file is no longer skipped) and renders
//! math to readable Unicode that pandoc's `--to plain` cannot.
//!
//! Layout:
//! - [`extract`] — the [`super::Extracted`] entry point.
//! - `render` — the `Node`-tree → plain-text walker.
//! - `math` — the `MathExpr` → Unicode renderer.
//! - `symbols` — the Unicode lookup tables.

mod expand;
mod extract;
mod math;
mod render;
mod symbols;

pub use extract::extract;
