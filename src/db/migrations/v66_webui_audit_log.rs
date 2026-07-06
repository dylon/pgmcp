//! Migration step 66: `webui_audit_log` — append-only audit trail for the
//! token-gated webui operator writes (mandate CRUD/promote, work-item
//! transitions, control halt/resume). Written in the same transaction as each
//! mutation. The `action` CHECK is built from `AuditAction::sql_in_list()`
//! (ADR-003) so the enum and the constraint cannot drift.

use sqlx::PgPool;

pub(super) const WEBUI_AUDIT_LOG: i32 = 66;
pub(super) const WEBUI_AUDIT_LOG_NAME: &str = "webui_audit_log";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS webui_audit_log (
            id BIGSERIAL PRIMARY KEY,
            at TIMESTAMPTZ NOT NULL DEFAULT now(),
            actor TEXT NOT NULL,
            action TEXT NOT NULL,
            target_kind TEXT,
            target_id TEXT,
            request_ip TEXT,
            before JSONB,
            after JSONB,
            reason TEXT,
            ok BOOLEAN NOT NULL DEFAULT true,
            error TEXT
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS webui_audit_log_at_idx ON webui_audit_log (at DESC)")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS webui_audit_log_action_idx ON webui_audit_log (action, at DESC)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS webui_audit_log_target_idx \
         ON webui_audit_log (target_kind, target_id)",
    )
    .execute(pool)
    .await?;

    let predicate = format!(
        "action IN ({})",
        crate::api::audit::AuditAction::sql_in_list()
    );
    super::v4_work_items::install_check(
        pool,
        "webui_audit_log",
        "webui_audit_log_action_check",
        &predicate,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(WEBUI_AUDIT_LOG, 66);
        assert_eq!(WEBUI_AUDIT_LOG_NAME, "webui_audit_log");
    }
}
