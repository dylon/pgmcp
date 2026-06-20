---------------------------- MODULE ContainmentFunctor ----------------------------
(***************************************************************************)
(* TLA+ companion to containment_functor.v (ADR-028, item 4).            *)
(*                                                                         *)
(* Models the hierarchical rollup as the Containment functor's action on  *)
(* an extensive metric and checks the strict-sum law with TLC: rolling up *)
(* bottom-up (sum of per-project totals) equals the direct workspace      *)
(* total (sum over all modules). The Rocq proof establishes this for ALL  *)
(* finite hierarchies; TLC validates it over an enumerated sample.        *)
(*                                                                         *)
(* A workspace is a Seq of projects; a project is a Seq of Nat counts.    *)
(***************************************************************************)
EXTENDS Naturals, Sequences

RECURSIVE SumSeq(_)
SumSeq(s) == IF s = <<>> THEN 0 ELSE Head(s) + SumSeq(Tail(s))

ProjectTotal(p) == SumSeq(p)

RECURSIVE MapProjectTotals(_)
MapProjectTotals(w) ==
  IF w = <<>> THEN <<>>
  ELSE <<ProjectTotal(Head(w))>> \o MapProjectTotals(Tail(w))

\* Workspace total via the two-step rollup (file->module->project, project->workspace).
WorkspaceViaRollup(w) == SumSeq(MapProjectTotals(w))

RECURSIVE Flatten(_)
Flatten(w) == IF w = <<>> THEN <<>> ELSE Head(w) \o Flatten(Tail(w))

\* Workspace total computed directly over all modules (ignoring the grouping).
WorkspaceDirect(w) == SumSeq(Flatten(w))

\* An enumerated sample of workspaces TLC checks the law over (incl. empties).
TestWorkspaces ==
  { <<>>,
    << <<1, 2>>, <<3>> >>,
    << <<>>, <<5, 5, 5>>, <<1>> >>,
    << <<2, 2>>, <<2, 2>>, <<2, 2>> >> }

VARIABLE w
Init == w \in TestWorkspaces
Next == UNCHANGED w
Spec == Init /\ [][Next]_w

\* The strict-sum functor law — TLC verifies it for every sampled workspace.
StrictSumLaw == WorkspaceViaRollup(w) = WorkspaceDirect(w)
=================================================================================
