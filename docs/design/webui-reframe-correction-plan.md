# pgmcp Web UI Re-frame Correction Plan

- Status: active correction plan
- Date: 2026-07-03
- Scope: `webui/`, `/webui`, `/webui/ws`, `/api/stats`, `/api/query`, `/api/mandates`, `/api/work_items`

## Purpose

This plan corrects the pgmcp web UI implementation back to the intended
architecture: a React application written in ClojureScript with Reagent,
re-frame, and re-com, backed by the planned CESK/statechart control plane.

The correction is not a cosmetic rewrite. The frontend must be a principled
reactive application:

- Reagent renders React components.
- re-frame owns application events, effects, subscriptions, and `app-db`.
- re-com supplies the reusable control and layout components.
- The CESK/statechart machine remains the single semantic transition authority.
- The daemon remains the authority for privileged backend work.

## Planning Evidence

The direct peer planning agents were attempted before writing this plan:

| Planning source | Result |
|---|---|
| `architect` A2A peer | Transport failed before task execution. |
| `orchestrator` A2A peer | Transport failed before task execution. |
| `web-author` A2A peer | Transport failed before task execution. |
| `codex-cli` A2A peer | Completed, but returned an empty artifact. |
| pgmcp memory search | Recovered prior-plan fragments naming a "CESK abstract machine on re-frame", an authored hierarchical and parallel statechart, and a `webui/cljs/src/webui/machine/` style split. |
| Current ADR | `docs/decisions/034-webui-admin-console.md` preserves the CESK/statechart, bounded store, effect-as-data, and transactional websocket replay requirements, but must be amended to make Reagent, re-frame, and re-com explicit. |
| `vinary-viewer` local docs | `docs/theory/01-reactive-architecture.md`, `docs/architecture/01-overview.md`, and `docs/design-decisions/0010-bounded-content-retention-and-render-metadata.md` confirm useful mediator, event/effect, state-ownership, and bounded-retention discipline. ADR-034 records why those benefits are adopted without replacing pgmcp's CESK/statechart. |

## Terms

| Term | Meaning |
|---|---|
| React | The browser UI rendering model targeted by Reagent. |
| Reagent | The ClojureScript interface to React used to define components as data. |
| re-frame | The ClojureScript event/effect/subscription architecture used to manage `app-db` and drive reactive rendering. |
| re-com | A Reagent component library used for predictable, accessible application controls and layouts. |
| CESK | Control, Environment, Store, Kontinuation. In this UI, `C` is the current event, `E` is effect adapters/runtime inputs, `S` is domain and UI state, and `K` is the pushdown navigation stack. |
| Statechart | A model-declared hierarchical and orthogonal state machine. It decides valid control transitions before re-frame commits state. |
| Effect as data | A declarative effect value returned by the pure machine and interpreted only at the re-frame edge. |
| Closed REST surface | Browser access only through `/api/stats`, `/api/query`, `/api/mandates`, and `/api/work_items`; no generic SQL or MCP browser client. |

## Non-negotiable Requirements

1. The web UI must use Reagent, re-frame, and re-com.
2. Re-frame event handlers must be the only path that mutates frontend state.
3. The CESK/statechart machine must be the only path that decides semantic
   state transitions.
4. Views must be Reagent/re-com components, not string-built HTML.
5. The implementation must not use `innerHTML` for application rendering.
6. Manual DOM listeners are forbidden for application controls; controls dispatch
   re-frame events through component props.
7. Edge-only DOM integration is allowed for bootstrapping React into the single
   root node and for global effects such as keyboard hooks, websocket events,
   and localStorage.
8. Effects must be returned as data by the machine and interpreted by re-frame
   `reg-fx` handlers.
9. The websocket replay protocol must remain resumable and bounded.
10. Event logs, reject logs, payload previews, and caches must remain bounded.
11. Topic filters and pause/resume must not lose events that the user would
    reasonably expect to see after re-enabling a topic.
12. The browser must not become a broad SQL client or a broad MCP client.
13. Backend and external dependency worktrees must not be edited to make the
    frontend correction convenient.

## Architecture

```mermaid
flowchart TB
  classDef view fill:#233143,stroke:#83b4ff,color:#ffffff
  classDef rf fill:#263822,stroke:#74c365,color:#ffffff
  classDef machine fill:#3a2f13,stroke:#e3b341,color:#ffffff
  classDef edge fill:#35223d,stroke:#c49cff,color:#ffffff
  classDef daemon fill:#431f28,stroke:#ef7777,color:#ffffff

  U[Operator]:::view
  V[Reagent + re-com views]:::view
  S[re-frame subscriptions]:::rf
  E[re-frame events]:::rf
  DB[(re-frame app-db)]:::rf
  M[Pure CESK/statechart machine]:::machine
  FX[re-frame fx interpreters]:::edge
  API[Closed REST endpoints]:::daemon
  WS[/webui/ws replay stream]:::daemon

  U --> V
  V -->|dispatch vectors| E
  S --> V
  DB --> S
  E -->|machine/dispatch| M
  M -->|machine' + effect data| E
  E --> DB
  E --> FX
  FX --> API
  FX --> WS
  API -->|loaded/error events| E
  WS -->|frame events| E
```

The critical invariant is:

```text
component event -> re-frame event -> CESK step -> app-db commit + effect data -> re-frame fx
```

No component may update nested state directly. No effect handler may decide a
state transition directly. Server responses and websocket frames re-enter the
same re-frame event path as user input.

## Namespace Plan

The current monolithic `pgmcp.webui.core` must be replaced by these namespaces:

| Namespace | Responsibility | Forbidden |
|---|---|---|
| `pgmcp.webui.schema` | Constants, topic set, event names, small validation predicates. | Fetch, websocket, DOM, React rendering. |
| `pgmcp.webui.model` | Initial CESK machine and authored statechart model. | re-frame registration, effects, DOM. |
| `pgmcp.webui.machine` | Pure `step`, `run`, target resolution, bounded reject handling, transition application. | Async, fetch, websocket, localStorage, DOM. |
| `pgmcp.webui.domain` | Pure store helpers: event rings, topic watermarks, query row normalization, mandate shaping. | Effects, DOM, re-frame registration. |
| `pgmcp.webui.events` | `re-frame.core/reg-event-fx` and `reg-event-db` declarations. Calls the pure machine for semantic events. | Rendering, direct websocket operations. |
| `pgmcp.webui.fx` | `reg-fx` edge interpreters for REST calls, websocket lifecycle, localStorage, and keyboard hooks. | Semantic state decisions. |
| `pgmcp.webui.subs` | `reg-sub` derived values for views. | State mutation, effects. |
| `pgmcp.webui.views.shell` | Topbar, navigation, connection controls, page frame. | Direct DOM mutation. |
| `pgmcp.webui.views.overview` | Status/index/cron/client/telemetry/counter panels. | REST calls. |
| `pgmcp.webui.views.query` | Query form and results. | REST calls, direct state writes. |
| `pgmcp.webui.views.events` | Topic filters, pause/resume, clear, event log, replay summary. | Websocket operations. |
| `pgmcp.webui.views.mandates` | Mandate filters and source list. | REST calls. |
| `pgmcp.webui.views.work` | Read-only tracker smart-view form and work item rows. | REST calls, tracker mutations. |
| `pgmcp.webui.core` | Boot only: initialize app-db, install edge hooks, mount Reagent root. | Application rendering logic beyond mounting root component. |

## re-frame App-db Schema

`app-db` is the reactive container. The CESK machine is the semantic payload.

```clojure
{:machine
 {:c nil
  :e {:now <effect-supplied-clock-id>}
  :s {:control {:view :overview
                :connection :idle
                :activity :ready
                :query :editing
                :mandates :idle
                :work :idle
                :events :streaming}
      :ui {:stats-kind :status
           :query {:mode :semantic
                   :text ""
                   :project ""
                   :limit "10"}
           :mandates {:scope :all
                      :project ""}
           :work {:view :next-actionable
                  :assignee ""
                  :limit "25"
                  :plan-public-id ""}
           :topics {:tracker true
                    :mandate true
                    :cron true
                    :task true
                    :index true
                    :client true
                    :scanner true
                    :control true
                    :trace true
                    :status true}}
      :domain {:stats {}
               :query-result nil
               :mandates-result nil
               :work-result nil
               :applied-seq 0
               :server-seq 0
               :topic-seqs {}
               :requests {:next-id 0
                          :pending {}
                          :stats {}
                          :query nil
                          :mandates nil
                          :work nil}}
      :rings {:events []
              :queued-events []
              :rejects []}}
  :k [{:kind :view :view :overview}]}
 :runtime {:token ""}}
```

Notes:

- `runtime` is re-frame edge state for serializable browser settings such as
  the remembered token. Non-serializable websocket objects stay outside
  `app-db` in the fx namespace.
- `machine.s` must remain serializable so debugging, replay, export, and
  time-travel remain possible.
- Field names should be keyword-based in CLJS. JSON conversion is isolated at
  the API and websocket boundaries.
- `:ui :query :limit` is kept as an editable string so the user can clear and
  retype the field without the controlled input snapping to a default. The
  domain layer parses it into a bounded integer only when shaping the closed
  `/api/query` request.

## Event Boundary

All user and server stimuli use one public event:

```clojure
[:machine/dispatch {:type :stats/load :kind :status}]
```

`pgmcp.webui.events` performs:

1. Read current `:machine` from `app-db`.
2. Call `pgmcp.webui.machine/run`.
3. Store the returned machine at `[:machine]`.
4. Translate returned effect data into re-frame effect maps.

Examples:

| Stimulus | Re-frame event | Machine event | Machine effects |
|---|---|---|---|
| Operator clicks Status | `[:machine/dispatch {:type :stats/load :kind :status}]` | `:stats/load` | `{:type :fetch-stats, :kind :status}` |
| Query form submit | `[:machine/dispatch {:type :query/run}]` | `:query/run` | `{:type :fetch-query, :request ...}` |
| Work view load | `[:machine/dispatch {:type :work/load}]` | `:work/load` | `{:type :fetch-work, :request ...}` |
| Websocket opens | `[:machine/dispatch {:type :ws/open}]` | `:ws/open` | maybe `{:type :ws-send-hello}` |
| Websocket frame arrives | `[:machine/dispatch {:type :ws/frame, :frame frame}]` | `:ws/frame` | none unless resync needed |
| Topic toggled | `[:machine/dispatch {:type :events/topic, :topic :cron, :checked? false}]` | `:events/topic` | `{:type :ws-sync-subscription}` |
| Pause clicked | `[:machine/dispatch {:type :events/pause}]` | `:events/pause` | none |

Direct `reg-event-db` handlers are allowed only for bootstrapping edge state
that has no semantic meaning, such as loading the remembered token into
`:runtime`.

## Effect Boundary

Effects returned by the machine are interpreted by `pgmcp.webui.fx`.

| Effect data | re-frame fx | Responsibility |
|---|---|---|
| `{:type :fetch-stats, :kind k}` | `:pgmcp/fetch-stats` | GET `/api/stats?kind=k`; dispatch loaded/error machine events. |
| `{:type :fetch-query, :request r}` | `:pgmcp/fetch-query` | POST `/api/query`; dispatch loaded/error machine events. |
| `{:type :fetch-mandates, :request r}` | `:pgmcp/fetch-mandates` | GET `/api/mandates?...`; dispatch loaded/error machine events. |
| `{:type :fetch-work, :request r}` | `:pgmcp/fetch-work` | GET `/api/work_items?...`; dispatch loaded/error machine events. |
| `{:type :ws-connect}` | `:pgmcp/ws-connect` | Open `/webui/ws` with token, since cursor, and selected topics. |
| `{:type :ws-disconnect}` | `:pgmcp/ws-disconnect` | Close current socket and dispatch close event if needed. |
| `{:type :ws-sync-subscription}` | `:pgmcp/ws-sync-subscription` | Send `hello` with selected topics and subscription cursor. |
| `{:type :remember-token, :token t}` | `:pgmcp/remember-token` | Persist token in localStorage. |

The websocket object is held in `pgmcp.webui.fx`, not in `app-db`, to preserve
serializability of the machine and store.

## Subscriptions

Subscriptions derive view data and must not perform effects.

| Subscription | Meaning |
|---|---|
| `[:control/view]` | Active view keyword. |
| `[:control/connection]` | Connection state keyword. |
| `[:control/activity]` | Activity state keyword. |
| `[:stats/current-kind]` | Current stats panel kind. |
| `[:stats/current-payload]` | Payload for selected stats kind, with pending placeholder. |
| `[:query/form]` | Controlled query form values. |
| `[:query/results]` | Normalized result rows. |
| `[:events/topics]` | Topic checkbox states plus disabled status for last selected topic. |
| `[:events/visible]` | Events visible under current topic filters. |
| `[:events/summary]` | `applied-seq`, `server-seq`, visible count, queued count, topic counts. |
| `[:events/paused?]` | Pause state. |
| `[:mandates/form]` | Mandate filter values. |
| `[:mandates/sources]` | Shaped mandate source rows. |
| `[:work/form]` | Work smart-view filter values. |
| `[:work/items]` | Normalized read-only work item rows. |
| `[:work/pending?]` | Work smart-view request status. |
| `[:machine/rejects]` | Bounded machine rejects for debugging. |

## Component Hierarchy

```text
app-root
  shell
    topbar
      brand
      navigation-tabs
      connection-controls
    active-view
      overview-page
        stats-toolbar
        stats-panels
        reject-panel
      query-page
        query-form
        query-results
      events-page
        event-toolbar
          topic-filter-group
          event-actions
        event-summary
        event-log
      mandates-page
        mandates-form
        mandate-source-list
      work-page
        work-form
        work-item-list
```

Use re-com controls where they fit naturally:

- `h-box`, `v-box`, `box`, `gap`, and `scroller` for layout.
- `button`, `single-dropdown`, `input-text`, `input-password`,
  `input-textarea`, `checkbox`, and `label` for controls.
- Custom Reagent markup is acceptable for dense data rows when re-com would add
  unnecessary wrapper weight, but dispatch and subscription rules still apply.

## Page Design

### Overview

Purpose: daemon operator dashboard.

Controls:

- Stats kind segmented controls: `status`, `index`, `cron`, `clients`,
  `telemetry`, `counters`.
- Connection state pill.
- Optional reject ring panel when non-empty.

Data layout:

- Dense panels using monospace JSON blocks where the backend shape is broad.
- Stable panel dimensions with scroll overflow, not layout shifts.

### Query

Purpose: closed search surface over existing daemon APIs.

Controls:

- Mode dropdown: `semantic`, `text`, `grep`.
- Query input.
- Project input.
- Limit input with bounded numeric constraints.
- Run button.

Results:

- Normalize semantic, text, and grep rows into `{path, lines, language, project,
  score, snippet}`.
- Show mode, count, and truncation marker.
- Keep snippets wrapped and bounded.

### Events

Purpose: resumable realtime operator log.

Controls:

- Topic checkboxes for the closed topic set.
- Pause/resume button.
- Clear button.
- Connect/disconnect controls in the shell.

Semantics:

- At least one known topic remains selected.
- Disabling a topic records its current sequence watermark.
- Re-enabling a topic computes a subscription cursor from selected topic
  watermarks, so replay can fetch relevant unseen rows.
- Paused events are queued in a bounded ring and drained on resume.
- Duplicate replay is ignored by per-topic sequence watermarks, not only by a
  single global cursor.

### Mandates

Purpose: inspect mandate sources without broad write power.

Controls:

- Scope dropdown: `all`, `global`, `workspace`, `project`.
- Project input.
- Load button.

Results:

- Source cards or rows showing scope, kind, path, and text.
- Long text scrolls inside the row; it must not overlap adjacent content.

### Work

Purpose: inspect the tracker backlog from the same closed smart views exposed to
agents, without granting browser-side mutation authority.

Controls:

- View dropdown: `next-actionable`, `needs-triage`, `blocked`, `overdue`,
  `my-work`.
- Assignee input for `my-work` and optional next-actionable ownership filters.
- Plan id input for next-actionable subtree filtering.
- Limit input with bounded numeric constraints.
- Load button.

Results:

- Rows show public id, kind, status, priority, progress, assignee/claim data,
  title, and bounded body preview.
- The browser calls only `GET /api/work_items`; it does not issue tracker
  mutation calls, generic MCP calls, or SQL.

## Statechart Plan

Implemented orthogonal regions:

| Region | States | Notes |
|---|---|---|
| `view` | `overview`, `query`, `events`, `mandates`, `work` | `ui/view` pushes `K`; `nav/back` pops `K`. |
| `connection` | `idle`, `connecting`, `live`, `closed`, `error` | Websocket events control status; reconnect is explicit. |
| `activity` | `ready`, `loading` | Derived aggregate over pending REST reads. It returns to `ready` only when the bounded request ledger is empty. |
| `query` | `editing`, `submitted`, `loaded`, `failed` | Query-specific lifecycle. Stale completions settle the request ledger but cannot overwrite newer query data or move the region. |
| `mandates` | `idle`, `loading`, `loaded`, `failed` | Mandate-specific lifecycle with the same current-request guard as query. |
| `work` | `idle`, `loading`, `loaded`, `failed` | Work smart-view lifecycle with the same current-request guard as query and mandates. |
| `events` | `streaming`, `paused` | Pause/resume is formally visible in the statechart. Paused frames queue in a bounded ring and drain when the region returns to `streaming`. |

Dynamic transition targets such as `:$query/loaded-target` are resolved from the
pre-event machine snapshot. This keeps orthogonal regions independent of map
iteration order: one region clearing a request cannot prevent another region
from making the transition that was valid at event entry.

## Implementation Sequence

1. Amend ADR-034 to explicitly name Reagent, re-frame, and re-com and to forbid
   direct DOM rendering.
2. Update `webui/shadow-cljs.edn` and `webui/package.json` only as needed for
   the intended frontend stack.
3. Replace `webui/resources/index.html` with a single mount root plus static
   metadata. It should not contain application sections or controls.
4. Create `schema`, `model`, `domain`, and `machine` namespaces with pure
   machine logic and no Reagent/re-frame dependency.
5. Create `events`, `fx`, and `subs` namespaces. Register the single semantic
   event path and edge effects.
6. Create Reagent/re-com view namespaces for shell, overview, query, events,
   mandates, and work.
7. Keep existing backend routes and websocket protocol unless focused frontend
   integration proves a backend issue.
8. Build the Shadow CLJS release and copy the compiled artifact through the
   existing script.
9. Run static anti-regression checks for direct DOM rendering and forbidden
   mutation patterns.

## Validation Plan

Focused checks:

```bash
cd webui
npm run build
cd ..
python3 scripts/smoke-webui-render.py
```

The smoke script serves the compiled assets from `webui/resources`, mocks the
closed REST surfaces, opens headless Chromium at mobile and desktop viewports,
exercises Overview, Query, Events, Mandates, and Work, and fails on
non-websocket console warnings/errors or horizontal body overflow.

Static checks:

```bash
./scripts/check-webui-architecture.sh
```

Interpretation:

- The script fails on raw DOM/string rendering, raw `[:input ...]` controls,
  raw form controls, browser edge APIs outside `fx.cljs`, ad hoc mutable cells
  outside `core.cljs`/`fx.cljs`, browser endpoint references outside the closed
  web UI surface, DOM hooks outside `core.cljs`/`fx.cljs`, missing Reagent
  mounting, missing re-frame events/effects/subscriptions, or a missing machine
  dispatch boundary.
- A reviewed exception is allowed only by narrowing the script after documenting
  why the exception is edge-only.

Optional focused Rust checks:

```bash
cargo test -p pgmcp-webui
```

Do not run `./scripts/verify.sh` during frontend iteration. Consider it only at
the final project gate if backend pgmcp code changed in a way that warrants the
full backend verification contract.

## Acceptance Criteria

The correction is acceptable only when all of these are true:

1. `webui/src-cljs` contains the planned namespace split.
2. `core.cljs` only initializes re-frame, installs allowed edge hooks, and
   mounts the Reagent root.
3. Application controls are Reagent/re-com components.
4. User actions dispatch re-frame vectors.
5. Server responses and websocket frames dispatch back into re-frame.
6. Re-frame handlers call the pure CESK/statechart machine for semantic events.
7. The pure machine returns updated machine state and effect data.
8. Effects are interpreted only by `reg-fx` handlers.
9. `app-db` stores a serializable CESK machine under `:machine`.
10. Websocket handles and other non-serializable runtime objects stay outside
    the machine store.
11. No application rendering uses `innerHTML` or string-built HTML.
12. The event log is bounded.
13. The reject log is bounded.
14. Query, mandates, work, and events are explicit statechart regions, not inferred
    only from ad hoc UI booleans or payload presence.
15. Topic filter replay uses per-topic watermarks or an equivalently precise
    mechanism.
16. Pause/resume queues and drains bounded events without losing expected
    visible events.
17. Query, stats, mandates, and work use only closed REST endpoints.
18. The frontend release build succeeds.
19. Static anti-regression checks pass or every exception is documented and
    edge-only.
20. No external dependency worktree is modified.
21. No backend changes are introduced merely to compensate for frontend design
    shortcuts.

## Admin-console overhaul (2026-07 amendment)

The original 21 criteria above are 100% architectural and — by their own
completeness — sanctioned a presentation layer that was never built (every pane a
`JSON.stringify` `<pre>`). This amendment adds the missing presentation-quality
requirement and expands the console from 5 read-only panes into a full workspace +
database management console. Full rationale, the expanded (still-closed, curated)
REST surface, the realtime producer seam, auth + audit, and the no-`innerHTML`
decision live in ADR-034 (§ "Admin-console expansion"). Deliberately, this widens
scope beyond criterion 21: the new capabilities require new *curated* backend
surface, entered as an explicit ADR amendment, not smuggled in to paper over a
frontend shortcut.

**Presentation-quality criteria (new):**

- P1. No pane's default view is a raw `<pre>`-JSON dump; a raw-JSON toggle is
  allowed only as a non-default escape hatch (`smoke-webui-render.py` enforces).
- P2. Every rich renderer emits hiccup — never `innerHTML`/string HTML — so the
  no-raw-HTML gate stays intact (Markdown via `hast->hiccup`, code via
  `spans->hiccup`, charts as hand-authored SVG hiccup).
- P3. Structural CSS references only the `--vv-*` theme tokens (Spacemacs
  dark + light).

**Scope expansion:** 6 new panes (Resources / Metrics / Clients / Database / Logs /
Experiments) on curated, server-defined endpoints behind the token+origin auth
middleware; realtime producers for all 10 topics; token-gated + audited operator
editing (mandate CRUD/promote + human-authority work-item transitions via
lightning-bug). Delivered in six phases A–F: Foundation (theme / auth / session /
`:simple` / embedded WASM grammars) → Overview+Query de-JSON → read data surface +
rendering toolkit → realtime producers → human-authority editing → polish +
verification. The browser statechart grew from 7 to ten orthogonal regions (added
`resources`, the shared `panel`, and `session`); the endpoint allow-list widened
but stays an explicit enumeration.

## Risks

| Risk | Mitigation |
|---|---|
| Re-frame duplicates the CESK state model. | Use re-frame as transport only: `app-db` contains the machine, and semantic events route through `machine/run`. |
| Effects creep into the pure machine. | Keep fetch, websocket, localStorage, clock, and DOM code out of `model`, `domain`, and `machine`. |
| Re-com wrappers make dense operator screens too spacious. | Use re-com for controls and layout primitives, and custom Reagent rows for dense logs/results. |
| Topic replay skips events after filter changes. | Track per-topic sequence watermarks and compute subscription cursor from selected topics. |
| Dependency versions mismatch. | Use current Clojars/NPM candidates, then verify with `npm run build` before claiming completion. |
| Backend verification dominates frontend work. | Use focused frontend checks during iteration; reserve `verify.sh` for the final backend-relevant gate only. |

## References

- David Harel, "Statecharts: A Visual Formalism for Complex Systems," Science
  of Computer Programming, 8(3), 1987. DOI:
  <https://doi.org/10.1016/0167-6423(87)90035-9>.
- David Van Horn and Matthew Might, "Abstracting Abstract Machines," 2010,
  <https://arxiv.org/abs/1007.4446>.
- re-frame Clojars package, version `1.4.7` observed on 2026-07-03:
  <https://clojars.org/re-frame>.
- Reagent Clojars package, version `2.0.1` observed on 2026-07-03:
  <https://clojars.org/reagent>.
- re-com Clojars package, version `2.29.4` observed on 2026-07-03:
  <https://clojars.org/re-com>.
