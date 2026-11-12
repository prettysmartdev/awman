//! Shared helpers for CLI per-command frontend impls.
//!
//! Each reads from stdin and nothing more. None of them tests whether stdin
//! is a terminal and none of them invents an answer when it is not: that is
//! the pattern F-50 step 1 removes. Whether this run can ask a human at all
//! is `CliFrontend::non_interactive` — one resolved flag, computed once from
//! Layer 2's `ResolvedFlags::is_headless` — and *what to answer* when
//! it cannot is the CLI profile in `src/command/headless.rs`. An `ask_*` body
//! consults those two before reaching for anything here.
//!
//! Every helper returns `None` when stdin ends without an answer (EOF or a
//! read error), so a caller that has no headless answer to fall back on can
//! raise `CommandError::InteractiveInputUnavailable` rather than proceed on a
//! value nobody supplied.

use crate::data::step_status::StepStatus;

thread_local! {
    static RELAY_INTERRUPT: std::cell::RefCell<Option<tokio_util::sync::CancellationToken>> = const { std::cell::RefCell::new(None) };
}

/// Scope the existing input renderers to one relayed dialog. Polling terminal
/// input lets Dispatch interrupt a prompt while it continues reading frames.
pub(crate) struct RelayInputGuard(Option<tokio_util::sync::CancellationToken>);

impl RelayInputGuard {
    pub(crate) fn new(token: &tokio_util::sync::CancellationToken) -> Self {
        Self(RELAY_INTERRUPT.with(|current| current.replace(Some(token.clone()))))
    }
}

impl Drop for RelayInputGuard {
    fn drop(&mut self) {
        RELAY_INTERRUPT.with(|current| current.replace(self.0.take()));
    }
}

fn relay_interrupted() -> bool {
    RELAY_INTERRUPT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
    })
}

fn interrupt_relay() {
    RELAY_INTERRUPT.with(|current| {
        if let Some(token) = current.borrow().as_ref() {
            token.cancel();
        }
    });
}

pub(crate) fn read_input_line(buf: &mut String) -> std::io::Result<usize> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    if !RELAY_INTERRUPT.with(|current| current.borrow().is_some()) {
        return std::io::stdin().read_line(buf);
    }
    let _raw = RawModeGuard::enable()?;
    loop {
        if relay_interrupted() {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        if !event::poll(std::time::Duration::from_millis(25))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Enter => {
                eprint!("\r\n");
                buf.push('\n');
                return Ok(buf.len());
            }
            KeyCode::Esc => return Err(std::io::ErrorKind::Interrupted.into()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                interrupt_relay();
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(0),
            KeyCode::Backspace if !buf.is_empty() => {
                buf.pop();
                eprint!("\x08 \x08");
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                buf.push(ch);
                eprint!("{ch}");
            }
            _ => {}
        }
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}

/// Read one line from stdin, or `None` at EOF / on a read error.
fn read_raw_line() -> Option<String> {
    let mut buf = String::new();
    match read_input_line(&mut buf) {
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
    let mut lines: Vec<String> = Vec::new();
    let mut read_anything = false;
    if RELAY_INTERRUPT.with(|current| current.borrow().is_some()) {
        while let Some(line) = read_raw_line() {
            read_anything = true;
            let line = line.strip_suffix('\n').unwrap_or(&line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                break;
            }
            lines.push(line.to_string());
        }
    } else {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) if line.is_empty() => {
                    read_anything = true;
                    break;
                }
                Ok(line) => {
                    read_anything = true;
                    lines.push(line);
                }
                Err(_) => break,
            }
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
            if relay_interrupted() {
                break true;
            }
            if !event::poll(std::time::Duration::from_millis(25))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Enter => break false,
                KeyCode::Esc => break true,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    interrupt_relay();
                    break true;
                }
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

/// Raw keyed summary input allows SIGINT to become the command's exit 130.
pub fn read_prerequisite_report(
    prompt: &crate::data::prompt::Prompt<bool>,
) -> Result<bool, crate::command::error::CommandError> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    for choice in &prompt.choices {
        if choice.key == '\n' {
            eprintln!("  {}", choice.label);
        } else {
            eprintln!("  [{}] {}", choice.key, choice.label);
        }
    }
    if prompt.choice_for_key('\n').is_some() {
        eprintln!("  [Esc] Cancel");
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
                KeyCode::Enter => {
                    return Ok(prompt
                        .answer_for_key('\n')
                        .unwrap_or_else(|| prompt.default_on_dismiss.unwrap_or(false)))
                }
                KeyCode::Esc => return Ok(prompt.default_on_dismiss.unwrap_or(false)),
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
