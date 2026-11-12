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
///
/// Public so the component tier can run the exact transport prefix through
/// a real `git` (the credential-helper destination check).
pub fn transport_git(opts: &StepCommandOptions) -> String {
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
            // An agent that already committed its own work leaves nothing
            // staged. That is success, so the push and pull request steps
            // that follow still run and publish those commits.
            let commit = format!(
                "if git diff --cached --quiet; then echo 'commit_changes: nothing to commit'; \
                 else git commit -m {}; fi",
                sh_quote(message)
            );
            let cmd = if *add_all {
                format!("git add -A -- . && {commit}")
            } else {
                commit
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

    /// Whether the step gets local work onto the remote branch (a commit or
    /// a push). These are re-run after an on_failure agent fixes a failed
    /// `poll_ci`, so CI sees the fix before it is polled again.
    fn republishes_ci_fix(&self) -> bool {
        false
    }

    /// Whether the step is a `push_branch`.
    fn pushes_branch(&self) -> bool {
        false
    }

    /// The step as re-run to publish a fix for `failed_jobs`, when that
    /// differs from the step itself: a commit names what it fixes.
    fn for_ci_fix(&self, _failed_jobs: &[String]) -> Option<Self> {
        None
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

    fn republishes_ci_fix(&self) -> bool {
        matches!(
            self,
            TeardownStep::CommitChanges { .. } | TeardownStep::PushBranch { .. }
        )
    }

    fn pushes_branch(&self) -> bool {
        matches!(self, TeardownStep::PushBranch { .. })
    }

    fn for_ci_fix(&self, failed_jobs: &[String]) -> Option<Self> {
        match self {
            TeardownStep::CommitChanges { message, add_all } => Some(TeardownStep::CommitChanges {
                message: ci_fix_commit_message(message, failed_jobs),
                add_all: *add_all,
            }),
            _ => None,
        }
    }
}

/// How many failed job names a CI fix commit message lists before
/// summarising the rest as a count.
const CI_FIX_MESSAGE_MAX_JOBS: usize = 5;

/// `"<message>: fix CI job a, b, c"`, or `"<message>: fix CI"` when the
/// failed jobs are unknown.
fn ci_fix_commit_message(message: &str, failed_jobs: &[String]) -> String {
    if failed_jobs.is_empty() {
        return format!("{message}: fix CI");
    }
    let mut names = failed_jobs
        .iter()
        .take(CI_FIX_MESSAGE_MAX_JOBS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if failed_jobs.len() > CI_FIX_MESSAGE_MAX_JOBS {
        names.push_str(&format!(
            " and {} more",
            failed_jobs.len() - CI_FIX_MESSAGE_MAX_JOBS
        ));
    }
    let noun = if failed_jobs.len() == 1 {
        "job"
    } else {
        "jobs"
    };
    format!("{message}: fix CI {noun} {names}")
}

/// Defaults for a `poll_ci` step that names neither value. Unchanged from the
/// per-phase copies these replace.
const DEFAULT_POLL_CI_INTERVAL_SECS: u32 = 30;
const DEFAULT_POLL_CI_MAX_RETRIES: u32 = 10;
