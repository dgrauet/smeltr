use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../metal-hook/src");
    println!("cargo:rerun-if-changed=../../metal-hook/include");
    println!("cargo:rerun-if-changed=../../metal-hook/Makefile");
    println!("cargo:rerun-if-env-changed=SMELTR_SKIP_DYLIB_BUILD");

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR must be inside the workspace")
        .to_path_buf();
    let metal_hook_dir = workspace_root.join("metal-hook");
    let dylib_src = metal_hook_dir.join("build").join("libmetal_hook.dylib");

    if std::env::var_os("SMELTR_SKIP_DYLIB_BUILD").is_none() {
        let status = Command::new("make")
            .arg("-C")
            .arg(&metal_hook_dir)
            .arg("all")
            .status()
            .expect("failed to invoke `make` for metal-hook");
        assert!(status.success(), "metal-hook build failed");
    }

    assert!(
        dylib_src.exists(),
        "metal-hook dylib not found at {} after build",
        dylib_src.display()
    );

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR not set by Cargo"));
    let dylib_dst = out_dir.join("libmetal_hook.dylib");
    std::fs::copy(&dylib_src, &dylib_dst).unwrap_or_else(|e| {
        panic!(
            "copy {} -> {}: {e}",
            dylib_src.display(),
            dylib_dst.display()
        )
    });
}
