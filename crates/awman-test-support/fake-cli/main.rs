//! `*-fake-cli`: the one helper executable tests install under another name
//! (`docker`, `container`, `sbx`, `gh`, `git`, `cargo`, …) inside a test
//! sandbox's `bin/` directory. FakeCli protocol v1 (WI 0144 design 6.2).
//!
//! This file must use only `std`. It is compiled as a `[[bin]]` target of
//! every package whose tests need it (`awman-fake-cli`, `xtask-fake-cli`,
//! `awman-test-support-fake-cli`), so it cannot depend on any crate.
//!
//! # How it works
//!
//! The persona is the file stem of `current_exe()` (`docker` for
//! `<bin>/docker`). The scenario lives in `<dir of exe>/.fake/<persona>/`;
//! no environment variable is involved, so it works when awman starts it
//! with a cleared environment.
//!
//! - No `rules` file → exit 127, `fake <persona>: no scenario installed`.
//! - Every invocation is recorded in `.fake/<persona>/calls/<n>/`:
//!   `argv`, `env` and `cwd` (escaped lines, see below), `stdin` (when the
//!   rule says `read-stdin`), `held` while holding, `child-pid` for
//!   `spawn-child`, and finally `done` containing the exit code.
//! - The rule whose `args` are the longest prefix of the arguments wins;
//!   among equally long prefixes the one written last wins. No match → exit
//!   127, `fake <persona>: no rule matches: <args>`.
//!
//! # `rules` file format (line oriented)
//!
//! ```text
//! awman-fake-cli rules v1
//! rule
//! args <arg>          one line per prefix argument, in order (may be absent)
//! stdout <text>       written to stdout
//! stderr <text>       written to stderr
//! exit <code>         default 0
//! read-stdin          read stdin to end of file and record it
//! hold                block until `.fake/<persona>/release` exists
//! touch <path>        create this file (and its parents)
//! spawn-child         start a child that sleeps forever (process-tree tests)
//! end
//! ```
//!
//! Values are escaped: `%` → `%25`, newline → `%0A`, carriage return →
//! `%0D`, tab → `%09`. Everything after the first space is the value.
//!
//! # Special first arguments
//!
//! - `--fake-protocol`: print the protocol version (`1`).
//! - `--fake-sleep`: sleep forever (used by `spawn-child`).
//! - `--fake-list-fds`: print every open descriptor number above 2 (Unix).

// A helper executable, not test code: it records its own environment,
// polls for the release file and starts its grandchild directly.
#![allow(clippy::disallowed_methods)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const FAKE_PROTOCOL: u32 = 1;
const HEADER: &str = "awman-fake-cli rules v1";

#[derive(Default, Debug)]
struct Rule {
    args: Vec<String>,
    stdout: String,
    stderr: String,
    exit: i32,
    read_stdin: bool,
    hold: bool,
    touch: Vec<String>,
    spawn_child: bool,
}

fn unescape(text: &str) -> String {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 1 {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3).unwrap_or(b"")).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn escape(text: &str) -> String {
    text.replace('%', "%25")
        .replace('\n', "%0A")
        .replace('\r', "%0D")
        .replace('\t', "%09")
}

fn parse_rules(text: &str) -> Result<Vec<Rule>, String> {
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Err(format!("rules file does not start with `{HEADER}`"));
    }
    let mut rules = Vec::new();
    let mut current: Option<Rule> = None;
    for line in lines {
        let (key, value) = match line.split_once(' ') {
            Some((k, v)) => (k, unescape(v)),
            None => (line, String::new()),
        };
        match (key, current.as_mut()) {
            ("rule", None) => current = Some(Rule::default()),
            ("end", Some(_)) => rules.push(current.take().unwrap()),
            ("args", Some(r)) => r.args.push(value),
            ("stdout", Some(r)) => r.stdout.push_str(&value),
            ("stderr", Some(r)) => r.stderr.push_str(&value),
            ("exit", Some(r)) => {
                r.exit = value.parse().map_err(|_| format!("bad exit `{value}`"))?
            }
            ("read-stdin", Some(r)) => r.read_stdin = true,
            ("hold", Some(r)) => r.hold = true,
            ("touch", Some(r)) => r.touch.push(value),
            ("spawn-child", Some(r)) => r.spawn_child = true,
            ("", _) => {}
            _ => return Err(format!("unexpected line `{line}`")),
        }
    }
    Ok(rules)
}

fn choose<'a>(rules: &'a [Rule], args: &[String]) -> Option<&'a Rule> {
    let mut best: Option<&Rule> = None;
    for rule in rules {
        let matches = args.len() >= rule.args.len() && args[..rule.args.len()] == rule.args[..];
        if matches && best.is_none_or(|b| rule.args.len() >= b.args.len()) {
            best = Some(rule);
        }
    }
    best
}

fn new_call_dir(calls: &Path) -> PathBuf {
    let _ = std::fs::create_dir_all(calls);
    let mut n = std::fs::read_dir(calls).map(|d| d.count()).unwrap_or(0) + 1;
    loop {
        let dir = calls.join(format!("{n:06}"));
        match std::fs::create_dir(&dir) {
            Ok(()) => return dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => fail(&format!("cannot record call: {e}")),
        }
    }
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(127)
}

fn pause(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

#[cfg(unix)]
fn list_fds() {
    for fd in 3..1024 {
        if std::fs::symlink_metadata(format!("/dev/fd/{fd}")).is_ok() {
            println!("{fd}");
        }
    }
}

#[cfg(not(unix))]
fn list_fds() {}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--fake-protocol") => {
            println!("{FAKE_PROTOCOL}");
            return;
        }
        Some("--fake-sleep") => loop {
            pause(3_600_000);
        },
        Some("--fake-list-fds") => {
            list_fds();
            return;
        }
        _ => {}
    }
    let exe = std::env::current_exe().unwrap_or_else(|e| fail(&format!("fake: current_exe: {e}")));
    let persona = exe
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "fake".into());
    let dir = exe
        .parent()
        .unwrap_or(Path::new("."))
        .join(".fake")
        .join(&persona);
    let text = match std::fs::read_to_string(dir.join("rules")) {
        Ok(text) => text,
        Err(_) => fail(&format!("fake {persona}: no scenario installed")),
    };
    let rules = parse_rules(&text).unwrap_or_else(|e| fail(&format!("fake {persona}: {e}")));

    let call = new_call_dir(&dir.join("calls"));
    let argv: String = args.iter().map(|a| escape(a) + "\n").collect();
    let env: String = std::env::vars_os()
        .map(|(k, v)| escape(&format!("{}={}", k.to_string_lossy(), v.to_string_lossy())) + "\n")
        .collect();
    let cwd = std::env::current_dir()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let _ = std::fs::write(call.join("argv"), argv);
    let _ = std::fs::write(call.join("env"), env);
    let _ = std::fs::write(call.join("cwd"), escape(&cwd) + "\n");

    let Some(rule) = choose(&rules, &args) else {
        let _ = std::fs::write(call.join("done"), "127\n");
        fail(&format!(
            "fake {persona}: no rule matches: {}",
            args.join(" ")
        ));
    };
    if rule.read_stdin {
        let mut input = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut input);
        let _ = std::fs::write(call.join("stdin"), input);
    }
    for file in &rule.touch {
        let path = Path::new(file);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, b"");
    }
    if rule.spawn_child {
        let child = std::process::Command::new(&exe)
            .arg("--fake-sleep")
            .stdin(std::process::Stdio::null())
            .spawn();
        match child {
            Ok(child) => {
                let _ = std::fs::write(call.join("child-pid"), format!("{}\n", child.id()));
            }
            Err(e) => fail(&format!("fake {persona}: spawn-child: {e}")),
        }
    }
    if rule.hold {
        let _ = std::fs::write(call.join("held"), b"");
        while !dir.join("release").exists() {
            pause(10);
        }
    }
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(rule.stdout.as_bytes());
    let _ = stdout.flush();
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(rule.stderr.as_bytes());
    let _ = stderr.flush();
    let _ = std::fs::write(call.join("done"), format!("{}\n", rule.exit));
    std::process::exit(rule.exit);
}
