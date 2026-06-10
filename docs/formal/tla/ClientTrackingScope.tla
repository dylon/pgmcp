----------------------------- MODULE ClientTrackingScope -----------------------------
(***************************************************************************)
(* Request and row-scope boundary for the client-tracking read tools:      *)
(*   active_clients and client_project_matrix.                             *)
(*                                                                         *)
(* Unlike the fail-closed coordination/deps tools, these two take an       *)
(* OPTIONAL project name filter that is trimmed and where a blank string   *)
(* means "no filter" rather than an error (the filter only ever REMOVES    *)
(* rows; it never widens or errors). client_project_matrix additionally    *)
(* clamps two windows:                                                     *)
(*   - since_minutes -> clamp(1, 44_640)   (1 minute .. 31 days)           *)
(*   - top_files_per_project -> clamp(0, 50)                               *)
(* Returned rows are grouped by project; both tools are read-only.         *)
(*                                                                         *)
(* One request is processed per behavior (CircularDependenciesScope shape) *)
(* so the state space stays small and finite.                             *)
(***************************************************************************)

EXTENDS Naturals, Integers, Sequences, FiniteSets

SinceMin == 1
SinceMax == 44640
TopMin == 0
TopMax == 50

Outcomes == {"ok"}   \* neither tool fails on the optional filter

\* Projects clients may be attributed to. There is a duplicate display-name pair
\* ("dup"); the optional name filter matches by NAME (so a duplicate name matches
\* BOTH rows) — these tools do not resolve to a single id, they filter rows, so a
\* duplicate name is not an error here, just a wider row match.
Projects ==
    { [id |-> 1, name |-> "a"],
      [id |-> 2, name |-> "b"],
      [id |-> 3, name |-> "dup"],
      [id |-> 4, name |-> "dup"] }

ProjectIds == {p.id : p \in Projects}

\* Client rows, each attributed to a project id (or none — modeled as 0).
Clients ==
    { [cid |-> 10, project_id |-> 1],
      [cid |-> 20, project_id |-> 2],
      [cid |-> 30, project_id |-> 3],
      [cid |-> 40, project_id |-> 4],
      [cid |-> 50, project_id |-> 0] }   \* an unattributed client

ClientIds == {c.cid : c \in Clients}

Tools == {"active", "matrix"}

\* The optional project filter `proj` (a name; "" = absent). `since`/`top` are the
\* matrix windows, exercising negative, zero, in-range, and overflow values.
Requests ==
    { [id |-> 1, tool |-> "active", proj |-> "",     since |-> 0,     top |-> 0],
      [id |-> 2, tool |-> "active", proj |-> "  ",   since |-> 0,     top |-> 0],
      [id |-> 3, tool |-> "active", proj |-> "a",    since |-> 0,     top |-> 0],
      [id |-> 4, tool |-> "active", proj |-> "dup",  since |-> 0,     top |-> 0],
      [id |-> 5, tool |-> "active", proj |-> "none", since |-> 0,     top |-> 0],
      [id |-> 6, tool |-> "matrix", proj |-> "",     since |-> -10,   top |-> -3],
      [id |-> 7, tool |-> "matrix", proj |-> "  ",   since |-> 0,     top |-> 0],
      [id |-> 8, tool |-> "matrix", proj |-> "b",    since |-> 1440,  top |-> 5],
      [id |-> 9, tool |-> "matrix", proj |-> "dup",  since |-> 999999,top |-> 999] }

RequestIds == {r.id : r \in Requests}

Trim(s) ==
    CASE s = "  " -> ""
      [] OTHER    -> s

\* Optional filter normalization: trim, blank -> "no filter" (NOT an error).
FilterActive(r) == Trim(r.proj) # ""

\* Project ids whose NAME matches the (trimmed) filter — a duplicate name yields
\* multiple ids. Only meaningful when the filter is active.
FilterIds(r) == {p.id : p \in {q \in Projects : q.name = Trim(r.proj)}}

Clamp(v, lo, hi) ==
    IF v < lo THEN lo ELSE IF v > hi THEN hi ELSE v

EffSince(r) == Clamp(r.since, SinceMin, SinceMax)
EffTop(r)   == Clamp(r.top, TopMin, TopMax)

\* Rows returned: all clients when the filter is absent, else only clients whose
\* project id is one of the filter-matched ids. (Filter only REMOVES rows.)
ScopedClients(r) ==
    IF FilterActive(r)
    THEN {c.cid : c \in {c2 \in Clients : c2.project_id \in FilterIds(r)}}
    ELSE ClientIds

ResponseFor(r) ==
    [ request_id |-> r.id,
      tool       |-> r.tool,
      outcome    |-> "ok",
      eff_since  |-> EffSince(r),
      eff_top    |-> EffTop(r),
      clients    |-> ScopedClients(r) ]

ResponseRecord ==
    [ request_id: RequestIds,
      tool: Tools,
      outcome: Outcomes,
      eff_since: SinceMin..SinceMax,
      eff_top: TopMin..TopMax,
      clients: SUBSET ClientIds ]

VARIABLES req, response

vars == <<req, response>>

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord
    /\ response.request_id = req.id

\* A blank (post-trim) project filter is NOT an error — it returns all rows.
BlankFilterIsUnfiltered ==
    (~FilterActive(req)) =>
        /\ response.outcome = "ok"
        /\ response.clients = ClientIds

\* An active filter only REMOVES rows: every returned client's project id is one
\* of the filter-matched ids (and the result is a subset of all clients).
FilterOnlyRemovesRows ==
    FilterActive(req) =>
        /\ response.clients \subseteq ClientIds
        /\ \A cid \in response.clients :
              \E c \in Clients : c.cid = cid /\ c.project_id \in FilterIds(req)

\* client_project_matrix clamps both windows into range.
SinceClamped ==
    response.eff_since = EffSince(req) /\ response.eff_since \in SinceMin..SinceMax

TopFilesClamped ==
    response.eff_top = EffTop(req) /\ response.eff_top \in TopMin..TopMax

\* The optional filter never produces an error outcome (no fail-closed path here).
NeverErrorsOnFilter ==
    response.outcome = "ok"

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        BlankFilterIsUnfiltered /\
        FilterOnlyRemovesRows /\
        SinceClamped /\
        TopFilesClamped /\
        NeverErrorsOnFilter)

================================================================================
