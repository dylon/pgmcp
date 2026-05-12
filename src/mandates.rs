//! Workspace/project mandate discovery.
//!
//! Mandates are intentionally file-backed for v1. pgmcp surfaces existing
//! agent-facing files and project override facts through MCP/REST/CLI, but
//! enforcement remains a client/hook/CI concern.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::{Config, FileTypeMapping, ProjectOverride};
use crate::db::DbClient;
use crate::db::queries::ProjectInfo;

pub const MANDATE_TEXT_LIMIT_BYTES: usize = 12_000;
const MANDATE_FILENAMES: &[(&str, &str)] = &[("AGENTS.md", "agents"), ("CLAUDE.md", "claude")];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MandateBundle {
    pub enforcement_model: String,
    pub project: Option<MandateProject>,
    pub workspace_roots: Vec<String>,
    pub sources: Vec<MandateSource>,
    pub skipped_sources: Vec<SkippedMandateSource>,
    pub project_override: Option<ProjectOverrideFacts>,
    pub guidance: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MandateProject {
    pub name: String,
    pub path: String,
    pub workspace_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MandateSource {
    pub scope: String,
    pub kind: String,
    pub path: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub truncated: bool,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkippedMandateSource {
    pub scope: String,
    pub kind: String,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectOverrideFacts {
    pub source_path: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub truncated: bool,
    pub text: String,
    pub git_index_history: Option<bool>,
    pub max_file_size_bytes: Option<u64>,
    pub exclude_patterns: Option<Vec<String>>,
    pub file_types: Option<Vec<FileTypeMapping>>,
}

pub fn resolve_effective_mandates(config: &Config, project: Option<&ProjectInfo>) -> MandateBundle {
    let workspace_roots = relevant_workspace_roots(config, project);
    let mut sources = Vec::new();
    let mut skipped_sources = Vec::new();
    let mut seen = HashSet::new();

    for root in &workspace_roots {
        collect_mandate_files(
            Path::new(root),
            "workspace",
            &mut seen,
            &mut sources,
            &mut skipped_sources,
        );
    }

    if let Some(project) = project {
        collect_mandate_files(
            Path::new(&project.path),
            "project",
            &mut seen,
            &mut sources,
            &mut skipped_sources,
        );
    }

    let project_override = project.and_then(|p| {
        resolve_project_override(Path::new(&p.path), &mut seen, &mut skipped_sources)
    });

    MandateBundle {
        enforcement_model: "advisory-via-mcp; hard gates require client hooks or CI".into(),
        project: project.map(|p| MandateProject {
            name: p.name.clone(),
            path: p.path.clone(),
            workspace_path: p.workspace_path.clone(),
        }),
        workspace_roots,
        sources,
        skipped_sources,
        project_override,
        guidance: vec![
            "Consult this bundle before non-trivial work in the workspace or project.".into(),
            "MCP can surface mandates and context, but cannot force every client tool call.".into(),
            "Use client hooks, pre-push hooks, CI, or verification scripts for hard enforcement."
                .into(),
        ],
    }
}

pub async fn resolve_project_for_mandates(
    db: &dyn DbClient,
    project: Option<&str>,
    cwd: Option<&str>,
) -> Result<Option<ProjectInfo>, sqlx::Error> {
    if let Some(project_name) = project {
        let projects = db.list_projects().await?;
        return Ok(projects.into_iter().find(|p| p.name == project_name));
    }

    if let Some(cwd) = cwd {
        let normalized = if cwd.ends_with('/') {
            cwd.to_string()
        } else {
            format!("{}/", cwd)
        };
        return db.find_project_by_cwd(&normalized).await;
    }

    Ok(None)
}

pub fn render_mandates_markdown(bundle: &MandateBundle) -> String {
    let mut out = String::new();
    out.push_str("### Mandates\n");

    if bundle.sources.is_empty() && bundle.project_override.is_none() {
        out.push_str("No AGENTS.md, CLAUDE.md, or .pgmcp.toml mandate sources found.\n");
        return out;
    }

    for source in &bundle.sources {
        out.push_str(&format!(
            "\n#### {} {} ({})\n",
            title_case(&source.scope),
            source.kind,
            source.path
        ));
        out.push_str(&format!(
            "sha256: {} | size_bytes: {} | truncated: {}\n\n",
            source.sha256, source.size_bytes, source.truncated
        ));
        out.push_str(&source.text);
        if !source.text.ends_with('\n') {
            out.push('\n');
        }
    }

    if let Some(override_facts) = &bundle.project_override {
        out.push_str(&format!(
            "\n#### Project .pgmcp.toml ({})\n",
            override_facts.source_path
        ));
        out.push_str(&format!(
            "sha256: {} | size_bytes: {} | truncated: {}\n",
            override_facts.sha256, override_facts.size_bytes, override_facts.truncated
        ));
        if let Some(index_history) = override_facts.git_index_history {
            out.push_str(&format!("git.index_history: {}\n", index_history));
        }
        if let Some(max_file_size) = override_facts.max_file_size_bytes {
            out.push_str(&format!("indexer.max_file_size_bytes: {}\n", max_file_size));
        }
        if let Some(patterns) = &override_facts.exclude_patterns {
            out.push_str(&format!("indexer.exclude_patterns: {:?}\n", patterns));
        }
        if let Some(file_types) = &override_facts.file_types {
            out.push_str(&format!(
                "indexer.file_types: {} entries\n",
                file_types.len()
            ));
        }
    }

    out
}

pub fn compact_sources(bundle: &MandateBundle) -> serde_json::Value {
    serde_json::json!({
        "enforcement_model": &bundle.enforcement_model,
        "project": &bundle.project,
        "workspace_roots": &bundle.workspace_roots,
        "sources": bundle.sources.iter().map(|s| serde_json::json!({
            "scope": &s.scope,
            "kind": &s.kind,
            "path": &s.path,
            "sha256": &s.sha256,
            "size_bytes": s.size_bytes,
            "truncated": s.truncated,
            "text": &s.text,
        })).collect::<Vec<_>>(),
        "skipped_sources": &bundle.skipped_sources,
        "project_override": &bundle.project_override,
        "guidance": &bundle.guidance,
    })
}

fn relevant_workspace_roots(config: &Config, project: Option<&ProjectInfo>) -> Vec<String> {
    if let Some(project) = project {
        return vec![project.workspace_path.clone()];
    }

    config.workspace.paths.clone()
}

fn collect_mandate_files(
    root: &Path,
    scope: &str,
    seen: &mut HashSet<PathBuf>,
    sources: &mut Vec<MandateSource>,
    skipped_sources: &mut Vec<SkippedMandateSource>,
) {
    for (filename, kind) in MANDATE_FILENAMES {
        let path = root.join(filename);
        match read_source(root, &path, scope, kind, seen) {
            SourceRead::Missing => {}
            SourceRead::Read(source) => sources.push(source),
            SourceRead::Skipped(skipped) => skipped_sources.push(skipped),
        }
    }
}

fn resolve_project_override(
    project_root: &Path,
    seen: &mut HashSet<PathBuf>,
    skipped_sources: &mut Vec<SkippedMandateSource>,
) -> Option<ProjectOverrideFacts> {
    let path = project_root.join(".pgmcp.toml");
    let source = match read_source(
        project_root,
        &path,
        "project",
        "pgmcp_project_override",
        seen,
    ) {
        SourceRead::Missing => return None,
        SourceRead::Read(source) => source,
        SourceRead::Skipped(skipped) => {
            skipped_sources.push(skipped);
            return None;
        }
    };

    let parsed = ProjectOverride::load(project_root);
    Some(ProjectOverrideFacts {
        source_path: source.path,
        sha256: source.sha256,
        size_bytes: source.size_bytes,
        truncated: source.truncated,
        text: source.text,
        git_index_history: parsed
            .as_ref()
            .and_then(|override_config| override_config.git.as_ref())
            .map(|git| git.index_history),
        max_file_size_bytes: parsed.as_ref().and_then(|override_config| {
            override_config
                .indexer
                .as_ref()
                .and_then(|indexer| indexer.max_file_size_bytes)
        }),
        exclude_patterns: parsed.as_ref().and_then(|override_config| {
            override_config
                .indexer
                .as_ref()
                .and_then(|indexer| indexer.exclude_patterns.clone())
        }),
        file_types: parsed.as_ref().and_then(|override_config| {
            override_config
                .indexer
                .as_ref()
                .and_then(|indexer| indexer.file_types.clone())
        }),
    })
}

enum SourceRead {
    Missing,
    Read(MandateSource),
    Skipped(SkippedMandateSource),
}

fn read_source(
    root: &Path,
    path: &Path,
    scope: &str,
    kind: &str,
    seen: &mut HashSet<PathBuf>,
) -> SourceRead {
    if !path.exists() {
        return SourceRead::Missing;
    }

    let root_canonical = match root.canonicalize() {
        Ok(path) => path,
        Err(e) => {
            return skipped(
                scope,
                kind,
                path,
                format!("root canonicalize failed: {}", e),
            );
        }
    };
    let path_canonical = match path.canonicalize() {
        Ok(path) => path,
        Err(e) => {
            return skipped(
                scope,
                kind,
                path,
                format!("source canonicalize failed: {}", e),
            );
        }
    };

    if !path_canonical.starts_with(&root_canonical) {
        return skipped(scope, kind, path, "source resolves outside root".into());
    }

    if !seen.insert(path_canonical.clone()) {
        return SourceRead::Missing;
    }

    let metadata = match std::fs::metadata(&path_canonical) {
        Ok(metadata) => metadata,
        Err(e) => return skipped(scope, kind, path, format!("metadata failed: {}", e)),
    };
    if !metadata.is_file() {
        return skipped(scope, kind, path, "source is not a regular file".into());
    }

    let bytes = match std::fs::read(&path_canonical) {
        Ok(bytes) => bytes,
        Err(e) => return skipped(scope, kind, path, format!("read failed: {}", e)),
    };

    let digest = sha256_hex(&bytes);
    let full_text = String::from_utf8_lossy(&bytes);
    let limit = if full_text.len() <= MANDATE_TEXT_LIMIT_BYTES {
        full_text.len()
    } else {
        full_text.floor_char_boundary(MANDATE_TEXT_LIMIT_BYTES)
    };
    let text = full_text[..limit].to_string();

    SourceRead::Read(MandateSource {
        scope: scope.into(),
        kind: kind.into(),
        path: path_canonical.to_string_lossy().into_owned(),
        sha256: digest,
        size_bytes: metadata.len(),
        truncated: bytes.len() > limit,
        text,
    })
}

fn skipped(scope: &str, kind: &str, path: &Path, reason: String) -> SourceRead {
    SourceRead::Skipped(SkippedMandateSource {
        scope: scope.into(),
        kind: kind.into(),
        path: path.to_string_lossy().into_owned(),
        reason,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push_str(&format!("{:02x}", byte));
    }
    output
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::tempdir;

    fn project_info(workspace: &Path, project: &Path) -> ProjectInfo {
        ProjectInfo {
            id: 1,
            workspace_path: workspace.to_string_lossy().into_owned(),
            path: project.to_string_lossy().into_owned(),
            name: "app".into(),
            git_common_dir: None,
            git_root_commits: None,
            discovered_at: Some(Utc::now()),
            last_scanned_at: None,
            file_count: Some(0),
        }
    }

    #[test]
    fn resolves_workspace_project_and_project_override_sources() {
        let temp = tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let project = workspace.join("app");
        std::fs::create_dir_all(&project).expect("create project");
        std::fs::write(workspace.join("AGENTS.md"), "workspace rules").expect("write agents");
        std::fs::write(project.join("CLAUDE.md"), "project info").expect("write claude");
        std::fs::write(
            project.join(".pgmcp.toml"),
            "[git]\nindex_history = true\n\n[indexer]\nmax_file_size_bytes = 42\n",
        )
        .expect("write override");

        let mut config = Config::default();
        config.workspace.paths = vec![workspace.to_string_lossy().into_owned()];

        let project = project_info(&workspace, &project);
        let bundle = resolve_effective_mandates(&config, Some(&project));

        assert_eq!(bundle.sources.len(), 2);
        assert!(bundle.sources.iter().any(|s| s.text == "workspace rules"));
        assert!(bundle.sources.iter().any(|s| s.text == "project info"));
        let facts = bundle.project_override.expect("override facts");
        assert_eq!(facts.git_index_history, Some(true));
        assert_eq!(facts.max_file_size_bytes, Some(42));
    }

    #[test]
    fn missing_sources_return_empty_bundle() {
        let temp = tempdir().expect("tempdir");
        let mut config = Config::default();
        config.workspace.paths = vec![temp.path().to_string_lossy().into_owned()];

        let bundle = resolve_effective_mandates(&config, None);

        assert!(bundle.sources.is_empty());
        assert!(bundle.project_override.is_none());
        assert!(bundle.skipped_sources.is_empty());
    }

    #[test]
    fn truncates_large_sources_on_char_boundary() {
        let temp = tempdir().expect("tempdir");
        let content = format!("{}{}", "a".repeat(MANDATE_TEXT_LIMIT_BYTES), "é");
        std::fs::write(temp.path().join("AGENTS.md"), content).expect("write source");

        let mut config = Config::default();
        config.workspace.paths = vec![temp.path().to_string_lossy().into_owned()];
        let bundle = resolve_effective_mandates(&config, None);

        let source = &bundle.sources[0];
        assert!(source.truncated);
        assert_eq!(source.text.len(), MANDATE_TEXT_LIMIT_BYTES);
        assert!(source.text.is_char_boundary(source.text.len()));
    }

    #[cfg(unix)]
    #[test]
    fn skips_symlink_that_resolves_outside_root() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::write(&outside, "outside").expect("write outside");
        symlink(&outside, root.join("AGENTS.md")).expect("symlink");

        let mut config = Config::default();
        config.workspace.paths = vec![root.to_string_lossy().into_owned()];
        let bundle = resolve_effective_mandates(&config, None);

        assert!(bundle.sources.is_empty());
        assert_eq!(bundle.skipped_sources.len(), 1);
        assert_eq!(
            bundle.skipped_sources[0].reason,
            "source resolves outside root"
        );
    }
}
