//! Per-pillar GPA history — one row per `quality_report` run, read back as the
//! trend strip. The only persisted artifact (findings are recomputed each run).

use chrono::{DateTime, Utc};
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

/// One timestamped quality-history sample, for the trend/forecast tools and the
/// digest. Unlike [`recent_gpas`] (which collapses to per-pillar `PillarTrend`
/// strips), this keeps the `computed_at` axis so a slope/forecast can be fit.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GpaPoint {
    pub at: DateTime<Utc>,
    pub engineering: Option<f32>,
    pub architecture: Option<f32>,
    pub security: Option<f32>,
    pub overall: Option<f32>,
}

/// All quality-history samples for a project within the last `days`, oldest →
/// newest. Empty (not an error) if the table is missing/empty — same
/// best-effort posture as [`recent_gpas`].
pub async fn gpa_series_since(pool: &PgPool, project_id: i32, days: i64) -> Vec<GpaPoint> {
    let days = days.clamp(1, 3650);
    #[derive(sqlx::FromRow)]
    struct Row {
        computed_at: DateTime<Utc>,
        engineering_gpa: Option<f32>,
        architecture_gpa: Option<f32>,
        security_gpa: Option<f32>,
        overall_gpa: Option<f32>,
    }
    let rows: Result<Vec<Row>, _> = sqlx::query_as::<_, Row>(
        "SELECT computed_at, engineering_gpa, architecture_gpa, security_gpa, overall_gpa
         FROM quality_report_history
         WHERE project_id = $1 AND computed_at >= NOW() - make_interval(days => $2::int)
         ORDER BY computed_at ASC",
    )
    .bind(project_id)
    .bind(days)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => rows
            .into_iter()
            .map(|r| GpaPoint {
                at: r.computed_at,
                engineering: r.engineering_gpa,
                architecture: r.architecture_gpa,
                security: r.security_gpa,
                overall: r.overall_gpa,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Fit a per-day OLS slope of the `overall` GPA over a [`GpaPoint`] series
/// (days measured from the first sample). `None` if fewer than two overall-GPA
/// points exist. Shared by the forecast tool and the digest's "GPA trending …"
/// line so they agree on the number.
pub fn overall_gpa_slope_per_day(series: &[GpaPoint]) -> Option<f64> {
    let t0 = series.first()?.at;
    let pts: Vec<(f64, f64)> = series
        .iter()
        .filter_map(|p| {
            p.overall.map(|g| {
                let days = (p.at - t0).num_seconds() as f64 / 86_400.0;
                (days, g as f64)
            })
        })
        .collect();
    crate::quality::forecast::ols_slope(&pts)
}
