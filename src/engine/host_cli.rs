//! The container and sandbox CLIs awman drives — `docker`, Apple's
//! `container`, and `sbx` — resolved through test isolation.
//!
//! Every spawn of one of them passes its program name through [`program`].
//! Resolution uses the instance's PATH/cwd/HOME/PATHEXT. Under test isolation the real CLI is
//! off unless its opt-in variable is set (`AWMAN_TEST_DOCKER`,
//! `AWMAN_TEST_APPLE_CONTAINER`, `AWMAN_TEST_SBX`): the name resolves to a path
//! that does not exist, so awman sees exactly what it sees when the CLI is not
//! installed. The container tests would otherwise build, run and remove
//! images, containers and sandboxes in the developer's own daemon.
//!
//! A fake a test put on `PATH` inside its own temp directory is not the
//! developer's CLI, so it is always allowed.

use std::path::Path;

use crate::data::config::env::{Env, EnvSnapshot};

/// Resolve only against the instance's PATH and working directory. A missing
/// executable stays missing even if the daemon has one on its own PATH.
pub fn program_in(env: &EnvSnapshot, name: &str) -> std::path::PathBuf {
    let cwd = env.cwd();
    let found = find_program(env, name, env.path_dirs());
    let opted_in = match name {
        "docker" => env.test_docker(),
        "container" => env.test_apple_container(),
        "sbx" => env.test_sbx(),
        _ => true,
    };
    let fake = found.as_ref().is_some_and(|p| {
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        canonical(p).starts_with(canonical(&env.temp_dir()))
    });
    if (cfg!(test) || env.test_isolation()) && !opted_in && !fake {
        return Path::new("/nonexistent/awman-test-isolation").join(name);
    }
    found.unwrap_or_else(|| {
        let p = Path::new(name);
        if p.is_absolute() {
            p.to_path_buf()
        } else if p.components().count() > 1 {
            cwd.join(p)
        } else {
            Path::new("/nonexistent/awman-missing-program").join(p)
        }
    })
}

/// `which` also consults HOME and (on Windows) PATHEXT. Bind those reads as
/// well as PATH/cwd, rather than leaving a hidden ambient-environment fallback.
pub(crate) fn find_program(
    env: &EnvSnapshot,
    name: &str,
    paths: Vec<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    which::WhichConfig::new_with_sys(SnapshotSystem(env))
        .custom_path_list(std::env::join_paths(paths).ok()?)
        .custom_cwd(env.cwd().to_path_buf())
        .binary_name(name.into())
        .first_result()
        .ok()
}

struct SnapshotSystem<'a>(&'a EnvSnapshot);
impl which::sys::Sys for SnapshotSystem<'_> {
    type ReadDirEntry = <which::sys::RealSys as which::sys::Sys>::ReadDirEntry;
    type Metadata = <which::sys::RealSys as which::sys::Sys>::Metadata;
    fn is_windows(&self) -> bool {
        cfg!(windows)
    }
    fn current_dir(&self) -> std::io::Result<std::path::PathBuf> {
        Ok(self.0.cwd().to_path_buf())
    }
    fn home_dir(&self) -> Option<std::path::PathBuf> {
        self.0.home_dir()
    }
    fn env_split_paths(&self, paths: &std::ffi::OsStr) -> Vec<std::path::PathBuf> {
        std::env::split_paths(paths).collect()
    }
    fn env_path(&self) -> Option<std::ffi::OsString> {
        self.0.get_os("PATH").map(Into::into)
    }
    fn env_path_ext(&self) -> Option<std::ffi::OsString> {
        self.0.get_os("PATHEXT").map(Into::into)
    }
    fn metadata(&self, path: &Path) -> std::io::Result<Self::Metadata> {
        which::sys::RealSys.metadata(path)
    }
    fn symlink_metadata(&self, path: &Path) -> std::io::Result<Self::Metadata> {
        which::sys::RealSys.symlink_metadata(path)
    }
    fn read_dir(
        &self,
        path: &Path,
    ) -> std::io::Result<Box<dyn Iterator<Item = std::io::Result<Self::ReadDirEntry>>>> {
        which::sys::RealSys.read_dir(path)
    }
    fn is_valid_executable(&self, path: &Path) -> std::io::Result<bool> {
        which::sys::RealSys.is_valid_executable(path)
    }
}

/// Resolve a host program for an explicit instance snapshot.
pub fn program(env: &EnvSnapshot, name: &str) -> std::path::PathBuf {
    program_in(env, name)
}

/// The single host command constructor. Every instance child receives an
/// explicit environment, including PATH, and never inherits daemon variables.
pub fn command(env: &EnvSnapshot, name: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program(env, name));
    command.env_clear().envs(env.spawn_env());
    if !env.cwd().as_os_str().is_empty() {
        command.current_dir(env.cwd());
    }
    command
}

pub(crate) fn pty_command(env: &EnvSnapshot, name: &str) -> portable_pty::CommandBuilder {
    let mut command = portable_pty::CommandBuilder::new(program(env, name));
    command.env_clear();
    for (key, value) in env.spawn_env() {
        command.env(key, value);
    }
    if !env.cwd().as_os_str().is_empty() {
        command.cwd(env.cwd());
    }
    command
}

/// Compatibility capture for builtin constructors. Instance hosts supply their
/// snapshot explicitly through the corresponding `*_in` constructors.
pub fn process_env() -> EnvSnapshot {
    Env::from_process()
}

// Runtime CLI calls in this scope poll the cancellation token and reap their process group.
thread_local! {
    static COLLECTION_CANCEL: std::cell::RefCell<Option<tokio_util::sync::CancellationToken>> = const { std::cell::RefCell::new(None) };
}

pub fn with_collection_cancel<T>(
    cancel: tokio_util::sync::CancellationToken,
    f: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            COLLECTION_CANCEL.with(|c| *c.borrow_mut() = None);
        }
    }
    COLLECTION_CANCEL.with(|c| *c.borrow_mut() = Some(cancel));
    let _reset = Reset;
    f()
}

pub fn check_collection_cancel() -> Result<(), crate::engine::error::EngineError> {
    if COLLECTION_CANCEL.with(|c| c.borrow().as_ref().is_some_and(|c| c.is_cancelled())) {
        Err(crate::engine::error::EngineError::Other(
            "collection cancelled".into(),
        ))
    } else {
        Ok(())
    }
}

/// Bounded output capture without pipe-reader threads that descendants can hold open.
/// Cancellation kills and reaps the local CLI process group, never the agent.
pub(crate) fn checked_output(
    command: &mut std::process::Command,
    binary: &str,
) -> Result<std::process::Output, crate::engine::error::EngineError> {
    use crate::engine::error::EngineError;
    use std::io::{Read, Seek};
    check_collection_cancel()?;
    let mut stdout = tempfile::tempfile().map_err(|e| EngineError::Container(e.to_string()))?;
    let mut stderr = tempfile::tempfile().map_err(|e| EngineError::Container(e.to_string()))?;
    command
        .stdin(std::process::Stdio::null())
        .stdout(
            stdout
                .try_clone()
                .map_err(|e| EngineError::Container(e.to_string()))?,
        )
        .stderr(
            stderr
                .try_clone()
                .map_err(|e| EngineError::Container(e.to_string()))?,
        );
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::ContainerRuntimeUnavailable {
                binary: binary.into(),
            }
        } else {
            EngineError::Container(format!("{binary}: {e}"))
        }
    })?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        let error = check_collection_cancel().err().or_else(|| {
            (std::time::Instant::now() >= deadline)
                .then(|| EngineError::Container(format!("{binary} timed out")))
        });
        if let Some(error) = error {
            #[cfg(unix)]
            {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(child.id() as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(EngineError::Container(e.to_string()));
            }
        }
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout
        .rewind()
        .and_then(|_| stdout.read_to_end(&mut out))
        .map_err(|e| EngineError::Container(e.to_string()))?;
    stderr
        .rewind()
        .and_then(|_| stderr.read_to_end(&mut err))
        .map_err(|e| EngineError::Container(e.to_string()))?;
    if !status.success() {
        return Err(EngineError::Container(format!(
            "{binary}: {}",
            String::from_utf8_lossy(&err).trim()
        )));
    }
    Ok(std::process::Output {
        status,
        stdout: out,
        stderr: err,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn independent_snapshots_resolve_docker_and_git_and_clear_inherited_env() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        for instance in ["one", "two"] {
            let bin = root.path().join(instance);
            std::fs::create_dir(&bin).unwrap();
            for program in ["docker", "git"] {
                let path = bin.join(program);
                std::fs::write(&path, format!(
                    "#!/bin/sh\nprintf '%s\\n' '{instance}:{program}' \"$DOCKER_HOST\" \"$PWD\" \"${{AWMAN_UNDECLARED-unset}}\" \"$1\"\n"
                )).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let snapshot = EnvSnapshot::from_instance_env(
                vec![
                    ("PATH".into(), bin.to_string_lossy().into_owned()),
                    ("DOCKER_HOST".into(), format!("unix:///{instance}.sock")),
                    // The fakes live in the test's temp dir; resolution judges
                    // "a test's own fake" by the snapshot's TMPDIR.
                    (
                        "TMPDIR".into(),
                        std::env::temp_dir().to_string_lossy().into_owned(),
                    ),
                ],
                bin.clone(),
                vec!["awman".into(), "chat".into()],
            );
            for name in ["docker", "git"] {
                assert_eq!(program_in(&snapshot, name), bin.join(name));
                let mut child = std::process::Command::new(program_in(&snapshot, name));
                child
                    .env_clear()
                    .envs(snapshot.spawn_env())
                    .current_dir(snapshot.cwd())
                    .arg("probe");
                let output = child.output().unwrap();
                assert!(output.status.success());
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap(),
                    format!(
                        "{instance}:{name}\nunix:///{instance}.sock\n{}\nunset\nprobe\n",
                        bin.display()
                    )
                );
                let command = command(&snapshot, name);
                assert_eq!(command.get_program(), bin.join(name).as_os_str());
                // Exactly the snapshot's PATH, TMPDIR and DOCKER_HOST: nothing inherited.
                assert_eq!(command.get_envs().count(), 3);
            }
        }
    }

    #[test]
    fn snapshot_debug_redacts_all_instance_input() {
        let snapshot = EnvSnapshot::from_instance_env(
            vec![("GITHUB_TOKEN".into(), "env-secret".into())],
            "/cwd-secret".into(),
            vec!["argv-secret".into()],
        );
        let debug = format!("{snapshot:?}");
        assert!(debug.contains("redacted"));
        for secret in ["GITHUB_TOKEN", "env-secret", "cwd-secret", "argv-secret"] {
            assert!(!debug.contains(secret));
        }
    }

    #[test]
    fn absolute_program_paths_are_preserved() {
        let env = EnvSnapshot::empty();
        assert_eq!(
            program(&env, "/absolute-program"),
            Path::new("/absolute-program")
        );
    }

    #[test]
    fn real_container_clis_are_off_under_isolation() {
        let env = EnvSnapshot::empty();
        for name in ["docker", "container", "sbx"] {
            let resolved = program(&env, name);
            assert!(resolved.starts_with("/nonexistent/awman-test-isolation"));
            assert!(!resolved.exists());
        }
    }
}
