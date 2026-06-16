//! Storage + retrieval for the A2A agent mailbox (`agent_messages` /
//! `agent_message_receipts`). The `a2a_send_message` / `a2a_inbox` /
//! `a2a_reply_message` / `a2a_ack_message` tools and the Phase-3 delivery stages
//! (UserPromptSubmit / SessionStart / PostToolUse) all go through here.
//!
//! Sessions are stored as TEXT (matching `mcp_clients.mcp_session_id` and the
//! Claude-Code hook `session_id`, both transport strings, not necessarily
//! UUIDs). Closed vocabularies (`MessageKind`, `DeliveryChannel`) are validated
//! by the callers; this layer treats them as already-checked `&str`.

use serde::Serialize;
use sqlx::PgPool;

/// A message to enqueue. At least one of `to_session` / `to_project_id` /
/// `to_agent` must be set (enforced by the table CHECK).
#[derive(Debug)]
pub struct NewMessage<'a> {
    pub from_agent: &'a str,
    pub from_session: Option<&'a str>,
    pub to_session: Option<&'a str>,
    pub to_project_id: Option<i32>,
    pub to_agent: Option<&'a str>,
    pub kind: &'a str,
    pub subject: Option<&'a str>,
    pub body: &'a str,
    pub reply_to: Option<i64>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Insert a message; returns its id.
pub async fn send(pool: &PgPool, m: &NewMessage<'_>) -> Result<i64, sqlx::Error> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO agent_messages
            (from_agent, from_session, to_session, to_project_id, to_agent,
             kind, subject, body, reply_to, expires_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
         RETURNING id",
    )
    .bind(m.from_agent)
    .bind(m.from_session)
    .bind(m.to_session)
    .bind(m.to_project_id)
    .bind(m.to_agent)
    .bind(m.kind)
    .bind(m.subject)
    .bind(m.body)
    .bind(m.reply_to)
    .bind(m.expires_at)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// One inbox row: a message addressed to the recipient, with that recipient's
/// receipt state (delivered/read/acked timestamps; NULL = not yet).
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct InboxRow {
    pub id: i64,
    pub from_agent: String,
    pub from_session: Option<String>,
    pub to_session: Option<String>,
    pub to_project_id: Option<i32>,
    pub to_agent: Option<String>,
    pub kind: String,
    pub subject: Option<String>,
    pub body: String,
    pub reply_to: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub read_at: Option<chrono::DateTime<chrono::Utc>>,
    pub acked_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Messages addressed to this recipient — by `recipient_session` (precise
/// instance), `recipient_project_id` (any agent on the project), or
/// `recipient_agent` (client-type broadcast) — that have not expired. The
/// receipt state is joined on `recipient_session`. With `unread_only`, rows the
/// recipient has already read are excluded.
pub async fn inbox(
    pool: &PgPool,
    recipient_session: Option<&str>,
    recipient_project_id: Option<i32>,
    recipient_agent: Option<&str>,
    unread_only: bool,
) -> Result<Vec<InboxRow>, sqlx::Error> {
    sqlx::query_as::<_, InboxRow>(
        "SELECT m.id, m.from_agent, m.from_session, m.to_session, m.to_project_id,
                m.to_agent, m.kind, m.subject, m.body, m.reply_to, m.created_at,
                r.delivered_at, r.read_at, r.acked_at
           FROM agent_messages m
           LEFT JOIN agent_message_receipts r
                  ON r.message_id = m.id AND r.recipient_session = $1
          WHERE (m.expires_at IS NULL OR m.expires_at > now())
            AND ( ($1::text IS NOT NULL AND m.to_session = $1)
               OR ($2::int  IS NOT NULL AND m.to_project_id = $2)
               OR ($3::text IS NOT NULL AND m.to_agent = $3) )
            AND ($4 = FALSE OR r.read_at IS NULL)
          ORDER BY m.created_at DESC",
    )
    .bind(recipient_session)
    .bind(recipient_project_id)
    .bind(recipient_agent)
    .bind(unread_only)
    .fetch_all(pool)
    .await
}

/// Which receipt timestamp a `record_receipt` call stamps.
#[derive(Debug, Clone, Copy)]
pub enum Mark {
    Delivered,
    Read,
    Acked,
}

/// Upsert a receipt for `(message_id, recipient_session)`, stamping the column
/// for `mark` (and `channel` on a delivery). Idempotent: re-stamping keeps the
/// first non-NULL timestamp via COALESCE so dedup logic stays stable.
pub async fn record_receipt(
    pool: &PgPool,
    message_id: i64,
    recipient_session: Option<&str>,
    recipient_agent: Option<&str>,
    channel: Option<&str>,
    mark: Mark,
) -> Result<(), sqlx::Error> {
    let (delivered, read, acked) = match mark {
        Mark::Delivered => ("now()", "NULL", "NULL"),
        Mark::Read => ("now()", "now()", "NULL"), // a read implies delivered
        Mark::Acked => ("now()", "now()", "now()"), // an ack implies read + delivered
    };
    let sql = format!(
        "INSERT INTO agent_message_receipts
            (message_id, recipient_session, recipient_agent, delivered_at, read_at, acked_at, channel)
         VALUES ($1, $2, $3, {delivered}, {read}, {acked}, $4)
         ON CONFLICT (message_id, recipient_session) DO UPDATE SET
            recipient_agent = COALESCE(EXCLUDED.recipient_agent, agent_message_receipts.recipient_agent),
            delivered_at = COALESCE(agent_message_receipts.delivered_at, EXCLUDED.delivered_at),
            read_at      = COALESCE(agent_message_receipts.read_at, EXCLUDED.read_at),
            acked_at     = COALESCE(agent_message_receipts.acked_at, EXCLUDED.acked_at),
            channel      = COALESCE(agent_message_receipts.channel, EXCLUDED.channel)"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(message_id)
        .bind(recipient_session)
        .bind(recipient_agent)
        .bind(channel)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Undelivered messages for a recipient (no receipt, or a receipt with NULL
/// `delivered_at`), for the delivery stages to render + mark. Same addressing as
/// [`inbox`].
pub async fn pending_undelivered(
    pool: &PgPool,
    recipient_session: Option<&str>,
    recipient_project_id: Option<i32>,
    recipient_agent: Option<&str>,
    limit: i64,
) -> Result<Vec<InboxRow>, sqlx::Error> {
    sqlx::query_as::<_, InboxRow>(
        "SELECT m.id, m.from_agent, m.from_session, m.to_session, m.to_project_id,
                m.to_agent, m.kind, m.subject, m.body, m.reply_to, m.created_at,
                r.delivered_at, r.read_at, r.acked_at
           FROM agent_messages m
           LEFT JOIN agent_message_receipts r
                  ON r.message_id = m.id AND r.recipient_session = $1
          WHERE (m.expires_at IS NULL OR m.expires_at > now())
            AND r.delivered_at IS NULL
            AND ( ($1::text IS NOT NULL AND m.to_session = $1)
               OR ($2::int  IS NOT NULL AND m.to_project_id = $2)
               OR ($3::text IS NOT NULL AND m.to_agent = $3) )
          ORDER BY m.created_at ASC
          LIMIT $4",
    )
    .bind(recipient_session)
    .bind(recipient_project_id)
    .bind(recipient_agent)
    .bind(limit)
    .fetch_all(pool)
    .await
}
