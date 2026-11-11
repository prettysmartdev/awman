//! Records the compiler version and target triple for failure evidence
//! (`records/<id>.json` fields `toolchain` and `target`, WI 3.8).
//! Build scripts run at compile time, so reading Cargo's variables here is
//! not a read of the test process environment.

// A build script runs at compile time, not in a test process.
#![allow(clippy::disallowed_methods)]

use std::process::Command;

fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.split_whitespace().nth(1).map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=AWMAN_TEST_SUPPORT_TOOLCHAIN={version}");
    println!("cargo:rustc-env=AWMAN_TEST_SUPPORT_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
