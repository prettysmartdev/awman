//! Shared helpers for CLI per-command frontend impls.
//!
//! Each reads from stdin and nothing more. None of them tests whether stdin
//! is a terminal and none of them invents an answer when it is not: that is
//! the pattern F-50 step 1 removes. Whether this run can ask a human at all
//! is `CliFrontend::non_interactive` — one resolved flag, computed once from
//! Layer 2's `ResolvedFlags::is_non_interactive` — and *what to answer* when
//! it cannot is the CLI profile in `src/command/headless.rs`. An `ask_*` body
//! consults those two before reaching for anything here.
//!
//! Every helper returns `None` when stdin ends without an answer (EOF or a
//! read error), so a caller that has no headless answer to fall back on can
//! raise `CommandError::InteractiveInputUnavailable` rather than proceed on a
//! value nobody supplied.

use crate::data::step_status::StepStatus;

/// Read one line from stdin, or `None` at EOF / on a read error.
fn read_raw_line() -> Option<String> {
    let mut buf = String::new();
    match std::io::stdin().read_line(&mut buf) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(buf),
    }
}

/// Prompt with `[Y/n]` or `[y/N]` and read the answer.
///
/// `default_yes` is the answer an empty line accepts — the one shown in
/// upper case in the suffix. It is not a headless default: stdin ending
/// without an answer is `None`.
pub fn yes_no(prompt: &str, default_yes: bool) -> Option<bool> {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    eprintln!("awman: {prompt} {suffix}");
    let buf = read_raw_line()?;
    Some(match buf.trim() {
        "y" | "Y" => true,
        "n" | "N" => false,
        _ => default_yes,
    })
}

/// Ask a yes/no question that has no default at all, re-asking until the
/// answer is `y` or `n`. `None` at EOF / on a read error.
///
/// The form for a question nobody may answer on the user's behalf: the squad
/// interview's confirmations have no `HeadlessDefaults` row, and the TUI's
/// `DialogRequest::YesNo` has no Enter-default either, so neither frontend
/// invents one (F-19).
pub fn yes_no_required(prompt: &str) -> Option<bool> {
    loop {
        eprintln!("awman: {prompt} [y/n]");
        match read_raw_line()?.trim() {
            "y" | "Y" => return Some(true),
            "n" | "N" => return Some(false),
            _ => eprintln!("awman: please answer y or n."),
        }
    }
}

/// Read a single line from stdin and return the trimmed content, or `None`
/// at EOF / on a read error.
pub fn read_line(prompt: &str) -> Option<String> {
    eprintln!("awman: {prompt}");
    Some(read_raw_line()?.trim().to_string())
}

/// Read lines from stdin until a blank line or EOF (Ctrl+D), joined with
/// newlines. `None` only when stdin could not be read at all.
pub fn read_multiline(prompt: &str) -> Option<String> {
    use std::io::BufRead as _;
    eprintln!("awman: {prompt}");
    eprintln!("awman: (enter a blank line or press Ctrl+D when done)");
    let stdin = std::io::stdin();
    let mut lines: Vec<String> = Vec::new();
    let mut read_anything = false;
    for line in stdin.lock().lines() {
        match line {
            Ok(l) if l.is_empty() => {
                read_anything = true;
                break;
            }
            Ok(l) => {
                read_anything = true;
                lines.push(l);
            }
            Err(_) => break,
        }
    }
    read_anything.then(|| lines.join("\n"))
}

/// Present a numbered menu and return the 1-based index chosen.
///
/// `default` is what an empty or unparseable line accepts — the value shown
/// in the `Choice [n]:` hint. `None` at EOF / on a read error.
pub fn pick_numbered(prompt: &str, options: &[&str], default: usize) -> Option<usize> {
    eprintln!("awman: {prompt}");
    for (i, opt) in options.iter().enumerate() {
        eprintln!("  [{}] {opt}", i + 1);
    }
    eprint!("Choice [{}]: ", default);
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let buf = read_raw_line()?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return Some(default);
    }
    Some(trimmed.parse::<usize>().unwrap_or(default))
}

/// Puts the terminal in raw mode and restores it on drop, so an error or a
/// panic while a secret is being typed cannot leave the echo off.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Read one line from the terminal without echoing it. Esc and Ctrl-C
/// cancel; an empty line is `None` too. The text is held in a
/// [`SecretString`](crate::engine::auth::credential::SecretString) the moment
/// it is complete and is never printed or logged. Errors when the terminal
/// cannot enter raw mode (stdin is not a TTY).
pub fn read_secret_line(
    prompt: &str,
) -> std::io::Result<Option<crate::engine::auth::credential::SecretString>> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

    eprint!("awman: {prompt} (input hidden): ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut typed = crate::engine::auth::credential::SecretString::input_buffer();
    let cancelled = {
        let _raw = RawModeGuard::enable()?;
        loop {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Enter => break false,
                KeyCode::Esc => break true,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break true,
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    typed.push_char(c)
                }
                KeyCode::Backspace => {
                    typed.pop_char();
                }
                _ => {}
            }
        }
    };
    eprintln!();
    // Wrapping first means every path below drops (and so overwrites) it.
    let secret = typed;
    Ok((!cancelled && !secret.expose().is_empty()).then_some(secret))
}

pub fn step_status_label(status: &StepStatus) -> String {
    status.label()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::step_status::StepStatus;

    #[test]
    fn step_status_label_all_variants() {
        assert_eq!(step_status_label(&StepStatus::Pending), "pending");
        assert_eq!(step_status_label(&StepStatus::Running), "running");
        assert_eq!(step_status_label(&StepStatus::Done), "done");
        assert_eq!(step_status_label(&StepStatus::Skipped), "skipped");
        assert_eq!(
            step_status_label(&StepStatus::Failed(String::new())),
            "failed"
        );
        assert_eq!(
            step_status_label(&StepStatus::Failed("out of disk".into())),
            "failed: out of disk"
        );
    }
}

#[cfg(all(test, unix))]
mod wi0149_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    // Re-exec a single test inside a disposable PTY. This exercises crossterm's
    // real raw-mode/echo handling without replacing production input code.
    #[test]
    fn wi0149_cli_masked_read_child() {
        let Ok(case) = std::env::var("AWMAN_WI0149_SECRET_CHILD") else {
            return;
        };
        let before = crossterm::terminal::is_raw_mode_enabled().unwrap();
        let value = read_secret_line("WI0149 private input").unwrap();
        match case.as_str() {
            "submit" => assert_eq!(value.unwrap().expose(), "private_fake_0149X"),
            _ => assert!(value.is_none()),
        }
        assert_eq!(crossterm::terminal::is_raw_mode_enabled().unwrap(), before);
        eprintln!("MASKED_READ_OK");
    }
    #[test]
    fn wi0149_cli_masked_read_never_echoes_and_restores_terminal() {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        for (case, input) in [
            ("submit", "private_fake_0149Z\u{7f}X\r"),
            ("escape", "private_fake_0149\u{1b}"),
            ("interrupt", "private_fake_0149\u{3}"),
            ("empty", "\r"),
        ] {
            let pair = native_pty_system()
                .openpty(PtySize {
                    rows: 24,
                    cols: 100,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            let mut cmd = CommandBuilder::new(std::env::current_exe().unwrap());
            cmd.args([
                "--exact",
                "frontend::cli::per_command::helpers::wi0149_tests::wi0149_cli_masked_read_child",
                "--nocapture",
            ]);
            cmd.env("AWMAN_WI0149_SECRET_CHILD", case);
            let mut child = pair.slave.spawn_command(cmd).unwrap();
            drop(pair.slave);
            let mut reader = pair.master.try_clone_reader().unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut buf = [0; 512];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if tx
                        .send(String::from_utf8_lossy(&buf[..n]).into_owned())
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let mut output = String::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !output.contains("input hidden") {
                output.push_str(
                    &rx.recv_timeout(Duration::from_secs(3))
                        .expect("child must request secret input"),
                );
                assert!(Instant::now() < deadline);
            }
            std::thread::sleep(Duration::from_millis(75));
            let mut writer = pair.master.take_writer().unwrap();
            writer.write_all(input.as_bytes()).unwrap();
            writer.flush().unwrap();
            while !output.contains("MASKED_READ_OK") {
                match rx.recv_timeout(Duration::from_secs(3)) {
                    Ok(chunk) => output.push_str(&chunk),
                    Err(_) => {
                        child.kill().unwrap();
                        panic!("masked read stalled: {output}");
                    }
                }
                assert!(Instant::now() < deadline);
            }
            assert!(
                !output.contains("private_fake_0149"),
                "secret echoed: {output}"
            );
            assert!(child.wait().unwrap().success(), "{output}");
        }
    }
}

/// Raw keyed summary input allows SIGINT to become the command's exit 130.
pub fn read_prerequisite_report(
    prompt: &crate::data::prompt::Prompt<bool>,
) -> Result<bool, crate::command::error::CommandError> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    for choice in &prompt.choices {
        eprintln!("  [{}] {}", choice.key, choice.label);
    }
    let _raw = RawModeGuard::enable().map_err(|_| crate::command::error::CommandError::Aborted)?;
    loop {
        if let Event::Key(key) =
            event::read().map_err(|_| crate::command::error::CommandError::Aborted)?
        {
            match key.code {
                KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Err(crate::command::error::CommandError::Aborted)
                }
                KeyCode::Enter | KeyCode::Esc => {
                    return Ok(prompt.default_on_dismiss.unwrap_or(false))
                }
                KeyCode::Char(ch) => {
                    if let Some(answer) = prompt.answer_for_key(ch) {
                        return Ok(answer);
                    }
                }
                _ => {}
            }
        }
    }
}
