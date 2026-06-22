#!/usr/bin/env bash
# Render every diagram source under src/ to a committed SVG in this directory.
#
# Usage:
#   ./render.sh                 # render every src/* whose extension is known
#   ./render.sh src/foo.mmd …   # render only the named sources
#
# Source extension → tool (all from pgmcp/docs/reference/diagramming-tools.md):
#   .mmd  → Mermaid CLI  (mmdc, headless via puppeteer.config.json --no-sandbox)
#   .dot  → Graphviz     (dot -Tsvg)
#   .d2   → D2           (d2)
#   .puml → PlantUML     (plantuml -tsvg)
#   .py   → Matplotlib   (python3 script; receives the output SVG path as argv[1])
#   .tex  → TikZ/PGF     (pdflatex → pdftocairo -svg)
#
# Pinned palette (use these hex values in every source so the whole set is
# visually consistent — "intuitive colorization per concept", doc guideline 14):
#   data plane    = teal   #0d9488  (fill #ccfbf1, stroke #0f766e)
#   control plane = indigo #4f46e5  (fill #e0e7ff, stroke #3730a3)
#   verb surface  = amber  #d97706  (fill #fef3c7, stroke #b45309)
#   corpus / pg   = slate  #475569  (fill #e2e8f0, stroke #334155)
#   trusted zone  = green  #16a34a  (fill #dcfce7, stroke #15803d)
#   untrusted     = red    #dc2626  (fill #fee2e2, stroke #b91c1c)
#   page states:  resident/clean = green #16a34a  (fill #dcfce7)
#                 dirty          = amber #d97706  (fill #fef3c7)
#                 evicted        = grey  #6b7280  (fill #e5e7eb)
#                 spilled (OOC)  = blue  #2563eb  (fill #dbeafe)
#                 pinned         = blue  #1d4ed8  (fill #dbeafe)
#                 summary node   = purple#7c3aed  (fill #ede9fe)
#   decision flow: admit=green #16a34a, evict=amber #d97706, exhausted=red #dc2626,
#                  demote=purple #7c3aed
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
    mmd)  mmdc -q -p puppeteer.config.json -c mermaid.config.json -b transparent -i "$f" -o "./$name.svg" ;;
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
