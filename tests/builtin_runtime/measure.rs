//! Measurements, not assertions about performance: binary size, first/warm
//! startup, peak RSS and state-directory growth. No target is invented and an
//! unoptimized artifact is never labelled a release estimate. Opt in with
//! `AWMAN_TEST_BUILTIN_MEASURE=1`; results go to stderr and to the TSV named by
//! `AWMAN_TEST_BUILTIN_REPORT`. Point `AWMAN_TEST_BUILTIN_ARTIFACT` at the
//! optimized release binary to measure the shipped artifact.
//!
//! Realistic git/build workloads inside a guest need a workload image (git and a
//! toolchain) that the Alpine-based fixture does not carry: BLOCKED, see
//! test-coverage-runtime.md.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::binary::{awman, has_builtin_runtime, Scratch};

fn record(name: &str, value: &str) {
    eprintln!("MEASURE: {name} = {value}");
    if let Ok(path) = std::env::var("AWMAN_TEST_BUILTIN_REPORT") {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "MEASURE\t{name}\t{value}");
        }
    }
}

fn dir_bytes(path: &Path) -> u64 {
    let Ok(read) = std::fs::read_dir(path) else {
        return 0;
    };
    read.flatten()
        .map(|entry| match entry.metadata() {
            Ok(m) if m.is_dir() => dir_bytes(&entry.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Peak RSS (bytes) of all children reaped so far by this process.
fn children_max_rss_bytes() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is a valid, zeroed out-parameter for getrusage.
    let ok = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) };
    assert_eq!(ok, 0);
    let raw = usage.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        raw
    } else {
        raw * 1024
    }
}

#[test]
fn builtin_measure_size_startup_rss_and_disk_growth() {
    if !std::env::var("AWMAN_TEST_BUILTIN_MEASURE").is_ok_and(|v| v != "0") {
        eprintln!("SKIP: builtin measurements are opt-in (AWMAN_TEST_BUILTIN_MEASURE=1)");
        return;
    }
    let binary: PathBuf = std::env::var_os("AWMAN_TEST_BUILTIN_ARTIFACT")
        .map(PathBuf::from)
        .unwrap_or_else(awman);
    let named = std::env::var_os("AWMAN_TEST_BUILTIN_ARTIFACT").is_some();
    let release = named && binary.components().any(|c| c.as_os_str() == "release");
    let label = if release {
        "release"
    } else {
        "not a named release artifact: NOT a release estimate"
    };
    record("artifact", &format!("{} [{label}]", binary.display()));
    record(
        "size_bytes",
        &std::fs::metadata(&binary).unwrap().len().to_string(),
    );
    record(
        "builtin_runtime_included",
        &has_builtin_runtime().to_string(),
    );

    let scratch = Scratch::new();
    let run = |arg: &str| {
        let start = Instant::now();
        let status = scratch
            .command(&binary, Some("/usr/bin:/bin"))
            .arg(arg)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        (start.elapsed(), status.success())
    };
    let route = if has_builtin_runtime() {
        "__awman-builtin-info"
    } else {
        "--version"
    };
    let (first, ok) = run(route);
    assert!(ok);
    let mut warm: Vec<Duration> = (0..10).map(|_| run(route).0).collect();
    warm.sort();
    record("startup_first_ms", &first.as_millis().to_string());
    record(
        "startup_warm_median_ms",
        &warm[warm.len() / 2].as_millis().to_string(),
    );
    record(
        "peak_rss_bytes_children",
        &children_max_rss_bytes().to_string(),
    );

    if has_builtin_runtime() {
        let state = tempfile::Builder::new()
            .prefix("awb.")
            .tempdir_in("/tmp")
            .unwrap();
        let state_dir = state.path().join("s");
        crate::sqlite::config_home(&scratch);
        let before = dir_bytes(state.path());
        crate::sqlite::status_until_listed(&scratch, &state_dir).unwrap();
        let after_first = dir_bytes(state.path());
        crate::sqlite::status_until_listed(&scratch, &state_dir).unwrap();
        let after_second = dir_bytes(state.path());
        record("state_dir_bytes_empty", &before.to_string());
        record(
            "state_dir_bytes_after_first_start",
            &after_first.to_string(),
        );
        record(
            "state_dir_bytes_after_second_start",
            &after_second.to_string(),
        );
    }
}
