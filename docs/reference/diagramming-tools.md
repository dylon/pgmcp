# Diagramming tools — toolbox catalog reference

This document accompanies the **`diagramming` domain** of the pgmcp toolbox tool
catalog (`src/tools_catalog/`, table `tool_cards`, surfaced via the `toolbox_*`
MCP tools). It lists the installed, agent-invocable diagramming utilities that
are cataloged as "cards", the tools that are installed but *not* carded (with the
reason), and the additional tools recommended for software-engineering /
scientific / architectural / security diagramming — each with an install command.

The catalog's invariant: **a card is a tool an agent can actually invoke
headlessly on this machine.** Every card's `availability`/`invocation` is
probe-grounded (real binary, path, and version captured by `--version` /
`command -v` / `kpsewhich`). GUI-only tools with no scriptable surface are
documented here but not carded.

> Domain summary: **42 cards across 8 categories** — `graph_layout`,
> `uml_architecture`, `scientific_plotting`, `diagram_language`, `ascii_diagram`,
> `diagram_conversion`, `circuit_diagram`, `protocol_data_diagram`.

Query them with the MCP tools, e.g.
`toolbox_search {query:"C4 software architecture diagram", domain:"diagramming"}`,
`toolbox_list {domain:"diagramming"}`, or
`toolbox_recommend {task:"render a call graph as SVG"}`.

──────────────────────────────────────────────────────────────────────────────

## Installed & carded (42)

### graph_layout
| slug | tool | version | invocation gist |
|··········|··········|··········|··········|
| `graphviz` | Graphviz | 14.1.5 | `dot -Tsvg g.dot -o g.svg` (+ neato/fdp/sfdp/twopi/circo) |
| `gvpr` | gvpr | 14.1.5 | `gvpr -c 'N[indegree==0]' g.dot` (graph stream editor) |

### uml_architecture
| slug | tool | version | invocation gist |
|··········|··········|··········|··········|
| `plantuml` | PlantUML | 1.2026.5 | `plantuml -tsvg diagram.puml` |
| `mermaid-cli` | Mermaid CLI (mmdc) | 11.15.0 | `mmdc -i d.mmd -o d.svg` |
| `d2` | D2 (Terrastruct) | 0.7.1 | `d2 --layout elk a.d2 a.svg` |
| `structurizr` | Structurizr CLI | 2026.05.16 | `structurizr export -workspace w.dsl -format mermaid` |
| `drawio` | draw.io desktop | 30.0.4 | `xvfb-run -a drawio -x -f svg -o o.svg d.drawio` |
| `mscgenjs` | mscgenjs | 5.0.1 | `mscgenjs -T svg -i h.msc -o h.svg` |
| `umlet` | UMLet | 15.1 | `umlet -action=convert -format svg -filename d.uxf` |
| `dbml-renderer` | dbml-renderer | 1.0.31 | `dbml-renderer -i schema.dbml -o schema.svg` |
| `kroki` | Kroki (render gateway; ~22 engines via core + bpmn/excalidraw companions) | 0.31.0 | `curl :8000/plantuml/svg --data-binary @d.puml -o d.svg` |

> **Kroki setup.** Carded against a `docker compose` stack (the `yuzutech/kroki`
> core + the `kroki-bpmn` and `kroki-excalidraw` companions) on `localhost:8000`.
> It renders ~22 engines: the JVM-core set (graphviz, plantuml, c4plantuml, d2,
> structurizr, erd, dbml, nomnoml, umlet, bytefield, wavedrom, tikz, pikchr,
> svgbob, ditaa, goat, vega, vegalite, wireviz, symbolator) plus the bpmn +
> excalidraw companions. **Mermaid and diagrams.net companions are deliberately
> not run** — the local `mermaid-cli`/`drawio` cards cover those — and the
> **`kroki-blockdiag` companion image is broken** (HTTP 200 + empty body), so the
> blockdiag suite is unavailable via Kroki too. `/health` lists a version for
> every engine even when its companion isn't running, so trust a real render, not
> the manifest.

### scientific_plotting
| slug | tool | version | invocation gist |
|··········|··········|··········|··········|
| `gnuplot` | gnuplot | 6.0p4 | `gnuplot -e "set term svg; set out 'p.svg'; plot 'd' w l"` |
| `plotutils` | GNU plotutils | 2.6 | `graph -T svg -C data > plot.svg` |
| `matplotlib` | Matplotlib | 3.10.9 | `python -c "...; plt.savefig('p.svg')"` (Agg) |
| `pgfplots` | PGFPlots | TeX Live 2026 | `\addplot table {d.dat};` → lualatex → dvisvgm |
| `vega-cli` | Vega CLI | 6.2.0 | `vg2svg chart.vg.json chart.svg` |
| `veusz` | Veusz | 4.2 | `veusz --export=f.pdf f.vsz` (+ `veusz.embed`) |
| `r-graphics` | R / Rscript | 4.6.0 | `Rscript -e 'svg("p.svg"); plot(x); dev.off()'` |
| `ggplot2` | ggplot2 | 4.0.3 | `Rscript -e 'library(ggplot2); ggsave("p.svg", ...)'` |
| `seaborn` | seaborn | 0.13.2 | `python -c "...sns.heatmap...; plt.savefig('h.svg')"` |
| `plotly` | Plotly | 6.8.0 | `fig.write_html('c.html')` / `fig.write_image('c.svg')` |
| `altair` | Vega-Altair | 6.2.1 | `chart.save('c.svg')` (needs `python-vl-convert`) |

### diagram_language
| slug | tool | version | invocation gist |
|··········|··········|··········|··········|
| `metapost` | MetaPost | 3.00 | `mpost figures.mp` → EPS; `mptopdf` |
| `tikz` | TikZ / PGF | TeX Live 2026 | `\begin{tikzpicture}…` → lualatex |
| `pic` | GNU pic | 1.24.1 | `pic d.pic | groff -p -Tps`; `pic2plot -Tsvg` |
| `asymptote` | Asymptote | 3.12 | `asy -f svg -o fig fig.asy` (2D/3D) |
| `pikchr` | Pikchr | 1.0 | `pikchr d.pikchr > d.svg` (PIC → self-contained SVG) |

### ascii_diagram
| slug | tool | version | invocation gist |
|··········|··········|··········|··········|
| `ditaa` | ditaa | 0.11.0 | `ditaa diagram.txt diagram.png` / `--svg` |
| `svgbob` | Svgbob | 0.7.6 | `svgbob_cli diagram.txt -o diagram.svg` |

### diagram_conversion
| slug | tool | version | invocation gist |
|··········|··········|··········|··········|
| `imagemagick` | ImageMagick | 7.1.2-25 | `magick -density 300 d.pdf d.png` |
| `graphicsmagick` | GraphicsMagick | 1.3.47 | `gm convert d.svg d.png` |
| `rsvg-convert` | rsvg-convert (librsvg) | 2.62.3 | `rsvg-convert -f pdf -o o.pdf in.svg` |
| `dvisvgm` | dvisvgm | 3.6 | `dvisvgm figure.dvi -o figure.svg` |
| `dot2tex` | dot2tex | 2.12.0 | `dot2tex -ftikz g.dot > g.tex` |
| `inkscape` | Inkscape (CLI) | 1.4.4 | `inkscape in.svg --export-type=pdf --export-filename=o.pdf` |
| `fig2dev` | fig2dev (Transfig) | 3.2.9 | `fig2dev -L svg d.fig d.svg` (Xfig's CLI) |
| `calligraconverter` | Calligra/Karbon CLI | 26.04.2 | `calligraconverter d.odg d.svg` |

### circuit_diagram
| slug | tool | version | invocation gist |
|··········|··········|··········|··········|
| `circuitikz` | CircuiTikZ | TeX Live 2026 | `\draw (0,0) to[R] (2,0);` → lualatex |
| `kicad` | KiCad (kicad-cli) | 10.0.3 | `kicad-cli sch export svg b.kicad_sch`; `pcb drc` |
| `xcircuit` | XCircuit | 3.10.30 | interactive + Tcl batch → EPS |

### protocol_data_diagram
| slug | tool | version | invocation gist |
|··········|··········|··········|··········|
| `bytefield-svg` | bytefield-svg | 1.11.0 | `bytefield-svg -s packet.edn -o packet.svg` |
| `wavedrom-cli` | WaveDrom CLI | 3.2.0 | `wavedrom-cli -i wave.json -s wave.svg` |

**Install-location notes.** The Node tools (`mscgenjs`, `vega-cli`/`vg2svg`,
`bytefield-svg`, `wavedrom-cli`, `dbml-renderer`) are installed in the
**workspace-parent** `node_modules`
(`/home/dylon/Workspace/f1r3fly.io/node_modules/.bin/`), not inside the pgmcp
crate; invoke by absolute path or `npx` from the workspace. `svgbob` is the
`svgbob_cli` binary in `~/.cargo/bin`. The Python plotting libs
(`matplotlib`/`seaborn`/`plotly`/`altair`) are pacman packages under
`/usr/lib/python3.14/site-packages`. `tikz`/`pgfplots`/`circuitikz` are TeX Live
packages resolved via `kpsewhich` and compiled through `lualatex`/`pdflatex`.

**Altair static export:** install the renderer with `sudo pacman -S
python-vl-convert` (in the `extra` repo) — without it, `chart.save('out.svg')`
cannot rasterize headlessly (interactive HTML still works, and the installed
`vega-cli` can render a saved Vega spec).

──────────────────────────────────────────────────────────────────────────────

## Installed but NOT carded (and why)

A card promises headless agent invocation. These installed tools don't currently
qualify, so they're documented instead:

| tool | status | reason / agent path |
|··········|··········|··········|
| **blockdiag suite** (blockdiag/seqdiag/actdiag/nwdiag/packetdiag/rackdiag) | broken — no working path | Runtime-broken under Python 3.14: imports the removed `pkg_resources` *and* Pillow-10's removed `ImageDraw.textsize`. Fix needs `pipx inject --force <pkg> 'setuptools<81' 'Pillow<10'` per venv (attempted; still fails — the codebase predates both removals). The **`kroki-blockdiag` companion image is also broken** — it returns HTTP 200 with an empty body for all six types (probe-verified 2026-06-16), so Kroki does not rescue it either. No headless path currently available. |
| **erd** (Haskell) | won't build → use `kroki` | `cabal install erd` fails: `erd` pins `text >=1 && <2` but modern `hashable` needs `text >=2.0.2` — an unsatisfiable constraint set on GHC 9.12. **Agent path: the carded `kroki` server (`POST :8000/erd/svg`, erd 0.2.3 bundled).** |
| **LabPlot** | GUI-only | KDE interactive plotting app with no headless export CLI. Use `gnuplot`/`matplotlib`/`veusz`/`r-graphics` for scripted scientific plots. |
| **Xfig** | GUI-only | Interactive `.fig` editor. Its agent path **is carded** as `fig2dev`. |
| **Karbon** (Calligra) | GUI-only | Interactive vector editor. Its agent path **is carded** as `calligraconverter`. |
| **Inkscape**, **KiCad** | carded | Primarily GUIs, but both ship real headless CLIs (`inkscape --export-*`, `kicad-cli`) — carded on that surface. |

──────────────────────────────────────────────────────────────────────────────

## Recommended (agent-usable; not installed)

CLI/headless tools worth adding, grouped by purpose. GUI-first apps are
deliberately excluded. Install commands assume Arch (`pacman`/AUR), `npm`,
`pipx`, `cabal`, or Docker as noted.

### Software architecture
- **nomnoml** — UML sketches from text. `npm install -g nomnoml` for a standalone
  CLI (also renderable now via the carded `kroki` server: `POST :8000/nomnoml/svg`).
- **C4-PlantUML** — C4-model stdlib for the installed PlantUML (not a package):
  `git clone https://github.com/plantuml-stdlib/C4-PlantUML` then `!include` it
  (Kroki also serves it directly as the `c4plantuml` engine).

### Database / data modeling
- **erd** — entity-relationship from a tiny DSL. `cabal install erd` (currently
  unbuildable — see above; try AUR `erd` or a pinned `text<2` if needed). Already
  renderable via the carded `kroki` server (`POST :8000/erd/svg`).

### Scientific
- **python-vl-convert** — altair's headless SVG/PNG renderer. `sudo pacman -S python-vl-convert`
- **python-kaleido** — Plotly's static-image export engine. `sudo pacman -S python-kaleido`
- **GLE** (Graphics Layout Engine) — publication plots. AUR `gle-graphics`
- **ploticus** — fast scripted data plots. AUR `ploticus`

### Software engineering / general
- **Graph::Easy** — ASCII/box-art graphs + format conversion. `cpan -i Graph::Easy` (or AUR `perl-graph-easy`)
- **aafigure** — ASCII-art → PNG/SVG. `pipx install aafigure`

### Circuits / EDA
- **circuit_macros** — m4 + gpic circuit schematics (m4 is installed). AUR `circuit_macros`

### Already covered (skip)
- Classic `mscgen` — the carded `mscgenjs` covers Message Sequence Charts.
- `seaborn`/`plotly`/`altair` — already installed (pacman) and carded.

──────────────────────────────────────────────────────────────────────────────

## Maintenance

- New cards land via `src/tools_catalog/diagramming_{graph,render,plotting}.rs`
  (`tool(...)` seeds). The referential-integrity tests in
  `src/tools_catalog/mod.rs` validate slug uniqueness, domain/category integrity,
  and `alternatives` cross-links.
- The `diagramming` domain is admitted by the **v35** CHECK-widen migration
  (`src/db/migrations/v35_toolbox_domain_diagramming.rs`), built from
  `ToolDomain::sql_in_list()` (ADR-003 single-source-of-truth idiom).
- Pure card additions need **no** embedding-signature bump
  (`DEV_TOOL_EMBEDDING_SIGNATURE` in `src/db/tool_cards.rs`); new cards insert with
  a NULL embedding that the embedding-migration cron backfills. On an
  already-seeded daemon, reach new cards via `toolbox_refresh {mode:"reembed"}`
  after the v35 migration applies on restart.
