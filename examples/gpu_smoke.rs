//! GPU smoke scenarios. Exercises `fuzzy_c_means_gpu` with each supported
//! precision on a synthetic two-blob dataset; asserts convergence +
//! correctness without requiring `#[ignore]` test-harness machinations.
//!
//! Invoked via the `cargo smoke` alias (`cargo run --release --example gpu_smoke`).
//! Exits non-zero iff any scenario fails; each scenario prints timing,
//! centroid-distance, iteration count, and Jaccard-vs-CPU where applicable.

fn main() -> std::process::ExitCode {
    cuda_impl::run_all()
}

mod cuda_impl {
    use std::time::Instant;

    use ndarray::Array2;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    pub fn run_all() -> std::process::ExitCode {
        println!("=== pgmcp GPU smoke scenarios ===");
        let mut failures = 0u32;

        failures += run("fp32_matches_cpu_jaccard", scenario_fp32_matches_cpu);
        failures += run("fp16_converges", scenario_fp16_converges);
        failures += run("bf16_converges", scenario_bf16_converges);
        failures += run(
            "fp16_fused_matches_cpu_jaccard",
            scenario_fp16_fused_matches_cpu,
        );
        failures += run(
            "bf16_fused_matches_cpu_jaccard",
            scenario_bf16_fused_matches_cpu,
        );

        if failures == 0 {
            println!("\n✓ all scenarios passed");
            std::process::ExitCode::SUCCESS
        } else {
            println!("\n✗ {} scenario(s) failed", failures);
            std::process::ExitCode::from(1)
        }
    }

    fn run(name: &str, scenario: fn() -> Result<String, String>) -> u32 {
        let start = Instant::now();
        print!("[{:<30}] ", name);
        match scenario() {
            Ok(summary) => {
                println!("PASS  ({:.2}s) {}", start.elapsed().as_secs_f64(), summary);
                0
            }
            Err(why) => {
                println!("FAIL  ({:.2}s) {}", start.elapsed().as_secs_f64(), why);
                1
            }
        }
    }

    /// Synthesize two well-separated Gaussian blobs in R^384 with a fixed seed
    /// for determinism.
    fn synth_two_blobs(n_per_blob: usize) -> Array2<f32> {
        const D: usize = 384;
        let n = n_per_blob * 2;
        let mut data = Array2::<f32>::zeros((n, D));
        let mut rng = StdRng::seed_from_u64(42);

        for i in 0..n_per_blob {
            for j in 0..D {
                let noise: f32 = rng.random_range(-0.1..0.1);
                data[[i, j]] = if j < D / 2 { 1.0 + noise } else { noise };
            }
        }
        for i in 0..n_per_blob {
            for j in 0..D {
                let noise: f32 = rng.random_range(-0.1..0.1);
                data[[n_per_blob + i, j]] = if j >= D / 2 { 1.0 + noise } else { noise };
            }
        }
        data
    }

    fn scenario_fp32_matches_cpu() -> Result<String, String> {
        use pgmcp::cron::topic_clustering::{fuzzy_c_means, fuzzy_c_means_gpu};
        use pgmcp::fcm::GpuPrecision;

        let data = synth_two_blobs(1000);

        let cpu = fuzzy_c_means(data.view(), 2, 2.0, 50, 1e-4, None);
        let gpu = fuzzy_c_means_gpu(data.view(), 2, 2.0, 50, 1e-4, GpuPrecision::Fp32);

        let jaccard = chunk_cluster_jaccard(&cpu.membership, &gpu.membership);
        if jaccard < 0.95 {
            return Err(format!(
                "Jaccard {:.3} < 0.95 (CPU iters {}, GPU iters {})",
                jaccard, cpu.iterations, gpu.iterations
            ));
        }
        Ok(format!(
            "jaccard={:.3} cpu_iters={} gpu_iters={}",
            jaccard, cpu.iterations, gpu.iterations
        ))
    }

    fn scenario_fp16_converges() -> Result<String, String> {
        use pgmcp::cron::topic_clustering::fuzzy_c_means_gpu;
        use pgmcp::fcm::GpuPrecision;

        let data = synth_two_blobs(1000);
        let result = fuzzy_c_means_gpu(data.view(), 2, 2.0, 50, 1e-4, GpuPrecision::Fp16);

        let dist = centroid_separation(&result.centroids);
        if dist < 0.5 {
            return Err(format!(
                "centroid distance {:.3} < 0.5 (iters {}, converged {})",
                dist, result.iterations, result.converged
            ));
        }
        if result.iterations == 0 {
            return Err("iterations=0 — GPU path did not run".to_string());
        }
        Ok(format!(
            "centroid_dist={:.3} iters={} converged={}",
            dist, result.iterations, result.converged
        ))
    }

    fn scenario_bf16_converges() -> Result<String, String> {
        use pgmcp::cron::topic_clustering::fuzzy_c_means_gpu;
        use pgmcp::fcm::GpuPrecision;

        let data = synth_two_blobs(1000);
        let result = fuzzy_c_means_gpu(data.view(), 2, 2.0, 50, 1e-4, GpuPrecision::Bf16);

        let dist = centroid_separation(&result.centroids);
        if dist < 0.5 {
            return Err(format!(
                "centroid distance {:.3} < 0.5 (iters {}, converged {})",
                dist, result.iterations, result.converged
            ));
        }
        if result.iterations == 0 {
            return Err("iterations=0 — GPU path did not run".to_string());
        }
        Ok(format!(
            "centroid_dist={:.3} iters={} converged={}",
            dist, result.iterations, result.converged
        ))
    }

    fn scenario_fp16_fused_matches_cpu() -> Result<String, String> {
        use pgmcp::cron::topic_clustering::{fuzzy_c_means, fuzzy_c_means_gpu};
        use pgmcp::fcm::GpuPrecision;

        let data = synth_two_blobs(1000);

        // Baseline: CPU FCM (fp32 via CudaFcmBackend's fp32 path, which routes
        // through the legacy f64 GpuFcm). Compare fp16 fused path to this.
        let cpu = fuzzy_c_means(data.view(), 2, 2.0, 50, 1e-4, None);
        let gpu = fuzzy_c_means_gpu(data.view(), 2, 2.0, 50, 1e-4, GpuPrecision::Fp16);

        let jaccard = chunk_cluster_jaccard(&cpu.membership, &gpu.membership);
        if jaccard < 0.90 {
            return Err(format!(
                "Jaccard {:.3} < 0.90 (CPU iters {}, GPU iters {})",
                jaccard, cpu.iterations, gpu.iterations
            ));
        }
        Ok(format!(
            "jaccard={:.3} cpu_iters={} gpu_iters={}",
            jaccard, cpu.iterations, gpu.iterations
        ))
    }

    fn scenario_bf16_fused_matches_cpu() -> Result<String, String> {
        use pgmcp::cron::topic_clustering::{fuzzy_c_means, fuzzy_c_means_gpu};
        use pgmcp::fcm::GpuPrecision;

        let data = synth_two_blobs(1000);

        let cpu = fuzzy_c_means(data.view(), 2, 2.0, 50, 1e-4, None);
        let gpu = fuzzy_c_means_gpu(data.view(), 2, 2.0, 50, 1e-4, GpuPrecision::Bf16);

        let jaccard = chunk_cluster_jaccard(&cpu.membership, &gpu.membership);
        if jaccard < 0.90 {
            return Err(format!(
                "Jaccard {:.3} < 0.90 (CPU iters {}, GPU iters {})",
                jaccard, cpu.iterations, gpu.iterations
            ));
        }
        Ok(format!(
            "jaccard={:.3} cpu_iters={} gpu_iters={}",
            jaccard, cpu.iterations, gpu.iterations
        ))
    }

    fn centroid_separation(centroids: &Array2<f32>) -> f32 {
        let c0 = centroids.row(0);
        let c1 = centroids.row(1);
        let diff = &c0 - &c1;
        diff.iter().map(|v| v * v).sum::<f32>().sqrt()
    }

    /// Jaccard similarity of argmax cluster assignments, handling label-permutation
    /// ambiguity (so {A↔0, B↔1} ~= {A↔1, B↔0}).
    fn chunk_cluster_jaccard(a: &Array2<f32>, b: &Array2<f32>) -> f32 {
        let n = a.nrows();
        let aa: Vec<usize> = (0..n).map(|i| argmax_row(&a.row(i))).collect();
        let bb: Vec<usize> = (0..n).map(|i| argmax_row(&b.row(i))).collect();

        let direct = agreement(&aa, &bb, |x| x);
        let inverted = agreement(&aa, &bb, |x| if x == 0 { 1 } else { 0 });
        direct.max(inverted)
    }

    fn argmax_row(row: &ndarray::ArrayView1<f32>) -> usize {
        let mut best = 0usize;
        let mut best_v = f32::NEG_INFINITY;
        for (i, &v) in row.iter().enumerate() {
            if v > best_v {
                best_v = v;
                best = i;
            }
        }
        best
    }

    fn agreement(a: &[usize], b: &[usize], map_b: impl Fn(usize) -> usize) -> f32 {
        let n = a.len();
        if n == 0 {
            return 1.0;
        }
        let matches = a
            .iter()
            .zip(b.iter())
            .filter(|&(&x, &y)| x == map_b(y))
            .count();
        matches as f32 / n as f32
    }
}
