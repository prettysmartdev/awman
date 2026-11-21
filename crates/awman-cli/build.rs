use std::env;

fn main() {
    if env::var("TARGET").is_ok_and(|target| target.ends_with("windows-msvc")) {
        // Async command dispatch has large debug futures. Match the main-thread
        // stack reserve used on Unix; Windows otherwise reserves only 1 MiB.
        println!("cargo:rustc-link-arg-bin=awman=/STACK:8388608");
    }
    println!("cargo:rustc-check-cfg=cfg(awman_builtin)");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BUILTIN_RUNTIME");

    if env::var_os("CARGO_FEATURE_BUILTIN_RUNTIME").is_none() {
        return;
    }

    // Keep exact target triples in sync with the workspace build.rs.
    let target = env::var("TARGET").expect("Cargo TARGET");
    let supported = matches!(
        target.as_str(),
        "aarch64-apple-darwin" | "aarch64-unknown-linux-gnu" | "x86_64-unknown-linux-gnu"
    );
    if supported {
        // The awman and host-shim dependency build scripts verify that the
        // payload exists and matches its pinned hashes for supported targets.
        println!("cargo:rustc-cfg=awman_builtin");
    }
}
