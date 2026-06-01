# LaTeX env-body / display-math argument swallow — Scientific Ledger

**Date opened:** 2026-05-31
**Host:** NVIDIA RTX 4060 Ti (8 GiB VRAM, Ada Lovelace, CC 8.9), Arch Linux
**Crates:** `pgmcp` (consumer) + `../latex-parser` (path dep; the defect lives here)
**Trigger:** After the original two-bug fix (`docs/.../what-is-the-root-whimsical-willow.md`
plan — Bug 1: pandoc→in-process LaTeX renderer; Bug 2: memory-graph-refresh
timeout) landed verify.sh-green and a clean rescan re-extracted the workspace's
`.tex` files, a **clean scan still left 66 of 418 LaTeX files with zero chunks**
— indexed, no extraction *error*, but rendered to empty/near-empty text. The
pandoc WARNs were gone (Bug 1 closed) yet the *content* of 66 papers was still
missing. A residual, deeper defect.

Every hypothesis, experiment, and measurement here is reproducible from the
recorded commands, per the CLAUDE.md "scientific ledger" rule.

---

## 1. Method

Enumerated the zero-chunk LaTeX files and looked for a shared structural cause:

```sh
psql -h localhost -U pgmcp -d pgmcp -tAc "
  SELECT f.path FROM indexed_files f
  WHERE f.language='latex'
    AND NOT EXISTS (SELECT 1 FROM file_chunks fc WHERE fc.file_id=f.id)
  ORDER BY f.size_bytes DESC NULLS LAST"
```

Built a **standalone diagnostic harness** (`/tmp/latex_diag`) that copies the
pgmcp `src/indexer/extract/latex/` renderer modules and points `latex-parser` at
a scratch copy (`/tmp/lp_fixed`) — so a parser hypothesis can be patched and
measured against the real failing files **without** rebuilding pgmcp (a ~10 min
cycle) or touching the user's actively-edited `latex-parser` working tree. The
harness prints `rendered N chars` for a given `.tex`.

Largest failing file: `…/Policy as Types beat2014/beat2014.tex` (31 KB) →
rendered **74 chars** (only the five `\newlength` preamble lines). 33 of the 66
failing files contain `\newenvironment`.

## 2. Root cause

The failing files share the macro idiom (beat2014.tex):

```latex
\newenvironment{grammar}{\[\begin{array}{l@{\quad}rcl@{\quad}l}}{\end{array}\]}
```

The **begin-def** argument `{\[\begin{array}{…}}` opens *two* constructs whose
closers live in the **separate end-def** argument `{\end{array}\]}`:

1. `\[` — display math, closed by `\]` (in end-def).
2. `\begin{array}` — environment, closed by `\end{array}` (in end-def).

`latex-parser`'s two content-scanning loops each scan until *their own*
terminator and **did not stop at an enclosing `}`**:

- `parser.rs::parse_math_content` scans until the matching `\]` / `$` / EOF.
- `parser.rs::parse_environment_body` scans until the matching `\end{name}` / EOF.

Inside the begin-def argument the terminator is absent (it is in the *other*
argument), so both loops ran to **EOF**, consuming the begin-def's own closing
`}` (as an unexpected-close error node) and **the entire remaining document** —
`\section`s, abstract, body prose, everything — into the first argument. The
renderer drops def-commands (`\newenvironment` is a `is_drop_command`), so the
swallowed payload was dropped with it → 74 chars out of 31 KB.

This is a **containment** defect: a construct opened inside an argument must not
consume past that argument's closing brace.

## 3. Hypotheses & experiments

| # | Hypothesis | Experiment | Result |
|---|------------|-----------|--------|
| H1 | Only the **array environment** swallows; stopping `parse_environment_body` at a bare `}` fixes it. | Patched `/tmp/lp_fixed` env-body loop to `break` on `RCurly`; ran diag on beat2014. | **REFUTED** — still 74 chars. The `\[` *display math* is opened *before* `\begin{array}`, so `parse_math_content` swallows first. |
| H2 | **Both** `parse_math_content` *and* `parse_environment_body` cross the enclosing `}`; both need the bare-`}` stop. | Added the same `RCurly → break` stop to `parse_math_content` too; re-ran diag. | **CONFIRMED** — beat2014 **74 → 23 263 chars**; abstract recovered (`grep persuasively` hits). |
| H3 | The fix generalizes to the 33 non-`\newenvironment` failing files (same scanner mechanism via top-level `\[`/`\begin`). | Ran the patched diag on `…/qm2pi/qm2pi.qmops.tex` (0 `\newcommand`/`\def`). | **CONFIRMED** — **74 → 10 309 chars.** |
| H4 | The bare-`}` stop does not regress the crate invariants (never-panic / lossless / `strict ⟺ has_errors` / incremental). | Full `latex-parser` test suite + clippy on the patched tree. | **CONFIRMED** — all suites `0 failed` (incl. the user's adversarial `verbatim`/`arg_attachment` cases); clippy clean. |

## 4. The fix

`latex-parser/src/parser.rs` — a bare `}` closes an enclosing group/argument, so
both content scanners stop there and hand the `}` back to the enclosing
`parse_delimited_content`, which closes the argument correctly. The construct is
recorded **unclosed** (`parse_environment_body` emits `UnclosedEnvironment`,
preserving `strict ⟺ has_errors`; `parse_math_content` simply stops — the
enclosing math close was already absent).

```rust
// parse_environment_body, after the EOF guard:
if matches!(self.cursor.peek(), LaTeXToken::Brace(BraceType::RCurly)) {
    self.record_error(ParseError::new(
        ParseErrorKind::UnclosedEnvironment, self.cursor.cur_span(),
        format!("Unclosed environment '{}'", expected),
    ).with_expected(vec![format!("\\end{{{}}}", expected)]));
    return (body, false);
}

// parse_math_content, after the matching-delimiter break:
if matches!(self.cursor.peek(), LaTeXToken::Brace(BraceType::RCurly)) {
    break;
}
```

Why a bare `}` is *always* an enclosing close here: a `{…}` group opened *within*
the scan is consumed by `parse_block → parse_group` (which eats its own matching
`}`). So a `}` only ever reaches the top of these loops when it has no opener
*inside* the current scan — i.e. it belongs to an enclosing scope. Stopping is
therefore lossless (the `}` is covered by the enclosing node, no coverage gap)
and terminating (still consumes ≥1 token/iter elsewhere).

## 5. Regression test (non-vacuous)

`latex-parser/tests/arg_attachment.rs::unbalanced_construct_in_arg_does_not_swallow_to_eof`
parses the beat2014 idiom + a trailing `\section{Body}` and prose, and asserts
the `\section` and prose are **top-level siblings** of `\newenvironment` (not
captured in its begin-def arg), plus `command_arg_text(\newenvironment)` excludes
the prose.

Proof it catches the bug (not vacuous): reverting **both** stops makes the test
fail with *“`\section` must be a top-level sibling, not swallowed into the
begin-def arg”*; restoring them makes it pass. The parse tree confirms the fixed
shape — body of 13 nodes: `Command(newenvironment) args=3`, then `Command(section)`
and the prose `Text` nodes as siblings.

## 6. Fuzz

`cargo +nightly fuzz run parse_no_panic` for 61 s (corpus seeded with the
begin-def-swallow pattern + `\[`-in-arg + `\begin`-in-arg variants):
**930 726 executions, 0 panics / 0 crashes / 0 leaks** (the fuzzer explored
`\renewenvironment`/`\newtheorem` around the seeds). Never-panic holds on the
changed recovery paths.

## 7. Live verification

Completed after `scripts/verify.sh` (all 8 gates green) → daemon restart on the
fixed binary → `reindex(language="latex")` (cleared 418 rows via the sanctioned
A6 tool) → startup scan re-extracted all 418 `.tex` with the fixed parser.

| Metric | Before fix | After fix |
|--------|-------:|------:|
| `beat2014.tex` rendered chars (diag) | 74 | 23 263 |
| `qm2pi.qmops.tex` rendered chars (diag) | 74 | 10 309 |
| `beat2014.tex` **live chunks** | 0 | **15** (abstract prose + math) |
| `qm2pi.qmops.tex` live chunks | 0 | 6 |
| `ex_nihilo_logic.tex` / `hctm.tex` (canonical) live chunks | 0 | 22 / 12 |
| LaTeX files carrying chunks | 352 / 418 | **364 / 418** (4 069 chunks) |
| LaTeX **latex extraction-failed** WARNs | n/a | **0** |
| LaTeX content **not indexed anywhere** | 66 | **0** |

beat2014's recovered chunks carry searchable Unicode math (`≡`×21, `∈`×15, `→`×12,
`∀`×10, `π`×6, `∃`×5, `⊆`, `×`) — the typing-judgement payload the renderer emits.

**The "zero-chunk" count is a red herring.** The raw per-row count fell only 66→54,
but **all 54 remaining zero-chunk rows are exact byte-duplicates** (their
`content_hash` = xxh3 of raw file bytes matches a chunk-bearing file — verified
100%, 0 not-dedup). pgmcp's production embed path (`src/embed/pool.rs::dedup`)
indexes byte-identical content **once** under a canonical path; copies register as
content-less rows. The workspace carries many duplicate paper trees (e.g. ten
`hctm.tex` copies across `arxiv/` subdirs). So **every LaTeX file's content is now
indexed** (directly or via its canonical) — content loss dropped from 66 swallowed
files to **0**.

The 32 `Document extraction failed` WARNs during the scan were **26 `doc` (catdoc),
4 `org` (pandoc), 2 `pdf`** — pre-existing, unrelated non-LaTeX extractor failures
on never-indexable files under `~/Documents/`. **Zero were LaTeX.**

## 8. Status — RESOLVED

- **latex-parser fix**: applied to `src/parser.rs` (byte-identical to the
  validated `/tmp/lp_fixed` copy); full suite + clippy + 930 726-exec fuzz green;
  non-vacuous regression test
  (`tests/arg_attachment.rs::unbalanced_construct_in_arg_does_not_swallow_to_eof`,
  proven to fail on revert).
- **pgmcp**: no code change required — the renderer walks the AST generically; the
  fix merely yields correctly-bounded ASTs. Re-verified via `scripts/verify.sh`
  (all 8 gates; latex-parser is a path dep) and confirmed live: 0 LaTeX content
  lost, 0 LaTeX extraction failures.
</content>
</invoke>
