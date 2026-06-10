----------------------------- MODULE A2aMailboxBoundary -----------------------------
(***************************************************************************)
(* Request and delivery boundary for the A2A agent-mailbox tools:          *)
(*   a2a_send_message and a2a_inbox.                                       *)
(*                                                                         *)
(* a2a_send_message obligations:                                           *)
(*   - the message body is required: trimmed, blank -> rejected;           *)
(*   - kind defaults to "message"; trimmed; validated against the closed   *)
(*     MessageKind vocab {message,request,fyi,request_worktree,accept,     *)
(*     decline,moved}; anything else -> rejected;                          *)
(*   - a `to_project` NAME (when present, non-blank) resolves fail-closed   *)
(*     via project_id_or_err (DUPLICATE -> rejected);                      *)
(*   - at least one of {to_session, to_project, to_agent} must address the *)
(*     message, else -> rejected.                                          *)
(*                                                                         *)
(* a2a_inbox obligations:                                                  *)
(*   - at least one of {session, project, agent} must be supplied;         *)
(*   - the project filter resolves fail-closed (DUPLICATE -> rejected) so  *)
(*     the inbox can never silently widen to "no project filter";          *)
(*   - read-marking writes a receipt keyed to the resolved recipient       *)
(*     session, and the `ON CONFLICT (message_id, recipient_session)`      *)
(*     upsert means a message is delivered AT MOST ONCE per recipient per  *)
(*     channel.                                                            *)
(*                                                                         *)
(* One request is processed per behavior (CircularDependenciesScope shape) *)
(* so the state space stays small and finite.                             *)
(***************************************************************************)

EXTENDS Naturals, Integers, Sequences, FiniteSets

Outcomes == {"ok", "rejected"}

\* The closed MessageKind vocabulary (mailbox.rs::MessageKind, 7 values).
MessageKinds ==
    {"message", "request", "fyi", "request_worktree", "accept", "decline", "moved"}
DefaultKind == "message"

\* "p" / "q" are unique projects; "dup" is a duplicate display-name pair the
\* resolver must reject when used as a to_project / inbox project filter.
Projects ==
    { [id |-> 1, name |-> "p"],
      [id |-> 2, name |-> "q"],
      [id |-> 3, name |-> "dup"],
      [id |-> 4, name |-> "dup"] }

ProjectIds == {p.id : p \in Projects}

Tools == {"send", "inbox"}

\* Sentinel "-" means "field absent". For send: body/kind/to_project/to_session/
\* to_agent. For inbox: project/session/agent are reused via to_project/to_session/
\* to_agent and body/kind are "-".
NoStr == "-"

\* SEND requests exercise: blank body, valid body, default+explicit+invalid kind,
\* duplicate/unknown/valid to_project, and the no-address case.
\* INBOX requests exercise: no-address, fail-closed duplicate project filter,
\* blank-project + session, and a valid session+project recipient.
Requests ==
    { \* a2a_send_message
      [id |-> 1, tool |-> "send", body |-> "hi",  kind |-> NoStr,
       to_project |-> "p",   to_session |-> NoStr, to_agent |-> NoStr],
      [id |-> 2, tool |-> "send", body |-> "   ", kind |-> NoStr,
       to_project |-> "p",   to_session |-> NoStr, to_agent |-> NoStr],
      [id |-> 3, tool |-> "send", body |-> "yo",  kind |-> "fyi",
       to_project |-> NoStr, to_session |-> "s1",  to_agent |-> NoStr],
      [id |-> 4, tool |-> "send", body |-> "yo",  kind |-> "bogus",
       to_project |-> NoStr, to_session |-> "s1",  to_agent |-> NoStr],
      [id |-> 5, tool |-> "send", body |-> "yo",  kind |-> NoStr,
       to_project |-> "dup", to_session |-> NoStr, to_agent |-> NoStr],
      [id |-> 6, tool |-> "send", body |-> "yo",  kind |-> NoStr,
       to_project |-> "missing", to_session |-> NoStr, to_agent |-> NoStr],
      [id |-> 7, tool |-> "send", body |-> "yo",  kind |-> NoStr,
       to_project |-> NoStr, to_session |-> NoStr, to_agent |-> NoStr],
      [id |-> 8, tool |-> "send", body |-> "yo",  kind |-> "request_worktree",
       to_project |-> NoStr, to_session |-> NoStr, to_agent |-> "claude"],
      \* a2a_inbox
      [id |-> 9, tool |-> "inbox", body |-> NoStr, kind |-> NoStr,
       to_project |-> NoStr, to_session |-> NoStr, to_agent |-> NoStr],
      [id |-> 10, tool |-> "inbox", body |-> NoStr, kind |-> NoStr,
       to_project |-> "dup", to_session |-> "s1", to_agent |-> NoStr],
      [id |-> 11, tool |-> "inbox", body |-> NoStr, kind |-> NoStr,
       to_project |-> "  ", to_session |-> "s1", to_agent |-> NoStr],
      [id |-> 12, tool |-> "inbox", body |-> NoStr, kind |-> NoStr,
       to_project |-> "p", to_session |-> "s1", to_agent |-> NoStr] }

RequestIds == {r.id : r \in Requests}

\* Trim model: only the synthetic padded inputs ("   "/"  ") trim to blank.
Trim(s) ==
    CASE s = "   " -> ""
      [] s = "  "  -> ""
      [] OTHER     -> s

ProjectMatches(name) == {p \in Projects : p.name = name}

\* project_id_or_err on a present, non-blank name. 0 = not a unique id.
ResolveId(name) ==
    LET t == Trim(name) IN
    IF t = "" THEN 0
    ELSE IF Cardinality(ProjectMatches(t)) = 1
         THEN (CHOOSE p \in ProjectMatches(t) : TRUE).id
         ELSE 0

\* A name field is "present" when it is not the absent sentinel.
Present(s) == s # NoStr

\* Whether a present-but-non-blank project NAME fails to resolve (blank counts as
\* absent, so does not by itself reject — but for send/inbox a duplicate/unknown
\* present name rejects fail-closed).
ProjectFilterRejects(name) ==
    Present(name) /\ Trim(name) # "" /\ ResolveId(name) = 0

\* Resolved to_project id for a send (None unless a present non-blank name).
ToProjectId(r) ==
    IF Present(r.to_project) /\ Trim(r.to_project) # "" THEN ResolveId(r.to_project) ELSE 0

\* send body: required, trimmed, blank rejects.
BodyOk(r) == Present(r.body) /\ Trim(r.body) # ""

\* send kind: default "message" when absent/blank; else must be in the closed vocab.
EffKind(r) ==
    LET k == IF Present(r.kind) /\ Trim(r.kind) # "" THEN Trim(r.kind) ELSE DefaultKind IN
    k
KindOk(r) == EffKind(r) \in MessageKinds

\* send addressing: at least one of to_session / to_project / to_agent present.
SendAddressed(r) ==
    Present(r.to_session) \/ Present(r.to_project) \/ Present(r.to_agent)

\* inbox addressing: at least one of session(=to_session)/project(=to_project)/
\* agent(=to_agent) present.
InboxAddressed(r) ==
    Present(r.to_session) \/ Present(r.to_project) \/ Present(r.to_agent)

SendAccepted(r) ==
    /\ SendAddressed(r)
    /\ BodyOk(r)
    /\ KindOk(r)
    /\ ~ProjectFilterRejects(r.to_project)

InboxAccepted(r) ==
    /\ InboxAddressed(r)
    /\ ~ProjectFilterRejects(r.to_project)

RequestAccepted(r) ==
    IF r.tool = "send" THEN SendAccepted(r) ELSE InboxAccepted(r)

\* The recipient session a read receipt is keyed to (inbox). "-" when none.
RecipientSession(r) == r.to_session

ResponseFor(r) ==
    LET accepted == RequestAccepted(r) IN
    [ request_id   |-> r.id,
      tool         |-> r.tool,
      outcome      |-> IF accepted THEN "ok" ELSE "rejected",
      \* the kind actually stored on a sent message ("" when not a send / rejected).
      stored_kind  |-> IF r.tool = "send" /\ accepted THEN EffKind(r) ELSE "",
      \* the resolved to_project id on a sent message (0 = none / rejected).
      to_project_id|-> IF r.tool = "send" /\ accepted THEN ToProjectId(r) ELSE 0,
      \* message_sent: send writes exactly one message iff accepted.
      message_sent |-> r.tool = "send" /\ accepted,
      \* receipt_session: the recipient a read receipt is keyed to (inbox, ok).
      receipt_session |-> IF r.tool = "inbox" /\ accepted
                          THEN RecipientSession(r) ELSE NoStr,
      \* receipts_per_recipient: the receipt upsert is keyed (message,recipient)
      \* so re-reading never produces more than one receipt per recipient per
      \* channel. Modeled as the at-most-one count after one inbox read.
      receipts_per_recipient |-> IF r.tool = "inbox" /\ accepted
                                 /\ Present(RecipientSession(r)) THEN 1 ELSE 0 ]

StoredKindDomain == MessageKinds \cup {""}

ResponseRecord ==
    [ request_id: RequestIds,
      tool: Tools,
      outcome: Outcomes,
      stored_kind: StoredKindDomain,
      to_project_id: ProjectIds \cup {0},
      message_sent: BOOLEAN,
      receipt_session: {r.to_session : r \in Requests} \cup {NoStr},
      receipts_per_recipient: 0..1 ]

VARIABLES req, response

vars == <<req, response>>

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord
    /\ response.request_id = req.id

\* a2a_send_message with a blank (post-trim) body is rejected and sends nothing.
SendRequiresBody ==
    (req.tool = "send" /\ ~BodyOk(req)) =>
        /\ response.outcome = "rejected"
        /\ response.message_sent = FALSE

\* Any sent message carries a kind from the closed MessageKind vocabulary.
StoredKindClosedVocab ==
    response.message_sent => response.stored_kind \in MessageKinds

\* Absent/blank kind defaults to "message" on the stored message.
KindDefaultsToMessage ==
    (req.tool = "send" /\ response.message_sent /\ ~ (Present(req.kind) /\ Trim(req.kind) # "")) =>
        response.stored_kind = DefaultKind

\* An out-of-vocab kind is rejected and sends nothing.
InvalidKindRejects ==
    (req.tool = "send" /\ Present(req.kind) /\ Trim(req.kind) # "" /\ ~KindOk(req)) =>
        /\ response.outcome = "rejected"
        /\ response.message_sent = FALSE

\* A present-but-non-blank to_project name resolves fail closed (duplicate/unknown
\* -> rejected, no send). A resolved to_project id is a real project id.
ToProjectFailClosed ==
    /\ (req.tool = "send" /\ ProjectFilterRejects(req.to_project)) =>
          /\ response.outcome = "rejected"
          /\ response.message_sent = FALSE
    /\ (response.message_sent /\ response.to_project_id # 0) =>
          response.to_project_id \in ProjectIds

\* A message with no recipient (send) is rejected and sends nothing.
SendMustAddress ==
    (req.tool = "send" /\ ~SendAddressed(req)) =>
        /\ response.outcome = "rejected"
        /\ response.message_sent = FALSE

\* a2a_inbox with no recipient address is rejected.
InboxMustAddress ==
    (req.tool = "inbox" /\ ~InboxAddressed(req)) =>
        response.outcome = "rejected"

\* a2a_inbox project filter resolves fail closed (duplicate/unknown -> rejected),
\* so the inbox never silently widens to "no project filter".
InboxProjectFilterFailClosed ==
    (req.tool = "inbox" /\ ProjectFilterRejects(req.to_project)) =>
        response.outcome = "rejected"

\* Read-marking is keyed to the resolved recipient session (the caller's own
\* session), never some other recipient.
ReceiptKeyedToRecipient ==
    (req.tool = "inbox" /\ response.outcome = "ok") =>
        response.receipt_session = RecipientSession(req)

\* Dedup/receipts: a message is delivered AT MOST ONCE per recipient per channel
\* (the (message_id, recipient_session) upsert), so one inbox read never records
\* more than one receipt for the recipient.
DeliveredAtMostOncePerRecipient ==
    response.receipts_per_recipient <= 1

\* Rejected requests are inert: no message sent, no receipt written.
RejectedRequestsInert ==
    response.outcome = "rejected" =>
        /\ response.message_sent = FALSE
        /\ response.receipts_per_recipient = 0

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        SendRequiresBody /\
        StoredKindClosedVocab /\
        KindDefaultsToMessage /\
        InvalidKindRejects /\
        ToProjectFailClosed /\
        SendMustAddress /\
        InboxMustAddress /\
        InboxProjectFilterFailClosed /\
        ReceiptKeyedToRecipient /\
        DeliveredAtMostOncePerRecipient /\
        RejectedRequestsInert)

================================================================================
