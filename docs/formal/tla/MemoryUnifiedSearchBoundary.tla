----------------------- MODULE MemoryUnifiedSearchBoundary -----------------------
(***************************************************************************)
(* `memory_unified_search` request boundary model.                          *)
(*                                                                         *)
(* The MCP wrapper rejects malformed query/filter input before embedding,    *)
(* checks the query embedding dimension before SQL, clamps `k` and           *)
(* `hnsw.ef_search`, and delegates to a read-only vector query over the      *)
(* unified-node matview.                                                    *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

ExpectedDim == 1024
MaxK == 200
MaxEf == 10000
MaxNodeTypes == 32

NodeTypes == {"memory_entity", "observation", "chunk", "project"}
EmbeddingNodeTypes == {"observation", "chunk"}

QueryModes == {"valid", "blank", "oversized"}
NodeModes == {"none", "dedupe", "empty", "blank_entry", "unknown", "too_many", "non_embedding"}
Outcomes == {"ok", "rejected"}
Reasons ==
    {"none", "blank_query", "query_too_large", "empty_node_types",
     "blank_node_type", "unknown_node_type", "too_many_node_types",
     "bad_embedding_dim"}

Requests ==
    { [ id |-> 1, query_mode |-> "valid", node_mode |-> "dedupe",
        embed_dim |-> ExpectedDim, k |-> 999, ef |-> -5 ],
      [ id |-> 2, query_mode |-> "blank", node_mode |-> "none",
        embed_dim |-> ExpectedDim, k |-> 20, ef |-> 64 ],
      [ id |-> 3, query_mode |-> "oversized", node_mode |-> "none",
        embed_dim |-> ExpectedDim, k |-> 20, ef |-> 64 ],
      [ id |-> 4, query_mode |-> "valid", node_mode |-> "empty",
        embed_dim |-> ExpectedDim, k |-> 20, ef |-> 64 ],
      [ id |-> 5, query_mode |-> "valid", node_mode |-> "blank_entry",
        embed_dim |-> ExpectedDim, k |-> 20, ef |-> 64 ],
      [ id |-> 6, query_mode |-> "valid", node_mode |-> "unknown",
        embed_dim |-> ExpectedDim, k |-> 20, ef |-> 64 ],
      [ id |-> 7, query_mode |-> "valid", node_mode |-> "too_many",
        embed_dim |-> ExpectedDim, k |-> 20, ef |-> 64 ],
      [ id |-> 8, query_mode |-> "valid", node_mode |-> "none",
        embed_dim |-> 384, k |-> 20, ef |-> 64 ],
      [ id |-> 9, query_mode |-> "valid", node_mode |-> "non_embedding",
        embed_dim |-> ExpectedDim, k |-> 0, ef |-> 50000 ] }

RequestIds == {r.id : r \in Requests}

RawNodeCount(mode) ==
    CASE mode = "none" -> 0
      [] mode = "dedupe" -> 2
      [] mode = "empty" -> 0
      [] mode = "too_many" -> MaxNodeTypes + 1
      [] OTHER -> 1

FilterSupplied(mode) == mode # "none"

NormalizedNodeTypes(mode) ==
    CASE mode = "none" -> {}
      [] mode = "dedupe" -> {"observation"}
      [] mode = "non_embedding" -> {"memory_entity"}
      [] OTHER -> {}

KFor(r) ==
    IF r.k < 1 THEN 1
    ELSE IF r.k > MaxK THEN MaxK
    ELSE r.k

EfFor(r) ==
    IF r.ef < 1 THEN 1
    ELSE IF r.ef > MaxEf THEN MaxEf
    ELSE r.ef

PreEmbedReason(r) ==
    CASE r.query_mode = "blank" -> "blank_query"
      [] r.query_mode = "oversized" -> "query_too_large"
      [] r.node_mode = "empty" -> "empty_node_types"
      [] RawNodeCount(r.node_mode) > MaxNodeTypes -> "too_many_node_types"
      [] r.node_mode = "blank_entry" -> "blank_node_type"
      [] r.node_mode = "unknown" -> "unknown_node_type"
      [] OTHER -> "none"

ReasonFor(r) ==
    IF PreEmbedReason(r) # "none" THEN PreEmbedReason(r)
    ELSE IF r.embed_dim # ExpectedDim THEN "bad_embedding_dim"
    ELSE "none"

Available(node_type) ==
    CASE node_type = "observation" -> 2
      [] node_type = "chunk" -> 1
      [] OTHER -> 0

CandidateTypes(r) ==
    IF FilterSupplied(r.node_mode)
    THEN NormalizedNodeTypes(r.node_mode)
    ELSE EmbeddingNodeTypes

ResultNodeTypesFor(r) ==
    IF ReasonFor(r) = "none"
    THEN {t \in CandidateTypes(r) : t \in EmbeddingNodeTypes /\ Available(t) > 0}
    ELSE {}

ResponseFor(r) ==
    LET pre == PreEmbedReason(r) IN
    LET reason == ReasonFor(r) IN
        [ request_id |-> r.id,
          outcome |-> IF reason = "none" THEN "ok" ELSE "rejected",
          reason |-> reason,
          embedded |-> pre = "none",
          queried |-> reason = "none",
          k |-> IF reason = "none" THEN KFor(r) ELSE 0,
          ef |-> IF reason = "none" THEN EfFor(r) ELSE 0,
          filter_supplied |-> FilterSupplied(r.node_mode),
          node_types |-> IF reason = "none" THEN NormalizedNodeTypes(r.node_mode) ELSE {},
          result_node_types |-> ResultNodeTypesFor(r),
          writes |-> 0,
          lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      embedded: BOOLEAN,
      queried: BOOLEAN,
      k: 0..MaxK,
      ef: 0..MaxEf,
      filter_supplied: BOOLEAN,
      node_types: SUBSET NodeTypes,
      result_node_types: SUBSET NodeTypes,
      writes: 0..0,
      lock_held: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK == response \in ResponseRecord

InvalidRequestsReject ==
    ReasonFor(req) # "none" => response.outcome = "rejected"

PreEmbedValidationPrecedesEmbedding ==
    PreEmbedReason(req) # "none" =>
        /\ response.embedded = FALSE
        /\ response.queried = FALSE

BadEmbeddingDoesNotQuery ==
    ReasonFor(req) = "bad_embedding_dim" =>
        /\ response.embedded = TRUE
        /\ response.queried = FALSE

SuccessfulRequestBounds ==
    response.outcome = "ok" =>
        /\ req.query_mode = "valid"
        /\ req.embed_dim = ExpectedDim
        /\ response.k \in 1..MaxK
        /\ response.ef \in 1..MaxEf

NodeTypeFiltersNormalized ==
    response.outcome = "ok" /\ response.filter_supplied =>
        /\ response.node_types = NormalizedNodeTypes(req.node_mode)
        /\ response.node_types # {}
        /\ Cardinality(response.node_types) <= MaxNodeTypes
        /\ response.node_types \subseteq NodeTypes

ResultsRespectFilter ==
    response.outcome = "ok" /\ response.filter_supplied =>
        response.result_node_types \subseteq response.node_types

OnlyEmbeddingRowsReturned ==
    response.outcome = "ok" =>
        response.result_node_types \subseteq EmbeddingNodeTypes

ReadOnlyNoHeldLock ==
    /\ response.writes = 0
    /\ response.lock_held = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsReject /\
        PreEmbedValidationPrecedesEmbedding /\
        BadEmbeddingDoesNotQuery /\
        SuccessfulRequestBounds /\
        NodeTypeFiltersNormalized /\
        ResultsRespectFilter /\
        OnlyEmbeddingRowsReturned /\
        ReadOnlyNoHeldLock)

================================================================================
