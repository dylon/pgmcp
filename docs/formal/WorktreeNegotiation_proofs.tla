---------------------- MODULE WorktreeNegotiation_proofs --------------------
(***************************************************************************)
(* TLAPS deductive proof of the worktree-negotiation gatekeeper safety     *)
(* property — a machine-checked inductive-invariant proof (z3/zenon),       *)
(* stronger than TLC's finite model-check. Kept in a separate module so the *)
(* base `WorktreeNegotiation` stays TLC-checkable without a TLAPS library    *)
(* dependency.                                                              *)
(***************************************************************************)
EXTENDS WorktreeNegotiation, TLAPS

(* The inductive invariant: well-typedness conjoined with the gatekeeper    *)
(* property. GatekeeperSafety is inductive on its own, but carrying TypeOK  *)
(* lets the SMT backend treat the state variables as booleans.             *)
Inv == TypeOK /\ GatekeeperSafety

THEOREM Safety == Spec => []GatekeeperSafety
<1>1. Init => Inv
  BY DEF Init, Inv, TypeOK, GatekeeperSafety, Phases
<1>2. Inv /\ [Next]_vars => Inv'
  BY DEF Inv, TypeOK, GatekeeperSafety, Phases, Next, vars,
         Request, Accept, Decline, Moved, ScannerObservesStable, Unblock
<1>3. Inv => GatekeeperSafety
  BY DEF Inv
<1>4. QED
  BY <1>1, <1>2, <1>3, PTL DEF Spec

=============================================================================
