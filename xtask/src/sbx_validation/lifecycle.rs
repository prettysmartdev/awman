//! The SBX shell-sandbox lifecycle shared by the developer diagnostic
//! (`make validate-sbx`) and the release-owned smoke (`release.rs`).
//!
//! Everything here is bounded and owned: every external call runs in its own
//! process group under a deadline, the sandbox name is random and refused if
//! it already exists, and removal runs after any failed step but never for a
//! name this attempt did not create.
use crate::evidence::digest;
use anyhow::{ensure, Context, Result};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub(crate) const STEPS: &[&str] = &[
    "version", "session", "create", "inspect", "exec", "run", "stop", "restart", "remove",
    "cleanup",
];
pub(crate) const GUEST_SCRIPT: &str = "set -eu; cd \"$2\"; test \"$(uname -s)\" = Linux; test \"$(cat .host-probe)\" = \"$1\"; printf '%s\\n' \"$1\" > .guest-probe";

/// Steps that release what earlier steps created. They keep their own
/// per-step deadline even after the overall deadline or an interruption, so
/// a stopped check still removes its sandbox.
const CLEANUP_STEPS: &[&str] = &["remove", "cleanup"];

/// Per-call deadlines plus an optional whole-check deadline and interruption
/// flag. `DEFAULT` is the developer diagnostic's historical budget.
#[derive(Clone)]
pub(crate) struct Deadlines {
    pub create: Duration,
    pub step: Duration,
    pub overall: Option<Instant>,
    pub interrupted: Option<Arc<AtomicBool>>,
}
impl Deadlines {
    pub(crate) const DEFAULT: Deadlines = Deadlines {
        create: Duration::from_secs(300),
        step: Duration::from_secs(60),
        overall: None,
        interrupted: None,
    };
}

/// A bounded call hit its deadline. Kept as a type so callers can classify
/// it without parsing messages; the text is unchanged from the diagnostic.
#[derive(Debug)]
pub(crate) struct TimedOut(pub String);
impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for TimedOut {}

/// The operator interrupted the check; the call was killed.
#[derive(Debug)]
pub(crate) struct Interrupted(pub String);
impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Interrupted {}

pub(crate) fn live_cli_version(version: &str) -> bool {
    version_at_least(version, "0.54.0")
}

/// Numeric `X.Y.Z` comparison; anything unparsable is "not at least".
pub(crate) fn version_at_least(version: &str, minimum: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        let parts: Option<Vec<u64>> = v.split('.').map(|s| s.parse().ok()).collect();
        parts.filter(|p| p.len() == 3)
    };
    match (parse(version), parse(minimum)) {
        (Some(v), Some(m)) => v >= m,
        _ => false,
    }
}

/// Every regular file under `root` (relative, `/`-separated) and its digest,
/// except `skip` at the top level. Symlinks and special files are refused.
pub(crate) fn files_except(root: &Path, skip: &str) -> Result<BTreeMap<String, String>> {
    fn walk(
        root: &Path,
        dir: &Path,
        skip: &str,
        hashes: &mut BTreeMap<String, String>,
    ) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(!kind.is_symlink(), "symlink in SBX smoke evidence");
            if kind.is_dir() {
                walk(root, &entry.path(), skip, hashes)?;
            } else {
                ensure!(kind.is_file(), "non-file in SBX smoke evidence");
                let name = entry
                    .path()
                    .strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/");
                if name != skip {
                    hashes.insert(name, digest::sha256_file(&entry.path())?);
                }
            }
        }
        Ok(())
    }
    let mut hashes = BTreeMap::new();
    walk(root, root, skip, &mut hashes)?;
    Ok(hashes)
}

pub(crate) fn files(root: &Path) -> Result<BTreeMap<String, String>> {
    files_except(root, "proof.json")
}

/// Cleanup runs even after a failed/partially completed create. Existing names
/// are refused before mutation; only this attempt's random name is removed.
pub(crate) fn smoke(
    name: &str,
    workspace: &Path,
    nonce: &str,
    mut call: impl FnMut(&str, &[&str]) -> Result<Vec<u8>>,
) -> Result<String> {
    let raw = call("version", &["--version"])?;
    let version = crate::release::sbx::version(&String::from_utf8_lossy(&raw))
        .filter(|v| live_cli_version(v))
        .context("validate-sbx needs sbx >= 0.54.0")?;
    let listing = call("session", &["--no-input", "ls", "--json"])?;
    ensure!(
        !crate::release::sbx::contains_name(&serde_json::from_slice(&listing)?, name)?,
        "sandbox name already exists; refusing to touch it"
    );
    let workspace_str = workspace.to_str().context("workspace path is not UTF-8")?;
    fs::write(workspace.join(".host-probe"), format!("{nonce}\n"))?;
    let result: Result<()> = (|| {
        call(
            "create",
            &[
                "--no-input",
                "create",
                "--name",
                name,
                "shell",
                workspace_str,
            ],
        )?;
        let inspect = call("inspect", &["--no-input", "inspect", name, "--json"])?;
        let value: serde_json::Value = serde_json::from_slice(&inspect)?;
        ensure!(
            value.is_object() || value.is_array(),
            "invalid SBX inspection"
        );
        call(
            "exec",
            &[
                "--no-input",
                "exec",
                "-w",
                workspace_str,
                name,
                "sh",
                "-c",
                GUEST_SCRIPT,
                "awman-validation",
                nonce,
                workspace_str,
            ],
        )?;
        check_marker(workspace, nonce)?;
        // Distinct witnesses prevent exec output from standing in for run.
        fs::remove_file(workspace.join(".guest-probe"))?;
        call(
            "run",
            &[
                "--no-input",
                "run",
                "--name",
                name,
                "--",
                "-c",
                GUEST_SCRIPT,
                "awman-validation",
                nonce,
                workspace_str,
            ],
        )?;
        check_marker(workspace, nonce)?;
        call("stop", &["--no-input", "stop", name])?;
        call(
            "restart",
            &[
                "--no-input",
                "exec",
                "-w",
                workspace_str,
                name,
                "sh",
                "-c",
                "test \"$(cat .guest-probe)\" = \"$1\"",
                "awman-validation",
                nonce,
            ],
        )?;
        Ok(())
    })();
    let removed = call("remove", &["--no-input", "rm", "--force", name]);
    let cleanup = call("cleanup", &["--no-input", "ls", "--json"]);
    // Cleanup errors take precedence and can never produce successful proof.
    removed.context("sandbox cleanup failed")?;
    ensure!(
        !crate::release::sbx::contains_name(
            &serde_json::from_slice(&cleanup.context("cannot verify sandbox cleanup")?)?,
            name
        )?,
        "sandbox survived cleanup: {name}"
    );
    result?;
    Ok(version)
}

pub(crate) fn check_marker(workspace: &Path, nonce: &str) -> Result<()> {
    ensure!(
        fs::read(workspace.join(".guest-probe"))? == format!("{nonce}\n").as_bytes(),
        "guest workspace round trip failed"
    );
    Ok(())
}

/// Inherit the normal login/configuration. No test HOME, credential inputs,
/// secret commands, host agent or release preflight is involved.
pub(crate) fn command(cli: &Path, workspace: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(cli);
    command
        .current_dir(workspace)
        .args(args)
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
}

/// The `sbx` executable on PATH, canonicalised.
pub(crate) fn locate_cli() -> Result<PathBuf> {
    locate("sbx")
}

/// An absolute program is used as given (it must be a file); a bare name is
/// searched on PATH. The result is canonicalised so its digest is stable.
pub(crate) fn locate(program: impl AsRef<Path>) -> Result<PathBuf> {
    let program = program.as_ref();
    let found = if program.is_absolute() {
        Some(program.to_path_buf()).filter(|p| p.is_file())
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join(program))
            .find(|p| p.is_file())
    };
    found
        .and_then(|p| p.canonicalize().ok())
        .context("sbx is not on PATH; install it and sign in with sbx login")
}

/// A fresh random sandbox name (`<prefix><24 hex>`) and the 64-hex nonce it
/// was derived from.
pub(crate) fn new_identity(prefix: &str) -> Result<(String, String)> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|e| anyhow::anyhow!("cannot allocate a random sandbox name: {e}"))?;
    let nonce = digest::sha256(&random);
    Ok((format!("{prefix}{}", &nonce[..24]), nonce))
}

/// Kill a child's whole process group, then the child, and reap it.
pub(crate) fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = child.kill();
    let _ = child.wait();
}

/// Wait for a spawned child under `deadline`, honouring `interrupted`.
/// `tick` runs every 100 ms (progress output). On timeout or interruption
/// the process group is killed and a `TimedOut`/`Interrupted` error returned.
pub(crate) fn wait_bounded(
    child: &mut std::process::Child,
    deadline: Instant,
    interrupted: Option<&AtomicBool>,
    what: &str,
    mut tick: impl FnMut(),
) -> Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if interrupted.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            kill_group(child);
            return Err(Interrupted(format!("interrupted during {what}")).into());
        }
        if Instant::now() >= deadline {
            kill_group(child);
            return Err(TimedOut(what.to_owned()).into());
        }
        tick();
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Run one lifecycle step: stdout/stderr go to `<out>/<step>.stdout|.stderr`,
/// its exit code into `steps`. `label` prefixes the progress lines.
#[allow(clippy::too_many_arguments)]
pub(crate) fn invoke(
    cli: &Path,
    workspace: &Path,
    out: &Path,
    step: &str,
    args: &[&str],
    steps: &mut BTreeMap<String, i32>,
    label: &str,
    deadlines: &Deadlines,
) -> Result<Vec<u8>> {
    eprintln!("{label}: {step} started");
    let started = Instant::now();
    let mut last_progress = started;
    let stdout = out.join(format!("{step}.stdout"));
    let mut c = command(cli, workspace, args);
    c.stdout(fs::File::create(&stdout)?)
        .stderr(fs::File::create(out.join(format!("{step}.stderr")))?);
    steps.insert(step.into(), -1);
    let mut child = c
        .spawn()
        .with_context(|| format!("cannot start sbx for {step}"))?;
    let cleanup = CLEANUP_STEPS.contains(&step);
    let mut deadline = Instant::now()
        + if step == "create" {
            deadlines.create
        } else {
            deadlines.step
        };
    if let (Some(overall), false) = (deadlines.overall, cleanup) {
        deadline = deadline.min(overall);
    }
    let interrupted = deadlines.interrupted.as_deref().filter(|_| !cleanup);
    let status = wait_bounded(&mut child, deadline, interrupted, step, || {
        if last_progress.elapsed() >= Duration::from_secs(10) {
            eprintln!(
                "{label}: {step} still running ({}s)",
                started.elapsed().as_secs()
            );
            last_progress = Instant::now();
        }
    })
    .map_err(|error| match error.downcast::<TimedOut>() {
        Ok(_) => TimedOut(format!("{step} timed out; see {}", out.display())).into(),
        Err(other) => other,
    })?;
    steps.insert(step.into(), status.code().unwrap_or(-1));
    eprintln!(
        "{label}: {step} {} ({:.1}s)",
        if status.success() { "passed" } else { "failed" },
        started.elapsed().as_secs_f64()
    );
    ensure!(status.success(), "{step} failed; see {step}.stderr");
    Ok(fs::read(stdout)?)
}
