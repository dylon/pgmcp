//! Plan-definition validation — the pure rule-checker half.
//!
//! A `plan_definition` dictates how a valid plan must be shaped via typed
//! `definition_rules` rows. The DB layer
//! (`crate::db::queries::work_items::validate_plan`) loads a definition's rules
//! and the instance subtree's facets; this module evaluates each rule and emits
//! [`Violation`]s in the `tool_architecture_violations` report shape (rule /
//! severity / item / message / recommended_fix). Pure and unit-testable.
//!
//! Validation is *advisory* (it reports); the hard completion gate is the
//! evidence-driven `→verified` transition, not this.

use serde::Serialize;

/// A dictated structural rule (the Rust mirror of a `definition_rules` row).
#[derive(Debug, Clone)]
pub struct RuleSpec {
    pub rule_kind: String,
    /// Which item kind the rule constrains (None = the plan as a whole).
    pub applies_to_kind: Option<String>,
    pub child_kind: Option<String>,
    pub min_count: Option<i32>,
    pub max_count: Option<i32>,
    pub field_name: Option<String>,
    pub pattern: Option<String>,
    pub severity: String,
}

/// Validation-relevant facets of one instance item (gathered by the DB layer).
#[derive(Debug, Clone)]
pub struct ItemFacet {
    pub public_id: String,
    pub parent_public_id: Option<String>,
    pub kind: String,
    pub title: String,
    pub has_body: bool,
    pub has_due: bool,
    pub acceptance_count: i64,
    pub parametric: bool,
    pub has_universal_criterion: bool,
    pub depth: i32,
    /// Names of the bug-detail fields (severity / reproduction_steps / …) that
    /// are present (non-blank) on this item, for `required_field` rules that
    /// target bug metadata. Empty for non-bug items.
    pub bug_fields: Vec<String>,
}

/// One validation finding.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Violation {
    pub rule_kind: String,
    pub severity: String,
    pub item_public_id: Option<String>,
    pub message: String,
    pub recommended_fix: String,
}

/// Severity rank for sorting (mirrors `tool_architecture_violations`): higher
/// is more severe.
pub fn severity_rank(sev: &str) -> u8 {
    match sev {
        "error" => 3,
        "warn" => 2,
        "info" => 1,
        _ => 0,
    }
}

/// Evaluate every rule against the instance subtree, returning violations
/// sorted by severity (error → warn → info). Closed dispatch over the
/// `rule_kind` vocabulary; an unknown kind is ignored (the DB CHECK prevents
/// persisting one).
pub fn validate(items: &[ItemFacet], rules: &[RuleSpec]) -> Vec<Violation> {
    let mut out: Vec<Violation> = Vec::new();
    for rule in rules {
        match rule.rule_kind.as_str() {
            "required_kind" => check_required_kind(items, rule, &mut out),
            "allowed_child_kind" => check_allowed_child_kind(items, rule, &mut out),
            "required_child_kind" => check_required_child_kind(items, rule, &mut out),
            "min_children" => check_child_count(items, rule, true, &mut out),
            "max_children" => check_child_count(items, rule, false, &mut out),
            "required_field" => check_required_field(items, rule, &mut out),
            "required_acceptance_criterion" => check_required_acceptance(items, rule, &mut out),
            "quantifier_requires_corpus" => check_quantifier(items, rule, &mut out),
            "naming_rule" => check_pattern(items, rule, PatternTarget::Title, &mut out),
            "id_rule" => check_pattern(items, rule, PatternTarget::PublicId, &mut out),
            "max_depth_advice" => check_max_depth(items, rule, &mut out),
            _ => {}
        }
    }
    out.sort_by_key(|v| std::cmp::Reverse(severity_rank(&v.severity)));
    out
}

fn push(out: &mut Vec<Violation>, rule: &RuleSpec, item: Option<&str>, msg: String, fix: String) {
    out.push(Violation {
        rule_kind: rule.rule_kind.clone(),
        severity: rule.severity.clone(),
        item_public_id: item.map(|s| s.to_string()),
        message: msg,
        recommended_fix: fix,
    });
}

/// Items whose kind is `applies_to_kind` (or all items if it's None).
fn items_of_kind<'a>(items: &'a [ItemFacet], kind: &Option<String>) -> Vec<&'a ItemFacet> {
    match kind {
        None => items.iter().collect(),
        Some(k) => items.iter().filter(|i| &i.kind == k).collect(),
    }
}

fn children_of<'a>(items: &'a [ItemFacet], parent: &str) -> Vec<&'a ItemFacet> {
    items
        .iter()
        .filter(|i| i.parent_public_id.as_deref() == Some(parent))
        .collect()
}

fn check_required_kind(items: &[ItemFacet], rule: &RuleSpec, out: &mut Vec<Violation>) {
    let Some(kind) = &rule.applies_to_kind else {
        return;
    };
    if !items.iter().any(|i| &i.kind == kind) {
        push(
            out,
            rule,
            None,
            format!("plan must contain at least one '{kind}' item"),
            format!("add a '{kind}' item to the plan"),
        );
    }
}

fn check_allowed_child_kind(items: &[ItemFacet], rule: &RuleSpec, out: &mut Vec<Violation>) {
    let (Some(parent_kind), Some(allowed)) = (&rule.applies_to_kind, &rule.child_kind) else {
        return;
    };
    // `child_kind` may be a comma-separated whitelist.
    let allowed_set: Vec<&str> = allowed.split(',').map(|s| s.trim()).collect();
    for parent in items.iter().filter(|i| &i.kind == parent_kind) {
        for child in children_of(items, &parent.public_id) {
            if !allowed_set.contains(&child.kind.as_str()) {
                push(
                    out,
                    rule,
                    Some(&child.public_id),
                    format!(
                        "'{}' is a '{}' child of a '{}', but only [{}] are allowed",
                        child.public_id, child.kind, parent_kind, allowed
                    ),
                    format!("change the child kind to one of [{allowed}] or re-parent it"),
                );
            }
        }
    }
}

fn check_required_child_kind(items: &[ItemFacet], rule: &RuleSpec, out: &mut Vec<Violation>) {
    let (Some(parent_kind), Some(child_kind)) = (&rule.applies_to_kind, &rule.child_kind) else {
        return;
    };
    for parent in items.iter().filter(|i| &i.kind == parent_kind) {
        if !children_of(items, &parent.public_id)
            .iter()
            .any(|c| &c.kind == child_kind)
        {
            push(
                out,
                rule,
                Some(&parent.public_id),
                format!("'{parent_kind}' must have at least one '{child_kind}' child"),
                format!("add a '{child_kind}' child under '{}'", parent.public_id),
            );
        }
    }
}

fn check_child_count(items: &[ItemFacet], rule: &RuleSpec, is_min: bool, out: &mut Vec<Violation>) {
    let Some(parent_kind) = &rule.applies_to_kind else {
        return;
    };
    let bound = if is_min {
        rule.min_count
    } else {
        rule.max_count
    };
    let Some(bound) = bound else { return };
    for parent in items.iter().filter(|i| &i.kind == parent_kind) {
        let n = children_of(items, &parent.public_id).len() as i32;
        let violated = if is_min { n < bound } else { n > bound };
        if violated {
            let (rel, fix) = if is_min {
                ("at least", "add")
            } else {
                ("at most", "remove")
            };
            push(
                out,
                rule,
                Some(&parent.public_id),
                format!("'{parent_kind}' must have {rel} {bound} children (has {n})"),
                format!("{fix} children of '{}'", parent.public_id),
            );
        }
    }
}

/// Bug-detail field names a `required_field` rule may target (severity lives on
/// the work_items spine, the rest on the `work_item_bug_details` sidecar). Any
/// other unknown field name is skipped (treated as present), matching the
/// pre-bug behavior.
const BUG_FIELD_NAMES: &[&str] = &[
    "severity",
    "reproduction_steps",
    "expected_behavior",
    "actual_behavior",
    "environment",
    "affected_version",
    "fixed_in_version",
    "root_cause",
];

fn check_required_field(items: &[ItemFacet], rule: &RuleSpec, out: &mut Vec<Violation>) {
    let Some(field) = &rule.field_name else {
        return;
    };
    for item in items_of_kind(items, &rule.applies_to_kind) {
        let present = match field.as_str() {
            "body" => item.has_body,
            "due_at" => item.has_due,
            "title" => !item.title.trim().is_empty(),
            f if BUG_FIELD_NAMES.contains(&f) => item.bug_fields.iter().any(|p| p == f),
            _ => true, // unknown field: not checkable here, skip
        };
        if !present {
            push(
                out,
                rule,
                Some(&item.public_id),
                format!(
                    "'{}' ({}) is missing required field '{field}'",
                    item.public_id, item.kind
                ),
                format!("set '{field}' on '{}'", item.public_id),
            );
        }
    }
}

fn check_required_acceptance(items: &[ItemFacet], rule: &RuleSpec, out: &mut Vec<Violation>) {
    for item in items_of_kind(items, &rule.applies_to_kind) {
        if item.acceptance_count == 0 {
            push(
                out,
                rule,
                Some(&item.public_id),
                format!(
                    "'{}' ({}) has no acceptance criterion — it cannot be machine-verified",
                    item.public_id, item.kind
                ),
                format!(
                    "add an acceptance criterion to '{}' via work_item_add_criterion",
                    item.public_id
                ),
            );
        }
    }
}

fn check_quantifier(items: &[ItemFacet], rule: &RuleSpec, out: &mut Vec<Violation>) {
    for item in items.iter().filter(|i| i.parametric) {
        if !item.has_universal_criterion {
            push(
                out,
                rule,
                Some(&item.public_id),
                format!(
                    "'{}' is a universal/parametric clause but has no universal-coverage acceptance criterion — \
                     a single passing case must NOT satisfy it",
                    item.public_id
                ),
                format!(
                    "add a coverage_mode='universal' acceptance criterion to '{}'",
                    item.public_id
                ),
            );
        }
    }
}

enum PatternTarget {
    Title,
    PublicId,
}

fn check_pattern(
    items: &[ItemFacet],
    rule: &RuleSpec,
    target: PatternTarget,
    out: &mut Vec<Violation>,
) {
    let Some(pattern) = &rule.pattern else { return };
    let re = match regex::Regex::new(pattern) {
        Ok(re) => re,
        Err(e) => {
            push(
                out,
                rule,
                None,
                format!("rule has an invalid regex '{pattern}': {e}"),
                "fix the rule's pattern".to_string(),
            );
            return;
        }
    };
    let field = match target {
        PatternTarget::Title => "title",
        PatternTarget::PublicId => "public_id",
    };
    for item in items_of_kind(items, &rule.applies_to_kind) {
        let value = match target {
            PatternTarget::Title => &item.title,
            PatternTarget::PublicId => &item.public_id,
        };
        if !re.is_match(value) {
            push(
                out,
                rule,
                Some(&item.public_id),
                format!(
                    "'{}' {field} '{value}' does not match required pattern /{pattern}/",
                    item.public_id
                ),
                format!("rename so its {field} matches /{pattern}/"),
            );
        }
    }
}

fn check_max_depth(items: &[ItemFacet], rule: &RuleSpec, out: &mut Vec<Violation>) {
    let Some(max) = rule.max_count else { return };
    for item in items.iter().filter(|i| i.depth > max) {
        push(
            out,
            rule,
            Some(&item.public_id),
            format!(
                "'{}' is at depth {} (advisory cap {max})",
                item.public_id, item.depth
            ),
            "consider flattening this branch".to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facet(public_id: &str, kind: &str, parent: Option<&str>) -> ItemFacet {
        ItemFacet {
            public_id: public_id.to_string(),
            parent_public_id: parent.map(|s| s.to_string()),
            kind: kind.to_string(),
            title: public_id.to_string(),
            has_body: false,
            has_due: false,
            acceptance_count: 0,
            parametric: false,
            has_universal_criterion: false,
            depth: 0,
            bug_fields: Vec::new(),
        }
    }

    fn rule(rule_kind: &str) -> RuleSpec {
        RuleSpec {
            rule_kind: rule_kind.to_string(),
            applies_to_kind: None,
            child_kind: None,
            min_count: None,
            max_count: None,
            field_name: None,
            pattern: None,
            severity: "error".to_string(),
        }
    }

    #[test]
    fn required_kind_fires_when_absent() {
        let items = vec![facet("p", "plan", None)];
        let mut r = rule("required_kind");
        r.applies_to_kind = Some("goal".to_string());
        let v = validate(&items, &[r]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule_kind, "required_kind");
    }

    #[test]
    fn required_kind_passes_when_present() {
        let items = vec![facet("p", "plan", None), facet("g", "goal", Some("p"))];
        let mut r = rule("required_kind");
        r.applies_to_kind = Some("goal".to_string());
        assert!(validate(&items, &[r]).is_empty());
    }

    #[test]
    fn allowed_child_kind_flags_disallowed_children() {
        let items = vec![
            facet("e", "epic", None),
            facet("t", "task", Some("e")),
            facet("n", "note", Some("e")),
        ];
        let mut r = rule("allowed_child_kind");
        r.applies_to_kind = Some("epic".to_string());
        r.child_kind = Some("task".to_string());
        let v = validate(&items, &[r]);
        assert_eq!(v.len(), 1, "only the note child is disallowed");
        assert_eq!(v[0].item_public_id.as_deref(), Some("n"));
    }

    #[test]
    fn min_children_enforced() {
        let items = vec![facet("p", "plan", None)];
        let mut r = rule("min_children");
        r.applies_to_kind = Some("plan".to_string());
        r.min_count = Some(1);
        let v = validate(&items, &[r]);
        assert_eq!(v.len(), 1, "plan with no children violates min 1");
    }

    #[test]
    fn required_acceptance_flags_missing() {
        let mut t = facet("t", "task", None);
        t.acceptance_count = 0;
        let mut r = rule("required_acceptance_criterion");
        r.applies_to_kind = Some("task".to_string());
        let v = validate(&[t], &[r]);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn quantifier_requires_universal_criterion() {
        let mut p = facet("u", "task", None);
        p.parametric = true;
        p.has_universal_criterion = false;
        let v = validate(&[p.clone()], &[rule("quantifier_requires_corpus")]);
        assert_eq!(
            v.len(),
            1,
            "parametric without universal criterion is flagged"
        );
        let mut ok = p;
        ok.has_universal_criterion = true;
        assert!(validate(&[ok], &[rule("quantifier_requires_corpus")]).is_empty());
    }

    #[test]
    fn id_rule_checks_regex() {
        let items = vec![facet("BadId", "task", None)];
        let mut r = rule("id_rule");
        r.pattern = Some("^[a-z0-9-]+$".to_string());
        let v = validate(&items, &[r]);
        assert_eq!(
            v.len(),
            1,
            "uppercase public_id fails the lowercase pattern"
        );
    }

    #[test]
    fn required_field_flags_missing_bug_metadata() {
        // A definition can dictate `required_field(applies_to_kind='bug',
        // field_name='severity')`; an absent severity is flagged, a present one
        // passes. Mirrors the hard gate in work_item_triage.
        let mut bug = facet("b", "bug", None);
        let mut r = rule("required_field");
        r.applies_to_kind = Some("bug".to_string());
        r.field_name = Some("severity".to_string());
        let v = validate(&[bug.clone()], std::slice::from_ref(&r));
        assert_eq!(v.len(), 1, "bug missing severity is flagged");
        assert_eq!(v[0].item_public_id.as_deref(), Some("b"));

        bug.bug_fields.push("severity".to_string());
        assert!(
            validate(&[bug], &[r]).is_empty(),
            "bug with severity present passes"
        );
    }

    #[test]
    fn required_field_unknown_field_is_skipped() {
        // A field name the facet does not track (and isn't a known bug field) is
        // treated as present (skip), preserving pre-bug behavior.
        let item = facet("x", "task", None);
        let mut r = rule("required_field");
        r.field_name = Some("nonexistent_field".to_string());
        assert!(validate(&[item], &[r]).is_empty());
    }

    #[test]
    fn violations_sorted_by_severity() {
        let items = vec![facet("p", "plan", None)];
        let mut err = rule("required_kind");
        err.applies_to_kind = Some("goal".to_string());
        err.severity = "error".to_string();
        let mut warn = rule("required_kind");
        warn.applies_to_kind = Some("epic".to_string());
        warn.severity = "warn".to_string();
        let v = validate(&items, &[warn, err]);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].severity, "error", "errors sort before warns");
    }
}
