------------------------- MODULE WorktreeNegotiation -------------------------
(***************************************************************************)
(* Formal model of the Phase-4 worktree-coordination protocol             *)
(* (ADR-011 addendum / docs decision 011).                                *)
(*                                                                         *)
(* Roles:                                                                  *)
(*   R  — Requester, an agent on a *dependent* project D (build broke).    *)
(*   E  — Editor, an agent on the *dependency* project U (being edited).   *)
(*   pgmcp — the gatekeeper: its git scanner is the ONLY authority that    *)
(*           may observe U "stable" (on its stable branch & clean).        *)
(*                                                                         *)
(* Exchange:  R -> E : request_worktree                                    *)
(*            E -> R : accept | decline                                    *)
(*            E -> R : moved        (a CANDIDATE claim, not authority)      *)
(*            (gate)  pgmcp scanner observes U stable                       *)
(*            System : D.blocked -> ready                                   *)
(*                                                                         *)
(* The central trust-boundary property mirrors the v17 CI-evidence         *)
(* gatekeeper: the Editor (an Agent) can never self-confirm the restore;   *)
(* only the System (git scanner) observation unblocks the dependent.       *)
(***************************************************************************)
EXTENDS Naturals

VARIABLES
    phase,     \* negotiation phase
    uStable,   \* TRUE once pgmcp's git scanner OBSERVED U on its stable branch & clean
    eMoved,    \* TRUE once the Editor CLAIMS it moved its edits to a worktree (candidate)
    dBlocked   \* TRUE while the dependent D is blocked on U

vars == << phase, uStable, eMoved, dBlocked >>

Phases == { "Idle", "Requested", "Accepted", "Declined", "Moved" }

TypeOK ==
    /\ phase \in Phases
    /\ uStable \in BOOLEAN
    /\ eMoved \in BOOLEAN
    /\ dBlocked \in BOOLEAN

Init ==
    /\ phase = "Idle"
    /\ uStable = FALSE   \* U is unstable (being edited) when coordination begins
    /\ eMoved = FALSE
    /\ dBlocked = TRUE    \* D is blocked because U is unstable

Request ==
    /\ phase = "Idle"
    /\ phase' = "Requested"
    /\ UNCHANGED << uStable, eMoved, dBlocked >>

Accept ==
    /\ phase = "Requested"
    /\ phase' = "Accepted"
    /\ UNCHANGED << uStable, eMoved, dBlocked >>

Decline ==
    /\ phase = "Requested"
    /\ phase' = "Declined"
    /\ UNCHANGED << uStable, eMoved, dBlocked >>

\* The Editor's CLAIM that it moved edits to a worktree. A candidate signal only:
\* it sets eMoved but NEVER uStable and NEVER unblocks D.
Moved ==
    /\ phase = "Accepted"
    /\ phase' = "Moved"
    /\ eMoved' = TRUE
    /\ UNCHANGED << uStable, dBlocked >>

\* The ONLY action that may set uStable: pgmcp's git scanner observes that U is
\* back on its stable branch and clean. Enabled only after a real move occurred.
ScannerObservesStable ==
    /\ eMoved = TRUE
    /\ uStable = FALSE
    /\ uStable' = TRUE
    /\ UNCHANGED << phase, eMoved, dBlocked >>

\* System unblocks D — GUARDED by the scanner observation (uStable). Never on the
\* Editor's eMoved claim alone.
Unblock ==
    /\ dBlocked = TRUE
    /\ uStable = TRUE
    /\ dBlocked' = FALSE
    /\ UNCHANGED << phase, uStable, eMoved >>

Next ==
    \/ Request \/ Accept \/ Decline \/ Moved
    \/ ScannerObservesStable \/ Unblock

\* Weak fairness on the progress actions so the liveness property holds: once a
\* request is accepted, the editor eventually moves, the scanner eventually
\* observes, and the system eventually unblocks.
Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(Accept) /\ WF_vars(Moved)
    /\ WF_vars(ScannerObservesStable) /\ WF_vars(Unblock)

-----------------------------------------------------------------------------
(* Properties *)

\* SAFETY (gatekeeper / trust boundary): whenever D is unblocked, the scanner
\* must have observed U stable. The only action clearing dBlocked is Unblock,
\* which is guarded by uStable, and uStable is monotone.
GatekeeperSafety == (~dBlocked) => uStable

\* SAFETY: the Editor's `moved` claim alone never unblocks D — without a scanner
\* observation, D stays blocked (no false unblock on an Agent's say-so).
NoUnblockOnClaimAlone == (eMoved /\ ~uStable) => dBlocked

\* LIVENESS: an accepted request eventually unblocks D (under the fairness above).
EventuallyUnblocked == (phase = "Accepted") ~> (~dBlocked)

=============================================================================
