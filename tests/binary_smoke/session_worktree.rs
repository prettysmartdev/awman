//! `--branch`, `--pr` and `--worktree` on `chat` and `exec prompt` at the
//! binary boundary: help text, conflicts, missing values, and the commands
//! that must not gain them. Nothing here launches a container.

use crate::{awman, make_git_repo};

fn help(args: &[&str]) -> String {
    let output = awman()
        .args(args)
        .arg("--help")
        .output()
        .expect("run awman");
    assert!(output.status.success(), "{args:?} --help failed");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn chat_and_exec_prompt_help_list_the_three_selectors() {
    for command in [&["chat"][..], &["exec", "prompt"][..]] {
        let text = help(command);
        for flag in ["--branch", "--pr", "--worktree"] {
            assert!(
                text.contains(flag),
                "{command:?} --help lacks {flag}:\n{text}"
            );
        }
    }
}

#[test]
fn exec_workflow_help_keeps_boolean_worktree_and_has_no_branch_or_pr() {
    let text = help(&["exec", "workflow"]);
    assert!(text.contains("--worktree"), "{text}");
    assert!(!text.contains("--branch"), "{text}");
    assert!(!text.contains("--pr "), "{text}");
    assert!(!text.contains("--pr\n"), "{text}");
    assert!(!text.contains("--pr <"), "{text}");
}

#[test]
fn remote_exec_prompt_help_has_none_of_the_selectors() {
    let text = help(&["remote", "exec", "prompt"]);
    for flag in ["--branch", "--worktree", "--pr "] {
        assert!(
            !text.contains(flag),
            "remote exec prompt must not offer {flag}:\n{text}"
        );
    }
}

#[test]
fn conflicting_selectors_fail_before_any_command_runs() {
    let repo = make_git_repo();
    for command in [&["chat"][..], &["exec", "prompt"][..]] {
        for (a, b) in [
            ("--branch", "--pr"),
            ("--branch", "--worktree"),
            ("--pr", "--worktree"),
        ] {
            let mut args: Vec<&str> = command.to_vec();
            args.extend([a, "x", b, "y"]);
            if command.len() == 2 {
                args.push("do it");
            }
            let output = awman()
                .current_dir(repo.path())
                .args(&args)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2), "{args:?}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(a) && stderr.contains(b) && stderr.contains("cannot be used"),
                "{args:?}: {stderr}"
            );
        }
    }
}

#[test]
fn selectors_require_a_value() {
    let repo = make_git_repo();
    for flag in ["--branch", "--pr", "--worktree"] {
        let output = awman()
            .current_dir(repo.path())
            .args(["exec", "prompt", flag])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{flag}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(flag), "{flag}: {stderr}");
    }
}

#[test]
fn workflow_rejects_branch_and_pr() {
    let repo = make_git_repo();
    for flag in ["--branch", "--pr"] {
        let output = awman()
            .current_dir(repo.path())
            .args(["exec", "workflow", "wf.toml", flag, "x"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{flag}");
    }
}

/// A repository with no commit cannot back a worktree; the command refuses
/// before it reaches Docker, image setup or any credential lookup.
#[test]
fn selectors_in_a_repository_without_commits_fail_cleanly() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .env("AWMAN_TEST_ISOLATION", "1")
        .args([
            "exec",
            "prompt",
            "--non-interactive",
            "--branch",
            "feature/x",
            "do it",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        !repo.path().join(".awman").exists(),
        "nothing may be provisioned"
    );
}

#[test]
fn chat_selectors_require_a_value() {
    let repo = make_git_repo();
    for flag in ["--branch", "--pr", "--worktree"] {
        let output = awman()
            .current_dir(repo.path())
            .args(["chat", flag])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{flag}");
    }
}

#[test]
fn a_selector_after_the_prompt_is_refused_instead_of_running_in_place() {
    let repo = make_git_repo();
    for selector in ["--branch", "--pr", "--worktree"] {
        let output = awman()
            .current_dir(repo.path())
            .env("AWMAN_TEST_ISOLATION", "1")
            .args([
                "exec",
                "prompt",
                "--non-interactive",
                "Fix the failing test",
                selector,
                "482",
            ])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{selector}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("after the prompt"), "{selector}: {stderr}");
        assert!(
            !repo.path().join(".awman").exists(),
            "{selector}: nothing may be provisioned"
        );
    }
}
