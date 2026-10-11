//! Builds llama.cpp and mtmd from Vendor/llama.cpp (scripts/fetch-llama.sh puts the pinned, patched tag there) as static
//! libraries, and generates bindings for llama.h, mtmd.h and mtmd-helper.h.

use std::env;
use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let source = root.join("Vendor/llama.cpp");
    if !source.join("CMakeLists.txt").exists() {
        panic!(
            "llama.cpp isn't in {}: run scripts/fetch-llama.sh first",
            source.display()
        );
    }
    println!("cargo:rerun-if-changed={}", source.join("include/llama.h").display());
    println!(
        "cargo:rerun-if-changed={}",
        root.join("scripts/llama.cpp-no-logits.patch").display()
    );
    println!("cargo:rerun-if-changed=wrapper.h");

    let vulkan = env::var_os("CARGO_FEATURE_VULKAN").is_some();
    let cuda = env::var_os("CARGO_FEATURE_CUDA").is_some();
    let on_off = |on: bool| if on { "ON" } else { "OFF" };

    // Only the libraries: no tools, no server, nothing downloaded at build time. OpenMP off: ggml's own thread pool
    // then runs the CPU backend, and nothing extra needs linking.
    let out = cmake::Config::new(&source)
        .profile("Release")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("GGML_NATIVE", "OFF")
        .define("GGML_OPENMP", "OFF")
        .define("GGML_VULKAN", on_off(vulkan))
        .define("GGML_CUDA", on_off(cuda))
        .define("LLAMA_BUILD_MTMD", "ON")
        .define("LLAMA_BUILD_COMMON", "OFF")
        .define("LLAMA_BUILD_TOOLS", "OFF")
        .define("LLAMA_BUILD_SERVER", "OFF")
        .define("LLAMA_BUILD_EXAMPLES", "OFF")
        .define("LLAMA_BUILD_TESTS", "OFF")
        .define("LLAMA_BUILD_APP", "OFF")
        .define("LLAMA_BUILD_UI", "OFF")
        .define("LLAMA_OPENSSL", "OFF")
        .build_target("mtmd")
        .build();
    let build = out.join("build");

    for dir in [
        "src",
        "tools/mtmd",
        "ggml/src",
        "ggml/src/ggml-vulkan",
        "ggml/src/ggml-cuda",
        "vendor/hash",
    ] {
        println!("cargo:rustc-link-search=native={}", build.join(dir).display());
    }
    // Dependents before what they depend on: GNU ld resolves static libraries in one pass.
    let mut libs = vec!["mtmd", "llama", "ggml"];
    if vulkan {
        libs.push("ggml-vulkan");
    }
    if cuda {
        libs.push("ggml-cuda");
    }
    libs.extend(["ggml-cpu", "ggml-base", "vendor-hash"]);
    for lib in libs {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    if vulkan {
        println!("cargo:rustc-link-lib=vulkan");
    }
    if cuda {
        let cuda_root = env::var("CUDA_PATH").unwrap_or_else(|_| "/opt/cuda".into());
        println!("cargo:rustc-link-search=native={cuda_root}/lib64");
        for lib in ["cudart", "cublas", "cublasLt", "cuda"] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }
    println!("cargo:rustc-link-lib=stdc++");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", source.join("include").display()))
        .clang_arg(format!("-I{}", source.join("ggml/include").display()))
        .clang_arg(format!("-I{}", source.join("tools/mtmd").display()))
        .allowlist_function("(llama|mtmd|ggml_backend|ggml_log)_.*")
        .allowlist_type("(llama|mtmd)_.*")
        .allowlist_var("(LLAMA|MTMD)_.*")
        .prepend_enum_name(false)
        .generate()
        .expect("bindgen failed");
    bindings
        .write_to_file(PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs"))
        .unwrap();
}
