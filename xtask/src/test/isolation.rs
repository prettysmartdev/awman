//! Isolation for the retained inline harnesses while their sandbox migration completes.
use anyhow::{ensure, Context, Result};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

type Snapshot = BTreeMap<PathBuf, (u64, Option<std::time::SystemTime>)>;
fn snapshot(root: &Path) -> Result<Snapshot> {
    fn walk(root: &Path, out: &mut Snapshot) -> Result<()> {
        if !root.exists() {
            return Ok(());
        }
        let meta = std::fs::symlink_metadata(root)?;
        out.insert(root.to_owned(), (meta.len(), meta.modified().ok()));
        if meta.is_dir() {
            for entry in std::fs::read_dir(root)? {
                walk(&entry?.path(), out)?;
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out)?;
    Ok(out)
}

fn inherited_input(name: &str) -> bool {
    [
        "PATH",
        "SystemRoot",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
        "LIBRARY_PATH",
        "CPATH",
        "CC",
        "CXX",
        "AR",
        "RUSTFLAGS",
        "RUSTDOCFLAGS",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "PYTHONHOME",
        "AWMAN_TEST_HOST_RESOURCES",
        "AWMAN_TEST_ARTIFACT",
        "AWMAN_TEST_ARTIFACT_SHA256",
        "AWMAN_TEST_EVIDENCE_DIR",
        "AWMAN_TEST_FIXTURES",
        "AWMAN_TEST_RUN_CONFIG",
        "CI",
    ]
    .contains(&name)
        || name.starts_with("CARGO_")
        || name.starts_with("RUSTUP_")
        || name.starts_with("PKG_CONFIG_")
        || name.starts_with("LLVM_")
}

pub struct Run {
    root: tempfile::TempDir,
    home: PathBuf,
    before: Snapshot,
}
impl Run {
    pub fn new() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is required for the run canary")?;
        Self::for_home(home)
    }
    fn for_home(home: PathBuf) -> Result<Self> {
        let before = snapshot(&home)?;
        let root = tempfile::Builder::new().prefix("awman-run-").tempdir_in(
            std::env::var_os("AWMAN_TEST_TMPROOT")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir),
        )?;
        for dir in ["home", "awman", "tmp", "config", "data", "cache"] {
            std::fs::create_dir(root.path().join(dir))?;
        }
        Ok(Self { root, home, before })
    }
    pub fn apply(&self, cmd: &mut Command) {
        let tool_path = cmd
            .get_envs()
            .find(|(key, _)| *key == "PATH")
            .and_then(|(_, value)| value.map(std::ffi::OsStr::to_owned));
        // A retained inline test must not inherit the operator's API tokens,
        // credential-helper overrides, remote daemon address, or agent config.
        // These are build/runtime inputs and explicitly granted test resources.
        cmd.env_clear();
        for (key, value) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if inherited_input(&name) {
                cmd.env(key, value);
            }
        }
        if let Some(path) = tool_path {
            cmd.env("PATH", path);
        }
        cmd.env("AWMAN_TEST_ISOLATION", "1");
        // Keep the build tools outside HOME, so changing HOME does not redownload Rust.
        for (key, default) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
            cmd.env(
                key,
                std::env::var_os(key)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.home.join(default)),
            );
        }
        for (key, dir) in [
            ("HOME", "home"),
            ("USERPROFILE", "home"),
            ("AWMAN_HOME", "awman"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("TMPDIR", "tmp"),
            ("TMP", "tmp"),
            ("TEMP", "tmp"),
            ("AWMAN_TEST_TMPROOT", "tmp"),
        ] {
            cmd.env(key, self.root.path().join(dir));
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1").env(
            "GIT_CONFIG_GLOBAL",
            self.root.path().join("home/.gitconfig"),
        );
    }
    pub fn finish(self) -> Result<()> {
        let leaks = cleanup_owned_processes(self.root.path());
        let after = snapshot(&self.home)?;
        let changed: std::collections::BTreeSet<_> = self
            .before
            .keys()
            .chain(after.keys())
            .filter(|path| self.before.get(*path) != after.get(*path))
            .collect();
        ensure!(
            changed.is_empty(),
            "HOME canary changed during test run: {changed:?}"
        );
        ensure!(
            leaks.is_empty(),
            "run-owned processes survived their test deadline and were killed: {leaks:?}"
        );
        self.root
            .close()
            .context("remove run-owned test directories")
    }
}

/// Process identity is the Linux creation time plus an inherited private root.
/// Native host containment remains a separate runner admission requirement.
#[cfg(target_os = "linux")]
pub(crate) fn cleanup_owned_processes(root: &Path) -> Vec<u32> {
    let mut leaks = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return leaks;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let path = entry.path();
        let identity = || -> Option<String> {
            let stat = std::fs::read_to_string(path.join("stat")).ok()?;
            let fields: Vec<_> = stat.rsplit_once(") ")?.1.split_whitespace().collect();
            if fields.first() == Some(&"Z") {
                return None;
            }
            Some(fields.get(19)?.to_string())
        };
        let Some(start) = identity() else { continue };
        let Ok(env) = std::fs::read(path.join("environ")) else {
            continue;
        };
        let owned = env.split(|b| *b == 0).any(|value| {
            [
                b"HOME=".as_slice(),
                b"TMPDIR=".as_slice(),
                b"AWMAN_HOME=".as_slice(),
            ]
            .iter()
            .any(|key| {
                value
                    .strip_prefix(*key)
                    .and_then(|p| std::str::from_utf8(p).ok())
                    .is_some_and(|p| Path::new(p).starts_with(root))
            })
        });
        if owned && identity().as_deref() == Some(start.as_str()) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            leaks.push(pid);
        }
    }
    leaks
}
#[cfg(not(target_os = "linux"))]
pub(crate) fn cleanup_owned_processes(_root: &Path) -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canary_detects_changed_content_and_new_paths() {
        let d = tempfile::tempdir().unwrap();
        let before = snapshot(d.path()).unwrap();
        std::fs::write(d.path().join(".gitconfig"), "changed").unwrap();
        assert_ne!(before, snapshot(d.path()).unwrap());
    }
    #[test]
    fn isolation_preserves_manifest_resource_inputs_and_ci_but_not_host_credentials() {
        let manifest = crate::manifest::load(&crate::gates::root()).unwrap();
        for key in [
            "fixtures",
            "run_config",
            "host_resources",
            "artifact",
            "artifact_sha256",
            "evidence",
        ] {
            assert!(
                inherited_input(&manifest.test_env[key]),
                "lost declared input {key}"
            );
        }
        assert!(inherited_input("CI"));
        for secret in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "SSH_AUTH_SOCK",
            "DOCKER_HOST",
            "CLAUDE_CONFIG_DIR",
            "INSTA_UPDATE",
        ] {
            assert!(
                !inherited_input(secret),
                "ambient host input {secret} leaked"
            );
        }
    }

    #[test]
    fn child_environment_uses_private_roots_and_retains_toolchain_paths() {
        let home = tempfile::tempdir().unwrap();
        let run = Run::for_home(home.path().to_owned()).unwrap();
        let mut c = Command::new("cargo");
        c.env("PATH", "/pinned-tools:/system-tools");
        run.apply(&mut c);
        let vars: BTreeMap<_, _> = c.get_envs().map(|(k, v)| (k, v.unwrap())).collect();
        assert_ne!(vars[std::ffi::OsStr::new("HOME")], run.home.as_os_str());
        assert_eq!(
            vars[std::ffi::OsStr::new("PATH")],
            "/pinned-tools:/system-tools"
        );
        assert!(Path::new(vars[std::ffi::OsStr::new("AWMAN_TEST_TMPROOT")])
            .starts_with(run.root.path()));
        assert_eq!(vars[std::ffi::OsStr::new("AWMAN_TEST_ISOLATION")], "1");
        for secret in ["GITHUB_TOKEN", "GH_TOKEN", "SSH_AUTH_SOCK", "DOCKER_HOST"] {
            assert!(!vars.contains_key(std::ffi::OsStr::new(secret)));
        }
        run.finish().unwrap();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn run_sweep_reaps_only_its_own_leaked_child() {
        let home = tempfile::tempdir().unwrap();
        let run = Run::for_home(home.path().to_owned()).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf 'ready\\n'; exec /bin/sleep 60"])
            .stdout(std::process::Stdio::piped());
        run.apply(&mut command);
        let mut leaked = command.spawn().unwrap();
        // spawn() can return before exec has installed the private environment.
        // Exercise the scanner only after this deliberately leaked child proves
        // it has entered that environment; no scheduler-delay assumption.
        let mut ready = [0u8; 6];
        std::io::Read::read_exact(&mut leaked.stdout.take().unwrap(), &mut ready).unwrap();
        assert_eq!(&ready, b"ready\n");
        let result = run.finish();
        if result.is_ok() {
            // A seeded sweep regression must fail this assertion without
            // leaving the deliberately leaked fixture alive after the test.
            let _ = leaked.kill();
            let _ = leaked.wait();
        }
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("processes survived"));
        assert!(!leaked.wait().unwrap().success());
    }
}
