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
