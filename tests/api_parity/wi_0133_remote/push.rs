//! The push stage after a successful command in a remote session, and the
//! `push_branch` step's git semantics, against a `git daemon` origin (WI 0133).

use serde_json::{json, Value};

use super::api_process::{clone_path, events_log, AgentMode, ApiHome};
use super::git_daemon::{git, git_output, Origin};

/// The `StatusMessage` lines the push stage emitted for one command.
fn push_messages(events_log: &str) -> Vec<String> {
    events_log
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| {
            event["payload"]["type"] == "StatusMessage"
                && event["payload"]["data"]["phase"] == "push"
        })
        .filter_map(|event| {
            event["payload"]["data"]["message"]
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

/// Files in `rev`'s tree in the bare origin.
fn origin_tree(origin: &Origin, rev: &str) -> Vec<String> {
    origin
        .git_bare(&["ls-tree", "-r", "--name-only", rev])
        .lines()
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_dynamic_remote_run_on_fresh_branch_with_base_ref_and_two_merge_refs_pushes() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    // `v1` is the base; `main` has moved past it since, and the two features
    // were cut from the base.
    let base = origin.git_bare(&["rev-parse", "main"]);
    origin.tag("v1", &base);
    let main_tip = origin.commit_on("main", None, &[("after-v1.txt", "x\n")], "after v1");
    let one = origin.commit_on("feature-one", Some("v1"), &[("one.txt", "1\n")], "one");
    let two = origin.commit_on("feature-two", Some("v1"), &[("two.txt", "2\n")], "two");
    let home = ApiHome::new();
    let server = home.start().await;

    let id = server
        .ready_session(json!({
            "session_type": "remote",
            "repo_url": origin.url,
            "branch": "wi-42",
            "base_ref": "v1",
            "merge_refs": ["feature-one", "feature-two"],
            "push": true,
        }))
        .await;
    let session = server.session(&id).await;
    assert_eq!(session["merge_refs"], json!(["feature-one", "feature-two"]));
    assert_eq!(origin.branch_sha("wi-42"), None, "setup must not push");

    let record = server
        .run_command(&id, "exec workflow", &["--dynamic", "--work-item", "0042"])
        .await;

    assert_eq!(record["status"], "done", "{record}\n{}", server.log());
    assert_eq!(record["exit_code"], 0, "{record}");
    let pushed = &record["result"]["pushed"];
    assert_eq!(pushed["remote"], "origin", "{record}");
    assert_eq!(pushed["branch"], "wi-42", "{record}");
    let sha = pushed["sha"].as_str().expect("pushed sha").to_string();
    // Verified over git://, not from awman's own report.
    assert_eq!(origin.branch_sha("wi-42"), Some(sha.clone()));

    let subject = origin.git_bare(&["log", "-1", "--format=%s|%ae", &sha]);
    assert_eq!(subject, "Implement work item 0042|awman@localhost");
    let tree = origin_tree(&origin, &sha);
    for expected in ["one.txt", "two.txt", "agent-edit.txt", "shared.txt"] {
        assert!(
            tree.iter().any(|f| f == expected),
            "{expected} not in {tree:?}"
        );
    }
    // Cut from `v1`, not from where `main` is now.
    assert!(!tree.iter().any(|f| f == "after-v1.txt"), "{tree:?}");
    // awman's own workflow bookkeeping is never committed.
    assert!(
        !tree.iter().any(|f| f.starts_with(".awman/workflows/")),
        "{tree:?}"
    );
    let history = origin.git_bare(&["rev-list", &sha]);
    for ancestor in [&base, &one, &two] {
        assert!(history.contains(ancestor.as_str()), "{ancestor} not merged");
    }
    assert!(!history.contains(main_tip.as_str()));
    // The two setup merges, then the leftover commit, on top of the base.
    let first_parents = origin.git_bare(&["log", "--first-parent", "--format=%s", &sha]);
    let first_parents: Vec<&str> = first_parents.lines().collect();
    assert_eq!(first_parents.len(), 4, "{first_parents:?}");
    assert_eq!(first_parents[3], "base");

    // Every other ref on the origin is untouched.
    let refs = origin.ls_remote();
    assert_eq!(refs.get("refs/heads/main"), Some(&main_tip));
    assert_eq!(refs.get("refs/heads/feature-one"), Some(&one));
    assert_eq!(refs.get("refs/heads/feature-two"), Some(&two));
    let clone = clone_path(&session);
    assert!(clone.join(".git").is_dir());
    assert!(
        !clone.join(".git/worktrees").exists(),
        "remote dynamic must not create a worktree"
    );
    assert_eq!(
        git(&clone, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
    assert!(git(&clone, &["status", "--porcelain"]).is_empty());
    assert_eq!(
        git(&clone, &["rev-parse", "--abbrev-ref", "wi-42@{upstream}"]),
        "origin/wi-42"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_exec_prompt_with_push_commits_and_pushes_uncommitted_agent_edits() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let home = ApiHome::new();
    let server = home.start().await;
    let id = server
        .ready_session(json!({
            "session_type": "remote",
            "repo_url": origin.url,
            "branch": "prompt-work",
            "push": true,
        }))
        .await;

    let record = server
        .run_command(&id, "exec prompt", &["edit a file"])
        .await;

    assert_eq!(record["status"], "done", "{record}");
    assert_eq!(record["exit_code"], 0, "{record}");
    let sha = record["result"]["pushed"]["sha"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(record["result"]["pushed"]["branch"], "prompt-work");
    assert_eq!(origin.branch_sha("prompt-work"), Some(sha.clone()));
    assert_eq!(
        origin.git_bare(&["log", "-1", "--format=%s|%an <%ae>", &sha]),
        format!("awman: exec prompt in session {id}|awman <awman@localhost>")
    );
    assert_eq!(
        origin.git_bare(&["show", &format!("{sha}:agent-edit.txt")]),
        "edited by the fake agent"
    );
    // Exactly the agent's change sits on top of `main`.
    let main = origin.branch_sha("main").unwrap();
    assert_eq!(origin.git_bare(&["rev-parse", &format!("{sha}^")]), main);
    assert_eq!(
        origin.git_bare(&["diff", "--name-only", &main, &sha]),
        "agent-edit.txt"
    );
    let clone = clone_path(&server.session(&id).await);
    assert!(git(&clone, &["status", "--porcelain"]).is_empty());

    let messages = push_messages(&events_log(&record).await);
    assert!(
        messages
            .iter()
            .any(|m| m == "$ git push -u origin HEAD:refs/heads/prompt-work"),
        "{messages:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_failed_command_does_not_push() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let refs_before = origin.ls_remote();
    let home = ApiHome::new();
    let server = home.start().await;
    let id = server
        .ready_session(json!({
            "session_type": "remote",
            "repo_url": origin.url,
            "branch": "doomed",
            "push": true,
        }))
        .await;
    let clone = clone_path(&server.session(&id).await);
    let head_before = git(&clone, &["rev-parse", "HEAD"]);

    home.set_agent(AgentMode::Fail);
    let record = server
        .run_command(&id, "exec prompt", &["edit then fail"])
        .await;

    assert_eq!(record["status"], "error", "{record}");
    assert_ne!(record["exit_code"], 0, "{record}");
    let result = record["result"].as_object().expect("result object");
    assert!(!result.contains_key("pushed"), "{record}");
    assert!(!result.contains_key("stage"), "{record}");
    // Nothing committed, nothing pushed: the edit is still loose in the clone.
    assert_eq!(origin.ls_remote(), refs_before);
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&clone, &["status", "--porcelain"]), "?? agent-edit.txt");
    assert!(push_messages(&events_log(&record).await).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_no_changes_run_reports_pushed_null_and_creates_no_remote_ref() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    // An existing branch the session continues: nothing to publish unless
    // the command changes something.
    origin.commit_on("ongoing", Some("main"), &[("o.txt", "o\n")], "ongoing");
    let refs_before = origin.ls_remote();
    let home = ApiHome::new();
    let server = home.start().await;
    let id = server
        .ready_session(json!({
            "session_type": "remote",
            "repo_url": origin.url,
            "branch": "ongoing",
            "push": true,
        }))
        .await;
    let clone = clone_path(&server.session(&id).await);
    let head_before = git(&clone, &["rev-parse", "HEAD"]);

    home.set_agent(AgentMode::NoOp);
    let record = server
        .run_command(&id, "exec prompt", &["look around"])
        .await;

    assert_eq!(record["status"], "done", "{record}");
    let result = record["result"].as_object().expect("result object");
    // Present and null: pushing is configured, there was nothing to push.
    assert_eq!(result.get("pushed"), Some(&Value::Null), "{record}");
    assert_eq!(origin.ls_remote(), refs_before);
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head_before);
    let messages = push_messages(&events_log(&record).await);
    // The stage ran, found nothing staged, and stopped short of a push.
    assert!(
        messages
            .iter()
            .any(|m| m == "$ git diff --cached --name-only"),
        "{messages:#?}"
    );
    assert!(
        !messages.iter().any(|m| m.starts_with("$ git push")),
        "{messages:#?}"
    );
    assert!(
        !messages.iter().any(|m| m.contains(" commit -m ")),
        "{messages:#?}"
    );

    // Moving the remote does not turn an unchanged command into an attempted
    // stale push. There is still no new local work to publish.
    let theirs = origin.commit_on("ongoing", None, &[("theirs.txt", "theirs\n")], "theirs");
    let record = server
        .run_command(&id, "exec prompt", &["look again"])
        .await;
    assert_eq!(record["status"], "done", "{record}");
    assert_eq!(record["result"]["pushed"], Value::Null);
    assert_eq!(origin.branch_sha("ongoing"), Some(theirs));
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head_before);
    assert!(!push_messages(&events_log(&record).await)
        .iter()
        .any(|m| m.starts_with("$ git push")));
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_non_fast_forward_push_errors_at_stage_push_and_is_never_forced() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    origin.commit_on("contested", Some("main"), &[("c.txt", "c\n")], "contested");
    let home = ApiHome::new();
    let server = home.start().await;
    let id = server
        .ready_session(json!({
            "session_type": "remote",
            "repo_url": origin.url,
            "branch": "contested",
            "push": true,
        }))
        .await;
    let clone = clone_path(&server.session(&id).await);
    // Someone else moves the branch after the session cloned it.
    let theirs = origin.commit_on("contested", None, &[("theirs.txt", "theirs\n")], "theirs");

    let record = server
        .run_command(&id, "exec prompt", &["edit a file"])
        .await;

    assert_eq!(record["status"], "error", "{record}");
    assert_eq!(record["exit_code"], 1, "{record}");
    let result = &record["result"];
    assert_eq!(result["stage"], "push", "{record}");
    assert_eq!(result["error_status"], 500, "{record}");
    assert_eq!(result["pushed"], Value::Null, "{record}");
    assert!(
        result.as_object().unwrap().contains_key("pushed"),
        "{record}"
    );
    let error = result["error"].as_str().unwrap();
    assert!(
        error.contains("git push -u origin HEAD:refs/heads/contested failed")
            && (error.contains("rejected") || error.contains("fetch first")),
        "{error}"
    );
    // Their commit survives: no force push happened.
    assert_eq!(origin.branch_sha("contested"), Some(theirs.clone()));
    // Ours stays committed in the clone for the caller to reconcile.
    let ours = git(&clone, &["rev-parse", "HEAD"]);
    assert_ne!(ours, theirs);
    assert!(git(&clone, &["status", "--porcelain"]).is_empty());
    assert_eq!(
        git(&clone, &["log", "-1", "--format=%s"]),
        format!("awman: exec prompt in session {id}")
    );
    let messages = push_messages(&events_log(&record).await);
    let pushes: Vec<&String> = messages
        .iter()
        .filter(|m| m.starts_with("$ git push"))
        .collect();
    assert_eq!(
        pushes,
        ["$ git push -u origin HEAD:refs/heads/contested"],
        "{messages:#?}"
    );
    assert!(
        !messages
            .iter()
            .any(|m| m.contains("--force") || m.contains(" -f ") || m.contains("+HEAD")),
        "{messages:#?}"
    );
    let events = events_log(&record).await;
    let last_status = events
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["payload"]["type"] == "CommandStatus")
        .last()
        .unwrap();
    assert_eq!(
        last_status["payload"]["data"]["status"], "error",
        "{events}"
    );
    assert!(last_status["payload"]["data"]["error"]
        .as_str()
        .unwrap()
        .contains("push:"));
}

/// The command `push_branch` (no remote, no branch) translates to, run for
/// real in a clone on a branch that has no upstream — where a bare
/// `git push` fails.
#[test]
fn real_git_push_branch_step_on_a_branch_with_no_upstream_succeeds() {
    use awman::data::workflow_definition::TeardownStep;
    use awman::engine::workflow::step_commands::teardown_step_to_shell;

    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let work = tempfile::tempdir().unwrap();
    git(work.path(), &["clone", "-q", &origin.url, "clone"]);
    let clone = work.path().join("clone");
    git(&clone, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(clone.join("topic.txt"), "topic\n").unwrap();
    git(&clone, &["add", "-A"]);
    git(
        &clone,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@awman.test",
            "commit",
            "-q",
            "-m",
            "topic",
        ],
    );
    let head = git(&clone, &["rev-parse", "HEAD"]);

    // What `push_branch` used to run: refused, because nothing is upstream.
    let bare_push = git_output(&clone, &["push"]);
    assert!(!bare_push.status.success());
    assert!(
        String::from_utf8_lossy(&bare_push.stderr).contains("no upstream"),
        "{}",
        String::from_utf8_lossy(&bare_push.stderr)
    );
    assert_eq!(origin.branch_sha("topic"), None);

    let (command, env) = teardown_step_to_shell(&TeardownStep::PushBranch {
        remote: None,
        branch: None,
    });
    // Run it as the step runner does: through `sh -c`, with the step's env.
    let output = std::process::Command::new("sh")
        .args(["-c", &command])
        .current_dir(&clone)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(env.unwrap_or_default())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(origin.branch_sha("topic"), Some(head));
    assert_eq!(git(&clone, &["config", "branch.topic.remote"]), "origin");
    assert_eq!(
        git(&clone, &["config", "branch.topic.merge"]),
        "refs/heads/topic"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_fresh_noop_is_not_published_but_setup_merges_are() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    origin.commit_on(
        "dependency",
        Some("main"),
        &[("dependency.txt", "dep\n")],
        "dependency",
    );
    let home = ApiHome::new();
    home.set_agent(AgentMode::NoOp);
    let server = home.start().await;
    for (branch, merges) in [
        ("fresh-noop", json!([])),
        ("merged-noop", json!(["dependency"])),
    ] {
        let id = server
            .ready_session(json!({"session_type":"remote", "repo_url":origin.url,
            "branch":branch, "merge_refs":merges, "push":true}))
            .await;
        let clone = clone_path(&server.session(&id).await);
        let before = git(&clone, &["rev-parse", "HEAD"]);
        let record = server
            .run_command(&id, "exec prompt", &["inspect only"])
            .await;
        assert_eq!(record["status"], "done", "{record}");
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), before);
        if branch == "fresh-noop" {
            assert_eq!(record["result"]["pushed"], Value::Null, "{record}");
            assert_eq!(origin.branch_sha(branch), None);
            assert!(!push_messages(&events_log(&record).await)
                .iter()
                .any(|m| m.starts_with("$ git push")));
        } else {
            assert_eq!(record["result"]["pushed"]["sha"], before, "{record}");
            assert_eq!(origin.branch_sha(branch), Some(before));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_host_push_ignores_agent_hooks_filters_helpers_and_pushurl() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let home = ApiHome::new();
    let server = home.start().await;
    let id = server
        .ready_session(json!({"session_type":"remote", "repo_url":origin.url,
        "branch":"safe-push", "push":true}))
        .await;
    home.set_agent(AgentMode::Poison);
    let record = server.run_command(&id, "exec prompt", &["edit"]).await;
    assert_eq!(record["status"], "done", "{record}\n{}", server.log());
    assert!(
        !home.control().join("host-executed").exists(),
        "agent helper executed on host"
    );
    let sha = origin.branch_sha("safe-push").unwrap();
    assert_eq!(record["result"]["pushed"]["sha"], sha);
    assert!(origin_tree(&origin, &sha).contains(&"agent-edit.txt".to_string()));
    let events = events_log(&record).await;
    assert!(events.contains("HEAD:refs/heads/safe-push"));
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_tag_names_cannot_shadow_checkout_merge_or_push() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let base = origin.git_bare(&["rev-parse", "main"]);
    let feature = origin.commit_on("feature", Some("main"), &[("f.txt", "f\n")], "feature");
    origin.tag("feature", &base);
    origin.tag("origin/feature", &base);
    origin.tag("release", &base);
    let home = ApiHome::new();
    let server = home.start().await;
    let existing = server
        .ready_session(json!({"session_type":"remote", "repo_url":origin.url,
        "branch":"feature"}))
        .await;
    let clone = clone_path(&server.session(&existing).await);
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), feature);
    assert_eq!(git(&clone, &["symbolic-ref", "HEAD"]), "refs/heads/feature");
    let id = server
        .ready_session(json!({"session_type":"remote", "repo_url":origin.url,
        "branch":"release", "merge_refs":["feature"], "push":true}))
        .await;
    let record = server.run_command(&id, "exec prompt", &["edit"]).await;
    assert_eq!(record["status"], "done", "{record}");
    let tip = origin.branch_sha("release").unwrap();
    assert!(origin_tree(&origin, &tip).contains(&"f.txt".to_string()));
    assert_eq!(origin.git_bare(&["rev-parse", "refs/tags/release"]), base);
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_dynamic_teardown_push_pr_and_automatic_push_execute_in_order() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    for (branch, after_pr) in [
        ("double-push", ""),
        (
            "leftovers",
            "[[teardown]]\ntype = 'run_shell'\ncommand = 'echo leftover > after-pr.txt'\n",
        ),
        (
            "failed-teardown",
            "[[teardown]]\ntype = 'run_shell'\ncommand = 'exit 9'\n",
        ),
    ] {
        let home = ApiHome::new();
        home.set_teardown(&format!(
            r#"
[[teardown]]
type = "run_shell"
command = "git config user.name Test && git config user.email test@localhost"
[[teardown]]
type = "commit_changes"
message = "workflow commit"
add_all = true
[[teardown]]
type = "push_branch"
[[teardown]]
type = "create_pull_request"
{after_pr}
"#
        ));
        let server = home.start().await;
        let id = server
            .ready_session(json!({"session_type":"remote", "repo_url":origin.url,
            "branch":branch, "push":true}))
            .await;
        let record = server
            .run_command(&id, "exec workflow", &["--dynamic", "--work-item", "0042"])
            .await;
        let pr_head = std::fs::read_to_string(home.control().join("pr-head"))
            .unwrap_or_else(|e| panic!("PR did not run: {e}; {record}\n{}", server.log()))
            .trim()
            .to_string();
        let pr_origin = std::fs::read_to_string(home.control().join("pr-origin")).unwrap();
        assert!(pr_origin.starts_with(&pr_head));
        assert_eq!(
            std::fs::read_to_string(home.control().join("pr-upstream"))
                .unwrap()
                .trim(),
            format!("origin/{branch}")
        );
        if branch == "failed-teardown" {
            assert_eq!(record["status"], "error", "{record}");
            assert_eq!(origin.branch_sha(branch), Some(pr_head));
            assert!(!record["result"].as_object().unwrap().contains_key("pushed"));
        } else {
            assert_eq!(record["status"], "done", "{record}\n{}", server.log());
            let final_sha = origin.branch_sha(branch).unwrap();
            assert_eq!(record["result"]["pushed"]["sha"], final_sha, "{record}");
            if branch == "double-push" {
                assert_eq!(
                    final_sha, pr_head,
                    "automatic push must not create a redundant commit"
                );
            } else {
                assert_ne!(final_sha, pr_head);
                assert_eq!(
                    origin.git_bare(&["rev-parse", &format!("{final_sha}^")]),
                    pr_head
                );
                assert!(origin_tree(&origin, &final_sha).contains(&"after-pr.txt".into()));
            }
        }
        events_log(&record).await;
    }
}

#[test]
fn real_git_push_branch_cannot_force_through_options_or_refspecs() {
    use awman::data::workflow_definition::TeardownStep;
    use awman::engine::workflow::step_commands::teardown_step_to_shell;
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let work = tempfile::tempdir().unwrap();
    git(work.path(), &["clone", "-q", &origin.url, "clone"]);
    let clone = work.path().join("clone");
    let tip = origin.commit_on("main", None, &[("new.txt", "new")], "new");
    for (remote, branch) in [
        ("origin", "-f"),
        ("origin", "--force"),
        ("--force", "HEAD"),
        ("origin", "+HEAD:main"),
    ] {
        let (command, env) = teardown_step_to_shell(&TeardownStep::PushBranch {
            remote: Some(remote.into()),
            branch: Some(branch.into()),
        });
        let output = std::process::Command::new("sh")
            .args(["-c", &command])
            .current_dir(&clone)
            .envs(env.unwrap_or_default())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{command}");
        assert_eq!(origin.branch_sha("main"), Some(tip.clone()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_remote_leader_cannot_rewrite_dirty_files_or_create_clean_commits() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    for (branch, mode) in [
        ("rewrite", AgentMode::LeaderRewrite),
        ("commit", AgentMode::LeaderCommit),
    ] {
        let home = ApiHome::new();
        let server = home.start().await;
        let id = server
            .ready_session(json!({"session_type":"remote", "repo_url":origin.url, "branch":branch}))
            .await;
        let clone = clone_path(&server.session(&id).await);
        std::fs::write(clone.join("shared.txt"), "already dirty\n").unwrap();
        home.set_agent(mode);
        let record = server
            .run_command(&id, "exec workflow", &["--dynamic", "--work-item", "0042"])
            .await;
        assert_eq!(record["status"], "error", "{record}");
        assert!(
            record["result"]["error"]
                .as_str()
                .unwrap()
                .contains("leader agent modified files in the session clone"),
            "{record}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_host_commit_excludes_pre_staged_bookkeeping() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    std::fs::create_dir_all(origin.seed().join(".awman/workflows")).unwrap();
    origin.commit_on(
        "main",
        None,
        &[
            (".awman/workflows/modified.json", "original"),
            (".awman/workflows/deleted.json", "original"),
        ],
        "tracked state",
    );
    for (branch, mode) in [
        ("state-only", AgentMode::NoOp),
        ("state-and-work", AgentMode::Edit),
    ] {
        let home = ApiHome::new();
        let server = home.start().await;
        let id = server
            .ready_session(json!({"session_type":"remote", "repo_url":origin.url,
            "branch":branch, "push":true}))
            .await;
        let clone = clone_path(&server.session(&id).await);
        let dir = clone.join(".awman/workflows");
        std::fs::write(dir.join("modified.json"), "modified").unwrap();
        std::fs::remove_file(dir.join("deleted.json")).unwrap();
        std::fs::write(dir.join("new.json"), "new").unwrap();
        std::fs::write(dir.join("dynamic-0042.toml"), "new").unwrap();
        git(&clone, &["add", "-A"]);
        let before = git(&clone, &["rev-parse", "HEAD"]);
        home.set_agent(mode);
        let record = server.run_command(&id, "exec prompt", &["inspect"]).await;
        assert_eq!(record["status"], "done", "{record}");
        if branch == "state-only" {
            assert_eq!(record["result"]["pushed"], Value::Null);
            assert_eq!(git(&clone, &["rev-parse", "HEAD"]), before);
            assert_eq!(origin.branch_sha(branch), None);
        } else {
            let sha = origin.branch_sha(branch).unwrap();
            assert_eq!(
                origin.git_bare(&["diff", "--name-only", &before, &sha]),
                "agent-edit.txt"
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("modified.json")).unwrap(),
            "modified"
        );
        assert!(dir.join("new.json").exists());
        assert!(!dir.join("deleted.json").exists());
    }
}
