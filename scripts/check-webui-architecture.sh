#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

fail() {
  printf 'webui architecture check failed: %s\n' "$1" >&2
  exit 1
}

require_pattern() {
  local pattern="$1"
  local path="$2"
  local message="$3"
  rg -q "$pattern" "$path" || fail "$message"
}

for required_file in \
  webui/src-cljs/pgmcp/webui/schema.cljs \
  webui/src-cljs/pgmcp/webui/model.cljs \
  webui/src-cljs/pgmcp/webui/domain.cljs \
  webui/src-cljs/pgmcp/webui/machine.cljs \
  webui/src-cljs/pgmcp/webui/events.cljs \
  webui/src-cljs/pgmcp/webui/fx.cljs \
  webui/src-cljs/pgmcp/webui/subs.cljs \
  webui/src-cljs/pgmcp/webui/views/shell.cljs \
  webui/src-cljs/pgmcp/webui/views/widgets.cljs \
  webui/src-cljs/pgmcp/webui/views/code.cljs \
  webui/src-cljs/pgmcp/webui/views/overview.cljs \
  webui/src-cljs/pgmcp/webui/views/query.cljs \
  webui/src-cljs/pgmcp/webui/views/events.cljs \
  webui/src-cljs/pgmcp/webui/views/mandates.cljs \
  webui/src-cljs/pgmcp/webui/views/work.cljs \
  webui/src-cljs/pgmcp/webui/views/resources.cljs \
  webui/src-cljs/pgmcp/webui/views/panel.cljs \
  webui/src-cljs/pgmcp/webui/views/layout.cljs \
  webui/src-cljs/pgmcp/webui/views/metrics.cljs \
  webui/src-cljs/pgmcp/webui/views/clients.cljs \
  webui/src-cljs/pgmcp/webui/views/database.cljs \
  webui/src-cljs/pgmcp/webui/views/logs.cljs \
  webui/src-cljs/pgmcp/webui/views/experiments.cljs \
  webui/src-cljs/pgmcp/webui/views/markdown.cljs \
  webui/src-cljs/pgmcp/webui/views/editor.cljs \
  webui/src-cljs/pgmcp/webui/viz.cljs \
  webui/src-cljs/pgmcp/webui/render.cljs
do
  [[ -f "$required_file" ]] || fail "missing required CLJS namespace file: $required_file"
done

render_hits="$(rg -n 'innerHTML|insertAdjacentHTML|dangerouslySetInnerHTML|<div class=|<span>|</|\[:(button|input|select|textarea)\b' webui/src-cljs webui/test-cljs || true)"
if [[ -n "$render_hits" ]]; then
  printf '%s\n' "$render_hits" >&2
  fail "raw DOM/string rendering or raw form controls found"
fi

# Markers are matched in their conventional MARKER form (uppercase TODO/FIXME/XXX,
# word-bounded hack/stub) so legitimate lowercase data — e.g. the "todo"/"fixme"
# work-item KIND vocabulary in schema.cljs — does not false-positive.
stale_marker_hits="$(rg -n '\bTODO\b|\bFIXME\b|\bXXX\b|\b[Hh]ack\b|\b[Ss]tubs?\b|future work|next regions|first implementation can keep|events-paused' \
  webui/src-cljs \
  webui/test-cljs \
  docs/design/webui-reframe-correction-plan.md \
  docs/decisions/034-webui-admin-console.md || true)"
if [[ -n "$stale_marker_hits" ]]; then
  printf '%s\n' "$stale_marker_hits" >&2
  fail "stale implementation marker or obsolete webui state found"
fi

dom_hits="$(rg -n '\.querySelector|\.addEventListener|createElement|\.getElementById' webui/src-cljs || true)"
dom_violations="$(printf '%s\n' "$dom_hits" | rg -v '^webui/src-cljs/pgmcp/webui/(core|fx)\.cljs:' || true)"
if [[ -n "$dom_violations" ]]; then
  printf '%s\n' "$dom_violations" >&2
  fail "DOM hooks outside boot/effect namespaces"
fi

edge_hits="$(rg -n 'js/window.*fetch|\.fetch|js/WebSocket|localStorage|URLSearchParams' webui/src-cljs || true)"
edge_violations="$(printf '%s\n' "$edge_hits" | rg -v '^webui/src-cljs/pgmcp/webui/fx\.cljs:' || true)"
if [[ -n "$edge_violations" ]]; then
  printf '%s\n' "$edge_violations" >&2
  fail "browser edge APIs outside fx namespace"
fi

pure_hits="$(rg -n 're-frame\.core|reagent|re-com\.core|js/window|js/document|js/WebSocket|\.fetch|localStorage|URLSearchParams|\.addEventListener|\.getElementById' \
  webui/src-cljs/pgmcp/webui/schema.cljs \
  webui/src-cljs/pgmcp/webui/model.cljs \
  webui/src-cljs/pgmcp/webui/domain.cljs \
  webui/src-cljs/pgmcp/webui/machine.cljs || true)"
if [[ -n "$pure_hits" ]]; then
  printf '%s\n' "$pure_hits" >&2
  fail "pure schema/model/domain/machine namespaces import reactive or browser edge APIs"
fi

dispatch_hits="$(rg -n 'rf/dispatch' webui/src-cljs || true)"
dispatch_violations="$(printf '%s\n' "$dispatch_hits" | rg -v '^webui/src-cljs/pgmcp/webui/(core|fx|views/[^/]+)\.cljs:' || true)"
if [[ -n "$dispatch_violations" ]]; then
  printf '%s\n' "$dispatch_violations" >&2
  fail "imperative dispatch outside boot/effect/view namespaces"
fi

endpoint_hits="$(rg -n '"/(api|webui)/' webui/src-cljs || true)"
endpoint_violations="$(printf '%s\n' "$endpoint_hits" | rg -v '"/api/(stats|query|mandates|work_items|resources|metrics|clients|db|logs|experiments)\b|"/webui/(ws|grammars)\b' || true)"
if [[ -n "$endpoint_violations" ]]; then
  printf '%s\n' "$endpoint_violations" >&2
  fail "browser references endpoints outside the closed web UI surface"
fi

mutation_hits="$(rg -n '\b(atom|reset!|swap!)\b' webui/src-cljs || true)"
mutation_violations="$(printf '%s\n' "$mutation_hits" | rg -v '^webui/src-cljs/pgmcp/webui/(core|fx)\.cljs:' || true)"
if [[ -n "$mutation_violations" ]]; then
  printf '%s\n' "$mutation_violations" >&2
  fail "ad hoc mutable cells outside boot/effect namespaces"
fi

rg -q "wasm-unsafe-eval" webui/src/assets.rs || fail "assets.rs CSP does not allow 'wasm-unsafe-eval' for tree-sitter WASM"
rg -q 'reagent.dom.client' webui/src-cljs/pgmcp/webui/core.cljs || fail "core.cljs does not mount through Reagent"
rg -q 'rf/reg-event-fx' webui/src-cljs/pgmcp/webui/events.cljs || fail "events.cljs does not register re-frame event handlers"
rg -q 'rf/reg-fx' webui/src-cljs/pgmcp/webui/fx.cljs || fail "fx.cljs does not register re-frame effects"
rg -q 'rf/reg-sub' webui/src-cljs/pgmcp/webui/subs.cljs || fail "subs.cljs does not register re-frame subscriptions"
rg -q 're-com.core' webui/src-cljs/pgmcp/webui/views || fail "view namespaces do not use re-com"
rg -q 'machine/run' webui/src-cljs/pgmcp/webui/events.cljs || fail "semantic events do not enter the CESK/statechart machine"
rg -q ':machine \(initial-machine\)' webui/src-cljs/pgmcp/webui/model.cljs || fail "app-db no longer stores the serializable machine"

require_pattern ':query \(get-in model \[:regions :query :initial\]\)' \
  webui/src-cljs/pgmcp/webui/model.cljs \
  "initial store does not derive query control state from the model"
require_pattern ':mandates \(get-in model \[:regions :mandates :initial\]\)' \
  webui/src-cljs/pgmcp/webui/model.cljs \
  "initial store does not derive mandates control state from the model"
require_pattern ':work \(get-in model \[:regions :work :initial\]\)' \
  webui/src-cljs/pgmcp/webui/model.cljs \
  "initial store does not derive work control state from the model"
require_pattern ':events \(get-in model \[:regions :events :initial\]\)' \
  webui/src-cljs/pgmcp/webui/model.cljs \
  "initial store does not derive events control state from the model"

require_pattern 'max-preview-chars' webui/src-cljs/pgmcp/webui/schema.cljs \
  "schema does not define a bounded payload preview limit"
require_pattern 'preview-text' webui/src-cljs/pgmcp/webui/views/common.cljs \
  "common views do not expose a bounded preview helper"
require_pattern 'json-preview' webui/src-cljs/pgmcp/webui/views/events.cljs \
  "event payload previews are not routed through the bounded preview helper"
require_pattern 'preview-text' webui/src-cljs/pgmcp/webui/views/query.cljs \
  "query snippets are not routed through the bounded preview helper"

for target in \
  ':\$query/run-target' \
  ':\$query/loaded-target' \
  ':\$query/error-target' \
  ':\$mandates/loaded-target' \
  ':\$mandates/error-target' \
  ':\$work/loaded-target' \
  ':\$work/error-target' \
  ':\$resources/loaded-target' \
  ':\$resources/error-target' \
  ':\$panel/loaded-target' \
  ':\$panel/error-target'
do
  require_pattern "$target" webui/src-cljs/pgmcp/webui/model.cljs \
    "model is missing dynamic statechart target $target"
  require_pattern "$target" webui/src-cljs/pgmcp/webui/machine.cljs \
    "machine does not resolve dynamic statechart target $target"
done

require_pattern 'hast->hiccup' webui/src-cljs/pgmcp/webui/render.cljs \
  "render namespace does not expose the hast->hiccup markdown transform"
require_pattern 'spans->hiccup' webui/src-cljs/pgmcp/webui/render.cljs \
  "render namespace does not expose the spans->hiccup code transform"

require_pattern 'resolve-target seed region' webui/src-cljs/pgmcp/webui/machine.cljs \
  "dynamic transition targets are not resolved from the pre-event snapshot"
require_pattern 'current-request\?' webui/src-cljs/pgmcp/webui/machine.cljs \
  "machine lacks current-request guard for stale REST completions"
require_pattern ':request-id \(:request-id effect\)' webui/src-cljs/pgmcp/webui/events.cljs \
  "re-frame effects do not preserve machine request ids"
require_pattern ':control/query' webui/src-cljs/pgmcp/webui/subs.cljs \
  "subscriptions do not expose query control region"
require_pattern ':control/mandates' webui/src-cljs/pgmcp/webui/subs.cljs \
  "subscriptions do not expose mandates control region"
require_pattern ':control/work' webui/src-cljs/pgmcp/webui/subs.cljs \
  "subscriptions do not expose work control region"
require_pattern ':control/events' webui/src-cljs/pgmcp/webui/subs.cljs \
  "subscriptions do not expose events control region"
require_pattern 'browser statechart has ten orthogonal regions' \
  docs/decisions/034-webui-admin-console.md \
  "ADR does not document the ten-region browser statechart"
require_pattern '## Vinary-viewer Review' \
  docs/decisions/034-webui-admin-console.md \
  "ADR does not preserve the vinary-viewer architecture comparison"
require_pattern 'No planned CESK/statechart benefit is intentionally traded away' \
  docs/decisions/034-webui-admin-console.md \
  "ADR does not explicitly preserve CESK/statechart benefits"
require_pattern 'Implemented orthogonal regions' \
  docs/design/webui-reframe-correction-plan.md \
  "correction plan does not document implemented statechart regions"
require_pattern 'vinary-viewer` local docs' \
  docs/design/webui-reframe-correction-plan.md \
  "correction plan does not record the vinary-viewer evidence source"

printf 'webui architecture check passed\n'
