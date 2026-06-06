------------------------ MODULE ExperimentArtifactCapture ------------------------
(***************************************************************************)
(* `experiment_log_artifact` capture boundary.                             *)
(*                                                                         *)
(* The tool accepts ad-hoc profiler/benchmark/debug artifacts. Correctness  *)
(* hinges on normalizing the caller's kind before parser dispatch and        *)
(* storage: `" hyperfine "` must behave exactly like `"hyperfine"`.          *)
(*                                                                         *)
(* Verified:                                                               *)
(*   - empty/whitespace-only kinds are rejected;                            *)
(*   - stored kind is always the normalized non-empty kind;                 *)
(*   - recognized parse requests replace caller metrics with parsed         *)
(*     summaries;                                                           *)
(*   - unrecognized or parse-disabled artifacts preserve caller metrics;    *)
(*   - content hash is present iff content is present.                      *)
(***************************************************************************)

EXTENDS Naturals, Sequences

NoReq == [id |-> 0, raw_kind |-> "", content |-> "none", parse |-> FALSE, user_metrics |-> "user"]

Requests ==
    { [id |-> 1, raw_kind |-> "", content |-> "none", parse |-> TRUE, user_metrics |-> "user"],
      [id |-> 2, raw_kind |-> "   ", content |-> "none", parse |-> TRUE, user_metrics |-> "user"],
      [id |-> 3, raw_kind |-> " hyperfine ", content |-> "hyperfine_json", parse |-> TRUE, user_metrics |-> "user"],
      [id |-> 4, raw_kind |-> "criterion", content |-> "criterion_json", parse |-> TRUE, user_metrics |-> "user"],
      [id |-> 5, raw_kind |-> "log", content |-> "log_text", parse |-> TRUE, user_metrics |-> "user"],
      [id |-> 6, raw_kind |-> "hyperfine", content |-> "hyperfine_json", parse |-> FALSE, user_metrics |-> "user"] }

RequestIds == {r.id : r \in Requests}
NormalizedKinds == {"hyperfine", "criterion", "log"}
MetricsKinds == {"user", "hyperfine_summary", "criterion_summary"}

NormalizedKind(raw) ==
    CASE raw = " hyperfine " -> "hyperfine"
      [] raw = "hyperfine" -> "hyperfine"
      [] raw = "criterion" -> "criterion"
      [] raw = "log" -> "log"
      [] OTHER -> ""

RecognizedParse(kind, content) ==
    \/ /\ kind = "hyperfine" /\ content = "hyperfine_json"
    \/ /\ kind = "criterion" /\ content = "criterion_json"

RequestFor(id) == CHOOSE r \in Requests : r.id = id

ParsedMetrics(kind) ==
    CASE kind = "hyperfine" -> "hyperfine_summary"
      [] kind = "criterion" -> "criterion_summary"
      [] OTHER -> "user"

VARIABLES req, status, artifacts, seen

vars == <<req, status, artifacts, seen>>

ArtifactRows ==
    [ request_id: RequestIds,
      kind: NormalizedKinds,
      metrics: MetricsKinds,
      has_hash: BOOLEAN,
      parsed: BOOLEAN ]

Init ==
    /\ req = NoReq
    /\ status = "idle"
    /\ artifacts = <<>>
    /\ seen = {}

PickRequest(r) ==
    /\ status = "idle"
    /\ r \in Requests
    /\ r.id \notin seen
    /\ req' = r
    /\ status' = "pending"
    /\ UNCHANGED <<artifacts, seen>>

RejectEmptyKind ==
    /\ status = "pending"
    /\ NormalizedKind(req.raw_kind) = ""
    /\ status' = "rejected"
    /\ seen' = seen \cup {req.id}
    /\ UNCHANGED <<req, artifacts>>

AppendArtifact ==
    /\ status = "pending"
    /\ LET kind == NormalizedKind(req.raw_kind) IN
       /\ kind # ""
       /\ LET parsed == req.parse /\ RecognizedParse(kind, req.content) IN
          /\ artifacts' =
                Append(artifacts,
                    [ request_id |-> req.id,
                      kind |-> kind,
                      metrics |-> IF parsed THEN ParsedMetrics(kind) ELSE req.user_metrics,
                      has_hash |-> req.content # "none",
                      parsed |-> parsed ])
          /\ status' = "ok"
    /\ seen' = seen \cup {req.id}
    /\ UNCHANGED req

Reset ==
    /\ status \in {"ok", "rejected"}
    /\ req' = NoReq
    /\ status' = "idle"
    /\ UNCHANGED <<artifacts, seen>>

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ RejectEmptyKind
    \/ AppendArtifact
    \/ Reset

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ status \in {"idle", "pending", "ok", "rejected"}
    /\ artifacts \in Seq(ArtifactRows)
    /\ seen \subseteq RequestIds

EmptyKindRejected ==
    \A i \in 1..Len(artifacts) : artifacts[i].request_id \notin {1, 2}

StoredKindNormalized ==
    \A i \in 1..Len(artifacts) :
        artifacts[i].kind = NormalizedKind(RequestFor(artifacts[i].request_id).raw_kind)

RecognizedParseReplacesMetrics ==
    \A i \in 1..Len(artifacts) :
        artifacts[i].parsed => artifacts[i].metrics = ParsedMetrics(artifacts[i].kind)

NoParsePreservesMetrics ==
    \A i \in 1..Len(artifacts) :
        ~artifacts[i].parsed => artifacts[i].metrics = "user"

HashIffContentPresent ==
    \A i \in 1..Len(artifacts) :
        artifacts[i].has_hash = (RequestFor(artifacts[i].request_id).content # "none")

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        EmptyKindRejected /\
        StoredKindNormalized /\
        RecognizedParseReplacesMetrics /\
        NoParsePreservesMetrics /\
        HashIffContentPresent)

=============================================================================
