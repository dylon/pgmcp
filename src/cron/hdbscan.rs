//! In-tree HDBSCAN\* density clustering (Phase 2, embedding track) — the
//! canonical BERTopic clusterer.
//!
//! Runs on the **reduced** embedding space (PCA/UMAP output), where Euclidean
//! distance is meaningful — clustering raw 1024-d directly is what collapsed the
//! FCM engine (distance concentration). HDBSCAN\* needs no fixed K and marks
//! low-density points as noise, which structurally avoids the "every chunk in
//! every topic" smearing.
//!
//! Implemented in-tree (no new crate) to respect the project's brittle
//! `ndarray 0.16` / CUDA dependency pins. The pipeline is the standard
//! Campello/McInnes HDBSCAN\*:
//!
//! 1. core distance = distance to the `min_samples`-th nearest neighbor;
//! 2. mutual-reachability distance `mrd(a,b) = max(core_a, core_b, d(a,b))`;
//! 3. minimum spanning tree over `mrd` (Prim, O(n²) — fine on reduced dims);
//! 4. single-linkage dendrogram from the sorted MST;
//! 5. condense the dendrogram against `min_cluster_size`;
//! 6. select the flat clustering by Excess of Mass (stability).
//!
//! Returns one label per row: `>= 0` is a cluster id, `-1` is noise.

use ndarray::ArrayView2;

/// Cluster `data` (n × d, expected reduced dims) with HDBSCAN\*.
///
/// `min_cluster_size` is the smallest admissible cluster; `min_samples`
/// smooths the density estimate (larger → more points declared noise). Returns
/// a label per row (`-1` = noise).
pub fn hdbscan(data: ArrayView2<f32>, min_cluster_size: usize, min_samples: usize) -> Vec<i32> {
    let n = data.nrows();
    if n == 0 {
        return Vec::new();
    }
    let mcs = min_cluster_size.max(2);
    if n <= mcs {
        // Too few points to split — one cluster (or, if n<2, trivially cluster 0).
        return vec![0; n];
    }
    let ms = min_samples.max(1).min(n - 1);

    let dist = pairwise_dist(data);
    let core = core_distances(&dist, n, ms);
    let mst = prim_mst_mrd(&dist, &core, n);
    let slt = single_linkage(&mst, n);
    extract_eom(&slt, n, mcs)
}

/// Flattened n×n Euclidean distance matrix.
fn pairwise_dist(data: ArrayView2<f32>) -> Vec<f32> {
    let n = data.nrows();
    let d = data.ncols();
    let mut out = vec![0.0f32; n * n];
    for i in 0..n {
        for j in (i + 1)..n {
            let mut s = 0.0f32;
            for k in 0..d {
                let diff = data[[i, k]] - data[[j, k]];
                s += diff * diff;
            }
            let dij = s.sqrt();
            out[i * n + j] = dij;
            out[j * n + i] = dij;
        }
    }
    out
}

/// `core[i]` = distance from `i` to its `ms`-th nearest neighbor.
fn core_distances(dist: &[f32], n: usize, ms: usize) -> Vec<f32> {
    let mut core = vec![0.0f32; n];
    let mut row: Vec<f32> = Vec::with_capacity(n);
    for i in 0..n {
        row.clear();
        for j in 0..n {
            if j != i {
                row.push(dist[i * n + j]);
            }
        }
        row.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // ms-th nearest (1-indexed) → index ms-1.
        core[i] = row[(ms - 1).min(row.len() - 1)];
    }
    core
}

/// Prim's MST over the mutual-reachability graph. Returns n-1 edges
/// `(u, v, mrd)` sorted ascending by weight.
fn prim_mst_mrd(dist: &[f32], core: &[f32], n: usize) -> Vec<(usize, usize, f32)> {
    let mrd = |i: usize, j: usize| -> f32 { core[i].max(core[j]).max(dist[i * n + j]) };
    let mut in_tree = vec![false; n];
    let mut best = vec![f32::INFINITY; n];
    let mut parent = vec![usize::MAX; n];
    best[0] = 0.0;
    let mut edges: Vec<(usize, usize, f32)> = Vec::with_capacity(n.saturating_sub(1));
    for _ in 0..n {
        // Pick the non-tree node with the smallest connection cost.
        let mut u = usize::MAX;
        let mut bu = f32::INFINITY;
        for v in 0..n {
            if !in_tree[v] && best[v] < bu {
                bu = best[v];
                u = v;
            }
        }
        if u == usize::MAX {
            break;
        }
        in_tree[u] = true;
        if parent[u] != usize::MAX {
            edges.push((parent[u], u, best[u]));
        }
        for v in 0..n {
            if !in_tree[v] {
                let w = mrd(u, v);
                if w < best[v] {
                    best[v] = w;
                    parent[v] = u;
                }
            }
        }
    }
    edges.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
    edges
}

/// A single-linkage merge: two child node ids joined at `dist`, with the merged
/// `size`. Node ids `0..n` are points; `n + k` is the k-th internal node.
struct Merge {
    left: usize,
    right: usize,
    dist: f32,
    size: usize,
}

/// Build the single-linkage dendrogram from the sorted MST edges via union-find.
fn single_linkage(mst: &[(usize, usize, f32)], n: usize) -> Vec<Merge> {
    // Union-find with each set tracking its current top dendrogram-node id.
    let mut parent: Vec<usize> = (0..n).collect();
    let mut top: Vec<usize> = (0..n).collect();
    let mut size: Vec<usize> = vec![1; n];
    fn find(parent: &mut [usize], x: usize) -> usize {
        let mut r = x;
        while parent[r] != r {
            r = parent[r];
        }
        let mut c = x;
        while parent[c] != r {
            let nxt = parent[c];
            parent[c] = r;
            c = nxt;
        }
        r
    }
    let mut merges: Vec<Merge> = Vec::with_capacity(mst.len());
    for (k, &(u, v, w)) in mst.iter().enumerate() {
        let ru = find(&mut parent, u);
        let rv = find(&mut parent, v);
        if ru == rv {
            continue;
        }
        let new_id = n + k;
        merges.push(Merge {
            left: top[ru],
            right: top[rv],
            dist: w,
            size: size[ru] + size[rv],
        });
        // Union rv into ru.
        parent[rv] = ru;
        size[ru] += size[rv];
        top[ru] = new_id;
    }
    merges
}

/// Condense the dendrogram against `mcs` and extract the flat clustering by
/// Excess of Mass. Returns a label per point (`-1` = noise).
fn extract_eom(merges: &[Merge], n: usize, mcs: usize) -> Vec<i32> {
    if merges.is_empty() {
        return vec![0; n]; // single component, no splits
    }
    let total_nodes = n + merges.len();
    let root = total_nodes - 1;

    // Children + merge-distance for each internal node.
    let children = |node: usize| -> Option<(usize, usize, f32)> {
        if node < n {
            None
        } else {
            let m = &merges[node - n];
            Some((m.left, m.right, m.dist))
        }
    };
    let node_size = |node: usize| -> usize { if node < n { 1 } else { merges[node - n].size } };
    // All points beneath a node (iterative).
    let points_under = |node: usize| -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![node];
        while let Some(x) = stack.pop() {
            if x < n {
                out.push(x);
            } else {
                let m = &merges[x - n];
                stack.push(m.left);
                stack.push(m.right);
            }
        }
        out
    };

    // Condensed-cluster bookkeeping. Cluster ids are a fresh dense space.
    let mut next_cid = 0usize;
    let mut birth: Vec<f64> = Vec::new(); // lambda at which cluster appeared
    let mut parent_cid: Vec<Option<usize>> = Vec::new();
    // Per-cluster stability accumulator and the points that fell out of it.
    let mut stability: Vec<f64> = Vec::new();
    let mut fallout_points: Vec<Vec<usize>> = Vec::new(); // points that left cluster C
    // A macro (not a closure) so it expands inline — a closure would hold a
    // persistent mutable borrow of these Vecs, conflicting with the direct
    // `stability[cid]` / `fallout_points[cid]` indexing in the loop below.
    macro_rules! alloc_cluster {
        ($birth_lambda:expr, $par:expr) => {{
            let id = next_cid;
            next_cid += 1;
            birth.push($birth_lambda);
            parent_cid.push($par);
            stability.push(0.0);
            fallout_points.push(Vec::new());
            id
        }};
    }

    let root_cid = alloc_cluster!(0.0, None);
    // BFS over the dendrogram, carrying the condensed-cluster id each node belongs to.
    let mut queue: std::collections::VecDeque<(usize, usize)> = std::collections::VecDeque::new();
    queue.push_back((root, root_cid));
    while let Some((node, cid)) = queue.pop_front() {
        let Some((a, b, d)) = children(node) else {
            continue; // a bare point
        };
        let lambda = if d > 0.0 {
            1.0 / d as f64
        } else {
            f64::INFINITY
        };
        let (sa, sb) = (node_size(a), node_size(b));
        let a_big = sa >= mcs;
        let b_big = sb >= mcs;
        match (a_big, b_big) {
            (true, true) => {
                // True split: two new child clusters.
                for &child in &[a, b] {
                    let child_cid = alloc_cluster!(lambda, Some(cid));
                    queue.push_back((child, child_cid));
                }
            }
            (true, false) => {
                // Persistence: `a` continues as the same cluster; `b` falls out.
                for p in points_under(b) {
                    stability[cid] += lambda - birth[cid];
                    fallout_points[cid].push(p);
                }
                queue.push_back((a, cid));
            }
            (false, true) => {
                for p in points_under(a) {
                    stability[cid] += lambda - birth[cid];
                    fallout_points[cid].push(p);
                }
                queue.push_back((b, cid));
            }
            (false, false) => {
                // Cluster dies: all remaining points fall out here.
                for &child in &[a, b] {
                    for p in points_under(child) {
                        stability[cid] += lambda - birth[cid];
                        fallout_points[cid].push(p);
                    }
                }
            }
        }
    }

    // Excess of Mass: process clusters child-before-parent (ids are assigned in
    // BFS order, so descending id is a valid reverse-topological order).
    let n_clusters = next_cid;
    let mut child_ids: Vec<Vec<usize>> = vec![Vec::new(); n_clusters];
    for (c, par) in parent_cid.iter().enumerate() {
        if let Some(p) = *par {
            child_ids[p].push(c);
        }
    }
    let mut selected = vec![false; n_clusters];
    let mut subtree_stab = vec![0.0f64; n_clusters];
    for c in (0..n_clusters).rev() {
        let child_sum: f64 = child_ids[c].iter().map(|&ch| subtree_stab[ch]).sum();
        if child_ids[c].is_empty() || stability[c] > child_sum {
            selected[c] = true;
            // Deselect all descendants.
            let mut stack = child_ids[c].clone();
            while let Some(x) = stack.pop() {
                selected[x] = false;
                stack.extend(child_ids[x].iter().copied());
            }
            subtree_stab[c] = stability[c];
        } else {
            subtree_stab[c] = child_sum;
        }
    }
    // The root cluster is never itself a topic (it's "everything"); if it ended
    // up selected (no meaningful sub-structure), treat all as one cluster only
    // when there are no other selected clusters.
    let any_non_root_selected = (1..n_clusters).any(|c| selected[c]);
    if any_non_root_selected {
        selected[root_cid] = false;
    }

    // Assign labels: a point's label is the deepest SELECTED cluster it fell out
    // of or any selected ancestor thereof. Each point fell out of exactly one
    // cluster; walk up until a selected cluster is found.
    let mut labels = vec![-1i32; n];
    // Map selected cluster id → dense output label.
    let mut out_label = vec![-1i32; n_clusters];
    let mut next_out = 0i32;
    for c in 0..n_clusters {
        if selected[c] {
            out_label[c] = next_out;
            next_out += 1;
        }
    }
    for (c, pts) in fallout_points.iter().enumerate() {
        for &p in pts {
            // Walk up from c to the nearest selected cluster.
            let mut cur = Some(c);
            while let Some(cc) = cur {
                if selected[cc] {
                    labels[p] = out_label[cc];
                    break;
                }
                cur = parent_cid[cc];
            }
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array2;

    /// Deterministic SplitMix64 + Box–Muller (no RNG dep; reproducible).
    struct Rng(u64, Option<f64>);
    impl Rng {
        fn new(s: u64) -> Self {
            Rng(s, None)
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn gauss(&mut self) -> f64 {
            if let Some(v) = self.1.take() {
                return v;
            }
            let u1 = ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64).max(1e-12);
            let u2 = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
            let mag = (-2.0 * u1.ln()).sqrt();
            self.1 = Some(mag * (std::f64::consts::TAU * u2).sin());
            mag * (std::f64::consts::TAU * u2).cos()
        }
    }

    fn blob(rng: &mut Rng, cx: f32, cy: f32, k: usize, spread: f32, out: &mut Vec<[f32; 2]>) {
        for _ in 0..k {
            out.push([
                cx + rng.gauss() as f32 * spread,
                cy + rng.gauss() as f32 * spread,
            ]);
        }
    }

    fn to_arr(pts: &[[f32; 2]]) -> Array2<f32> {
        let n = pts.len();
        let mut a = Array2::<f32>::zeros((n, 2));
        for (i, p) in pts.iter().enumerate() {
            a[[i, 0]] = p[0];
            a[[i, 1]] = p[1];
        }
        a
    }

    #[test]
    fn recovers_three_well_separated_blobs() {
        let mut rng = Rng::new(7);
        let mut pts = Vec::new();
        blob(&mut rng, 0.0, 0.0, 40, 0.25, &mut pts);
        blob(&mut rng, 20.0, 0.0, 40, 0.25, &mut pts);
        blob(&mut rng, 0.0, 20.0, 40, 0.25, &mut pts);
        let a = to_arr(&pts);
        let labels = hdbscan(a.view(), 8, 5);
        let distinct: std::collections::HashSet<i32> =
            labels.iter().copied().filter(|&l| l >= 0).collect();
        assert_eq!(distinct.len(), 3, "expected 3 clusters, got {distinct:?}");
        // Most points should be clustered, not noise.
        let noise = labels.iter().filter(|&&l| l < 0).count();
        assert!(
            noise < pts.len() / 5,
            "too much noise: {noise}/{}",
            pts.len()
        );
        // Points in the same blob (contiguous 40-blocks) share a label.
        assert_eq!(labels[0], labels[39]);
        assert_eq!(labels[40], labels[79]);
        assert_ne!(labels[0], labels[40]);
    }

    #[test]
    fn single_blob_is_one_cluster() {
        let mut rng = Rng::new(3);
        let mut pts = Vec::new();
        blob(&mut rng, 5.0, 5.0, 50, 0.3, &mut pts);
        let labels = hdbscan(to_arr(&pts).view(), 10, 5);
        let distinct: std::collections::HashSet<i32> =
            labels.iter().copied().filter(|&l| l >= 0).collect();
        assert!(
            distinct.len() <= 1,
            "one blob → ≤1 cluster, got {distinct:?}"
        );
    }

    #[test]
    fn empty_and_tiny_inputs() {
        let empty = Array2::<f32>::zeros((0, 3));
        assert!(hdbscan(empty.view(), 5, 3).is_empty());
        let tiny = Array2::<f32>::from_shape_fn((3, 2), |(i, _)| i as f32);
        // n <= mcs → single cluster, no panic.
        let labels = hdbscan(tiny.view(), 5, 2);
        assert_eq!(labels.len(), 3);
    }

    #[test]
    fn two_blobs_plus_scattered_noise() {
        let mut rng = Rng::new(11);
        let mut pts = Vec::new();
        blob(&mut rng, 0.0, 0.0, 50, 0.2, &mut pts);
        blob(&mut rng, 15.0, 15.0, 50, 0.2, &mut pts);
        // Scattered uniform-ish noise far between.
        for _ in 0..10 {
            pts.push([
                rng.gauss() as f32 * 8.0 + 7.0,
                rng.gauss() as f32 * 8.0 + 7.0,
            ]);
        }
        let labels = hdbscan(to_arr(&pts).view(), 10, 5);
        let distinct: std::collections::HashSet<i32> =
            labels.iter().copied().filter(|&l| l >= 0).collect();
        assert_eq!(
            distinct.len(),
            2,
            "expected 2 dense clusters, got {distinct:?}"
        );
        // The two dense blobs should be (mostly) clustered.
        let blob0 = (0..50).filter(|&i| labels[i] >= 0).count();
        assert!(blob0 > 40, "blob 0 mostly clustered: {blob0}/50");
    }
}
