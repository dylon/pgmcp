---------------------------- MODULE ExperimentDecideAtomicity ----------------------------
(***************************************************************************)
(* `experiment_decide` request boundary and atomic publication model.       *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

Hypotheses == {1}
Experiments == {1}
Verdicts == {"pending", "accepted", "rejected", "inconclusive"}
Statuses == {"running", "decided"}
Outcomes == {"ok", "rejected", "db_error"}
PersistModes == {"commit", "dbfail"}

NoReq == [id |-> 0, hypothesis_id |-> 0, metric_ok |-> FALSE,
          control |-> "", treatment |-> ""]

Requests ==
    { [id |-> 1, hypothesis_id |-> 1, metric_ok |-> TRUE,
       control |-> "control", treatment |-> "treatment"],
      [id |-> 2, hypothesis_id |-> 0, metric_ok |-> TRUE,
       control |-> "control", treatment |-> "treatment"],
      [id |-> 3, hypothesis_id |-> 1, metric_ok |-> FALSE,
       control |-> "control", treatment |-> "treatment"],
      [id |-> 4, hypothesis_id |-> 1, metric_ok |-> TRUE,
       control |-> "same", treatment |-> "same"] }

RequestIds == {r.id : r \in Requests}

ValidRequest(r) ==
    /\ r.hypothesis_id \in Hypotheses
    /\ r.metric_ok
    /\ r.control # ""
    /\ r.treatment # ""
    /\ r.control # r.treatment

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      wrote: BOOLEAN,
      verdict: Verdicts ]

VARIABLES phase, req, results, hypVerdict, expStatus, responses, seen

vars == <<phase, req, results, hypVerdict, expStatus, responses, seen>>

Init ==
    /\ phase = "idle"
    /\ req = NoReq
    /\ results = {}
    /\ hypVerdict = [h \in Hypotheses |-> "pending"]
    /\ expStatus = [e \in Experiments |-> "running"]
    /\ responses = <<>>
    /\ seen = {}

Decide(r, mode) ==
    /\ phase = "idle"
    /\ r \in Requests
    /\ r.id \notin seen
    /\ mode \in PersistModes
    /\ req' = r
    /\ seen' = seen \cup {r.id}
    /\ IF ~ValidRequest(r) THEN
          /\ results' = results
          /\ hypVerdict' = hypVerdict
          /\ expStatus' = expStatus
          /\ responses' = Append(responses,
                [request_id |-> r.id, outcome |-> "rejected",
                 wrote |-> FALSE, verdict |-> "pending"])
       ELSE IF mode = "dbfail" THEN
          /\ results' = results
          /\ hypVerdict' = hypVerdict
          /\ expStatus' = expStatus
          /\ responses' = Append(responses,
                [request_id |-> r.id, outcome |-> "db_error",
                 wrote |-> FALSE, verdict |-> "accepted"])
       ELSE
          /\ results' = results \cup {r.hypothesis_id}
          /\ hypVerdict' = [hypVerdict EXCEPT ![r.hypothesis_id] = "accepted"]
          /\ expStatus' = [expStatus EXCEPT ![1] = "decided"]
          /\ responses' = Append(responses,
                [request_id |-> r.id, outcome |-> "ok",
                 wrote |-> TRUE, verdict |-> "accepted"])
    /\ phase' = "done"

Reset ==
    /\ phase = "done"
    /\ req' = NoReq
    /\ phase' = "idle"
    /\ UNCHANGED <<results, hypVerdict, expStatus, responses, seen>>

Next ==
    \/ \E r \in Requests, mode \in PersistModes : Decide(r, mode)
    \/ Reset

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ phase \in {"idle", "done"}
    /\ req \in Requests \cup {NoReq}
    /\ results \subseteq Hypotheses
    /\ hypVerdict \in [Hypotheses -> Verdicts]
    /\ expStatus \in [Experiments -> Statuses]
    /\ responses \in Seq(ResponseRecord)
    /\ seen \subseteq RequestIds

InvalidRequestsNoWrite ==
    \A i \in 1..Len(responses) :
        LET r == CHOOSE x \in Requests : x.id = responses[i].request_id IN
        ~ValidRequest(r) => responses[i].wrote = FALSE

DbFailureRollsBack ==
    \A i \in 1..Len(responses) :
        responses[i].outcome = "db_error" => responses[i].wrote = FALSE

CommittedDecisionAtomic ==
    \A i \in 1..Len(responses) :
        responses[i].wrote =>
            /\ 1 \in results
            /\ hypVerdict[1] = responses[i].verdict
            /\ expStatus[1] = "decided"

NoPartialPublishedState ==
    (1 \in results) <=> (hypVerdict[1] # "pending" /\ expStatus[1] = "decided")

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsNoWrite /\
        DbFailureRollsBack /\
        CommittedDecisionAtomic /\
        NoPartialPublishedState)

================================================================================
