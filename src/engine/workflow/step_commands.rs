//! Step-to-command translation — Layer 1.
//!
//! Pure functions that translate `SetupStep` and `TeardownStep` values into
//! shell command strings and optional per-step env var overrides. No external
//! dependencies — stateless mapping only.

use std::collections::HashMap;

use crate::data::workflow_definition::{SetupStep, TeardownStep};
use crate::data::workflow_prompt_template::{extract_section, substitute_prompt, WorkItemContext};

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

/// Translate a setup step into a shell command string and optional env overrides.
pub fn setup_step_to_shell(step: &SetupStep) -> (String, Option<HashMap<String, String>>) {
    match step {
        SetupStep::CloneRepo { url, branch, into } => {
            let mut cmd = "git clone".to_string();
            if let Some(b) = branch {
                cmd.push_str(&format!(" -b {}", sh_quote(b)));
            }
            cmd.push_str(&format!(" {}", sh_quote(url)));
            if let Some(d) = into {
                cmd.push_str(&format!(" {}", sh_quote(d)));
            }
            (cmd, None)
        }
        SetupStep::CheckoutCreateBranch { branch, base } => {
            let b_q = sh_quote(branch);
            let cmd = match base {
                Some(base) => format!(
                    "git fetch origin 2>/dev/null; git checkout -B {b_q} {base_q} 2>/dev/null || git checkout {b_q} && git pull origin {b_q} 2>/dev/null || true",
                    base_q = sh_quote(base),
                ),
                None => format!(
                    "git fetch origin 2>/dev/null; git checkout {b_q} 2>/dev/null && git pull origin {b_q} 2>/dev/null || git checkout -b {b_q}"
                ),
            };
            (cmd, None)
        }
        SetupStep::PullBranch { remote, branch } => {
            let cmd = match (remote, branch) {
                (Some(r), Some(b)) => format!("git pull {} {}", sh_quote(r), sh_quote(b)),
                _ => "git pull".to_string(),
            };
            (cmd, None)
        }
        SetupStep::RunShell { command, env } => (command.clone(), env.clone()),
        SetupStep::RunScript { path, env } => (format!("sh {}", sh_quote(path)), env.clone()),
    }
}

/// Translate a teardown step into a shell command string and optional env overrides.
pub fn teardown_step_to_shell(step: &TeardownStep) -> (String, Option<HashMap<String, String>>) {
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
            if let Some(base) = base {
                cmd.push_str(&format!(" --base {}", sh_quote(base)));
            }
            (cmd, None)
        }
        TeardownStep::PushBranch { remote, branch } => {
            let cmd = match (remote, branch) {
                (Some(r), Some(b)) => format!("git push {} {}", sh_quote(r), sh_quote(b)),
                _ => "git push".to_string(),
            };
            let mut env = HashMap::new();
            env.insert(
                "GIT_SSH_COMMAND".to_string(),
                "ssh -o StrictHostKeyChecking=accept-new".to_string(),
            );
            (cmd, Some(env))
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
        SetupStep::CloneRepo { url, branch, into } => SetupStep::CloneRepo {
            url: sub(url, ctx),
            branch: sub_opt(branch, ctx),
            into: sub_opt(into, ctx),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_repo_full() {
        let step = SetupStep::CloneRepo {
            url: "https://github.com/org/repo".to_string(),
            branch: Some("main".to_string()),
            into: Some("subdir".to_string()),
        };
        let (cmd, env) = setup_step_to_shell(&step);
        assert_eq!(
            cmd,
            "git clone -b 'main' 'https://github.com/org/repo' 'subdir'"
        );
        assert!(env.is_none());
    }

    #[test]
    fn clone_repo_minimal() {
        let step = SetupStep::CloneRepo {
            url: "https://github.com/org/repo".to_string(),
            branch: None,
            into: None,
        };
        let (cmd, _) = setup_step_to_shell(&step);
        assert_eq!(cmd, "git clone 'https://github.com/org/repo'");
    }

    #[test]
    fn checkout_create_branch_with_base() {
        let step = SetupStep::CheckoutCreateBranch {
            branch: "feature/x".to_string(),
            base: Some("main".to_string()),
        };
        let (cmd, _) = setup_step_to_shell(&step);
        assert!(cmd.contains("git checkout -B 'feature/x' 'main'"));
    }

    #[test]
    fn checkout_create_branch_without_base() {
        let step = SetupStep::CheckoutCreateBranch {
            branch: "feature/x".to_string(),
            base: None,
        };
        let (cmd, _) = setup_step_to_shell(&step);
        assert!(cmd.contains("git checkout 'feature/x'"));
        assert!(cmd.contains("git checkout -b 'feature/x'"));
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
        assert_eq!(cmd, "git push 'origin' 'feature/x'");
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
        assert_eq!(cmd, "git push");
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
