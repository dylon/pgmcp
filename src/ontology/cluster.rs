//! Faceted (multi-aspect) clustering substrate.
//!
//! The coherence rule (SC-Taxo / multi-aspect taxonomy induction): concepts are
//! clustered **within a facet only**, so each hierarchy tree is single-aspect and
//! facets never contaminate one another. A concept that is genuinely two facets
//! is modeled as two concept entities linked by `part_of`, keeping every tree
//! single-aspect.
//!
//! [`facet_partition`] is the pure primitive that enforces this. The FCM call
//! over a facet's partition and the DB loader that fills [`ConceptVec`]s live with
//! their consumer, the Phase-4 hierarchy builder.

// facet_partition is consumed by the Phase-4 hierarchy builder.
#![allow(dead_code)]

use std::collections::HashMap;

use crate::ontology::facet::Facet;

/// A concept's embedding, tagged with its facet, ready for faceted clustering.
#[derive(Debug, Clone)]
pub struct ConceptVec {
    pub entity_id: i64,
    pub facet: Facet,
    /// BGE-M3 1024-d, L2-normalized (as stored on the concept's observation).
    pub embedding: Vec<f32>,
}

/// Partition concepts by facet — the multi-aspect coherence guarantee. Each
/// returned bucket contains only concepts of that one facet, so a downstream
/// clusterer produces a single-aspect tree per facet.
pub fn facet_partition(concepts: Vec<ConceptVec>) -> HashMap<Facet, Vec<ConceptVec>> {
    let mut out: HashMap<Facet, Vec<ConceptVec>> = HashMap::new();
    for c in concepts {
        out.entry(c.facet).or_default().push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cv(entity_id: i64, facet: Facet) -> ConceptVec {
        ConceptVec {
            entity_id,
            facet,
            embedding: vec![0.0; 4],
        }
    }

    #[test]
    fn partition_is_single_aspect() {
        let concepts = vec![
            cv(1, Facet::Algorithm),
            cv(2, Facet::Security),
            cv(3, Facet::Algorithm),
            cv(4, Facet::Security),
            cv(5, Facet::Algorithm),
        ];
        let parts = facet_partition(concepts);
        assert_eq!(parts.len(), 2, "two facets ⇒ two buckets");
        assert_eq!(parts[&Facet::Algorithm].len(), 3);
        assert_eq!(parts[&Facet::Security].len(), 2);
        for (facet, bucket) in &parts {
            assert!(
                bucket.iter().all(|c| c.facet == *facet),
                "no cross-facet contamination within a bucket"
            );
        }
    }

    #[test]
    fn empty_input_yields_empty_partition() {
        assert!(facet_partition(Vec::new()).is_empty());
    }
}
