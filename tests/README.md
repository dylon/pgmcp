# pgmcp — test suite layout & local setup

## Where the tests live

| Location | What runs there |
|---|---|
| `src/**/mod tests` | Tier A — pure unit and property tests. No external dependencies. Always run. |
| `pgmcp-testing/src/**/mod tests` | Harness unit tests (URL parsing, config discovery). Always run. |
| `pgmcp-testing/tests/*.rs` | Tier B — mock-backed tool tests (use `MockDbClient`). Always run. Tier C — real-Postgres integration / E2E tests (use `TestTransaction` or `TestDatabase`). Self-skip when no test DB is configured. |
| `tests/gpu_fallback_smoke.rs` | GPU-specific smoke. Runs via verify.sh gate 6. |
| `examples/gpu_smoke.rs` | GPU smoke scenarios. Runs via `cargo smoke` (verify.sh gate 7). |

## No Docker

The test suite deliberately does **not** depend on Docker. Real-Postgres tests
connect to the user's existing local install via the harness in
`pgmcp-testing/src/db_harness.rs`. Tests that require a DB auto-skip when the
environment is not configured, so `./scripts/verify.sh` stays green for
casual contributors.

## Enabling real-Postgres tests

You need:

1. **PostgreSQL ≥ 17** running locally.
2. **pgvector ≥ 0.7** installed cluster-wide. `CREATE EXTENSION vector` must
   succeed in a fresh database.
3. **A role with `CREATEDB` privilege.** Tests create and drop their own
   databases. If pgvector isn't a `TRUSTED` extension in your install, the
   role also needs `SUPERUSER`.

### Option A — environment variable

```bash
export PGMCP_TEST_DATABASE_URL="postgres://you:password@localhost:5432/anydb"
```

The `/anydb` path component is ignored — only the base URL matters. The
harness creates its own `pgmcp_test_*` databases.

### Option B — test config file

Drop a `~/.config/pgmcp/test-config.toml`:

```toml
[database]
host = "localhost"
port = 5432
name = "anydb"
user = "you"
password = "password"       # or omit and set PGMCP_DB_PASSWORD
```

Option A wins if both are set. If neither option is configured, the harness
falls back to `~/.config/pgmcp/config.toml` as the connection authority, but it
still creates separate `pgmcp_test_*` databases and never runs tests against the
configured pgmcp database's tables.

## How the harness works

### `TestTransaction` (default; fastest)

Single-connection tests use a shared per-process "template" database created
once at startup. Each test opens a SQL `BEGIN` on the shared pool; the
transaction `ROLLBACK`s on `Drop`. Nothing is ever committed → no cleanup
needed, crash-safe by construction. Most real-DB tests use this.

### `TestDatabase` (per-test isolation)

Tests that need multi-connection visibility (indexer, cron workers, CLI/daemon
subprocess E2E) use `CREATE DATABASE pgmcp_test_<uuid7> WITH TEMPLATE …`
(≈ 20–50 ms per test vs. ≈ 500 ms for a fresh migration run). `Drop` fires a
detached cleanup thread that `DROP DATABASE`s the test DB. Databases leaked
by `SIGKILL` / `OOM` are swept on the next test-binary run — the sweep runs
once per process before any database is created.

### Subprocess harness

`cli_harness::PgmcpProcess` spawns `target/release/pgmcp serve` or `daemon`
against a `TestDatabase`. Set `PGMCP_TEST_BIN` to override the binary path,
or rely on the default `target/{release,debug}/pgmcp` lookup.

## Running the tests

```bash
# Everything, including real-DB tests if configured:
./scripts/verify.sh

# Just the pgmcp-testing crate's tests:
cargo test --release -p pgmcp-testing

# A single real-DB test:
cargo test --release -p pgmcp-testing --test db_sql_surface_integration \
    project_upsert_increments_count_and_returns_id
```

When `PGMCP_TEST_DATABASE_URL` is unset, real-DB tests print
`SKIPPED: no test DB configured (…)` and exit 0.
