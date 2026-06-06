------------------------ MODULE ExperimentMeasurementRecord ------------------------
(***************************************************************************)
(* `experiment_record_measurement` boundary and atomic write model.         *)
(*                                                                         *)
(* The tool validates user-submitted experiment measurements, then commits   *)
(* the run row, sample rows, and experiment status in one transaction. The   *)
(* transaction takes exactly one advisory lock keyed by the run identity, so *)
(* concurrent NULL-hypothesis upserts serialize without a lock-order cycle.  *)
(*                                                                         *)
(* Verified:                                                               *)
(*   - malformed requests never create sample batches or status updates;    *)
(*   - DB failures roll back the whole measurement write;                   *)
(*   - successful writes have run, samples, and status as one atomic unit;   *)
(*   - committed labels, metrics, and source values are normalized;          *)
(*   - committed sample counts are non-empty and bounded;                   *)
(*   - warm-up rows are retained but excluded from conformance counts;       *)
(*   - at most one advisory lock is held by the transaction.                *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

MaxSamples == 3

Labels == {"control", "treatment", "paired"}
Metrics == {"latency_ms", "score"}
Sources == {"external_benchmark", "pgmcp_metric", "agent_scalar", "manual"}
Experiments == {"exp1", "exp2"}
Hypotheses == {"none", "hyp1", "hyp2", "missing", "foreign"}
UnitKeyModes == {"none", "aligned", "length_mismatch", "empty", "duplicate"}

NoReq ==
    [ id |-> 0,
      experiment |-> "exp1",
      experiment_exists |-> TRUE,
      hypothesis |-> "none",
      arm_label |-> "",
      arm_kind |-> "control",
      metric |-> "",
      sample_count |-> 0,
      finite |-> TRUE,
      source |-> "manual",
      unit_key_mode |-> "none",
      warmup |-> FALSE,
      db_fails |-> FALSE ]

Requests ==
    { [ id |-> 1, experiment |-> "exp1", experiment_exists |-> TRUE, hypothesis |-> "hyp1",
        arm_label |-> "", arm_kind |-> "control", metric |-> "latency_ms", sample_count |-> 1,
        finite |-> TRUE, source |-> "manual", unit_key_mode |-> "none", warmup |-> FALSE,
        db_fails |-> FALSE ],
      [ id |-> 2, experiment |-> "exp1", experiment_exists |-> TRUE, hypothesis |-> "hyp1",
        arm_label |-> "control", arm_kind |-> "control", metric |-> "", sample_count |-> 1,
        finite |-> TRUE, source |-> "manual", unit_key_mode |-> "none", warmup |-> FALSE,
        db_fails |-> FALSE ],
      [ id |-> 3, experiment |-> "exp1", experiment_exists |-> TRUE, hypothesis |-> "hyp1",
        arm_label |-> "control", arm_kind |-> "control", metric |-> "latency_ms", sample_count |-> 0,
        finite |-> TRUE, source |-> "manual", unit_key_mode |-> "none", warmup |-> FALSE,
        db_fails |-> FALSE ],
      [ id |-> 4, experiment |-> "exp1", experiment_exists |-> TRUE, hypothesis |-> "hyp1",
        arm_label |-> "control", arm_kind |-> "control", metric |-> "latency_ms",
        sample_count |-> MaxSamples + 1, finite |-> TRUE, source |-> "manual",
        unit_key_mode |-> "none", warmup |-> FALSE, db_fails |-> FALSE ],
      [ id |-> 5, experiment |-> "exp1", experiment_exists |-> TRUE, hypothesis |-> "hyp1",
        arm_label |-> "control", arm_kind |-> "control", metric |-> "latency_ms", sample_count |-> 1,
        finite |-> FALSE, source |-> "manual", unit_key_mode |-> "none", warmup |-> FALSE,
        db_fails |-> FALSE ],
      [ id |-> 6, experiment |-> "exp1", experiment_exists |-> TRUE, hypothesis |-> "hyp1",
        arm_label |-> "control", arm_kind |-> "control", metric |-> "latency_ms", sample_count |-> 1,
        finite |-> TRUE, source |-> "spreadsheet", unit_key_mode |-> "none", warmup |-> FALSE,
        db_fails |-> FALSE ],
      [ id |-> 7, experiment |-> "exp1", experiment_exists |-> TRUE, hypothesis |-> "hyp1",
        arm_label |-> "control", arm_kind |-> "control", metric |-> "latency_ms", sample_count |-> 2,
        finite |-> TRUE, source |-> "manual", unit_key_mode |-> "length_mismatch", warmup |-> FALSE,
        db_fails |-> FALSE ],
      [ id |-> 8, experiment |-> "exp1", experiment_exists |-> TRUE, hypothesis |-> "hyp1",
        arm_label |-> "control", arm_kind |-> "control", metric |-> "latency_ms", sample_count |-> 2,
        finite |-> TRUE, source |-> "manual", unit_key_mode |-> "duplicate", warmup |-> FALSE,
        db_fails |-> FALSE ],
      [ id |-> 9, experiment |-> "exp1", experiment_exists |-> FALSE, hypothesis |-> "hyp1",
        arm_label |-> "control", arm_kind |-> "control", metric |-> "latency_ms", sample_count |-> 1,
        finite |-> TRUE, source |-> "manual", unit_key_mode |-> "none", warmup |-> FALSE,
        db_fails |-> FALSE ],
      [ id |-> 10, experiment |-> "exp1", experiment_exists |-> TRUE, hypothesis |-> "missing",
        arm_label |-> "control", arm_kind |-> "control", metric |-> "latency_ms", sample_count |-> 1,
        finite |-> TRUE, source |-> "manual", unit_key_mode |-> "none", warmup |-> FALSE,
        db_fails |-> FALSE ],
      [ id |-> 11, experiment |-> "exp1", experiment_exists |-> TRUE, hypothesis |-> "foreign",
        arm_label |-> "control", arm_kind |-> "control", metric |-> "latency_ms", sample_count |-> 1,
        finite |-> TRUE, source |-> "manual", unit_key_mode |-> "none", warmup |-> FALSE,
        db_fails |-> FALSE ],
      [ id |-> 12, experiment |-> "exp1", experiment_exists |-> TRUE, hypothesis |-> "hyp1",
        arm_label |-> " paired ", arm_kind |-> " control ", metric |-> " latency_ms ",
        sample_count |-> 2, finite |-> TRUE, source |-> " manual ",
        unit_key_mode |-> "aligned", warmup |-> FALSE, db_fails |-> FALSE ],
      [ id |-> 13, experiment |-> "exp2", experiment_exists |-> TRUE, hypothesis |-> "none",
        arm_label |-> "treatment", arm_kind |-> "treatment", metric |-> "score", sample_count |-> 1,
        finite |-> TRUE, source |-> "agent_scalar", unit_key_mode |-> "none", warmup |-> FALSE,
        db_fails |-> FALSE ],
      [ id |-> 14, experiment |-> "exp1", experiment_exists |-> TRUE, hypothesis |-> "hyp1",
        arm_label |-> "control", arm_kind |-> "control", metric |-> "latency_ms", sample_count |-> 1,
        finite |-> TRUE, source |-> "manual", unit_key_mode |-> "none", warmup |-> FALSE,
        db_fails |-> TRUE ],
      [ id |-> 15, experiment |-> "exp1", experiment_exists |-> TRUE, hypothesis |-> "hyp1",
        arm_label |-> "control", arm_kind |-> "control", metric |-> "latency_ms", sample_count |-> 1,
        finite |-> TRUE, source |-> "manual", unit_key_mode |-> "none", warmup |-> TRUE,
        db_fails |-> FALSE ] }

RequestIds == {r.id : r \in Requests}

NormalizeLabel(raw) ==
    CASE raw = " paired " -> "paired"
      [] raw = " control " -> "control"
      [] OTHER -> raw

NormalizeMetric(raw) ==
    CASE raw = " latency_ms " -> "latency_ms"
      [] OTHER -> raw

NormalizeSource(raw) ==
    CASE raw = " manual " -> "manual"
      [] raw = "" -> "manual"
      [] OTHER -> raw

ValidHypothesis(r) ==
    \/ r.hypothesis = "none"
    \/ r.hypothesis = "hyp1"
    \/ /\ r.experiment = "exp2" /\ r.hypothesis = "hyp2"

ValidUnitKeys(r) ==
    r.unit_key_mode \in {"none", "aligned"}

ValidRequest(r) ==
    /\ r.experiment_exists
    /\ NormalizeLabel(r.arm_label) \in Labels
    /\ NormalizeLabel(r.arm_kind) \in {"control", "treatment", "baseline"}
    /\ NormalizeMetric(r.metric) \in Metrics
    /\ r.sample_count \in 1..MaxSamples
    /\ r.finite
    /\ NormalizeSource(r.source) \in Sources
    /\ ValidUnitKeys(r)
    /\ ValidHypothesis(r)

RunKey(r) ==
    << r.experiment, r.hypothesis, NormalizeLabel(r.arm_label) >>

NoKey == <<"none", "none", "none">>

RunKeys == {RunKey(r) : r \in Requests}

RequestFor(id) == CHOOSE r \in Requests : r.id = id

BatchRows ==
    [ request_id: RequestIds,
      run_key: RunKeys,
      arm: Labels,
      metric: Metrics,
      source: Sources,
      sample_count: 1..MaxSamples,
      conformance_count: 0..MaxSamples,
      warmup: BOOLEAN ]

VARIABLES req, status, lock, run_keys, sample_batches, status_updates

vars == <<req, status, lock, run_keys, sample_batches, status_updates>>

Init ==
    /\ req = NoReq
    /\ status = "idle"
    /\ lock = NoKey
    /\ run_keys = {}
    /\ sample_batches = <<>>
    /\ status_updates = {}

PickRequest(r) ==
    /\ status = "idle"
    /\ r \in Requests
    /\ req' = r
    /\ status' = "pending"
    /\ UNCHANGED <<lock, run_keys, sample_batches, status_updates>>

RejectInvalid ==
    /\ status = "pending"
    /\ ~ValidRequest(req)
    /\ status' = "rejected"
    /\ UNCHANGED <<req, lock, run_keys, sample_batches, status_updates>>

AcquireLock ==
    /\ status = "pending"
    /\ ValidRequest(req)
    /\ lock = NoKey
    /\ lock' = RunKey(req)
    /\ status' = "locked"
    /\ UNCHANGED <<req, run_keys, sample_batches, status_updates>>

Commit ==
    /\ status = "locked"
    /\ ~req.db_fails
    /\ run_keys' = run_keys \cup {lock}
    /\ sample_batches' =
        Append(sample_batches,
            [ request_id |-> req.id,
              run_key |-> RunKey(req),
              arm |-> NormalizeLabel(req.arm_label),
              metric |-> NormalizeMetric(req.metric),
              source |-> NormalizeSource(req.source),
              sample_count |-> req.sample_count,
              conformance_count |-> IF req.warmup THEN 0 ELSE req.sample_count,
              warmup |-> req.warmup ])
    /\ status_updates' = status_updates \cup {req.id}
    /\ lock' = NoKey
    /\ status' = "ok"
    /\ UNCHANGED req

DbFailureRollback ==
    /\ status = "locked"
    /\ req.db_fails
    /\ lock' = NoKey
    /\ status' = "db_error"
    /\ UNCHANGED <<req, run_keys, sample_batches, status_updates>>

TerminalStutter ==
    /\ status \in {"ok", "rejected", "db_error"}
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ RejectInvalid
    \/ AcquireLock
    \/ Commit
    \/ DbFailureRollback
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

BatchIds == {sample_batches[i].request_id : i \in 1..Len(sample_batches)}

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ status \in {"idle", "pending", "locked", "ok", "rejected", "db_error"}
    /\ lock \in RunKeys \cup {NoKey}
    /\ run_keys \subseteq RunKeys
    /\ sample_batches \in Seq(BatchRows)
    /\ status_updates \subseteq RequestIds

NoDanglingLock ==
    (status = "locked") <=> (lock # NoKey)

InvalidRequestsHaveNoSamplesOrStatus ==
    \A r \in Requests :
        ~ValidRequest(r) =>
            /\ r.id \notin BatchIds
            /\ r.id \notin status_updates

DbFailuresRollback ==
    \A r \in Requests :
        r.db_fails =>
            /\ r.id \notin BatchIds
            /\ r.id \notin status_updates

AtomicRunSamplesStatus ==
    /\ BatchIds = status_updates
    /\ \A i \in 1..Len(sample_batches) :
        sample_batches[i].run_key \in run_keys

CommittedRowsAreValidated ==
    \A i \in 1..Len(sample_batches) :
        /\ ValidRequest(RequestFor(sample_batches[i].request_id))
        /\ ~RequestFor(sample_batches[i].request_id).db_fails

CommittedTextIsNormalized ==
    \A i \in 1..Len(sample_batches) :
        LET r == RequestFor(sample_batches[i].request_id) IN
            /\ sample_batches[i].arm = NormalizeLabel(r.arm_label)
            /\ sample_batches[i].metric = NormalizeMetric(r.metric)
            /\ sample_batches[i].source = NormalizeSource(r.source)

SampleCountsAreBounded ==
    \A i \in 1..Len(sample_batches) :
        sample_batches[i].sample_count \in 1..MaxSamples

WarmupsExcludedFromConformance ==
    \A i \in 1..Len(sample_batches) :
        sample_batches[i].conformance_count =
            IF sample_batches[i].warmup THEN 0 ELSE sample_batches[i].sample_count

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        NoDanglingLock /\
        InvalidRequestsHaveNoSamplesOrStatus /\
        DbFailuresRollback /\
        AtomicRunSamplesStatus /\
        CommittedRowsAreValidated /\
        CommittedTextIsNormalized /\
        SampleCountsAreBounded /\
        WarmupsExcludedFromConformance)

=============================================================================
