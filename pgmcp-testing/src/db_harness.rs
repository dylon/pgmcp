//! Real-Postgres test harness — no Docker.
//!
//! Two complementary patterns:
//!
//! * [`TestTransaction`] (Pattern A) — opens a transaction against a shared
//!   per-process template database. Drops ROLLBACK. Zero cleanup cost;
//!   crash-safe by construction (nothing is ever committed). Best for
//!   single-connection tests.
//!
//! * [`TestDatabase`] (Pattern B) — creates a fresh `pgmcp_test_<uuid7>`
//!   database via `CREATE DATABASE … WITH TEMPLATE <shared>`; `Drop` fires a
//!   detached cleanup thread that drops the database. Best for
//!   multi-connection tests (indexer, cron, subprocess E2E). Any database
//!   that leaks due to SIGKILL / OOM is swept on the next test-binary run.
//!
//! Both entry points return `TestDbUnavailable` if the environment is not
//! set up, so a test can skip cleanly via the [`crate::require_test_txn`]
//! and [`crate::require_test_db`] macros.
//!
//! ## Required local setup
//!
//! * PostgreSQL ≥ 17 with the `vector` extension (pgvector ≥ 0.7) installed
//!   cluster-wide. `CREATE EXTENSION vector` in a fresh DB must succeed.
//! * A role with `CREATEDB` privilege (and, if pgvector isn't already a
//!   trusted extension, `SUPERUSER`).
//! * One of:
//!   * `PGMCP_TEST_DATABASE_URL` set to `postgres://user:pass@host:port/dbname`
//!     (the `/dbname` path component is ignored — only the base URL matters).
//!   * `~/.config/pgmcp/test-config.toml` containing a `[database]` section
//!     in the pgmcp config format.
//!   * `~/.config/pgmcp/config.toml`; used only when no test-specific config
//!     exists, and still only as the connection authority for `pgmcp_test_*`
//!     databases.
//!
//! See `tests/README.md` in the repo root for a walk-through.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sqlx::{Connection, PgConnection, PgPool, Postgres, Transaction};
use tokio::sync::OnceCell;
use uuid::Uuid;

use pgmcp::config::{Config, DatabaseConfig, VectorConfig};

/// Reason a real-DB test couldn't be set up. Surfaced so `require_test_*!`
/// can print a short human-readable "SKIPPED" line.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TestDbUnavailable {
    /// No test-specific or default pgmcp database config was present.
    #[error(
        "no test DB configured (set PGMCP_TEST_DATABASE_URL, ~/.config/pgmcp/test-config.toml, or ~/.config/pgmcp/config.toml)"
    )]
    NotConfigured,
    /// Env var or config file was present but unparseable.
    #[error("bad test DB config: {0}")]
    BadConfig(String),
    /// We had a config but couldn't reach Postgres (wrong URL, server down,
    /// auth failed, etc).
    #[error("connect failed: {0}")]
    ConnectFailed(String),
    /// `CREATE DATABASE` or `CREATE EXTENSION vector` failed.
    #[error("setup failed: {0}")]
    SetupFailed(String),
    /// `run_migrations` failed.
    #[error("migrations failed: {0}")]
    MigrationsFailed(String),
}

/// Parsed test-DB location — base URL plus a per-process shared template
/// database name. Cheap to copy; constructed lazily the first time a
/// harness entry point is called.
#[derive(Debug, Clone)]
struct TestDbConfig {
    /// Everything up to (but not including) the trailing `/dbname`, e.g.
    /// `postgres://user:pass@localhost:5432`.
    base_url: String,
    /// Shared template database for this process. Created once, dropped at
    /// process exit (best-effort via the orphan sweep).
    template_db: String,
}

impl TestDbConfig {
    /// Discover the test-DB location from the environment.
    fn discover() -> Result<Self, TestDbUnavailable> {
        if let Ok(url) = std::env::var("PGMCP_TEST_DATABASE_URL") {
            return Self::from_url(&url);
        }
        if let Some(path) = test_config_path()
            && path.exists()
        {
            return Self::from_config_path(&path);
        }
        let default_path = Config::default_config_path();
        if default_path.exists() {
            return Self::from_config_path(&default_path);
        }
        Err(TestDbUnavailable::NotConfigured)
    }

    fn from_config_path(path: &Path) -> Result<Self, TestDbUnavailable> {
        let cfg = Config::load(Some(path))
            .map_err(|e| TestDbUnavailable::BadConfig(format!("load {}: {}", path.display(), e)))?;
        Self::from_database_config(&cfg.database)
    }

    fn from_url(url: &str) -> Result<Self, TestDbUnavailable> {
        // Find the last '/' that delimits the database name from the host.
        // Reject URLs without a scheme or path — we need to know where the
        // base ends.
        let scheme_end = url.find("://").ok_or_else(|| {
            TestDbUnavailable::BadConfig(format!("missing scheme in URL: {}", url))
        })?;
        let after_scheme = &url[scheme_end + 3..];
        let Some(slash_rel) = after_scheme.find('/') else {
            return Err(TestDbUnavailable::BadConfig(format!(
                "URL has no database path: {}",
                url
            )));
        };
        let base_url = url[..scheme_end + 3 + slash_rel].to_string();
        Ok(Self {
            base_url,
            template_db: template_db_name(),
        })
    }

    fn from_database_config(db: &DatabaseConfig) -> Result<Self, TestDbUnavailable> {
        let full = db.connection_url();
        Self::from_url(&full)
    }

    /// URL for the PostgreSQL maintenance database (always `postgres`,
    /// which exists on every cluster by default). We connect here to
    /// issue `CREATE DATABASE` / `DROP DATABASE` / orphan sweep — all
    /// cluster-level operations that must not run against a user DB.
    fn maintenance_url(&self) -> String {
        format!("{}/postgres", self.base_url)
    }

    fn url_for(&self, db_name: &str) -> String {
        format!("{}/{}", self.base_url, db_name)
    }
}

fn test_config_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".config/pgmcp/test-config.toml"))
}

fn template_db_name() -> String {
    format!("pgmcp_test_shared_{}", std::process::id())
}

fn per_test_db_name() -> String {
    // now_v7 encodes a timestamp + random bytes → collision-free across
    // parallel tests within a process and across distinct processes on the
    // same Postgres cluster.
    format!("pgmcp_test_{}", Uuid::now_v7().simple())
}

// ---------------------------------------------------------------------------
// Process-wide state: config + shared pool + orphan-sweep sentinel.
// ---------------------------------------------------------------------------

static TEST_DB_CONFIG: OnceLock<Result<TestDbConfig, TestDbUnavailable>> = OnceLock::new();
static SHARED_POOL: OnceCell<PgPool> = OnceCell::const_new();
static ORPHAN_SWEEP: OnceCell<()> = OnceCell::const_new();

fn test_db_config() -> Result<&'static TestDbConfig, TestDbUnavailable> {
    let slot = TEST_DB_CONFIG.get_or_init(TestDbConfig::discover);
    slot.as_ref().map_err(|e| e.clone())
}

/// Sweep orphan `pgmcp_test_*` databases left behind by prior crashed
/// runs. Runs exactly once per process, before any test database is
/// created. Best-effort — failures are logged to stderr and ignored.
async fn run_orphan_sweep() -> Result<(), TestDbUnavailable> {
    let cfg = test_db_config()?.clone();
    ORPHAN_SWEEP
        .get_or_init(move || async move {
            let url = cfg.maintenance_url();
            let mut conn = match PgConnection::connect(&url).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("orphan sweep: connect failed: {}", e);
                    return;
                }
            };
            let rows: Vec<(String,)> = match sqlx::query_as(
                "SELECT datname FROM pg_database WHERE datname LIKE 'pgmcp_test_%'",
            )
            .fetch_all(&mut conn)
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("orphan sweep: list failed: {}", e);
                    return;
                }
            };
            for (name,) in rows {
                // Don't drop our own template — it hasn't been created yet
                // when the sweep runs (we create it after the sweep
                // completes), but be defensive in case of stale state
                // from an earlier process reusing our pid after wrap.
                if name == cfg.template_db {
                    continue;
                }
                drop_database_force(&mut conn, &name).await;
            }
        })
        .await;
    Ok(())
}

/// Terminate all connections to `db_name` then `DROP DATABASE IF EXISTS`.
/// Best-effort — errors are swallowed. The caller is expected to hold a
/// connection to the *maintenance* DB, not to `db_name` itself.
async fn drop_database_force(conn: &mut PgConnection, db_name: &str) {
    let escaped = db_name.replace('\'', "''");
    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = '{}' AND pid <> pg_backend_pid()",
        escaped,
    )))
    .execute(&mut *conn)
    .await;
    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS \"{}\"",
        db_name
    )))
    .execute(&mut *conn)
    .await;
}

/// Get-or-create the per-process shared pool. First caller bootstraps:
/// orphan-sweep → CREATE DATABASE for the template → run migrations →
/// stash the pool. Subsequent callers reuse the same pool.
async fn shared_pool() -> Result<&'static PgPool, TestDbUnavailable> {
    run_orphan_sweep().await?;
    SHARED_POOL
        .get_or_try_init(|| async {
            let cfg = test_db_config()?;

            // Connect to the maintenance DB and create our template. If a
            // stale template of the same name exists (shouldn't happen
            // after the sweep but defend anyway), drop it first.
            let mut maint = PgConnection::connect(&cfg.maintenance_url())
                .await
                .map_err(|e| TestDbUnavailable::ConnectFailed(e.to_string()))?;
            drop_database_force(&mut maint, &cfg.template_db).await;
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE DATABASE \"{}\"",
                cfg.template_db
            )))
            .execute(&mut maint)
            .await
            .map_err(|e| {
                TestDbUnavailable::SetupFailed(format!(
                    "CREATE DATABASE {}: {}",
                    cfg.template_db, e
                ))
            })?;
            drop(maint);

            // Connect to the new template DB and run migrations.
            let template_url = cfg.url_for(&cfg.template_db);
            let pool = PgPool::connect(&template_url)
                .await
                .map_err(|e| TestDbUnavailable::ConnectFailed(e.to_string()))?;
            let vector_config = VectorConfig::default();
            pgmcp::db::migrations::run_migrations(&pool, &vector_config, false)
                .await
                .map_err(|e| TestDbUnavailable::MigrationsFailed(e.to_string()))?;
            Ok(pool)
        })
        .await
}

// ---------------------------------------------------------------------------
// Pattern A — transaction rollback against the shared template DB.
// ---------------------------------------------------------------------------

/// A transaction opened on the shared template database. The transaction
/// auto-rolls-back on `Drop`, so no commit = no cleanup = crash-safe.
///
/// Use [`TestTransaction::conn`] to get a `&mut PgConnection` that can be
/// handed to `sqlx::query(...).execute(txn.conn())` and friends.
pub struct TestTransaction {
    /// `None` only during the move out in `Drop`; callers never observe it.
    inner: Option<Transaction<'static, Postgres>>,
}

impl TestTransaction {
    pub async fn begin() -> Result<Self, TestDbUnavailable> {
        let pool = shared_pool().await?;
        let txn = pool
            .begin()
            .await
            .map_err(|e| TestDbUnavailable::SetupFailed(format!("BEGIN: {}", e)))?;
        Ok(Self { inner: Some(txn) })
    }

    /// The connection bound to this transaction. All queries must use it —
    /// going through the pool directly would open a separate connection
    /// that doesn't see the transaction's writes.
    pub fn conn(&mut self) -> &mut PgConnection {
        self.inner
            .as_mut()
            .expect("transaction still alive")
            .as_mut()
    }
}

// sqlx::Transaction's own Drop impl schedules a ROLLBACK when the inner
// Option is dropped — nothing for us to do here.

// ---------------------------------------------------------------------------
// Pattern B — per-test database (CREATE + DROP).
// ---------------------------------------------------------------------------

/// A per-test PostgreSQL database created from the shared template.
/// `Drop` best-effort drops the DB on a detached runtime thread; anything
/// that leaks is swept on the next test-binary run.
pub struct TestDatabase {
    db_name: String,
    /// Taken out in `Drop` so the cleanup thread owns it.
    pool: Option<PgPool>,
    /// URL to the maintenance (`postgres`) DB — needed in `Drop` to issue
    /// the `DROP DATABASE` once our own pool is closed.
    maintenance_url: String,
}

impl TestDatabase {
    pub async fn new() -> Result<Self, TestDbUnavailable> {
        // Ensures the shared template exists and the orphan sweep ran.
        let _ = shared_pool().await?;
        let cfg = test_db_config()?.clone();

        let db_name = per_test_db_name();
        let mut maint = PgConnection::connect(&cfg.maintenance_url())
            .await
            .map_err(|e| TestDbUnavailable::ConnectFailed(e.to_string()))?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE \"{}\" WITH TEMPLATE \"{}\"",
            db_name, cfg.template_db,
        )))
        .execute(&mut maint)
        .await
        .map_err(|e| {
            TestDbUnavailable::SetupFailed(format!("CREATE DATABASE {}: {}", db_name, e))
        })?;
        drop(maint);

        let url = cfg.url_for(&db_name);
        let pool = PgPool::connect(&url)
            .await
            .map_err(|e| TestDbUnavailable::ConnectFailed(e.to_string()))?;

        Ok(Self {
            db_name,
            pool: Some(pool),
            maintenance_url: cfg.maintenance_url(),
        })
    }

    /// Access the test's connection pool. Tests can hand out connections
    /// or `Arc<dyn pgmcp::db::DbClient>` wrappers as needed.
    pub fn pool(&self) -> &PgPool {
        self.pool.as_ref().expect("pool taken only in Drop")
    }

    pub fn db_name(&self) -> &str {
        &self.db_name
    }

    /// Connection URL for spawning subprocesses (daemon, CLI) that open
    /// their own pools.
    pub fn connection_url(&self) -> String {
        let scheme_end = self
            .maintenance_url
            .rfind("/postgres")
            .expect("maintenance_url built with /postgres suffix");
        format!("{}/{}", &self.maintenance_url[..scheme_end], self.db_name)
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let pool = self.pool.take();
        let db_name = std::mem::take(&mut self.db_name);
        let maint_url = self.maintenance_url.clone();
        // Detached cleanup — a fresh current-thread runtime on a fresh OS
        // thread. The handle is dropped; anything that doesn't complete
        // before process exit is swept on the next run.
        std::thread::Builder::new()
            .name(format!("pgmcp-testdb-drop-{}", db_name))
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(async move {
                    if let Some(pool) = pool {
                        pool.close().await;
                    }
                    let Ok(mut maint) = PgConnection::connect(&maint_url).await else {
                        return;
                    };
                    drop_database_force(&mut maint, &db_name).await;
                });
            })
            .ok();
    }
}

// ---------------------------------------------------------------------------
// Unit tests — run against the local Postgres if available, skip otherwise.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_url_splits_base_and_dbname() {
        let cfg = TestDbConfig::from_url("postgres://u:p@localhost:5432/mydb").expect("parses");
        assert_eq!(cfg.base_url, "postgres://u:p@localhost:5432");
    }

    #[test]
    fn from_url_missing_database_path_is_error() {
        let err = TestDbConfig::from_url("postgres://localhost:5432").expect_err("no path");
        assert!(matches!(err, TestDbUnavailable::BadConfig(_)));
    }

    #[test]
    fn from_url_missing_scheme_is_error() {
        let err = TestDbConfig::from_url("localhost/db").expect_err("no scheme");
        assert!(matches!(err, TestDbUnavailable::BadConfig(_)));
    }

    #[test]
    fn from_config_path_uses_database_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[database]
host = "dbhost"
port = 15432
name = "prod"
user = "tester"
password = "pw"
"#,
        )
        .expect("write config");

        let cfg = TestDbConfig::from_config_path(&path).expect("parses");
        assert_eq!(cfg.base_url, "postgres://tester:pw@dbhost:15432");
    }

    #[test]
    fn maintenance_url_uses_postgres_db() {
        let cfg = TestDbConfig::from_url("postgres://u@h:5432/anydb").expect("parses");
        assert_eq!(cfg.maintenance_url(), "postgres://u@h:5432/postgres");
    }

    #[test]
    fn url_for_appends_name() {
        let cfg = TestDbConfig::from_url("postgres://u@h:5432/whatever").expect("parses");
        assert_eq!(cfg.url_for("foo"), "postgres://u@h:5432/foo");
    }

    #[test]
    fn template_db_name_contains_pid() {
        let name = template_db_name();
        let pid = std::process::id();
        assert!(name.contains(&pid.to_string()));
        assert!(name.starts_with("pgmcp_test_shared_"));
    }

    #[test]
    fn per_test_db_name_has_uuid_suffix() {
        let a = per_test_db_name();
        let b = per_test_db_name();
        assert!(a.starts_with("pgmcp_test_"));
        assert!(b.starts_with("pgmcp_test_"));
        assert_ne!(a, b, "uuid should make two calls distinct");
    }
}
