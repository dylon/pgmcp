//! F2 — `resolve_symbol_reference_targets` runs all four phases without
//! the `invalid reference to FROM-clause entry for table "sr"` error.
//!
//! Regression for the 155× ERROR records in
//! `~/.local/share/pgmcp/pgmcp.log` originating from
//! `pgmcp::cron::symbol_extraction` across every indexed project. The
//! bug landed in commit `2ba8a4b` ("Shadow-ASR: unified semantic
//! representation across all 12 backends", 2026-05-23) when Phase 2 and
//! Phase 3 of the UPDATE chain started referencing the target alias
//! `sr` inside `JOIN ... ON` predicates — which Postgres rejects since
//! the UPDATE target is only in scope for SET/WHERE/RETURNING. Plan
//! reference: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! F2.
//!
//! Test seeds one project with two files (file1 imports file2; file2
//! defines a symbol `target_func`) and four `symbol_references` rows
//! engineered to exercise each phase:
//!
//!   - Phase 1 (`exact_in_file`): ref in file2 → `target_func`
//!     (defined in file2).
//!   - Phase 2 (`exact_via_import`): ref in file1 → `target_func`
//!     (resolved via the import edge to file2).
//!   - Phase 3 (`bare_name_unique`): ref in file3 → `target_func`
//!     (no import edge; matches by name within the project; `target_func` has
//!     exactly one project-wide definition ⇒ the *unique* bare-name tier). The
//!     confidence-graded split (`bare_name_unique` / `bare_name_ambiguous`)
//!     replaced the legacy single `bare_name_in_project` tier; the
//!     `bare_name_ambiguous` branch has its own test below.
//!   - Phase 4 (`unresolved`): ref in file1 → `nonexistent_symbol`.
//!
//! Asserts no error from the four UPDATE chain calls and that the
//! per-row `resolution_kind` matches the engineered classification.

use pgmcp::db::queries;
use pgmcp_testing::require_test_db;

async fn seed(pool: &sqlx::PgPool) -> (i32, i64, i64, i64) {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name)
         VALUES ('/ws/p14_f2', '/ws/p14_f2/p', 'p14_f2_test')
         ON CONFLICT (path) DO UPDATE SET workspace_path = EXCLUDED.workspace_path
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("project");

    // Three indexed_files. File1 imports file2; file3 is unrelated.
    let mut file_ids: Vec<i64> = Vec::with_capacity(3);
    for (rel, content_hash) in [
        ("src/file1.rs", 1001_i64),
        ("src/file2.rs", 1002_i64),
        ("src/file3.rs", 1003_i64),
    ] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO indexed_files
                 (project_id, path, relative_path, language, size_bytes,
                  content, content_hash, line_count, modified_at)
             VALUES ($1, $2, $3, 'rust', 64, 'unused', $4, 1, NOW())
             ON CONFLICT (path) DO UPDATE SET content_hash = EXCLUDED.content_hash
             RETURNING id",
        )
        .bind(project_id)
        .bind(format!("/ws/p14_f2/p/{rel}"))
        .bind(rel)
        .bind(content_hash)
        .fetch_one(pool)
        .await
        .expect("file");
        file_ids.push(id);
    }
    let file1 = file_ids[0];
    let file2 = file_ids[1];
    let file3 = file_ids[2];

    // file2 defines `target_func`.
    sqlx::query(
        "INSERT INTO file_symbols
             (file_id, name, kind, start_line, end_line, scope_path)
         VALUES ($1, 'target_func', 'function', 1, 1,
                 'crate::file2::target_func')
         ON CONFLICT DO NOTHING",
    )
    .bind(file2)
    .execute(pool)
    .await
    .expect("file_symbols insert");

    // file1 imports file2.
    sqlx::query(
        "INSERT INTO code_graph_edges
             (project_id, source_file_id, target_file_id, edge_type, target_raw)
         VALUES ($1, $2, $3, 'import', 'crate::file2')
         ON CONFLICT DO NOTHING",
    )
    .bind(project_id)
    .bind(file1)
    .bind(file2)
    .execute(pool)
    .await
    .expect("import edge");

    // Four symbol_references engineered for the four phases.
    // (source_file_id, target_raw, ref_kind, source_line)
    let refs: &[(i64, &str, &str, i32)] = &[
        // Phase 1: ref in file2 -> target_func (same file)
        (file2, "target_func", "call", 1),
        // Phase 2: ref in file1 -> target_func (resolves via import edge)
        (file1, "target_func", "call", 1),
        // Phase 3: ref in file3 -> target_func (no import edge; bare name)
        (file3, "target_func", "call", 1),
        // Phase 4: ref in file1 -> nonexistent_symbol (truly unresolved)
        (file1, "nonexistent_symbol", "call", 2),
    ];
    for (src, raw, kind, line) in refs {
        sqlx::query(
            "INSERT INTO symbol_references
                 (source_file_id, target_raw, ref_kind, source_line)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING",
        )
        .bind(src)
        .bind(*raw)
        .bind(*kind)
        .bind(line)
        .execute(pool)
        .await
        .expect("symbol_references insert");
    }

    (project_id, file1, file2, file3)
}

#[tokio::test(flavor = "multi_thread")]
async fn resolution_pass_runs_all_four_phases_without_sr_scope_error() {
    let testdb = require_test_db!();
    let pool = testdb.pool();
    let (project_id, file1, file2, file3) = seed(pool).await;

    // Pre-fix this call returned `Err(sqlx::Error::Database(...))`
    // with the message `invalid reference to FROM-clause entry for
    // table "sr"` from Phase 2's first execution; Phases 3-4 never
    // ran and the cron logged it 155 times across all projects.
    let resolved = queries::resolve_symbol_reference_targets(pool, project_id)
        .await
        .expect("resolution must succeed end-to-end across all four phases");

    // 4 references seeded; all 4 should land in one of the four classes
    // (in_file / via_import / bare_name / unresolved).
    assert_eq!(
        resolved, 4,
        "expected all 4 seeded references to be classified, got {resolved}"
    );

    // Per-row classification check.
    let rows: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT source_file_id, target_raw, resolution_kind
           FROM symbol_references
          WHERE source_file_id IN ($1, $2, $3)
            AND target_raw IN ('target_func', 'nonexistent_symbol')
          ORDER BY source_file_id, target_raw",
    )
    .bind(file1)
    .bind(file2)
    .bind(file3)
    .fetch_all(pool)
    .await
    .expect("fetch rows");

    // Build a small map (src, raw) -> kind for assertion clarity.
    let mut by_key: std::collections::HashMap<(i64, String), String> =
        std::collections::HashMap::new();
    for (src, raw, kind) in rows {
        by_key.insert(
            (src, raw),
            kind.expect("resolution_kind must be populated post-Phase-4"),
        );
    }

    assert_eq!(
        by_key
            .get(&(file2, "target_func".to_string()))
            .map(String::as_str),
        Some("exact_in_file"),
        "Phase 1: file2's self-ref to target_func should be exact_in_file"
    );
    assert_eq!(
        by_key
            .get(&(file1, "target_func".to_string()))
            .map(String::as_str),
        Some("exact_via_import"),
        "Phase 2: file1's ref to target_func should resolve via import edge"
    );
    assert_eq!(
        by_key
            .get(&(file3, "target_func".to_string()))
            .map(String::as_str),
        Some("bare_name_unique"),
        "Phase 3: file3's ref to target_func (no import, lone candidate) \
         should be the unique bare-name tier"
    );
    assert_eq!(
        by_key
            .get(&(file1, "nonexistent_symbol".to_string()))
            .map(String::as_str),
        Some("unresolved"),
        "Phase 4: file1's ref to nonexistent_symbol should be unresolved"
    );
}

/// Phase 3 ambiguity grading: a bare-name reference whose target name has
/// *multiple* project-wide definitions (and no import edge to disambiguate)
/// lands in `bare_name_ambiguous` at low confidence, not `bare_name_unique`.
/// This is the branch the v14 vocabulary fix unblocked — pre-fix, Phase 3
/// violated the stale CHECK and rolled back the whole resolution transaction.
#[tokio::test(flavor = "multi_thread")]
async fn phase3_multiple_candidates_grade_ambiguous() {
    let testdb = require_test_db!();
    let pool = testdb.pool();

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name)
         VALUES ('/ws/p14_ambig', '/ws/p14_ambig/p', 'p14_ambig_test')
         ON CONFLICT (path) DO UPDATE SET workspace_path = EXCLUDED.workspace_path
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("project");

    // Two files each define `dup`; a third file references it with no import
    // edge → two project-wide candidates → ambiguous.
    let mut file_ids: Vec<i64> = Vec::with_capacity(3);
    for (rel, hash) in [
        ("src/a.rs", 2001_i64),
        ("src/b.rs", 2002_i64),
        ("src/c.rs", 2003_i64),
    ] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO indexed_files
                 (project_id, path, relative_path, language, size_bytes,
                  content, content_hash, line_count, modified_at)
             VALUES ($1, $2, $3, 'rust', 64, 'unused', $4, 1, NOW())
             ON CONFLICT (path) DO UPDATE SET content_hash = EXCLUDED.content_hash
             RETURNING id",
        )
        .bind(project_id)
        .bind(format!("/ws/p14_ambig/p/{rel}"))
        .bind(rel)
        .bind(hash)
        .fetch_one(pool)
        .await
        .expect("file");
        file_ids.push(id);
    }
    let (file_a, file_b, file_c) = (file_ids[0], file_ids[1], file_ids[2]);

    for fid in [file_a, file_b] {
        sqlx::query(
            "INSERT INTO file_symbols (file_id, name, kind, start_line, end_line)
             VALUES ($1, 'dup', 'function', 1, 1)
             ON CONFLICT DO NOTHING",
        )
        .bind(fid)
        .execute(pool)
        .await
        .expect("file_symbols insert");
    }

    sqlx::query(
        "INSERT INTO symbol_references
             (source_file_id, target_raw, ref_kind, source_line)
         VALUES ($1, 'dup', 'call', 1)
         ON CONFLICT DO NOTHING",
    )
    .bind(file_c)
    .execute(pool)
    .await
    .expect("symbol_references insert");

    queries::resolve_symbol_reference_targets(pool, project_id)
        .await
        .expect("resolution must succeed end-to-end");

    let row: (Option<String>, Option<f32>) = sqlx::query_as(
        "SELECT resolution_kind, resolution_confidence
           FROM symbol_references
          WHERE source_file_id = $1 AND target_raw = 'dup'",
    )
    .bind(file_c)
    .fetch_one(pool)
    .await
    .expect("fetch resolution");

    assert_eq!(
        row.0.as_deref(),
        Some("bare_name_ambiguous"),
        "two same-name candidates with no import edge ⇒ ambiguous tier"
    );
    assert_eq!(
        row.1,
        Some(0.3_f32),
        "ambiguous tier carries the low (0.3) confidence"
    );
}

/// Resolution is idempotent and cheap to re-run: once every reference is
/// classified, a second pass resolves nothing — the EXISTS backlog guard
/// short-circuits before the phase UPDATEs. This is what makes it safe for the
/// symbol-extraction cron to call resolution on a "no new files" cycle purely to
/// DRAIN a stranded backlog (references the pre-fix Phase-3 timeout left NULL,
/// then a later no-files run skipped past by advancing the watermark).
#[tokio::test(flavor = "multi_thread")]
async fn resolution_is_idempotent_second_pass_drains_nothing() {
    let testdb = require_test_db!();
    let pool = testdb.pool();
    let (project_id, _f1, _f2, _f3) = seed(pool).await;

    let first = queries::resolve_symbol_reference_targets(pool, project_id)
        .await
        .expect("first resolution pass");
    assert_eq!(first, 4, "first pass classifies all 4 seeded references");

    let second = queries::resolve_symbol_reference_targets(pool, project_id)
        .await
        .expect("second resolution pass must succeed (no-op)");
    assert_eq!(
        second, 0,
        "fully-resolved project: second pass drains nothing (backlog guard short-circuits)"
    );
}
