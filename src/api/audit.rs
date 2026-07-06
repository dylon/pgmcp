//! Audit trail for token-gated webui operator writes.
//!
//! Every operator mutation (mandate CRUD/promote, work-item transitions,
//! control halt/resume) writes exactly one `webui_audit_log` row IN THE SAME
//! TRANSACTION as the domain change (and its realtime event), so the three
//! commit atomically or not at all. `AuditAction` is a closed vocabulary
//! (ADR-003 idiom: `as_str` / `sql_in_list()` + a golden test pinning the set;
//! the v66 migration builds the CHECK from `sql_in_list()`).

use serde_json::Value;
use sqlx::{Postgres, Transaction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditAction {
    MandatePromote,
    MandateCreate,
    MandateUpdate,
    MandateDelete,
    MandateRetire,
    WorkItemTransition,
    WorkItemUpdate,
    WorkItemTriage,
    WorkItemConfirm,
    ExperimentUpdate,
    ControlHalt,
    ControlResume,
}

impl AuditAction {
    pub const ALL: &'static [AuditAction] = &[
        AuditAction::MandatePromote,
        AuditAction::MandateCreate,
        AuditAction::MandateUpdate,
        AuditAction::MandateDelete,
        AuditAction::MandateRetire,
        AuditAction::WorkItemTransition,
        AuditAction::WorkItemUpdate,
        AuditAction::WorkItemTriage,
        AuditAction::WorkItemConfirm,
        AuditAction::ExperimentUpdate,
        AuditAction::ControlHalt,
        AuditAction::ControlResume,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AuditAction::MandatePromote => "mandate_promote",
            AuditAction::MandateCreate => "mandate_create",
            AuditAction::MandateUpdate => "mandate_update",
            AuditAction::MandateDelete => "mandate_delete",
            AuditAction::MandateRetire => "mandate_retire",
            AuditAction::WorkItemTransition => "work_item_transition",
            AuditAction::WorkItemUpdate => "work_item_update",
            AuditAction::WorkItemTriage => "work_item_triage",
            AuditAction::WorkItemConfirm => "work_item_confirm",
            AuditAction::ExperimentUpdate => "experiment_update",
            AuditAction::ControlHalt => "control_halt",
            AuditAction::ControlResume => "control_resume",
        }
    }

    /// The `'a', 'b', ...` fragment for a `CHECK (action IN (...))` constraint.
    pub fn sql_in_list() -> String {
        Self::ALL
            .iter()
            .map(|a| format!("'{}'", a.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A single audit entry. `before`/`after` capture the pre/post JSON state of the
/// mutated entity where meaningful.
pub struct AuditEntry {
    pub actor: String,
    pub action: AuditAction,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    pub request_ip: Option<String>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub reason: Option<String>,
    pub ok: bool,
    pub error: Option<String>,
}

/// Append one audit row to the caller's open transaction. JSON columns bind as
/// text + `::jsonb` cast (the root sqlx build has no `json` feature).
pub async fn audit_write_tx(
    tx: &mut Transaction<'_, Postgres>,
    entry: &AuditEntry,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO webui_audit_log \
         (actor, action, target_kind, target_id, request_ip, before, after, reason, ok, error) \
         VALUES ($1, $2, $3, $4, $5, $6::jsonb, $7::jsonb, $8, $9, $10)",
    )
    .bind(&entry.actor)
    .bind(entry.action.as_str())
    .bind(&entry.target_kind)
    .bind(&entry.target_id)
    .bind(&entry.request_ip)
    .bind(entry.before.as_ref().map(|v| v.to_string()))
    .bind(entry.after.as_ref().map(|v| v.to_string()))
    .bind(&entry.reason)
    .bind(entry.ok)
    .bind(&entry.error)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_vocabulary_is_closed_and_pinned() {
        assert_eq!(AuditAction::ALL.len(), 12);
        let list = AuditAction::sql_in_list();
        for action in AuditAction::ALL {
            assert!(
                list.contains(action.as_str()),
                "missing {}",
                action.as_str()
            );
        }
        // round-trip uniqueness
        let mut seen = std::collections::HashSet::new();
        for a in AuditAction::ALL {
            assert!(seen.insert(a.as_str()), "duplicate {}", a.as_str());
        }
    }
}
