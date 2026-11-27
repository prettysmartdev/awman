//! Release-owned SBX smoke (WI 0094): the shared shell-sandbox lifecycle plus
//! a bounded `awman ready` through the exact attested asset, on the Mac and,
//! only when explicitly selected, on the Linux host over SSH.
//!
//! The check uses the host's existing SBX login. It never runs `sbx login` or
//! `logout`, never reads provider keys, keychains or `~/.docker`, never passes
//! secrets to a sandbox and never invokes a provider agent. The Linux host gets
//! only `release.sh` and the asset bytes. Every sandbox and remote directory it
//! creates carries this check's random name and is recorded in `intent.json`
//! before the first mutation; reconciliation removes exactly those.
//!
//! Evidence layout of `out_dir` (`<state>/sbx-smoke/<id>/`):
//!
//! ```text
//! binding.json   canonical binding; sha256(file) == binding.sha256()
//! intent.json    ownership marker, written before the first mutation
//! lifecycle/     Mac: <step>.stdout|.stderr, workspace/.host-probe|.guest-probe
//!                Linux: receipt.json, ssh.log, probe.sh, host.txt, ssh-exit.txt
//! ready/         ready.stdout, ready.stderr, ready-exit.txt, config.json, ...
//! reconcile.json post-check listing audit
//! report.json    SbxSmokeReport, written last and atomically
//! ```
use super::lifecycle::{
    files_except, invoke, locate, new_identity, smoke, version_at_least, wait_bounded, Deadlines,
    Interrupted, TimedOut, STEPS,
};
use super::linux::{forward_progress_with, ssh_command_with};
use crate::evidence::digest;
use crate::release::util::{
    private_dir, read_json, valid_ssh_destination, write_atomic, write_json,
};
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufReader, Seek, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};

pub(crate) const SCHEMA: u32 = 1;
pub(crate) const LIMITS: &str = "Release SBX smoke: logged-in CLI, shell sandbox lifecycle, guest \
execution, workspace sharing and exact-asset readiness. It does not prove provider \
authentication, effective agent policy or a successful provider task.";

/// The remote payload (modes `probe`, `stage`, `run`, `list`, `remove`).
const SCRIPT: &str = include_str!("release.sh");
/// Prefix of this check's sandbox names (the diagnostic uses `awman-validate-`).
pub(crate) const SANDBOX_PREFIX: &str = "awman-release-";
/// `runtime` in `awman ready --json` for `runtime.kind = "sbx"`.
const READY_RUNTIME: &str = "docker-sbx-experimental";
/// The private global awman config used for readiness.
const READY_CONFIG: &str =
    r#"{"runtime":{"kind":"sbx"},"agents":{"auth":{"mode":"none","refresh":{"enabled":false}}}}"#;
/// Linux arch the release obligation qualifies; probes reject anything else.
const LINUX_ARCH: &str = "x86_64";
/// Default remote base for the owned root (predictable, not `$TMPDIR`).
const REMOTE_TMP: &str = "/tmp";
/// Never forwarded to the exact asset (their values are never read).
const PROVIDER_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GH_TOKEN",
    "GITHUB_TOKEN",
];
const PROBE_SECS: u64 = 20;
const LIST_SECS: u64 = 60;
const STAGE_SECS: u64 = 60;
const UPLOAD_SECS: u64 = 300;
const STEP_SECS: u64 = 60;
const CREATE_SECS: u64 = 300;

// ───────────────────────────── selection ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SbxHostSelection {
    pub id: String,
    pub host: String,
    pub destination: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SbxSelection {
    pub schema: u32,
    pub hosts: Vec<SbxHostSelection>,
    pub sha256: String,
}

/// Mac always; Linux only for an explicitly supplied destination.
pub(crate) fn select(
    mac_id: &str,
    linux_id: &str,
    explicit_linux_host: Option<&str>,
) -> Result<SbxSelection> {
    let mut hosts = vec![SbxHostSelection {
        id: mac_id.into(),
        host: "local".into(),
        destination: None,
    }];
    if let Some(destination) = explicit_linux_host {
        ensure!(
            valid_ssh_destination(destination),
            "invalid LINUX_HOST; expected user@host or an SSH host alias"
        );
        hosts.push(SbxHostSelection {
            id: linux_id.into(),
            host: "ssh".into(),
            destination: Some(destination.into()),
        });
    }
    Ok(SbxSelection {
        schema: 1,
        sha256: SbxSelection::digest(&hosts)?,
        hosts,
    })
}

impl SbxSelection {
    pub(crate) fn digest(hosts: &[SbxHostSelection]) -> Result<String> {
        digest::input_digest(&json!({"schema": 1, "hosts": hosts}))
    }

    /// Ok(()) when `current` may continue an attempt frozen as `self`, given
    /// which ids already have a verified passing record. Err(reason) is the
    /// text of `sbx-selection-changed`. Nothing is ever dropped.
    pub(crate) fn admits(
        &self,
        current: &SbxSelection,
        passed: &BTreeSet<String>,
    ) -> std::result::Result<(), String> {
        let ssh = |s: &SbxSelection| {
            s.hosts
                .iter()
                .find(|h| h.host == "ssh")
                .map(|h| (h.id.clone(), h.destination.clone().unwrap_or_default()))
        };
        let local = |s: &SbxSelection| {
            s.hosts
                .iter()
                .filter(|h| h.host == "local")
                .map(|h| h.id.clone())
                .collect::<Vec<_>>()
        };
        if local(self) != local(current) {
            return Err(
                "the SBX host selection of this attempt differs from the current controller's"
                    .into(),
            );
        }
        match (ssh(self), ssh(current)) {
            (None, None) => Ok(()),
            (None, Some((_, now))) => Err(format!(
                "this attempt was pinned with SBX on the Mac only; LINUX_HOST={now} would add a \
                 Linux SBX check. Unset LINUX_HOST (a saved release host still serves the Linux \
                 KVM and Docker checks), or start a new attempt with SUPERSEDE=1 before the seal"
            )),
            (Some((id, frozen)), None) if passed.contains(&id) => {
                let _ = frozen;
                Ok(())
            }
            (Some((_, frozen)), None) => Err(format!(
                "this attempt selected a Linux SBX check on {frozen}; set LINUX_HOST={frozen} to \
                 finish the selected Linux SBX check"
            )),
            (Some((id, frozen)), Some((id_now, now))) if id == id_now && frozen == now => Ok(()),
            (Some((_, frozen)), Some((_, now))) => Err(format!(
                "this attempt selected a Linux SBX check on {frozen}; LINUX_HOST={now} would \
                 substitute another host. Set LINUX_HOST={frozen}, or start a new attempt with \
                 SUPERSEDE=1 before the seal"
            )),
        }
    }
}

// ───────────────────────────── job and records ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Target {
    Local,
    Ssh { destination: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SbxSmokeBinding {
    pub schema: u32,
    pub obligation: String,
    pub triple: String,
    pub commit: String,
    pub tree: String,
    pub manifest_sha256: String,
    pub plan_sha256: String,
    pub policy_sha256: String,
    pub asset: String,
    pub asset_sha256: String,
    pub selection_sha256: String,
    pub destination: Option<String>,
    pub steps: Vec<String>,
    pub ready_agent: String,
    pub min_cli_version: String,
    pub lifecycle_budget_seconds: u64,
    pub ready_budget_seconds: u64,
    pub budget_seconds: u64,
}
impl SbxSmokeBinding {
    pub(crate) fn sha256(&self) -> Result<String> {
        digest::input_digest(self)
    }
    /// Exactly the bytes whose SHA-256 is `sha256()`.
    fn canonical(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&serde_json::to_value(self)?)?)
    }
}

/// The fixed readiness invocation for `agent`.
pub(crate) fn ready_argv(agent: &str) -> Vec<String> {
    ["ready", "--agent", agent, "--prepare-only", "--json"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

pub(crate) struct SbxSmokeJob {
    pub binding: SbxSmokeBinding,
    pub target: Target,
    /// Verified local copy of the exact attested asset.
    pub asset_path: PathBuf,
    /// Private, durable evidence directory `<state>/sbx-smoke/<id>` (0700).
    pub out_dir: PathBuf,
    /// Set when the operator interrupted; the run stops, reconciles and
    /// returns an error starting with "interrupted".
    pub interrupted: Arc<AtomicBool>,
    /// Test seams. Production: "sbx", "ssh", "/tmp".
    pub sbx_program: PathBuf,
    pub ssh_program: PathBuf,
    pub remote_tmp: String,
}
impl SbxSmokeJob {
    /// A production job: `sbx`/`ssh` from PATH and the fixed remote base.
    pub(crate) fn new(
        binding: SbxSmokeBinding,
        target: Target,
        asset_path: PathBuf,
        out_dir: PathBuf,
        interrupted: Arc<AtomicBool>,
    ) -> Self {
        SbxSmokeJob {
            binding,
            target,
            asset_path,
            out_dir,
            interrupted,
            sbx_program: "sbx".into(),
            ssh_program: "ssh".into(),
            remote_tmp: REMOTE_TMP.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CliIdentity {
    pub path: String,
    pub version: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Outcome {
    Passed,
    Failed,
    Blocked,
}
impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Passed => "passed",
            Outcome::Failed => "failed",
            Outcome::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadyResult {
    pub argv: Vec<String>,
    pub exit: Option<i32>,
    pub ready: bool,
    pub runtime: Option<String>,
    pub runtime_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SbxSmokeReport {
    pub schema: u32,
    pub obligation: String,
    pub binding_sha256: String,
    pub outcome: Outcome,
    pub reason_code: Option<String>,
    pub reason: Option<String>,
    /// `os`, `arch`, `os_build`, `hostname_sha256`.
    pub host: BTreeMap<String, String>,
    pub cli: Option<CliIdentity>,
    pub cli_after: Option<CliIdentity>,
    pub asset_sha256: Option<String>,
    pub asset_version: Option<String>,
    pub steps: BTreeMap<String, i32>,
    pub ready: Option<ReadyResult>,
    pub sandbox: String,
    pub ready_sandbox: String,
    pub nonce_sha256: String,
    /// Every file in the directory except `report.json`.
    pub files: BTreeMap<String, String>,
    pub started: DateTime<Utc>,
    pub finished: DateTime<Utc>,
    pub limits: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct SbxSmokeRecord {
    pub outcome: String,
    pub report_sha256: String,
    pub binding_sha256: String,
    pub asset_sha256: String,
    pub selection_sha256: String,
    pub destination: Option<String>,
    pub cli_version: Option<String>,
    pub cli_sha256: Option<String>,
    pub host: BTreeMap<String, String>,
    pub reason_code: Option<String>,
    pub reason: Option<String>,
    pub finished: Option<DateTime<Utc>>,
}

/// Ownership marker: what this check may create and, on resume, remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    schema: u32,
    obligation: String,
    sandbox: String,
    ready_sandbox: String,
    #[serde(default)]
    sandbox_claimed: bool,
    #[serde(default)]
    ready_claimed: bool,
    nonce_sha256: String,
    workspace: String,
    remote_root: Option<String>,
    destination: Option<String>,
    started: DateTime<Utc>,
    deadline: DateTime<Utc>,
}

/// A prerequisite that is not this check's defect (retried next time).
#[derive(Debug)]
struct Blocked {
    code: &'static str,
    message: String,
}
impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for Blocked {}
fn blocked(code: &'static str, message: impl Into<String>) -> anyhow::Error {
    Blocked {
        code,
        message: message.into(),
    }
    .into()
}

// ───────────────────────────── small helpers ─────────────────────────────

/// The production sandbox name awman derives for `(workspace, agent)`
/// (`src/engine/sandbox/naming.rs`: FNV-1a 64, low 32 bits).
pub(crate) fn sandbox_name(workspace: &Path, agent: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in workspace.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("awman-{:08x}-{agent}", hash & 0xffff_ffff)
}

fn safe_word(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn triple_parts(triple: &str) -> Option<(&'static str, &str)> {
    let arch = triple.split('-').next()?;
    let os = if triple.ends_with("-apple-darwin") {
        "macos"
    } else if triple.contains("-linux-") {
        "linux"
    } else {
        return None;
    };
    Some((os, arch))
}

fn listing_contains(bytes: &[u8], name: &str) -> Result<bool> {
    crate::release::sbx::contains_name(&serde_json::from_slice(bytes)?, name)
}

fn b64_decode(text: &str) -> Result<Vec<u8>> {
    let value = |c: u8| -> Result<u32> {
        Ok(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => bail!("invalid base64"),
        } as u32)
    };
    let bytes = text.as_bytes();
    ensure!(bytes.len().is_multiple_of(4), "invalid base64 length");
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let last = i + 1 == bytes.len() / 4;
        let pad = chunk.iter().rev().take_while(|b| **b == b'=').count();
        ensure!(pad <= 2 && (pad == 0 || last), "invalid base64 padding");
        let mut n = 0u32;
        for (j, c) in chunk.iter().enumerate() {
            n = (n << 6) | if j >= 4 - pad { 0 } else { value(*c)? };
        }
        let three = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&three[..3 - pad]);
    }
    Ok(out)
}

/// A receipt field written by `release.sh`'s `b64`: a string or null.
fn b64_field(value: &Value) -> Result<Option<Vec<u8>>> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => b64_decode(s).map(Some),
        _ => bail!("receipt field is not base64"),
    }
}

fn parse_ready(bytes: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(bytes);
    let parsed = serde_json::from_str::<Value>(text.trim()).ok().or_else(|| {
        let start = text.find("\n{").map(|i| i + 1)?;
        serde_json::from_str(text[start..].trim()).ok()
    });
    parsed.filter(Value::is_object)
}

fn ready_result(agent: &str, exit: Option<i32>, stdout: &[u8]) -> ReadyResult {
    let parsed = parse_ready(stdout);
    let runtime = parsed
        .as_ref()
        .and_then(|v| v["runtime"].as_str())
        .map(str::to_owned);
    ReadyResult {
        argv: ready_argv(agent),
        exit,
        ready: parsed.as_ref().is_some_and(|v| v["ready"] == true),
        runtime_kind: runtime
            .as_deref()
            .map(|r| if r == READY_RUNTIME { "sbx" } else { r }.to_owned()),
        runtime,
    }
}

fn stdin_file(bytes: &[u8]) -> Result<fs::File> {
    let mut file = tempfile::tempfile()?;
    file.write_all(bytes)?;
    file.rewind()?;
    Ok(file)
}

fn group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

/// Run to completion under `timeout` (and `until`), stdout captured to a
/// private temp file (≤ 4 MiB), stderr discarded. `None` = killed by signal.
fn capture(
    command: &mut Command,
    stdin: Option<fs::File>,
    timeout: Duration,
    interrupted: Option<&AtomicBool>,
    what: &str,
) -> Result<(Option<i32>, Vec<u8>)> {
    use std::io::Read;
    let mut out = tempfile::tempfile()?;
    command
        .stdout(out.try_clone()?)
        .stderr(Stdio::null())
        .stdin(match stdin {
            Some(file) => Stdio::from(file),
            None => Stdio::null(),
        });
    group(command);
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start {what}"))?;
    let status = wait_bounded(
        &mut child,
        Instant::now() + timeout,
        interrupted,
        what,
        || {},
    )?;
    out.rewind()?;
    let mut bytes = Vec::new();
    out.take(4 << 20).read_to_end(&mut bytes)?;
    Ok((status.code(), bytes))
}

/// Run with stdout/stderr in files; returns the exit code (`-1` = signal).
fn to_files(
    command: &mut Command,
    stdout: &Path,
    stderr: &Path,
    deadline: Instant,
    interrupted: Option<&AtomicBool>,
    what: &str,
) -> Result<i32> {
    command
        .stdout(fs::File::create(stdout)?)
        .stderr(fs::File::create(stderr)?)
        .stdin(Stdio::null());
    group(command);
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start {what}"))?;
    let status = wait_bounded(&mut child, deadline, interrupted, what, || {})?;
    Ok(status.code().unwrap_or(-1))
}

fn is<E: std::error::Error + Send + Sync + 'static>(error: &anyhow::Error) -> bool {
    error.chain().any(|e| e.is::<E>())
}

fn uname(flag: &str) -> String {
    let mut c = Command::new("uname");
    c.arg(flag);
    capture(&mut c, None, Duration::from_secs(5), None, "uname")
        .ok()
        .filter(|(code, _)| *code == Some(0))
        .map(|(_, out)| String::from_utf8_lossy(&out).trim().to_owned())
        .unwrap_or_default()
}

fn local_host() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("os".into(), std::env::consts::OS.into()),
        ("arch".into(), std::env::consts::ARCH.into()),
        ("os_build".into(), uname("-r")),
        (
            "hostname_sha256".into(),
            digest::sha256(uname("-n").as_bytes()),
        ),
    ])
}

fn remote_root(job: &SbxSmokeJob, name: &str) -> String {
    format!(
        "{}/awman-sbx-release-{name}",
        job.remote_tmp.trim_end_matches('/')
    )
}

// ───────────────────────────── probes ─────────────────────────────

/// Read-only prerequisite probe for preflight/doctor (no create). Err carries
/// (reason_code, message), reason_code ∈ {"sbx-cli-missing","sbx-cli-too-old",
/// "sbx-not-signed-in","linux-host-unreachable","linux-host-prerequisite"}.
pub(crate) fn probe(
    target: &Target,
    min_cli_version: &str,
    ssh_program: &Path,
) -> std::result::Result<CliIdentity, (String, String)> {
    probe_with(
        target,
        min_cli_version,
        Path::new("sbx"),
        ssh_program,
        LINUX_ARCH,
        true,
    )
}

/// Current CLI identity on the host, for invalidating reused evidence.
pub(crate) fn cli_identity(
    target: &Target,
    ssh_program: &Path,
) -> std::result::Result<CliIdentity, (String, String)> {
    probe_with(
        target,
        "0.0.0",
        Path::new("sbx"),
        ssh_program,
        LINUX_ARCH,
        false,
    )
}

fn probe_with(
    target: &Target,
    min: &str,
    sbx_program: &Path,
    ssh_program: &Path,
    linux_arch: &str,
    session: bool,
) -> std::result::Result<CliIdentity, (String, String)> {
    let err = |code: &str, message: String| (code.to_owned(), message);
    let (identity, listing) = match target {
        Target::Local => {
            let cli = locate(sbx_program).map_err(|e| err("sbx-cli-missing", format!("{e:#}")))?;
            let sha256 =
                digest::sha256_file(&cli).map_err(|e| err("sbx-cli-missing", format!("{e:#}")))?;
            let run = |args: &[&str]| {
                let mut c = Command::new(&cli);
                c.args(args);
                capture(&mut c, None, Duration::from_secs(PROBE_SECS), None, "sbx")
            };
            let version = match run(&["--version"]) {
                Ok((Some(0), out)) => crate::release::sbx::version(&String::from_utf8_lossy(&out)),
                _ => return Err(err("sbx-cli-missing", "sbx --version did not run".into())),
            };
            let listing = if session {
                Some(match run(&["--no-input", "ls", "--json"]) {
                    Ok((Some(0), out)) => out,
                    _ => Vec::new(),
                })
            } else {
                None
            };
            let identity = CliIdentity {
                path: cli.to_string_lossy().into_owned(),
                version: version.unwrap_or_default(),
                sha256,
            };
            (identity, listing)
        }
        Target::Ssh { destination } => {
            let mut c = ssh_command_with(ssh_program, destination, &["sh", "-s", "--", "probe"])
                .map_err(|e| err("linux-host-prerequisite", format!("{e:#}")))?;
            let stdin = stdin_file(SCRIPT.as_bytes())
                .map_err(|e| err("linux-host-prerequisite", format!("{e:#}")))?;
            let (code, out) = capture(
                &mut c,
                Some(stdin),
                Duration::from_secs(PROBE_SECS + 10),
                None,
                "ssh",
            )
            .map_err(|e| match is::<TimedOut>(&e) {
                true => err(
                    "linux-host-unreachable",
                    format!("{destination}: SSH probe timed out"),
                ),
                false => err("linux-host-unreachable", format!("{e:#}")),
            })?;
            match code {
                Some(0) => {}
                Some(255) | None => {
                    return Err(err(
                        "linux-host-unreachable",
                        format!("{destination} is not reachable over SSH (BatchMode)"),
                    ))
                }
                Some(n) => {
                    return Err(err(
                        "linux-host-prerequisite",
                        format!("{destination}: SBX probe exited {n}"),
                    ))
                }
            }
            let parsed = (|| -> Result<(CliIdentity, Option<Vec<u8>>, String, String)> {
                let v: Value = serde_json::from_slice(&out)?;
                let text = |k: &str| -> Result<String> {
                    Ok(
                        String::from_utf8_lossy(&b64_field(&v[k])?.unwrap_or_default())
                            .trim()
                            .to_owned(),
                    )
                };
                let listing = match v["session"].as_i64() {
                    Some(0) => b64_field(&v["listing"])?,
                    _ => None,
                };
                Ok((
                    CliIdentity {
                        path: text("cli")?,
                        version: v["cli_version"].as_str().unwrap_or_default().to_owned(),
                        sha256: v["cli_sha256"].as_str().unwrap_or_default().to_owned(),
                    },
                    listing,
                    text("os")?,
                    text("arch")?,
                ))
            })()
            .map_err(|e| {
                err(
                    "linux-host-prerequisite",
                    format!("{destination}: unreadable probe: {e:#}"),
                )
            })?;
            let (identity, listing, os, arch) = parsed;
            if os != "Linux" || arch != linux_arch {
                return Err(err(
                    "linux-host-prerequisite",
                    format!("{destination} is {os}/{arch}, not Linux/{linux_arch}"),
                ));
            }
            if identity.path.is_empty() {
                return Err(err(
                    "sbx-cli-missing",
                    format!("sbx is not on the SSH account PATH of {destination}"),
                ));
            }
            (
                identity,
                if session {
                    Some(listing.unwrap_or_default())
                } else {
                    None
                },
            )
        }
    };
    if !digest::is_sha256(&identity.sha256) {
        return Err(err(
            "sbx-cli-missing",
            "cannot fingerprint the sbx executable".into(),
        ));
    }
    if identity.version.is_empty() || !version_at_least(&identity.version, min) {
        return Err(err(
            "sbx-cli-too-old",
            format!(
                "sbx {} is older than {min}",
                if identity.version.is_empty() {
                    "(unknown)"
                } else {
                    &identity.version
                }
            ),
        ));
    }
    if let Some(listing) = listing {
        if listing_contains(&listing, "awman-release-probe").is_err() {
            return Err(err(
                "sbx-not-signed-in",
                "sbx ls --json failed; run sbx login on that host".into(),
            ));
        }
    }
    Ok(identity)
}

// ───────────────────────────── run ─────────────────────────────

/// Run the whole check synchronously (call from a worker thread). Idempotent:
/// - a verifying passed/failed report already in `out_dir` is returned
///   without running anything; a verifying blocked one is retried;
/// - a report that does not verify is an error and is never replaced;
/// - a leftover intent is reconciled first (only this job's sandbox/root),
///   the partial evidence is moved aside next to `out_dir`, then a fresh run
///   starts with a new name and nonce.
///
/// Always writes `report.json` before returning Ok. Err only for conditions
/// that prevented writing a report (state I/O, a foreign binding) or for an
/// operator interruption (message starts with "interrupted").
pub(crate) fn run(job: &SbxSmokeJob) -> Result<SbxSmokeReport> {
    check_job(job)?;
    let report_path = job.out_dir.join("report.json");
    let mut aside = None;
    if report_path.exists() {
        let record = verify_report(job).context(
            "the existing SBX smoke report does not verify; it is never replaced automatically",
        )?;
        if record.outcome != "blocked" {
            return read_json(&report_path);
        }
        aside = Some("blocked");
    }
    if job.out_dir.join("intent.json").exists() {
        if let Err(error) = reconcile(job) {
            // Keep the intent: nothing may be forgotten that could still exist.
            return write_unreconciled(job, error);
        }
        aside.get_or_insert("interrupted");
    }
    if aside.is_none() && fs::read_dir(&job.out_dir)?.next().is_some() {
        aside = Some("partial");
    }
    if let Some(label) = aside {
        move_aside(job, label)?;
    }
    fresh(job)
}

fn check_job(job: &SbxSmokeJob) -> Result<()> {
    let b = &job.binding;
    ensure!(b.schema == SCHEMA, "unsupported SBX smoke binding schema");
    ensure!(
        b.steps.iter().map(String::as_str).eq(STEPS.iter().copied()),
        "SBX smoke binding steps differ from the lifecycle"
    );
    ensure!(safe_word(&b.ready_agent), "invalid readiness agent");
    ensure!(
        b.min_cli_version.split('.').count() == 3 && version_at_least(&b.min_cli_version, "0.0.0"),
        "invalid minimum sbx version"
    );
    ensure!(
        b.lifecycle_budget_seconds > 0
            && b.ready_budget_seconds > 0
            && b.budget_seconds >= b.lifecycle_budget_seconds.max(b.ready_budget_seconds),
        "invalid SBX smoke budgets"
    );
    ensure!(
        triple_parts(&b.triple).is_some(),
        "unsupported SBX smoke triple"
    );
    match &job.target {
        Target::Local => ensure!(b.destination.is_none(), "local SBX smoke has a destination"),
        Target::Ssh { destination } => {
            ensure!(valid_ssh_destination(destination), "invalid LINUX_HOST");
            ensure!(
                b.destination.as_deref() == Some(destination.as_str()),
                "SBX smoke destination differs from its binding"
            );
            ensure!(
                job.remote_tmp.starts_with('/')
                    && !job.remote_tmp.contains("..")
                    && job
                        .remote_tmp
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c)),
                "invalid remote temporary directory"
            );
        }
    }
    private_dir(&job.out_dir)?;
    let binding = job.out_dir.join("binding.json");
    if binding.exists() {
        ensure!(
            fs::read(&binding)? == b.canonical()?,
            "{} belongs to a different SBX smoke binding",
            job.out_dir.display()
        );
    }
    Ok(())
}

fn move_aside(job: &SbxSmokeJob, label: &str) -> Result<()> {
    let parent = job
        .out_dir
        .parent()
        .context("evidence directory has no parent")?;
    let name = job
        .out_dir
        .file_name()
        .context("evidence directory has no name")?
        .to_string_lossy();
    let target = parent.join(format!(
        "{name}-{label}-{}",
        &crate::release::util::random_id()?[..12]
    ));
    fs::rename(&job.out_dir, &target)
        .with_context(|| format!("retain {} as {}", job.out_dir.display(), target.display()))?;
    private_dir(&job.out_dir)
}

fn new_report(job: &SbxSmokeJob, started: DateTime<Utc>) -> Result<SbxSmokeReport> {
    Ok(SbxSmokeReport {
        schema: SCHEMA,
        obligation: job.binding.obligation.clone(),
        binding_sha256: job.binding.sha256()?,
        outcome: Outcome::Passed,
        reason_code: None,
        reason: None,
        host: BTreeMap::new(),
        cli: None,
        cli_after: None,
        asset_sha256: None,
        asset_version: None,
        steps: BTreeMap::new(),
        ready: None,
        sandbox: String::new(),
        ready_sandbox: String::new(),
        nonce_sha256: String::new(),
        files: BTreeMap::new(),
        started,
        finished: started,
        limits: LIMITS.into(),
    })
}

impl SbxSmokeReport {
    /// Blocked < failed < failed cleanup; the first reason of a rank stays.
    fn set(&mut self, outcome: Outcome, code: &str, reason: impl Into<String>) {
        let rank = |outcome: Outcome, code: Option<&str>| match (outcome, code) {
            (Outcome::Passed, _) => 0,
            (Outcome::Blocked, _) => 1,
            (Outcome::Failed, Some("cleanup")) => 3,
            (Outcome::Failed, _) => 2,
        };
        if rank(outcome, Some(code)) > rank(self.outcome, self.reason_code.as_deref()) {
            self.outcome = outcome;
            self.reason_code = Some(code.into());
            self.reason = Some(reason.into());
        }
    }
    fn block(&mut self, code: &str, reason: impl Into<String>) {
        self.set(Outcome::Blocked, code, reason)
    }
    fn fail(&mut self, code: &str, reason: impl Into<String>) {
        self.set(Outcome::Failed, code, reason)
    }
    fn passed(&self) -> bool {
        self.outcome == Outcome::Passed
    }
}

fn write_unreconciled(job: &SbxSmokeJob, error: anyhow::Error) -> Result<SbxSmokeReport> {
    let now = Utc::now();
    let mut report = new_report(job, now)?;
    if let Ok(intent) = read_json::<Intent>(&job.out_dir.join("intent.json")) {
        report.sandbox = intent.sandbox;
        report.ready_sandbox = intent.ready_sandbox;
        report.nonce_sha256 = intent.nonce_sha256;
    }
    match error.chain().find_map(|e| e.downcast_ref::<Blocked>()) {
        Some(b) => report.block(
            b.code,
            format!("previous attempt not reconciled: {}", b.message),
        ),
        None => report.fail(
            "cleanup",
            format!("previous attempt not reconciled: {error:#}"),
        ),
    }
    write_atomic(&job.out_dir.join("binding.json"), &job.binding.canonical()?)?;
    finalize(job, report)
}

fn fresh(job: &SbxSmokeJob) -> Result<SbxSmokeReport> {
    let started = Utc::now();
    let t0 = Instant::now();
    write_atomic(&job.out_dir.join("binding.json"), &job.binding.canonical()?)?;
    let mut report = new_report(job, started)?;
    let result = match &job.target {
        Target::Local => run_local(job, &mut report, t0),
        Target::Ssh { destination } => run_ssh(job, destination, &mut report, t0),
    };
    if let Err(error) = result {
        if is::<Interrupted>(&error) {
            // Remove what exists now; the intent stays for the next resume.
            let _ = reconcile(job);
            bail!("interrupted: {error:#}");
        }
        return Err(error);
    }
    finalize(job, report)
}

/// Seal the directory: files, finish time, atomic report. A report that
/// claims to pass is re-verified from the raw files before it is returned.
fn finalize(job: &SbxSmokeJob, mut report: SbxSmokeReport) -> Result<SbxSmokeReport> {
    let path = job.out_dir.join("report.json");
    report.finished = Utc::now();
    report.files = files_except(&job.out_dir, "report.json")?;
    write_json(&path, &report)?;
    if report.passed() {
        if let Err(error) = verify_report(job) {
            report.fail("sbx-smoke-invalid", format!("self-check: {error:#}"));
            report.files = files_except(&job.out_dir, "report.json")?;
            write_json(&path, &report)?;
        }
    }
    Ok(report)
}

fn deadline_of(t0: Instant, seconds: u64) -> Instant {
    t0 + Duration::from_secs(seconds)
}

/// Written before the first mutation; the ownership marker for resume.
fn write_intent(
    job: &SbxSmokeJob,
    report: &SbxSmokeReport,
    workspace: &str,
    remote_root: Option<String>,
) -> Result<()> {
    let started = report.started;
    write_json(
        &job.out_dir.join("intent.json"),
        &Intent {
            schema: SCHEMA,
            obligation: job.binding.obligation.clone(),
            sandbox: report.sandbox.clone(),
            ready_sandbox: report.ready_sandbox.clone(),
            sandbox_claimed: false,
            ready_claimed: false,
            nonce_sha256: report.nonce_sha256.clone(),
            workspace: workspace.into(),
            remote_root,
            destination: job.binding.destination.clone(),
            started,
            deadline: started + chrono::Duration::seconds(job.binding.budget_seconds as i64),
        },
    )
}

fn update_claims(job: &SbxSmokeJob, sandbox: bool, ready: bool) -> Result<()> {
    let path = job.out_dir.join("intent.json");
    let mut intent: Intent = read_json(&path)?;
    intent.sandbox_claimed |= sandbox;
    intent.ready_claimed |= ready;
    write_json(&path, &intent)
}

// ───────────────────────────── Mac ─────────────────────────────

fn run_local(job: &SbxSmokeJob, r: &mut SbxSmokeReport, t0: Instant) -> Result<()> {
    let b = &job.binding;
    r.host = local_host();
    let (os, arch) = triple_parts(&b.triple).context("unsupported triple")?;
    if os != std::env::consts::OS || arch != std::env::consts::ARCH {
        r.block(
            "sbx-host-unsupported",
            format!(
                "this host is {}/{}, not {}",
                std::env::consts::OS,
                std::env::consts::ARCH,
                b.triple
            ),
        );
        return Ok(());
    }
    let cli = match locate(&job.sbx_program) {
        Ok(cli) => cli,
        Err(e) => {
            r.block("sbx-cli-missing", format!("{e:#}"));
            return Ok(());
        }
    };
    let cli_sha = digest::sha256_file(&cli)?;
    let out = job.out_dir.canonicalize()?;
    let lifecycle = out.join("lifecycle");
    let workspace = lifecycle.join("workspace");
    let ready_dir = out.join("ready");
    let repo = ready_dir.join("repo");
    for dir in [&lifecycle, &workspace, &ready_dir, &repo] {
        private_dir(dir)?;
    }
    let repo = repo.canonicalize()?;
    let (name, nonce) = new_identity(SANDBOX_PREFIX)?;
    r.sandbox = name.clone();
    r.ready_sandbox = sandbox_name(&repo, &b.ready_agent);
    r.nonce_sha256 = digest::sha256(nonce.as_bytes());
    let overall = deadline_of(t0, b.budget_seconds);
    let deadlines = Deadlines {
        create: Duration::from_secs(CREATE_SECS),
        step: Duration::from_secs(STEP_SECS),
        overall: Some(overall.min(deadline_of(t0, b.lifecycle_budget_seconds))),
        interrupted: Some(job.interrupted.clone()),
    };
    let workspace_str = workspace.to_string_lossy().into_owned();
    let mut steps = BTreeMap::new();
    let mut intent = false;
    let result = smoke(&name, &workspace, &nonce, |step, args| {
        if step == "create" && !intent {
            write_intent(job, r, &workspace_str, None)?;
            update_claims(job, true, false)?;
            intent = true;
        }
        let bytes = invoke(
            &cli,
            &workspace,
            &lifecycle,
            step,
            args,
            &mut steps,
            "release sbx [macOS]",
            &deadlines,
        )?;
        if step == "version" {
            let version = crate::release::sbx::version(&String::from_utf8_lossy(&bytes));
            if !version
                .as_deref()
                .is_some_and(|v| version_at_least(v, &b.min_cli_version))
            {
                return Err(blocked(
                    "sbx-cli-too-old",
                    format!("release SBX smoke needs sbx >= {}", b.min_cli_version),
                ));
            }
        }
        Ok(bytes)
    });
    r.steps = steps;
    let version = fs::read(lifecycle.join("version.stdout"))
        .ok()
        .and_then(|raw| crate::release::sbx::version(&String::from_utf8_lossy(&raw)))
        .unwrap_or_default();
    r.cli = Some(CliIdentity {
        path: cli.to_string_lossy().into_owned(),
        version,
        sha256: cli_sha.clone(),
    });
    if let Err(error) = result {
        if is::<Interrupted>(&error) {
            return Err(error);
        }
        classify_local(r, &lifecycle, &name, &error);
    }
    if r.passed() {
        readiness_local(job, r, &cli, &ready_dir, &repo, overall)?;
    }
    if intent {
        audit(job, r)?;
    }
    let after = digest::sha256_file(&cli).unwrap_or_default();
    r.cli_after = r.cli.clone().map(|c| CliIdentity {
        sha256: after.clone(),
        ..c
    });
    if after != cli_sha {
        r.fail("cli-changed", "the sbx executable changed during the check");
    }
    Ok(())
}

/// Before `create`: BLOCKED with the precise prerequisite. From `create` on:
/// FAILED, cleanup failures first; deadlines are FAILED even when cleanup verified.
fn classify_local(r: &mut SbxSmokeReport, lifecycle: &Path, name: &str, error: &anyhow::Error) {
    let message = format!("{error:#}");
    let steps = r.steps.clone();
    let step = |s: &str| steps.get(s).copied();
    if step("create").is_none() {
        if let Some(b) = error.chain().find_map(|e| e.downcast_ref::<Blocked>()) {
            r.block(b.code, b.message.clone());
        } else if is::<TimedOut>(error) {
            r.fail("timeout", message);
        } else if step("version") != Some(0) {
            r.block("sbx-cli-missing", message);
        } else if step("session").is_none() {
            r.block("sbx-cli-too-old", message);
        } else if step("session") != Some(0) {
            r.block("sbx-not-signed-in", message);
        } else {
            match fs::read(lifecycle.join("session.stdout"))
                .map_err(anyhow::Error::from)
                .and_then(|l| listing_contains(&l, name))
            {
                Ok(true) => r.block("sbx-name-occupied", message),
                _ => r.block("sbx-not-signed-in", message),
            }
        }
        return;
    }
    let cleaned = step("remove") == Some(0)
        && step("cleanup") == Some(0)
        && fs::read(lifecycle.join("cleanup.stdout"))
            .map_err(anyhow::Error::from)
            .and_then(|l| listing_contains(&l, name))
            .is_ok_and(|present| !present);
    if !cleaned {
        r.fail("cleanup", message);
    } else if is::<TimedOut>(error) {
        r.fail("timeout", message);
    } else {
        r.fail("lifecycle", message);
    }
}

/// Readiness environment: the login environment without any `AWMAN_*` or
/// provider variable (names only are inspected), private `AWMAN_HOME`, and
/// the fingerprinted `sbx` first on PATH so awman drives the same CLI.
fn readiness_command(asset: &Path, repo: &Path, home: &Path, cli: &Path) -> Command {
    let mut c = Command::new(asset);
    c.current_dir(repo);
    for (key, _) in std::env::vars_os() {
        let key_text = key.to_string_lossy();
        if key_text.starts_with("AWMAN_")
            || key_text.starts_with("GIT_")
            || PROVIDER_ENV.contains(&key_text.as_ref())
        {
            c.env_remove(&key);
        }
    }
    let mut path = vec![cli.parent().map(Path::to_path_buf).unwrap_or_default()];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    if let Ok(joined) = std::env::join_paths(path) {
        c.env("PATH", joined);
    }
    c.env("AWMAN_HOME", home);
    c
}

fn git_init(repo: &Path, log: &Path) -> Result<()> {
    let git = |args: &[&str]| -> Result<()> {
        let mut c = Command::new("git");
        c.current_dir(repo)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TEMPLATE_DIR", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0");
        let (code, out) = capture(&mut c, None, Duration::from_secs(30), None, "git")?;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?
            .write_all(&out)?;
        ensure!(code == Some(0), "git {} failed", args.join(" "));
        Ok(())
    };
    git(&["init", "-q"])?;
    let mut verify = Command::new("git");
    verify
        .current_dir(repo)
        .args(["rev-parse", "--show-toplevel"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TEMPLATE_DIR", "/dev/null");
    let (code, root) = capture(
        &mut verify,
        None,
        Duration::from_secs(30),
        None,
        "git rev-parse",
    )?;
    ensure!(
        code == Some(0)
            && PathBuf::from(String::from_utf8_lossy(&root).trim()).canonicalize()?
                == repo.canonicalize()?,
        "Git resolved outside the private readiness repository"
    );
    git(&[
        "-c",
        "user.name=awman-smoke",
        "-c",
        "user.email=smoke@awman.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "awman release smoke",
    ])
}

fn readiness_local(
    job: &SbxSmokeJob,
    r: &mut SbxSmokeReport,
    cli: &Path,
    ready_dir: &Path,
    repo: &Path,
    overall: Instant,
) -> Result<()> {
    let b = &job.binding;
    let interrupted = Some(job.interrupted.as_ref());
    let listing = |label: &str| -> Result<(i32, Vec<u8>)> {
        let stdout = ready_dir.join(format!("{label}.stdout"));
        let mut c = Command::new(cli);
        c.current_dir(ready_dir)
            .args(["--no-input", "ls", "--json"]);
        let code = to_files(
            &mut c,
            &stdout,
            &ready_dir.join(format!("{label}.stderr")),
            Instant::now() + Duration::from_secs(STEP_SECS),
            None,
            "sbx ls",
        )?;
        Ok((code, fs::read(stdout)?))
    };
    let asset_sha = digest::sha256_file(&job.asset_path)?;
    r.asset_sha256 = Some(asset_sha.clone());
    if asset_sha != b.asset_sha256 {
        r.fail(
            "readiness",
            "the exact asset digest differs from the attested asset",
        );
        return Ok(());
    }
    if let Err(e) = git_init(repo, &ready_dir.join("repo.log")) {
        r.fail("readiness", format!("private repository: {e:#}"));
        return Ok(());
    }
    let home = ready_dir.join("awman-home");
    private_dir(&home)?;
    write_atomic(&home.join("config.json"), READY_CONFIG.as_bytes())?;
    write_atomic(&ready_dir.join("config.json"), READY_CONFIG.as_bytes())?;
    let mut version = readiness_command(&job.asset_path, repo, &home, cli);
    version.arg("--version");
    match to_files(
        &mut version,
        &ready_dir.join("version.stdout"),
        &ready_dir.join("version.stderr"),
        Instant::now() + Duration::from_secs(PROBE_SECS),
        interrupted,
        "awman --version",
    ) {
        Ok(_) => {}
        Err(e) if is::<Interrupted>(&e) => return Err(e),
        Err(_) => {}
    }
    r.asset_version = fs::read_to_string(ready_dir.join("version.stdout"))
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_owned()))
        .filter(|s| !s.is_empty());
    match listing("pre") {
        Ok((0, bytes)) => match listing_contains(&bytes, &r.ready_sandbox) {
            Ok(false) => {}
            Ok(true) => {
                r.block(
                    "sbx-name-occupied",
                    format!("{} already exists; refusing to touch it", r.ready_sandbox),
                );
                return Ok(());
            }
            Err(e) => {
                r.fail("readiness", format!("unreadable sandbox listing: {e:#}"));
                return Ok(());
            }
        },
        Ok((code, _)) => {
            r.fail(
                "readiness",
                format!("sbx ls --json exited {code} before readiness"),
            );
            return Ok(());
        }
        Err(e) => {
            r.fail("readiness", format!("{e:#}"));
            return Ok(());
        }
    }
    eprintln!("release sbx [macOS]: ready started");
    update_claims(job, false, true)?;
    let mut ready = readiness_command(&job.asset_path, repo, &home, cli);
    ready.args(ready_argv(&b.ready_agent));
    let deadline = (Instant::now() + Duration::from_secs(b.ready_budget_seconds)).min(overall);
    let exit = match to_files(
        &mut ready,
        &ready_dir.join("ready.stdout"),
        &ready_dir.join("ready.stderr"),
        deadline,
        interrupted,
        "awman ready",
    ) {
        Ok(code) => Some(code),
        Err(e) if is::<Interrupted>(&e) => return Err(e),
        Err(e) if is::<TimedOut>(&e) => {
            r.fail("timeout", "awman ready reached its deadline");
            None
        }
        Err(e) => {
            r.fail("readiness", format!("{e:#}"));
            None
        }
    };
    fs::write(
        ready_dir.join("ready-exit.txt"),
        exit.map(|c| c.to_string()).unwrap_or_else(|| "none".into()),
    )?;
    let result = ready_result(
        &b.ready_agent,
        exit,
        &fs::read(ready_dir.join("ready.stdout"))?,
    );
    eprintln!(
        "release sbx [macOS]: ready {}",
        if exit == Some(0) && result.ready {
            "passed"
        } else {
            "failed"
        }
    );
    if exit.is_some()
        && (exit != Some(0) || !result.ready || result.runtime_kind.as_deref() != Some("sbx"))
    {
        r.fail(
            "readiness",
            format!(
                "awman ready exited {exit:?}, ready={}, runtime={:?}",
                result.ready, result.runtime
            ),
        );
    }
    r.ready = Some(result);
    // The readiness sandbox did not exist before; remove it if it does now.
    if let Ok((0, bytes)) = listing("post") {
        if listing_contains(&bytes, &r.ready_sandbox).unwrap_or(false) {
            let mut rm = Command::new(cli);
            rm.current_dir(ready_dir)
                .args(["--no-input", "rm", "--force", &r.ready_sandbox]);
            let code = to_files(
                &mut rm,
                &ready_dir.join("remove.stdout"),
                &ready_dir.join("remove.stderr"),
                Instant::now() + Duration::from_secs(STEP_SECS),
                None,
                "sbx rm",
            )?;
            if code != 0 {
                r.fail(
                    "cleanup",
                    format!("removing {} exited {code}", r.ready_sandbox),
                );
            }
        }
    }
    if digest::sha256_file(&job.asset_path)? != asset_sha {
        r.fail("readiness", "the exact asset changed during readiness");
    }
    for scratch in [repo.to_path_buf(), home] {
        if let Err(e) = fs::remove_dir_all(&scratch) {
            r.fail(
                "cleanup",
                format!("cannot remove scratch {}: {e}", scratch.display()),
            );
        }
    }
    Ok(())
}

// ───────────────────────────── Linux over SSH ─────────────────────────────

fn ssh_call(
    job: &SbxSmokeJob,
    destination: &str,
    remote: &[&str],
    stdin: Option<fs::File>,
    timeout: Duration,
) -> Result<(Option<i32>, Vec<u8>)> {
    let ssh = locate(&job.ssh_program).map_err(|_| {
        blocked(
            "linux-host-prerequisite",
            "ssh is not installed on this controller",
        )
    })?;
    let mut c = ssh_command_with(&ssh, destination, remote)?;
    capture(&mut c, stdin, timeout, None, "ssh")
}

fn script_stdin() -> Result<fs::File> {
    stdin_file(SCRIPT.as_bytes())
}

fn run_ssh(
    job: &SbxSmokeJob,
    destination: &str,
    r: &mut SbxSmokeReport,
    t0: Instant,
) -> Result<()> {
    let b = &job.binding;
    let (os, arch) = triple_parts(&b.triple).context("unsupported triple")?;
    ensure!(os == "linux", "an SSH SBX smoke must target Linux");
    let ssh = match locate(&job.ssh_program) {
        Ok(ssh) => ssh,
        Err(_) => {
            r.block(
                "linux-host-prerequisite",
                "ssh is not installed on this controller",
            );
            return Ok(());
        }
    };
    let (name, nonce) = new_identity(SANDBOX_PREFIX)?;
    let root = remote_root(job, &name);
    r.sandbox = name.clone();
    r.ready_sandbox = sandbox_name(Path::new(&format!("{root}/repo")), &b.ready_agent);
    r.nonce_sha256 = digest::sha256(nonce.as_bytes());
    let overall = deadline_of(t0, b.budget_seconds);
    let unreachable = |r: &mut SbxSmokeReport| {
        r.block(
            "linux-host-unreachable",
            format!("{destination} is not reachable over SSH (BatchMode)"),
        )
    };
    write_intent(job, r, &format!("{root}/workspace"), Some(root.clone()))?;

    // 1. Stage the owned root (refuses an existing path).
    match ssh_call(
        job,
        destination,
        &["sh", "-s", "--", "stage", &name, &root],
        Some(script_stdin()?),
        Duration::from_secs(STAGE_SECS),
    ) {
        Ok((Some(0), _)) => {}
        Ok((Some(3), _)) => {
            r.block(
                "sbx-name-occupied",
                format!("{root} already exists on {destination}; refusing to touch it"),
            );
            return Ok(());
        }
        Ok((Some(255) | None, _)) => {
            unreachable(r);
            return Ok(());
        }
        Ok((Some(code), _)) => {
            r.block(
                "linux-host-prerequisite",
                format!("staging on {destination} exited {code}"),
            );
            return audit(job, r);
        }
        Err(e) if is::<TimedOut>(&e) => {
            unreachable(r);
            return Ok(());
        }
        Err(e) => return Err(e),
    }
    // 2. Upload the exact asset on stdin (into the 0700 root only we own).
    let asset_sha = digest::sha256_file(&job.asset_path)?;
    if asset_sha != b.asset_sha256 {
        r.asset_sha256 = Some(asset_sha);
        r.fail(
            "readiness",
            "the exact asset digest differs from the attested asset",
        );
        return audit(job, r);
    }
    let target = format!("{root}/awman");
    match ssh_call(
        job,
        destination,
        &["cat", ">", &target],
        Some(fs::File::open(&job.asset_path)?),
        Duration::from_secs(UPLOAD_SECS),
    ) {
        Ok((Some(0), _)) => {}
        Ok((Some(255) | None, _)) => {
            unreachable(r);
            return Ok(());
        }
        Ok((Some(code), _)) => {
            r.block(
                "linux-host-prerequisite",
                format!("asset upload to {destination} exited {code}"),
            );
            return audit(job, r);
        }
        Err(e) if is::<TimedOut>(&e) => {
            unreachable(r);
            return Ok(());
        }
        Err(e) => return Err(e),
    }
    // 3. Lifecycle + readiness in one bounded session.
    let lifecycle = job.out_dir.join("lifecycle");
    let ready_dir = job.out_dir.join("ready");
    private_dir(&lifecycle)?;
    private_dir(&ready_dir)?;
    write_atomic(&lifecycle.join("probe.sh"), SCRIPT.as_bytes())?;
    write_atomic(&lifecycle.join("host.txt"), destination.as_bytes())?;
    write_atomic(&ready_dir.join("config.json"), READY_CONFIG.as_bytes())?;
    let budget = b.ready_budget_seconds.to_string();
    let (step_secs, create_secs) = (STEP_SECS.to_string(), CREATE_SECS.to_string());
    let mut c = ssh_command_with(
        &ssh,
        destination,
        &[
            "setsid",
            "sh",
            "-s",
            "--",
            "run",
            &name,
            &nonce,
            &root,
            &b.asset_sha256,
            &b.ready_agent,
            &r.ready_sandbox,
            arch,
            &b.min_cli_version,
            &budget,
            &step_secs,
            &create_secs,
        ],
    )?;
    c.stdin(Stdio::from(script_stdin()?))
        .stdout(fs::File::create(lifecycle.join("receipt.json"))?)
        .stderr(fs::File::create(lifecycle.join("ssh.log"))?);
    eprintln!(
        "release sbx [Linux {destination}]: started; logs in {}",
        lifecycle.display()
    );
    let mut progress = BufReader::new(fs::File::open(lifecycle.join("ssh.log"))?);
    let mut pending = Vec::new();
    let mut child = c
        .spawn()
        .context("cannot start ssh for the Linux SBX smoke")?;
    let waited = wait_bounded(
        &mut child,
        overall,
        Some(job.interrupted.as_ref()),
        "Linux SBX smoke",
        || {
            let _ =
                forward_progress_with(&mut progress, &mut pending, "release-sbx-linux: ", |line| {
                    eprintln!("{line}")
                });
        },
    );
    let _ = forward_progress_with(&mut progress, &mut pending, "release-sbx-linux: ", |l| {
        eprintln!("{l}")
    });
    let code = match waited {
        Ok(status) => status.code(),
        Err(e) if is::<Interrupted>(&e) => return Err(e),
        Err(e) if is::<TimedOut>(&e) => {
            r.fail("timeout", "the Linux SBX smoke reached its deadline");
            None
        }
        Err(e) => return Err(e),
    };
    write_atomic(
        &lifecycle.join("ssh-exit.txt"),
        code.map(|c| c.to_string())
            .unwrap_or_else(|| "-1".into())
            .as_bytes(),
    )?;
    if code == Some(255) {
        unreachable(r);
        return audit(job, r);
    }
    let receipt: Option<Value> =
        serde_json::from_slice(&fs::read(lifecycle.join("receipt.json"))?).ok();
    match receipt {
        Some(receipt) => absorb_receipt(r, &receipt, &ready_dir, code, b)?,
        None if code.is_some() => {
            if code == Some(2) {
                r.block(
                    "linux-host-prerequisite",
                    format!("{destination} lacks a prerequisite; inspect lifecycle/ssh.log"),
                );
            } else {
                r.fail(
                    "lifecycle",
                    format!("the Linux SBX smoke exited {code:?} without a receipt"),
                );
            }
        }
        None => {}
    }
    audit(job, r)
}

/// Copy what the receipt proves into the report and classify its outcome.
fn absorb_receipt(
    r: &mut SbxSmokeReport,
    receipt: &Value,
    ready_dir: &Path,
    code: Option<i32>,
    b: &SbxSmokeBinding,
) -> Result<()> {
    update_claims_from_receipt(
        r,
        receipt,
        ready_dir.parent().context("Linux output directory")?,
    )?;
    if let Some(raw_logs) = receipt["raw_logs"].as_object() {
        let dir = ready_dir
            .parent()
            .context("Linux lifecycle output directory")?
            .join("remote-logs");
        private_dir(&dir)?;
        for (name, value) in raw_logs {
            ensure!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
                "invalid remote log name"
            );
            if let Some(bytes) = b64_field(value)? {
                write_atomic(&dir.join(name), &bytes)?;
            }
        }
    }
    let text = |k: &str| -> String {
        b64_field(&receipt[k])
            .ok()
            .flatten()
            .map(|v| String::from_utf8_lossy(&v).trim().to_owned())
            .unwrap_or_default()
    };
    let target = receipt["target"].as_str().unwrap_or_default();
    r.host = BTreeMap::from([
        (
            "os".into(),
            target.split('/').next().unwrap_or_default().to_owned(),
        ),
        (
            "arch".into(),
            target.split('/').nth(1).unwrap_or_default().to_owned(),
        ),
        ("os_build".into(), text("os_build")),
        (
            "hostname_sha256".into(),
            receipt["hostname_sha256"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        ),
    ]);
    r.steps = serde_json::from_value(receipt["steps"].clone()).unwrap_or_default();
    let cli = CliIdentity {
        path: text("cli"),
        version: receipt["cli_version"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        sha256: receipt["cli_sha256"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    };
    r.cli_after = Some(CliIdentity {
        sha256: receipt["cli_sha256_after"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        ..cli.clone()
    });
    r.cli = Some(cli);
    r.asset_sha256 = receipt["asset_sha256"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    r.asset_version = Some(text("asset_version"))
        .filter(|s| !s.is_empty())
        .map(|s| s.lines().next().unwrap_or_default().to_owned());
    let ready = &receipt["ready"];
    if ready["ran"] == true {
        let stdout = b64_field(&ready["stdout"])?.unwrap_or_default();
        let stderr = b64_field(&ready["stderr"])?.unwrap_or_default();
        write_atomic(&ready_dir.join("ready.stdout"), &stdout)?;
        write_atomic(&ready_dir.join("ready.stderr"), &stderr)?;
        let exit = ready["exit"].as_i64().map(|c| c as i32);
        write_atomic(
            &ready_dir.join("ready-exit.txt"),
            exit.map(|c| c.to_string())
                .unwrap_or_else(|| "none".into())
                .as_bytes(),
        )?;
        r.ready = Some(ready_result(&b.ready_agent, exit, &stdout));
    }
    let blocked_code = receipt["blocked"].as_str().unwrap_or_default();
    let exit = receipt["exit"].as_i64();
    let cleaned = !r.steps.contains_key("create")
        || (r.steps.get("remove") == Some(&0)
            && r.steps.get("cleanup") == Some(&0)
            && b64_field(&receipt["cleanup"])
                .ok()
                .flatten()
                .is_some_and(|l| listing_contains(&l, &r.sandbox).is_ok_and(|p| !p)));
    if !cleaned {
        r.fail(
            "cleanup",
            "the Linux sandbox was not removed and verified absent",
        );
    }
    if !blocked_code.is_empty() {
        let code: &'static str = match blocked_code {
            "sbx-cli-missing" => "sbx-cli-missing",
            "sbx-cli-too-old" => "sbx-cli-too-old",
            "sbx-not-signed-in" => "sbx-not-signed-in",
            "sbx-name-occupied" => "sbx-name-occupied",
            _ => "linux-host-prerequisite",
        };
        r.block(code, format!("Linux SBX prerequisite: {blocked_code}"));
    } else if exit != Some(0) || code != Some(0) {
        let error = ready["error"].as_str().unwrap_or_default();
        match error {
            "" if r.steps.contains_key("create") => r.fail(
                "lifecycle",
                format!("the Linux SBX lifecycle exited {exit:?}"),
            ),
            "" => r.block(
                "linux-host-prerequisite",
                format!("the Linux SBX smoke exited {exit:?} before create"),
            ),
            other => r.fail("readiness", format!("Linux readiness failed: {other}")),
        }
    }
    if let Some(ready) = &r.ready {
        if ready.exit != Some(0) || !ready.ready || ready.runtime_kind.as_deref() != Some("sbx") {
            let reason = format!(
                "awman ready exited {:?}, ready={}, runtime={:?}",
                ready.exit, ready.ready, ready.runtime
            );
            r.fail("readiness", reason);
        }
    }
    Ok(())
}

fn update_claims_from_receipt(r: &SbxSmokeReport, receipt: &Value, out: &Path) -> Result<()> {
    // The intent is the durable controller ownership record; remote claims are
    // accepted only from the payload receipt bound to this report's sandbox.
    ensure!(
        receipt["sandbox"] == r.sandbox,
        "remote ownership receipt names another sandbox"
    );
    let path = out.join("intent.json");
    let mut intent: Intent = read_json(&path)?;
    intent.sandbox_claimed |= receipt["sandbox_claimed"] == true;
    intent.ready_claimed |= receipt["ready_claimed"] == true;
    write_json(&path, &intent)?;
    Ok(())
}

// ───────────────────────────── reconcile ─────────────────────────────

/// What a listing found and what had to be removed.
struct Sweep {
    removed: Vec<String>,
    listing: Value,
    root: Option<String>,
}

/// Remove only what `intent` names, then verify absence. Blocked = the host
/// or CLI could not be asked (intent must be kept); other errors = removal
/// failed or something survived.
fn sweep(job: &SbxSmokeJob, intent: &Intent) -> Result<Sweep> {
    ensure!(
        intent.obligation == job.binding.obligation
            && intent.sandbox.starts_with(SANDBOX_PREFIX)
            && safe_word(&intent.sandbox)
            && safe_word(&intent.ready_sandbox),
        "intent.json does not belong to this check"
    );
    let names: Vec<&str> = [
        intent.sandbox_claimed.then_some(intent.sandbox.as_str()),
        intent
            .ready_claimed
            .then_some(intent.ready_sandbox.as_str()),
    ]
    .into_iter()
    .flatten()
    .collect();
    match &job.target {
        Target::Local => {
            let cli = locate(&job.sbx_program)
                .map_err(|e| blocked("sbx-cli-missing", format!("{e:#}")))?;
            let list = || -> Result<Value> {
                let mut c = Command::new(&cli);
                c.args(["--no-input", "ls", "--json"]);
                match capture(&mut c, None, Duration::from_secs(LIST_SECS), None, "sbx ls")? {
                    (Some(0), out) => {
                        let v: Value = serde_json::from_slice(&out).map_err(|e| {
                            blocked("sbx-not-signed-in", format!("unreadable listing: {e}"))
                        })?;
                        crate::release::sbx::contains_name(&v, "awman-release-probe")
                            .map_err(|e| blocked("sbx-not-signed-in", format!("{e:#}")))?;
                        Ok(v)
                    }
                    (code, _) => Err(blocked(
                        "sbx-not-signed-in",
                        format!("sbx ls --json exited {code:?}"),
                    )),
                }
            };
            let listing = list()?;
            let mut removed = Vec::new();
            for name in names.iter().copied() {
                if crate::release::sbx::contains_name(&listing, name)? {
                    let mut c = Command::new(&cli);
                    c.args(["--no-input", "rm", "--force", name]);
                    let (code, _) =
                        capture(&mut c, None, Duration::from_secs(STEP_SECS), None, "sbx rm")?;
                    ensure!(code == Some(0), "removing owned sandbox {name} failed");
                    removed.push(name.to_owned());
                }
            }
            let listing = if removed.is_empty() { listing } else { list()? };
            for name in names.iter().copied() {
                ensure!(
                    !crate::release::sbx::contains_name(&listing, name)?,
                    "owned sandbox {name} survived removal"
                );
            }
            Ok(Sweep {
                removed,
                listing,
                root: None,
            })
        }
        Target::Ssh { destination } => {
            let root = intent
                .remote_root
                .clone()
                .context("intent has no remote root")?;
            ensure!(
                root == remote_root(job, &intent.sandbox),
                "intent remote root is not owned by this check"
            );
            let list = || -> Result<(Value, String)> {
                let (code, out) = ssh_call(
                    job,
                    destination,
                    &["sh", "-s", "--", "list", &intent.sandbox, &root],
                    Some(script_stdin()?),
                    Duration::from_secs(LIST_SECS + 15),
                )
                .map_err(|e| match is::<TimedOut>(&e) {
                    true => blocked(
                        "linux-host-unreachable",
                        format!("{destination}: listing timed out"),
                    ),
                    false => e,
                })?;
                match code {
                    Some(0) => {}
                    Some(255) | None => {
                        return Err(blocked(
                            "linux-host-unreachable",
                            format!("{destination} is not reachable over SSH"),
                        ))
                    }
                    Some(n) => {
                        return Err(blocked(
                            "linux-host-prerequisite",
                            format!("{destination}: listing exited {n}"),
                        ))
                    }
                }
                let v: Value = serde_json::from_slice(&out)?;
                let state = v["root"].as_str().unwrap_or_default().to_owned();
                let listing = match (v["listing_exit"].as_i64(), b64_field(&v["listing"])?) {
                    (Some(0), Some(bytes)) => {
                        serde_json::from_slice::<Value>(&bytes).map_err(|e| {
                            blocked("sbx-not-signed-in", format!("unreadable listing: {e}"))
                        })?
                    }
                    (Some(127), _) => {
                        return Err(blocked(
                            "sbx-cli-missing",
                            format!("sbx is not on the SSH account PATH of {destination}"),
                        ))
                    }
                    _ => {
                        return Err(blocked(
                            "sbx-not-signed-in",
                            format!("sbx ls --json failed on {destination}"),
                        ))
                    }
                };
                crate::release::sbx::contains_name(&listing, "awman-release-probe")
                    .map_err(|e| blocked("sbx-not-signed-in", format!("{e:#}")))?;
                Ok((listing, state))
            };
            let (listing, state) = list()?;
            let mut present = Vec::new();
            for name in names.iter().copied() {
                if crate::release::sbx::contains_name(&listing, name)? {
                    present.push(name.to_owned());
                }
            }
            let mut removed = present.clone();
            if state == "owned" {
                removed.push(root.clone());
            }
            if removed.is_empty() {
                return Ok(Sweep {
                    removed,
                    listing,
                    root: Some(state),
                });
            }
            let mut args = vec![
                "sh",
                "-s",
                "--",
                "remove",
                intent.sandbox.as_str(),
                root.as_str(),
            ];
            args.extend(present.iter().map(String::as_str));
            let (code, _) = ssh_call(
                job,
                destination,
                &args,
                Some(script_stdin()?),
                Duration::from_secs(STEP_SECS * 3),
            )?;
            ensure!(
                code == Some(0),
                "removing owned objects on {destination} exited {code:?}"
            );
            let (listing, state) = list()?;
            for name in names.iter().copied() {
                ensure!(
                    !crate::release::sbx::contains_name(&listing, name)?,
                    "owned sandbox {name} survived removal on {destination}"
                );
            }
            ensure!(
                state != "owned",
                "owned root {root} survived removal on {destination}"
            );
            Ok(Sweep {
                removed,
                listing,
                root: Some(state),
            })
        }
    }
}

/// Remove only what `out_dir/intent.json` names and verify absence.
/// Ok(true) = something had to be removed (a leak), Ok(false) = nothing was
/// left, Err = absence could not be verified (the intent stays).
pub(crate) fn reconcile(job: &SbxSmokeJob) -> Result<bool> {
    let path = job.out_dir.join("intent.json");
    if !path.exists() {
        return Ok(false);
    }
    let intent: Intent = read_json(&path)?;
    let sweep = sweep(job, &intent)?;
    write_json(
        &job.out_dir.join("resume-reconcile.json"),
        &json!({"schema": 1, "sandbox": intent.sandbox, "ready_sandbox": intent.ready_sandbox,
                "removed": sweep.removed, "root": sweep.root, "at": Utc::now()}),
    )?;
    Ok(!sweep.removed.is_empty())
}

/// The post-check audit: `reconcile.json`. Anything removed here leaked.
fn audit(job: &SbxSmokeJob, r: &mut SbxSmokeReport) -> Result<()> {
    let intent: Intent = read_json(&job.out_dir.join("intent.json"))?;
    let entry = match sweep(job, &intent) {
        Ok(sweep) => {
            if !sweep.removed.is_empty() {
                r.fail(
                    "cleanup",
                    format!(
                        "owned objects survived the check and were removed: {}",
                        sweep.removed.join(", ")
                    ),
                );
            }
            if sweep.root.as_deref() == Some("foreign") {
                r.fail(
                    "cleanup",
                    "the owned remote root path is occupied by a foreign directory",
                );
            }
            json!({"schema": 1, "sandbox": intent.sandbox, "ready_sandbox": intent.ready_sandbox,
                   "leaked": sweep.removed.len(), "removed": sweep.removed, "root": sweep.root,
                   "listing": sweep.listing})
        }
        Err(error) => {
            match error.chain().find_map(|e| e.downcast_ref::<Blocked>()) {
                Some(b)
                    if r.steps.contains_key("create")
                        || r.ready.as_ref().is_some_and(|v| v.exit.is_some()) =>
                {
                    r.fail(
                        "cleanup",
                        format!(
                            "cleanup could not be verified after resource creation: {}",
                            b.message
                        ),
                    )
                }
                Some(b) => r.block(b.code, format!("cannot verify cleanup: {}", b.message)),
                None => r.fail("cleanup", format!("{error:#}")),
            }
            json!({"schema": 1, "sandbox": intent.sandbox, "ready_sandbox": intent.ready_sandbox,
                   "leaked": null, "error": format!("{error:#}")})
        }
    };
    write_json(&job.out_dir.join("reconcile.json"), &entry)
}

// ───────────────────────────── verification ─────────────────────────────

/// Re-derive the result from raw files. A passing report must prove every
/// step; a blocked/failed report returns its record when well-formed and
/// bound. Anything unbound, tampered, short or duplicated is Err.
pub(crate) fn verify_report(job: &SbxSmokeJob) -> Result<SbxSmokeRecord> {
    let dir = &job.out_dir;
    let raw = fs::read(dir.join("report.json")).context("missing SBX smoke report")?;
    let report: SbxSmokeReport =
        serde_json::from_slice(&raw).context("unreadable SBX smoke report")?;
    let b = &job.binding;
    let binding_sha = b.sha256()?;
    ensure!(
        report.schema == SCHEMA,
        "unsupported SBX smoke report schema"
    );
    ensure!(
        report.obligation == b.obligation,
        "SBX smoke report is for another obligation"
    );
    ensure!(
        report.binding_sha256 == binding_sha
            && digest::sha256_file(&dir.join("binding.json"))? == binding_sha,
        "SBX smoke report is not bound to this release's inputs"
    );
    ensure!(
        report.files == files_except(dir, "report.json")?,
        "SBX smoke evidence changed"
    );
    ensure!(
        report.limits == LIMITS,
        "SBX smoke limits statement differs"
    );
    ensure!(
        report.started <= report.finished,
        "SBX smoke timestamps are inverted"
    );
    match report.outcome {
        Outcome::Passed => ensure!(
            report.reason_code.is_none() && report.reason.is_none(),
            "passing SBX smoke report carries a failure reason"
        ),
        _ => ensure!(
            report.reason_code.is_some(),
            "non-passing SBX smoke report has no reason"
        ),
    }
    if report.outcome == Outcome::Passed {
        verify_pass(job, &report)?;
    }
    Ok(SbxSmokeRecord {
        outcome: report.outcome.as_str().into(),
        report_sha256: digest::sha256(&raw),
        binding_sha256: binding_sha,
        asset_sha256: b.asset_sha256.clone(),
        selection_sha256: b.selection_sha256.clone(),
        destination: b.destination.clone(),
        cli_version: report.cli.as_ref().map(|c| c.version.clone()),
        cli_sha256: report.cli.as_ref().map(|c| c.sha256.clone()),
        host: report.host.clone(),
        reason_code: report.reason_code.clone(),
        reason: report.reason.clone(),
        finished: Some(report.finished),
    })
}

fn verify_pass(job: &SbxSmokeJob, report: &SbxSmokeReport) -> Result<()> {
    let dir = &job.out_dir;
    let b = &job.binding;
    ensure!(
        b.steps.iter().map(String::as_str).eq(STEPS.iter().copied()),
        "binding steps differ from the lifecycle"
    );
    let expected: BTreeMap<String, i32> = b.steps.iter().map(|s| (s.clone(), 0)).collect();
    ensure!(
        report.steps == expected,
        "missing, extra or failed SBX smoke step"
    );
    let elapsed = (report.finished - report.started).num_seconds();
    ensure!(
        elapsed >= 0 && elapsed as u64 <= b.budget_seconds,
        "SBX smoke exceeded its budget"
    );
    let (os, arch) = triple_parts(&b.triple).context("unsupported triple")?;
    ensure!(
        report.host.get("os").map(String::as_str) == Some(os)
            && report.host.get("arch").map(String::as_str) == Some(arch),
        "SBX smoke ran on a host that does not match {}",
        b.triple
    );
    let cli = report.cli.as_ref().context("missing SBX CLI identity")?;
    ensure!(
        version_at_least(&cli.version, &b.min_cli_version) && digest::is_sha256(&cli.sha256),
        "missing or too old SBX CLI identity"
    );
    ensure!(
        report.cli_after.as_ref() == Some(cli),
        "the sbx executable changed during the check"
    );
    ensure!(
        report.asset_sha256.as_deref() == Some(b.asset_sha256.as_str()),
        "readiness did not run the attested asset"
    );
    let ready = report.ready.as_ref().context("missing readiness result")?;
    ensure!(
        ready.argv == ready_argv(&b.ready_agent)
            && ready.exit == Some(0)
            && ready.ready
            && ready.runtime_kind.as_deref() == Some("sbx"),
        "exact-asset readiness did not pass on the SBX runtime"
    );
    ensure!(
        fs::read_to_string(dir.join("ready/ready-exit.txt"))? == "0",
        "readiness exit record differs"
    );
    ensure!(
        &ready_result(
            &b.ready_agent,
            Some(0),
            &fs::read(dir.join("ready/ready.stdout"))?
        ) == ready,
        "readiness output does not show a ready SBX runtime"
    );
    ensure!(
        fs::read(dir.join("ready/config.json"))? == READY_CONFIG.as_bytes(),
        "readiness used an unexpected awman configuration"
    );
    // Ownership: intent, names and nonce agree.
    let intent: Intent = read_json(&dir.join("intent.json"))?;
    ensure!(
        intent.obligation == b.obligation
            && intent.sandbox == report.sandbox
            && intent.ready_sandbox == report.ready_sandbox
            && intent.sandbox_claimed
            && intent.ready_claimed
            && intent.nonce_sha256 == report.nonce_sha256
            && intent.destination == b.destination
            && report.sandbox.starts_with(SANDBOX_PREFIX)
            && safe_word(&report.sandbox),
        "SBX smoke ownership record differs"
    );
    let check_nonce = |host: &[u8], guest: &[u8]| -> Result<()> {
        let nonce = host.strip_suffix(b"\n").context("malformed host probe")?;
        ensure!(
            !nonce.is_empty() && host == guest && digest::sha256(nonce) == report.nonce_sha256,
            "guest workspace round trip failed"
        );
        Ok(())
    };
    // Final audit: nothing owned survived, nothing had to be removed.
    let audit: Value = read_json(&dir.join("reconcile.json"))?;
    ensure!(
        audit["leaked"] == 0,
        "the post-check audit found leaked objects"
    );
    for name in [&report.sandbox, &report.ready_sandbox] {
        ensure!(
            !crate::release::sbx::contains_name(&audit["listing"], name)?,
            "owned sandbox {name} survived the check"
        );
    }
    match &job.target {
        Target::Local => {
            ensure!(b.destination.is_none(), "local SBX smoke has a destination");
            for step in STEPS {
                for stream in ["stdout", "stderr"] {
                    ensure!(
                        report
                            .files
                            .contains_key(&format!("lifecycle/{step}.{stream}")),
                        "missing {step} logs"
                    );
                }
            }
            check_nonce(
                &fs::read(dir.join("lifecycle/workspace/.host-probe"))?,
                &fs::read(dir.join("lifecycle/workspace/.guest-probe"))?,
            )?;
            for (file, name) in [
                ("lifecycle/session.stdout", &report.sandbox),
                ("lifecycle/cleanup.stdout", &report.sandbox),
                ("ready/pre.stdout", &report.ready_sandbox),
            ] {
                ensure!(
                    !listing_contains(&fs::read(dir.join(file))?, name)?,
                    "{name} appears in {file}"
                );
            }
            let workspace = dir.canonicalize()?.join("lifecycle/workspace");
            ensure!(
                intent.workspace == workspace.to_string_lossy(),
                "SBX smoke mounted another workspace"
            );
            let repo = dir.canonicalize()?.join("ready/repo");
            ensure!(
                report.ready_sandbox == sandbox_name(&repo, &b.ready_agent),
                "readiness sandbox name differs"
            );
        }
        Target::Ssh { destination } => {
            ensure!(
                b.destination.as_deref() == Some(destination.as_str()),
                "SBX smoke destination differs"
            );
            ensure!(
                fs::read_to_string(dir.join("lifecycle/host.txt"))? == *destination
                    && fs::read_to_string(dir.join("lifecycle/ssh-exit.txt"))? == "0",
                "Linux SSH command did not pass on the selected host"
            );
            ensure!(
                fs::read(dir.join("lifecycle/probe.sh"))? == SCRIPT.as_bytes(),
                "Linux payload differs"
            );
            ensure!(
                audit["root"] == "absent",
                "the owned Linux root survived the check"
            );
            let receipt: Value = read_json(&dir.join("lifecycle/receipt.json"))?;
            let root = remote_root(job, &report.sandbox);
            ensure!(
                receipt["schema"] == 1
                    && receipt["mode"] == "release"
                    && receipt["exit"] == 0
                    && receipt["blocked"] == ""
                    && receipt["guest_round_trip"] == true,
                "Linux guest smoke did not pass"
            );
            ensure!(
                receipt["target"]
                    == format!(
                        "linux/{}",
                        triple_parts(&b.triple).map(|t| t.1).unwrap_or_default()
                    ),
                "unsupported Linux smoke platform"
            );
            ensure!(
                receipt["sandbox"] == report.sandbox.as_str()
                    && receipt["ready_sandbox"] == report.ready_sandbox.as_str()
                    && receipt["root"] == root.as_str()
                    && intent.remote_root.as_deref() == Some(root.as_str()),
                "Linux sandbox identity differs"
            );
            ensure!(
                report.ready_sandbox
                    == sandbox_name(Path::new(&format!("{root}/repo")), &b.ready_agent),
                "readiness sandbox name differs"
            );
            ensure!(
                serde_json::from_value::<BTreeMap<String, i32>>(receipt["steps"].clone())?
                    == expected,
                "missing or failed Linux smoke step"
            );
            ensure!(
                receipt["cli_version"] == cli.version.as_str()
                    && receipt["cli_sha256"] == cli.sha256.as_str()
                    && receipt["cli_sha256_after"] == cli.sha256.as_str()
                    && receipt["asset_sha256"] == b.asset_sha256.as_str(),
                "Linux receipt identity differs from the report"
            );
            let field = |k: &str| -> Result<Vec<u8>> {
                b64_field(&receipt[k])?.with_context(|| format!("missing Linux {k}"))
            };
            ensure!(
                !listing_contains(&field("session")?, &report.sandbox)?,
                "Linux sandbox name was already occupied"
            );
            ensure!(
                !listing_contains(&field("cleanup")?, &report.sandbox)?,
                "Linux sandbox survived cleanup"
            );
            let inspection: Value = serde_json::from_slice(&field("inspection")?)?;
            ensure!(
                inspection.is_object() || inspection.is_array(),
                "unreadable Linux inspection"
            );
            check_nonce(&field("host_probe")?, &field("guest_probe")?)?;
            let ready_raw = &receipt["ready"];
            ensure!(
                ready_raw["ran"] == true && ready_raw["exit"] == 0 && ready_raw["error"] == "",
                "Linux readiness did not pass"
            );
            ensure!(
                b64_field(&ready_raw["stdout"])?.as_deref()
                    == Some(fs::read(dir.join("ready/ready.stdout"))?.as_slice()),
                "Linux readiness output differs from the receipt"
            );
            let pre =
                b64_field(&ready_raw["pre"])?.context("missing Linux readiness pre-listing")?;
            ensure!(
                !listing_contains(&pre, &report.ready_sandbox)?,
                "Linux readiness sandbox pre-existed"
            );
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "release_tests.rs"]
mod tests;
