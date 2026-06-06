---------------------- MODULE OntologyCreateConceptAtomicity ----------------------
(***************************************************************************)
(* `ontology_create_concept` request boundary and concurrent creation model. *)
(*                                                                         *)
(* The Rust implementation trims and bounds concept names, trims/parses the  *)
(* facet vocabulary, then creates or resolves a concept under exactly one    *)
(* PostgreSQL transaction-level advisory lock keyed by normalized name.      *)
(*                                                                         *)
(* This model explores two workers racing on the same valid request, plus    *)
(* malformed-name/facet requests and an existing canonical concept.          *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

Workers == {"w1", "w2"}
MaxNameChars == 256

Phases == {"start", "locked", "done"}
LockOwners == Workers \cup {"none"}
Outcomes == {"none", "ok", "rejected"}
Reasons == {"none", "bad_name", "bad_facet"}
Statuses == {"none", "candidate", "canonical"}
Facets == {"tool", "component", "collection"}
NameModes == {"valid", "blank", "oversized"}
FacetModes == {"tool", "component", "collection", "blank", "unknown"}

Requests ==
    { [ id |-> 1, name_mode |-> "valid", raw_name |-> " Formal Verification Tool ",
        facet_mode |-> "tool", initial_status |-> "none", initial_facet |-> "tool" ],
      [ id |-> 2, name_mode |-> "blank", raw_name |-> "   ",
        facet_mode |-> "tool", initial_status |-> "none", initial_facet |-> "tool" ],
      [ id |-> 3, name_mode |-> "oversized", raw_name |-> "too-long",
        facet_mode |-> "tool", initial_status |-> "none", initial_facet |-> "tool" ],
      [ id |-> 4, name_mode |-> "valid", raw_name |-> "Z3",
        facet_mode |-> "blank", initial_status |-> "none", initial_facet |-> "tool" ],
      [ id |-> 5, name_mode |-> "valid", raw_name |-> "Z3",
        facet_mode |-> "unknown", initial_status |-> "none", initial_facet |-> "tool" ],
      [ id |-> 6, name_mode |-> "valid", raw_name |-> " Curated Parser ",
        facet_mode |-> "collection", initial_status |-> "canonical",
        initial_facet |-> "component" ],
      [ id |-> 7, name_mode |-> "valid", raw_name |-> " Candidate Parser ",
        facet_mode |-> "collection", initial_status |-> "candidate",
        initial_facet |-> "component" ] }

RequestIds == {r.id : r \in Requests}

NormalizedName(r) ==
    CASE r.name_mode = "valid" /\ r.raw_name = " Formal Verification Tool "
            -> "Formal Verification Tool"
      [] r.name_mode = "valid" /\ r.raw_name = " Curated Parser "
            -> "Curated Parser"
      [] r.name_mode = "valid" /\ r.raw_name = " Candidate Parser "
            -> "Candidate Parser"
      [] r.name_mode = "valid" -> r.raw_name
      [] OTHER -> ""

NameLen(r) ==
    CASE r.name_mode = "valid" -> 24
      [] r.name_mode = "blank" -> 0
      [] r.name_mode = "oversized" -> MaxNameChars + 1

FacetFor(r) ==
    CASE r.facet_mode = "tool" -> "tool"
      [] r.facet_mode = "component" -> "component"
      [] r.facet_mode = "collection" -> "collection"
      [] OTHER -> ""

InitialActive(r) ==
    IF r.initial_status = "none" THEN 0 ELSE 1

ReasonFor(r) ==
    CASE NormalizedName(r) = "" \/ NameLen(r) > MaxNameChars -> "bad_name"
      [] ~(FacetFor(r) \in Facets) -> "bad_facet"
      [] OTHER -> "none"

ResponseRecord ==
    [ outcome: Outcomes,
      reason: Reasons,
      created: BOOLEAN,
      name: {"", "Formal Verification Tool", "Z3", "Curated Parser",
             "Candidate Parser"},
      facet: Facets \cup {"none"},
      status: Statuses ]

InitialResponse ==
    [ outcome |-> "none",
      reason |-> "none",
      created |-> FALSE,
      name |-> "",
      facet |-> "none",
      status |-> "none" ]

VARIABLES req, phase, activeCount, metaStatus, metaFacet, lockOwner, responses

vars == <<req, phase, activeCount, metaStatus, metaFacet, lockOwner, responses>>

Init ==
    /\ req \in Requests
    /\ phase = [w \in Workers |-> "start"]
    /\ activeCount = InitialActive(req)
    /\ metaStatus = req.initial_status
    /\ metaFacet = req.initial_facet
    /\ lockOwner = "none"
    /\ responses = [w \in Workers |-> InitialResponse]

Reject(w) ==
    /\ phase[w] = "start"
    /\ ReasonFor(req) # "none"
    /\ phase' = [phase EXCEPT ![w] = "done"]
    /\ responses' = [responses EXCEPT ![w] =
        [ outcome |-> "rejected",
          reason |-> ReasonFor(req),
          created |-> FALSE,
          name |-> "",
          facet |-> "none",
          status |-> "none" ]]
    /\ UNCHANGED <<req, activeCount, metaStatus, metaFacet, lockOwner>>

Acquire(w) ==
    /\ phase[w] = "start"
    /\ ReasonFor(req) = "none"
    /\ lockOwner = "none"
    /\ phase' = [phase EXCEPT ![w] = "locked"]
    /\ lockOwner' = w
    /\ UNCHANGED <<req, activeCount, metaStatus, metaFacet, responses>>

FinishInsert(w) ==
    /\ phase[w] = "locked"
    /\ lockOwner = w
    /\ activeCount = 0
    /\ activeCount' = 1
    /\ metaStatus' = "candidate"
    /\ metaFacet' = FacetFor(req)
    /\ phase' = [phase EXCEPT ![w] = "done"]
    /\ lockOwner' = "none"
    /\ responses' = [responses EXCEPT ![w] =
        [ outcome |-> "ok",
          reason |-> "none",
          created |-> TRUE,
          name |-> NormalizedName(req),
          facet |-> FacetFor(req),
          status |-> "candidate" ]]
    /\ UNCHANGED req

FinishExisting(w) ==
    /\ phase[w] = "locked"
    /\ lockOwner = w
    /\ activeCount = 1
    /\ LET nextFacet == IF metaStatus = "candidate" THEN FacetFor(req) ELSE metaFacet IN
       /\ activeCount' = activeCount
       /\ metaStatus' = metaStatus
       /\ metaFacet' = nextFacet
       /\ phase' = [phase EXCEPT ![w] = "done"]
       /\ lockOwner' = "none"
       /\ responses' = [responses EXCEPT ![w] =
            [ outcome |-> "ok",
              reason |-> "none",
              created |-> FALSE,
              name |-> NormalizedName(req),
              facet |-> nextFacet,
              status |-> metaStatus ]]
    /\ UNCHANGED req

Next ==
    \E w \in Workers :
        Reject(w) \/ Acquire(w) \/ FinishInsert(w) \/ FinishExisting(w)

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ phase \in [Workers -> Phases]
    /\ activeCount \in 0..2
    /\ metaStatus \in Statuses
    /\ metaFacet \in Facets
    /\ lockOwner \in LockOwners
    /\ responses \in [Workers -> ResponseRecord]

InvalidRequestsDoNotWrite ==
    ReasonFor(req) # "none" =>
        /\ activeCount = InitialActive(req)
        /\ metaStatus = req.initial_status
        /\ metaFacet = req.initial_facet

NoDuplicateActiveConcept ==
    activeCount <= 1

SingleLockOwner ==
    lockOwner # "none" =>
        /\ phase[lockOwner] = "locked"
        /\ \A w \in Workers : w # lockOwner => phase[w] # "locked"

DoneWorkersReleaseLock ==
    \A w \in Workers : phase[w] = "done" => lockOwner # w

AgentCreatesCandidateOnly ==
    \A w \in Workers :
        responses[w].outcome = "ok" /\ responses[w].created =>
            /\ responses[w].status = "candidate"
            /\ responses[w].facet = FacetFor(req)

ExistingCanonicalPreserved ==
    req.initial_status = "canonical" =>
        \A w \in Workers :
            responses[w].outcome = "ok" =>
                /\ responses[w].created = FALSE
                /\ responses[w].status = "canonical"
                /\ responses[w].facet = req.initial_facet

ResponseReflectsPersistedMeta ==
    \A w \in Workers :
        responses[w].outcome = "ok" =>
            /\ responses[w].status = metaStatus
            /\ responses[w].facet = metaFacet

SuccessfulResponsesUseNormalizedName ==
    \A w \in Workers :
        responses[w].outcome = "ok" =>
            /\ responses[w].name = NormalizedName(req)
            /\ responses[w].name # ""
            /\ NameLen(req) \in 1..MaxNameChars

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsDoNotWrite /\
        NoDuplicateActiveConcept /\
        SingleLockOwner /\
        DoneWorkersReleaseLock /\
        AgentCreatesCandidateOnly /\
        ExistingCanonicalPreserved /\
        ResponseReflectsPersistedMeta /\
        SuccessfulResponsesUseNormalizedName)

=============================================================================
