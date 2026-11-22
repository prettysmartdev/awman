//! Failure evidence (WI 3.8, design 4.4).
//!
//! When a test that owns a [`crate::TestSandbox`] fails, blocks or leaks, the
//! sandbox writes `records/<id>.json` to `$AWMAN_TEST_EVIDENCE_DIR`, or to
//! `<sandbox>/evidence/` when that variable is unset (the sandbox is then
//! kept and its path printed). Captured process output is copied next to the
//! record so CI can upload one directory.

use serde::Serialize;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Once;

thread_local! {
    static NOTES: RefCell<BTreeMap<String, String>> = const { RefCell::new(BTreeMap::new()) };
    static LAST_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Add a field to the failure record of the current test (for example
/// `note("seed", …)` for a property test, or a VM owner ID). Call it from the
/// test's own thread; [`crate::TestSandbox::note`] works from any thread.
pub fn note(key: &str, value: &str) {
    NOTES.with(|n| {
        n.borrow_mut().insert(key.to_string(), value.to_string());
    });
}

pub(crate) fn take_thread_notes() -> BTreeMap<String, String> {
    NOTES.with(|n| std::mem::take(&mut *n.borrow_mut()))
}

/// `"<package>::<binary> <test path>"` for the test running on this thread
/// (libtest and nextest name the test thread after the test).
pub fn test_id() -> String {
    let path = std::thread::current()
        .name()
        .unwrap_or("unknown")
        .to_string();
    format_test_id(&path)
}

pub(crate) fn format_test_id(path: &str) -> String {
    format!("{}::{} {}", package(), binary(), path)
}

fn package() -> String {
    crate::runtime_env::get()
        .package
        .clone()
        .unwrap_or_else(|| "unknown".to_string())
}

/// The running test binary's target name (file stem without Cargo's hash).
pub(crate) fn binary() -> String {
    let stem = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "unknown".to_string());
    match stem.rsplit_once('-') {
        Some((name, hash)) if hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit()) => {
            name.to_string()
        }
        _ => stem,
    }
}

/// The command that reruns one test locally.
pub(crate) fn reproduce(test_path: &str) -> String {
    let package = package();
    let binary = binary();
    let target = if binary == package.replace('-', "_") {
        "--lib".to_string()
    } else {
        format!("--test {binary}")
    };
    format!("cargo nextest run -p {package} {target} -E 'test(={test_path})'")
}

/// Install (once per process) a panic hook that remembers the panic message
/// of each thread for its failure record, then calls the previous hook.
pub(crate) fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };
            let location = info
                .location()
                .map(|l| format!(" ({}:{})", l.file(), l.line()))
                .unwrap_or_default();
            LAST_PANIC.with(|p| *p.borrow_mut() = Some(format!("{message}{location}")));
            previous(info);
        }));
    });
}

pub(crate) fn last_panic() -> Option<String> {
    LAST_PANIC.with(|p| p.borrow().clone())
}

/// One supervised process in a failure record.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessRecord {
    /// Label given to the command.
    pub label: String,
    /// Process ID.
    pub pid: u32,
    /// Process-group ID (Unix) / pid (Windows job).
    pub pgid: i64,
    /// How it ended (`exit code 0`, `signal 9`, `killed at deadline`, `running`).
    pub exit: String,
    /// stdout file, relative to the record directory.
    pub stdout: String,
    /// stderr file, relative to the record directory.
    pub stderr: String,
}

/// `records/<id>.json`, schema 1 (design 4.4).
#[derive(Debug, Clone, Serialize)]
pub struct FailureRecord {
    /// Always 1.
    pub schema: u32,
    /// `<package>::<binary> <test path>`.
    pub test: String,
    /// Tier name.
    pub tier: String,
    /// Proptest seed (from `note("seed", …)`), if any.
    pub seed: Option<String>,
    /// Target triple the test was built for.
    pub target: String,
    /// rustc version.
    pub toolchain: String,
    /// Fixture ID → SHA-256 of every fixture used.
    pub fixtures: BTreeMap<String, String>,
    /// Sandbox root.
    pub sandbox: String,
    /// Supervised processes.
    pub processes: Vec<ProcessRecord>,
    /// VM owner IDs (from `note("vm", …)`).
    pub vms: Vec<String>,
    /// Capabilities granted to the test.
    pub capabilities: Vec<String>,
    /// Granted capabilities that were absent when re-probed after the failure.
    pub capabilities_lost: Vec<String>,
    /// Leaks found by the end-of-test check.
    pub leaks: Vec<crate::Leak>,
    /// The panic message, if the test panicked.
    pub panic: Option<String>,
    /// Single command reproducing the test locally.
    pub reproduce: String,
    /// Other notes.
    pub notes: BTreeMap<String, String>,
}

/// File-name-safe form of a test ID.
pub(crate) fn file_stem(test: &str) -> String {
    test.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Write `record` and copy the process output files next to it. Returns the
/// record path.
pub(crate) fn write_record(
    record: &mut FailureRecord,
    sandbox_root: &Path,
    evidence_dir: Option<&Path>,
) -> std::io::Result<PathBuf> {
    let records = match evidence_dir {
        Some(dir) => dir.join("records"),
        None => sandbox_root.join("evidence").join("records"),
    };
    let stem = file_stem(&record.test);
    let output_dir = records.join(&stem);
    std::fs::create_dir_all(&output_dir)?;
    for process in &mut record.processes {
        for file in [&mut process.stdout, &mut process.stderr] {
            let source = PathBuf::from(&*file);
            if let Some(name) = source.file_name() {
                let target = output_dir.join(name);
                let _ = std::fs::copy(&source, &target);
                *file = format!("{stem}/{}", name.to_string_lossy());
            }
        }
    }
    let path = records.join(format!("{stem}.json"));
    let text = serde_json::to_string_pretty(record).expect("record serializes");
    std::fs::write(&path, text)?;
    Ok(path)
}

pub(crate) fn toolchain() -> &'static str {
    env!("AWMAN_TEST_SUPPORT_TOOLCHAIN")
}

pub(crate) fn target() -> &'static str {
    env!("AWMAN_TEST_SUPPORT_TARGET")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_are_per_thread_and_taken_once() {
        note("seed", "42");
        let notes = take_thread_notes();
        assert_eq!(notes.get("seed").map(String::as_str), Some("42"));
        assert!(take_thread_notes().is_empty());
        std::thread::spawn(|| assert!(take_thread_notes().is_empty()))
            .join()
            .unwrap();
    }

    #[test]
    fn file_stems_are_safe() {
        assert_eq!(
            file_stem("awman-cli::component engine::git::x"),
            "awman-cli__component_engine__git__x"
        );
    }

    #[test]
    fn test_id_names_the_test_thread() {
        assert!(test_id().ends_with(" evidence::tests::test_id_names_the_test_thread"));
    }

    #[test]
    fn toolchain_and_target_are_known() {
        assert_ne!(toolchain(), "unknown");
        assert!(target().contains('-'));
    }

    #[test]
    fn write_record_copies_process_output() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("1.out");
        std::fs::write(&out, "hello").unwrap();
        let mut record = FailureRecord {
            schema: 1,
            test: "p::b t".into(),
            tier: "component".into(),
            seed: None,
            target: target().into(),
            toolchain: toolchain().into(),
            fixtures: BTreeMap::new(),
            sandbox: dir.path().display().to_string(),
            processes: vec![ProcessRecord {
                label: "x".into(),
                pid: 1,
                pgid: 1,
                exit: "exit code 1".into(),
                stdout: out.display().to_string(),
                stderr: dir.path().join("missing.err").display().to_string(),
            }],
            vms: vec![],
            capabilities: vec![],
            capabilities_lost: vec![],
            leaks: vec![],
            panic: Some("boom".into()),
            reproduce: reproduce("t"),
            notes: BTreeMap::new(),
        };
        let path = write_record(&mut record, dir.path(), None).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["schema"], 1);
        assert_eq!(json["panic"], "boom");
        assert_eq!(json["processes"][0]["stdout"], "p__b_t/1.out");
        let copied = path.parent().unwrap().join("p__b_t/1.out");
        assert_eq!(std::fs::read_to_string(copied).unwrap(), "hello");
    }
}
