//! LaTeX AST (`Node` tree) → plain-text walker.
//!
//! Walks `latex_parser`'s presentational `Node` tree, emitting readable prose:
//! command markup is stripped (keeping the prose inside its arguments), accents
//! and special characters become Unicode, math is rendered via [`super::math`],
//! lists/tables are linearized, and verbatim code is emitted literally. The
//! output is fed through `normalize_extracted_text`, which composes combining
//! accents (NFKC) and collapses whitespace — so this walker can be liberal with
//! spaces and only needs to be *correct*, not minimal.

use latex_parser::{Command, Document, Environment, Node, NodeRef};

use super::math::{render_math, render_math_nodes};
use super::symbols::{accent_combining, math_operator, math_symbol, text_symbol};

/// Options for [`to_plain_text`].
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Include `% comment` text in the output (default off).
    pub include_comments: bool,
    /// Expand `\newcommand`/`\def`/`\DeclareMathOperator` macros before
    /// rendering (a pure AST→AST pass in `super::expand`). On by default: it
    /// surfaces symbol/operator/abbreviation macros as readable text and never
    /// loses content (termination-guarded; unexpanded uses render as their name).
    pub expand_macros: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            include_comments: false,
            expand_macros: true,
        }
    }
}

/// Render a parsed [`Document`] to plain text. `capacity_hint` (the source byte
/// length) pre-sizes the output buffer.
pub fn to_plain_text(doc: &Document, capacity_hint: usize, opts: &RenderOptions) -> String {
    // Optionally expand macros first (a pure AST→AST pass).
    let expanded;
    let doc = if opts.expand_macros {
        expanded = super::expand::expand_macros(doc);
        &expanded
    } else {
        doc
    };
    let mut w = Writer::new(capacity_hint);
    walk(&doc.preamble, &mut w, opts);
    if let Some(body) = &doc.body {
        walk(body, &mut w, opts);
    }
    w.finish()
}

/// A spacing-aware string accumulator. Coalesces requested spaces, applies a
/// pending accent to the next character, and bounds blank-line runs.
struct Writer {
    buf: String,
    pending_space: bool,
    pending_accent: Option<char>,
    at_line_start: bool,
}

impl Writer {
    fn new(cap: usize) -> Self {
        Self {
            buf: String::with_capacity(cap),
            pending_space: false,
            pending_accent: None,
            at_line_start: true,
        }
    }

    fn flush_space(&mut self) {
        if self.pending_space && !self.at_line_start {
            self.buf.push(' ');
        }
        self.pending_space = false;
    }

    /// Append a word/phrase, applying any pending accent to its first character.
    fn text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.flush_space();
        match self.pending_accent.take() {
            Some(mark) => {
                let mut chars = s.chars();
                if let Some(first) = chars.next() {
                    self.buf.push(first);
                    self.buf.push(mark);
                    self.buf.push_str(chars.as_str());
                }
            }
            None => self.buf.push_str(s),
        }
        self.at_line_start = false;
    }

    /// Append text verbatim (for code/math), preserving internal newlines.
    fn raw(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.flush_space();
        self.buf.push_str(s);
        self.at_line_start = s.ends_with('\n');
    }

    /// Request a single separating space (coalesced; suppressed at line start).
    fn space(&mut self) {
        if !self.at_line_start {
            self.pending_space = true;
        }
    }

    /// End the current line.
    fn newline(&mut self) {
        self.pending_space = false;
        while self.buf.ends_with(' ') {
            self.buf.pop();
        }
        if !self.buf.is_empty() && !self.buf.ends_with('\n') {
            self.buf.push('\n');
        }
        self.at_line_start = true;
    }

    /// Emit a paragraph break (blank line), capped at two trailing newlines.
    fn blank_line(&mut self) {
        self.pending_space = false;
        while self.buf.ends_with(' ') {
            self.buf.pop();
        }
        if self.buf.is_empty() {
            return;
        }
        let trailing = self.buf.chars().rev().take_while(|&c| c == '\n').count();
        for _ in trailing..2 {
            self.buf.push('\n');
        }
        self.at_line_start = true;
    }

    fn set_accent(&mut self, mark: char) {
        self.pending_accent = Some(mark);
    }

    fn finish(mut self) -> String {
        self.pending_accent = None;
        while self.buf.ends_with([' ', '\n']) {
            self.buf.pop();
        }
        self.buf
    }
}

fn walk(nodes: &[NodeRef], w: &mut Writer, opts: &RenderOptions) {
    for n in nodes {
        match &n.node {
            Node::Text(t) => w.text(&convert_punctuation(t)),
            Node::Whitespace(ws) => emit_whitespace(ws, w),
            Node::Group(g) => walk(g, w, opts),
            Node::Comment(c) => {
                if opts.include_comments {
                    w.space();
                    w.text(c.trim());
                }
            }
            Node::Math(m) => {
                let s = render_math(m);
                if !s.is_empty() {
                    w.space();
                    w.text(&s);
                    w.space();
                }
            }
            Node::Command(c) => walk_command(c, w, opts),
            Node::Environment(e) => walk_environment(e, w, opts),
            Node::Verbatim(v) => {
                w.blank_line();
                w.raw(&v.body);
                w.blank_line();
            }
            Node::VerbatimInline(v) => w.raw(&v.body),
            Node::Parameter(p) => w.text(&format!("#{p}")),
            Node::Error(err) => {
                // Lossless: emit the unparsed bytes + any recovered nodes.
                w.text(err.raw_text.trim());
                walk(&err.recovered_nodes, w, opts);
            }
        }
    }
}

/// LaTeX text-node punctuation: em/en dashes and smart double-quotes. Single
/// `'`/`` ` `` are left as ASCII (contractions, code).
fn convert_punctuation(t: &str) -> String {
    if !t.contains(['-', '`', '\'']) {
        return t.to_string();
    }
    t.replace("---", "—")
        .replace("--", "–")
        .replace("``", "“")
        .replace("''", "”")
}

fn emit_whitespace(ws: &str, w: &mut Writer) {
    if ws.bytes().filter(|&b| b == b'\n').count() >= 2 {
        w.blank_line();
    } else {
        w.space();
    }
}

fn name_base(name: &str) -> &str {
    name.strip_suffix('*').unwrap_or(name)
}

fn walk_command(c: &Command, w: &mut Writer, opts: &RenderOptions) {
    let name = c.name.as_str();

    // 1. Text-mode accents (`\'e`, `\^{o}`, `\c{c}`): set a pending accent and
    //    render the braced argument so it attaches to the first character.
    //    (\overline/\underline are prose wrappers, not accents — see default.)
    if !matches!(name, "overline" | "underline")
        && let Some(mark) = accent_combining(name)
    {
        w.set_accent(mark);
        for a in &c.args {
            walk(&a.content, w, opts);
        }
        return;
    }

    // 2. Literal symbol / special-character commands (`\&`, `\ss`, `\LaTeX`).
    if let Some(sym) = text_symbol(name) {
        w.text(sym);
        return;
    }

    // 3. Math symbols/operators that appear in text mode (`\rightarrow`).
    if let Some(sym) = math_symbol(name).or_else(|| math_operator(name)) {
        w.text(sym);
        return;
    }

    // 4. Spacing commands → a single space (their length arg, if any, is dropped).
    if is_space_command(name) {
        w.space();
        return;
    }

    // 5. Explicit breaks.
    match name {
        "\\" | "newline" | "linebreak" | "newpage" | "clearpage" | "cleardoublepage"
        | "pagebreak" => {
            w.newline();
            return;
        }
        "par" | "bigskip" | "medskip" | "smallskip" | "vspace" => {
            w.blank_line();
            return;
        }
        _ => {}
    }

    // 6. Sectioning: blank line, then the (last, mandatory) title, then a newline.
    if is_sectioning(name_base(name)) {
        w.blank_line();
        if let Some(title) = c.args.last() {
            walk(&title.content, w, opts);
        }
        w.newline();
        return;
    }

    // 7. Metadata / definition commands → drop entirely (name + args).
    if is_drop_command(name) {
        return;
    }

    // 8. Commands whose useful content is their *last* argument.
    match name {
        "href" | "multicolumn" | "multirow" | "raisebox" | "parbox" | "makebox" | "framebox" => {
            if let Some(last) = c.args.last() {
                walk(&last.content, w, opts);
            }
            return;
        }
        "url" | "nolinkurl" => {
            if let Some(first) = c.args.first() {
                walk(&first.content, w, opts);
            }
            return;
        }
        _ => {}
    }

    // 9. Default: emit the prose inside required arguments; for an argument-less
    //    command, emit its name as a searchable token when it looks like content
    //    (a user macro `\Cat` → "Cat"), else drop a formatting control word.
    if c.args.is_empty() {
        if is_content_word(name) {
            w.text(name);
        }
    } else {
        for a in &c.args {
            walk(&a.content, w, opts);
        }
    }
}

fn walk_environment(e: &Environment, w: &mut Writer, opts: &RenderOptions) {
    if e.is_math() {
        let s = render_math_nodes(&e.body);
        if !s.is_empty() {
            w.blank_line();
            w.text(&s);
            w.blank_line();
        }
        return;
    }

    match name_base(&e.name) {
        "itemize" | "enumerate" | "description" => render_list(e, w, opts),
        "tabular" | "tabularx" | "tabulary" | "array" | "longtable" | "tabbing" => {
            render_table(e, w, opts)
        }
        "document" | "abstract" | "quote" | "quotation" | "center" | "flushleft" | "flushright"
        | "verse" | "minipage" | "figure" | "table" | "wrapfigure" | "wraptable" | "frame"
        | "block" | "theorem" | "proof" | "lemma" | "definition" | "example" | "remark"
        | "corollary" | "proposition" | "note" | "thebibliography" => {
            w.blank_line();
            walk(&e.body, w, opts);
            w.blank_line();
        }
        _ => walk(&e.body, w, opts),
    }
}

fn render_list(e: &Environment, w: &mut Writer, opts: &RenderOptions) {
    let ordered = name_base(&e.name) == "enumerate";
    let mut idx = 1u32;
    w.blank_line();
    for n in &e.body {
        match &n.node {
            Node::Command(c) if c.name == "item" => {
                w.newline();
                if ordered {
                    w.text(&format!("{idx}. "));
                    idx += 1;
                } else {
                    w.text("- ");
                }
                if let Some(label) = c.optional_args.first() {
                    walk(&label.content, w, opts);
                    w.text(": ");
                }
            }
            _ => walk(std::slice::from_ref(n), w, opts),
        }
    }
    w.blank_line();
}

fn render_table(e: &Environment, w: &mut Writer, opts: &RenderOptions) {
    w.blank_line();
    for n in &e.body {
        match &n.node {
            // `\\` row break.
            Node::Command(c) if c.name == "\\" => w.newline(),
            // `&` cell separator (a leaf Text node).
            Node::Text(t) if t == "&" => w.space(),
            // Horizontal rules carry no text.
            Node::Command(c)
                if matches!(
                    c.name.as_str(),
                    "hline" | "cline" | "toprule" | "midrule" | "bottomrule" | "cmidrule"
                ) => {}
            _ => walk(std::slice::from_ref(n), w, opts),
        }
    }
    w.blank_line();
}

/// Spacing commands that render as a single space.
fn is_space_command(name: &str) -> bool {
    matches!(
        name,
        "," | ";"
            | ":"
            | "!"
            | " "
            | "quad"
            | "qquad"
            | "enspace"
            | "thinspace"
            | "negthinspace"
            | "medspace"
            | "negmedspace"
            | "thickspace"
            | "negthickspace"
            | "hspace"
            | "hskip"
            | "hfill"
            | "hphantom"
            | "phantom"
    )
}

fn is_sectioning(name: &str) -> bool {
    matches!(
        name,
        "part"
            | "chapter"
            | "section"
            | "subsection"
            | "subsubsection"
            | "paragraph"
            | "subparagraph"
    )
}

/// Metadata / definition / reference commands whose content is not prose.
fn is_drop_command(name: &str) -> bool {
    matches!(
        name,
        "label"
            | "ref"
            | "eqref"
            | "pageref"
            | "vref"
            | "autoref"
            | "nameref"
            | "cref"
            | "Cref"
            | "labelcref"
            | "cpageref"
            | "Cpageref"
            | "crefrange"
            | "Crefrange"
            | "cite"
            | "citep"
            | "citet"
            | "citealp"
            | "citealt"
            | "Citep"
            | "Citet"
            | "citeauthor"
            | "citeyear"
            | "citeyearpar"
            | "textcite"
            | "parencite"
            | "footcite"
            | "autocite"
            | "supercite"
            | "nocite"
            | "usepackage"
            | "RequirePackage"
            | "documentclass"
            | "includegraphics"
            | "input"
            | "include"
            | "subfile"
            | "import"
            | "subimport"
            | "bibliography"
            | "bibliographystyle"
            | "printbibliography"
            | "addbibresource"
            | "newcommand"
            | "renewcommand"
            | "providecommand"
            | "DeclareRobustCommand"
            | "newenvironment"
            | "renewenvironment"
            | "newtheorem"
            | "theoremstyle"
            | "DeclareMathOperator"
            | "def"
            | "let"
            | "setcounter"
            | "addtocounter"
            | "stepcounter"
            | "refstepcounter"
            | "setlength"
            | "addtolength"
            | "settowidth"
            | "settoheight"
            | "settodepth"
            | "definecolor"
            | "color"
            | "pagecolor"
            | "hypersetup"
            | "pagestyle"
            | "thispagestyle"
            | "graphicspath"
            | "tableofcontents"
            | "listoffigures"
            | "listoftables"
            | "maketitle"
            | "bibitem"
            | "index"
            | "glossary"
    )
}

/// No-text formatting / spacing control words: dropped when argument-less so an
/// unknown *content* macro (`\Cat`) is still emitted as a searchable token.
fn is_content_word(name: &str) -> bool {
    if name.len() < 2 || !name.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    !matches!(
        name,
        "hfill"
            | "vfill"
            | "hrule"
            | "vrule"
            | "hrulefill"
            | "dotfill"
            | "noindent"
            | "indent"
            | "centering"
            | "raggedright"
            | "raggedleft"
            | "nopagebreak"
            | "nolinebreak"
            | "frenchspacing"
            | "nonfrenchspacing"
            | "normalsize"
            | "small"
            | "large"
            | "Large"
            | "LARGE"
            | "huge"
            | "Huge"
            | "tiny"
            | "scriptsize"
            | "footnotesize"
            | "normalfont"
            | "bfseries"
            | "itshape"
            | "ttfamily"
            | "rmfamily"
            | "sffamily"
            | "mdseries"
            | "upshape"
            | "slshape"
            | "scshape"
            | "em"
            | "boldmath"
            | "unboldmath"
            | "displaystyle"
            | "textstyle"
            | "scriptstyle"
            | "scriptscriptstyle"
            | "protect"
            | "relax"
            | "ignorespaces"
            | "noalign"
            | "fussy"
            | "sloppy"
            | "flushbottom"
            | "raggedbottom"
            | "linewidth"
            | "textwidth"
            | "columnwidth"
            | "textheight"
            | "baselineskip"
            | "parindent"
            | "parskip"
            | "footnotemark"
            | "frontmatter"
            | "mainmatter"
            | "backmatter"
            | "appendix"
            | "newline"
            | "par"
            | "begingroup"
            | "endgroup"
            | "bgroup"
            | "egroup"
            | "leavevmode"
            | "toprule"
            | "midrule"
            | "bottomrule"
            | "hline"
            | "endfirsthead"
            | "endhead"
            | "endfoot"
            | "endlastfoot"
    )
}
