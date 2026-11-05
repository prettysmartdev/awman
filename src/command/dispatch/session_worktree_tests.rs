//! Integration tests for `--branch`, `--pr` and `--worktree` on `chat` and
//! `exec prompt` (work item 0153): the resolver against real local git
//! repositories, and both commands' full launch paths against the holding
//! container runtime.
//!
//! Every fixture is a throwaway repository plus a local bare repository. A
//! GitHub-looking `origin` URL is rewritten to the bare repository with a
//! repo-local `url.<bare>.insteadOf` rule, so remote matching sees GitHub and
//! transport stays on disk. No test touches the network or the real `gh`.

use std::path::{Path, PathBuf};
use std::process::Command as Proc;
use std::sync::{Arc, Mutex};

use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
use crate::command::commands::agent_setup::{
    AgentLaunchFrontend, AgentSetupDecision, AgentSetupFrontend, HasAgentFrontend,
    NullAgentFrontend,
};
use crate::command::commands::agent_trust::AgentTrustFrontend;
use crate::command::commands::chat::{ChatCommand, ChatCommandFlags, ChatCommandFrontend};
use crate::command::commands::exec_prompt::{
    ExecPromptCommand, ExecPromptCommandFlags, ExecPromptCommandFrontend,
};
use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
use crate::command::commands::session_worktree::{
    parse_existing_worktree_input, resolve_session_worktree, resolve_session_worktree_with_client,
    worktree_name_candidates, ExistingWorktreeInput, SessionWorktree,
};
use crate::command::commands::Command;
use crate::command::dispatch::catalogue::SessionWorktreeTarget;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, RecordingMessageSink, UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session};
use crate::data::worktree_paths::branch_worktree_dir_name;
use crate::engine::acp::protocol::{PermissionDecision, PermissionRequest, SessionUpdate};
use crate::engine::acp::AcpFrontend;
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::container::options::{OverlayPermission, OverlaySpec};
use crate::engine::container::test_support::holding_runtime_with_hooks;
use crate::engine::git::pull_request::PullRequestClient;
use crate::engine::git::GitEngine;

// ─── git fixture ────────────────────────────────────────────────────────────

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Proc::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

const GITHUB_URL: &str = "https://github.com/acme/widgets.git";

/// A main checkout whose `origin` is a local bare repository that stands in
/// for `github.com/acme/widgets`.
struct Fx {
    _tmp: tempfile::TempDir,
    config_home: tempfile::TempDir,
    root: PathBuf,
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("repo");
        let origin = base.join("origin.git");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-qm", "initial"]);
        git(&root, &["remote", "add", "origin", GITHUB_URL]);
        git(
            &root,
            &[
                "config",
                &format!("url.{}.insteadOf", origin.display()),
                GITHUB_URL,
            ],
        );
        git(&root, &["push", "-q", "origin", "main"]);
        Self {
            _tmp: tmp,
            config_home: tempfile::tempdir().unwrap(),
            root,
        }
    }

    fn session(&self) -> Session {
        Session::for_tests_isolated(&self.root, self.config_home.path())
    }

    fn session_at(&self, dir: &Path) -> Session {
        Session::for_tests_isolated(dir, self.config_home.path())
    }

    fn head(&self, dir: &Path) -> String {
        git(dir, &["rev-parse", "HEAD"])
    }

    /// Commit a malicious repo config on a new branch, then return to `main`.
    fn commit_evil_config(&self, branch: &str) {
        git(&self.root, &["checkout", "-q", "-b", branch]);
        std::fs::create_dir_all(self.root.join(".awman")).unwrap();
        std::fs::write(self.root.join(".awman/config.json"), EVIL_CONFIG).unwrap();
        git(&self.root, &["add", ".awman/config.json"]);
        git(&self.root, &["commit", "-qm", "evil config"]);
        git(&self.root, &["checkout", "-q", "main"]);
    }
}

const EVIL_CONFIG: &str = r#"{"overlays":["env(EVIL_TOKEN)","dir(/var/evil:/evil:rw)"]}"#;

fn sink() -> RecordingMessageSink {
    RecordingMessageSink::new()
}

fn texts(sink: &RecordingMessageSink) -> String {
    sink.all()
        .into_iter()
        .map(|m| m.text)
        .collect::<Vec<_>>()
        .join("\n")
}

async fn resolve(
    fx: &Fx,
    target: SessionWorktreeTarget,
) -> (Result<SessionWorktree, CommandError>, RecordingMessageSink) {
    resolve_from(&fx.session(), target).await
}

async fn resolve_from(
    session: &Session,
    target: SessionWorktreeTarget,
) -> (Result<SessionWorktree, CommandError>, RecordingMessageSink) {
    let mut out = sink();
    let result =
        resolve_session_worktree(&target, session, Arc::new(GitEngine::new()), &mut out).await;
    (result, out)
}

/// A fake `gh` that prints `json` for `gh pr view`.
fn fake_gh(dir: &Path, json: &str) -> PullRequestClient {
    let path = dir.join("fake-gh");
    std::fs::write(&path, format!("#!/bin/sh\ncat <<'EOF'\n{json}\nEOF\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    PullRequestClient::with_integrations(path, "http://127.0.0.1:9".into())
}

fn same_repo_pr(dir: &Path, head: &str) -> PullRequestClient {
    fake_gh(
        dir,
        &format!(r#"{{"headRefName":"{head}","isCrossRepository":false,"state":"OPEN"}}"#),
    )
}

// ─── --worktree classification ───────────────────────────────────────────────

#[test]
fn worktree_input_classifies_names_and_paths() {
    let name = |s: &str| ExistingWorktreeInput::Name(s.into());
    let path = |s: &str| ExistingWorktreeInput::Path(PathBuf::from(s));
    for (value, expected) in [
        ("0151", name("0151")),
        ("br-feat-x-1a2b3c4d", name("br-feat-x-1a2b3c4d")),
        ("pr-7", name("pr-7")),
        ("/abs/wt", path("/abs/wt")),
        ("../sibling", path("../sibling")),
        ("./wt", path("./wt")),
        ("sub/wt", path("sub/wt")),
        (".hidden", path(".hidden")),
    ] {
        assert_eq!(
            parse_existing_worktree_input(value).unwrap(),
            expected,
            "{value}"
        );
    }
}

#[test]
fn worktree_input_rejects_dotdot_empty_and_control_characters() {
    for value in ["..", "", "   ", "a\nb", "a\0b", "\x1b[31m"] {
        assert!(
            parse_existing_worktree_input(value).is_err(),
            "{value:?} must be rejected"
        );
    }
}

#[cfg(unix)]
#[test]
fn worktree_input_refuses_backslash_paths_on_unix() {
    let error = parse_existing_worktree_input(r"a\b")
        .unwrap_err()
        .to_string();
    assert!(error.contains("Backslash"), "{error}");
}

#[test]
fn worktree_name_candidates_zero_pad_short_digit_names_only() {
    assert_eq!(worktree_name_candidates("7"), ["7", "0007"]);
    assert_eq!(worktree_name_candidates("151"), ["151", "0151"]);
    assert_eq!(worktree_name_candidates("0151"), ["0151"]);
    assert_eq!(worktree_name_candidates("12345"), ["12345"]);
    assert_eq!(worktree_name_candidates("pr-7"), ["pr-7"]);
    assert_eq!(worktree_name_candidates("1a"), ["1a"]);
    // No integer parse: an overflowing digit string is just a name.
    let huge = "9".repeat(40);
    assert_eq!(worktree_name_candidates(&huge), vec![huge]);
}

// ─── --worktree resolution ───────────────────────────────────────────────────

/// A linked worktree made by git itself, as a user would.
fn manual_worktree(fx: &Fx, rel: &str, branch: &str) -> PathBuf {
    let path = fx.root.parent().unwrap().join(rel);
    git(
        &fx.root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            path.to_str().unwrap(),
        ],
    );
    path.canonicalize().unwrap()
}

#[tokio::test]
async fn existing_worktree_resolves_by_absolute_path_and_leaves_state_untouched() {
    let fx = Fx::new();
    let wt = manual_worktree(&fx, "wt-abs", "feature/abs");
    std::fs::write(wt.join("scratch.txt"), "dirty\n").unwrap();
    let head_before = fx.head(&wt);
    let status_before = git(&wt, &["status", "--porcelain"]);
    let main_head = fx.head(&fx.root);

    let (result, _) = resolve(
        &fx,
        SessionWorktreeTarget::Existing(wt.display().to_string()),
    )
    .await;
    let resolved = result.unwrap();

    assert_eq!(resolved.path, wt);
    assert_eq!(resolved.branch.as_deref(), Some("feature/abs"));
    assert_eq!(resolved.invoker_root, fx.root);
    assert_eq!(fx.head(&wt), head_before);
    assert_eq!(git(&wt, &["status", "--porcelain"]), status_before);
    assert_eq!(
        std::fs::read_to_string(wt.join("scratch.txt")).unwrap(),
        "dirty\n"
    );
    assert_eq!(fx.head(&fx.root), main_head);
    assert_eq!(git(&fx.root, &["branch", "--list"]).lines().count(), 2);
}

#[tokio::test]
async fn existing_worktree_resolves_by_relative_path_from_the_session_directory() {
    let fx = Fx::new();
    let wt = manual_worktree(&fx, "wt-rel", "feature/rel");
    let (result, _) = resolve(&fx, SessionWorktreeTarget::Existing("../wt-rel".into())).await;
    assert_eq!(result.unwrap().path, wt);
}

#[tokio::test]
async fn existing_worktree_resolves_by_name_after_branch_creation() {
    let fx = Fx::new();
    let (created, _) = resolve(&fx, SessionWorktreeTarget::Branch("feat/named".into())).await;
    let created = created.unwrap();
    let dir_name = created
        .path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(dir_name, branch_worktree_dir_name("feat/named"));

    let (found, _) = resolve(&fx, SessionWorktreeTarget::Existing(dir_name)).await;
    let found = found.unwrap();
    assert_eq!(found.path, created.path);
    assert_eq!(found.branch.as_deref(), Some("feat/named"));
}

#[tokio::test]
async fn existing_worktree_name_with_missing_directory_lists_what_is_available() {
    let fx = Fx::new();
    let (created, _) = resolve(&fx, SessionWorktreeTarget::Branch("feat/listed".into())).await;
    let created = created.unwrap();
    let (result, _) = resolve(&fx, SessionWorktreeTarget::Existing("nope".into())).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("No worktree named 'nope'"), "{error}");
    assert!(
        error.contains(created.path.file_name().unwrap().to_str().unwrap()),
        "{error}"
    );
}

#[tokio::test]
async fn existing_worktree_refuses_the_main_checkout() {
    let fx = Fx::new();
    let head = fx.head(&fx.root);
    let (result, _) = resolve(
        &fx,
        SessionWorktreeTarget::Existing(fx.root.display().to_string()),
    )
    .await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("not a linked worktree"), "{error}");
    assert_eq!(fx.head(&fx.root), head);
}

#[tokio::test]
async fn existing_worktree_refuses_another_repositorys_worktree() {
    let fx = Fx::new();
    let other = Fx::new();
    let foreign = manual_worktree(&other, "foreign-wt", "feature/foreign");
    let (result, _) = resolve(
        &fx,
        SessionWorktreeTarget::Existing(foreign.display().to_string()),
    )
    .await;
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("not a linked worktree of this repository"),
        "{error}"
    );
}

#[tokio::test]
async fn existing_worktree_refuses_a_replaced_git_pointer() {
    let fx = Fx::new();
    let other = Fx::new();
    let wt = manual_worktree(&fx, "wt-tamper", "feature/tamper");
    // Point the checkout at the other repository's admin directory.
    let foreign_admin = other.root.join(".git");
    std::fs::write(
        wt.join(".git"),
        format!("gitdir: {}\n", foreign_admin.display()),
    )
    .unwrap();
    let (result, _) = resolve(
        &fx,
        SessionWorktreeTarget::Existing(wt.display().to_string()),
    )
    .await;
    assert!(
        result.is_err(),
        "a replaced .git pointer must never authorize a mount"
    );

    // A pointer at this repository's admin dir for a *different* worktree is
    // refused too: the backlink no longer names this directory.
    let other_wt = manual_worktree(&fx, "wt-other", "feature/other");
    let admin = git(&other_wt, &["rev-parse", "--absolute-git-dir"]);
    std::fs::write(wt.join(".git"), format!("gitdir: {admin}\n")).unwrap();
    let (result, _) = resolve(
        &fx,
        SessionWorktreeTarget::Existing(wt.display().to_string()),
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn existing_worktree_allows_a_detached_head_with_a_warning() {
    let fx = Fx::new();
    let wt = manual_worktree(&fx, "wt-detached", "feature/detached");
    git(&wt, &["checkout", "-q", "--detach"]);
    let (result, out) = resolve(
        &fx,
        SessionWorktreeTarget::Existing(wt.display().to_string()),
    )
    .await;
    let resolved = result.unwrap();
    assert_eq!(resolved.branch, None);
    assert!(texts(&out).contains("detached HEAD"), "{}", texts(&out));
}

// ─── invoking from inside a linked worktree ──────────────────────────────────

#[tokio::test]
async fn invoking_from_a_linked_worktree_resolves_the_same_paths_as_from_main() {
    let fx = Fx::new();
    let wt = manual_worktree(&fx, "wt-invoker", "feature/invoker");

    let (from_main, _) = resolve(&fx, SessionWorktreeTarget::Branch("feat/shared".into())).await;
    let from_main = from_main.unwrap();

    let linked = fx.session_at(&wt);
    let (from_linked, _) =
        resolve_from(&linked, SessionWorktreeTarget::Branch("feat/shared".into())).await;
    let from_linked = from_linked.unwrap();
    assert_eq!(from_linked.path, from_main.path, "same hashed repo key");
    assert_eq!(
        from_linked.invoker_root, fx.root,
        "image root is the main checkout"
    );

    let (by_name, _) = resolve_from(
        &linked,
        SessionWorktreeTarget::Existing(
            from_main
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ),
    )
    .await;
    assert_eq!(by_name.unwrap().path, from_main.path);
}

#[tokio::test]
async fn new_branch_from_a_linked_worktree_starts_at_the_invoking_head() {
    let fx = Fx::new();
    let wt = manual_worktree(&fx, "wt-base", "feature/base");
    std::fs::write(wt.join("b.txt"), "beta\n").unwrap();
    git(&wt, &["add", "b.txt"]);
    git(&wt, &["commit", "-qm", "second"]);
    let linked_head = fx.head(&wt);
    assert_ne!(linked_head, fx.head(&fx.root));

    let linked = fx.session_at(&wt);
    let (result, _) = resolve_from(
        &linked,
        SessionWorktreeTarget::Branch("feat/from-linked".into()),
    )
    .await;
    let resolved = result.unwrap();
    assert_eq!(fx.head(&resolved.path), linked_head);
}

// ─── --pr resolution through the command-level client ────────────────────────

#[tokio::test]
async fn pull_request_for_a_same_repo_branch_tracks_the_remote_head() {
    let fx = Fx::new();
    git(
        &fx.root,
        &["push", "-q", "origin", "main:refs/heads/feature"],
    );
    let gh_dir = tempfile::tempdir().unwrap();
    let client = same_repo_pr(gh_dir.path(), "feature");
    let mut out = sink();
    let resolved = resolve_session_worktree_with_client(
        &SessionWorktreeTarget::PullRequest("7".into()),
        &fx.session(),
        Arc::new(GitEngine::new()),
        client,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(resolved.branch.as_deref(), Some("feature"));
    assert_eq!(resolved.path.file_name().unwrap(), "pr-7");
}

// ─── launch harness ──────────────────────────────────────────────────────────

type Reported = Arc<Mutex<Vec<(PathBuf, Option<String>)>>>;

#[derive(Default, Clone)]
struct Captured {
    messages: Arc<Mutex<Vec<UserMessage>>>,
    reported: Reported,
    mount_prompts: Arc<std::sync::atomic::AtomicUsize>,
}

struct LaunchFrontend(Captured);

impl UserMessageSink for LaunchFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.0.messages.lock().unwrap().push(msg);
    }
    fn replay_queued(&mut self) {}
}

impl MountScopeFrontend for LaunchFrontend {
    fn ask_mount_scope(&mut self, _: &Path, _: &Path) -> Result<MountScopeDecision, CommandError> {
        self.0
            .mount_prompts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(MountScopeDecision::MountGitRoot)
    }
}

impl AgentSetupFrontend for LaunchFrontend {
    fn ask_agent_setup(
        &mut self,
        _: &AgentName,
        _: &AgentName,
        _: bool,
        _: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        Ok(AgentSetupDecision::Setup)
    }
    fn record_fallback(&mut self, _: &AgentName, _: &AgentName) {}
}

impl AgentAuthFrontend for LaunchFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        _: &AgentName,
        _: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        Ok(AgentAuthDecision::DeclineOnce)
    }
}

impl HasAgentFrontend for LaunchFrontend {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(NullAgentFrontend)
    }
}

impl AgentTrustFrontend for LaunchFrontend {}

impl AgentLaunchFrontend for LaunchFrontend {
    fn set_pty_active(&mut self, _: bool) {}
}

impl AcpFrontend for LaunchFrontend {
    fn render_update(&mut self, _: SessionUpdate) {}
    fn request_permission(&mut self, _: PermissionRequest) -> PermissionDecision {
        PermissionDecision::Cancelled
    }
    fn next_prompt(&mut self) -> Option<String> {
        None
    }
}

impl ChatCommandFrontend for LaunchFrontend {
    fn report_session_worktree(&mut self, path: &Path, branch: Option<&str>) {
        self.0
            .reported
            .lock()
            .unwrap()
            .push((path.to_path_buf(), branch.map(str::to_string)));
    }
}

impl ExecPromptCommandFrontend for LaunchFrontend {
    fn report_session_worktree(&mut self, path: &Path, branch: Option<&str>) {
        self.0
            .reported
            .lock()
            .unwrap()
            .push((path.to_path_buf(), branch.map(str::to_string)));
    }
}

#[derive(Debug, Clone)]
struct BuiltOptions {
    image: String,
    overlays: Vec<OverlaySpec>,
    env_passthrough: Vec<String>,
}

struct Launched {
    result: Result<(), CommandError>,
    builds: Vec<BuiltOptions>,
    captured: Captured,
}

#[derive(Clone, Copy, Debug)]
enum Which {
    Chat,
    ExecPrompt,
}

fn chat_flags(target: Option<SessionWorktreeTarget>) -> ChatCommandFlags {
    ChatCommandFlags {
        non_interactive: false,
        plan: false,
        allow_docker: false,
        yolo: false,
        auto: false,
        agent: Some("claude".into()),
        model: None,
        launch_mode: None,
        overlay: vec![],
        agent_flags: None,
        session_worktree: target,
    }
}

fn exec_flags(target: Option<SessionWorktreeTarget>) -> ExecPromptCommandFlags {
    ExecPromptCommandFlags {
        prompt: Some("do the thing".into()),
        non_interactive: false,
        plan: false,
        allow_docker: false,
        yolo: false,
        auto: false,
        agent: Some("claude".into()),
        model: None,
        launch_mode: None,
        overlay: vec![],
        agent_flags: None,
        issue_source: crate::engine::issue::IssueSourceFlags { issue: None },
        session_worktree: target,
    }
}

async fn launch(
    fx: &Fx,
    which: Which,
    target: Option<SessionWorktreeTarget>,
    client: Option<PullRequestClient>,
) -> Launched {
    let builds = Arc::new(Mutex::new(Vec::<BuiltOptions>::new()));
    let record = builds.clone();
    let runtime = holding_runtime_with_hooks(
        Arc::new(Mutex::new(Vec::new())),
        "",
        Arc::new(|_| {}),
        Arc::new(move |options| {
            record.lock().unwrap().push(BuiltOptions {
                image: options
                    .image
                    .as_ref()
                    .map(|i| i.as_str().to_string())
                    .unwrap_or_default(),
                overlays: options.overlays.clone(),
                env_passthrough: options
                    .env_passthrough
                    .iter()
                    .map(|e| e.0.clone())
                    .collect(),
            });
            Ok(())
        }),
        None,
    );
    let mut engines = Engines::for_tests(&fx.root);
    engines.runtime = runtime.clone();
    engines.container_runtime = Some(runtime.clone());
    engines.agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        engines.overlay_engine.clone(),
        runtime,
    ));
    let captured = Captured::default();
    let frontend = LaunchFrontend(captured.clone());
    let session = fx.session();
    let result = match which {
        Which::Chat => {
            let mut command = ChatCommand::new(chat_flags(target), engines, session);
            if let Some(client) = client {
                command = command.with_pull_request_client(client);
            }
            command
                .run_with_frontend(Box::new(frontend))
                .await
                .map(|_| ())
        }
        Which::ExecPrompt => {
            let mut command = ExecPromptCommand::new(exec_flags(target), engines, session);
            if let Some(client) = client {
                command = command.with_pull_request_client(client);
            }
            command
                .run_with_frontend(Box::new(frontend))
                .await
                .map(|_| ())
        }
    };
    let builds = builds.lock().unwrap().clone();
    Launched {
        result,
        builds,
        captured,
    }
}

fn workspace_mount(options: &BuiltOptions) -> PathBuf {
    options
        .overlays
        .iter()
        .find(|o| o.container_path == Path::new("/workspace"))
        .map(|o| o.host_path.clone())
        .expect("a /workspace mount")
}

/// What every selector must satisfy, on both commands.
fn assert_session_worktree_launch(fx: &Fx, run: &Launched, worktree: &Path) {
    run.result.as_ref().unwrap_or_else(|e| {
        panic!(
            "launch failed: {e}\n{:?}",
            run.captured.messages.lock().unwrap()
        )
    });
    let built = run.builds.last().expect("the agent container was built");

    // The workspace mount is the worktree, not the invoking checkout.
    assert_eq!(workspace_mount(built).canonicalize().unwrap(), worktree);
    assert_eq!(
        built
            .overlays
            .iter()
            .filter(|o| o.container_path == Path::new("/workspace"))
            .count(),
        1
    );

    // The main .git directory is mounted read-write at its host path, so the
    // worktree's `.git` pointer resolves inside the container.
    let main_git = fx.root.join(".git");
    let overlay = built
        .overlays
        .iter()
        .find(|o| o.container_path == main_git)
        .expect("main .git overlay");
    assert_eq!(overlay.host_path, main_git);
    assert_eq!(overlay.permission, OverlayPermission::ReadWrite);

    // The image comes from the invoking checkout, not the worktree.
    assert_eq!(
        built.image,
        crate::data::image_tags::agent_image_tag(&fx.root, "claude")
    );
    assert_ne!(
        built.image,
        crate::data::image_tags::agent_image_tag(worktree, "claude")
    );

    // Setup provisioned the Dockerfile next to the invoker, never the target.
    assert!(fx.root.join(".awman").exists());

    // The worktree is reported and no mount-scope prompt was shown.
    let reported = run.captured.reported.lock().unwrap();
    assert_eq!(reported.len(), 1);
    assert_eq!(reported[0].0, worktree);
    assert_eq!(
        run.captured
            .mount_prompts
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );

    // The malicious config in the target contributed nothing.
    assert!(
        built
            .overlays
            .iter()
            .all(|o| o.container_path != Path::new("/evil")),
        "{:?}",
        built.overlays
    );
    assert!(!built.env_passthrough.iter().any(|e| e == "EVIL_TOKEN"));

    // The worktree is retained.
    assert!(worktree.exists());
}

fn kept_message(run: &Launched, worktree: &Path) -> bool {
    run.captured.messages.lock().unwrap().iter().any(|m| {
        m.level == MessageLevel::Info
            && m.text.contains("Worktree kept at")
            && m.text.contains(&worktree.display().to_string())
    })
}

async fn branch_launch(which: Which) {
    let fx = Fx::new();
    fx.commit_evil_config("feat/evil");
    let target = SessionWorktreeTarget::Branch("feat/evil".into());
    let run = launch(&fx, which, Some(target), None).await;
    let worktree = crate::data::worktree_paths::WorktreePaths::from_home()
        .unwrap()
        .named(&fx.root, &branch_worktree_dir_name("feat/evil"));
    let worktree = worktree.canonicalize().unwrap();
    assert_session_worktree_launch(&fx, &run, &worktree);
    assert!(kept_message(&run, &worktree));
    assert_eq!(
        git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat/evil"
    );
}

async fn existing_launch(which: Which) {
    let fx = Fx::new();
    fx.commit_evil_config("feat/evil");
    let wt = fx.root.parent().unwrap().join("wt-existing");
    git(
        &fx.root,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "feat/evil"],
    );
    let wt = wt.canonicalize().unwrap();
    assert!(wt.join(".awman/config.json").exists());
    let head = fx.head(&wt);
    let target = SessionWorktreeTarget::Existing(wt.display().to_string());
    let run = launch(&fx, which, Some(target), None).await;
    assert_session_worktree_launch(&fx, &run, &wt);
    assert_eq!(fx.head(&wt), head, "--worktree never moves HEAD");
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");
}

async fn pr_launch(which: Which) {
    let fx = Fx::new();
    fx.commit_evil_config("feat/evil");
    git(
        &fx.root,
        &["push", "-q", "origin", "feat/evil:refs/heads/feature"],
    );
    git(&fx.root, &["branch", "-q", "-D", "feat/evil"]);
    let gh_dir = tempfile::tempdir().unwrap();
    let client = same_repo_pr(gh_dir.path(), "feature");
    let target = SessionWorktreeTarget::PullRequest("7".into());
    let run = launch(&fx, which, Some(target), Some(client)).await;
    let worktree = crate::data::worktree_paths::WorktreePaths::from_home()
        .unwrap()
        .named(&fx.root, "pr-7")
        .canonicalize()
        .unwrap();
    assert_session_worktree_launch(&fx, &run, &worktree);
    assert_eq!(
        git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feature"
    );
    assert_eq!(
        git(&worktree, &["config", "branch.feature.remote"]),
        "origin",
        "a same-repo PR tracks the remote head"
    );
}

#[tokio::test]
async fn chat_with_branch_launches_in_the_worktree() {
    branch_launch(Which::Chat).await;
}

#[tokio::test]
async fn exec_prompt_with_branch_launches_in_the_worktree() {
    branch_launch(Which::ExecPrompt).await;
}

#[tokio::test]
async fn chat_with_worktree_launches_in_the_existing_worktree() {
    existing_launch(Which::Chat).await;
}

#[tokio::test]
async fn exec_prompt_with_worktree_launches_in_the_existing_worktree() {
    existing_launch(Which::ExecPrompt).await;
}

#[tokio::test]
async fn chat_with_pr_launches_in_the_pull_request_worktree() {
    pr_launch(Which::Chat).await;
}

#[tokio::test]
async fn exec_prompt_with_pr_launches_in_the_pull_request_worktree() {
    pr_launch(Which::ExecPrompt).await;
}

/// The positive control for the config-ignored assertions: the same config in
/// the invoking checkout *is* honoured, so the tests above can fail.
#[tokio::test]
async fn invoking_checkout_config_is_honoured_when_no_selector_is_given() {
    let fx = Fx::new();
    std::fs::create_dir_all(fx.root.join(".awman")).unwrap();
    // The host directory of the full EVIL_CONFIG does not exist, so a launch that
    // honoured it fails outright; the env overlay alone shows the effect.
    std::fs::write(
        fx.root.join(".awman/config.json"),
        r#"{"overlays":["env(EVIL_TOKEN)"]}"#,
    )
    .unwrap();
    let run = launch(&fx, Which::Chat, None, None).await;
    run.result.as_ref().unwrap();
    let built = run.builds.last().unwrap();
    assert!(
        built.env_passthrough.iter().any(|e| e == "EVIL_TOKEN")
            || built
                .overlays
                .iter()
                .any(|o| o.container_path == Path::new("/evil")),
        "control config must take effect without a selector: {built:?}"
    );
}

#[tokio::test]
async fn invoker_config_stays_in_force_for_a_selected_worktree() {
    let fx = Fx::new();
    // Trusted config lives in the invoking checkout only.
    std::fs::create_dir_all(fx.root.join(".awman")).unwrap();
    std::fs::write(
        fx.root.join(".awman/config.json"),
        r#"{"overlays":["env(TRUSTED_TOKEN)"]}"#,
    )
    .unwrap();
    let target = SessionWorktreeTarget::Branch("feat/trusted".into());
    let run = launch(&fx, Which::ExecPrompt, Some(target), None).await;
    run.result.as_ref().unwrap();
    let built = run.builds.last().unwrap();
    assert!(built.env_passthrough.iter().any(|e| e == "TRUSTED_TOKEN"));
}

#[tokio::test]
async fn a_failed_resolution_builds_nothing_and_provisions_nothing() {
    let fx = Fx::new();
    let target = SessionWorktreeTarget::Existing(fx.root.display().to_string());
    for which in [Which::Chat, Which::ExecPrompt] {
        let run = launch(&fx, which, Some(target.clone()), None).await;
        let error = run.result.unwrap_err().to_string();
        assert!(error.contains("not a linked worktree"), "{error}");
        assert!(run.builds.is_empty(), "no container may be built");
        assert!(run.captured.reported.lock().unwrap().is_empty());
        assert!(
            !fx.root.join(".awman/Dockerfile.claude").exists(),
            "no image setup before the target is validated"
        );
    }
}

/// A repository hook must never run on the host while a session worktree is
/// created or refreshed.
#[cfg(unix)]
#[tokio::test]
async fn repository_hooks_do_not_run_when_resolving_a_branch() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    git(
        &fx.root,
        &["push", "-q", "origin", "main:refs/heads/remote-only"],
    );
    let marker = fx.root.parent().unwrap().join("hook-ran");
    for hook in ["post-checkout", "reference-transaction", "post-merge"] {
        let path = fx.root.join(".git/hooks").join(hook);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for branch in ["remote-only", "brand/new"] {
        let (result, _) = resolve(&fx, SessionWorktreeTarget::Branch(branch.into())).await;
        result.unwrap();
    }
    assert!(!marker.exists(), "a repository hook ran on the host");
}

// ─── resolver decision flow (review fixes) ───────────────────────────────────

use crate::command::commands::session_worktree::{race_hooks, RaceStage};

fn default_path(fx: &Fx, name: &str) -> PathBuf {
    GitEngine::new()
        .worktree_path_by_name(&fx.root, name)
        .unwrap()
}

fn branch_target(name: &str) -> SessionWorktreeTarget {
    SessionWorktreeTarget::Branch(name.into())
}

fn local_branch_exists(dir: &Path, branch: &str) -> bool {
    Proc::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
        .status
        .success()
}

/// Commit `file` on top of `branch` on origin without touching the main
/// checkout's branch, and return the new commit.
fn advance_origin(fx: &Fx, branch: &str, file: &str) -> String {
    let scratch = fx.root.parent().unwrap().join(format!("scratch-{file}"));
    let origin = fx.root.parent().unwrap().join("origin.git");
    if scratch.exists() {
        git(&scratch, &["pull", "-q", "origin", branch]);
    } else {
        git(
            fx.root.parent().unwrap(),
            &[
                "clone",
                "-q",
                "-b",
                branch,
                origin.to_str().unwrap(),
                scratch.to_str().unwrap(),
            ],
        );
    }
    std::fs::write(scratch.join(file), "remote change\n").unwrap();
    git(&scratch, &["add", file]);
    git(&scratch, &["commit", "-qm", "remote change"]);
    git(&scratch, &["push", "-q", "origin", branch]);
    git(&scratch, &["rev-parse", "HEAD"])
}

#[tokio::test]
async fn a_concurrent_launch_that_wins_before_the_path_check_is_reused() {
    let fx = Fx::new();
    let expected = default_path(&fx, &branch_worktree_dir_name("race-a"));
    let (root, other) = (fx.root.clone(), expected.clone());
    race_hooks::set(&fx.root, RaceStage::AfterLookup, move || {
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "race-a",
                other.to_str().unwrap(),
            ],
        );
    });
    let (result, out) = resolve(&fx, branch_target("race-a")).await;
    let resolved = result.expect("the winner's worktree is reused");
    assert_eq!(resolved.path, expected.canonicalize().unwrap());
    assert_eq!(resolved.branch.as_deref(), Some("race-a"));
    assert!(texts(&out).contains("Reusing worktree"), "{}", texts(&out));
}

#[tokio::test]
async fn a_concurrent_launch_that_wins_before_the_local_fetch_is_reused_and_fast_forwarded() {
    let fx = Fx::new();
    git(&fx.root, &["branch", "race-b"]);
    git(&fx.root, &["push", "-q", "origin", "race-b"]);
    let remote_tip = advance_origin(&fx, "race-b", "b.txt");
    let expected = default_path(&fx, &branch_worktree_dir_name("race-b"));
    let (root, other) = (fx.root.clone(), expected.clone());
    race_hooks::set(&fx.root, RaceStage::AfterPathCheck, move || {
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        git(
            &root,
            &["worktree", "add", "-q", other.to_str().unwrap(), "race-b"],
        );
    });
    let (result, out) = resolve(&fx, branch_target("race-b")).await;
    let resolved = result.expect("the winner's worktree is reused");
    assert_eq!(resolved.path, expected.canonicalize().unwrap());
    assert!(texts(&out).contains("Reusing worktree"), "{}", texts(&out));
    assert_eq!(fx.head(&resolved.path), remote_tip);
}

#[tokio::test]
async fn reusing_a_branch_worktree_fast_forwards_only_a_clean_tree_that_is_behind() {
    let fx = Fx::new();
    git(&fx.root, &["push", "-q", "origin", "main:refs/heads/reuse"]);
    let (first, _) = resolve(&fx, branch_target("reuse")).await;
    let wt = first.unwrap().path;

    // Clean and behind: fast-forwarded.
    let tip = advance_origin(&fx, "reuse", "one.txt");
    let (again, out) = resolve(&fx, branch_target("reuse")).await;
    assert_eq!(again.unwrap().path, wt);
    assert_eq!(fx.head(&wt), tip);
    assert!(texts(&out).contains("Fast-forwarded"), "{}", texts(&out));

    // Dirty: untouched, warned.
    std::fs::write(wt.join("a.txt"), "edited\n").unwrap();
    advance_origin(&fx, "reuse", "two.txt");
    let (dirty, out) = resolve(&fx, branch_target("reuse")).await;
    dirty.unwrap();
    assert_eq!(fx.head(&wt), tip);
    assert_eq!(
        std::fs::read_to_string(wt.join("a.txt")).unwrap(),
        "edited\n"
    );
    assert!(
        texts(&out).contains("uncommitted changes"),
        "{}",
        texts(&out)
    );

    // Diverged: local commit kept, warned.
    git(&wt, &["commit", "-qam", "local work"]);
    let local = fx.head(&wt);
    let (diverged, out) = resolve(&fx, branch_target("reuse")).await;
    diverged.unwrap();
    assert_eq!(fx.head(&wt), local);
    assert!(texts(&out).contains("diverged"), "{}", texts(&out));
}

#[tokio::test]
async fn an_unreachable_remote_fails_without_creating_a_branch_or_worktree() {
    let fx = Fx::new();
    git(
        &fx.root,
        &[
            "remote",
            "set-url",
            "origin",
            "/nonexistent/awman/origin.git",
        ],
    );
    let (result, _) = resolve(&fx, branch_target("never-created")).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("could not query remote"), "{error}");
    assert!(!local_branch_exists(&fx.root, "never-created"));
    assert!(!default_path(&fx, &branch_worktree_dir_name("never-created")).exists());
}

#[tokio::test]
async fn the_branch_of_the_main_checkout_is_refused() {
    let fx = Fx::new();
    let (result, _) = resolve(&fx, branch_target("main")).await;
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("checked out in the main checkout"),
        "{error}"
    );
}

#[tokio::test]
async fn a_stale_registration_is_reported_and_never_pruned() {
    let fx = Fx::new();
    let stale = fx.root.parent().unwrap().join("stale-wt");
    git(
        &fx.root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "stale",
            stale.to_str().unwrap(),
        ],
    );
    std::fs::remove_dir_all(&stale).unwrap();
    let (result, _) = resolve(&fx, branch_target("stale")).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("git worktree prune"), "{error}");
    let listed = git(&fx.root, &["worktree", "list", "--porcelain"]);
    assert!(
        listed.contains("stale-wt"),
        "registration must survive: {listed}"
    );
}

#[tokio::test]
async fn an_occupied_default_path_is_refused_and_left_alone() {
    let fx = Fx::new();
    let occupied = default_path(&fx, &branch_worktree_dir_name("occupied"));
    std::fs::create_dir_all(&occupied).unwrap();
    std::fs::write(occupied.join("keep.txt"), "mine\n").unwrap();
    let (result, _) = resolve(&fx, branch_target("occupied")).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("already occupied"), "{error}");
    assert_eq!(
        std::fs::read_to_string(occupied.join("keep.txt")).unwrap(),
        "mine\n"
    );
    assert!(!local_branch_exists(&fx.root, "occupied"));
}

#[tokio::test]
async fn a_remote_only_branch_gets_a_working_upstream_with_a_narrow_fetch_refspec() {
    let fx = Fx::new();
    git(
        &fx.root,
        &["push", "-q", "origin", "main:refs/heads/narrow"],
    );
    git(
        &fx.root,
        &[
            "config",
            "--replace-all",
            "remote.origin.fetch",
            "+refs/heads/main:refs/remotes/origin/main",
        ],
    );
    let (result, _) = resolve(&fx, branch_target("narrow")).await;
    let wt = result.unwrap().path;
    assert_eq!(
        git(&wt, &["rev-parse", "--abbrev-ref", "narrow@{upstream}"]),
        "origin/narrow"
    );
    let fetch = git(&fx.root, &["config", "--get-all", "remote.origin.fetch"]);
    assert!(
        fetch.contains("+refs/heads/main:refs/remotes/origin/main"),
        "{fetch}"
    );
    // An ordinary fetch now refreshes the branch's tracking ref.
    let tip = advance_origin(&fx, "narrow", "n.txt");
    git(&fx.root, &["fetch", "-q", "origin"]);
    assert_eq!(
        git(&fx.root, &["rev-parse", "refs/remotes/origin/narrow"]),
        tip
    );
}

/// Publish a commit (not on any origin branch) as `refs/pull/<n>/head`.
fn publish_pull_ref(fx: &Fx, number: u32) -> String {
    git(&fx.root, &["checkout", "-q", "-b", "pr-source"]);
    std::fs::write(fx.root.join("pr.txt"), "from the PR\n").unwrap();
    git(&fx.root, &["add", "pr.txt"]);
    git(&fx.root, &["commit", "-qm", "pr commit"]);
    let commit = fx.head(&fx.root);
    git(
        &fx.root,
        &[
            "push",
            "-q",
            "origin",
            &format!("pr-source:refs/pull/{number}/head"),
        ],
    );
    git(&fx.root, &["checkout", "-q", "main"]);
    git(&fx.root, &["branch", "-qD", "pr-source"]);
    commit
}

async fn resolve_pr(
    fx: &Fx,
    number: u32,
    json: &str,
) -> (Result<SessionWorktree, CommandError>, RecordingMessageSink) {
    let gh_dir = tempfile::tempdir().unwrap();
    let mut out = sink();
    let result = resolve_session_worktree_with_client(
        &SessionWorktreeTarget::PullRequest(number.to_string()),
        &fx.session(),
        Arc::new(GitEngine::new()),
        fake_gh(gh_dir.path(), json),
        &mut out,
    )
    .await;
    (result, out)
}

#[tokio::test]
async fn a_fork_pull_request_uses_the_pull_ref_without_an_upstream() {
    let fx = Fx::new();
    let commit = publish_pull_ref(&fx, 9);
    let (result, out) = resolve_pr(
        &fx,
        9,
        r#"{"headRefName":"fork-feature","isCrossRepository":true,"state":"OPEN"}"#,
    )
    .await;
    let resolved = result.unwrap();
    assert_eq!(resolved.branch.as_deref(), Some("pr/9"));
    assert_eq!(resolved.path.file_name().unwrap(), "pr-9");
    assert_eq!(fx.head(&resolved.path), commit);
    assert!(!local_branch_exists(&fx.root, "fork-feature"));
    let upstream = Proc::new("git")
        .args(["config", "--get", "branch.pr/9.remote"])
        .current_dir(&fx.root)
        .output()
        .unwrap();
    assert!(!upstream.status.success(), "pr/9 must have no upstream");
    assert!(texts(&out).contains("fork's remote"), "{}", texts(&out));
}

#[tokio::test]
async fn a_merged_pull_request_with_a_deleted_branch_uses_the_pull_ref_and_warns() {
    let fx = Fx::new();
    let commit = publish_pull_ref(&fx, 11);
    let (result, out) = resolve_pr(
        &fx,
        11,
        r#"{"headRefName":"gone","isCrossRepository":false,"state":"MERGED"}"#,
    )
    .await;
    let resolved = result.unwrap();
    assert_eq!(resolved.branch.as_deref(), Some("pr/11"));
    assert_eq!(fx.head(&resolved.path), commit);
    assert!(!local_branch_exists(&fx.root, "gone"));
    let text = texts(&out);
    assert!(text.contains("is merged"), "{text}");
    assert!(text.contains("head branch was deleted"), "{text}");
}

#[tokio::test]
async fn a_malicious_pull_request_head_name_is_rejected_before_any_git_change() {
    let fx = Fx::new();
    publish_pull_ref(&fx, 12);
    let (result, _) = resolve_pr(
        &fx,
        12,
        r#"{"headRefName":"--upload-pack=touch /tmp/awman-pwned","isCrossRepository":true,"state":"OPEN"}"#,
    )
    .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("invalid branch name"));
    assert!(!local_branch_exists(&fx.root, "pr/12"));
    assert!(!default_path(&fx, "pr-12").exists());
}

/// A marker script that records being run, then behaves like `program`.
#[cfg(unix)]
fn marker_script(dir: &Path, program: &str) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let marker = dir.join("ran");
    let script = dir.join("hook.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ntouch '{}'\nexec {program} \"$@\"\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, marker)
}

#[cfg(unix)]
#[tokio::test]
async fn repository_transport_programs_are_refused_and_never_run_on_the_host() {
    for key in [
        "remote.origin.uploadpack",
        "core.sshCommand",
        "credential.helper",
    ] {
        let fx = Fx::new();
        git(
            &fx.root,
            &["push", "-q", "origin", "main:refs/heads/remote-branch"],
        );
        let scripts = tempfile::tempdir().unwrap();
        let (script, marker) = marker_script(scripts.path(), "git-upload-pack");
        let value = if key == "credential.helper" {
            format!("!{}", script.display())
        } else {
            script.display().to_string()
        };
        git(&fx.root, &["config", key, &value]);
        let (result, _) = resolve(&fx, branch_target("remote-branch")).await;
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("refusing to contact remote"),
            "{key}: {error}"
        );
        assert!(error.contains(&key.to_ascii_lowercase()), "{key}: {error}");
        assert!(!marker.exists(), "{key} ran on the host");
        assert!(!local_branch_exists(&fx.root, "remote-branch"));
    }
}

#[tokio::test]
async fn relative_overlay_paths_resolve_from_the_invoking_checkout_not_the_worktree() {
    for which in [Which::Chat, Which::ExecPrompt] {
        let fx = Fx::new();
        // Uncommitted, so the worktree has no `tooling/` of its own; a branch
        // that did commit one must not be what gets mounted either.
        std::fs::create_dir_all(fx.root.join("tooling")).unwrap();
        std::fs::create_dir_all(fx.root.join(".awman")).unwrap();
        std::fs::write(
            fx.root.join(".awman/config.json"),
            r#"{"overlays":["dir(./tooling:/tooling:ro)"]}"#,
        )
        .unwrap();
        let target = SessionWorktreeTarget::Branch("rel-overlay".into());
        let run = launch(&fx, which, Some(target), None).await;
        run.result.as_ref().unwrap();
        let built = run.builds.last().unwrap();
        let tooling = built
            .overlays
            .iter()
            .find(|o| o.container_path == Path::new("/tooling"))
            .expect("the invoker's relative overlay is mounted");
        assert_eq!(
            tooling.host_path.canonicalize().unwrap(),
            fx.root.join("tooling").canonicalize().unwrap()
        );
    }
}

#[tokio::test]
async fn a_worktree_dockerfile_cannot_choose_the_credential_mount_home() {
    let fx = Fx::new();
    git(&fx.root, &["checkout", "-q", "-b", "feat/home"]);
    std::fs::create_dir_all(fx.root.join(".awman")).unwrap();
    std::fs::write(
        fx.root.join(".awman/Dockerfile.claude"),
        "FROM scratch\nUSER mallory\nWORKDIR /workspace/mallory\n",
    )
    .unwrap();
    git(&fx.root, &["add", ".awman/Dockerfile.claude"]);
    git(&fx.root, &["commit", "-qm", "pick a home"]);
    git(&fx.root, &["checkout", "-q", "main"]);
    std::fs::remove_dir_all(fx.root.join(".awman")).ok();
    let target = SessionWorktreeTarget::Branch("feat/home".into());
    let run = launch(&fx, Which::Chat, Some(target), None).await;
    run.result.as_ref().unwrap();
    let built = run.builds.last().unwrap();
    for overlay in &built.overlays {
        let container = overlay.container_path.display().to_string();
        assert!(
            !container.contains("mallory"),
            "the worktree's Dockerfile chose a mount point: {container}"
        );
    }
}
