----------------------------- MODULE OntologyTreeScope -----------------------------
(***************************************************************************)
(* `ontology_tree` bounded hierarchy read model.                            *)
(*                                                                         *)
(* Subtree mode resolves one active concept, clamps traversal depth, follows*)
(* active hierarchy edges only through active ontology concepts, tracks the *)
(* visited path, and returns duplicate-free named edges. Facet mode returns *)
(* hierarchy edges whose endpoints both belong to the requested facet.      *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

MaxDepth == 50
FacetLimit == 500

Facets == {"none", "component", "data_structure", "bogus"}
Statuses == {"idle", "pending", "done"}
Reasons == {"none", "blank_root", "missing_root", "invalid_facet"}

NoReq ==
    [ id |-> 0,
      root |-> "none",
      facet |-> "none",
      depth |-> 5 ]

Requests ==
    { [id |-> 1, root |-> "   ", facet |-> "none", depth |-> 5],
      [id |-> 2, root |-> "missing", facet |-> "none", depth |-> 5],
      [id |-> 3, root |-> "root", facet |-> "none", depth |-> 100],
      [id |-> 4, root |-> "root", facet |-> "none", depth |-> 1],
      [id |-> 5, root |-> "none", facet |-> "bogus", depth |-> 5],
      [id |-> 6, root |-> "none", facet |-> "data_structure", depth |-> 5],
      [id |-> 7, root |-> "none", facet |-> "none", depth |-> 5] }

NormalizeRoot(root) ==
    CASE root = "   " -> ""
      [] OTHER -> root

ValidFacet(facet) == facet \in {"none", "component", "data_structure"}

ClampDepth(depth) ==
    IF depth < 1 THEN 1 ELSE IF depth > MaxDepth THEN MaxDepth ELSE depth

ConceptIds == {1, 2, 3, 4, 5}
RootId == 1

ConceptMeta ==
    { [id |-> 1, name |-> "root", facet |-> "data_structure", active |-> TRUE, is_concept |-> TRUE],
      [id |-> 2, name |-> "a", facet |-> "data_structure", active |-> TRUE, is_concept |-> TRUE],
      [id |-> 3, name |-> "b", facet |-> "data_structure", active |-> TRUE, is_concept |-> TRUE],
      [id |-> 4, name |-> "inactive", facet |-> "data_structure", active |-> FALSE, is_concept |-> TRUE],
      [id |-> 5, name |-> "note", facet |-> "data_structure", active |-> TRUE, is_concept |-> FALSE] }

RootExists(root) == root = "root"

ConceptFor(id) == CHOOSE c \in ConceptMeta : c.id = id

ValidConcept(id) ==
    LET c == ConceptFor(id) IN c.active /\ c.is_concept

HierarchyEdges ==
    { [child |-> 2, parent |-> 1, relation |-> "is_a", depth |-> 1],
      [child |-> 3, parent |-> 2, relation |-> "is_a", depth |-> 2],
      [child |-> 1, parent |-> 3, relation |-> "is_a", depth |-> 3],
      [child |-> 4, parent |-> 1, relation |-> "is_a", depth |-> 1],
      [child |-> 5, parent |-> 1, relation |-> "is_a", depth |-> 1] }

EdgeValid(edge) ==
    /\ ValidConcept(edge.child)
    /\ ValidConcept(edge.parent)

SubtreeRows(depth) ==
    {edge \in HierarchyEdges :
        /\ edge.depth <= depth
        /\ EdgeValid(edge)
        /\ edge.child # RootId}

FacetRows(facet) ==
    {edge \in HierarchyEdges :
        /\ EdgeValid(edge)
        /\ ConceptFor(edge.child).facet = facet
        /\ ConceptFor(edge.parent).facet = facet}

NoResp ==
    [ rejected |-> FALSE,
      reason |-> "none",
      mode |-> "none",
      normalized_root |-> "none",
      facet |-> "none",
      effective_depth |-> 5,
      rows |-> {} ]

ResponseRecord ==
    [ rejected: BOOLEAN,
      reason: Reasons,
      mode: {"none", "subtree", "facet", "all"},
      normalized_root: {"", "none", "missing", "root"},
      facet: Facets,
      effective_depth: 1..MaxDepth,
      rows: SUBSET HierarchyEdges ]

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

RejectBlankRoot ==
    /\ status = "pending"
    /\ req.root # "none"
    /\ NormalizeRoot(req.root) = ""
    /\ resp' = [NoResp EXCEPT
        !.rejected = TRUE,
        !.reason = "blank_root",
        !.mode = "subtree",
        !.normalized_root = ""]
    /\ status' = "done"
    /\ UNCHANGED req

RejectMissingRoot ==
    /\ status = "pending"
    /\ req.root # "none"
    /\ NormalizeRoot(req.root) # ""
    /\ ~RootExists(NormalizeRoot(req.root))
    /\ resp' = [NoResp EXCEPT
        !.rejected = TRUE,
        !.reason = "missing_root",
        !.mode = "subtree",
        !.normalized_root = NormalizeRoot(req.root)]
    /\ status' = "done"
    /\ UNCHANGED req

RespondSubtree ==
    /\ status = "pending"
    /\ req.root # "none"
    /\ NormalizeRoot(req.root) # ""
    /\ RootExists(NormalizeRoot(req.root))
    /\ LET depth == ClampDepth(req.depth) IN
       /\ resp' =
            [ rejected |-> FALSE,
              reason |-> "none",
              mode |-> "subtree",
              normalized_root |-> NormalizeRoot(req.root),
              facet |-> "none",
              effective_depth |-> depth,
              rows |-> SubtreeRows(depth) ]
    /\ status' = "done"
    /\ UNCHANGED req

RejectInvalidFacet ==
    /\ status = "pending"
    /\ req.root = "none"
    /\ ~ValidFacet(req.facet)
    /\ resp' = [NoResp EXCEPT
        !.rejected = TRUE,
        !.reason = "invalid_facet",
        !.mode = "facet",
        !.facet = req.facet]
    /\ status' = "done"
    /\ UNCHANGED req

RespondFacet ==
    /\ status = "pending"
    /\ req.root = "none"
    /\ ValidFacet(req.facet)
    /\ req.facet # "none"
    /\ resp' =
        [ rejected |-> FALSE,
          reason |-> "none",
          mode |-> "facet",
          normalized_root |-> "none",
          facet |-> req.facet,
          effective_depth |-> 5,
          rows |-> FacetRows(req.facet) ]
    /\ status' = "done"
    /\ UNCHANGED req

RespondAllFacets ==
    /\ status = "pending"
    /\ req.root = "none"
    /\ req.facet = "none"
    /\ resp' =
        [ rejected |-> FALSE,
          reason |-> "none",
          mode |-> "all",
          normalized_root |-> "none",
          facet |-> "none",
          effective_depth |-> 5,
          rows |-> {edge \in HierarchyEdges : EdgeValid(edge)} ]
    /\ status' = "done"
    /\ UNCHANGED req

TerminalStutter ==
    /\ status = "done"
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ RejectBlankRoot
    \/ RejectMissingRoot
    \/ RespondSubtree
    \/ RejectInvalidFacet
    \/ RespondFacet
    \/ RespondAllFacets
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

EdgeKeys(rows) ==
    { [child |-> edge.child, parent |-> edge.parent, relation |-> edge.relation] : edge \in rows }

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ status \in Statuses
    /\ resp \in ResponseRecord

BlankRootsRejected ==
    status = "done" /\ req.root # "none" /\ NormalizeRoot(req.root) = "" =>
        /\ resp.rejected
        /\ resp.reason = "blank_root"
        /\ resp.rows = {}

MissingRootsRejected ==
    status = "done" /\ req.root # "none" /\ NormalizeRoot(req.root) # "" /\ ~RootExists(NormalizeRoot(req.root)) =>
        /\ resp.rejected
        /\ resp.reason = "missing_root"
        /\ resp.rows = {}

InvalidFacetsRejected ==
    status = "done" /\ req.root = "none" /\ ~ValidFacet(req.facet) =>
        /\ resp.rejected
        /\ resp.reason = "invalid_facet"
        /\ resp.rows = {}

DepthClamped ==
    status = "done" /\ ~resp.rejected /\ resp.mode = "subtree" =>
        resp.effective_depth = ClampDepth(req.depth)

RowsWithinDepth ==
    status = "done" /\ ~resp.rejected /\ resp.mode = "subtree" =>
        \A edge \in resp.rows : edge.depth <= resp.effective_depth

RowsAreActiveConceptEdges ==
    status = "done" /\ ~resp.rejected =>
        \A edge \in resp.rows : EdgeValid(edge)

NoRootAsOwnDescendant ==
    status = "done" /\ ~resp.rejected /\ resp.mode = "subtree" =>
        \A edge \in resp.rows : edge.child # RootId

FacetRowsScoped ==
    status = "done" /\ ~resp.rejected /\ resp.mode = "facet" =>
        \A edge \in resp.rows :
            /\ ConceptFor(edge.child).facet = resp.facet
            /\ ConceptFor(edge.parent).facet = resp.facet

OutputBounded ==
    status = "done" /\ ~resp.rejected =>
        Cardinality(resp.rows) <= FacetLimit

NoDuplicateEdges ==
    status = "done" /\ ~resp.rejected =>
        Cardinality(EdgeKeys(resp.rows)) = Cardinality(resp.rows)

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        BlankRootsRejected /\
        MissingRootsRejected /\
        InvalidFacetsRejected /\
        DepthClamped /\
        RowsWithinDepth /\
        RowsAreActiveConceptEdges /\
        NoRootAsOwnDescendant /\
        FacetRowsScoped /\
        OutputBounded /\
        NoDuplicateEdges)

================================================================================
