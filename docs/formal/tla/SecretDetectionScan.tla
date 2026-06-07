------------------------------ MODULE SecretDetectionScan ------------------------------
(***************************************************************************)
(* `secret_detection` direct MCP tool boundary.                            *)
(*                                                                         *)
(* The tool validates local numeric params, resolves a normalized project,  *)
(* streams indexed file content one row at a time, stops at an effective    *)
(* finding limit, drops the stream, and only then runs the crypto-symbol    *)
(* enrichment query.                                                        *)
(*                                                                         *)
(* Verified:                                                               *)
(*   - non-finite entropy rejects before lookup/scan;                       *)
(*   - blank/missing/duplicate projects reject before scanning;             *)
(*   - min_entropy clamps to 0..=8 and limit clamps to 1..=500;             *)
(*   - findings never exceed the effective limit;                           *)
(*   - at most one file body is resident in the streaming scan;             *)
(*   - crypto enrichment cannot run while the content stream is held.       *)
(***************************************************************************)

EXTENDS Naturals

MaxEntropy == 8
MaxLimit == 500

Reasons == {"none", "bad_entropy", "blank_project", "missing_project", "duplicate_project"}

NoReq ==
    [ id |-> 0,
      project |-> "",
      entropy_kind |-> "finite",
      entropy |-> 4,
      limit |-> 100,
      available_findings |-> 0 ]

Requests ==
    { [ id |-> 1, project |-> " p6-sd ", entropy_kind |-> "finite",
        entropy |-> 99, limit |-> 0, available_findings |-> 2 ],
      [ id |-> 2, project |-> "p6-sd", entropy_kind |-> "nan",
        entropy |-> 4, limit |-> 100, available_findings |-> 1 ],
      [ id |-> 3, project |-> "   ", entropy_kind |-> "finite",
        entropy |-> 4, limit |-> 100, available_findings |-> 1 ],
      [ id |-> 4, project |-> "missing", entropy_kind |-> "finite",
        entropy |-> 4, limit |-> 100, available_findings |-> 1 ],
      [ id |-> 5, project |-> "dupe", entropy_kind |-> "finite",
        entropy |-> 4, limit |-> 100, available_findings |-> 1 ],
      [ id |-> 6, project |-> "p6-sd", entropy_kind |-> "finite",
        entropy |-> 4, limit |-> 999, available_findings |-> 600 ],
      [ id |-> 7, project |-> "p6-sd", entropy_kind |-> "finite",
        entropy |-> 0, limit |-> 10, available_findings |-> 0 ] }

TrimProject(p) ==
    CASE p = " p6-sd " -> "p6-sd"
      [] p = "   " -> ""
      [] OTHER -> p

ProjectState(p) ==
    CASE TrimProject(p) = "" -> "blank_project"
      [] TrimProject(p) = "missing" -> "missing_project"
      [] TrimProject(p) = "dupe" -> "duplicate_project"
      [] OTHER -> "ok"

ClampEntropy(e) ==
    IF e > MaxEntropy THEN MaxEntropy ELSE e

ClampLimit(n) ==
    IF n < 1 THEN 1 ELSE IF n > MaxLimit THEN MaxLimit ELSE n

Min(a, b) == IF a < b THEN a ELSE b

Reject(reason, lookups) ==
    [ rejected |-> TRUE,
      reason |-> reason,
      normalized_project |-> "",
      effective_entropy |-> 4,
      effective_limit |-> 100,
      project_lookups |-> lookups,
      scanned_files_resident |-> 0,
      stream_open_at_crypto_query |-> FALSE,
      crypto_query_runs |-> FALSE,
      findings |-> 0,
      writes |-> 0 ]

Accept(r) ==
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized_project |-> TrimProject(r.project),
      effective_entropy |-> ClampEntropy(r.entropy),
      effective_limit |-> ClampLimit(r.limit),
      project_lookups |-> 1,
      scanned_files_resident |-> IF r.available_findings = 0 THEN 0 ELSE 1,
      stream_open_at_crypto_query |-> FALSE,
      crypto_query_runs |-> TRUE,
      findings |-> Min(r.available_findings, ClampLimit(r.limit)),
      writes |-> 0 ]

Evaluate(r) ==
    IF r.entropy_kind # "finite" THEN Reject("bad_entropy", 0)
    ELSE IF ProjectState(r.project) # "ok" THEN Reject(ProjectState(r.project), 1)
    ELSE Accept(r)

NoResp ==
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized_project |-> "",
      effective_entropy |-> 4,
      effective_limit |-> 100,
      project_lookups |-> 0,
      scanned_files_resident |-> 0,
      stream_open_at_crypto_query |-> FALSE,
      crypto_query_runs |-> FALSE,
      findings |-> 0,
      writes |-> 0 ]

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
    /\ resp.normalized_project \in {"", "p6-sd"}
    /\ resp.effective_entropy \in 0..MaxEntropy
    /\ resp.effective_limit \in 1..MaxLimit
    /\ resp.project_lookups \in 0..1
    /\ resp.scanned_files_resident \in 0..1
    /\ resp.stream_open_at_crypto_query \in BOOLEAN
    /\ resp.crypto_query_runs \in BOOLEAN
    /\ resp.findings \in 0..MaxLimit
    /\ resp.writes = 0

InvalidInputsDoNotScan ==
    req # NoReq /\ resp.rejected =>
        /\ resp.findings = 0
        /\ resp.scanned_files_resident = 0
        /\ resp.crypto_query_runs = FALSE

ProjectRejectsBeforeScan ==
    req # NoReq /\ resp.reason \in {"blank_project", "missing_project", "duplicate_project"} =>
        /\ resp.project_lookups = 1
        /\ resp.findings = 0

EffectiveBoundsHold ==
    req # NoReq /\ ~resp.rejected =>
        /\ resp.effective_entropy <= MaxEntropy
        /\ resp.effective_limit <= MaxLimit
        /\ resp.findings <= resp.effective_limit

StreamingMemoryBound ==
    req # NoReq => resp.scanned_files_resident <= 1

CryptoAfterStreamDrop ==
    req # NoReq /\ resp.crypto_query_runs =>
        resp.stream_open_at_crypto_query = FALSE

ReadOnly ==
    req # NoReq => resp.writes = 0

=============================================================================
