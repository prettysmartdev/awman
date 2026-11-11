//! Processes under a pseudo-terminal.

use crate::sandbox;
use awman_test_support::Exit;
use std::time::Duration;

#[cfg_attr(not(unix), ignore = "not-applicable: uses sh")]
#[test]
fn pty_child_sees_a_terminal_and_receives_input() {
    let sandbox = sandbox("pty::pty_child_sees_a_terminal_and_receives_input");
    let mut child = sandbox
        .pty_command("sh")
        .args([
            "-c",
            "if [ -t 0 ]; then echo tty; fi; read line; echo got:$line",
        ])
        .spawn();
    child.wait_for_output("tty", Duration::from_secs(10));
    child.write_all(b"hello\n");
    child.wait_for_output("got:hello", Duration::from_secs(10));
    assert_eq!(child.wait(Duration::from_secs(10)), Exit::Code(0));
}

#[cfg_attr(not(unix), ignore = "not-applicable: uses sh")]
#[test]
fn pty_deadline_kills_the_child() {
    let sandbox = sandbox("pty::pty_deadline_kills_the_child");
    let mut child = sandbox
        .pty_command("sh")
        .args(["-c", "read never"])
        .deadline(Duration::from_millis(300))
        .spawn();
    assert_eq!(child.wait(Duration::from_secs(10)), Exit::KilledAtDeadline);
}

/// A process exit must include its final transcript bytes, without a separate
/// wait_for_output that hides a race between waitpid and the capture reader.
#[cfg_attr(
    not(unix),
    ignore = "not-applicable: Unix terminal EOF semantics; Windows ConPTY remains P9"
)]
#[test]
fn wait_drains_final_output_before_returning_the_exit_status() {
    let sandbox = sandbox("pty::wait_drains_final_output_before_returning_the_exit_status");
    for index in 0..80 {
        let marker = format!("terminal-final-{index}");
        let mut child = sandbox
            .pty_command("sh")
            .args(["-c", "printf '%s\\n' \"$1\"", "fixture", &marker])
            .spawn();
        assert_eq!(child.wait(Duration::from_secs(5)), Exit::Code(0));
        assert!(
            child.output_so_far().contains(&marker),
            "lost final bytes for {marker}"
        );
    }
    sandbox.finish();
}
