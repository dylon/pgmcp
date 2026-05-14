//! Scanner-level integration test for document indexing.
//!
//! Builds a fixture tree on disk, runs the scanner against it, and
//! asserts that synthetic-project discovery, source-form dedup, and
//! extension filtering all behave correctly. Deliberately scoped below
//! the full DB-backed pipeline (which lives in
//! `tests/indexer_pipeline_e2e.rs`) so this test runs reliably without
//! Postgres or any external CLI tools.
//!
//! Synthetic-project paths are injected explicitly via `SyntheticRoots`
//! — these tests do NOT mutate `$HOME`. That used to be necessary
//! because `scan_workspaces` resolved `~/Papers/` and friends via
//! `dirs::home_dir()`; the resulting `unsafe { set_var("HOME", …) }`
//! race produced an indefinite deadlock when concurrent tests
//! clobbered each other's overrides.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, unbounded};
use dashmap::DashMap;
use tempfile::TempDir;

use pgmcp::config::{Config, ProjectOverride};
use pgmcp::indexer::scanner::{self, ProjectRoot, SyntheticRoots};

fn write_file(dir: &std::path::Path, rel: &str, body: &str) -> PathBuf {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, body).unwrap();
    path
}

fn drain(rx: &Receiver<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    while let Ok(p) = rx.recv_timeout(Duration::from_millis(50)) {
        out.push(p);
    }
    out
}

/// Run a scan with explicit synthetic roots and a fresh unbounded
/// channel. Tests pass `SyntheticRoots::empty()` when they only want
/// the regular workspace walker, or set specific fields to point at
/// tempdir-backed `Papers`/`Documents`/`.claude`/`.codex` paths.
fn run_scan(
    workspace_paths: Vec<String>,
    roots: SyntheticRoots,
) -> (Vec<PathBuf>, Arc<DashMap<PathBuf, ProjectRoot>>) {
    let mut config = Config::default();
    config.workspace.paths = workspace_paths;

    let project_roots: Arc<DashMap<PathBuf, ProjectRoot>> = Arc::new(DashMap::new());
    let project_overrides: Arc<DashMap<PathBuf, ProjectOverride>> = Arc::new(DashMap::new());

    let (tx, rx) = unbounded::<PathBuf>();
    scanner::scan_workspaces(&config, &roots, tx, &project_roots, &project_overrides);
    let files = drain(&rx);

    (files, project_roots)
}

#[test]
fn synthetic_project_papers_indexes_org_pdf_tex_with_dedup() {
    // Lay down a small "Papers" tree under a tempdir and inject it as
    // the synthetic Papers root — no `$HOME` mutation needed.
    let home = TempDir::new().unwrap();
    let papers_root = home.path().join("Papers");
    std::fs::create_dir_all(&papers_root).unwrap();

    // Three sibling forms of the same paper — dedup should keep only the
    // .org form (highest priority).
    write_file(
        &papers_root,
        "attention.org",
        "* Attention\nIs all you need.\n",
    );
    write_file(
        &papers_root,
        "attention.tex",
        "\\section{Attention}\nIs all you need.\n",
    );
    write_file(&papers_root, "attention.pdf", "%PDF-1.7\nfake content\n");

    // A second paper present only as .pdf — should be kept.
    write_file(
        &papers_root,
        "transformer.pdf",
        "%PDF-1.7\ntransformer paper\n",
    );

    // A LaTeX build artifact that should be excluded by PAPERS_DIR_EXCLUDES.
    write_file(&papers_root, "attention.aux", "garbage aux file");

    // A non-priority text file — kept unconditionally because .txt isn't
    // in the priority list.
    write_file(&papers_root, "notes.txt", "loose notes");

    let roots = SyntheticRoots {
        papers: Some(papers_root.clone()),
        ..SyntheticRoots::empty()
    };
    let (mut files, project_roots) = run_scan(Vec::new(), roots);

    files.sort();
    let rel: HashSet<String> = files
        .iter()
        .map(|p| {
            p.strip_prefix(&papers_root)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    // Source-form dedup: only .org survives for "attention.*"; "transformer.pdf"
    // stays as the sole representative; "notes.txt" is kept.
    assert!(
        rel.contains("attention.org"),
        "expected attention.org indexed; got {rel:?}"
    );
    assert!(
        !rel.contains("attention.pdf"),
        "expected attention.pdf dedup'd out"
    );
    assert!(
        !rel.contains("attention.tex"),
        "expected attention.tex dedup'd out"
    );
    assert!(
        rel.contains("transformer.pdf"),
        "expected transformer.pdf indexed"
    );
    assert!(rel.contains("notes.txt"), "expected notes.txt indexed");

    // Build artifacts excluded.
    assert!(!rel.contains("attention.aux"), "expected .aux excluded");

    // Synthetic project registered under the Papers root.
    assert!(
        project_roots.contains_key(&papers_root),
        "expected ProjectRoot registered for ~/Papers/"
    );
    let pr = project_roots.get(&papers_root).unwrap();
    assert_eq!(pr.name, "Papers");
}

#[test]
fn per_project_priority_override_replaces_global() {
    let home = TempDir::new().unwrap();
    let papers_root = home.path().join("Papers");
    std::fs::create_dir_all(&papers_root).unwrap();

    write_file(&papers_root, "invoice.org", "* Invoice\n");
    write_file(&papers_root, "invoice.pdf", "%PDF-1.7\ninvoice\n");

    // .pgmcp.toml that flips the priority to prefer .pdf over .org.
    let override_toml = r#"
[indexer]
source_priority = ["pdf", "org"]
"#;
    write_file(&papers_root, ".pgmcp.toml", override_toml);

    let roots = SyntheticRoots {
        papers: Some(papers_root.clone()),
        ..SyntheticRoots::empty()
    };
    let (files, _project_roots) = run_scan(Vec::new(), roots);

    let rel: HashSet<String> = files
        .iter()
        .map(|p| {
            p.strip_prefix(&papers_root)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        rel.contains("invoice.pdf"),
        "expected pdf to win per override; got {rel:?}"
    );
    assert!(
        !rel.contains("invoice.org"),
        "expected org to be deduplicated out"
    );
}

#[test]
fn empty_home_synthetic_dirs_skip_silently() {
    // No synthetic roots supplied + no workspace paths => nothing to scan.
    let (files, project_roots) = run_scan(Vec::new(), SyntheticRoots::empty());

    assert!(
        files.is_empty(),
        "expected no files indexed when synthetic roots are all None"
    );
    assert!(
        project_roots.is_empty(),
        "no synthetic projects should register"
    );
}

#[test]
fn regular_workspace_scan_still_works_with_document_extensions() {
    // Sanity: adding the new file types didn't break the regular
    // `scan_single_workspace` path. Build a git repo (`.git/` dir),
    // drop a `.txt` and a `.rs` in it, scan; both should index.
    let workspace = TempDir::new().unwrap();
    std::fs::create_dir_all(workspace.path().join(".git")).unwrap();
    write_file(workspace.path(), "src/main.rs", "fn main() {}\n");
    write_file(workspace.path(), "README.txt", "hello\n");

    let (files, project_roots) = run_scan(
        vec![workspace.path().to_string_lossy().into_owned()],
        SyntheticRoots::empty(),
    );
    let names: HashSet<String> = files
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    assert!(names.contains("main.rs"), "got {names:?}");
    assert!(names.contains("README.txt"), "got {names:?}");
    assert!(
        project_roots.contains_key(&workspace.path().to_path_buf()),
        "expected workspace to be discovered as a project root via .git/"
    );
}
