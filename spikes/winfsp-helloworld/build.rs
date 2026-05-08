//! Build script for the WinFsp hello-world spike.
//!
//! Tasks:
//! 1. Run bindgen against the WinFsp C headers to produce
//!    `bindings.rs` in OUT_DIR.
//! 2. Tell rustc to link against winfsp-x64.lib at the path the
//!    WinFsp installer puts it.
//! 3. Tell the MSVC linker to delay-load winfsp-x64.dll, matching
//!    WinFsp's expected load model.

use std::env;
use std::path::PathBuf;

fn main() {
    if !cfg!(target_os = "windows") {
        // Spike is Windows-only; nothing to do elsewhere.
        return;
    }

    // -- 1. Locate WinFsp ----------------------------------------------------
    // Prefer WINFSP_DIR env var; fall back to the conventional
    // installer path. (winget installs to "Program Files (x86)" on
    // 64-bit hosts because the WinFsp installer is a 32-bit MSI.)
    let winfsp_dir = env::var("WINFSP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\Program Files (x86)\WinFsp"));
    let inc_dir = winfsp_dir.join("inc");
    let lib_dir = winfsp_dir.join("lib");

    println!("cargo:rerun-if-env-changed=WINFSP_DIR");
    println!("cargo:rerun-if-changed=wrapper.h");

    // -- 2. Run bindgen ------------------------------------------------------
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", inc_dir.display()))
        // Match Windows headers' assumptions.
        .clang_arg("-DWIN32_LEAN_AND_MEAN")
        // Allowlist what we need; deny everything else so the
        // generated file stays small and auditable.
        .allowlist_function("Fsp.*")
        .allowlist_type("FSP_.*")
        .allowlist_var("FSP_.*")
        // Pull in the FILE_ATTRIBUTE_* / FILE_ACCESS_* / NTSTATUS
        // constants we need; bindgen would otherwise leave them out
        // because they live in Windows headers, not WinFsp itself.
        .allowlist_var("FILE_ATTRIBUTE_.*")
        .allowlist_var("FILE_GENERIC_.*")
        .allowlist_var("STATUS_.*")
        // Sane defaults.
        .derive_default(true)
        .layout_tests(false)
        .generate()
        .expect("bindgen failed to generate WinFsp bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs");
    bindings
        .write_to_file(&out_path)
        .expect("failed to write generated bindings");

    // -- 3. Linker setup -----------------------------------------------------
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=winfsp-x64");
    // WinFsp uses delay-loading so the binary doesn't fail to start
    // when winfsp-x64.dll isn't present (e.g., in CI). Matches what
    // the C samples do.
    println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll");
    println!("cargo:rustc-link-arg=delayimp.lib");
}
