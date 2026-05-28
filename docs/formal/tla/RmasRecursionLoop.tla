---- MODULE RmasRecursionLoop ----
(*
 * ADR-009 Track B (RecursiveMAS / Phase R1) — the latent-recursion loop's
 * decode discipline. RecursiveMAS chains agents A1→…→A_N and loops A_N→A1 for
 * MaxRounds; intermediate rounds stay entirely in latent space and ONLY the
 * final round's last agent decodes to text (Yang et al., arXiv:2604.25917).
 *
 * This abstracts the loop to a baton (`holder`) passing across `NAgents`, a
 * `round` counter, and a `decoded` flag, and proves the medium discipline:
 * `decoded` becomes true only at the final round's last agent.
 *
 * Mechanically checked: TLC (RmasRecursionLoop.cfg; states/ log).
 *)
EXTENDS Naturals

CONSTANTS
    NAgents,                \* agents in the latent loop (A1 .. A_NAgents)
    MaxRounds               \* recursion rounds before the final decode

VARIABLES
    round,                  \* completed loop-backs, 0 .. MaxRounds
    holder,                 \* which agent currently holds the latent state
    decoded                 \* has the final textual answer been produced?

vars == <<round, holder, decoded>>

TypeOK ==
    /\ round \in 0..MaxRounds
    /\ holder \in 1..NAgents
    /\ decoded \in BOOLEAN

Init ==
    /\ round = 0
    /\ holder = 1
    /\ decoded = FALSE

\* Pass the latent state to the next agent (no decode — stays in latent space).
Pass ==
    /\ ~decoded
    /\ holder < NAgents
    /\ holder' = holder + 1
    /\ UNCHANGED <<round, decoded>>

\* Close a non-final round: the last agent hands the latent state back to the
\* first; still no text decode.
LoopBack ==
    /\ ~decoded
    /\ holder = NAgents
    /\ round < MaxRounds
    /\ round' = round + 1
    /\ holder' = 1
    /\ UNCHANGED decoded

\* Final round, last agent: decode the latent state to the textual answer.
Decode ==
    /\ ~decoded
    /\ holder = NAgents
    /\ round = MaxRounds
    /\ decoded' = TRUE
    /\ UNCHANGED <<round, holder>>

Next == Pass \/ LoopBack \/ Decode

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

Done == decoded

RoundsBounded == round <= MaxRounds

\* THE discipline: text is produced only at the final round's last agent —
\* never mid-loop. This is the Tier-1 invariant that, together with the
\* projection side-condition "black-box roles are Text-only", lets the same
\* protocol skeleton carry text or latent media (ADR-009).
LatentNeverDecodedMidLoop ==
    decoded => (round = MaxRounds /\ holder = NAgents)

Invariants ==
    /\ TypeOK
    /\ RoundsBounded
    /\ LatentNeverDecodedMidLoop

DeadlockFreedom == (~Done) => ENABLED Next

EventualTermination == <>Done

THEOREM Safety == Spec => []Invariants
THEOREM Liveness == Spec => EventualTermination
====
