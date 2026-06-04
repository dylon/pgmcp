//! Phase-3 message delivery: render undelivered mailbox messages into a compact
//! markdown block for the model-visible context channels — UserPromptSubmit
//! `additional_context`, SessionStart `pgmcp context`, and PostToolUse
//! `additionalContext` — and record the per-recipient delivery receipts so a
//! message is not re-surfaced. The reliable floor remains the `a2a_inbox` pull.
//!
//! `recipient_session` is the receipt dedup key (the Claude-Code hook
//! `session_id` on these channels); `recipient_project_id` / `recipient_agent`
//! are the addressing dimensions that match project- and agent-broadcast
//! messages (session-addressed messages, keyed by `mcp_session_id`, are
//! delivered via the `a2a_inbox` pull instead — see the mailbox design notes).

use sqlx::PgPool;

use crate::a2a::mailbox_store::{self, Mark};

/// UTF-8-safe truncation to at most `max` chars (appends an ellipsis).
fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Render undelivered messages for a recipient and mark them delivered on
/// `channel`. Returns the markdown block, or `None` when there is nothing to
/// deliver.
pub async fn render_and_deliver(
    pool: &PgPool,
    recipient_session: Option<&str>,
    recipient_project_id: Option<i32>,
    recipient_agent: Option<&str>,
    channel: &str,
    max: i64,
) -> Option<String> {
    let pending = mailbox_store::pending_undelivered(
        pool,
        recipient_session,
        recipient_project_id,
        recipient_agent,
        max,
    )
    .await
    .ok()?;
    if pending.is_empty() {
        return None;
    }

    let mut block = String::from("## 📨 Agent messages\n\n");
    for m in &pending {
        let subj = m
            .subject
            .as_deref()
            .map(|s| format!(" — {s}"))
            .unwrap_or_default();
        block.push_str(&format!(
            "- **{}** ({}){}: {} _(reply via `a2a_reply_message {{message_id: {}}}`)_\n",
            m.from_agent,
            m.kind,
            subj,
            truncate(&m.body, 240),
            m.id,
        ));
        // Receipt keyed on recipient_session → this instance won't see it again
        // on this channel; other instances/projects still get their own copy.
        let _ = mailbox_store::record_receipt(
            pool,
            m.id,
            recipient_session,
            recipient_agent,
            Some(channel),
            Mark::Delivered,
        )
        .await;
    }
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn truncate_is_utf8_safe() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello…");
        // Multibyte: must not split a char.
        let s = "héllo wörld"; // é, ö are 2 bytes
        let t = truncate(s, 4);
        assert!(t.ends_with('…'));
        assert!(s.starts_with(t.trim_end_matches('…')));
    }
}
