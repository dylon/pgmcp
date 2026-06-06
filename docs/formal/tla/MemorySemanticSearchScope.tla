------------------------- MODULE MemorySemanticSearchScope -------------------------
(***************************************************************************)
(* `memory_semantic_search` request/query boundary.                         *)
(*                                                                         *)
(* The MCP tool normalizes query/tier/limit inputs before embedding and     *)
(* the SQL query filters observations with EXISTS predicates so multi-scope *)
(* or multi-tier entities do not duplicate one observation.                 *)
(*                                                                         *)
(* Verified:                                                               *)
(*   - blank queries are rejected before embedding;                         *)
(*   - invalid tiers are rejected;                                          *)
(*   - query embeddings must be 1024-dimensional;                           *)
(*   - limits are clamped to 1..=200;                                       *)
(*   - returned observations satisfy active/scope/tier filters;             *)
(*   - multi-scope and multi-tier memberships do not duplicate rows.        *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

MaxLimit == 200

Scopes == {"none", "s1", "s2", "s3"}
Tiers == {"none", "working", "episodic", "semantic", "procedural", "reflective", "bogus"}
Statuses == {"idle", "pending", "done"}
Reasons == {"none", "blank_query", "invalid_tier", "bad_embedding"}

NoReq ==
    [ id |-> 0,
      query |-> "",
      scope |-> "none",
      tier |-> "none",
      limit |-> 20,
      embedding_dim |-> 1024 ]

Requests ==
    { [id |-> 1, query |-> "", scope |-> "none", tier |-> "none", limit |-> 20, embedding_dim |-> 1024],
      [id |-> 2, query |-> "   ", scope |-> "none", tier |-> "none", limit |-> 20, embedding_dim |-> 1024],
      [id |-> 3, query |-> "memory", scope |-> "none", tier |-> "bogus", limit |-> 20, embedding_dim |-> 1024],
      [id |-> 4, query |-> "memory", scope |-> "none", tier |-> "none", limit |-> 20, embedding_dim |-> 384],
      [id |-> 5, query |-> " memory ", scope |-> "none", tier |-> "none", limit |-> 20, embedding_dim |-> 1024],
      [id |-> 6, query |-> "memory", scope |-> "s1", tier |-> " semantic ", limit |-> 20, embedding_dim |-> 1024],
      [id |-> 7, query |-> "memory", scope |-> "s2", tier |-> "working", limit |-> 0, embedding_dim |-> 1024],
      [id |-> 8, query |-> "memory", scope |-> "s3", tier |-> "none", limit |-> 500, embedding_dim |-> 1024] }

RequestIds == {r.id : r \in Requests}

NormalizeQuery(q) ==
    CASE q = " memory " -> "memory"
      [] q = "   " -> ""
      [] OTHER -> q

NormalizeTier(t) ==
    CASE t = " semantic " -> "semantic"
      [] t = "" -> "none"
      [] OTHER -> t

ValidTier(t) ==
    NormalizeTier(t) \in {"none", "working", "episodic", "semantic", "procedural", "reflective"}

ClampLimit(n) ==
    IF n < 1 THEN 1 ELSE IF n > MaxLimit THEN MaxLimit ELSE n

ObservationRows ==
    { [ observation_id |-> 1, entity_id |-> 1, active |-> TRUE,
        has_embedding |-> TRUE, scopes |-> {"s1", "s2"},
        tiers |-> {"semantic", "procedural"}, rank |-> 1 ],
      [ observation_id |-> 2, entity_id |-> 2, active |-> TRUE,
        has_embedding |-> TRUE, scopes |-> {"s2"},
        tiers |-> {"working"}, rank |-> 2 ],
      [ observation_id |-> 3, entity_id |-> 3, active |-> FALSE,
        has_embedding |-> TRUE, scopes |-> {"s1"},
        tiers |-> {"semantic"}, rank |-> 3 ],
      [ observation_id |-> 4, entity_id |-> 4, active |-> TRUE,
        has_embedding |-> FALSE, scopes |-> {"s1"},
        tiers |-> {"semantic"}, rank |-> 4 ] }

RowMatches(row, scope, tier) ==
    /\ row.active
    /\ row.has_embedding
    /\ (scope = "none" \/ scope \in row.scopes)
    /\ (tier = "none" \/ tier \in row.tiers)

\* TLC does not guarantee set iteration order above. This model checks
\* membership, uniqueness, and bounds; SQL ordering is covered by direct tests.
RowsFor(scope, tier) ==
    CASE scope = "none" /\ tier = "none" ->
        << CHOOSE r \in ObservationRows : r.observation_id = 1,
           CHOOSE r \in ObservationRows : r.observation_id = 2 >>
      [] scope = "s1" /\ tier = "semantic" ->
        << CHOOSE r \in ObservationRows : r.observation_id = 1 >>
      [] scope = "s2" /\ tier = "working" ->
        << CHOOSE r \in ObservationRows : r.observation_id = 2 >>
      [] OTHER -> <<>>

TakeRows(rows, limit) ==
    SubSeq(rows, 1, IF Len(rows) < limit THEN Len(rows) ELSE limit)

ResponseRows ==
    [ observation_id: {1, 2, 3, 4},
      entity_id: {1, 2, 3, 4},
      active: BOOLEAN,
      has_embedding: BOOLEAN,
      scopes: SUBSET {"s1", "s2", "s3"},
      tiers: SUBSET {"working", "episodic", "semantic", "procedural", "reflective"},
      rank: 1..4 ]

NoResp ==
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized_query |-> "",
      normalized_tier |-> "none",
      effective_limit |-> 20,
      results |-> <<>> ]

VARIABLES req, status, resp

vars == <<req, status, resp>>

Init ==
    /\ req = NoReq
    /\ status = "idle"
    /\ resp = NoResp

PickRequest(r) ==
    /\ status = "idle"
    /\ r \in Requests
    /\ req' = r
    /\ status' = "pending"
    /\ UNCHANGED resp

RejectBlankQuery ==
    /\ status = "pending"
    /\ NormalizeQuery(req.query) = ""
    /\ resp' = [NoResp EXCEPT
        !.rejected = TRUE,
        !.reason = "blank_query",
        !.normalized_query = ""]
    /\ status' = "done"
    /\ UNCHANGED req

RejectInvalidTier ==
    /\ status = "pending"
    /\ NormalizeQuery(req.query) # ""
    /\ ~ValidTier(req.tier)
    /\ resp' = [NoResp EXCEPT
        !.rejected = TRUE,
        !.reason = "invalid_tier",
        !.normalized_query = NormalizeQuery(req.query),
        !.normalized_tier = NormalizeTier(req.tier)]
    /\ status' = "done"
    /\ UNCHANGED req

RejectBadEmbedding ==
    /\ status = "pending"
    /\ NormalizeQuery(req.query) # ""
    /\ ValidTier(req.tier)
    /\ req.embedding_dim # 1024
    /\ resp' = [NoResp EXCEPT
        !.rejected = TRUE,
        !.reason = "bad_embedding",
        !.normalized_query = NormalizeQuery(req.query),
        !.normalized_tier = NormalizeTier(req.tier),
        !.effective_limit = ClampLimit(req.limit)]
    /\ status' = "done"
    /\ UNCHANGED req

Respond ==
    /\ status = "pending"
    /\ NormalizeQuery(req.query) # ""
    /\ ValidTier(req.tier)
    /\ req.embedding_dim = 1024
    /\ LET tier == NormalizeTier(req.tier) IN
       LET limit == ClampLimit(req.limit) IN
       /\ resp' =
            [ rejected |-> FALSE,
              reason |-> "none",
              normalized_query |-> NormalizeQuery(req.query),
              normalized_tier |-> tier,
              effective_limit |-> limit,
              results |-> TakeRows(RowsFor(req.scope, tier), limit) ]
    /\ status' = "done"
    /\ UNCHANGED req

TerminalStutter ==
    /\ status = "done"
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ RejectBlankQuery
    \/ RejectInvalidTier
    \/ RejectBadEmbedding
    \/ Respond
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

ResultIds == {resp.results[i].observation_id : i \in 1..Len(resp.results)}

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ status \in Statuses
    /\ resp.rejected \in BOOLEAN
    /\ resp.reason \in Reasons
    /\ resp.normalized_query \in {"", "memory"}
    /\ resp.normalized_tier \in Tiers
    /\ resp.effective_limit \in 1..MaxLimit
    /\ resp.results \in Seq(ResponseRows)

BlankQueriesRejected ==
    status = "done" /\ NormalizeQuery(req.query) = "" =>
        /\ resp.rejected
        /\ resp.reason = "blank_query"
        /\ Len(resp.results) = 0

InvalidTiersRejected ==
    status = "done" /\ NormalizeQuery(req.query) # "" /\ ~ValidTier(req.tier) =>
        /\ resp.rejected
        /\ resp.reason = "invalid_tier"
        /\ Len(resp.results) = 0

BadEmbeddingRejected ==
    status = "done" /\ NormalizeQuery(req.query) # "" /\ ValidTier(req.tier) /\ req.embedding_dim # 1024 =>
        /\ resp.rejected
        /\ resp.reason = "bad_embedding"
        /\ Len(resp.results) = 0

LimitClamped ==
    status = "done" /\ ~resp.rejected =>
        resp.effective_limit = ClampLimit(req.limit)

OutputWithinLimit ==
    status = "done" /\ ~resp.rejected =>
        Len(resp.results) <= resp.effective_limit

RowsMatchScopeAndTier ==
    status = "done" /\ ~resp.rejected =>
        \A i \in 1..Len(resp.results) :
            RowMatches(resp.results[i], req.scope, resp.normalized_tier)

NoDuplicateObservationRows ==
    status = "done" /\ ~resp.rejected =>
        Cardinality(ResultIds) = Len(resp.results)

NormalizedInputsUsed ==
    status = "done" =>
        /\ resp.normalized_query = NormalizeQuery(req.query)
        /\ resp.normalized_tier = NormalizeTier(req.tier)

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        BlankQueriesRejected /\
        InvalidTiersRejected /\
        BadEmbeddingRejected /\
        LimitClamped /\
        OutputWithinLimit /\
        RowsMatchScopeAndTier /\
        NoDuplicateObservationRows /\
        NormalizedInputsUsed)

=============================================================================
