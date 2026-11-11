//! The supervisor: environment, readiness, deadlines, process trees, fds.

use crate::{expect_panic, fake_cli_exe, leaky_sandbox, sandbox};
use awman_test_support::{wait_for, Exit, Ready, Stdin};
use std::collections::BTreeMap;
use std::time::Duration;

#[test]
fn child_environment_is_exactly_the_sandbox_map() {
    let sandbox = sandbox("process::child_environment_is_exactly_the_sandbox_map");
    let fake = sandbox.install_fake(fake_cli_exe(), "envdump");
    fake.on(&[]).stdout("ok\n");
    let out = sandbox.command("envdump").env("EXTRA", "1").run();
    out.assert_success();
    let call = &fake.calls()[0];
    let mut expected: BTreeMap<String, String> = sandbox
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    expected.insert("EXTRA".into(), "1".into());
    assert_eq!(call.env, expected);
    assert_eq!(call.cwd, sandbox.work());
}

#[test]
fn env_remove_and_current_dir_apply() {
    let sandbox = sandbox("process::env_remove_and_current_dir_apply");
    let fake = sandbox.install_fake(fake_cli_exe(), "tool");
    fake.on(&[]).exit(0);
    sandbox
        .command("tool")
        .env_remove("TZ")
        .current_dir(sandbox.tmp())
        .run()
        .assert_success();
    let call = &fake.calls()[0];
    assert!(!call.env.contains_key("TZ"));
    assert_eq!(call.cwd, sandbox.tmp());
}

#[test]
fn stdin_is_null_by_default_and_bytes_are_delivered() {
    let sandbox = sandbox("process::stdin_is_null_by_default_and_bytes_are_delivered");
    let fake = sandbox.install_fake(fake_cli_exe(), "reader");
    fake.on(&[]).read_stdin();
    sandbox.command("reader").run().assert_success();
    sandbox
        .command("reader")
        .stdin(Stdin::Bytes(b"hello".to_vec()))
        .run()
        .assert_success();
    let calls = fake.calls();
    assert_eq!(calls[0].stdin, b"");
    assert_eq!(calls[1].stdin, b"hello");
}

#[test]
fn piped_stdin_reaches_the_child() {
    let sandbox = sandbox("process::piped_stdin_reaches_the_child");
    let fake = sandbox.install_fake(fake_cli_exe(), "reader");
    fake.on(&[]).read_stdin();
    let mut child = sandbox.command("reader").stdin(Stdin::Piped).spawn();
    child.stdin().write_all(b"typed").unwrap();
    child.close_stdin();
    assert_eq!(child.wait(Duration::from_secs(10)), Exit::Code(0));
    assert_eq!(fake.calls()[0].stdin, b"typed");
}

#[test]
fn spawn_waits_for_a_ready_line() {
    let sandbox = sandbox("process::spawn_waits_for_a_ready_line");
    let fake = sandbox.install_fake(fake_cli_exe(), "server");
    fake.on(&[])
        .stdout("booting\nlistening on 127.0.0.1:0\n")
        .hold_until_released();
    // The fake prints only after release: release from another thread once held.
    let handle = {
        let fake = fake.clone();
        std::thread::spawn(move || {
            fake.wait_until_held(Duration::from_secs(10));
            fake.release();
        })
    };
    let mut child = sandbox
        .command("server")
        .ready(Ready::StdoutLine("listening on".into()))
        .spawn();
    assert!(child.stdout_so_far().contains("listening on 127.0.0.1:0"));
    handle.join().unwrap();
    assert_eq!(child.wait(Duration::from_secs(10)), Exit::Code(0));
}

#[test]
fn readiness_failure_panics_with_the_output() {
    let sandbox = sandbox("process::readiness_failure_panics_with_the_output");
    let fake = sandbox.install_fake(fake_cli_exe(), "broken");
    fake.on(&[]).stderr("config error: nope\n").exit(2);
    let text = expect_panic(|| {
        let _child = sandbox
            .command("broken")
            .label("broken server")
            .ready(Ready::StdoutLine("ready".into()))
            .spawn();
    });
    assert!(text.contains("`broken server` was not ready"), "{text}");
    assert!(text.contains("config error: nope"), "{text}");
}

#[test]
fn file_readiness_detects_early_exit() {
    let sandbox = sandbox("process::file_readiness_detects_early_exit");
    let fake = sandbox.install_fake(fake_cli_exe(), "quitter");
    fake.on(&[]).exit(3);
    let marker = sandbox.tmp().join("ready");
    let text = expect_panic(|| {
        let _child = sandbox
            .command("quitter")
            .ready(Ready::FileExists(marker.clone()))
            .spawn();
    });
    assert!(text.contains("it ended (exit code 3)"), "{text}");
}

/// Guards: 776e56d6, 58a1dadb, e1f959c0 hanging tests.
#[test]
fn deadline_kills_the_whole_tree() {
    let sandbox = sandbox("process::deadline_kills_the_whole_tree");
    let fake = sandbox.install_fake(fake_cli_exe(), "hang");
    fake.on(&[]).spawn_child().hold_until_released();
    let out = sandbox
        .command("hang")
        .deadline(Duration::from_millis(500))
        .run();
    assert_eq!(out.exit, Exit::KilledAtDeadline);
    let grandchild = fake.calls()[0].child_pid.expect("grandchild pid");
    wait_for("the grandchild to be gone", Duration::from_secs(5), || {
        (!alive(grandchild)).then_some(())
    });
}

#[test]
fn dropping_a_child_kills_its_tree() {
    let sandbox = sandbox("process::dropping_a_child_kills_its_tree");
    let fake = sandbox.install_fake(fake_cli_exe(), "daemonish");
    fake.on(&[]).spawn_child().hold_until_released();
    let child = sandbox.command("daemonish").spawn();
    let call = fake.wait_until_held(Duration::from_secs(10));
    let pids = [child.pid(), call.child_pid.unwrap()];
    drop(child);
    for pid in pids {
        wait_for("the tree to be gone", Duration::from_secs(5), || {
            (!alive(pid)).then_some(())
        });
    }
}

#[test]
fn wait_expiry_kills_and_panics() {
    let sandbox = sandbox("process::wait_expiry_kills_and_panics");
    let fake = sandbox.install_fake(fake_cli_exe(), "slow");
    fake.on(&[]).hold_until_released();
    let mut child = sandbox.command("slow").label("slow tool").spawn();
    let pid = child.pid();
    let text = expect_panic(|| {
        child.wait(Duration::from_millis(200));
    });
    assert!(text.contains("`slow tool` did not exit"), "{text}");
    assert!(!alive(pid));
}

#[test]
fn interrupt_and_terminate_reach_the_child() {
    let sandbox = sandbox("process::interrupt_and_terminate_reach_the_child");
    let fake = sandbox.install_fake(fake_cli_exe(), "sleepy");
    fake.on(&[]).hold_until_released();
    let mut child = sandbox.command("sleepy").spawn();
    fake.wait_until_held(Duration::from_secs(10));
    child.terminate();
    let exit = child.wait(Duration::from_secs(10));
    if cfg!(unix) {
        assert_eq!(exit, Exit::Signal(15));
    } else {
        assert!(!exit.success());
    }
}

#[test]
fn leader_exit_with_running_descendants_is_a_leak() {
    let (_parent, sandbox) =
        leaky_sandbox("process::leader_exit_with_running_descendants_is_a_leak");
    let fake = sandbox.install_fake(fake_cli_exe(), "forker");
    fake.on(&[]).spawn_child();
    let out = sandbox.command("forker").run();
    assert_eq!(out.exit, Exit::Code(0));
    let grandchild = fake.calls()[0].child_pid.unwrap();
    let text = expect_panic(|| sandbox.finish());
    assert!(text.starts_with("LEAK: "), "{text}");
    assert!(
        text.contains("left processes running in its group"),
        "{text}"
    );
    wait_for(
        "the leaked grandchild to be killed",
        Duration::from_secs(5),
        || (!alive(grandchild)).then_some(()),
    );
}

/// Guards: e6a2d429, e6322996, f5b724fc fork-inherited private-file and build locks.
#[cfg_attr(not(unix), ignore = "not-applicable: descriptor listing uses /dev/fd")]
#[test]
fn children_inherit_only_stdio() {
    let sandbox = sandbox("process::children_inherit_only_stdio");
    // A descriptor the parent deliberately leaves inheritable.
    let file = std::fs::File::create(sandbox.tmp().join("lock")).unwrap();
    #[cfg(unix)]
    let _inheritable = nix::unistd::dup(&file).unwrap();
    let out = sandbox.command(fake_cli_exe()).arg("--fake-list-fds").run();
    out.assert_success();
    assert_eq!(out.stdout_text().trim(), "", "child saw extra descriptors");
}

/// Guards: 39fe0e05, 72537ed5, d5043695.
#[test]
fn many_concurrent_installs_and_spawns_never_hit_text_file_busy() {
    let threads: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || {
                let sandbox = sandbox(
                    "process::many_concurrent_installs_and_spawns_never_hit_text_file_busy",
                );
                let fake = sandbox.install_fake(fake_cli_exe(), &format!("tool{i}"));
                fake.on(&[]).stdout("ok\n");
                for _ in 0..5 {
                    sandbox.command(format!("tool{i}")).run().assert_success();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

#[test]
fn output_is_drained_to_proc_files() {
    let sandbox = sandbox("process::output_is_drained_to_proc_files");
    let fake = sandbox.install_fake(fake_cli_exe(), "talker");
    fake.on(&[]).stdout("to stdout\n").stderr("to stderr\n");
    let out = sandbox.command("talker").run();
    assert_eq!(out.stdout_text(), "to stdout\n");
    assert_eq!(out.stderr_text(), "to stderr\n");
    let procs: Vec<_> = std::fs::read_dir(sandbox.root().join("proc"))
        .unwrap()
        .flatten()
        .collect();
    assert!(procs
        .iter()
        .any(|e| std::fs::read_to_string(e.path()).unwrap_or_default() == "to stdout\n"));
}

#[test]
fn large_output_is_bounded_and_marked() {
    let sandbox = sandbox("process::large_output_is_bounded_and_marked");
    let fake = sandbox.install_fake(fake_cli_exe(), "flood");
    fake.on(&[]).stdout(&"x".repeat(5000));
    let out = sandbox.command("flood").max_output(1000).run();
    let text = out.stdout_text();
    assert!(text.starts_with(&"x".repeat(1000)));
    assert!(text.contains("output truncated"));
}

#[test]
fn programs_must_be_on_the_sandbox_path() {
    let sandbox = sandbox("process::programs_must_be_on_the_sandbox_path");
    let text = expect_panic(|| {
        sandbox.command("definitely-not-a-tool-xyz").run();
    });
    assert!(text.contains("not on the sandbox PATH"), "{text}");
}

#[test]
fn unregistered_executables_inside_the_sandbox_are_refused() {
    let sandbox = sandbox("process::unregistered_executables_inside_the_sandbox_are_refused");
    let script = sandbox.tmp().join("tool");
    std::fs::copy(fake_cli_exe(), &script).unwrap();
    let text = expect_panic(|| {
        sandbox.command(&script).run();
    });
    assert!(
        text.contains("was not installed with exe::install"),
        "{text}"
    );
}

#[test]
fn agents_are_refused_directly_and_through_interpreters() {
    let sandbox = sandbox("process::agents_are_refused_directly_and_through_interpreters");
    let text = expect_panic(|| {
        sandbox.command("sh").args(["-c", "claude --version"]).run();
    });
    assert!(text.starts_with("REFUSED: "), "{text}");
    let text = expect_panic(|| {
        sandbox
            .command("sh")
            .args(["-c", "exec /usr/local/bin/codex"])
            .run();
    });
    assert!(text.starts_with("REFUSED: "), "{text}");
    // A fake installed under an agent's name is allowed: it is not an agent.
    let fake = sandbox.install_fake(fake_cli_exe(), "claude");
    fake.on(&[]).stdout("fake claude\n");
    assert_eq!(
        sandbox.command("claude").run().stdout_text(),
        "fake claude\n"
    );
}

#[test]
fn system_tools_run_with_the_sandbox_environment() {
    let sandbox = sandbox("process::system_tools_run_with_the_sandbox_environment");
    let out = sandbox.command("sh").args(["-c", "echo $HOME"]).run();
    out.assert_success();
    assert_eq!(out.stdout_text().trim(), sandbox.home().to_string_lossy());
}

fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // A zombie still "exists" for kill(0); read the /proc state on Linux.
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => !stat.split(") ").nth(1).is_some_and(|s| s.starts_with('Z')),
            Err(_) => nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}
