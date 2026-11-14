//! Host-terminal variables that never reach a container (WI 0141 §3).
//!
//! A container inherits what its launcher passes. Variables such as `TMUX`,
//! `REX_*` or `TERM_PROGRAM` describe — and sometimes address — the host's
//! multiplexer or terminal emulator, and an exotic host `TERM` (`xterm-rex`,
//! `xterm-kitty`) has no terminfo entry inside the image. This module is the
//! one deny-list; each runtime backend applies it at the single point where
//! its run spec gets its environment, after overlays and agent env are
//! merged, so even an overlay that asks for one of these names gets a notice
//! and no variable. It is pure and not configurable.
//!
//! Lives in the container tier (the WI names this path); the sandbox tier
//! imports it the same way it already imports `io_bridge::QuietPtyWriter`.

/// What every container sees as `TERM`, whatever the host's is.
pub const CONTAINER_TERM: &str = "xterm-256color";

/// `COLORTERM` is passed only when the host sets it; this is its only
/// accepted value.
pub const COLORTERM_TRUECOLOR: &str = "truecolor";

/// Whether `name` is a host-terminal variable the container must not see:
/// `REX_*`, `TMUX`, `TMUX_*`, `ZELLIJ`, `ZELLIJ_*`, `STY`, `WINDOW`,
/// `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`, `TERM_SESSION_ID`, `WEZTERM_*`,
/// `KITTY_*`, `ITERM_*`, `LC_TERMINAL*`, `ALACRITTY_*`, `GHOSTTY_*`,
/// `VTE_VERSION`, `WT_SESSION`, `WT_PROFILE_ID`.
///
/// A `_*` entry matches the prefix including the underscore, so `TMUX_PANE`
/// is denied but `TMUXX` is not; `LC_TERMINAL*` has no underscore, so both
/// `LC_TERMINAL_VERSION` and `LC_TERMINALX` are denied. Matching is
/// case-sensitive, like environment variable names.
pub fn is_host_terminal_var(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "TMUX",
        "ZELLIJ",
        "STY",
        "WINDOW",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "TERM_SESSION_ID",
        "VTE_VERSION",
        "WT_SESSION",
        "WT_PROFILE_ID",
    ];
    const PREFIXES: &[&str] = &[
        "REX_",
        "TMUX_",
        "ZELLIJ_",
        "WEZTERM_",
        "KITTY_",
        "ITERM_",
        "LC_TERMINAL",
        "ALACRITTY_",
        "GHOSTTY_",
    ];
    EXACT.contains(&name) || PREFIXES.iter().any(|p| name.starts_with(p))
}

/// The result of scrubbing one run spec's environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubbedEnv {
    /// The variables to pass, in their original order.
    pub kept: Vec<(String, String)>,
    /// Names removed, in their original order, for the notice.
    pub dropped: Vec<String>,
}

impl ScrubbedEnv {
    /// One line naming the dropped variables, or `None` when nothing was.
    pub fn notice(&self) -> Option<String> {
        if self.dropped.is_empty() {
            return None;
        }
        Some(format!(
            "host terminal variables not passed to the container: {}",
            self.dropped.join(", ")
        ))
    }
}

/// Remove every deny-listed name from a merged environment and set `TERM`
/// to [`CONTAINER_TERM`] (replacing any host value, exactly once). Other
/// names, `COLORTERM` included, are kept as given. Explicit per-session
/// secrets (0147) never match the deny-list.
pub fn scrub_host_terminal_vars(env: Vec<(String, String)>) -> ScrubbedEnv {
    let mut out = ScrubbedEnv::default();
    let mut term_seen = false;
    for (name, value) in env {
        if is_host_terminal_var(&name) {
            out.dropped.push(name);
        } else if name == "TERM" {
            if !term_seen {
                term_seen = true;
                out.kept.push((name, CONTAINER_TERM.to_string()));
            }
        } else {
            out.kept.push((name, value));
        }
    }
    if !term_seen {
        out.kept
            .push(("TERM".to_string(), CONTAINER_TERM.to_string()));
    }
    out
}

/// The terminal variables every container gets: `TERM=xterm-256color`, plus
/// `COLORTERM=truecolor` when `host_colorterm` is set.
pub fn container_terminal_env(host_colorterm: Option<&str>) -> Vec<(String, String)> {
    let mut env = vec![("TERM".to_string(), CONTAINER_TERM.to_string())];
    if host_colorterm.is_some_and(|v| !v.is_empty()) {
        env.push(("COLORTERM".to_string(), COLORTERM_TRUECOLOR.to_string()));
    }
    env
}

/// The one scrub every backend applies to its merged run-spec environment:
/// deny-listed names removed, `TERM` forced to [`CONTAINER_TERM`], and any
/// `COLORTERM` replaced by `truecolor` only when `host_colorterm` is set.
pub fn scrub_run_env(env: Vec<(String, String)>, host_colorterm: Option<&str>) -> ScrubbedEnv {
    let env = env.into_iter().filter(|(n, _)| n != "COLORTERM").collect();
    let mut out = scrub_host_terminal_vars(env);
    if let Some(extra) = container_terminal_env(host_colorterm).into_iter().nth(1) {
        out.kept.push(extra);
    }
    out
}
