/// Build script: link AOCL-BLIS (BLAS) for ndarray's CPU fallback, emit
/// RUNPATH for the CUDA toolkit so the binary can resolve libcudart.so /
/// libcublasLt.so at runtime, and compile the fused-reduction PTX kernels
/// (src/fcm/cuda/kernels.cu) with nvcc.
fn main() {
    // AOCL-BLIS (libblis-mt.so.5) provides cblas_sgemm/dgemm/sdot/ddot for
    // ndarray's `blas` feature. The lib lives in /usr/lib (default loader
    // path), so no rpath/RUNPATH is needed — just the link directive.
    // Probe via pkg-config to fail loudly if aocl-blis isn't installed.
    let blis_pc = std::process::Command::new("pkg-config")
        .args(["--exists", "blis-mt"])
        .status();
    match blis_pc {
        Ok(s) if s.success() => {
            // We use `rustc-link-arg` (not `rustc-link-lib`) so the `-l`
            // flag is appended at the end of the rust-lld command line,
            // after every rlib that might reference cblas_sgemm. Wrapping
            // with `--no-as-needed` forces DT_NEEDED for libblis-mt even
            // though no symbols from it are referenced ahead of the lib
            // in scan order — without it, rust-lld silently drops the
            // library and emits "undefined symbol: cblas_sgemm".
            println!("cargo:rustc-link-arg=-Wl,--no-as-needed");
            println!("cargo:rustc-link-arg=-lblis-mt");
            println!("cargo:rustc-link-arg=-Wl,--as-needed");
            println!("cargo:warning=pgmcp: linking AOCL-BLIS (blis-mt)");
        }
        _ => panic!(
            "pgmcp: AOCL-BLIS not found via pkg-config('blis-mt'). \
             Install aocl-blis (Arch: pacman -S aocl-blis)."
        ),
    }

    // CUDA toolkit is mandatory. Cudarc's build.rs emits link-search +
    // link-lib directives for libcudart/libcublas/libcublasLt but does not
    // embed an rpath — so we add one for runtime library resolution.
    match find_cuda_lib_dir() {
        Some(dir) => {
            println!("cargo:rustc-link-search=native={dir}");
            println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
            println!("cargo:warning=pgmcp: selected CUDA lib dir {dir}");
        }
        None => {
            panic!(
                "pgmcp: no CUDA lib dir found with both libcudart.so and \
                 libcublasLt.so present. Set CUDA_HOME / CUDA_PATH / CUDA_ROOT \
                 / CUDA_TOOLKIT_ROOT_DIR, or install the CUDA toolkit at \
                 /usr/local/cuda, /opt/cuda, /usr/lib/cuda, or /usr."
            );
        }
    }

    // candle uses cudarc directly; no ONNX Runtime to RUNPATH-embed.
    // Re-run when Cargo.lock changes (e.g. on candle version bump).
    println!("cargo:rerun-if-changed=Cargo.lock");

    // Compile src/fcm/cuda/kernels.cu → $OUT_DIR/fcm_kernels.ptx via nvcc.
    //
    // Target is compute_89 (RTX 4060 Ti — Ada Lovelace). The resulting PTX
    // is loaded at runtime and JIT-compiled to sm_89 SASS by the driver.
    // Baking PTX (not cubin) into the binary means upgrading CUDA drivers
    // doesn't require a rebuild.
    let cu_src = "src/fcm/cuda/kernels.cu";
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let ptx_out = format!("{out_dir}/fcm_kernels.ptx");
    println!("cargo:rerun-if-changed={cu_src}");

    let status = std::process::Command::new("nvcc")
        .args([
            "-ptx",
            "-std=c++20",
            "--gpu-architecture=compute_89",
            "-O3",
            "--use_fast_math",
            "-o",
            &ptx_out,
            cu_src,
        ])
        .status()
        .expect("nvcc not found on PATH — CUDA toolkit is mandatory");
    if !status.success() {
        panic!("nvcc failed to compile {cu_src} (exit {:?})", status.code());
    }
    println!("cargo:warning=pgmcp: compiled fcm_kernels.ptx → {ptx_out}");
}

/// Find a directory that contains BOTH `libcudart.so` and `libcublasLt.so`.
///
/// Requiring both libs in the same directory prevents an rpath split where
/// cudart lives in one dir and cublasLt lives in another — if that happens,
/// cudarc's link-search and ours can disagree, yielding subtle runtime load
/// failures on one of the two libraries.
///
/// Search order mirrors cudarc 0.19.4's `build.rs`:
///   1. env-var pinned paths: `CUDA_HOME`, `CUDA_PATH`, `CUDA_ROOT`, `CUDA_TOOLKIT_ROOT_DIR`
///   2. well-known roots: `/usr`, `/usr/local/cuda`, `/opt/cuda`, `/usr/lib/cuda`
///
/// For each root, each of these subpaths is probed:
/// `lib`, `lib/stubs`, `lib64`, `lib64/stubs`, `lib/x86_64-linux-gnu`,
/// `targets/x86_64-linux/lib`, `targets/x86_64-linux/lib/stubs`
fn find_cuda_lib_dir() -> Option<String> {
    let subpaths = [
        "lib",
        "lib/stubs",
        "lib64",
        "lib64/stubs",
        "lib/x86_64-linux-gnu",
        "targets/x86_64-linux/lib",
        "targets/x86_64-linux/lib/stubs",
    ];

    let env_vars = [
        "CUDA_HOME",
        "CUDA_PATH",
        "CUDA_ROOT",
        "CUDA_TOOLKIT_ROOT_DIR",
    ];
    for env in &env_vars {
        if let Ok(val) = std::env::var(env) {
            for sub in &subpaths {
                let dir = format!("{val}/{sub}");
                if has_both_cuda_libs(&dir) {
                    return Some(dir);
                }
            }
        }
    }

    let roots = ["/usr", "/usr/local/cuda", "/opt/cuda", "/usr/lib/cuda"];
    for root in &roots {
        for sub in &subpaths {
            let dir = format!("{root}/{sub}");
            if has_both_cuda_libs(&dir) {
                return Some(dir);
            }
        }
    }

    None
}

/// True iff `dir` contains both `libcudart.so` (or a versioned form) and
/// `libcublasLt.so` (or a versioned form). Some distros ship only the
/// versioned symlinks (e.g. `libcudart.so.13`), so we probe the unversioned
/// symlink plus the `.so.13` / `.so.12` variants.
fn has_both_cuda_libs(dir: &str) -> bool {
    let has_cudart = ["libcudart.so", "libcudart.so.13", "libcudart.so.12"]
        .iter()
        .any(|name| std::path::Path::new(&format!("{dir}/{name}")).exists());
    let has_cublaslt = ["libcublasLt.so", "libcublasLt.so.13", "libcublasLt.so.12"]
        .iter()
        .any(|name| std::path::Path::new(&format!("{dir}/{name}")).exists());
    has_cudart && has_cublaslt
}
