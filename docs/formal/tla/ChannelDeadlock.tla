------------------------------ MODULE ChannelDeadlock ------------------------------
(***************************************************************************)
(* ADR-011 — operational model behind the shadow-ASR channel deadlock      *)
(* analysis (src/graph/petri.rs, `tool_channel_deadlock`).                 *)
(*                                                                         *)
(* Processes communicate over bounded channels by an ordered list of       *)
(* send/recv ops (their sync_ops message skeleton). `Deadlock` = some       *)
(* process is unfinished and EVERY process is finished or blocked (on an    *)
(* empty receive / full send) — a dead marking. This config is the SAFE,    *)
(* gated, green check: process 1 SENDS first, so it is never initially      *)
(* blocked; the system always makes progress and `[]NoDeadlock` holds —     *)
(* matching `channel_cycles`' "initially-blocked-mutual-wait" being absent. *)
(*                                                                         *)
(* WITNESS (manual, red by design): make BOTH processes receive first       *)
(* (ops 1 :> <<recv c1, send c2>>, 2 :> <<recv c2, send c1>>) and check      *)
(* INVARIANT NoDeadlock — TLC reports the reachable mutual-wait deadlock,    *)
(* the communication cycle `tool_channel_deadlock` flags.                  *)
(***************************************************************************)
EXTENDS Naturals, Sequences, TLC

\* Concrete bounded model (TLC .cfg cannot hold `:>`/`@@` literals): 2 processes,
\* 2 channels (1, 2), capacity 1. P1 sends c1 then receives c2; P2 receives c1
\* then sends c2 — P1 is a producer first, so neither starves.
Procs == {1, 2}
Cap   == 1
Ops   == ( 1 :> << [kind |-> "send", chan |-> 1], [kind |-> "recv", chan |-> 2] >>
        @@ 2 :> << [kind |-> "recv", chan |-> 1], [kind |-> "send", chan |-> 2] >> )
Chans == {1, 2}

VARIABLES
    buf,    \* buf[c] : messages buffered on channel c (0 .. Cap)
    pc      \* pc[p]  : index of p's next op (1 .. Len+1)

vars == <<buf, pc>>

Done(p)  == pc[p] > Len(Ops[p])
Op(p)    == Ops[p][pc[p]]

CanSend(p) == ~Done(p) /\ Op(p).kind = "send" /\ buf[Op(p).chan] < Cap
CanRecv(p) == ~Done(p) /\ Op(p).kind = "recv" /\ buf[Op(p).chan] >= 1

TypeOK ==
    /\ buf \in [Chans -> 0..Cap]
    /\ pc \in [Procs -> Nat]
    /\ \A p \in Procs : pc[p] \in 1..(Len(Ops[p]) + 1)

Init ==
    /\ buf = [c \in Chans |-> 0]
    /\ pc  = [p \in Procs |-> 1]

DoSend(p) ==
    /\ CanSend(p)
    /\ buf' = [buf EXCEPT ![Op(p).chan] = @ + 1]
    /\ pc'  = [pc  EXCEPT ![p] = @ + 1]

DoRecv(p) ==
    /\ CanRecv(p)
    /\ buf' = [buf EXCEPT ![Op(p).chan] = @ - 1]
    /\ pc'  = [pc  EXCEPT ![p] = @ + 1]

Next == \E p \in Procs : DoSend(p) \/ DoRecv(p)
Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

\* A process is blocked iff unfinished but can neither send nor receive now.
Blocked(p) == ~Done(p) /\ ~CanSend(p) /\ ~CanRecv(p)

\* Dead marking: someone is unfinished, yet every process is finished or blocked.
Deadlock ==
    /\ \E p \in Procs : ~Done(p)
    /\ \A p \in Procs : Done(p) \/ Blocked(p)

NoDeadlock == ~Deadlock

THEOREM SafeIsDeadlockFree == Spec => []NoDeadlock
=============================================================================
