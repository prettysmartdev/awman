//! Making executables available inside a sandbox without a write-then-exec race.
//!
//! A file being written by one thread while another thread forks keeps an
//! inherited write descriptor open in the fork until it execs; running the
//! file then fails with `ETXTBSY` (text file busy). So test code never writes
//! an executable and runs it. Instead:
//!
//! 1. hard link the source when it is on the same filesystem; otherwise
//! 2. make a content-addressed copy once per process under
//!    `<tmproot>/awman-test-exe/<sha256>/` (write to a temp file, fsync,
//!    close, rename) and hard link that; and
//! 3. the supervisor retries `ETXTBSY` at spawn until the deadline.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// A stable, content-addressed copy of `src` on the sandbox filesystem
/// (created once per process). Returns its path.
pub fn install(src: &Path) -> PathBuf {
    static CACHE: OnceLock<Mutex<BTreeMap<PathBuf, PathBuf>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut cache = cache.lock().unwrap();
    if let Some(path) = cache.get(src) {
        if path.is_file() {
            return path.clone();
        }
    }
    let digest = crate::fixtures::sha256_file(src)
        .unwrap_or_else(|e| panic!("cannot read executable {}: {e}", src.display()));
    let runtime = crate::runtime_env::get();
    let base = runtime
        .tmproot
        .clone()
        .unwrap_or_else(|| runtime.system_temp.clone())
        .join("awman-test-exe")
        .join(&digest);
    std::fs::create_dir_all(&base)
        .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: {}: {e}", base.display()));
    let name = src.file_name().expect("executable has a file name");
    let target = base.join(name);
    if !target.is_file()
        || crate::fixtures::sha256_file(&target).ok().as_deref() != Some(digest.as_str())
    {
        atomic_copy(src, &target);
    }
    cache.insert(src.to_path_buf(), target.clone());
    target
}

/// Install `src` as `<dir>/<name>` (`<name>.exe` on Windows when `src` has
/// that extension): hard link when possible, else link or copy the
/// content-addressed copy from [`install`]. Returns the installed path.
pub fn install_as(src: &Path, dir: &Path, name: &str) -> PathBuf {
    let mut file = name.to_string();
    if cfg!(windows) && src.extension().is_some_and(|e| e == "exe") && !name.ends_with(".exe") {
        file.push_str(".exe");
    }
    let target = dir.join(file);
    if target.exists() {
        panic!("{} is already installed", target.display());
    }
    if std::fs::hard_link(src, &target).is_ok() {
        return target;
    }
    let stable = install(src);
    if std::fs::hard_link(&stable, &target).is_ok() {
        return target;
    }
    atomic_copy(&stable, &target);
    target
}

/// The executable `name` that cargo built with the running test binary.
///
/// Prefer the [`cargo_bin!`](crate::cargo_bin) macro, which supplies the
/// compile-time `CARGO_BIN_EXE_<name>` value as `compiled`.
///
/// `CARGO_BIN_EXE_<name>` is an absolute path baked in when the test is
/// compiled, and cargo does not rebuild the test when that path changes. A
/// target directory shared by checkouts mounted at different paths (the host
/// worktree and the same tree inside a container) therefore reuses a test
/// binary pointing into the other checkout. So the path is resolved at run
/// time instead: cargo and nextest place test binaries in
/// `<target>/<profile>/deps/` and the package's executables in
/// `<target>/<profile>/`. When that sibling does not exist (an unusual
/// layout) the compile-time path is used.
pub fn cargo_bin(name: &str, compiled: &str) -> PathBuf {
    let test_exe = std::env::current_exe().ok();
    resolve_cargo_bin(test_exe.as_deref(), name, Path::new(compiled))
}

/// [`cargo_bin`] with the running test binary passed in (no process access).
pub fn resolve_cargo_bin(test_exe: Option<&Path>, name: &str, compiled: &Path) -> PathBuf {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let sibling = test_exe
        .and_then(Path::parent)
        .filter(|deps| deps.file_name().is_some_and(|n| n == "deps"))
        .and_then(Path::parent)
        .map(|profile| profile.join(&file));
    match sibling {
        Some(path) if path.is_file() => path,
        _ => compiled.to_path_buf(),
    }
}

/// The path of an executable built with the tests, resolved at run time
/// (see [`exe::cargo_bin`](crate::exe::cargo_bin)). Use it instead of
/// `env!("CARGO_BIN_EXE_<name>")`: `cargo_bin!("awman")` is a `&'static Path`.
#[macro_export]
macro_rules! cargo_bin {
    ($name:literal) => {{
        static PATH: ::std::sync::OnceLock<::std::path::PathBuf> = ::std::sync::OnceLock::new();
        PATH.get_or_init(|| {
            $crate::exe::cargo_bin($name, ::std::env!(::std::concat!("CARGO_BIN_EXE_", $name)))
        })
        .as_path()
    }};
}

/// Write to a temp file in the target directory, fsync, close, set the
/// executable bit, then rename into place.
fn atomic_copy(src: &Path, target: &Path) {
    let dir = target.parent().expect("target has a parent");
    let mut temp = tempfile::Builder::new()
        .prefix(".install-")
        .tempfile_in(dir)
        .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: {}: {e}", dir.display()));
    let bytes = std::fs::read(src).unwrap_or_else(|e| panic!("{}: {e}", src.display()));
    temp.write_all(&bytes)
        .and_then(|_| temp.as_file().sync_all())
        .unwrap_or_else(|e| panic!("writing {}: {e}", target.display()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("chmod {}: {e}", target.display()));
    }
    let path = temp.into_temp_path();
    // Another process may have installed the same content meanwhile; rename
    // replaces it atomically with identical bytes.
    path.persist(target)
        .unwrap_or_else(|e| panic!("installing {}: {e}", target.display()));
    // Make sure no descriptor of ours stays open for writing.
    drop(File::open(target));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_bin_prefers_the_sibling_of_the_running_test_binary() {
        let dir = tempfile::tempdir().unwrap();
        let deps = dir.path().join("debug").join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        let test_exe = deps.join("gates-0123");
        let built = dir
            .path()
            .join("debug")
            .join(format!("xtask{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&built, b"").unwrap();
        let stale = Path::new("/elsewhere/target/debug/xtask");
        assert_eq!(resolve_cargo_bin(Some(&test_exe), "xtask", stale), built);
    }

    #[test]
    fn cargo_bin_falls_back_to_the_compiled_path() {
        let dir = tempfile::tempdir().unwrap();
        let compiled = Path::new("/compiled/target/debug/xtask");
        let deps = dir.path().join("debug").join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        // No sibling executable.
        let test_exe = deps.join("gates-0123");
        assert_eq!(
            resolve_cargo_bin(Some(&test_exe), "xtask", compiled),
            compiled
        );
        // A test binary outside a `deps` directory.
        let file = format!("xtask{}", std::env::consts::EXE_SUFFIX);
        std::fs::write(dir.path().join("debug").join(file), b"").unwrap();
        let loose = dir.path().join("debug").join("gates");
        assert_eq!(resolve_cargo_bin(Some(&loose), "xtask", compiled), compiled);
        // Unknown test binary.
        assert_eq!(resolve_cargo_bin(None, "xtask", compiled), compiled);
    }

    #[test]
    fn install_as_links_and_refuses_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("tool");
        std::fs::write(&src, b"#!/bin/sh\n").unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let installed = install_as(&src, &bin, "docker");
        assert_eq!(std::fs::read(&installed).unwrap(), b"#!/bin/sh\n");
        let again = std::panic::catch_unwind(|| install_as(&src, &bin, "docker"));
        assert!(again.is_err());
    }

    #[test]
    fn content_addressed_copy_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("payload");
        std::fs::write(&src, b"same bytes").unwrap();
        let a = install(&src);
        let b = install(&src);
        assert_eq!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"same bytes");
        let digest = crate::fixtures::sha256_file(&src).unwrap();
        assert!(a.parent().unwrap().ends_with(&digest));
    }
}
