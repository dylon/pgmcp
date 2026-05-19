//! Tier 1a verification — confirm the production pool actually applies
//! the per-session timeouts configured on `DatabaseConfig`.
//!
//! Two checks:
//! 1. `statement_timeout` causes a long `SELECT pg_sleep(...)` to fail
//!    with SQLSTATE `57014` (query_canceled).
//! 2. `SET LOCAL statement_timeout` inside a transaction overrides the
//!    daemon-wide ceiling, so legitimate long analytic queries succeed.

use pgmcp::config::DatabaseConfig;
use pgmcp::db::pool;
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn session_statement_timeout_cancels_long_query() {
    let db = require_test_db!();
    let mut cfg = database_config_from_url(&db.connection_url());
    cfg.statement_timeout_ms = 500;
    cfg.test_before_acquire = false;
    cfg.max_connections = 1;

    let pool = pool::create_pool(&cfg)
        .await
        .expect("create_pool should succeed with short statement_timeout");

    let result = sqlx::query("SELECT pg_sleep(5)").execute(&pool).await;
    let err = result.expect_err("pg_sleep(5) must fail under a 500ms statement_timeout");
    let sqlstate = match &err {
        sqlx::Error::Database(db_err) => db_err.code().map(|s| s.into_owned()),
        _ => None,
    };
    assert_eq!(
        sqlstate.as_deref(),
        Some("57014"),
        "expected SQLSTATE 57014 (query_canceled), got error: {err}",
    );
}

#[tokio::test]
async fn set_local_statement_timeout_overrides_default() {
    let db = require_test_db!();
    let mut cfg = database_config_from_url(&db.connection_url());
    cfg.statement_timeout_ms = 500;
    cfg.test_before_acquire = false;
    cfg.max_connections = 1;

    let pool = pool::create_pool(&cfg)
        .await
        .expect("create_pool should succeed with short statement_timeout");

    let mut tx = pool.begin().await.expect("BEGIN");
    sqlx::query("SET LOCAL statement_timeout = '10s'")
        .execute(&mut *tx)
        .await
        .expect("SET LOCAL must succeed inside the transaction");
    sqlx::query("SELECT pg_sleep(1)")
        .execute(&mut *tx)
        .await
        .expect("pg_sleep(1) under SET LOCAL 10s must succeed despite daemon-wide 500ms cap");
    tx.commit().await.expect("COMMIT");
}

fn database_config_from_url(url: &str) -> DatabaseConfig {
    let without_scheme = url
        .strip_prefix("postgres://")
        .expect("test harness uses postgres:// URLs");
    let (authority, name) = without_scheme
        .rsplit_once('/')
        .expect("test harness URL includes database name");
    let (userinfo, host_port) = authority
        .rsplit_once('@')
        .map_or((None, authority), |(user, host)| (Some(user), host));
    let (user, password) = match userinfo.and_then(|u| u.split_once(':')) {
        Some((user, password)) => (user.to_string(), Some(password.to_string())),
        None => (
            userinfo
                .map(str::to_string)
                .unwrap_or_else(|| DatabaseConfig::default().user),
            None,
        ),
    };
    let (host, port) = host_port
        .rsplit_once(':')
        .map_or((host_port, 5432), |(h, p)| {
            (h, p.parse::<u16>().expect("test DB port is numeric"))
        });
    DatabaseConfig {
        host: host.to_string(),
        port,
        name: name.to_string(),
        user,
        password,
        ..DatabaseConfig::default()
    }
}
