---------------------------- MODULE WorkSummaryBoundary ----------------------------
(***************************************************************************)
(* `work_summary` request boundary and side-effect discipline.             *)
(*                                                                         *)
(* The MCP wrapper validates + normalizes + clamps every parameter at the   *)
(* request boundary (`WorkSummaryRequest::from_params`) BEFORE any work.    *)
(* On accept it performs a bounded per-repo live-git scan (one `git log     *)
(* --numstat` pass + one reconciliation query per canonical repo), attaches *)
(* freshness-gated topic enrichment, and renders exactly one envelope whose *)
(* `normalized` block echoes the resolved parameters.                       *)
(*                                                                         *)
(* Unlike `quality_report`, there is no project-lookup phase: workspace     *)
(* enumeration is itself the first side effect, so EVERY reject is local    *)
(* and incurs zero repo scans / DB queries / renders.                      *)
(*                                                                         *)
(* Verified:                                                               *)
(*   - any invalid parameter rejects before any scan, query, or render;     *)
(*   - repo scanning is bounded by min(available, clamp(max_repos));        *)
(*   - per-repo git + DB reads are bounded (one pass each, no unbounded     *)
(*     loop);                                                              *)
(*   - topic enrichment happens only when use_graph != off AND the project  *)
(*     index is fresh (the freshness gate);                                 *)
(*   - accepted calls render exactly one canonical-format envelope.         *)
(***************************************************************************)

EXTENDS Naturals

ClampLo  == 1
ClampHi  == 1000
Available == 4          \* canonical repos discovered under the workspace

Formats == {"markdown", "org", "json"}
Groups  == {"project", "theme", "week"}
Graphs  == {"auto", "on", "off"}
Reasons ==
    {"none", "no_workspace", "bad_window", "blank_author",
     "bad_format", "bad_group", "bad_graph"}

Min(a, b) == IF a < b THEN a ELSE b

Clamp(n) == IF n < ClampLo THEN ClampLo ELSE IF n > ClampHi THEN ClampHi ELSE n

NoReq ==
    [ id |-> 0, workspace |-> "ws", window_ok |-> TRUE, author |-> "me",
      format |-> "markdown", group |-> "project", graph |-> "auto",
      max_repos |-> 100, fresh |-> TRUE ]

(* A representative request set covering each reject path and the accept    *)
(* variants (graph on+fresh -> topics, on+stale -> none, off -> none;        *)
(* max_repos clamped low/high; group/window variants).                     *)
Requests ==
    { \* --- local rejects ---
      [ id |-> 1, workspace |-> "", window_ok |-> TRUE, author |-> "me",
        format |-> "markdown", group |-> "project", graph |-> "auto",
        max_repos |-> 100, fresh |-> TRUE ],
      [ id |-> 2, workspace |-> "ws", window_ok |-> FALSE, author |-> "me",
        format |-> "markdown", group |-> "project", graph |-> "auto",
        max_repos |-> 100, fresh |-> TRUE ],
      [ id |-> 3, workspace |-> "ws", window_ok |-> TRUE, author |-> "<blank>",
        format |-> "markdown", group |-> "project", graph |-> "auto",
        max_repos |-> 100, fresh |-> TRUE ],
      [ id |-> 4, workspace |-> "ws", window_ok |-> TRUE, author |-> "me",
        format |-> "pdf", group |-> "project", graph |-> "auto",
        max_repos |-> 100, fresh |-> TRUE ],
      [ id |-> 5, workspace |-> "ws", window_ok |-> TRUE, author |-> "me",
        format |-> "markdown", group |-> "sprint", graph |-> "auto",
        max_repos |-> 100, fresh |-> TRUE ],
      [ id |-> 6, workspace |-> "ws", window_ok |-> TRUE, author |-> "me",
        format |-> "markdown", group |-> "project", graph |-> "sometimes",
        max_repos |-> 100, fresh |-> TRUE ],
      \* --- accepts ---
      [ id |-> 7, workspace |-> "ws", window_ok |-> TRUE, author |-> "all",
        format |-> "json", group |-> "week", graph |-> "on",
        max_repos |-> 0, fresh |-> TRUE ],          \* clamp low -> 1
      [ id |-> 8, workspace |-> "ws", window_ok |-> TRUE, author |-> "dylon",
        format |-> "org", group |-> "theme", graph |-> "on",
        max_repos |-> 2000, fresh |-> FALSE ],       \* clamp high; stale -> no topics
      [ id |-> 9, workspace |-> "ws", window_ok |-> TRUE, author |-> "me",
        format |-> "markdown", group |-> "project", graph |-> "off",
        max_repos |-> 3, fresh |-> TRUE ],           \* graph off -> no enrichment
      [ id |-> 10, workspace |-> "ws", window_ok |-> TRUE, author |-> "me",
        format |-> "markdown", group |-> "project", graph |-> "auto",
        max_repos |-> 100, fresh |-> TRUE ] }        \* fresh + auto -> topics

ReposScanned(r) == Min(Available, Clamp(r.max_repos))

Reject(r, reason) ==
    [ rejected |-> TRUE, reason |-> reason,
      canonical_format |-> "markdown",
      clamped_max |-> Clamp(r.max_repos),
      repos_scanned |-> 0, git_passes |-> 0, db_queries |-> 0,
      enrichment_reads |-> 0, topics_attached |-> FALSE, renders |-> 0 ]

Accept(r) ==
    [ rejected |-> FALSE, reason |-> "none",
      canonical_format |-> r.format,
      clamped_max |-> Clamp(r.max_repos),
      repos_scanned |-> ReposScanned(r),
      git_passes |-> ReposScanned(r),                       \* one numstat pass/repo
      db_queries |-> ReposScanned(r),                       \* one reconciliation/repo
      enrichment_reads |-> IF r.graph # "off" THEN ReposScanned(r) ELSE 0,
      topics_attached |-> (r.graph # "off" /\ r.fresh),
      renders |-> 1 ]

Evaluate(r) ==
    IF r.workspace = ""            THEN Reject(r, "no_workspace")
    ELSE IF ~r.window_ok           THEN Reject(r, "bad_window")
    ELSE IF r.author = "<blank>"   THEN Reject(r, "blank_author")
    ELSE IF ~(r.format \in Formats) THEN Reject(r, "bad_format")
    ELSE IF ~(r.group \in Groups)   THEN Reject(r, "bad_group")
    ELSE IF ~(r.graph \in Graphs)   THEN Reject(r, "bad_graph")
    ELSE Accept(r)

NoResp ==
    [ rejected |-> FALSE, reason |-> "none",
      canonical_format |-> "markdown", clamped_max |-> 100,
      repos_scanned |-> 0, git_passes |-> 0, db_queries |-> 0,
      enrichment_reads |-> 0, topics_attached |-> FALSE, renders |-> 0 ]

VARIABLES req, resp
vars == <<req, resp>>

Init ==
    /\ req = NoReq
    /\ resp = NoResp

Handle(r) ==
    /\ req = NoReq
    /\ r \in Requests
    /\ req' = r
    /\ resp' = Evaluate(r)

Done ==
    /\ req # NoReq
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Requests : Handle(r)
    \/ Done

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ resp.rejected \in BOOLEAN
    /\ resp.reason \in Reasons
    /\ resp.canonical_format \in Formats \cup {"pdf"}
    /\ resp.clamped_max \in ClampLo..ClampHi
    /\ resp.repos_scanned \in 0..Available
    /\ resp.git_passes \in 0..Available
    /\ resp.db_queries \in 0..Available
    /\ resp.enrichment_reads \in 0..Available
    /\ resp.topics_attached \in BOOLEAN
    /\ resp.renders \in 0..1

\* Any invalid parameter rejects before ANY scan / query / render.
LocalRejectsHaveNoSideEffects ==
    req # NoReq /\ resp.rejected =>
        /\ resp.repos_scanned = 0
        /\ resp.git_passes = 0
        /\ resp.db_queries = 0
        /\ resp.enrichment_reads = 0
        /\ resp.renders = 0

\* Work only ever happens on an accepted request.
ResolveBeforeWork ==
    req # NoReq /\ resp.repos_scanned > 0 => ~resp.rejected

\* The repo scan is bounded by the clamped max and the available repo count.
BoundedRepoScan ==
    req # NoReq =>
        /\ resp.clamped_max <= ClampHi
        /\ resp.clamped_max >= ClampLo
        /\ resp.repos_scanned <= Available
        /\ resp.repos_scanned <= resp.clamped_max
        /\ (resp.rejected => resp.repos_scanned = 0)

\* Per-repo reads are bounded: one git pass + one DB query per scanned repo
\* (no unbounded loop over a repo).
ChurnReadsBounded ==
    req # NoReq =>
        /\ resp.git_passes <= resp.repos_scanned
        /\ resp.db_queries <= resp.repos_scanned

\* Topic enrichment is consulted only when use_graph != off, and topics are
\* attached only when the request is accepted, graph is enabled, AND fresh.
EnrichmentGatedByFreshness ==
    req # NoReq =>
        /\ (resp.enrichment_reads > 0 => (~resp.rejected /\ req.graph # "off"))
        /\ (resp.topics_attached =>
                (~resp.rejected /\ req.graph # "off" /\ req.fresh))

\* Accepted calls render exactly one canonical-format envelope; rejects none.
OneRenderPerAccept ==
    req # NoReq =>
        /\ (~resp.rejected => resp.renders = 1)
        /\ (resp.rejected => resp.renders = 0)

CanonicalEnvelopeFormat ==
    req # NoReq /\ ~resp.rejected => resp.canonical_format \in Formats

=============================================================================
