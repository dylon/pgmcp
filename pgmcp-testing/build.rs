//! Mirror pgmcp's rpath setup so cross-crate test binaries
//! (`pgmcp-testing/tests/*.rs`) can find libonnxruntime.so, libcudart.so,
//! libcublasLt.so, libmkl_*.so at runtime.
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

    // Intel MKL libs. MKL uses a layered model where libmkl_intel_lp64.so
    // does NOT declare DT_NEEDED for libmkl_core.so / libmkl_sequential.so;
    // we must explicitly link them with --no-as-needed so the linker keeps
    // them in DT_NEEDED. Mirrors the parent crate's build.rs MKL block.
    if let Some(dir) = find_mkl_lib_dir() {
        println!("cargo:rustc-link-search=native={dir}");
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
        println!("cargo:rustc-link-arg=-Wl,--no-as-needed");
        println!("cargo:rustc-link-arg=-lmkl_core");
        println!("cargo:rustc-link-arg=-lmkl_sequential");
        println!("cargo:rustc-link-arg=-Wl,--as-needed");
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

fn find_mkl_lib_dir() -> Option<String> {
    if let Ok(output) = std::process::Command::new("pkg-config")
        .args(["--libs-only-L", "mkl-dynamic-lp64-seq"])
        .output()
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for token in stdout.split_whitespace() {
            if let Some(path) = token.strip_prefix("-L") {
                return Some(path.to_string());
            }
        }
    }
    for dir in [
        "/opt/intel/oneapi/mkl/latest/lib",
        "/opt/intel/oneapi/mkl/latest/lib/intel64",
    ] {
        let path = std::path::Path::new(dir);
        if path.join("libmkl_core.so.2").exists() || path.join("libmkl_core.so").exists() {
            return Some(dir.to_string());
        }
    }
    None
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
    if let Ok(p) = std::env::var("XDG_CACHE_HOME") {
        if !p.is_empty() {
            return Some(std::path::PathBuf::from(p));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(std::path::PathBuf::from(home).join(".cache"))
}
