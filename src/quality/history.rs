//! Per-pillar GPA history — one row per `quality_report` run, read back as the
//! trend strip. The only persisted artifact (findings are recomputed each run).

use sqlx::PgPool;
use tracing::warn;

use crate::quality::findings::Pillar;
use crate::quality::report::{PillarTrend, QualityReport};

/// Insert one history row. GPA columns are nullable (a pillar can be N/A).
/// Best-effort: a failure (e.g. the v9 table not yet migrated) is logged, not
/// fatal — the report is still returned.
pub async fn insert_history(pool: &PgPool, project_id: i32, report: &QualityReport) {
    let eng = report.pillar(Pillar::Engineering).and_then(|p| p.gpa());
    let arch = report.pillar(Pillar::Architecture).and_then(|p| p.gpa());
    let sec = report.pillar(Pillar::Security).and_then(|p| p.gpa());
    let overall = report.overall_gpa();

    let summary = serde_json::json!({
        "overall_grade": report.overall_grade(),
        "orr_pass": report.orr_pass(),
        "finding_count": report.findings.len(),
    });

    let res = sqlx::query(
        "INSERT INTO quality_report_history
            (project_id, engineering_gpa, architecture_gpa, security_gpa, overall_gpa, raw_summary)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(project_id)
    .bind(eng.map(|v| v as f32))
    .bind(arch.map(|v| v as f32))
    .bind(sec.map(|v| v as f32))
    .bind(overall.map(|v| v as f32))
    .bind(summary)
    .execute(pool)
    .await;
    if let Err(e) = res {
        warn!(error = %e, "quality_report_history insert failed (non-fatal)");
    }
}

/// Recent per-pillar GPAs, oldest → newest, capped at `n` points. Returns an
/// empty vec (not an error) if the table is missing or empty.
pub async fn recent_gpas(pool: &PgPool, project_id: i32, n: usize) -> Vec<PillarTrend> {
    if n == 0 {
        return Vec::new();
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        engineering_gpa: Option<f32>,
        architecture_gpa: Option<f32>,
        security_gpa: Option<f32>,
    }
    let rows: Result<Vec<Row>, _> = sqlx::query_as::<_, Row>(
        "SELECT engineering_gpa, architecture_gpa, security_gpa
         FROM quality_report_history
         WHERE project_id = $1
         ORDER BY computed_at DESC
         LIMIT $2",
    )
    .bind(project_id)
    .bind(n as i64)
    .fetch_all(pool)
    .await;

    let mut rows = match rows {
        Ok(r) => r,
        Err(_) => return Vec::new(), // table not migrated yet, or none
    };
    rows.reverse(); // chronological

    let eng: Vec<f64> = rows
        .iter()
        .filter_map(|r| r.engineering_gpa.map(|v| v as f64))
        .collect();
    let arch: Vec<f64> = rows
        .iter()
        .filter_map(|r| r.architecture_gpa.map(|v| v as f64))
        .collect();
    let sec: Vec<f64> = rows
        .iter()
        .filter_map(|r| r.security_gpa.map(|v| v as f64))
        .collect();

    let mut out = Vec::new();
    if !eng.is_empty() {
        out.push(PillarTrend {
            pillar: Pillar::Engineering,
            gpas: eng,
        });
    }
    if !arch.is_empty() {
        out.push(PillarTrend {
            pillar: Pillar::Architecture,
            gpas: arch,
        });
    }
    if !sec.is_empty() {
        out.push(PillarTrend {
            pillar: Pillar::Security,
            gpas: sec,
        });
    }
    out
}
