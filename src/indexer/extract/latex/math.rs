//! `MathExpr` → readable text renderer.
//!
//! Symbols, operators, and Greek letters become Unicode (`α`, `≤`, `∑`, `∫`);
//! scripts use `^`/`_`, fractions use `/`, and roots use `√(…)`, so identifiers
//! and numbers stay searchable. This is strictly better than `pandoc --to plain`,
//! which emits a warning and dumps the raw TeX for any non-trivial math.

use latex_parser::{MathContent, MathExpr, NodeRef, Span, Spanned, lift_math, lift_math_nodes};

use super::symbols::{accent_combining, math_operator, math_symbol};

/// Render a math span (`$...$`, `\[...\]`) to text.
pub fn render_math(content: &MathContent) -> String {
    let lifted = lift_math(content);
    render_expr(&lifted.node).trim().to_string()
}

/// Render the body of a math *environment* (`equation`, `align`, ...) to text.
pub fn render_math_nodes(nodes: &[NodeRef]) -> String {
    let lifted = lift_math_nodes(nodes, Span::empty(0));
    render_expr(&lifted.node).trim().to_string()
}

fn render_expr(e: &MathExpr) -> String {
    match e {
        MathExpr::Number(s) | MathExpr::Text(s) | MathExpr::Raw(s) => s.clone(),
        MathExpr::Identifier(name) => math_symbol(name)
            .map(str::to_string)
            .unwrap_or_else(|| name.clone()),
        MathExpr::Operator(op) => math_operator(op)
            .map(str::to_string)
            .unwrap_or_else(|| op.clone()),
        // A math parse error: emit nothing rather than a diagnostic string; the
        // surrounding prose is still rendered.
        MathExpr::Error(_) => String::new(),
        MathExpr::Fraction {
            numerator,
            denominator,
        } => format!(
            "{}/{}",
            paren_if_compound(numerator),
            paren_if_compound(denominator)
        ),
        MathExpr::Binomial { top, bottom } => {
            format!(
                "C({}, {})",
                render_expr(&top.node),
                render_expr(&bottom.node)
            )
        }
        MathExpr::Subscript { base, subscript } => {
            format!("{}_{}", render_expr(&base.node), script(subscript))
        }
        MathExpr::Superscript { base, superscript } => {
            format!("{}^{}", render_expr(&base.node), script(superscript))
        }
        MathExpr::SubSuperscript {
            base,
            subscript,
            superscript,
        } => format!(
            "{}_{}^{}",
            render_expr(&base.node),
            script(subscript),
            script(superscript)
        ),
        MathExpr::Root { index, radicand } => match index {
            Some(i) => format!("{}√({})", script(i), render_expr(&radicand.node)),
            None => format!("√({})", render_expr(&radicand.node)),
        },
        MathExpr::BigOperator {
            operator,
            lower,
            upper,
            body,
        } => {
            let sym = math_operator(operator).unwrap_or(operator.as_str());
            format!(
                "{}{} {}",
                sym,
                limits(lower, upper),
                render_expr(&body.node)
            )
        }
        MathExpr::Integral {
            kind,
            lower,
            upper,
            body,
        } => {
            let sym = math_operator(kind).unwrap_or("∫");
            format!(
                "{}{} {}",
                sym,
                limits(lower, upper),
                render_expr(&body.node)
            )
        }
        MathExpr::Function { name, argument } => {
            let arg = render_expr(&argument.node);
            if arg.is_empty() {
                name.clone()
            } else {
                format!("{name} {arg}")
            }
        }
        MathExpr::Grouped {
            left_delim,
            content,
            right_delim,
        } => format!(
            "{}{}{}",
            delim(left_delim),
            render_expr(&content.node),
            delim(right_delim)
        ),
        MathExpr::Sequence(items) => render_sequence(items),
        MathExpr::Matrix { kind, rows } => render_matrix(kind, rows),
        MathExpr::Accent { accent, base } => {
            let mut b = render_expr(&base.node);
            if let Some(mark) = accent_combining(accent) {
                // Append the combining mark; `normalize_extracted_text`'s NFKC
                // pass composes it onto the (single) base char where possible.
                b.push(mark);
            }
            b
        }
    }
}

/// Render a sub/superscript operand. A single character is bare (`x^2`); anything
/// longer is parenthesized (`x^(n+1)`).
fn script(e: &Spanned<MathExpr>) -> String {
    let r = render_expr(&e.node);
    if r.chars().count() <= 1 {
        r
    } else {
        format!("({r})")
    }
}

/// Parenthesize a fraction operand only when it is a multi-term sequence, so
/// `\frac{a+b}{c}` → `(a+b)/c` but `\frac{a}{b}` → `a/b`.
fn paren_if_compound(e: &Spanned<MathExpr>) -> String {
    let r = render_expr(&e.node);
    if matches!(e.node, MathExpr::Sequence(_)) {
        format!("({r})")
    } else {
        r
    }
}

fn limits(
    lower: &Option<Box<Spanned<MathExpr>>>,
    upper: &Option<Box<Spanned<MathExpr>>>,
) -> String {
    let mut s = String::new();
    if let Some(l) = lower {
        s.push('_');
        s.push_str(&script(l));
    }
    if let Some(u) = upper {
        s.push('^');
        s.push_str(&script(u));
    }
    s
}

fn delim(d: &str) -> String {
    match d {
        "\\{" => "{".to_string(),
        "\\}" => "}".to_string(),
        "\\langle" | "langle" => "⟨".to_string(),
        "\\rangle" | "rangle" => "⟩".to_string(),
        "\\lvert" | "lvert" | "\\vert" | "vert" | "|" => "|".to_string(),
        "\\lVert" | "\\Vert" | "Vert" | "\\|" => "‖".to_string(),
        "\\lceil" | "lceil" => "⌈".to_string(),
        "\\rceil" | "rceil" => "⌉".to_string(),
        "\\lfloor" | "lfloor" => "⌊".to_string(),
        "\\rfloor" | "rfloor" => "⌋".to_string(),
        // `\left.` / `\right.` is an invisible delimiter.
        "." => String::new(),
        other => other.to_string(),
    }
}

fn render_sequence(items: &[Spanned<MathExpr>]) -> String {
    let mut out = String::new();
    for item in items {
        let r = render_expr(&item.node);
        if r.is_empty() {
            continue;
        }
        if matches!(item.node, MathExpr::Operator(_)) {
            // Pad binary operators / relations with spaces for readability;
            // `normalize_extracted_text` collapses any doubled spaces.
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            out.push_str(&r);
            out.push(' ');
        } else {
            out.push_str(&r);
        }
    }
    out
}

fn render_matrix(kind: &str, rows: &[Vec<Spanned<MathExpr>>]) -> String {
    let body = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| render_expr(&c.node))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .collect::<Vec<_>>()
        .join("; ");
    match kind {
        "cases" | "dcases" | "rcases" => format!("{{ {body} }}"),
        _ => format!("[{body}]"),
    }
}
