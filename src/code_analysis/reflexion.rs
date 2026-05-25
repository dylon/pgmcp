//! Reflexion-model conformance (Murphy, Notkin & Sullivan, "Software Reflexion
//! Models: Bridging the Gap between Design and Implementation", TSE 2001).
//! (graph-roadmap Phase 3.2)
//!
//! Compares a *declared* layered architecture (layers + permitted inter-layer
//! dependencies, from `.pgmcp.toml [architecture]`) against the *actual* import
//! edges. In reflexion terms every actual edge is one of:
//!
//!  - **convergence** — present AND permitted (same-layer, or an `allow` rule),
//!  - **divergence** — present but NOT permitted (a layering violation),
//!
//! and every permitted dependency with no actual edge is an **absence**
//! (computed by the caller from [`ReflexionSummary::realized_pairs`]).
//!
//! Pure and DB-free: the caller supplies the declared rules and the list of
//! actual `(src_path, dst_path)` import edges. File→layer assignment is by
//! first-matching path prefix (relative to the project root).

use std::collections::HashSet;

use crate::config::ArchitectureRules;

/// Per-edge reflexion verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Source and target are in the same layer (always permitted).
    SameLayer,
    /// Cross-layer edge explicitly permitted by an `allow` rule.
    Allowed,
    /// Cross-layer edge that no `allow` rule permits — a layering violation.
    Divergence,
    /// At least one endpoint is unlayered (no declared prefix matched) — not
    /// judged (the rules don't cover it).
    Unlayered,
}

/// A divergent (violating) edge, with the layers it crossed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    pub src_path: String,
    pub dst_path: String,
    pub from_layer: String,
    pub to_layer: String,
}

/// Outcome of classifying a whole edge set against the layer model.
#[derive(Debug, Clone, Default)]
pub struct ReflexionSummary {
    /// Edges that violate the declared rules.
    pub divergences: Vec<Divergence>,
    /// Count of permitted edges (same-layer + allowed).
    pub convergences: usize,
    /// Count of edges with at least one unlayered endpoint.
    pub unlayered: usize,
    /// Distinct permitted cross-layer `(from, to)` pairs that were actually
    /// realized by ≥1 edge. The caller derives **absences** =
    /// declared `allow` rules ∖ this set.
    pub realized_pairs: HashSet<(String, String)>,
}

/// A compiled layer model: ordered layers (name + owned path prefixes) and the
/// set of permitted directed `(from, to)` layer pairs.
#[derive(Debug, Clone)]
pub struct LayerModel {
    layers: Vec<(String, Vec<String>)>,
    allow: HashSet<(String, String)>,
}

impl LayerModel {
    /// Compile from the declared `.pgmcp.toml [architecture]` rules.
    pub fn from_rules(rules: &ArchitectureRules) -> Self {
        let layers = rules
            .layers
            .iter()
            .map(|l| (l.name.clone(), l.paths.clone()))
            .collect();
        let allow = rules
            .allow
            .iter()
            .map(|r| (r.from.clone(), r.to.clone()))
            .collect();
        Self { layers, allow }
    }

    /// True when no layers are declared (nothing to check).
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// The first declared layer whose any prefix is a prefix of `path`, or
    /// `None` when the file belongs to no declared layer.
    pub fn layer_of<'a>(&'a self, path: &str) -> Option<&'a str> {
        self.layers
            .iter()
            .find(|(_, prefixes)| prefixes.iter().any(|p| path.starts_with(p.as_str())))
            .map(|(name, _)| name.as_str())
    }

    /// Classify a single directed import edge.
    pub fn classify(&self, src_path: &str, dst_path: &str) -> Verdict {
        match (self.layer_of(src_path), self.layer_of(dst_path)) {
            (Some(s), Some(t)) => {
                if s == t {
                    Verdict::SameLayer
                } else if self.allow.contains(&(s.to_string(), t.to_string())) {
                    Verdict::Allowed
                } else {
                    Verdict::Divergence
                }
            }
            _ => Verdict::Unlayered,
        }
    }

    /// Classify a whole edge set into a [`ReflexionSummary`]. `edges` are
    /// `(src_path, dst_path)` of actual import edges.
    pub fn summarize<'a, I>(&self, edges: I) -> ReflexionSummary
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut summary = ReflexionSummary::default();
        for (src, dst) in edges {
            match self.classify(src, dst) {
                Verdict::SameLayer => summary.convergences += 1,
                Verdict::Allowed => {
                    summary.convergences += 1;
                    // Safe to unwrap: Allowed implies both endpoints layered.
                    if let (Some(s), Some(t)) = (self.layer_of(src), self.layer_of(dst)) {
                        summary
                            .realized_pairs
                            .insert((s.to_string(), t.to_string()));
                    }
                }
                Verdict::Divergence => {
                    let from_layer = self.layer_of(src).unwrap_or_default().to_string();
                    let to_layer = self.layer_of(dst).unwrap_or_default().to_string();
                    summary.divergences.push(Divergence {
                        src_path: src.to_string(),
                        dst_path: dst.to_string(),
                        from_layer,
                        to_layer,
                    });
                }
                Verdict::Unlayered => summary.unlayered += 1,
            }
        }
        summary
    }

    /// Declared permitted pairs NOT realized by any actual edge ("absences").
    pub fn absences(&self, realized: &HashSet<(String, String)>) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .allow
            .iter()
            .filter(|pair| !realized.contains(*pair))
            .cloned()
            .collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AllowRule, ArchitectureRules, LayerDef};

    fn rules() -> ArchitectureRules {
        ArchitectureRules {
            layers: vec![
                LayerDef {
                    name: "api".into(),
                    paths: vec!["src/api/".into(), "src/mcp/".into()],
                },
                LayerDef {
                    name: "domain".into(),
                    paths: vec!["src/graph/".into()],
                },
                LayerDef {
                    name: "data".into(),
                    paths: vec!["src/db/".into()],
                },
            ],
            allow: vec![
                AllowRule {
                    from: "api".into(),
                    to: "domain".into(),
                },
                AllowRule {
                    from: "domain".into(),
                    to: "data".into(),
                },
            ],
        }
    }

    #[test]
    fn layer_of_first_prefix_match() {
        let m = LayerModel::from_rules(&rules());
        assert_eq!(m.layer_of("src/api/handlers.rs"), Some("api"));
        assert_eq!(m.layer_of("src/mcp/server.rs"), Some("api"));
        assert_eq!(m.layer_of("src/graph/dsm.rs"), Some("domain"));
        assert_eq!(m.layer_of("src/db/queries.rs"), Some("data"));
        assert_eq!(m.layer_of("src/util/misc.rs"), None);
    }

    #[test]
    fn classifies_each_reflexion_category() {
        let m = LayerModel::from_rules(&rules());
        // same layer
        assert_eq!(
            m.classify("src/api/a.rs", "src/mcp/b.rs"),
            Verdict::SameLayer
        );
        // permitted cross-layer
        assert_eq!(
            m.classify("src/api/a.rs", "src/graph/g.rs"),
            Verdict::Allowed
        );
        // forbidden cross-layer (api → data skips the allowed path)
        assert_eq!(
            m.classify("src/api/a.rs", "src/db/q.rs"),
            Verdict::Divergence
        );
        // upward edge data → domain is not in `allow` ⇒ divergence
        assert_eq!(
            m.classify("src/db/q.rs", "src/graph/g.rs"),
            Verdict::Divergence
        );
        // unlayered endpoint
        assert_eq!(
            m.classify("src/util/x.rs", "src/db/q.rs"),
            Verdict::Unlayered
        );
    }

    #[test]
    fn summarize_tallies_and_absences() {
        let m = LayerModel::from_rules(&rules());
        let edges = vec![
            ("src/api/a.rs", "src/graph/g.rs"), // allowed (api→domain)
            ("src/api/a.rs", "src/mcp/b.rs"),   // same-layer
            ("src/api/a.rs", "src/db/q.rs"),    // divergence (api→data)
            ("src/util/x.rs", "src/db/q.rs"),   // unlayered
        ];
        let s = m.summarize(edges);
        assert_eq!(s.convergences, 2);
        assert_eq!(s.unlayered, 1);
        assert_eq!(s.divergences.len(), 1);
        assert_eq!(s.divergences[0].from_layer, "api");
        assert_eq!(s.divergences[0].to_layer, "data");
        // api→domain realized; domain→data declared but absent.
        let absent = m.absences(&s.realized_pairs);
        assert_eq!(absent, vec![("domain".to_string(), "data".to_string())]);
    }
}
