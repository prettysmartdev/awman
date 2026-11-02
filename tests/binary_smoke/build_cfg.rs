//! The CLI test gates must match the library's exact builtin target set.
use std::process::Command;

#[test]
fn builtin_cfg_uses_exact_supported_triples() {
    let tmp = tempfile::tempdir().unwrap();
    let build = tmp
        .path()
        .join(format!("cli-build{}", std::env::consts::EXE_SUFFIX));
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs");
    let compiled = Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(&build)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for (target, supported) in [
        ("aarch64-apple-darwin", true),
        ("aarch64-unknown-linux-gnu", true),
        ("x86_64-unknown-linux-gnu", true),
        ("aarch64-unknown-linux-musl", false),
        ("x86_64-unknown-linux-musl", false),
        ("x86_64-pc-windows-msvc", false),
    ] {
        for feature in [false, true] {
            let mut command = Command::new(&build);
            command
                .env("TARGET", target)
                .env_remove("CARGO_FEATURE_BUILTIN_RUNTIME");
            if feature {
                command.env("CARGO_FEATURE_BUILTIN_RUNTIME", "1");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{target}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let cfg = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                cfg.lines()
                    .any(|line| line == "cargo:rustc-cfg=awman_builtin"),
                supported && feature,
                "{target}, feature={feature}"
            );
        }
    }
}
