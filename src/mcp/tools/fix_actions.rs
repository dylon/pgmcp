//! `RecommendedFix` — uniform action contract emitted by recommendation-shaped tools.
//!
//! Every finding from a Tier-1+ tool that proposes a remediation embeds one of
//! these. Downstream agents (Claude Code, automation pipelines) dispatch on the
//! `action` discriminator and execute. Diagnostic-only tools (e.g.
//! `architecture_violations` pre-Enhancement-A) emit findings without a
//! `recommended_fix`.
//!
//! Keep this file dependency-light. Tool bodies build `RecommendedFix` values
//! and serialize through `serde_json::to_value`; nothing here talks to the DB.

#![allow(dead_code)] // Tier-2..5 tools that emit RecommendedFix values are added in
// later phases. The enum and builders are infrastructure;
// exposing them as `pub` is the contract, even before any
// tool body references them.

use serde::{Deserialize, Serialize};

/// The 13 canonical fix actions a finding can recommend.
///
/// Serialized as snake_case strings (e.g. `"split_file"`, `"extract_trait"`).
/// Adding a new variant is a contract change — bump downstream parsers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixAction {
    /// Insert a translation/adapter layer between two over-coupled subsystems.
    AddAntiCorruptionLayer,
    /// Add tests covering a fragile or under-covered path; finding is a hint, not a refactor.
    AddTest,
    /// Pull scattered logic from many hub-and-spoke partners into a single absorbing file.
    ConsolidateLogic,
    /// Remove a file (and its `pub mod` declaration). Used for zombie / dead modules.
    DeleteFile,
    /// Lift duplicated chunks into a private (or pub) function; usually within a single crate.
    ExtractFunction,
    /// Lift a Java/TS/etc. `interface` from concrete implementations.
    ExtractInterface,
    /// Lift identifier-renaming-only boilerplate into a `macro_rules!`/generic/template.
    ExtractMacro,
    /// Lift a cluster across projects into a new shared crate/package.
    ExtractModule,
    /// Lift a Rust `trait` (or Python `Protocol`) from concrete implementations.
    ExtractTrait,
    /// Reverse a directional dependency by introducing the abstraction on the stable side.
    InvertDependency,
    /// Combine two files into one — used when the second is a clear superset/subset.
    MergeFiles,
    /// Move a function (or symbol) from one file to another. Also used for renames
    /// (same `path`, different `name` in `target`).
    MoveFunction,
    /// Split one file into multiple files along community / topic boundaries.
    SplitFile,
}

impl FixAction {
    pub fn as_str(self) -> &'static str {
        match self {
            FixAction::AddAntiCorruptionLayer => "add_anti_corruption_layer",
            FixAction::AddTest => "add_test",
            FixAction::ConsolidateLogic => "consolidate_logic",
            FixAction::DeleteFile => "delete_file",
            FixAction::ExtractFunction => "extract_function",
            FixAction::ExtractInterface => "extract_interface",
            FixAction::ExtractMacro => "extract_macro",
            FixAction::ExtractModule => "extract_module",
            FixAction::ExtractTrait => "extract_trait",
            FixAction::InvertDependency => "invert_dependency",
            FixAction::MergeFiles => "merge_files",
            FixAction::MoveFunction => "move_function",
            FixAction::SplitFile => "split_file",
        }
    }
}

/// Effort tier — coarse-grained, hand-tuned, surfaces in `tech_debt_burn_down` packing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimatedEffort {
    Small,
    Medium,
    Large,
}

/// One contiguous range within a single file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PathRange {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// Where the finding lives — the file(s) and line ranges currently containing the issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationRef {
    pub project: String,
    pub paths: Vec<PathRange>,
}

/// Where the fix should land — paths can be existing or proposed-new.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TargetPath {
    /// Existing path, when the action targets a file that already exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    /// Proposed new path (no file at this location yet).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_new_path: Option<String>,
    /// Optional name override, used by `move_function` to express renames.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_name: Option<String>,
    /// Optional list of contiguous source ranges that should land at this target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_ranges: Option<Vec<(u32, u32)>>,
}

/// Aggregated target — multiple `TargetPath`s for `split_file` etc.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TargetRef {
    pub paths: Vec<TargetPath>,
}

/// One file:line citation (an import or call site that must update with the fix).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileLine {
    pub path: String,
    pub line: u32,
}

/// The contract every Tier-1+ finding embeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecommendedFix {
    pub action: FixAction,
    pub location: LocationRef,
    pub target: TargetRef,
    pub steps: Vec<String>,
    pub references: Vec<FileLine>,
    /// Rule-strength derived in 0..1. Tools should not hardcode 1.0.
    /// Adding tree-sitter symbol data typically raises this by ~0.15.
    pub confidence: f64,
    pub estimated_effort: EstimatedEffort,
}

impl RecommendedFix {
    /// Builder shortcut — most tools start from a known action + location and
    /// add steps/references incrementally.
    pub fn new(action: FixAction, project: impl Into<String>) -> Self {
        Self {
            action,
            location: LocationRef {
                project: project.into(),
                paths: Vec::new(),
            },
            target: TargetRef::default(),
            steps: Vec::new(),
            references: Vec::new(),
            confidence: 0.5,
            estimated_effort: EstimatedEffort::Medium,
        }
    }

    pub fn with_confidence(mut self, c: f64) -> Self {
        self.confidence = c.clamp(0.0, 1.0);
        self
    }

    pub fn with_effort(mut self, e: EstimatedEffort) -> Self {
        self.estimated_effort = e;
        self
    }

    pub fn add_location(mut self, range: PathRange) -> Self {
        self.location.paths.push(range);
        self
    }

    pub fn add_target(mut self, t: TargetPath) -> Self {
        self.target.paths.push(t);
        self
    }

    pub fn add_step(mut self, step: impl Into<String>) -> Self {
        self.steps.push(step.into());
        self
    }

    pub fn add_reference(mut self, file_line: FileLine) -> Self {
        self.references.push(file_line);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_action_round_trips_through_json() {
        for action in [
            FixAction::AddAntiCorruptionLayer,
            FixAction::AddTest,
            FixAction::ConsolidateLogic,
            FixAction::DeleteFile,
            FixAction::ExtractFunction,
            FixAction::ExtractInterface,
            FixAction::ExtractMacro,
            FixAction::ExtractModule,
            FixAction::ExtractTrait,
            FixAction::InvertDependency,
            FixAction::MergeFiles,
            FixAction::MoveFunction,
            FixAction::SplitFile,
        ] {
            let s = serde_json::to_string(&action).expect("serialize action");
            let parsed: FixAction = serde_json::from_str(&s).expect("deserialize action");
            assert_eq!(parsed, action, "round-trip mismatch for {:?}", action);
            // The string form must equal the snake_case literal.
            let stripped: String = s.trim_matches('"').to_string();
            assert_eq!(stripped, action.as_str());
        }
    }

    #[test]
    fn estimated_effort_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&EstimatedEffort::Small).expect("serialize"),
            "\"small\""
        );
        assert_eq!(
            serde_json::to_string(&EstimatedEffort::Medium).expect("serialize"),
            "\"medium\""
        );
        assert_eq!(
            serde_json::to_string(&EstimatedEffort::Large).expect("serialize"),
            "\"large\""
        );
    }

    #[test]
    fn recommended_fix_builder_round_trip() {
        let fix = RecommendedFix::new(FixAction::SplitFile, "f1r3node")
            .with_confidence(0.78)
            .with_effort(EstimatedEffort::Large)
            .add_location(PathRange {
                path: "src/cli/mod.rs".into(),
                start_line: 1,
                end_line: 720,
            })
            .add_target(TargetPath {
                suggested_new_path: Some("src/cli/dispatch.rs".into()),
                line_ranges: Some(vec![(1, 180)]),
                ..Default::default()
            })
            .add_step("Create src/cli/dispatch.rs from lines 1-180 of src/cli/mod.rs.")
            .add_reference(FileLine {
                path: "src/main.rs".into(),
                line: 14,
            });

        let json = serde_json::to_value(&fix).expect("serialize fix");
        assert_eq!(json["action"], "split_file");
        assert_eq!(json["location"]["project"], "f1r3node");
        assert_eq!(json["confidence"], 0.78);
        assert_eq!(json["estimated_effort"], "large");
        assert_eq!(
            json["target"]["paths"][0]["suggested_new_path"],
            "src/cli/dispatch.rs"
        );
        assert!(
            json["target"]["paths"][0].get("path").is_none(),
            "absent fields must be omitted, not null"
        );

        let parsed: RecommendedFix = serde_json::from_value(json).expect("deserialize fix");
        assert_eq!(parsed.action, FixAction::SplitFile);
        assert_eq!(parsed.confidence, 0.78);
        assert_eq!(parsed.estimated_effort, EstimatedEffort::Large);
    }

    #[test]
    fn confidence_is_clamped_to_unit_interval() {
        let high = RecommendedFix::new(FixAction::AddTest, "x").with_confidence(1.5);
        assert_eq!(high.confidence, 1.0);
        let low = RecommendedFix::new(FixAction::AddTest, "x").with_confidence(-0.4);
        assert_eq!(low.confidence, 0.0);
    }

    #[test]
    fn target_path_omits_none_fields() {
        let t = TargetPath {
            suggested_new_path: Some("new.rs".into()),
            ..Default::default()
        };
        let json = serde_json::to_value(&t).expect("serialize target path");
        assert!(json.get("path").is_none());
        assert!(json.get("start_line").is_none());
        assert!(json.get("suggested_name").is_none());
        assert_eq!(json["suggested_new_path"], "new.rs");
    }
}
