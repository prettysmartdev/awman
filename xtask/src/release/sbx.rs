//! Isolated SBX qualification and exact-case evidence. Never reads or records key bytes.
use super::{
    error::Stop,
    util::{is_sha256, read_json, sha256, sha256_file},
};
use crate::manifest::release_policy::{native_filter, native_tests, SBX_CASES, SBX_NATIVE};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const INPUTS: [&str; 3] = [
    "AWMAN_TEST_SBX_HOME",
    "AWMAN_TEST_SBX_IDENTITY",
    "AWMAN_TEST_SBX_ANTHROPIC_KEY_FILE",
];
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Qualification {
    pub cli_version: String,
    pub operator_label: String,
    pub inputs_sha256: String,
    pub cli_sha256: String,
}
pub fn inputs_digest(version: &str, operator_label: &str) -> Result<String> {
    Ok(sha256(&serde_json::to_vec(
        &json!({"cli_version":version,"operator_label":operator_label,
        "suite_filter":native_filter(SBX_NATIVE)?,"test_ids":native_tests(SBX_NATIVE)?}),
    )?))
}
pub fn version(raw: &str) -> Option<String> {
    raw.split(|c: char| !c.is_ascii_digit() && c != '.')
        .find_map(|part| {
            let v: Vec<_> = part.split('.').map(str::parse::<u64>).collect();
            match v.as_slice() {
                [Ok(a), Ok(b), Ok(c)] => Some((*a, *b, *c)),
                _ => None,
            }
        })
        .filter(|v| *v >= (0, 53, 0))
        .map(|(a, b, c)| format!("{a}.{b}.{c}"))
}
fn blocked(code: &str) -> anyhow::Error {
    Stop::new(code, format!("SBX qualification: {code}")).into()
}
fn input(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| blocked("sbx-identity-missing"))
}
fn private(path: &Path, directory: bool, mode: u32) -> Result<()> {
    ensure!(path.is_absolute(), "absolute isolated path required");
    #[cfg(not(unix))]
    let _ = mode;
    let m = std::fs::symlink_metadata(path)?;
    ensure!(
        !m.file_type().is_symlink() && m.is_dir() == directory && (directory || m.is_file()),
        "isolated path has wrong type"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            m.mode() & 0o777 == mode && m.uid() == nix::unistd::geteuid().as_raw(),
            "isolated path permissions/owner differ"
        );
    }
    Ok(())
}
fn executable() -> Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join("sbx"))
        .find(|p| p.is_file())
        .and_then(|p| p.canonicalize().ok())
        .ok_or_else(|| blocked("no-sbx-cli"))
}
/// No inherited daemon/config/credential overrides; the dedicated HOME is the identity.
fn command(home: &Path) -> Result<Command> {
    let mut c = Command::new(executable()?);
    c.env_clear()
        .current_dir(home)
        .env("HOME", home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default());
    c.stdin(Stdio::null());
    Ok(c)
}
/// File-backed output avoids pipe deadlocks; every command has an owned group and deadline.
fn bounded(mut c: Command, seconds: u64) -> Result<(bool, Vec<u8>)> {
    let out = tempfile::tempfile()?;
    c.stdout(out.try_clone()?).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    let mut child = c.spawn()?;
    let until = Instant::now() + Duration::from_secs(seconds);
    let success = loop {
        if let Some(s) = child.try_wait()? {
            break s.success();
        }
        if Instant::now() >= until {
            #[cfg(unix)]
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = child.kill();
            let _ = child.wait();
            return Err(blocked("timeout"));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    use std::io::{Read, Seek};
    let mut out = out;
    out.rewind()?;
    let mut bytes = Vec::new();
    out.take(1024 * 1024).read_to_end(&mut bytes)?;
    Ok((success, bytes))
}
fn validate_inputs() -> Result<PathBuf> {
    let home = PathBuf::from(input(INPUTS[0])?);
    let _label = input(INPUTS[1])?;
    let key = PathBuf::from(input(INPUTS[2])?);
    private(&home, true, 0o700)
        .and_then(|_| private(&key, false, 0o600))
        .map_err(|_| blocked("sbx-identity-missing"))?;
    let mut personal_homes = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        personal_homes.push(PathBuf::from(home));
    }
    #[cfg(unix)]
    if let Some(user) = nix::unistd::User::from_uid(nix::unistd::geteuid())? {
        personal_homes.push(user.dir);
    }
    isolated_home(&home, &personal_homes)?;
    Ok(home)
}

fn cli_digest() -> Result<String> {
    sha256_file(&executable()?)
}

pub fn probe() -> Result<Qualification> {
    let home = validate_inputs()?;
    let identity = input(INPUTS[1])?;
    let cli_sha256 = cli_digest()?;
    let empty = tempfile::tempdir()?;
    qualify(&identity, &cli_sha256, |request| {
        let mut c = match request {
            Probe::Version => {
                let mut c = command(&home)?;
                c.arg("--version");
                c
            }
            Probe::SignedIn => {
                let mut c = command(&home)?;
                c.args(["--no-input", "ls", "--json"]);
                c
            }
            Probe::EmptyHome => {
                let mut c = command(empty.path())?;
                c.args(["--no-input", "ls", "--json"]);
                c
            }
            Probe::EmptyHomeSecrets | Probe::DedicatedSecrets => {
                let mut c = command(if matches!(request, Probe::EmptyHomeSecrets) {
                    empty.path()
                } else {
                    &home
                })?;
                c.args(["--no-input", "secret", "ls", "--global", "--json"]);
                c
            }
            Probe::Network(host) => {
                let mut c = Command::new("curl");
                c.env_clear()
                    .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                    .args([
                        "--disable",
                        "--silent",
                        "--output",
                        "/dev/null",
                        "--connect-timeout",
                        "5",
                        "--max-time",
                        "10",
                        &format!("https://{host}:443/"),
                    ]);
                c
            }
        };
        c.stdin(Stdio::null());
        bounded(
            c,
            match request {
                Probe::Version => 15,
                Probe::Network(_) => 12,
                _ => 30,
            },
        )
    })
}
#[derive(Clone, Copy)]
enum Probe {
    Version,
    SignedIn,
    EmptyHome,
    EmptyHomeSecrets,
    DedicatedSecrets,
    Network(&'static str),
}
/// The negative control is required even when the dedicated identity works.
fn qualify(
    operator_label: &str,
    cli_sha256: &str,
    mut run: impl FnMut(Probe) -> Result<(bool, Vec<u8>)>,
) -> Result<Qualification> {
    let (ok, bytes) =
        run(Probe::Version).map_err(|e| preserve_timeout(e, "sbx-cli-incompatible"))?;
    let cli_version = if ok {
        version(&String::from_utf8_lossy(&bytes))
    } else {
        None
    }
    .ok_or_else(|| blocked("sbx-cli-incompatible"))?;
    ensure_probe(
        run(Probe::SignedIn)
            .map_err(|e| preserve_timeout(e, "sbx-not-signed-in"))?
            .0,
        "sbx-not-signed-in",
    )?;
    ensure_probe(
        !run(Probe::EmptyHome)
            .map_err(|e| preserve_timeout(e, "sbx-identity-not-isolated"))?
            .0,
        "sbx-identity-not-isolated",
    )?;
    for request in [Probe::EmptyHomeSecrets, Probe::DedicatedSecrets] {
        let (ok, bytes) =
            run(request).map_err(|e| preserve_timeout(e, "sbx-identity-not-isolated"))?;
        // Release requires an empty global store to qualify scoped injection.
        let listing = serde_json::from_slice::<Value>(&bytes).ok();
        let empty = listing
            .as_ref()
            .is_some_and(|v| v.as_array().is_some_and(Vec::is_empty));
        ensure_probe(
            if matches!(request, Probe::DedicatedSecrets) {
                ok && empty
            } else {
                !ok || empty
            },
            "sbx-identity-not-isolated",
        )?;
    }
    for host in ["docker.io", "api.anthropic.com"] {
        ensure_probe(run(Probe::Network(host)).is_ok_and(|v| v.0), "sbx-network")?;
    }
    Ok(Qualification {
        inputs_sha256: inputs_digest(&cli_version, operator_label)?,
        cli_version,
        operator_label: operator_label.into(),
        cli_sha256: cli_sha256.into(),
    })
}

fn isolated_home(home: &Path, personal_homes: &[PathBuf]) -> Result<()> {
    let home = home.canonicalize()?;
    for personal in personal_homes {
        let personal = personal
            .canonicalize()
            .map_err(|_| blocked("sbx-identity-not-isolated"))?;
        ensure_probe(
            home != personal && !personal.starts_with(&home),
            "sbx-identity-not-isolated",
        )?;
    }
    Ok(())
}
fn preserve_timeout(e: anyhow::Error, code: &str) -> anyhow::Error {
    if e.downcast_ref::<Stop>()
        .is_some_and(|s| s.code == "timeout")
    {
        e
    } else {
        blocked(code)
    }
}

fn ensure_probe(ok: bool, code: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(blocked(code))
    }
}
/// Apply only to the SBX run after generic isolation has cleared inherited credentials.
pub fn grant(c: &mut Command) -> Result<()> {
    for name in INPUTS {
        c.env(name, input(name)?);
    }
    c.env("HOME", input(INPUTS[0])?)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME");
    Ok(())
}
/// Reserve private workspaces and their production sandbox names before any test starts.
/// Artifact tests read sbx-owned.json and initialise these directories as fresh Git repos.
pub fn allocate(out: &Path, root: &Path) -> Result<()> {
    let root = root.canonicalize()?;
    let mut cases = Vec::new();
    for (agent, _, _, _) in SBX_CASES {
        let workspace = root.join(format!("sbx-{agent}/workspace"));
        super::util::private_dir(&workspace)?;
        let workspace = workspace.canonicalize()?;
        cases.push(
            json!({"agent":agent,"sandbox":sandbox_name(&workspace, agent),"workspace":workspace}),
        );
    }
    super::util::write_json(
        &out.join("sbx-owned.json"),
        &json!({"schema":1,"root":root,"cases":cases}),
    )?;
    Ok(())
}
/// Stable public naming contract (src/engine/sandbox/naming.rs). No product linkage
/// is available in a relocatable artifact kit; tests exercise that the asset agrees.
fn sandbox_name(workspace: &Path, agent: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in workspace.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("awman-{:08x}-{agent}", hash & 0xffff_ffff)
}
/// Only reserved names from this allocation may be removed, including after a kill.
pub fn cleanup(out: &Path, root: &Path) -> Result<bool> {
    if !out.join("sbx-owned.json").exists() {
        return Ok(false);
    }
    let owned: Value = read_json(&out.join("sbx-owned.json"))?;
    let expected_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    ensure!(
        owned["root"] == expected_root.to_string_lossy().as_ref(),
        "SBX ownership root differs"
    );
    let names = owned_cases(&owned)?;
    let home = PathBuf::from(input(INPUTS[0])?);
    let mut leaked = false;
    for (_, name) in names {
        let mut c = command(&home)?;
        c.args(["--no-input", "ls", "--json"]);
        let (ok, bytes) = bounded(c, 20)?;
        ensure!(ok, "cannot verify owned SBX cleanup");
        let listing: Value = serde_json::from_slice(&bytes)?;
        if contains_name(&listing, &name)? {
            leaked = true;
            super::util::write_atomic(&out.join("sbx-leak"), name.as_bytes())?;
            let mut c = command(&home)?;
            c.args(["--no-input", "rm", "--force", &name]);
            ensure!(bounded(c, 30)?.0, "owned SBX sandbox removal failed");
            let mut c = command(&home)?;
            c.args(["--no-input", "ls", "--json"]);
            let (ok, bytes) = bounded(c, 20)?;
            ensure!(
                ok && !contains_name(&serde_json::from_slice(&bytes)?, &name)?,
                "owned SBX sandbox still exists"
            );
        }
    }
    Ok(leaked || out.join("sbx-leak").exists())
}
pub fn owned_cases(owned: &Value) -> Result<Vec<(String, String)>> {
    ensure!(owned["schema"] == 1, "invalid SBX ownership schema");
    let root = Path::new(owned["root"].as_str().context("SBX ownership root")?);
    ensure!(root.is_absolute(), "invalid SBX ownership root");
    let cases = owned["cases"].as_array().context("SBX ownership cases")?;
    ensure!(
        cases.len() == SBX_CASES.len(),
        "SBX ownership case count differs"
    );
    SBX_CASES
        .iter()
        .map(|(agent, _, _, _)| {
            let rows: Vec<_> = cases.iter().filter(|c| c["agent"] == *agent).collect();
            ensure!(rows.len() == 1, "SBX ownership agent differs");
            let workspace = Path::new(
                rows[0]["workspace"]
                    .as_str()
                    .context("SBX ownership workspace")?,
            );
            ensure!(
                workspace == root.join(format!("sbx-{agent}/workspace"))
                    && rows[0]["sandbox"] == sandbox_name(workspace, agent),
                "SBX ownership name or scope differs"
            );
            Ok((agent.to_string(), sandbox_name(workspace, agent)))
        })
        .collect()
}
/// Match the contract's array / sandboxes envelope; unknown JSON is never absence.
pub(crate) fn contains_name(v: &Value, name: &str) -> Result<bool> {
    let rows = match v {
        Value::Array(rows) => rows,
        other => match (other.get("sandboxes"), other.get("Sandboxes")) {
            (Some(value), None) | (None, Some(value)) => {
                value.as_array().context("invalid SBX listing envelope")?
            }
            _ => anyhow::bail!("unreadable or ambiguous SBX sandbox listing"),
        },
    };
    let names = rows
        .iter()
        .map(|row| match (row.get("name"), row.get("Name")) {
            (Some(value), None) | (None, Some(value)) => value
                .as_str()
                .filter(|n| {
                    !n.is_empty()
                        && n.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                })
                .context("SBX listing row has invalid name"),
            _ => anyhow::bail!("SBX listing row has ambiguous or missing name"),
        })
        .collect::<Result<Vec<_>>>()?;
    let unique: std::collections::BTreeSet<_> = names.iter().collect();
    ensure!(unique.len() == names.len(), "duplicate SBX listing names");
    Ok(names.contains(&name))
}
pub fn collect(out: &Path, q: &Qualification) -> Result<Value> {
    let mut sbx = serde_json::to_value(q)?;
    sbx["ownership_sha256"] = json!(sha256_file(&out.join("sbx-owned.json"))?);
    sbx["cases"] = json!(SBX_CASES
        .iter()
        .map(|(agent, _, _, _)| {
            let file = format!("sbx-smoke.{agent}.json");
            Ok(json!({"sha256":sha256_file(&out.join(&file))?,"file":file}))
        })
        .collect::<Result<Vec<Value>>>()?);
    verify(out, &sbx)?;
    Ok(sbx)
}
pub fn verify(out: &Path, sbx: &Value) -> Result<()> {
    ensure!(
        !out.join("sbx-leak").exists(),
        "owned SBX sandbox leak voided the result"
    );
    let q: Qualification = serde_json::from_value(sbx.clone())?;
    ensure!(
        version(&q.cli_version).as_deref() == Some(&q.cli_version)
            && !q.operator_label.trim().is_empty()
            && is_sha256(&q.cli_sha256)
            && q.inputs_sha256 == inputs_digest(&q.cli_version, &q.operator_label)?,
        "SBX qualification inputs differ"
    );
    ensure!(
        sbx["ownership_sha256"] == sha256_file(&out.join("sbx-owned.json"))?,
        "SBX ownership digest differs"
    );
    let owned: Value = read_json(&out.join("sbx-owned.json"))?;
    let owned = owned_cases(&owned)?;
    let cases = sbx["cases"].as_array().context("missing SBX cases")?;
    ensure!(cases.len() == SBX_CASES.len(), "SBX case count differs");
    for (agent, test, disposition, kit) in SBX_CASES {
        let file = format!("sbx-smoke.{agent}.json");
        let rows: Vec<_> = cases.iter().filter(|c| c["file"] == file).collect();
        ensure!(
            rows.len() == 1 && rows[0]["sha256"] == sha256_file(&out.join(&file))?,
            "SBX case digest differs"
        );
        let v: Value = read_json(&out.join(&file))?;
        ensure!(
            v["schema"] == 1
                && v["test"] == format!("awman-cli::artifact sbx_smoke::{test}")
                && v["agent"] == *agent
                && v["disposition"] == *disposition
                && v["kit_handle"] == *kit
                && v["workload"]
                    == if *disposition == "owned-workload" {
                        Value::Null
                    } else {
                        json!(agent)
                    }
                && v["kit_digest"].as_str().is_some_and(is_sha256)
                && v["sandbox"] == owned.iter().find(|(a, _)| a == agent).unwrap().1
                && v["sbx_cli"] == q.cli_version
                && v["ready_exit"] == 0
                && v["task_exit"] == 0
                && v["output_bytes"].as_u64().is_some_and(|n| n > 0)
                && v["removed"] == true,
            "SBX smoke case did not pass its exact contract: {agent}"
        );
        ensure!(
            v["inspect_sha256"].is_null() || v["inspect_sha256"].as_str().is_some_and(is_sha256),
            "invalid SBX inspect identity"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
