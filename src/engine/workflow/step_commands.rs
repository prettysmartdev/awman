//! Step-to-command translation — Layer 1.
//!
//! Pure functions that translate `SetupStep` and `TeardownStep` values into
//! shell command strings and optional per-step env var overrides. No external
//! dependencies — stateless mapping only.

use std::collections::HashMap;

use crate::data::workflow_definition::{CloneConflictMode, SetupStep, TeardownStep};
use crate::data::workflow_prompt_template::{extract_section, substitute_prompt, WorkItemContext};
use crate::data::workflow_state::PhaseKind;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PushAuth {
    #[default]
    None,
    Ssh {
        key_path: String,
        known_hosts: Option<String>,
    },
    HttpsToken {
        env_var: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepCommandOptions {
    pub push_auth: PushAuth,
    pub remote_url: Option<String>,
}

/// Transport runs with controlled metadata supplied by OverlayEngine. Never
/// consult hooks, SSH config, helpers or URL rewrites from the checkout.
fn transport_git(opts: &StepCommandOptions) -> String {
    if opts.push_auth == PushAuth::None && opts.remote_url.is_none() {
        return "git".into();
    }
    let mut git = "git -c core.hooksPath=/dev/null -c core.fsmonitor=false -c maintenance.auto=false -c gc.auto=0 -c push.gpgSign=false -c push.recurseSubmodules=no -c credential.helper=".to_string();
    if let PushAuth::HttpsToken { env_var } = &opts.push_auth {
        let destination = opts.remote_url.as_deref().unwrap_or("");
        let (host, path) = destination
            .strip_prefix("https://")
            .unwrap_or("")
            .split_once('/')
            .unwrap_or(("", ""));
        // Consume Git's credential protocol and answer only for the approved
        // HTTPS destination. The token itself is expanded in the child env.
        let helper = format!(
            "credential.helper=!f() {{ protocol=; host=; path=; while IFS='=' read -r k v; do case \\\"$k\\\" in protocol) protocol=$v;; host) host=$v;; path) path=$v;; esac; done; if [ \\\"$1\\\" = get ] && [ \\\"$protocol\\\" = https ] && [ \\\"$host\\\" = {host} ] && [ \\\"$path\\\" = {path} ]; then printf '%s\\n' 'username=x-access-token' \\\"password=${env_var}\\\"; fi; }}; f",
            host = sh_quote(host), path = sh_quote(path)
        );
        // Rust escapes quotes once; sh_quote then quotes the complete config
        // argument once. Literal backslashes must not reach the helper body.
        let helper = helper.replace("\\\"", "\"");
        git.push_str(&format!(
            " -c credential.useHttpPath=true -c {}",
            sh_quote(&helper)
        ));
    }
    git
}

fn transport_env(opts: &StepCommandOptions) -> Option<HashMap<String, String>> {
    if opts.push_auth == PushAuth::None && opts.remote_url.is_none() {
        return Some(HashMap::from([(
            "GIT_SSH_COMMAND".into(),
            "ssh -o StrictHostKeyChecking=accept-new".into(),
        )]));
    }
    let mut env = HashMap::from([("GIT_TERMINAL_PROMPT".into(), "0".into())]);
    let ssh = match &opts.push_auth {
        PushAuth::Ssh {
            key_path,
            known_hosts,
        } => {
            let mut ssh = format!("ssh -F /dev/null -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new", sh_quote(key_path));
            if let Some(path) = known_hosts {
                ssh.push_str(&format!(" -o UserKnownHostsFile={}", sh_quote(path)));
            }
            ssh
        }
        _ => "ssh -F /dev/null -o BatchMode=yes -o StrictHostKeyChecking=accept-new".into(),
    };
    env.insert("GIT_SSH_COMMAND".into(), ssh);
    Some(env)
}

/// Quote `s` as a single POSIX shell word so embedded whitespace, quotes, and
/// metacharacters cannot break out of the argument. Workflow files are
/// author-controlled but may still contain literal quotes, spaces, or other
/// punctuation; unescaped interpolation would either break the command or
/// allow injection in less-trusted authoring scenarios.
fn sh_quote(s: &str) -> String {
    // Single-quoted strings have no escapes in POSIX sh; the only character
    // that needs handling is the single quote itself, which must close the
    // quote, emit a literal `\'`, and reopen.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Directory a `clone_repo` step will clone into: the `into` field when set,
/// otherwise the "humanish" name git derives from the URL (last path
/// component, trailing `/` and `.git` stripped).
fn clone_target_dir(url: &str, into: Option<&str>) -> String {
    if let Some(d) = into {
        return d.to_string();
    }
    let trimmed = url.trim_end_matches('/');
    let name = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    name.strip_suffix(".git").unwrap_or(name).to_string()
}

/// Translate a setup step into a shell command string and optional env overrides.
pub fn setup_step_to_shell(step: &SetupStep) -> (String, Option<HashMap<String, String>>) {
    setup_step_to_shell_with(step, &StepCommandOptions::default())
}

pub fn setup_step_to_shell_with(
    step: &SetupStep,
    opts: &StepCommandOptions,
) -> (String, Option<HashMap<String, String>>) {
    match step {
        SetupStep::CloneRepo {
            url,
            branch,
            into,
            conflict_mode,
        } => {
            let mut clone = format!("{} clone", transport_git(opts));
            if let Some(b) = branch {
                clone.push_str(&format!(" -b {}", sh_quote(b)));
            }
            clone.push_str(&format!(
                " {}{}",
                if opts.remote_url.is_some() { "-- " } else { "" },
                sh_quote(opts.remote_url.as_deref().unwrap_or(url))
            ));
            if let Some(d) = into {
                clone.push_str(&format!(" {}", sh_quote(d)));
            }
            // The step is a conflict only when the target directory already
            // holds a clone of this exact URL (per remote.origin.url in its
            // .git/config); `conflict_mode` decides what happens then. A
            // directory occupied by anything else is left alone in every mode
            // — `git clone` itself fails on it with a clear error.
            let dir = clone_target_dir(url, into.as_deref());
            let already_cloned = format!(
                "[ -f {config_q} ] && [ \"$(git -C {dir_q} config --get remote.origin.url)\" = {url_q} ]",
                config_q = sh_quote(&format!("{dir}/.git/config")),
                dir_q = sh_quote(&dir),
                url_q = sh_quote(url),
            );
            let cmd = match conflict_mode {
                CloneConflictMode::Skip => format!(
                    "if {already_cloned}; then echo {msg_q}; else {clone}; fi",
                    msg_q = sh_quote(&format!(
                        "clone_repo: {dir} already cloned from {url}; skipping"
                    )),
                ),
                CloneConflictMode::Replace => format!(
                    "if {already_cloned}; then echo {msg_q}; rm -rf {dir_q}; fi; {clone}",
                    msg_q = sh_quote(&format!(
                        "clone_repo: {dir} already cloned from {url}; replacing"
                    )),
                    dir_q = sh_quote(&dir),
                ),
                CloneConflictMode::Error => format!(
                    "if {already_cloned}; then echo {msg_q} >&2; exit 1; else {clone}; fi",
                    msg_q = sh_quote(&format!(
                        "clone_repo: {dir} already cloned from {url} (conflict_mode = error)"
                    )),
                ),
            };
            (
                cmd,
                if opts == &StepCommandOptions::default() {
                    None
                } else {
                    transport_env(opts)
                },
            )
        }
        SetupStep::CheckoutCreateBranch { branch, base } => {
            let b_q = sh_quote(branch);
            let cmd = match base {
                Some(base) => format!(
                    "git fetch origin 2>/dev/null; git checkout {b_q} 2>/dev/null || git checkout -b {b_q} {base_q}",
                    base_q = sh_quote(base),
                ),
                None => format!(
                    "git fetch origin 2>/dev/null; git checkout {b_q} 2>/dev/null || git checkout -b {b_q}"
                ),
            };
            (cmd, None)
        }
        SetupStep::PullBranch { remote, branch } => {
            if opts == &StepCommandOptions::default() {
                return (
                    match (remote, branch) {
                        (Some(r), Some(b)) => format!("git pull {} {}", sh_quote(r), sh_quote(b)),
                        _ => "git pull".into(),
                    },
                    None,
                );
            }
            let destination = opts.remote_url.as_deref().or(remote.as_deref());
            let cmd = match (destination, branch) {
                (Some(r), Some(b)) => format!(
                    "{} pull -- {} {}",
                    transport_git(opts),
                    sh_quote(r),
                    sh_quote(b)
                ),
                (Some(r), None) => format!("{} pull -- {}", transport_git(opts), sh_quote(r)),
                _ => format!("{} pull", transport_git(opts)),
            };
            (cmd, transport_env(opts))
        }
        SetupStep::RunShell { command, env } => (command.clone(), env.clone()),
        SetupStep::RunScript { path, env } => (format!("sh {}", sh_quote(path)), env.clone()),
        SetupStep::PollCi { .. } => {
            unreachable!("PollCi steps are handled natively by the engine, not translated to shell")
        }
    }
}

/// Translate a teardown step into a shell command string and optional env overrides.
pub fn teardown_step_to_shell(step: &TeardownStep) -> (String, Option<HashMap<String, String>>) {
    teardown_step_to_shell_with(step, &StepCommandOptions::default())
}

pub fn teardown_step_to_shell_with(
    step: &TeardownStep,
    opts: &StepCommandOptions,
) -> (String, Option<HashMap<String, String>>) {
    match step {
        TeardownStep::RunShell { command, env } => (command.clone(), env.clone()),
        TeardownStep::RunScript { path } => (format!("sh {}", sh_quote(path)), None),
        TeardownStep::CommitChanges { message, add_all } => {
            let msg_q = sh_quote(message);
            let cmd = if *add_all {
                format!("git add -A && git commit -m {msg_q}")
            } else {
                format!("git commit -m {msg_q}")
            };
            (cmd, None)
        }
        TeardownStep::CreatePullRequest { title, body, base } => {
            let t = title.as_deref().unwrap_or("Automated PR");
            let b = body.as_deref().unwrap_or("");
            let mut cmd = format!(
                "gh pr create --title {} --body {}",
                sh_quote(t),
                sh_quote(b),
            );
            if opts.remote_url.is_some() {
                cmd.push_str(" --head \"$(git symbolic-ref --short HEAD)\"");
            }
            if let Some(base) = base {
                cmd.push_str(&format!(" --base {}", sh_quote(base)));
            }
            (cmd, None)
        }
        TeardownStep::PushBranch { remote, branch } => {
            let remote = opts
                .remote_url
                .as_deref()
                .or(remote.as_deref())
                .unwrap_or("origin");
            let branch = branch.as_deref().unwrap_or("HEAD");
            if branch.starts_with(['-', '+']) || branch.contains(':') {
                return (
                    "printf '%s\\n' 'push_branch requires a branch name, not options or a refspec' >&2; exit 2".into(),
                    None,
                );
            }
            let cmd = format!(
                "{} push -u -- {} {}",
                transport_git(opts),
                sh_quote(remote),
                sh_quote(branch)
            );
            (cmd, transport_env(opts))
        }
        TeardownStep::PollCi { .. } => {
            unreachable!("PollCi steps are handled natively by the engine, not translated to shell")
        }
    }
}

/// Human-readable description of a setup step.
pub fn setup_step_description(step: &SetupStep) -> String {
    match step {
        SetupStep::CloneRepo { url, .. } => format!("clone_repo: {url}"),
        SetupStep::CheckoutCreateBranch { branch, .. } => {
            format!("checkout_create_branch: {branch}")
        }
        SetupStep::PullBranch { remote, branch } => match (remote, branch) {
            (Some(r), Some(b)) => format!("pull_branch: {r} {b}"),
            _ => "pull_branch".to_string(),
        },
        SetupStep::RunShell { command, .. } => format!("run_shell: {command}"),
        SetupStep::RunScript { path, .. } => format!("run_script: {path}"),
        SetupStep::PollCi {
            interval_secs,
            max_retries,
        } => {
            let i = interval_secs.unwrap_or(30);
            let r = max_retries.unwrap_or(10);
            format!("poll_ci: interval={i}s, retries={r}")
        }
    }
}

/// Human-readable description of a teardown step.
pub fn teardown_step_description(step: &TeardownStep) -> String {
    match step {
        TeardownStep::RunShell { command, .. } => format!("run_shell: {command}"),
        TeardownStep::RunScript { path } => format!("run_script: {path}"),
        TeardownStep::CommitChanges { message, .. } => format!("commit_changes: {message}"),
        TeardownStep::CreatePullRequest { title, .. } => match title {
            Some(t) => format!("create_pull_request: {t}"),
            None => "create_pull_request".to_string(),
        },
        TeardownStep::PushBranch { remote, branch } => match (remote, branch) {
            (Some(r), Some(b)) => format!("push_branch: {r} {b}"),
            _ => "push_branch".to_string(),
        },
        TeardownStep::PollCi {
            interval_secs,
            max_retries,
        } => {
            let i = interval_secs.unwrap_or(30);
            let r = max_retries.unwrap_or(10);
            format!("poll_ci: interval={i}s, retries={r}")
        }
    }
}

fn sub(s: &str, ctx: Option<&WorkItemContext>) -> String {
    substitute_prompt(s, ctx).rendered
}

fn sub_opt(s: &Option<String>, ctx: Option<&WorkItemContext>) -> Option<String> {
    s.as_ref().map(|v| sub(v, ctx))
}

/// Apply work-item template substitution to all string fields of a setup step.
pub fn substitute_setup_step(step: &SetupStep, ctx: Option<&WorkItemContext>) -> SetupStep {
    match step {
        SetupStep::CloneRepo {
            url,
            branch,
            into,
            conflict_mode,
        } => SetupStep::CloneRepo {
            url: sub(url, ctx),
            branch: sub_opt(branch, ctx),
            into: sub_opt(into, ctx),
            conflict_mode: *conflict_mode,
        },
        SetupStep::CheckoutCreateBranch { branch, base } => SetupStep::CheckoutCreateBranch {
            branch: sub(branch, ctx),
            base: sub_opt(base, ctx),
        },
        SetupStep::PullBranch { remote, branch } => SetupStep::PullBranch {
            remote: sub_opt(remote, ctx),
            branch: sub_opt(branch, ctx),
        },
        SetupStep::RunShell { command, env } => SetupStep::RunShell {
            command: sub(command, ctx),
            env: env.clone(),
        },
        SetupStep::RunScript { path, env } => SetupStep::RunScript {
            path: sub(path, ctx),
            env: env.clone(),
        },
        SetupStep::PollCi {
            interval_secs,
            max_retries,
        } => SetupStep::PollCi {
            interval_secs: *interval_secs,
            max_retries: *max_retries,
        },
    }
}

/// Apply work-item template substitution to all string fields of a teardown step.
pub fn substitute_teardown_step(
    step: &TeardownStep,
    ctx: Option<&WorkItemContext>,
) -> TeardownStep {
    match step {
        TeardownStep::RunShell { command, env } => TeardownStep::RunShell {
            command: sub(command, ctx),
            env: env.clone(),
        },
        TeardownStep::RunScript { path } => TeardownStep::RunScript {
            path: sub(path, ctx),
        },
        TeardownStep::CommitChanges { message, add_all } => TeardownStep::CommitChanges {
            message: sub(message, ctx),
            add_all: *add_all,
        },
        TeardownStep::CreatePullRequest { title, body, base } => {
            let resolved_title = match title {
                Some(t) => Some(sub(t, ctx)),
                None => ctx.map(|wi| format!("Implement awman/work-item-{:04}", wi.number)),
            };
            let resolved_body = match body {
                Some(b) => Some(sub(b, ctx)),
                None => ctx.and_then(|wi| extract_section(&wi.content, "Summary")),
            };
            TeardownStep::CreatePullRequest {
                title: resolved_title,
                body: resolved_body,
                base: sub_opt(base, ctx),
            }
        }
        TeardownStep::PushBranch { remote, branch } => TeardownStep::PushBranch {
            remote: sub_opt(remote, ctx),
            branch: sub_opt(branch, ctx),
        },
        TeardownStep::PollCi {
            interval_secs,
            max_retries,
        } => TeardownStep::PollCi {
            interval_secs: *interval_secs,
            max_retries: *max_retries,
        },
    }
}

/// The slice of a shell-phase step that `WorkflowEngine::run_phase` needs.
///
/// `SetupStep` and `TeardownStep` stay distinct types — they accept different
/// actions — but the phase runner treats them identically, so before F-34 the
/// engine carried two near-identical copies of every phase function. This
/// trait is what lets one `run_phase` drive both.
pub trait PhaseStepSpec: Sized {
    /// Only create_pull_request interprets successful stdout as a PR URL.
    fn creates_pull_request(&self) -> bool {
        false
    }

    /// Which phase steps of this type belong to. `run_phase` asserts (in
    /// debug builds) that its `kind` argument agrees.
    fn kind() -> PhaseKind;

    /// Apply work-item template substitution to every string field.
    fn substitute(&self, ctx: Option<&WorkItemContext>) -> Self;

    /// One-line human description, used for state, logs and frontend events.
    fn description(&self) -> String;

    /// The shell command and per-step env overrides this step runs as.
    /// Never called for a step whose [`PhaseStepSpec::poll_ci`] is `Some`.
    fn to_shell(&self) -> (String, Option<HashMap<String, String>>) {
        self.to_shell_with(&StepCommandOptions::default())
    }

    fn to_shell_with(&self, opts: &StepCommandOptions)
        -> (String, Option<HashMap<String, String>>);

    /// `Some((interval_secs, max_retries))` when the step is a native CI poll
    /// the engine runs itself rather than a command in a container.
    fn poll_ci(&self) -> Option<(u32, u32)>;

    /// Whether the step publishes the run's work (a push, a pull request,
    /// waiting on CI for it). Such a step never starts once the run is
    /// cancelled.
    fn publishes(&self) -> bool {
        false
    }
}

impl PhaseStepSpec for SetupStep {
    fn kind() -> PhaseKind {
        PhaseKind::Setup
    }

    fn substitute(&self, ctx: Option<&WorkItemContext>) -> Self {
        substitute_setup_step(self, ctx)
    }

    fn description(&self) -> String {
        setup_step_description(self)
    }

    fn to_shell_with(
        &self,
        opts: &StepCommandOptions,
    ) -> (String, Option<HashMap<String, String>>) {
        setup_step_to_shell_with(self, opts)
    }

    fn poll_ci(&self) -> Option<(u32, u32)> {
        match self {
            SetupStep::PollCi {
                interval_secs,
                max_retries,
            } => Some((
                interval_secs.unwrap_or(DEFAULT_POLL_CI_INTERVAL_SECS),
                max_retries.unwrap_or(DEFAULT_POLL_CI_MAX_RETRIES),
            )),
            _ => None,
        }
    }
}

impl PhaseStepSpec for TeardownStep {
    fn creates_pull_request(&self) -> bool {
        matches!(self, TeardownStep::CreatePullRequest { .. })
    }

    fn kind() -> PhaseKind {
        PhaseKind::Teardown
    }

    fn substitute(&self, ctx: Option<&WorkItemContext>) -> Self {
        substitute_teardown_step(self, ctx)
    }

    fn description(&self) -> String {
        teardown_step_description(self)
    }

    fn to_shell_with(
        &self,
        opts: &StepCommandOptions,
    ) -> (String, Option<HashMap<String, String>>) {
        teardown_step_to_shell_with(self, opts)
    }

    fn poll_ci(&self) -> Option<(u32, u32)> {
        match self {
            TeardownStep::PollCi {
                interval_secs,
                max_retries,
            } => Some((
                interval_secs.unwrap_or(DEFAULT_POLL_CI_INTERVAL_SECS),
                max_retries.unwrap_or(DEFAULT_POLL_CI_MAX_RETRIES),
            )),
            _ => None,
        }
    }

    fn publishes(&self) -> bool {
        !super::WorkflowEngine::teardown_step_runs_when_cancelled(self)
    }
}

/// Defaults for a `poll_ci` step that names neither value. Unchanged from the
/// per-phase copies these replace.
const DEFAULT_POLL_CI_INTERVAL_SECS: u32 = 30;
const DEFAULT_POLL_CI_MAX_RETRIES: u32 = 10;
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_repo_full() {
        let step = SetupStep::CloneRepo {
            url: "https://github.com/org/repo".to_string(),
            branch: Some("main".to_string()),
            into: Some("subdir".to_string()),
            conflict_mode: CloneConflictMode::Skip,
        };
        let (cmd, env) = setup_step_to_shell(&step);
        assert_eq!(
            cmd,
            "if [ -f 'subdir/.git/config' ] && [ \"$(git -C 'subdir' config --get remote.origin.url)\" = 'https://github.com/org/repo' ]; then echo 'clone_repo: subdir already cloned from https://github.com/org/repo; skipping'; else git clone -b 'main' 'https://github.com/org/repo' 'subdir'; fi"
        );
        assert!(env.is_none());
    }

    #[test]
    fn clone_repo_minimal() {
        let step = SetupStep::CloneRepo {
            url: "https://github.com/org/repo".to_string(),
            branch: None,
            into: None,
            conflict_mode: CloneConflictMode::Skip,
        };
        let (cmd, _) = setup_step_to_shell(&step);
        assert_eq!(
            cmd,
            "if [ -f 'repo/.git/config' ] && [ \"$(git -C 'repo' config --get remote.origin.url)\" = 'https://github.com/org/repo' ]; then echo 'clone_repo: repo already cloned from https://github.com/org/repo; skipping'; else git clone 'https://github.com/org/repo'; fi"
        );
    }

    #[test]
    fn clone_repo_conflict_mode_replace() {
        let step = SetupStep::CloneRepo {
            url: "https://github.com/org/repo".to_string(),
            branch: None,
            into: None,
            conflict_mode: CloneConflictMode::Replace,
        };
        let (cmd, _) = setup_step_to_shell(&step);
        assert_eq!(
            cmd,
            "if [ -f 'repo/.git/config' ] && [ \"$(git -C 'repo' config --get remote.origin.url)\" = 'https://github.com/org/repo' ]; then echo 'clone_repo: repo already cloned from https://github.com/org/repo; replacing'; rm -rf 'repo'; fi; git clone 'https://github.com/org/repo'"
        );
    }

    #[test]
    fn clone_repo_conflict_mode_error() {
        let step = SetupStep::CloneRepo {
            url: "https://github.com/org/repo".to_string(),
            branch: None,
            into: None,
            conflict_mode: CloneConflictMode::Error,
        };
        let (cmd, _) = setup_step_to_shell(&step);
        assert_eq!(
            cmd,
            "if [ -f 'repo/.git/config' ] && [ \"$(git -C 'repo' config --get remote.origin.url)\" = 'https://github.com/org/repo' ]; then echo 'clone_repo: repo already cloned from https://github.com/org/repo (conflict_mode = error)' >&2; exit 1; else git clone 'https://github.com/org/repo'; fi"
        );
    }

    #[test]
    fn clone_target_dir_derives_default_name() {
        assert_eq!(
            clone_target_dir("https://github.com/org/repo", None),
            "repo"
        );
        assert_eq!(
            clone_target_dir("https://github.com/org/repo.git", None),
            "repo"
        );
        assert_eq!(
            clone_target_dir("https://github.com/org/repo.git/", None),
            "repo"
        );
        assert_eq!(
            clone_target_dir("git@github.com:org/repo.git", None),
            "repo"
        );
        assert_eq!(
            clone_target_dir("https://github.com/org/repo", Some("subdir")),
            "subdir"
        );
    }

    #[test]
    fn checkout_create_branch_with_base() {
        let step = SetupStep::CheckoutCreateBranch {
            branch: "feature/x".to_string(),
            base: Some("main".to_string()),
        };
        let (cmd, _) = setup_step_to_shell(&step);
        assert_eq!(
            cmd,
            "git fetch origin 2>/dev/null; git checkout 'feature/x' 2>/dev/null || git checkout -b 'feature/x' 'main'"
        );
    }

    #[test]
    fn checkout_create_branch_without_base() {
        let step = SetupStep::CheckoutCreateBranch {
            branch: "feature/x".to_string(),
            base: None,
        };
        let (cmd, _) = setup_step_to_shell(&step);
        assert_eq!(
            cmd,
            "git fetch origin 2>/dev/null; git checkout 'feature/x' 2>/dev/null || git checkout -b 'feature/x'"
        );
    }

    #[test]
    fn pull_branch_with_args() {
        let step = SetupStep::PullBranch {
            remote: Some("origin".to_string()),
            branch: Some("main".to_string()),
        };
        let (cmd, _) = setup_step_to_shell(&step);
        assert_eq!(cmd, "git pull 'origin' 'main'");
    }

    #[test]
    fn pull_branch_defaults() {
        let step = SetupStep::PullBranch {
            remote: None,
            branch: None,
        };
        let (cmd, _) = setup_step_to_shell(&step);
        assert_eq!(cmd, "git pull");
    }

    #[test]
    fn run_shell_passes_through() {
        let mut env_map = HashMap::new();
        env_map.insert("FOO".to_string(), "bar".to_string());
        let step = SetupStep::RunShell {
            command: "cargo test".to_string(),
            env: Some(env_map.clone()),
        };
        let (cmd, env) = setup_step_to_shell(&step);
        assert_eq!(cmd, "cargo test");
        assert_eq!(env.unwrap(), env_map);
    }

    #[test]
    fn run_script_wraps_in_sh() {
        let step = SetupStep::RunScript {
            path: "./setup.sh".to_string(),
            env: None,
        };
        let (cmd, _) = setup_step_to_shell(&step);
        assert_eq!(cmd, "sh './setup.sh'");
    }

    #[test]
    fn commit_changes_add_all() {
        let step = TeardownStep::CommitChanges {
            message: "auto commit".to_string(),
            add_all: true,
        };
        let (cmd, _) = teardown_step_to_shell(&step);
        assert_eq!(cmd, "git add -A && git commit -m 'auto commit'");
    }

    #[test]
    fn commit_changes_no_add() {
        let step = TeardownStep::CommitChanges {
            message: "auto commit".to_string(),
            add_all: false,
        };
        let (cmd, _) = teardown_step_to_shell(&step);
        assert_eq!(cmd, "git commit -m 'auto commit'");
    }

    #[test]
    fn create_pr_full() {
        let step = TeardownStep::CreatePullRequest {
            title: Some("feat: my feature".to_string()),
            body: Some("PR body text".to_string()),
            base: Some("main".to_string()),
        };
        let (cmd, _) = teardown_step_to_shell(&step);
        assert_eq!(
            cmd,
            "gh pr create --title 'feat: my feature' --body 'PR body text' --base 'main'"
        );
    }

    #[test]
    fn create_pr_minimal() {
        let step = TeardownStep::CreatePullRequest {
            title: Some("feat: my feature".to_string()),
            body: None,
            base: None,
        };
        let (cmd, _) = teardown_step_to_shell(&step);
        assert_eq!(cmd, "gh pr create --title 'feat: my feature' --body ''");
    }

    #[test]
    fn create_pr_no_title_defaults() {
        let step = TeardownStep::CreatePullRequest {
            title: None,
            body: None,
            base: None,
        };
        let (cmd, _) = teardown_step_to_shell(&step);
        assert_eq!(cmd, "gh pr create --title 'Automated PR' --body ''");
    }

    #[test]
    fn push_branch_full() {
        let step = TeardownStep::PushBranch {
            remote: Some("origin".to_string()),
            branch: Some("feature/x".to_string()),
        };
        let (cmd, env) = teardown_step_to_shell(&step);
        assert_eq!(cmd, "git push -u -- 'origin' 'feature/x'");
        let env = env.expect("push_branch must set GIT_SSH_COMMAND env");
        assert_eq!(
            env.get("GIT_SSH_COMMAND").unwrap(),
            "ssh -o StrictHostKeyChecking=accept-new",
        );
    }

    #[test]
    fn push_branch_defaults() {
        let step = TeardownStep::PushBranch {
            remote: None,
            branch: None,
        };
        let (cmd, env) = teardown_step_to_shell(&step);
        assert_eq!(cmd, "git push -u -- 'origin' 'HEAD'");
        let env = env.expect("push_branch must set GIT_SSH_COMMAND env");
        assert_eq!(
            env.get("GIT_SSH_COMMAND").unwrap(),
            "ssh -o StrictHostKeyChecking=accept-new",
        );
    }

    #[test]
    fn push_branch_remote_without_branch_defaults_to_head() {
        let step = TeardownStep::PushBranch {
            remote: Some("upstream".to_string()),
            branch: None,
        };
        let (cmd, env) = teardown_step_to_shell(&step);
        assert_eq!(cmd, "git push -u -- 'upstream' 'HEAD'");
        let env = env.expect("push_branch must set GIT_SSH_COMMAND env");
        assert_eq!(
            env.get("GIT_SSH_COMMAND").unwrap(),
            "ssh -o StrictHostKeyChecking=accept-new",
        );
    }

    #[test]
    fn push_branch_branch_without_remote_defaults_to_origin() {
        let step = TeardownStep::PushBranch {
            remote: None,
            branch: Some("feature/y".to_string()),
        };
        let (cmd, env) = teardown_step_to_shell(&step);
        assert_eq!(cmd, "git push -u -- 'origin' 'feature/y'");
        let env = env.expect("push_branch must set GIT_SSH_COMMAND env");
        assert_eq!(
            env.get("GIT_SSH_COMMAND").unwrap(),
            "ssh -o StrictHostKeyChecking=accept-new",
        );
    }

    #[test]
    fn teardown_run_shell_passes_through() {
        let mut env_map = HashMap::new();
        env_map.insert("BAR".to_string(), "baz".to_string());
        let step = TeardownStep::RunShell {
            command: "npm test".to_string(),
            env: Some(env_map.clone()),
        };
        let (cmd, env) = teardown_step_to_shell(&step);
        assert_eq!(cmd, "npm test");
        assert_eq!(env.unwrap(), env_map);
    }

    #[test]
    fn teardown_run_script_wraps_in_sh() {
        let step = TeardownStep::RunScript {
            path: "./teardown.sh".to_string(),
        };
        let (cmd, env) = teardown_step_to_shell(&step);
        assert_eq!(cmd, "sh './teardown.sh'");
        assert!(env.is_none());
    }

    #[test]
    fn sh_quote_handles_embedded_single_quote() {
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        // Round-trip through `sh -c` would re-emit "it's" — verify the literal.
        let step = TeardownStep::CommitChanges {
            message: "it's done".to_string(),
            add_all: false,
        };
        let (cmd, _) = teardown_step_to_shell(&step);
        assert_eq!(cmd, "git commit -m 'it'\\''s done'");
    }

    #[test]
    fn sh_quote_blocks_injection_attempts() {
        // A hostile/typo'd title with shell metacharacters must stay inside
        // a single argument when interpolated.
        let step = TeardownStep::CreatePullRequest {
            title: Some("x\"; rm -rf / #".to_string()),
            body: None,
            base: None,
        };
        let (cmd, _) = teardown_step_to_shell(&step);
        assert_eq!(cmd, "gh pr create --title 'x\"; rm -rf / #' --body ''");
    }

    #[test]
    fn sh_quote_blocks_injection_via_single_quote() {
        // Even a literal apostrophe in the user input cannot escape: the
        // single-quote-aware escape closes-emits-reopens.
        let step = TeardownStep::CommitChanges {
            message: "'; rm -rf / #".to_string(),
            add_all: false,
        };
        let (cmd, _) = teardown_step_to_shell(&step);
        assert_eq!(cmd, "git commit -m ''\\''; rm -rf / #'");
    }

    #[test]
    fn setup_step_description_all_variants() {
        assert_eq!(
            setup_step_description(&SetupStep::CloneRepo {
                url: "https://x.com/repo".to_string(),
                branch: None,
                into: None,
                conflict_mode: CloneConflictMode::Skip,
            }),
            "clone_repo: https://x.com/repo"
        );
        assert_eq!(
            setup_step_description(&SetupStep::CheckoutCreateBranch {
                branch: "feature/x".to_string(),
                base: None,
            }),
            "checkout_create_branch: feature/x"
        );
        assert_eq!(
            setup_step_description(&SetupStep::PullBranch {
                remote: Some("origin".to_string()),
                branch: Some("main".to_string()),
            }),
            "pull_branch: origin main"
        );
        assert_eq!(
            setup_step_description(&SetupStep::PullBranch {
                remote: None,
                branch: None,
            }),
            "pull_branch"
        );
        assert_eq!(
            setup_step_description(&SetupStep::RunShell {
                command: "cargo test".to_string(),
                env: None,
            }),
            "run_shell: cargo test"
        );
        assert_eq!(
            setup_step_description(&SetupStep::RunScript {
                path: "./setup.sh".to_string(),
                env: None,
            }),
            "run_script: ./setup.sh"
        );
    }

    #[test]
    fn teardown_step_description_all_variants() {
        assert_eq!(
            teardown_step_description(&TeardownStep::RunShell {
                command: "npm test".to_string(),
                env: None,
            }),
            "run_shell: npm test"
        );
        assert_eq!(
            teardown_step_description(&TeardownStep::RunScript {
                path: "./cleanup.sh".to_string(),
            }),
            "run_script: ./cleanup.sh"
        );
        assert_eq!(
            teardown_step_description(&TeardownStep::CommitChanges {
                message: "chore: auto-commit".to_string(),
                add_all: true,
            }),
            "commit_changes: chore: auto-commit"
        );
        assert_eq!(
            teardown_step_description(&TeardownStep::CreatePullRequest {
                title: Some("feat: x".to_string()),
                body: None,
                base: None,
            }),
            "create_pull_request: feat: x"
        );
        assert_eq!(
            teardown_step_description(&TeardownStep::CreatePullRequest {
                title: None,
                body: None,
                base: None,
            }),
            "create_pull_request"
        );
        assert_eq!(
            teardown_step_description(&TeardownStep::PushBranch {
                remote: Some("origin".to_string()),
                branch: Some("feature/x".to_string()),
            }),
            "push_branch: origin feature/x"
        );
        assert_eq!(
            teardown_step_description(&TeardownStep::PushBranch {
                remote: None,
                branch: None,
            }),
            "push_branch"
        );
    }

    fn wi() -> WorkItemContext {
        WorkItemContext {
            number: 42,
            content: "# Title\n\n## Summary\nDo the thing\n".to_string(),
        }
    }

    #[test]
    fn substitute_teardown_commit_message() {
        let step = TeardownStep::CommitChanges {
            message: "Implement {{work_item_number}}".to_string(),
            add_all: true,
        };
        let result = substitute_teardown_step(&step, Some(&wi()));
        assert_eq!(
            result,
            TeardownStep::CommitChanges {
                message: "Implement 0042".to_string(),
                add_all: true,
            }
        );
    }

    #[test]
    fn substitute_teardown_create_pr() {
        let step = TeardownStep::CreatePullRequest {
            title: Some("feat: WI {{work_item_number}}".to_string()),
            body: Some("Content: {{work_item_section:[Summary]}}".to_string()),
            base: Some("main".to_string()),
        };
        let result = substitute_teardown_step(&step, Some(&wi()));
        match result {
            TeardownStep::CreatePullRequest { title, body, base } => {
                assert_eq!(title.unwrap(), "feat: WI 0042");
                assert_eq!(body.unwrap(), "Content: Do the thing");
                assert_eq!(base.unwrap(), "main");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn substitute_teardown_create_pr_defaults_from_work_item() {
        let step = TeardownStep::CreatePullRequest {
            title: None,
            body: None,
            base: None,
        };
        let result = substitute_teardown_step(&step, Some(&wi()));
        match result {
            TeardownStep::CreatePullRequest { title, body, base } => {
                assert_eq!(title.unwrap(), "Implement awman/work-item-0042");
                assert_eq!(body.unwrap(), "Do the thing");
                assert!(base.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn substitute_teardown_create_pr_defaults_without_work_item() {
        let step = TeardownStep::CreatePullRequest {
            title: None,
            body: None,
            base: None,
        };
        let result = substitute_teardown_step(&step, None);
        match result {
            TeardownStep::CreatePullRequest { title, body, .. } => {
                assert!(title.is_none());
                assert!(body.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn substitute_setup_checkout_branch() {
        let step = SetupStep::CheckoutCreateBranch {
            branch: "feature/{{work_item_number}}".to_string(),
            base: Some("main".to_string()),
        };
        let result = substitute_setup_step(&step, Some(&wi()));
        match result {
            SetupStep::CheckoutCreateBranch { branch, base } => {
                assert_eq!(branch, "feature/0042");
                assert_eq!(base.unwrap(), "main");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn substitute_noop_without_context() {
        let step = TeardownStep::CommitChanges {
            message: "Implement {{work_item_number}}".to_string(),
            add_all: false,
        };
        let result = substitute_teardown_step(&step, None);
        assert_eq!(
            result,
            TeardownStep::CommitChanges {
                message: "Implement ".to_string(),
                add_all: false,
            }
        );
    }

    #[test]
    fn substitute_leaves_non_template_strings_intact() {
        let step = TeardownStep::PushBranch {
            remote: Some("origin".to_string()),
            branch: Some("feature/x".to_string()),
        };
        let result = substitute_teardown_step(&step, Some(&wi()));
        assert_eq!(result, step);
    }
}

#[cfg(test)]
mod push_auth_tests {
    use super::*;

    fn push(remote: Option<&str>, branch: Option<&str>) -> TeardownStep {
        TeardownStep::PushBranch {
            remote: remote.map(String::from),
            branch: branch.map(String::from),
        }
    }

    fn with(auth: PushAuth, step: &TeardownStep) -> (String, Option<HashMap<String, String>>) {
        teardown_step_to_shell_with(
            step,
            &StepCommandOptions {
                push_auth: auth,
                remote_url: None,
            },
        )
    }

    #[test]
    fn defaults_unchanged() {
        let step = push(None, None);
        assert_eq!(
            teardown_step_to_shell(&step),
            with(PushAuth::None, &step),
            "PushAuth::None must equal the pre-grant behaviour"
        );
        let (cmd, env) = teardown_step_to_shell(&step);
        assert!(cmd.ends_with("push -u -- 'origin' 'HEAD'"));
        assert_eq!(
            env.unwrap()["GIT_SSH_COMMAND"],
            "ssh -o StrictHostKeyChecking=accept-new"
        );
        let commit = TeardownStep::CommitChanges {
            message: "m".into(),
            add_all: true,
        };
        assert_eq!(
            teardown_step_to_shell(&commit),
            with(
                PushAuth::Ssh {
                    key_path: "/k".into(),
                    known_hosts: None
                },
                &commit
            ),
            "push auth never changes non-push steps"
        );
    }

    #[test]
    fn ssh_push_auth_sets_identities_only_and_known_hosts() {
        let (cmd, env) = with(
            PushAuth::Ssh {
                key_path: "/root/.ssh/awman_push_key".into(),
                known_hosts: Some("/root/.ssh/known_hosts".into()),
            },
            &push(None, None),
        );
        assert!(cmd.ends_with("push -u -- 'origin' 'HEAD'"));
        assert_eq!(
            env.unwrap()["GIT_SSH_COMMAND"],
            "ssh -F /dev/null -i '/root/.ssh/awman_push_key' -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile='/root/.ssh/known_hosts'"
        );
        let (_, env) = with(
            PushAuth::Ssh {
                key_path: "/k".into(),
                known_hosts: None,
            },
            &push(None, None),
        );
        let ssh = &env.unwrap()["GIT_SSH_COMMAND"];
        assert!(ssh.contains("IdentitiesOnly=yes"));
        assert!(!ssh.contains("UserKnownHostsFile"));
    }

    #[test]
    fn ssh_key_path_is_shell_quoted() {
        let (_, env) = with(
            PushAuth::Ssh {
                key_path: "/k'; touch /pwned; echo '".into(),
                known_hosts: None,
            },
            &push(None, None),
        );
        let ssh = &env.unwrap()["GIT_SSH_COMMAND"];
        assert!(ssh.contains(r"'/k'\''; touch /pwned; echo '\'''"), "{ssh}");
    }

    #[test]
    fn https_push_auth_uses_env_var_name_only() {
        let (cmd, env) = with(
            PushAuth::HttpsToken {
                env_var: "AWMAN_GIT_TOKEN".into(),
            },
            &push(None, Some("feature/x")),
        );
        assert!(cmd.contains("-c credential.helper="), "{cmd}");
        assert!(cmd.contains("AWMAN_GIT_TOKEN"));
        assert!(cmd.ends_with("push -u -- 'origin' 'feature/x'"), "{cmd}");
        // The helper references the variable; the value can never be in argv.
        assert!(cmd.contains("$AWMAN_GIT_TOKEN"));
        assert_eq!(env.unwrap()["GIT_TERMINAL_PROMPT"], "0");
    }

    #[test]
    fn push_refuses_option_like_branches_regardless_of_auth() {
        for branch in ["-f", "+main", "a:b"] {
            let (cmd, _) = with(PushAuth::None, &push(None, Some(branch)));
            assert!(cmd.contains("exit 2"), "{branch}: {cmd}");
            assert!(!cmd.contains("git push"));
        }
    }

    #[test]
    fn create_pull_request_command_is_independent_of_push_auth() {
        let step = TeardownStep::CreatePullRequest {
            title: Some("t".into()),
            body: None,
            base: Some("main".into()),
        };
        assert_eq!(
            teardown_step_to_shell(&step),
            with(
                PushAuth::HttpsToken {
                    env_var: "AWMAN_GIT_TOKEN".into()
                },
                &step
            )
        );
    }
}

#[cfg(test)]
mod review_transport_tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};
    #[test]
    fn https_helper_obeys_git_protocol_and_restricts_destination() {
        let opts = StepCommandOptions {
            push_auth: PushAuth::HttpsToken {
                env_var: "AWMAN_GIT_TOKEN".into(),
            },
            remote_url: Some("https://github.com/o/r.git".into()),
        };
        let args = shell_words::split(&transport_git(&opts)).unwrap();
        for (host, path, allowed) in [
            ("github.com", "o/r.git", true),
            ("evil.test", "o/r.git", false),
            ("github.com", "other/repo", false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut child = Command::new("git")
                .args(&args[1..])
                .args(["credential", "fill"])
                .current_dir(dir.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("AWMAN_GIT_TOKEN", "fixture_secret")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            writeln!(
                child.stdin.take().unwrap(),
                "protocol=https\nhost={host}\npath={path}\n"
            )
            .unwrap();
            let output = child.wait_with_output().unwrap();
            assert_eq!(
                output.status.success(),
                allowed,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).contains("password=fixture_secret"),
                allowed
            );
            assert!(!args.iter().any(|a| a.contains("fixture_secret")));
        }
    }
    #[test]
    fn setup_uses_selected_transport_and_pinned_destination() {
        let opts = StepCommandOptions {
            push_auth: PushAuth::Ssh {
                key_path: "/root/.ssh/awman_push_key".into(),
                known_hosts: Some("/root/.ssh/known_hosts".into()),
            },
            remote_url: Some("ssh://git@trusted.test/o/r".into()),
        };
        let step: SetupStep = serde_json::from_value(
            serde_json::json!({"type":"pull_branch", "remote":"origin", "branch":"main"}),
        )
        .unwrap();
        let (shell, env) = setup_step_to_shell_with(&step, &opts);
        assert!(shell.contains("ssh://git@trusted.test/o/r"));
        assert!(!shell.contains("'origin'"));
        let env = env.unwrap();
        let ssh = &env["GIT_SSH_COMMAND"];
        assert!(ssh.contains("-F /dev/null"));
        assert!(ssh.contains("awman_push_key"));
        assert!(ssh.contains("IdentitiesOnly=yes"));
    }
}
