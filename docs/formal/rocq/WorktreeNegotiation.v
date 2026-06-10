(***************************************************************************)
(*  WorktreeNegotiation.v                                                   *)
(*                                                                          *)
(*  Rocq/Coq companion to WorktreeNegotiation.tla. Proves the central       *)
(*  trust-boundary property of the Phase-4 worktree-coordination protocol:  *)
(*  the dependent project D is unblocked ONLY after pgmcp's git scanner      *)
(*  observed the dependency U stable — never on the Editor agent's `moved`   *)
(*  claim alone. This mirrors the v17 CI-evidence gatekeeper and the         *)
(*  `system_absent_from_judgment_columns` property of the work-item tracker. *)
(*                                                                          *)
(*  No axioms, no admits; closes with Qed.                                   *)
(***************************************************************************)

(* Negotiation phases. *)
Inductive Phase : Type :=
| Idle | Requested | Accepted | Declined | Moved.

(* Protocol state. *)
Record State : Type := mkState {
  phase    : Phase;
  uStable  : bool;   (* scanner observed U on its stable branch & clean *)
  eMoved   : bool;   (* Editor claims it moved edits to a worktree (candidate) *)
  dBlocked : bool    (* dependent D is blocked *)
}.

Definition init : State := mkState Idle false false true.

(* One protocol step. Note: only [Scanner] may set [uStable], and [Unblock] —
   the only action that clears [dBlocked] — is guarded by [uStable]. *)
Inductive step : State -> State -> Prop :=
| StRequest : forall s, phase s = Idle ->
    step s (mkState Requested (uStable s) (eMoved s) (dBlocked s))
| StAccept : forall s, phase s = Requested ->
    step s (mkState Accepted (uStable s) (eMoved s) (dBlocked s))
| StDecline : forall s, phase s = Requested ->
    step s (mkState Declined (uStable s) (eMoved s) (dBlocked s))
| StMoved : forall s, phase s = Accepted ->
    step s (mkState Moved (uStable s) true (dBlocked s))
| StScanner : forall s, eMoved s = true ->
    step s (mkState (phase s) true (eMoved s) (dBlocked s))
| StUnblock : forall s, dBlocked s = true -> uStable s = true ->
    step s (mkState (phase s) (uStable s) (eMoved s) false).

(* Reachable states. *)
Inductive reachable : State -> Prop :=
| reach_init : reachable init
| reach_step : forall s s', reachable s -> step s s' -> reachable s'.

(* ---------------------------------------------------------------------- *)
(* Gatekeeper safety: in every reachable state, if D is unblocked then the *)
(* scanner observed U stable.                                              *)
(* ---------------------------------------------------------------------- *)
Theorem gatekeeper_safety :
  forall s, reachable s -> dBlocked s = false -> uStable s = true.
Proof.
  intros s Hr.
  induction Hr as [| s s' Hr IH Hstep].
  - (* init: dBlocked init = true, so dBlocked init = false is impossible. *)
    simpl. discriminate.
  - (* inductive step: case on the transition that produced s'. *)
    inversion Hstep; subst; simpl in *; intro Hub.
    + (* StRequest: dBlocked and uStable unchanged from s. *)
      apply IH; exact Hub.
    + (* StAccept *)
      apply IH; exact Hub.
    + (* StDecline *)
      apply IH; exact Hub.
    + (* StMoved *)
      apply IH; exact Hub.
    + (* StScanner: uStable s' = true directly. *)
      reflexivity.
    + (* StUnblock: uStable s' = uStable s, and the guard gives uStable s = true. *)
      assumption.
Qed.

(* ---------------------------------------------------------------------- *)
(* Corollary (no false unblock): the Editor's `moved` claim without a       *)
(* scanner observation never leaves D unblocked.                            *)
(* ---------------------------------------------------------------------- *)
Theorem no_unblock_on_claim_alone :
  forall s, reachable s -> eMoved s = true -> uStable s = false -> dBlocked s = true.
Proof.
  intros s Hr Hmoved Hunstable.
  (* If D were unblocked, gatekeeper_safety would force uStable s = true,
     contradicting Hunstable. *)
  destruct (dBlocked s) eqn:Hd.
  - reflexivity.
  - exfalso.
    pose proof (gatekeeper_safety s Hr Hd) as Hstable.
    rewrite Hstable in Hunstable. discriminate.
Qed.
