//! Optional Linux smoke over the same SSH destination convention as release.
use super::*;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::process::Command;
use std::time::{Duration, Instant};

const SCRIPT: &str = include_str!("linux.sh");

// Only relay progress lines; full SBX output stays in ssh.log. Preserve partial
// lines between polls so SSH packet boundaries cannot lose or duplicate a step.
fn forward_progress(
    reader: &mut impl BufRead,
    pending: &mut Vec<u8>,
    emit: impl FnMut(&str),
) -> Result<()> {
    forward_progress_with(reader, pending, "validate-sbx-linux: ", emit)
}

pub(super) fn forward_progress_with(
    reader: &mut impl BufRead,
    pending: &mut Vec<u8>,
    prefix: &str,
    mut emit: impl FnMut(&str),
) -> Result<()> {
    loop {
        if reader.read_until(b'\n', pending)? == 0 {
            return Ok(());
        }
        if pending.ends_with(b"\n") {
            let line = String::from_utf8_lossy(pending);
            let line = line.trim_end_matches(['\r', '\n']);
            if line.starts_with(prefix) {
                emit(line);
            }
            pending.clear();
        }
    }
}

fn ssh_command(destination: &str, name: &str, nonce: &str) -> Result<Command> {
    ensure!(
        crate::release::util::valid_ssh_destination(destination),
        "invalid LINUX_HOST"
    );
    ensure!(
        [name, nonce].iter().all(|s| !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')),
        "invalid remote smoke arguments"
    );
    ssh_command_with(
        Path::new("ssh"),
        destination,
        &["sh", "-s", "--", name, nonce],
    )
}

/// `program -T <fixed options> <destination> <remote...>`. ssh joins the
/// remote words with spaces for the remote shell, so every word is limited to
/// characters with no shell meaning (plus a lone `>` redirection).
pub(super) fn ssh_command_with(
    program: &Path,
    destination: &str,
    remote: &[&str],
) -> Result<Command> {
    ensure!(
        crate::release::util::valid_ssh_destination(destination),
        "invalid LINUX_HOST"
    );
    ensure!(
        !remote.is_empty()
            && remote.iter().all(|s| *s == ">"
                || (!s.is_empty()
                    && s.bytes().all(|b| b.is_ascii_alphanumeric()
                        || matches!(b, b'/' | b'.' | b'_' | b'-' | b'=')))),
        "invalid remote smoke arguments"
    );
    let mut command = Command::new(program);
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=8",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ClearAllForwardings=yes",
        destination,
    ]);
    command.args(remote);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    Ok(command)
}

pub(super) fn run(out: &Path, destination: &str, name: &str, nonce: &str) -> Result<()> {
    fs::create_dir(out)?;
    fs::write(out.join("probe.sh"), SCRIPT)?;
    fs::write(out.join("host.txt"), destination)?;
    let mut command = ssh_command(destination, name, nonce)?;
    command
        .stdin(fs::File::open(out.join("probe.sh"))?)
        .stdout(fs::File::create(out.join("receipt.json"))?)
        .stderr(fs::File::create(out.join("ssh.log"))?);
    eprintln!(
        "validate-sbx: Linux on {destination}; logs in {}",
        out.display()
    );
    let mut progress = BufReader::new(fs::File::open(out.join("ssh.log"))?);
    let mut pending = Vec::new();
    let mut progress_enabled = true;
    let started = Instant::now();
    let mut last_progress = started;
    let mut child = command
        .spawn()
        .context("cannot start ssh for Linux SBX validation")?;
    let deadline = Instant::now() + Duration::from_secs(900);
    let status = loop {
        if progress_enabled {
            if let Err(error) =
                forward_progress(&mut progress, &mut pending, |line| eprintln!("{line}"))
            {
                eprintln!("validate-sbx: live log unavailable ({error}); retaining SSH log");
                progress_enabled = false;
            }
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Linux SBX validation timed out; inspect linux/ssh.log");
        }
        if last_progress.elapsed() >= Duration::from_secs(10) {
            eprintln!(
                "validate-sbx [Linux {destination}]: still running ({}s)",
                started.elapsed().as_secs()
            );
            last_progress = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    forward_progress(&mut progress, &mut pending, |line| eprintln!("{line}"))?;
    eprintln!(
        "validate-sbx [Linux {destination}]: {} ({:.1}s)",
        if status.success() {
            "smoke finished; checking receipt"
        } else {
            "failed"
        },
        started.elapsed().as_secs_f64()
    );
    fs::write(
        out.join("ssh-exit.txt"),
        status.code().unwrap_or(-1).to_string(),
    )?;
    if matches!(status.code(), Some(2 | 255)) {
        return Err(crate::release::error::Stop::new(
            if status.code() == Some(255) {
                "linux-host-unreachable"
            } else {
                "linux-host-prerequisite"
            },
            "Linux SBX prerequisite unavailable; inspect linux/ssh.log",
        )
        .into());
    }
    ensure!(
        status.success(),
        "Linux SBX validation failed; inspect linux/ssh.log"
    );
    verify(out, destination, name)?;
    eprintln!("validate-sbx [Linux {destination}]: passed");
    Ok(())
}

pub(super) fn verify(out: &Path, destination: &str, expected_name: &str) -> Result<()> {
    ensure!(
        crate::release::util::valid_ssh_destination(destination),
        "invalid Linux destination"
    );
    ensure!(
        fs::read_to_string(out.join("host.txt"))? == destination,
        "Linux destination differs"
    );
    ensure!(
        fs::read_to_string(out.join("ssh-exit.txt"))? == "0",
        "Linux SSH command did not pass"
    );
    let receipt: Value = serde_json::from_slice(&fs::read(out.join("receipt.json"))?)?;
    ensure!(
        receipt["schema"] == 1 && receipt["exit"] == 0 && receipt["guest_round_trip"] == true,
        "Linux guest smoke did not pass"
    );
    ensure!(
        matches!(
            receipt["target"].as_str(),
            Some("linux/x86_64" | "linux/aarch64")
        ),
        "unsupported Linux smoke platform"
    );
    ensure!(
        receipt["cli_version"]
            .as_str()
            .is_some_and(live_cli_version)
            && receipt["cli_sha256"]
                .as_str()
                .is_some_and(digest::is_sha256),
        "missing Linux CLI identity"
    );
    let expected: BTreeMap<String, i32> = STEPS.iter().map(|s| ((*s).into(), 0)).collect();
    ensure!(
        serde_json::from_value::<BTreeMap<String, i32>>(receipt["steps"].clone())? == expected,
        "missing or failed Linux smoke step"
    );
    let name = receipt["sandbox"]
        .as_str()
        .context("missing Linux sandbox name")?;
    ensure!(name == expected_name, "Linux sandbox identity differs");
    ensure!(
        !crate::release::sbx::contains_name(&receipt["session"], name)?,
        "Linux sandbox name was already occupied"
    );
    ensure!(
        receipt["inspection"].is_object() || receipt["inspection"].is_array(),
        "unreadable Linux inspection"
    );
    ensure!(
        !crate::release::sbx::contains_name(&receipt["cleanup"], name)?,
        "Linux sandbox survived cleanup"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awman_test_support::{TestCase, TestSandbox};
    use serde_json::json;

    #[test]
    fn live_progress_forwards_partial_lines_once_and_keeps_raw_output_in_the_log() {
        let mut reader = std::io::Cursor::new(b"validate-sbx-linux: create star".to_vec());
        let mut pending = Vec::new();
        let mut lines = Vec::new();
        forward_progress(&mut reader, &mut pending, |line| {
            lines.push(line.to_owned())
        })
        .unwrap();
        assert!(lines.is_empty());
        reader
            .get_mut()
            .extend_from_slice(b"ted\nraw SBX output\n\xff\nvalidate-sbx-linux: create passed\n");
        forward_progress(&mut reader, &mut pending, |line| {
            lines.push(line.to_owned())
        })
        .unwrap();
        forward_progress(&mut reader, &mut pending, |line| {
            lines.push(line.to_owned())
        })
        .unwrap();
        assert_eq!(
            lines,
            [
                "validate-sbx-linux: create started",
                "validate-sbx-linux: create passed"
            ]
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn ssh_destination_cannot_inject_options_or_shell_commands() {
        for destination in [
            "-oProxyCommand=bad",
            "user@host;false",
            "user@host\nfalse",
            "user@$(false)",
        ] {
            assert!(ssh_command(destination, "awman-validate-test", "abc123").is_err());
        }
        assert!(ssh_command("user@host", "bad;name", "abc123").is_err());
        let c = ssh_command("user@host", "awman-validate-test", "abc123").unwrap();
        let args = c
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect::<Vec<_>>();
        assert!(args.contains(&"BatchMode=yes") && args.contains(&"ForwardAgent=no"));
        assert_eq!(
            &args[args.len() - 6..],
            &[
                "user@host",
                "sh",
                "-s",
                "--",
                "awman-validate-test",
                "abc123"
            ]
        );
    }

    #[test]
    fn linux_proof_needs_complete_passing_smoke_and_verified_cleanup() {
        let s = TestSandbox::for_test(TestCase::component("sbx_validation::linux::tests::linux_proof_needs_complete_passing_smoke_and_verified_cleanup"));
        fs::write(s.work().join("host.txt"), "user@host").unwrap();
        fs::write(s.work().join("ssh-exit.txt"), "0").unwrap();
        let steps: BTreeMap<String, i32> = STEPS.iter().map(|s| ((*s).into(), 0)).collect();
        let receipt = json!({"schema":1,"exit":0,"target":"linux/x86_64","sandbox":"awman-validate-test","cli_version":"0.54.1","cli_sha256":"a".repeat(64),"guest_round_trip":true,"steps":steps,"session":[],"inspection":{},"cleanup":[]});
        let write = |value: &Value| {
            fs::write(
                s.work().join("receipt.json"),
                serde_json::to_vec(value).unwrap(),
            )
            .unwrap()
        };
        write(&receipt);
        verify(&s.work(), "user@host", "awman-validate-test").unwrap();
        for (key, value) in [
            ("cleanup", json!({})),
            ("cleanup", json!([{"name":"awman-validate-test"}])),
            ("guest_round_trip", json!(false)),
            ("target", json!("windows/x86_64")),
            ("exit", json!(1)),
        ] {
            let mut bad = receipt.clone();
            bad[key] = value;
            write(&bad);
            assert!(verify(&s.work(), "user@host", "awman-validate-test").is_err());
        }
        let mut bad = receipt.clone();
        bad["steps"].as_object_mut().unwrap().remove("run");
        write(&bad);
        assert!(verify(&s.work(), "user@host", "awman-validate-test").is_err());
        write(&receipt);
        assert!(verify(&s.work(), "other@host", "awman-validate-test").is_err());
        assert!(verify(&s.work(), "user@host", "different-sandbox").is_err());
        fs::write(s.work().join("ssh-exit.txt"), "255").unwrap();
        assert!(verify(&s.work(), "user@host", "awman-validate-test").is_err());
        s.finish();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ssh_payload_runs_without_keys_and_its_trap_preserves_failure_and_cleanup() {
        use awman_test_support::Stdin;
        use std::os::unix::fs::PermissionsExt;
        let mut s = TestSandbox::for_test(TestCase::component("sbx_validation::linux::tests::ssh_payload_runs_without_keys_and_its_trap_preserves_failure_and_cleanup"));
        // A test-owned sbx fixture; never resolve or execute a real SBX binary.
        let fake = s.bin().join("sbx");
        fs::write(
            &fake,
            r#"#!/bin/sh
if test "$1" = --version; then printf 'sbx 0.54.1\n'; exit 0; fi
phase=$2
printf '%s\n' "$phase" >> "$AWMAN_SBX_FAKE_CALLS"
if test "$phase" = "$AWMAN_SBX_FAKE_FAIL"; then exit 1; fi
case "$phase" in
  ls) printf '[]\n' ;;
  inspect) printf '{}\n' ;;
  exec|run) cp .host-probe .guest-probe ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        s.register_exe("sbx", &fake);
        let calls = s.work().join("calls");
        for fault in ["", "run", "create", "ls"] {
            fs::write(&calls, b"").unwrap();
            let result = s
                .command("sh")
                .args(["-s", "--", "awman-validate-test", "abc123"])
                .stdin(Stdin::Bytes(SCRIPT.as_bytes().to_vec()))
                .env("AWMAN_SBX_FAKE_FAIL", fault)
                .env("AWMAN_SBX_FAKE_CALLS", &calls)
                .run();
            let receipt: Value = serde_json::from_slice(&result.stdout).unwrap();
            assert!(result
                .stderr_text()
                .contains("validate-sbx-linux: version started"));
            assert!(result.stderr_text().contains("validate-sbx-linux: cleanup"));
            assert_eq!(
                result.exit.success(),
                fault.is_empty(),
                "{}",
                result.stderr_text()
            );
            assert_eq!(receipt["guest_round_trip"], fault.is_empty());
            if fault == "ls" {
                assert_eq!(
                    result.exit,
                    awman_test_support::Exit::Code(2),
                    "signed-out/unavailable sessions are blocked prerequisites"
                );
            }
            let calls = fs::read_to_string(&calls).unwrap();
            if matches!(fault, "create" | "ls") {
                assert!(
                    !calls.lines().any(|s| s == "rm"),
                    "failed create must not authorize deleting someone else's sandbox"
                );
            } else {
                assert!(calls.ends_with("rm\nls\n"), "{calls}");
            }
            if fault.is_empty() {
                fs::write(s.work().join("host.txt"), "user@host").unwrap();
                fs::write(s.work().join("ssh-exit.txt"), "0").unwrap();
                fs::write(s.work().join("receipt.json"), &result.stdout).unwrap();
                verify(&s.work(), "user@host", "awman-validate-test").unwrap();
            }
        }
        s.finish();
    }
}
