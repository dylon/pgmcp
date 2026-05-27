//! Mirror pgmcp's BLAS link + rpath setup so cross-crate test binaries
//! (`pgmcp-testing/tests/*.rs`) link AOCL-BLIS (libblis-mt) and find
//! libonnxruntime.so, libcudart.so, libcublasLt.so at runtime.
//!
//! Cargo's `cargo:rustc-link-arg=-Wl,-rpath,...` directive applies only to
//! the package's own targets — it does not propagate transitively to crates
//! that depend on this one (or that this one depends on). pgmcp's build.rs
//! sets the right rpaths for its own `target/release/pgmcp` binary, but
//! pgmcp-testing's test binaries link against pgmcp without inheriting the
//! same rpaths. Re-emitting them here lets `cargo test -p pgmcp-testing`
//! succeed.

fn main() {
    // CUDA libs (libcudart.so, libcublasLt.so).
    if let Some(dir) = find_cuda_lib_dir() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    }

    // AOCL-BLIS (libblis-mt.so.5). System lib in /usr/lib (default loader
    // path). Mirrors the parent crate's build.rs link directive so test
    // binaries built here resolve cblas_* symbols. See build.rs in the
    // parent crate for why we use rustc-link-arg + --no-as-needed instead
    // of cargo:rustc-link-lib.
    let blis_pc = std::process::Command::new("pkg-config")
        .args(["--exists", "blis-mt"])
        .status();
    if matches!(blis_pc, Ok(s) if s.success()) {
        println!("cargo:rustc-link-arg=-Wl,--no-as-needed");
        println!("cargo:rustc-link-arg=-lblis-mt");
        println!("cargo:rustc-link-arg=-Wl,--as-needed");
    } else {
        panic!(
            "pgmcp-testing: AOCL-BLIS not found via pkg-config('blis-mt'). \
             Install aocl-blis."
        );
    }

    // ONNX Runtime, downloaded by the `ort` crate into ~/.cache/ort.pyke.io.
    if let Some(dir) = find_ort_lib_dir() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    }
}

fn find_cuda_lib_dir() -> Option<String> {
    let subpaths = [
        "lib",
        "lib/stubs",
        "lib64",
        "lib64/stubs",
        "lib/x86_64-linux-gnu",
        "targets/x86_64-linux/lib",
    ];
    for env in [
        "CUDA_HOME",
        "CUDA_PATH",
        "CUDA_ROOT",
        "CUDA_TOOLKIT_ROOT_DIR",
    ] {
        if let Ok(val) = std::env::var(env) {
            for sub in &subpaths {
                let dir = format!("{val}/{sub}");
                if has_cuda_libs(&dir) {
                    return Some(dir);
                }
            }
        }
    }
    for root in ["/usr", "/usr/local/cuda", "/opt/cuda", "/usr/lib/cuda"] {
        for sub in &subpaths {
            let dir = format!("{root}/{sub}");
            if has_cuda_libs(&dir) {
                return Some(dir);
            }
        }
    }
    None
}

fn has_cuda_libs(dir: &str) -> bool {
    ["libcublasLt.so", "libcublasLt.so.13", "libcublasLt.so.12"]
        .iter()
        .any(|name| std::path::Path::new(&format!("{dir}/{name}")).exists())
}

fn find_ort_lib_dir() -> Option<String> {
    let cache_root = dirs_cache_dir()?.join("ort.pyke.io").join("dfbin");
    if !cache_root.exists() {
        return None;
    }
    // Layout: dfbin/<triple>/<checksum>/onnxruntime/lib/libonnxruntime.so
    let triple_dir = cache_root.join("x86_64-unknown-linux-gnu");
    if !triple_dir.exists() {
        return None;
    }
    let entries = std::fs::read_dir(&triple_dir).ok()?;
    for entry in entries.flatten() {
        let lib = entry.path().join("onnxruntime").join("lib");
        if lib.join("libonnxruntime.so").exists() {
            return lib.to_str().map(String::from);
        }
    }
    None
}

/// Cargo build-script context has no `dirs` crate available without
/// declaring one; reimplement the bit we need.
fn dirs_cache_dir() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("XDG_CACHE_HOME")
        && !p.is_empty()
    {
        return Some(std::path::PathBuf::from(p));
    }
    let home = std::env::var("HOME").ok()?;
    Some(std::path::PathBuf::from(home).join(".cache"))
}
