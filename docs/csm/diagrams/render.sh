#!/usr/bin/env bash
# Render every diagram source under src/ to a committed SVG in this directory.
#
# Usage:
#   ./render.sh                 # render every src/* whose extension is known
#   ./render.sh src/foo.puml …  # render only the named sources
#
# Source extension → tool (all from pgmcp/docs/reference/diagramming-tools.md):
#   .puml → PlantUML     (plantuml -tsvg)   ← PREFERRED for sequence/state/activity/class
#   .dot  → Graphviz     (dot -Tsvg)        ← ASTs, trees, dependency + traceability graphs
#   .d2   → D2           (d2)               ← nested-container architecture
#   .tex  → TikZ/PGF     (pdflatex → pdftocairo -svg)  ← math, nested sets, stacks, string diagrams
#   .py   → Matplotlib   (python3 script; receives the output SVG path as argv[1])
#
# NB: Mermaid is deliberately NOT used in this treatise (PlantUML is preferred per
# the project owner). There are no .mmd sources here.
#
# Pinned palette — one concept = one colour (doc guideline 14, "intuitive
# colorization per concept"). Use these hex values in every source so the whole
# set is visually consistent. Each concept: main / fill (light) / stroke (dark).
#   Role / participant      = indigo  #4f46e5  (fill #e0e7ff, stroke #3730a3)
#   Send · internal-choice ⊕ = emerald #059669  (fill #d1fae5, stroke #047857)
#   Recv · external-choice & = sky     #0284c7  (fill #e0f2fe, stroke #075985)
#   Push · Call  (Σ_call)    = violet  #7c3aed  (fill #ede9fe, stroke #6d28d9)
#   Pop  · Return(Σ_ret)     = fuchsia #c026d3  (fill #fae8ff, stroke #a21caf)
#   Internal · neutral(Σ_int)= slate   #475569  (fill #e2e8f0, stroke #334155)
#   Medium: Text             = green   #16a34a  (fill #dcfce7, stroke #15803d)
#   Medium: Latent           = amber   #d97706  (fill #fef3c7, stroke #b45309)
#   GlobalType / type layer  = indigo  #3730a3  (fill #e0e7ff, stroke #312e81)
#   LocalMachine / runtime   = teal    #0d9488  (fill #ccfbf1, stroke #0f766e)
#   Conformance: accept      = green   #16a34a  (fill #dcfce7, stroke #15803d)
#   Conformance: reject      = red     #dc2626  (fill #fee2e2, stroke #b91c1c)
#   Critic gate / verify     = gold    #ca8a04  (fill #fef9c3, stroke #a16207)
#   Mailbox plane            = cyan    #0891b2  (fill #cffafe, stroke #0e7490)
#   Task plane               = orange  #ea580c  (fill #ffedd5, stroke #c2410c)
#   RLM frame / pushdown store= violet #7c3aed  (fill #ede9fe, stroke #6d28d9)
#   pi (file work)           = green   #16a34a  (fill #dcfce7, stroke #15803d)
#   pgmcp (analytical)       = indigo  #4f46e5  (fill #e0e7ff, stroke #3730a3)
set -euo pipefail
cd "$(dirname "$0")"
shopt -s nullglob

render_one() {
  local f="$1" name ext tmp
  name="$(basename "${f%.*}")"
  ext="${f##*.}"
  # Skip helper/library sources (e.g. _palette.py) — they render nothing.
  case "$name" in _*) return 0 ;; esac
  case "$ext" in
    dot)  dot -Tsvg "$f" -o "./$name.svg" ;;
    d2)   d2 --pad 20 "$f" "./$name.svg" ;;
    puml) plantuml -tsvg -o "$PWD" "$f" ;;
    py)   python3 "$f" "./$name.svg" ;;
    tex)
      tmp="$(mktemp -d)"
      if ! pdflatex -interaction=nonstopmode -halt-on-error -output-directory="$tmp" "$f" >"$tmp/log" 2>&1; then
        echo "  FAIL $name.svg (pdflatex)"; tail -15 "$tmp/log" | sed 's/^/        /'; rm -rf "$tmp"; return 1
      fi
      pdftocairo -svg "$tmp/$name.pdf" "./$name.svg"
      rm -rf "$tmp" ;;
    svg)  return 0 ;;   # a hand-authored SVG, nothing to render
    *)    return 0 ;;
  esac
  if [ -s "./$name.svg" ]; then echo "  ok   $name.svg"; else echo "  FAIL $name.svg (empty output)"; return 1; fi
}

rc=0
if [ "$#" -gt 0 ]; then
  for f in "$@"; do render_one "$f" || rc=1; done
else
  for f in src/*; do [ -f "$f" ] && { render_one "$f" || rc=1; }; done
fi
exit "$rc"
