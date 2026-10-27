//! Remote session setup against a `git daemon` origin (WI 0133): base ref,
//! merge refs, their failures, commit identity and restore.

use serde_json::json;

use super::api_process::{clone_path, ApiHome};
use super::git_daemon::{git, git_output, Origin};

/// Lines of the server log for one session (setup lines carry `[<id8>]`).
fn session_log_lines(log: &str, session_id: &str) -> Vec<String> {
    let tag = format!("[{}]", &session_id[..8]);
    log.lines()
        .filter(|line| line.contains(&tag))
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_merge_conflict_fails_setup_at_merging_refs_naming_the_path() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    origin.commit_on(
        "left",
        Some("main"),
        &[("shared.txt", "left edit\n")],
        "left",
    );
    origin.commit_on(
        "right",
        Some("main"),
        &[("shared.txt", "right edit\n")],
        "right",
    );
    let refs_before = origin.ls_remote();
    let home = ApiHome::new();
    let server = home.start().await;

    let id = server
        .create_session(json!({
            "session_type": "remote",
            "repo_url": origin.url,
            "branch": "combined",
            "merge_refs": ["left", "right"],
            "push": true,
        }))
        .await;
    let status = server.wait_for_setup(&id).await;

    assert_eq!(status["status"], "failed", "{status}");
    assert_eq!(status["error"]["stage"], "merging_refs", "{status}");
    let message = status["error"]["message"].as_str().unwrap();
    assert!(message.contains("origin/right"), "{message}");
    assert!(message.contains("shared.txt"), "{message}");
    // The first ref merged; the second is the one that conflicted.
    let lines = session_log_lines(&server.log(), &id).join("\n");
    assert!(lines.contains("state → MergingRefs"), "{lines}");
    assert!(!lines.contains("state → RunningReady"), "{lines}");
    // The partial clone is deleted, and nothing reached the origin.
    let clone = clone_path(&server.session(&id).await);
    assert!(!clone.exists(), "clone left behind at {}", clone.display());
    assert_eq!(origin.ls_remote(), refs_before);
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_existing_branch_with_base_ref_fails_at_setting_up_branch() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let feature = origin.commit_on("feature", Some("main"), &[("f.txt", "f\n")], "feature");
    let home = ApiHome::new();
    let server = home.start().await;

    let id = server
        .create_session(json!({
            "session_type": "remote",
            "repo_url": origin.url,
            "branch": "feature",
            "base_ref": "main",
        }))
        .await;
    let status = server.wait_for_setup(&id).await;

    assert_eq!(status["status"], "failed", "{status}");
    // The branch stage's failure key is "branch" (as for every branch-stage
    // failure); the status it failed in is `setting_up_branch`.
    assert_eq!(status["error"]["stage"], "branch", "{status}");
    let message = status["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("branch 'feature' already exists on origin; omit base_ref"),
        "{message}"
    );
    let lines = session_log_lines(&server.log(), &id).join("\n");
    assert!(
        lines.contains("state → SettingUpBranch: branch=feature"),
        "{lines}"
    );
    assert!(!lines.contains("state → MergingRefs"), "{lines}");
    assert!(!lines.contains("state → RunningReady"), "{lines}");
    assert!(!clone_path(&server.session(&id).await).exists());
    assert_eq!(origin.branch_sha("feature"), Some(feature));
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_merges_succeed_in_a_clone_with_no_git_identity() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let feature = origin.commit_on("feature", Some("main"), &[("f.txt", "f\n")], "feature");
    let home = ApiHome::new();
    let server = home.start().await;

    let id = server
        .ready_session(json!({
            "session_type": "remote",
            "repo_url": origin.url,
            "branch": "work",
            "merge_refs": ["feature"],
        }))
        .await;
    let clone = clone_path(&server.session(&id).await);

    // The server runs with no global or system git config, and the clone has
    // no `user.*` of its own: awman supplied its fallback for the merge only.
    assert!(!git_output(&clone, &["config", "user.email"])
        .status
        .success());
    assert!(!git_output(&clone, &["config", "user.name"])
        .status
        .success());
    assert_eq!(git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]), "work");
    let parents = git(&clone, &["log", "-1", "--format=%P"]);
    assert_eq!(
        parents.split_whitespace().count(),
        2,
        "not a merge: {parents}"
    );
    assert!(parents.contains(&feature), "{parents}");
    assert_eq!(
        git(&clone, &["log", "-1", "--format=%an <%ae>|%cn <%ce>"]),
        "awman <awman@localhost>|awman <awman@localhost>"
    );
    assert!(clone.join("f.txt").exists());
    assert!(git(&clone, &["status", "--porcelain"]).is_empty());
    let lines = session_log_lines(&server.log(), &id).join("\n");
    assert!(
        lines.contains("committing as awman <awman@localhost> (no git identity in the clone)"),
        "{lines}"
    );
    // Without `push`, setup publishes nothing.
    assert_eq!(origin.branch_sha("work"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_restored_session_keeps_repo_url_branch_and_push() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let home = ApiHome::new();
    let mut first = home.start().await;
    let id = first
        .ready_session(json!({
            "session_type": "remote",
            "repo_url": origin.url,
            "branch": "restored",
            "base_ref": "main",
            "push": true,
        }))
        .await;
    first.kill();

    let second = home.start().await;
    let session = second.session(&id).await;
    assert_eq!(session["status"], "active", "{session}");
    assert_eq!(session["session_type"], "remote", "{session}");
    assert_eq!(session["repo_url"], origin.url.as_str(), "{session}");
    assert_eq!(session["branch"], "restored", "{session}");
    assert_eq!(session["base_ref"], "main", "{session}");
    assert_eq!(session["push"], true, "{session}");
    assert!(
        second.log().contains("Restored session"),
        "{}",
        second.log()
    );

    // The restored in-memory session still knows its branch: a command run
    // after the restart pushes the clone to it.
    let record = second
        .run_command(&id, "exec prompt", &["make an edit"])
        .await;
    assert_eq!(record["status"], "done", "{record}");
    let pushed = &record["result"]["pushed"];
    assert_eq!(pushed["branch"], "restored", "{record}");
    assert_eq!(
        origin.branch_sha("restored").as_deref(),
        pushed["sha"].as_str(),
        "{record}"
    );
}

/// The WI 0133 request rules surface as 400s on the live HTTP API. Needs no
/// git: every request is refused before a clone is attempted.
#[tokio::test(flavor = "multi_thread")]
async fn real_network_session_request_validation_over_http_returns_400() {
    let home = ApiHome::new();
    let server = home.start().await;
    let url = "git://127.0.0.1:9/origin.git";
    for (body, expected) in [
        (
            json!({"session_type": "local", "workdir": "/nonexistent", "push": true}),
            "push is only valid when session_type is 'remote'",
        ),
        (
            json!({"session_type": "local", "workdir": "/nonexistent", "base_ref": "main"}),
            "base_ref is only valid when session_type is 'remote'",
        ),
        (
            json!({"session_type": "remote", "repo_url": url, "push": true}),
            "push requires branch on a remote session",
        ),
        (
            json!({"session_type": "remote", "repo_url": url, "merge_refs": ["a"]}),
            "merge_refs requires branch on a remote session",
        ),
        (
            json!({"session_type": "remote", "repo_url": url, "branch": "w",
                   "merge_refs": ["origin/a"]}),
            "merge_refs 'origin/a' is not a valid git ref name",
        ),
        (
            json!({"session_type": "remote", "repo_url": url, "branch": "w",
                   "merge_refs": ["refs/heads/a"]}),
            "merge_refs 'refs/heads/a' is not a valid git ref name",
        ),
        (
            json!({"session_type": "remote", "repo_url": "file:///srv/origin.git",
                   "branch": "w", "push": true}),
            "scheme",
        ),
    ] {
        let (status, response) = server.post_session(body.clone()).await;
        assert_eq!(status, 400, "{body} → {response}");
        let error = response["error"].as_str().unwrap_or_default();
        assert!(error.contains(expected), "{body} → {error}");
    }
    let sessions = server.get("/v1/sessions").await;
    assert_eq!(sessions["sessions"].as_array().map(Vec::len), Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_tag_only_merge_refs_and_reserved_or_sha_names_are_http_400() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    origin.tag("v1", "main");
    let home = ApiHome::new();
    let server = home.start().await;
    for name in [
        "v1",
        "HEAD",
        "abc1234",
        "ABCDEF0123456789ABCDEF0123456789ABCDEF01",
    ] {
        let (code, body) = server
            .post_session(json!({"session_type":"remote", "repo_url":origin.url,
            "branch":"work", "merge_refs":[name]}))
            .await;
        assert_eq!(code, 400, "{name}: {body}");
        assert!(body.to_string().contains("merge_refs"), "{body}");
    }
}
