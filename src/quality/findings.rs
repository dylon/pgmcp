//! The canonical finding vocabulary shared by every `collect_*` helper and the
//! `quality_report` aggregator.
//!
//! There is intentionally no pre-existing `Finding`/`Severity` type in the tree
//! (only a file-local one in `tool_documented_tech_debt`), so this module is the
//! single source of truth. [`Finding`] reuses [`PathRange`] and [`RecommendedFix`]
//! from `crate::mcp::tools::fix_actions` rather than duplicating location/fix
//! shapes.

use serde::{Deserialize, Serialize};

use crate::mcp::tools::fix_actions::{PathRange, RecommendedFix};

/// Severity, synthesized per-tool by the aggregator (most tools emit no severity
/// of their own; a few pass theirs through). Ordered Critical > … > Info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

impl Severity {
    /// 4 (Critical) … 0 (Info). Cached into [`Finding::severity_rank`] for cheap
    /// sorting and surfaced to JSON consumers.
    pub fn rank(self) -> i32 {
        match self {
            Severity::Critical => 4,
            Severity::High => 3,
            Severity::Medium => 2,
            Severity::Low => 1,
            Severity::Info => 0,
        }
    }

    /// Weight used by `finding_density` (the per-pillar dimension that lets the
    /// grade reflect enumerated findings) and the top-issues roll-up.
    pub fn weight(self) -> f64 {
        match self {
            Severity::Critical => 10.0,
            Severity::High => 4.0,
            Severity::Medium => 1.0,
            Severity::Low => 0.25,
            Severity::Info => 0.0,
        }
    }

    /// Geometric/no-entry unicode glyph (NOT emoji) used uniformly across all
    /// render formats — see `crate::render`'s glyph policy.
    pub fn glyph(self) -> &'static str {
        match self {
            Severity::Critical => "⛔",
            Severity::High => "◆",
            Severity::Medium => "◇",
            Severity::Low => "○",
            Severity::Info => "·",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Severity::Critical => "Critical",
            Severity::High => "High",
            Severity::Medium => "Medium",
            Severity::Low => "Low",
            Severity::Info => "Info",
        }
    }

    /// Parse a `min_severity` param value. `Info` is intentionally NOT accepted
    /// as a floor — the documented floors are `low|medium|high|critical`.
    /// Returns `None` for anything unrecognized so the caller can error cleanly
    /// rather than silently defaulting.
    pub fn parse_floor(s: &str) -> Option<Severity> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Severity::Low),
            "medium" | "med" => Some(Severity::Medium),
            "high" => Some(Severity::High),
            "critical" | "crit" => Some(Severity::Critical),
            _ => None,
        }
    }
}

/// The eight finding buckets the report groups by. Each maps to exactly one
/// [`Pillar`] for the `finding_density` dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCategory {
    CodeHealth,
    Architecture,
    Security,
    Concurrency,
    TestsDocs,
    Duplication,
    Dependency,
    Hygiene,
}

impl FindingCategory {
    /// All eight, in display order (used to render category sections + appendix).
    pub const ALL: [FindingCategory; 8] = [
        FindingCategory::CodeHealth,
        FindingCategory::Architecture,
        FindingCategory::Security,
        FindingCategory::Concurrency,
        FindingCategory::TestsDocs,
        FindingCategory::Duplication,
        FindingCategory::Dependency,
        FindingCategory::Hygiene,
    ];

    /// Category → pillar map for `finding_density`. Engineering absorbs the
    /// general code-quality buckets; Architecture owns structural + dependency
    /// concerns; Security owns its own.
    pub fn pillar(self) -> Pillar {
        match self {
            FindingCategory::CodeHealth
            | FindingCategory::TestsDocs
            | FindingCategory::Duplication
            | FindingCategory::Concurrency
            | FindingCategory::Hygiene => Pillar::Engineering,
            FindingCategory::Architecture | FindingCategory::Dependency => Pillar::Architecture,
            FindingCategory::Security => Pillar::Security,
        }
    }

    /// Human-readable section title.
    pub fn title(self) -> &'static str {
        match self {
            FindingCategory::CodeHealth => "Code Health",
            FindingCategory::Architecture => "Architecture",
            FindingCategory::Security => "Security",
            FindingCategory::Concurrency => "Concurrency & Safety",
            FindingCategory::TestsDocs => "Tests & Docs",
            FindingCategory::Duplication => "Duplication",
            FindingCategory::Dependency => "Dependencies",
            FindingCategory::Hygiene => "Hygiene",
        }
    }
}

/// The three top-level pillars, each graded independently and averaged into the
/// overall GPA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pillar {
    Engineering,
    Architecture,
    Security,
}

impl Pillar {
    pub const ALL: [Pillar; 3] = [Pillar::Engineering, Pillar::Architecture, Pillar::Security];

    pub fn title(self) -> &'static str {
        match self {
            Pillar::Engineering => "Engineering",
            Pillar::Architecture => "Architecture",
            Pillar::Security => "Security",
        }
    }
}

/// One issue identified by a single analysis tool, normalized into a uniform
/// shape so the aggregator can rank, de-duplicate, and render across tools.
///
/// `location == None` denotes a project-/module-/cluster-level finding. For
/// cluster-keyed tools (e.g. `find_duplicates`, `coupling_cohesion_report`) the
/// `collect_*` helper MUST fold the cluster/module discriminator into `kind`
/// (e.g. `kind = "duplicate_cluster:42"`) so [`Finding::dedupe_key`] stays
/// collision-free — the bare `(source_tool, kind, description)` fallback would
/// otherwise merge distinct clusters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// Tool slug that produced this finding (e.g. `"secret_detection"`).
    pub source_tool: String,
    pub category: FindingCategory,
    pub project: String,
    pub severity: Severity,
    /// Cached `severity.rank()` for cheap sorting / JSON consumers.
    pub severity_rank: i32,
    /// Raw per-tool numeric score when meaningful (entropy, composite, lof, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Primary location. `None` for project/module/cluster-level findings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<PathRange>,
    /// Extra locations (dependency cycles, taint source/sink pairs, shotgun
    /// surgery call-sites).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_locations: Vec<PathRange>,
    /// Stable subtype within `source_tool` (e.g. `"god_module"`, `"pii_logged"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub description: String,
    /// Tool-specific payload, populated ONLY when the caller asked for the JSON
    /// envelope (`include_underlying_json=true`); kept `None` in the default
    /// render path to avoid multi-MB responses on large projects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
    /// Remediation, when the source tool proposes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_fix: Option<RecommendedFix>,
}

impl Finding {
    /// Construct a finding, caching `severity_rank` from `severity`. Optional
    /// fields default empty and are set via the `with_*` builders.
    pub fn new(
        source_tool: impl Into<String>,
        category: FindingCategory,
        project: impl Into<String>,
        severity: Severity,
        description: impl Into<String>,
    ) -> Self {
        Finding {
            source_tool: source_tool.into(),
            category,
            project: project.into(),
            severity,
            severity_rank: severity.rank(),
            score: None,
            location: None,
            additional_locations: Vec::new(),
            kind: None,
            description: description.into(),
            raw: None,
            recommended_fix: None,
        }
    }

    pub fn with_score(mut self, score: f64) -> Self {
        self.score = Some(score);
        self
    }

    pub fn with_location(mut self, loc: PathRange) -> Self {
        self.location = Some(loc);
        self
    }

    /// Convenience for the common `path` + single line (1-based) case; scanners
    /// that compute lines as `usize` cast at the call site.
    pub fn at(mut self, path: impl Into<String>, line: u32) -> Self {
        self.location = Some(PathRange {
            path: path.into(),
            start_line: line,
            end_line: line,
        });
        self
    }

    /// Whole-file finding (no meaningful line).
    pub fn at_file(mut self, path: impl Into<String>) -> Self {
        self.location = Some(PathRange {
            path: path.into(),
            start_line: 0,
            end_line: 0,
        });
        self
    }

    pub fn with_additional(mut self, locs: Vec<PathRange>) -> Self {
        self.additional_locations = locs;
        self
    }

    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    pub fn with_raw(mut self, raw: serde_json::Value) -> Self {
        self.raw = Some(raw);
        self
    }

    pub fn with_fix(mut self, fix: RecommendedFix) -> Self {
        self.recommended_fix = Some(fix);
        self
    }

    /// Stable key for cross-tool de-duplication. Uses the primary location when
    /// present; otherwise folds `kind` + `description` (cluster-keyed tools put
    /// their discriminator in `kind` — see the type docs).
    pub fn dedupe_key(&self) -> String {
        const SEP: char = '\u{1}';
        match &self.location {
            Some(loc) => format!(
                "{}{SEP}{}{SEP}{}{SEP}{}",
                self.source_tool,
                loc.path,
                loc.start_line,
                self.kind.as_deref().unwrap_or("")
            ),
            None => format!(
                "{}{SEP}{}{SEP}{}",
                self.source_tool,
                self.kind.as_deref().unwrap_or(""),
                self.description
            ),
        }
    }

    /// The display path for the primary location (`path:line`, `path`, or
    /// `(project-level)`), used by every renderer.
    pub fn location_label(&self) -> String {
        match &self.location {
            Some(loc) if loc.start_line > 0 => format!("{}:{}", loc.path, loc.start_line),
            Some(loc) => loc.path.clone(),
            None => "(project-level)".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_rank_and_weight_are_monotonic() {
        let order = [
            Severity::Critical,
            Severity::High,
            Severity::Medium,
            Severity::Low,
            Severity::Info,
        ];
        for pair in order.windows(2) {
            assert!(pair[0].rank() > pair[1].rank(), "rank must decrease");
            assert!(
                pair[0].weight() >= pair[1].weight(),
                "weight must not increase"
            );
        }
        assert_eq!(Severity::Info.weight(), 0.0);
    }

    #[test]
    fn severity_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Severity::Critical).expect("serialize"),
            "\"critical\""
        );
    }

    #[test]
    fn parse_floor_rejects_info_and_garbage() {
        assert_eq!(Severity::parse_floor("HIGH"), Some(Severity::High));
        assert_eq!(Severity::parse_floor("med"), Some(Severity::Medium));
        assert_eq!(Severity::parse_floor("info"), None);
        assert_eq!(Severity::parse_floor("bogus"), None);
    }

    #[test]
    fn category_pillar_map_covers_all_eight() {
        // Every category maps to a pillar; spot-check the three buckets.
        assert_eq!(FindingCategory::Security.pillar(), Pillar::Security);
        assert_eq!(FindingCategory::Architecture.pillar(), Pillar::Architecture);
        assert_eq!(FindingCategory::Dependency.pillar(), Pillar::Architecture);
        assert_eq!(FindingCategory::CodeHealth.pillar(), Pillar::Engineering);
        assert_eq!(FindingCategory::Hygiene.pillar(), Pillar::Engineering);
    }

    #[test]
    fn dedupe_key_distinguishes_path_and_cluster_findings() {
        let a = Finding::new(
            "secret_detection",
            FindingCategory::Security,
            "p",
            Severity::Critical,
            "x",
        )
        .at("src/a.rs", 10);
        let b = Finding::new(
            "secret_detection",
            FindingCategory::Security,
            "p",
            Severity::Critical,
            "y",
        )
        .at("src/a.rs", 11);
        assert_ne!(a.dedupe_key(), b.dedupe_key(), "different lines differ");

        // Cluster-keyed: discriminator lives in `kind`.
        let c1 = Finding::new(
            "find_duplicates",
            FindingCategory::Duplication,
            "p",
            Severity::Low,
            "cluster",
        )
        .with_kind("duplicate_cluster:1");
        let c2 = Finding::new(
            "find_duplicates",
            FindingCategory::Duplication,
            "p",
            Severity::Low,
            "cluster",
        )
        .with_kind("duplicate_cluster:2");
        assert_ne!(c1.dedupe_key(), c2.dedupe_key(), "distinct clusters differ");
    }

    #[test]
    fn location_label_handles_three_shapes() {
        let line =
            Finding::new("t", FindingCategory::Hygiene, "p", Severity::Low, "d").at("f.rs", 7);
        assert_eq!(line.location_label(), "f.rs:7");
        let file =
            Finding::new("t", FindingCategory::Hygiene, "p", Severity::Low, "d").at_file("f.rs");
        assert_eq!(file.location_label(), "f.rs");
        let proj = Finding::new("t", FindingCategory::Hygiene, "p", Severity::Low, "d");
        assert_eq!(proj.location_label(), "(project-level)");
    }
}
