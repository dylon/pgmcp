//! Trained defect-proneness model (graph-roadmap Phase 3.5).
//!
//! `bug_prediction` scored files with a hand-tuned weighted sum. This fits a
//! real **logistic regression** (Rahman & Devanbu, "How, and Why, Process
//! Metrics Are Better", ICSE 2013, feature family) from the project's own
//! history: features = structural/process metrics (churn, commits, authors,
//! in/out-degree, LOC) and label = "has the file been touched by a bug-fix
//! commit" (`fix_commit_ratio > 0`). `fix_commit_ratio` is deliberately NOT a
//! feature, so the model learns which *other* signals predict fix-proneness
//! rather than memorising the label.
//!
//! Hand-rolled (standardize → batch gradient descent → sigmoid), so it adds no
//! heavy ML dependency — matching the crate's dependency-minimization posture
//! (cf. the inlined RNG in `isolation_forest`). Pure: the caller supplies the
//! feature matrix + labels; no DB or network. `bug_prediction` falls back to
//! its heuristic score on cold start (too few of either class to fit).

/// A fitted logistic-regression defect model. Standardization statistics are
/// stored so `predict` can apply the same transform to new feature rows.
#[derive(Debug, Clone)]
pub struct LogisticModel {
    pub means: Vec<f64>,
    pub stds: Vec<f64>,
    pub weights: Vec<f64>,
    pub bias: f64,
    /// Training-set size and positive-label count, for transparency.
    pub n_samples: usize,
    pub n_positive: usize,
}

#[inline]
fn sigmoid(z: f64) -> f64 {
    1.0 / (1.0 + (-z).exp())
}

impl LogisticModel {
    /// Fit by batch gradient descent on standardized features. Returns `None`
    /// when the data can't support a fit (no features, mismatched lengths, or
    /// only one class present — the cold-start case the caller must handle).
    pub fn fit(features: &[Vec<f64>], labels: &[f64], iters: usize, lr: f64) -> Option<Self> {
        let n = features.len();
        if n == 0 || n != labels.len() {
            return None;
        }
        let d = features.iter().map(|r| r.len()).max().unwrap_or(0);
        if d == 0 {
            return None;
        }
        let n_positive = labels.iter().filter(|&&y| y >= 0.5).count();
        // Need both classes to learn a boundary.
        if n_positive == 0 || n_positive == n {
            return None;
        }

        // Standardization stats per feature.
        let mut means = vec![0.0; d];
        for row in features {
            for (j, m) in means.iter_mut().enumerate() {
                *m += row.get(j).copied().unwrap_or(0.0);
            }
        }
        for m in &mut means {
            *m /= n as f64;
        }
        let mut stds = vec![0.0; d];
        for row in features {
            for j in 0..d {
                let v = row.get(j).copied().unwrap_or(0.0) - means[j];
                stds[j] += v * v;
            }
        }
        for s in &mut stds {
            *s = (*s / n as f64).sqrt().max(1e-9);
        }

        // Pre-standardize the design matrix.
        let x: Vec<Vec<f64>> = features
            .iter()
            .map(|row| {
                (0..d)
                    .map(|j| (row.get(j).copied().unwrap_or(0.0) - means[j]) / stds[j])
                    .collect()
            })
            .collect();

        let mut weights = vec![0.0; d];
        let mut bias = 0.0;
        let lr = if lr > 0.0 { lr } else { 0.1 };
        for _ in 0..iters.max(1) {
            let mut grad_w = vec![0.0; d];
            let mut grad_b = 0.0;
            for (xi, &yi) in x.iter().zip(labels) {
                let z = bias + xi.iter().zip(&weights).map(|(a, b)| a * b).sum::<f64>();
                let err = sigmoid(z) - yi; // ∂loss/∂z for log-loss
                for j in 0..d {
                    grad_w[j] += err * xi[j];
                }
                grad_b += err;
            }
            let inv = 1.0 / n as f64;
            for j in 0..d {
                weights[j] -= lr * grad_w[j] * inv;
            }
            bias -= lr * grad_b * inv;
        }

        Some(LogisticModel {
            means,
            stds,
            weights,
            bias,
            n_samples: n,
            n_positive,
        })
    }

    /// Predicted defect-proneness probability in (0, 1) for a raw feature row.
    pub fn predict(&self, row: &[f64]) -> f64 {
        let z = self.bias
            + (0..self.weights.len())
                .map(|j| {
                    let v = (row.get(j).copied().unwrap_or(0.0) - self.means[j]) / self.stds[j];
                    v * self.weights[j]
                })
                .sum::<f64>();
        sigmoid(z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_a_separable_boundary() {
        // Feature 0 cleanly separates: low → class 0, high → class 1.
        let mut features = Vec::new();
        let mut labels = Vec::new();
        for i in 0..20 {
            features.push(vec![i as f64, (i % 2) as f64]); // f0 ramps, f1 is noise
            labels.push(if i >= 10 { 1.0 } else { 0.0 });
        }
        let model = LogisticModel::fit(&features, &labels, 2000, 0.3).expect("fits");
        assert_eq!(model.n_samples, 20);
        assert_eq!(model.n_positive, 10);
        // Predictions must separate the two classes.
        let lo = model.predict(&[1.0, 0.0]);
        let hi = model.predict(&[18.0, 0.0]);
        assert!(lo < 0.5, "low feature → class 0, got {lo}");
        assert!(hi > 0.5, "high feature → class 1, got {hi}");
        assert!(hi > lo);
    }

    #[test]
    fn single_class_yields_no_model() {
        let features = vec![vec![1.0], vec![2.0], vec![3.0]];
        let all_zero = vec![0.0, 0.0, 0.0];
        assert!(LogisticModel::fit(&features, &all_zero, 100, 0.1).is_none());
        let all_one = vec![1.0, 1.0, 1.0];
        assert!(LogisticModel::fit(&features, &all_one, 100, 0.1).is_none());
    }

    #[test]
    fn empty_or_mismatched_is_none() {
        assert!(LogisticModel::fit(&[], &[], 100, 0.1).is_none());
        assert!(LogisticModel::fit(&[vec![1.0]], &[0.0, 1.0], 100, 0.1).is_none());
    }
}
