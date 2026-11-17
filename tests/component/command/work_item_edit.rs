//! Creating and editing work items (WI 0147): `new spec` and `edit spec --amend`
//! against local files and a GitHub source, through the production commands,
//! resolver, provider and runtime. The agent is a fake `docker run` that holds
//! until the test releases it; GitHub is a fake `gh`. Nothing reaches a real
//! tracker, container or host agent.
use crate::support;
use awman::command::commands::edit::{
    EditCommand, EditCommandFrontend, EditCommonFlags, EditSpecFlags, EditSubcommand,
};
use awman::command::commands::new::{
    NewCommand, NewCommandFrontend, NewOutcome, NewSpecFlags, NewSubcommand,
};
use awman::command::commands::prompt_templates::edit::{
    render_edit_prompt, EditLocation, EditPromptRequest, EditSubject,
};
use awman::command::commands::prompt_templates::render_interview_prompt;
use awman::command::commands::{
    agent_auth::*, agent_setup::*, agent_trust::AgentTrustFrontend, mount_scope::*, Command,
};
use awman::command::dispatch::Engines;
use awman::command::error::CommandError;
use awman::command::work_item::spec_service::NewSpecFrontend;
use awman::command::work_item::WorkItemError;
use awman::data::message::{MessageLevel, UserMessage, UserMessageSink};
use awman::data::session::{AgentName, Session, SessionOpenOptions};
use awman::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use awman_test_support::fakes::FakeCli;
use awman_test_support::fixtures::{ConfigTree, RepoBuilder};
use awman_test_support::TestSandbox;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ISSUE_URL: &str = "https://github.com/Acme/Widgets/issues/73";
const TEMPLATE: &str = "# Work Item: {{kind}}\n\nTitle: x\nNumber: [{{number}}]\n\n- summary\n";
const LIMIT: Duration = Duration::from_secs(15);
/// A title that makes the test frontend dismiss the title question.
const ABORT: &str = "<dismissed>";

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Message(MessageLevel, String),
    Pty(bool),
}

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<Event>>>);
impl Log {
    fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }
    fn messages(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Message(_, text) => Some(text),
                Event::Pty(_) => None,
            })
            .collect()
    }
    fn all_text(&self) -> String {
        self.messages().join("\n")
    }
    /// The draft's host path from the pre-launch message.
    fn draft_path(&self) -> Option<PathBuf> {
        self.messages()
            .iter()
            .find_map(|m| m.strip_prefix("Work item draft: ").map(PathBuf::from))
    }
}

/// A command frontend that records messages and terminal ownership, answers
/// the Q&A with fixed values, and gives the agent piped (non-PTY) I/O.
struct Front {
    log: Log,
    title: String,
}
impl UserMessageSink for Front {
    fn write_message(&mut self, msg: UserMessage) {
        self.log
            .0
            .lock()
            .unwrap()
            .push(Event::Message(msg.level, msg.text));
    }
    fn replay_queued(&mut self) {}
}
impl MountScopeFrontend for Front {
    fn ask_mount_scope(&mut self, _: &Path, _: &Path) -> Result<MountScopeDecision, CommandError> {
        Ok(MountScopeDecision::MountGitRoot)
    }
}
impl AgentSetupFrontend for Front {
    fn ask_agent_setup(
        &mut self,
        _: &AgentName,
        _: &AgentName,
        _: bool,
        _: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        Ok(AgentSetupDecision::Abort)
    }
    fn record_fallback(&mut self, _: &AgentName, _: &AgentName) {}
}
impl AgentAuthFrontend for Front {
    fn ask_agent_auth_consent(
        &mut self,
        _: &AgentName,
        _: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        Ok(AgentAuthDecision::DeclineOnce)
    }
}
impl HasAgentFrontend for Front {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(Piped)
    }
}
impl AgentTrustFrontend for Front {}
impl AgentLaunchFrontend for Front {
    fn set_pty_active(&mut self, active: bool) {
        self.log.0.lock().unwrap().push(Event::Pty(active));
    }
}
impl EditCommandFrontend for Front {}
impl NewSpecFrontend for Front {
    fn ask_spec_title(&mut self) -> Result<String, CommandError> {
        // A dismissed dialog (Ctrl+C) surfaces as `Aborted`.
        if self.title == ABORT {
            return Err(CommandError::Aborted);
        }
        Ok(self.title.clone())
    }
    fn ask_spec_summary(&mut self) -> Result<String, CommandError> {
        Ok("the summary".into())
    }
}
impl NewCommandFrontend for Front {
    fn ask_workflow_step_name(&mut self) -> Result<String, CommandError> {
        Ok("one".into())
    }
    fn ask_workflow_step_prompt(&mut self) -> Result<String, CommandError> {
        Ok("prompt".into())
    }
}

/// Headless agent I/O: output is discarded, stdin stays open.
struct Piped;
impl UserMessageSink for Piped {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}
#[async_trait::async_trait]
impl AgentFrontend for Piped {
    fn report_status(&mut self, _: AgentStatus) {}
    fn report_progress(&mut self, _: AgentProgress) {}
    fn take_io(&mut self) -> AgentIo {
        let (stdout, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stderr, mut err_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move { while out_rx.recv().await.is_some() {} });
        tokio::spawn(async move { while err_rx.recv().await.is_some() {} });
        AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
    fn grace_timeout(&self) -> Duration {
        Duration::from_secs(600)
    }
}

struct World {
    sandbox: TestSandbox,
    repo: PathBuf,
    session: Session,
    engines: Engines,
    docker: FakeCli,
    gh: FakeCli,
}

impl World {
    fn front(&self, title: &str) -> (Box<Front>, Log) {
        let log = Log::default();
        (
            Box::new(Front {
                log: log.clone(),
                title: title.into(),
            }),
            log,
        )
    }

    fn new_spec(&self, interview: bool, work_item: Option<&str>) -> NewCommand {
        NewCommand::new(
            NewSubcommand::Spec(NewSpecFlags {
                interview,
                non_interactive: true,
                work_item: work_item.map(str::to_string),
            }),
            self.engines.clone(),
            self.session.clone(),
        )
    }

    fn amend(&self, work_item: &str) -> EditCommand {
        EditCommand::new(
            EditSubcommand::Spec(EditSpecFlags {
                common: EditCommonFlags {
                    reference: work_item.into(),
                    interview: false,
                    amend: true,
                    headless: true,
                    prompt: None,
                    json: false,
                    allow_docker: false,
                    yolo: false,
                    auto: false,
                    agent: None,
                    model: None,
                    overlays: vec![],
                    agent_flags: None,
                },
            }),
            self.engines.clone(),
            self.session.clone(),
        )
    }

    fn work_item_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.repo.join("aspec/work-items"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn gh_calls(&self, sub: &str) -> Vec<awman_test_support::fakes::Call> {
        self.gh
            .calls()
            .into_iter()
            .filter(|c| c.argv.get(1).map(String::as_str) == Some(sub))
            .collect()
    }

    fn docker_runs(&self) -> Vec<awman_test_support::fakes::Call> {
        self.docker
            .calls()
            .into_iter()
            .filter(|c| c.argv.first().map(String::as_str) == Some("run"))
            .collect()
    }

    /// Drafts left in the repo context directory.
    fn drafts(&self) -> Vec<PathBuf> {
        walk(&self.sandbox.awman_home().join("context"))
            .into_iter()
            .filter(|p| {
                p.parent()
                    .is_some_and(|d| d.ends_with("scratch/work-items"))
            })
            .collect()
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// A checkout with a work item template, `origin` on GitHub, the given repo
/// config, a fake `gh` and a fake Docker whose `run` holds until released.
fn world(id: &'static str, repo_config: serde_json::Value, files: &[(&str, &str)]) -> World {
    world_in(support::sandbox(id), repo_config, files, json!({}))
}

/// `world` over a caller-owned sandbox and extra global config, so a test can
/// start its own owned loopback tracker first.
fn world_in(
    mut sandbox: TestSandbox,
    repo_config: serde_json::Value,
    files: &[(&str, &str)],
    global_extra: serde_json::Value,
) -> World {
    let mut builder = RepoBuilder::new(&sandbox, "repo")
        .file("aspec/work-items/0000-template.md", TEMPLATE)
        .file(
            "aspec/work-items/0003-existing.md",
            "# Existing\n\nlocal 3\n",
        );
    for (path, content) in files {
        builder = builder.file(path, content);
    }
    let repo = builder.init();
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/Acme/Widgets.git",
    ]);
    let root = repo.path().to_path_buf();
    let mut global =
        json!({"runtime":{"kind":"docker"},"agents":{"auth":{"refresh":{"enabled":false}}}});
    if let Some(extra) = global_extra.as_object() {
        for (key, value) in extra {
            global[key] = value.clone();
        }
    }
    ConfigTree::new()
        .global(global)
        .repo(repo_config)
        .write(&sandbox, Some(&root));
    let docker = sandbox.install_fake(support::fake_cli_exe(), "docker");
    // Anything the runtime asks besides `run` succeeds quietly.
    docker.on(&[]);
    awman_test_support::fakes::docker_cli_baseline(&docker);
    docker
        .on(&["image", "inspect"])
        .stdout("[{\"Config\":{\"Env\":[\"HOME=/home/agent\"]}}]\n");
    docker.on(&["run"]).hold_until_released();
    let gh = sandbox.install_fake(support::fake_cli_exe(), "gh");
    gh.on(&["issue", "create"])
        .read_stdin()
        .stdout(&format!("{ISSUE_URL}\n"));
    gh.on(&["issue", "edit"]).read_stdin();
    gh.on(&["issue", "view"]).stdout(&issue_json("raw body\n"));
    // Host CLI isolation admits a fake only inside the snapshot's temp root;
    // the sandbox's fake binaries sit beside its scratch directory.
    let temp_root = sandbox.root().display().to_string();
    for key in ["TMPDIR", "TMP", "TEMP"] {
        sandbox.env_mut().set(key, &temp_root);
    }
    let env = support::env_snapshot(&sandbox);
    let session = Session::open_at_git_root(
        root.clone(),
        root.clone(),
        SessionOpenOptions {
            env: Some(env.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    // The bundle resolves docker from the process PATH; launch through the
    // sandbox's fake instead, with the production agent engine.
    let mut engines = Engines::for_env(env.clone()).unwrap();
    let docker_runtime = Arc::new(awman::engine::container::ContainerRuntime::docker_in(env));
    engines.runtime = docker_runtime.clone();
    engines.container_runtime = Some(docker_runtime.clone());
    engines.agent_engine = Arc::new(awman::engine::agent::AgentEngine::new(
        engines.overlay_engine.clone(),
        docker_runtime,
    ));
    World {
        sandbox,
        repo: root,
        session,
        engines,
        docker,
        gh,
    }
}

fn issue_json(body: &str) -> String {
    json!({"number": 73, "title": "Widget title", "body": body, "url": ISSUE_URL}).to_string()
}

fn github() -> serde_json::Value {
    json!({"workItems": {"source": "github"}})
}

/// Wait (off the async runtime) until the fake agent is running.
async fn agent_running<T: std::fmt::Debug>(
    docker: &FakeCli,
    task: &mut tokio::task::JoinHandle<T>,
) {
    let calls = docker.clone();
    let docker = docker.clone();
    let held = tokio::task::spawn_blocking(move || docker.wait_until_held(LIMIT));
    tokio::select! {
        held = held => { held.unwrap(); }
        done = task => panic!("the command finished before the agent ran: {done:?}; docker calls: {:#?}", calls.calls()),
    }
}

async fn finish<T: Send + 'static>(task: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(LIMIT, task)
        .await
        .expect("command finished")
        .unwrap()
}

/// The pre-launch draft message precedes terminal hand-off to the agent.
fn assert_path_before_launch(log: &Log) {
    let events = log.events();
    let path = events
        .iter()
        .position(|e| matches!(e, Event::Message(_, t) if t.starts_with("Work item draft: ")))
        .expect("draft path message");
    let pty = events
        .iter()
        .position(|e| *e == Event::Pty(true))
        .expect("agent launched");
    assert!(path < pty, "{events:#?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn github_new_spec_creates_an_issue_without_a_file_or_number() {
    let w = world(
        "command::work_item_edit::github_new_spec_creates_an_issue_without_a_file_or_number",
        github(),
        &[],
    );
    let before = w.work_item_files();
    let (front, log) = w.front("-dash title");
    let outcome = w
        .new_spec(false, None)
        .run_with_frontend(front)
        .await
        .unwrap();
    let NewOutcome::Spec(outcome) = outcome else {
        panic!("expected spec")
    };
    assert_eq!(outcome.path, None);
    assert_eq!(outcome.url.as_deref(), Some(ISSUE_URL));
    assert_eq!(outcome.number, Some(73));
    assert_eq!(w.work_item_files(), before, "no local file or number");
    let creates = w.gh_calls("create");
    assert_eq!(creates.len(), 1);
    assert!(
        creates[0].argv.contains(&"--title=-dash title".to_string()),
        "{:?}",
        creates[0].argv
    );
    let body = String::from_utf8(creates[0].stdin.clone()).unwrap();
    assert!(body.contains("Number: []"), "{body}");
    assert!(body.contains("- the summary"), "{body}");
    assert!(log.all_text().contains(ISSUE_URL), "{}", log.all_text());
    assert!(w.docker_runs().is_empty(), "no agent without an interview");
    // The destination is named before anything is published.
    assert!(
        log.all_text()
            .contains("creating a github work item in github:acme/widgets"),
        "{}",
        log.all_text()
    );
    assert!(w.drafts().is_empty(), "the recovery draft is removed");
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn interview_creates_the_issue_from_the_draft_even_after_a_failed_agent() {
    let w = world(
        "command::work_item_edit::interview_creates_the_issue_from_the_draft_even_after_a_failed_agent",
        github(),
        &[],
    );
    w.docker.on(&["run"]).hold_until_released().exit(3);
    let before = w.work_item_files();
    let (front, log) = w.front("Widget title");
    let command = w.new_spec(true, None);
    let mut task = tokio::spawn(async move { command.run_with_frontend(front).await });
    agent_running(&w.docker, &mut task).await;
    assert_path_before_launch(&log);
    let draft = log.draft_path().expect("draft path");
    // The resolver canonicalizes the home, including Windows' verbatim prefix.
    let repo_context = std::fs::canonicalize(w.sandbox.awman_home().join("context/repo")).unwrap();
    assert!(draft.starts_with(&repo_context), "{draft:?}");
    let seeded = std::fs::read_to_string(&draft).unwrap();
    assert!(seeded.contains("Number: []"), "{seeded}");
    // The agent sees the draft through a read-write context(repo) mount.
    let run = &w.docker_runs()[0];
    let repo_dir = draft.parent().unwrap().parent().unwrap().parent().unwrap();
    let mount = format!("{}:/awman/context/repo", repo_dir.display());
    assert!(
        run.argv
            .iter()
            .any(|a| *a == mount || *a == format!("{mount}:rw")),
        "{:?}",
        run.argv
    );
    assert!(
        w.gh_calls("create").is_empty(),
        "nothing is created before an edit"
    );
    std::fs::write(&draft, "# Widget\n\nwritten by the agent\n").unwrap();
    w.docker.release();
    let result = finish(task).await;
    assert!(result.is_err(), "a failed agent still fails the command");
    let creates = w.gh_calls("create");
    assert_eq!(creates.len(), 1, "exactly one create");
    assert!(creates[0]
        .argv
        .contains(&"--title=Widget title".to_string()));
    assert_eq!(creates[0].stdin, b"# Widget\n\nwritten by the agent\n");
    assert!(log.all_text().contains(ISSUE_URL), "{}", log.all_text());
    assert!(
        !draft.exists(),
        "the draft is removed after the final upload"
    );
    assert_eq!(w.work_item_files(), before);
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn amend_uploads_the_raw_body_once_and_removes_the_draft() {
    let w = world(
        "command::work_item_edit::amend_uploads_the_raw_body_once_and_removes_the_draft",
        github(),
        &[],
    );
    let (front, log) = w.front("unused");
    let command = w.amend("73");
    let mut task = tokio::spawn(async move { command.run_with_frontend(front).await });
    agent_running(&w.docker, &mut task).await;
    assert_path_before_launch(&log);
    assert!(
        log.all_text().contains(ISSUE_URL),
        "URL printed before launch"
    );
    let draft = log.draft_path().unwrap();
    // Seeded from the raw body: no `# <title>` heading is added.
    assert_eq!(std::fs::read_to_string(&draft).unwrap(), "raw body\n");
    std::fs::write(
        &draft,
        "raw body\n\n## Agent implementation notes\n\ndone\n",
    )
    .unwrap();
    w.docker.release();
    finish(task).await.unwrap();
    let edits = w.gh_calls("edit");
    assert_eq!(edits.len(), 1, "{edits:?}");
    let body = String::from_utf8(edits[0].stdin.clone()).unwrap();
    assert_eq!(body, "raw body\n\n## Agent implementation notes\n\ndone\n");
    assert!(!body.starts_with("# "), "no title heading is uploaded");
    assert!(!draft.exists());
    assert!(w.drafts().is_empty());
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn amend_with_an_unchanged_draft_makes_no_upload() {
    let w = world(
        "command::work_item_edit::amend_with_an_unchanged_draft_makes_no_upload",
        github(),
        &[],
    );
    let (front, log) = w.front("unused");
    let command = w.amend("github:73");
    let mut task = tokio::spawn(async move { command.run_with_frontend(front).await });
    agent_running(&w.docker, &mut task).await;
    let draft = log.draft_path().unwrap();
    w.docker.release();
    finish(task).await.unwrap();
    assert!(w.gh_calls("edit").is_empty());
    assert!(!draft.exists());
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn amend_never_overwrites_a_concurrent_remote_change() {
    let w = world(
        "command::work_item_edit::amend_never_overwrites_a_concurrent_remote_change",
        github(),
        &[],
    );
    let (front, log) = w.front("unused");
    let command = w.amend("73");
    let mut task = tokio::spawn(async move { command.run_with_frontend(front).await });
    agent_running(&w.docker, &mut task).await;
    let draft = log.draft_path().unwrap();
    // Someone edits the issue on the tracker while the agent runs.
    w.gh.on(&["issue", "view"])
        .stdout(&issue_json("edited on the tracker\n"));
    std::fs::write(&draft, "agent edit\n").unwrap();
    w.docker.release();
    let error = finish(task).await.unwrap_err();
    assert!(error.to_string().contains("RemoteConflict"), "{error}");
    assert!(w.gh_calls("edit").is_empty(), "the remote change is kept");
    assert!(draft.exists(), "the draft is kept for recovery");
    assert!(
        log.all_text().contains(&draft.display().to_string()),
        "{}",
        log.all_text()
    );
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_repo_context_stops_before_launch() {
    let mut config = github();
    config["overlays"] = json!(["context(repo:ro)"]);
    let w = world(
        "command::work_item_edit::read_only_repo_context_stops_before_launch",
        config,
        &[],
    );
    let (front, log) = w.front("unused");
    let error = w.amend("73").run_with_frontend(front).await.unwrap_err();
    assert!(
        matches!(
            error,
            CommandError::WorkItem(WorkItemError::ReadOnlyContext)
        ),
        "{error}"
    );
    assert!(w.docker_runs().is_empty());
    assert!(!log.events().contains(&Event::Pty(true)));
    assert!(w.drafts().is_empty());
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn ambiguous_local_amend_lists_every_match_before_launch() {
    let w = world(
        "command::work_item_edit::ambiguous_local_amend_lists_every_match_before_launch",
        json!({}),
        &[
            ("aspec/work-items/0042-first.md", "# First\n"),
            ("aspec/work-items/0042-second.md", "# Second\n"),
        ],
    );
    let (front, log) = w.front("unused");
    let error = w.amend("0042").run_with_frontend(front).await.unwrap_err();
    assert!(
        matches!(
            &error,
            CommandError::WorkItem(WorkItemError::AmbiguousWorkItem { number: 42, paths }) if paths.len() == 2
        ),
        "{error}"
    );
    let text = log.all_text();
    assert!(
        text.contains("0042-first.md") && text.contains("0042-second.md"),
        "{text}"
    );
    assert!(w.docker_runs().is_empty());
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn seeding_from_the_configured_source_suggests_amend() {
    let w = world(
        "command::work_item_edit::seeding_from_the_configured_source_suggests_amend",
        github(),
        &[],
    );
    let (front, _log) = w.front("unused");
    let error = w
        .new_spec(false, Some("#73"))
        .run_with_frontend(front)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            CommandError::WorkItem(WorkItemError::AlreadyExists { .. })
        ),
        "{error}"
    );
    assert!(error.to_string().contains("edit spec"), "{error}");
    assert!(w.gh_calls("create").is_empty());
    assert!(w.docker_runs().is_empty());
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn local_new_spec_keeps_numbered_files() {
    let w = world(
        "command::work_item_edit::local_new_spec_keeps_numbered_files",
        json!({}),
        &[],
    );
    let (front, _log) = w.front("Local thing");
    let outcome = w
        .new_spec(false, None)
        .run_with_frontend(front)
        .await
        .unwrap();
    let NewOutcome::Spec(outcome) = outcome else {
        panic!("expected spec")
    };
    let path = PathBuf::from(outcome.path.unwrap());
    assert!(
        path.ends_with("aspec/work-items/0004-local-thing.md"),
        "{path:?}"
    );
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("Number: [0004]"));
    assert_eq!((outcome.url, outcome.number), (None, Some(4)));
    assert!(w.gh.calls().is_empty());
    w.sandbox.finish();
}

#[test]
fn prompts_name_the_file_and_never_expand_work_item_text() {
    let interview = render_interview_prompt(
        "/awman/context/repo/scratch/work-items/x.md",
        "Bug",
        "{summary}",
        "body with {path}",
    );
    assert!(interview.contains("/awman/context/repo/scratch/work-items/x.md"));
    assert!(interview.contains("\"{summary}\""), "{interview}");
    assert!(interview.ends_with("body with {path}"), "{interview}");
    assert!(interview.contains("Re-read the file"));
    let edit = render_edit_prompt(&EditPromptRequest {
        subject: EditSubject::Spec { title: "Title" },
        name: "",
        primary_path: "/workspace/aspec/work-items/0003-x.md",
        companion_paths: &[],
        bundle_dir: None,
        location: EditLocation::InPlace,
        global: false,
        interview: false,
        amend: true,
        headless: false,
        brief: None,
    });
    assert!(edit.contains("/workspace/aspec/work-items/0003-x.md"));
    assert!(edit.contains("\"Title\""));
    assert!(edit.contains("re-read"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dismissed_question_never_publishes_a_work_item() {
    let w = world(
        "command::work_item_edit::a_dismissed_question_never_publishes_a_work_item",
        github(),
        &[],
    );
    let before = w.work_item_files();
    for interview in [false, true] {
        let (front, _log) = w.front(ABORT);
        let result = w.new_spec(interview, None).run_with_frontend(front).await;
        assert!(
            matches!(result, Err(CommandError::Aborted)),
            "{:?}",
            result.err()
        );
    }
    assert!(w.gh_calls("create").is_empty(), "nothing was published");
    assert!(w.docker_runs().is_empty(), "no agent was started");
    assert!(w.drafts().is_empty());
    assert_eq!(w.work_item_files(), before);
    w.sandbox.finish();
}

// ---------------------------------------------------------------------------
// Existing remote issues (WI 0059): ordinary edit, interview and amend against
// the production GitHub provider (fake `gh`) and a configured HTTP provider
// (an owned loopback tracker). Nothing may ever create an issue.
// ---------------------------------------------------------------------------

use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, Respond, ResponseTemplate,
};

const HTTP_REF: &str = "http:tracker:PROJ-42";
const HTTP_URL_PATH: &str = "/items/PROJ-42";
const RAW: &str = "raw body\n";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Backend {
    Github,
    Http,
}
const BACKENDS: [Backend; 2] = [Backend::Github, Backend::Http];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Ordinary,
    Interview,
    Amend,
}
const MODES: [Mode; 3] = [Mode::Ordinary, Mode::Interview, Mode::Amend];

#[derive(Clone, Copy, Debug)]
enum Reject {
    Auth,
    Permission,
    RateLimit,
}
const REJECTIONS: [Reject; 3] = [Reject::Auth, Reject::Permission, Reject::RateLimit];

fn http_issue(body: &str) -> serde_json::Value {
    json!({"id": "PROJ-42", "number": 42, "title": "Tracker title", "body": body})
}

/// The tracker's PATCH echoes the uploaded body back with the same identity.
struct EchoPatch;
impl Respond for EchoPatch {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let sent: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        ResponseTemplate::new(200).set_body_json(http_issue(sent["body"].as_str().unwrap()))
    }
}

/// One checkout, a fake `gh`, a fake Docker and an owned loopback HTTP tracker
/// named `tracker`, with `RAW` as both issues' body.
struct Remote {
    w: World,
    tracker: MockServer,
}

#[derive(Debug, PartialEq)]
struct Write {
    verb: &'static str,
    body: String,
}

impl Remote {
    /// Stop the owned tracker and wait until its port is closed, then run the
    /// sandbox leak check (which must still see every other resource).
    async fn finish(self) {
        let Remote { w, tracker } = self;
        let addr = *tracker.address();
        drop(tracker);
        awman_test_support::wait_for_async("the tracker to stop", LIMIT, || async move {
            tokio::net::TcpStream::connect(addr)
                .await
                .is_err()
                .then_some(())
        })
        .await;
        w.sandbox.finish();
    }

    async fn new(id: &'static str, repo_config: serde_json::Value, files: &[(&str, &str)]) -> Self {
        let mut sandbox = support::sandbox(id);
        sandbox.redact("test-secret-tracker");
        sandbox
            .env_mut()
            .set("TRACKER_TOKEN", "test-secret-tracker");
        let tracker = MockServer::builder()
            .listener(sandbox.ports().tcp_listener())
            .start()
            .await;
        Mock::given(method("GET"))
            .and(path(HTTP_URL_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(http_issue(RAW)))
            .mount(&tracker)
            .await;
        Mock::given(method("PATCH"))
            .and(path(HTTP_URL_PATH))
            .respond_with(EchoPatch)
            .mount(&tracker)
            .await;
        let global = json!({"workItems": {"providers": {"tracker": {
            "baseUrl": format!("{}/items", tracker.uri()), "tokenEnv": "TRACKER_TOKEN"}}}});
        let w = world_in(sandbox, repo_config, files, global);
        Self { w, tracker }
    }

    fn reference(&self, backend: Backend) -> &'static str {
        match backend {
            Backend::Github => "github:73",
            Backend::Http => HTTP_REF,
        }
    }

    /// Every provider write so far, normalized across providers. Anything but
    /// an `edit` (create, close, reopen, labels, delete) is visible as its
    /// own verb, and a metadata-carrying PATCH would fail the key assertion.
    async fn writes(&self, backend: Backend) -> Vec<Write> {
        match backend {
            Backend::Github => self
                .w
                .gh
                .calls()
                .into_iter()
                .filter(|c| c.argv.get(1).map(String::as_str) != Some("view"))
                .map(|c| {
                    let verb = match c.argv[1].as_str() {
                        "edit" => "edit",
                        "create" => "create",
                        _ => "other",
                    };
                    if verb == "edit" {
                        // Body only: never a title, label, state or assignee.
                        assert_eq!(
                            c.argv,
                            [
                                "issue",
                                "edit",
                                "73",
                                "--repo",
                                "github.com/Acme/Widgets",
                                "--body-file",
                                "-"
                            ],
                            "{:?}",
                            c.argv
                        );
                    }
                    Write {
                        verb,
                        body: String::from_utf8(c.stdin).unwrap(),
                    }
                })
                .collect(),
            Backend::Http => self
                .tracker
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|r| r.method.as_str() != "GET")
                .map(|r| {
                    let verb = match r.method.as_str() {
                        "PATCH" => "edit",
                        "POST" => "create",
                        _ => "other",
                    };
                    assert_eq!(r.url.path(), HTTP_URL_PATH, "only the original item");
                    let sent: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
                    let keys: Vec<_> = sent.as_object().unwrap().keys().cloned().collect();
                    assert_eq!(keys, ["body"], "only the body changes: {sent}");
                    Write {
                        verb,
                        body: sent["body"].as_str().unwrap().to_string(),
                    }
                })
                .collect(),
        }
    }

    async fn reads(&self, backend: Backend) -> usize {
        match backend {
            Backend::Github => self.w.gh_calls("view").len(),
            Backend::Http => self
                .tracker
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.method.as_str() == "GET")
                .count(),
        }
    }

    /// The tracker's current body changes behind the command's back.
    async fn change_remote(&self, backend: Backend, body: &str) {
        match backend {
            Backend::Github => {
                self.w.gh.on(&["issue", "view"]).stdout(&issue_json(body));
            }
            Backend::Http => {
                Mock::given(method("GET"))
                    .and(path(HTTP_URL_PATH))
                    .respond_with(ResponseTemplate::new(200).set_body_json(http_issue(body)))
                    .with_priority(1)
                    .mount(&self.tracker)
                    .await;
            }
        }
    }

    /// Reads fail the same way every provider reports them.
    async fn reject_reads(&self, backend: Backend, reject: Reject) {
        match backend {
            Backend::Github => {
                self.w
                    .gh
                    .on(&["issue", "view"])
                    .stderr(gh_stderr(reject))
                    .exit(1);
            }
            Backend::Http => {
                Mock::given(method("GET"))
                    .and(path(HTTP_URL_PATH))
                    .respond_with(ResponseTemplate::new(http_status(reject)))
                    .with_priority(1)
                    .mount(&self.tracker)
                    .await;
            }
        }
    }

    async fn reject_updates(&self, backend: Backend, reject: Reject) {
        match backend {
            Backend::Github => {
                self.w
                    .gh
                    .on(&["issue", "edit"])
                    .read_stdin()
                    .stderr(gh_stderr(reject))
                    .exit(1);
            }
            Backend::Http => {
                Mock::given(method("PATCH"))
                    .and(path(HTTP_URL_PATH))
                    .respond_with(ResponseTemplate::new(http_status(reject)))
                    .with_priority(1)
                    .mount(&self.tracker)
                    .await;
            }
        }
    }

    fn command(&self, reference: &str, mode: Mode) -> EditCommand {
        let (interview, amend, headless, prompt) = match mode {
            Mode::Ordinary => (false, false, true, Some("tighten the wording".to_string())),
            Mode::Interview => (true, false, false, None),
            Mode::Amend => (false, true, true, None),
        };
        EditCommand::new(
            EditSubcommand::Spec(EditSpecFlags {
                common: EditCommonFlags {
                    reference: reference.into(),
                    interview,
                    amend,
                    headless,
                    prompt,
                    json: false,
                    allow_docker: false,
                    yolo: false,
                    auto: false,
                    agent: None,
                    model: None,
                    overlays: vec![],
                    agent_flags: None,
                },
            }),
            self.w.engines.clone(),
            self.w.session.clone(),
        )
    }

    /// Start the command and wait until its (held) agent runs.
    async fn start(
        &self,
        reference: &str,
        mode: Mode,
    ) -> (
        tokio::task::JoinHandle<Result<awman::command::commands::edit::EditOutcome, CommandError>>,
        Log,
    ) {
        let (front, log) = self.w.front("unused");
        let command = self.command(reference, mode);
        let mut task = tokio::spawn(async move { command.run_with_frontend(front).await });
        agent_running(&self.w.docker, &mut task).await;
        (task, log)
    }
}

fn gh_stderr(reject: Reject) -> &'static str {
    match reject {
        Reject::Auth => "To get started with GitHub CLI, please run: gh auth login\n",
        Reject::Permission => "HTTP 403: Resource not accessible by integration\n",
        Reject::RateLimit => "HTTP 429: API rate limit exceeded\n",
    }
}

fn http_status(reject: Reject) -> u16 {
    match reject {
        Reject::Auth => 401,
        Reject::Permission => 403,
        Reject::RateLimit => 429,
    }
}

fn local_files(w: &World) -> Vec<(String, String)> {
    w.work_item_files()
        .into_iter()
        .map(|name| {
            let body =
                std::fs::read_to_string(w.repo.join("aspec/work-items").join(&name)).unwrap();
            (name, body)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn every_edit_mode_updates_the_original_issue_and_never_creates_one() {
    for backend in BACKENDS {
        for mode in MODES {
            let r = Remote::new(
                "command::work_item_edit::every_edit_mode_updates_the_original_issue_and_never_creates_one",
                json!({}),
                &[],
            )
            .await;
            let local_before = local_files(&r.w);
            let (task, log) = r.start(r.reference(backend), mode).await;
            let draft = log
                .draft_path()
                .unwrap_or_else(|| panic!("{backend:?} {mode:?}"));
            assert_path_before_launch(&log);
            // The raw body, not a rendered document with a title heading.
            assert_eq!(
                std::fs::read_to_string(&draft).unwrap(),
                RAW,
                "{backend:?} {mode:?}"
            );
            let edited = format!("{RAW}\n## {mode:?}\n\nmore\n");
            std::fs::write(&draft, &edited).unwrap();
            r.w.docker.release();
            let outcome = finish(task)
                .await
                .unwrap_or_else(|e| panic!("{backend:?} {mode:?}: {e}"));
            assert_eq!(
                r.writes(backend).await,
                [Write {
                    verb: "edit",
                    body: edited
                }],
                "{backend:?} {mode:?}"
            );
            assert!(
                outcome
                    .changed
                    .iter()
                    .any(|c| c.contains("PROJ-42") || c.contains("issues/73")),
                "{:?}",
                outcome.changed
            );
            assert_eq!(outcome.amend, mode == Mode::Amend);
            assert_eq!(outcome.interview, mode == Mode::Interview);
            // The other provider and the local tree were never touched.
            let other = if backend == Backend::Github {
                Backend::Http
            } else {
                Backend::Github
            };
            assert_eq!(r.reads(other).await, 0, "{backend:?} {mode:?}");
            assert!(r.writes(other).await.is_empty());
            assert_eq!(local_files(&r.w), local_before);
            assert!(
                !draft.exists() && r.w.drafts().is_empty(),
                "draft cleaned up"
            );
            // The agent is pointed at the container draft path, never a host
            // path. (Headless agents read the prompt from stdin, so check the
            // target the prompt is rendered from, and that argv has no host path.)
            assert!(
                outcome.target.files[0]
                    .container_path
                    .starts_with("/awman/context/repo/scratch/work-items/"),
                "{:?}",
                outcome.target.files
            );
            let run = &r.w.docker_runs()[0];
            let host_draft = draft.display().to_string();
            assert!(
                !run.argv.iter().any(|a| a.contains(&host_draft)),
                "{:?}",
                run.argv
            );
            r.finish().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_references_override_the_configured_source_without_probing_others() {
    // A local item 73 exists too: nothing may fall back to it or away from it.
    let files = [(
        "aspec/work-items/0073-local.md",
        "# Local 73\n\nlocal body\n",
    )];
    for source in ["local", "github", "http:tracker"] {
        for (reference, owner) in [
            ("github:73", Some(Backend::Github)),
            ("Acme/Widgets#73", Some(Backend::Github)),
            (HTTP_REF, Some(Backend::Http)),
            ("local:73", None),
        ] {
            let r = Remote::new(
                "command::work_item_edit::explicit_references_override_the_configured_source_without_probing_others",
                json!({"workItems": {"source": source}}),
                &files,
            )
            .await;
            let local_before = local_files(&r.w);
            let (task, log) = r.start(reference, Mode::Amend).await;
            let local =
                std::fs::canonicalize(r.w.repo.join("aspec/work-items/0073-local.md")).unwrap();
            match owner {
                Some(_) => {
                    let draft = log.draft_path().expect("remote edits use a draft");
                    std::fs::write(&draft, "edited remotely\n").unwrap();
                }
                None => {
                    assert!(log.draft_path().is_none(), "{source} {reference}");
                    // The local file is edited in place by the agent.
                    std::fs::write(&local, "# Local 73\n\nedited locally\n").unwrap();
                }
            }
            r.w.docker.release();
            finish(task)
                .await
                .unwrap_or_else(|e| panic!("{source} {reference}: {e}"));
            for backend in BACKENDS {
                let writes = r.writes(backend).await;
                if owner == Some(backend) {
                    assert_eq!(writes.len(), 1, "{source} {reference}");
                    assert_eq!(writes[0].body, "edited remotely\n");
                    assert!(r.reads(backend).await >= 1);
                } else {
                    assert!(writes.is_empty(), "{source} {reference}: {writes:?}");
                    assert_eq!(r.reads(backend).await, 0, "{source} {reference}");
                }
            }
            let local_after = local_files(&r.w);
            if owner.is_some() {
                assert_eq!(local_after, local_before, "{source} {reference}");
            } else {
                assert_ne!(local_after, local_before);
            }
            r.finish().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn resolution_failures_never_fall_back_or_launch_an_agent() {
    let files = [(
        "aspec/work-items/0073-local.md",
        "# Local 73\n\nlocal body\n",
    )];
    for backend in BACKENDS {
        for reject in REJECTIONS {
            // Under each default, the explicit remote reference fails on its own.
            for source in ["local", "github", "http:tracker"] {
                let r = Remote::new(
                    "command::work_item_edit::resolution_failures_never_fall_back_or_launch_an_agent",
                    json!({"workItems": {"source": source}}),
                    &files,
                )
                .await;
                r.reject_reads(backend, reject).await;
                let local_before = local_files(&r.w);
                let (front, log) = r.w.front("unused");
                let error = r
                    .command(r.reference(backend), Mode::Amend)
                    .run_with_frontend(front)
                    .await
                    .unwrap_err();
                let label = format!("{backend:?} {reject:?} {source}: {error}");
                assert!(!error.to_string().is_empty(), "{label}");
                assert!(r.w.docker_runs().is_empty(), "no agent: {label}");
                assert!(!log.events().contains(&Event::Pty(true)), "{label}");
                assert!(r.writes(backend).await.is_empty(), "{label}");
                assert!(r.w.drafts().is_empty(), "{label}");
                assert_eq!(local_files(&r.w), local_before, "{label}");
                assert!(
                    !error.to_string().contains("test-secret-tracker"),
                    "{label}"
                );
                r.finish().await;
            }
        }
    }
    // A bare number under the GitHub default fails the same way; it does not
    // silently edit local item 73.
    let r = Remote::new(
        "command::work_item_edit::resolution_failures_never_fall_back_or_launch_an_agent_bare",
        github(),
        &files,
    )
    .await;
    r.reject_reads(Backend::Github, Reject::Auth).await;
    let local_before = local_files(&r.w);
    let (front, _log) = r.w.front("unused");
    let result = r.command("73", Mode::Amend).run_with_frontend(front).await;
    assert!(result.is_err());
    assert!(r.w.docker_runs().is_empty());
    assert_eq!(local_files(&r.w), local_before);
    r.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_agent_still_publishes_the_saved_edit_and_reports_the_exit() {
    for backend in BACKENDS {
        let r = Remote::new(
            "command::work_item_edit::a_failed_agent_still_publishes_the_saved_edit_and_reports_the_exit",
            json!({}),
            &[],
        )
        .await;
        r.w.docker.on(&["run"]).hold_until_released().exit(3);
        let (task, log) = r.start(r.reference(backend), Mode::Amend).await;
        let draft = log.draft_path().unwrap();
        std::fs::write(&draft, "partial but saved\n").unwrap();
        r.w.docker.release();
        let error = finish(task).await.unwrap_err();
        assert!(
            error.to_string().contains("agent exited with code 3"),
            "{error}"
        );
        assert_eq!(
            r.writes(backend).await,
            [Write {
                verb: "edit",
                body: "partial but saved\n".into()
            }],
            "{backend:?}"
        );
        assert!(
            log.all_text().contains("agent exited with code 3"),
            "{}",
            log.all_text()
        );
        assert!(!draft.exists(), "published drafts are cleaned up");
        r.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_agent_still_finalizes_the_saved_edit() {
    for backend in BACKENDS {
        let r = Remote::new(
            "command::work_item_edit::a_cancelled_agent_still_finalizes_the_saved_edit",
            json!({}),
            &[],
        )
        .await;
        let token = tokio_util::sync::CancellationToken::new();
        let (front, log) = r.w.front("unused");
        let command = r
            .command(r.reference(backend), Mode::Amend)
            .with_cancellation(token.clone());
        let mut task = tokio::spawn(async move { command.run_with_frontend(front).await });
        agent_running(&r.w.docker, &mut task).await;
        let draft = log.draft_path().unwrap();
        std::fs::write(&draft, "saved before cancel\n").unwrap();
        token.cancel();
        // The container stops when the runtime is asked to; release the fake.
        r.w.docker.release();
        let _ = finish(task).await;
        assert_eq!(
            r.writes(backend).await,
            [Write {
                verb: "edit",
                body: "saved before cancel\n".into()
            }],
            "{backend:?}"
        );
        r.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_rejections_retain_the_draft_and_are_visible_with_the_execution_failure() {
    for backend in BACKENDS {
        for reject in REJECTIONS {
            for exit_code in [0, 3] {
                let r = Remote::new(
                    "command::work_item_edit::upload_rejections_retain_the_draft_and_are_visible_with_the_execution_failure",
                    json!({}),
                    &[],
                )
                .await;
                r.reject_updates(backend, reject).await;
                r.w.docker
                    .on(&["run"])
                    .hold_until_released()
                    .exit(exit_code);
                let (task, log) = r.start(r.reference(backend), Mode::Amend).await;
                let draft = log.draft_path().unwrap();
                std::fs::write(&draft, "important work\n").unwrap();
                r.w.docker.release();
                let error = finish(task).await.unwrap_err();
                let label = format!("{backend:?} {reject:?} exit {exit_code}: {error}");
                let text = error.to_string();
                assert!(text.contains("saving failed"), "{label}");
                assert_eq!(
                    text.contains("agent exited with code 3"),
                    exit_code == 3,
                    "{label}"
                );
                // The user's work survives, and the messages say where.
                assert_eq!(
                    std::fs::read_to_string(&draft).unwrap(),
                    "important work\n",
                    "{label}"
                );
                let shown = log.all_text();
                assert!(
                    shown.contains(&draft.display().to_string()),
                    "{label}: {shown}"
                );
                assert!(shown.contains("saving failed"), "{label}: {shown}");
                if exit_code == 3 {
                    assert!(
                        shown.contains("agent exited with code 3"),
                        "{label}: {shown}"
                    );
                }
                // Only edits of the original were attempted; nothing was created.
                let writes = r.writes(backend).await;
                assert!(!writes.is_empty(), "{label}");
                assert!(
                    writes.iter().all(|w| w.verb == "edit"),
                    "{label}: {writes:?}"
                );
                assert!(!shown.contains("test-secret-tracker"), "{label}");
                r.finish().await;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_conflict_pauses_every_publish_including_finalization() {
    for backend in BACKENDS {
        for exit_code in [0, 3] {
            let r = Remote::new(
                "command::work_item_edit::a_remote_conflict_pauses_every_publish_including_finalization",
                json!({}),
                &[],
            )
            .await;
            r.w.docker
                .on(&["run"])
                .hold_until_released()
                .exit(exit_code);
            let (task, log) = r.start(r.reference(backend), Mode::Amend).await;
            let draft = log.draft_path().unwrap();
            r.change_remote(backend, "edited on the tracker\n").await;
            std::fs::write(&draft, "agent edit\n").unwrap();
            r.w.docker.release();
            let error = finish(task).await.unwrap_err();
            let label = format!("{backend:?} exit {exit_code}: {error}");
            assert!(error.to_string().contains("RemoteConflict"), "{label}");
            if exit_code == 3 {
                assert!(
                    error.to_string().contains("agent exited with code 3"),
                    "{label}"
                );
            }
            assert!(
                r.writes(backend).await.is_empty(),
                "remote change is kept: {label}"
            );
            assert_eq!(
                std::fs::read_to_string(&draft).unwrap(),
                "agent edit\n",
                "{label}"
            );
            assert!(
                log.all_text().contains(&draft.display().to_string()),
                "{label}"
            );
            r.finish().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_and_whitespace_drafts_publish_nothing() {
    for backend in BACKENDS {
        for content in ["", " \n\t\n"] {
            let r = Remote::new(
                "command::work_item_edit::empty_and_whitespace_drafts_publish_nothing",
                json!({}),
                &[],
            )
            .await;
            let (task, log) = r.start(r.reference(backend), Mode::Ordinary).await;
            let draft = log.draft_path().unwrap();
            std::fs::write(&draft, content).unwrap();
            r.w.docker.release();
            let outcome = finish(task).await.unwrap();
            assert!(
                r.writes(backend).await.is_empty(),
                "{backend:?} {content:?}"
            );
            assert!(outcome.changed.is_empty(), "{:?}", outcome.changed);
            r.finish().await;
        }
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unsafe_draft_replacements_are_never_read_or_uploaded() {
    for backend in BACKENDS {
        for kind in ["symlink", "invalid-utf8", "directory"] {
            let r = Remote::new(
                "command::work_item_edit::unsafe_draft_replacements_are_never_read_or_uploaded",
                json!({}),
                &[],
            )
            .await;
            let secret = r.w.sandbox.tmp().join("host-secret");
            std::fs::write(&secret, "HOST SECRET").unwrap();
            let (task, log) = r.start(r.reference(backend), Mode::Amend).await;
            let draft = log.draft_path().unwrap();
            std::fs::remove_file(&draft).unwrap();
            match kind {
                "symlink" => std::os::unix::fs::symlink(&secret, &draft).unwrap(),
                "invalid-utf8" => std::fs::write(&draft, [0xff, 0xfe, 0x00]).unwrap(),
                _ => std::fs::create_dir(&draft).unwrap(),
            }
            r.w.docker.release();
            let error = finish(task).await.unwrap_err();
            let label = format!("{backend:?} {kind}: {error}");
            assert!(error.to_string().contains("saving failed"), "{label}");
            assert!(r.writes(backend).await.is_empty(), "{label}");
            assert!(
                draft.symlink_metadata().is_ok(),
                "retained for recovery: {label}"
            );
            assert_eq!(std::fs::read_to_string(&secret).unwrap(), "HOST SECRET");
            assert!(log.all_text().contains("saving failed"), "{label}");
            r.finish().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn seeded_creation_from_a_remote_issue_leaves_the_issue_untouched() {
    // `new spec --work-item` seeds a *new* local item from an issue that is
    // not in the creation target; it never edits or creates remotely.
    for backend in BACKENDS {
        let r = Remote::new(
            "command::work_item_edit::seeded_creation_from_a_remote_issue_leaves_the_issue_untouched",
            json!({}),
            &[],
        )
        .await;
        let (front, _log) = r.w.front("Seeded thing");
        // A seed always runs the interview agent: wait for the held fake
        // container, release it, then await the command.
        let command = r.w.new_spec(false, Some(r.reference(backend)));
        let mut task = tokio::spawn(async move { command.run_with_frontend(front).await });
        agent_running(&r.w.docker, &mut task).await;
        r.w.docker.release();
        let outcome = finish(task).await.unwrap();
        let NewOutcome::Spec(outcome) = outcome else {
            panic!("expected spec")
        };
        // Title and slug come from the seed (the remote issue), numbered next
        // in the local creation target.
        let path = PathBuf::from(outcome.path.expect("local creation"));
        assert_eq!(
            path.parent(),
            Some(r.w.repo.join("aspec/work-items").as_path())
        );
        assert!(
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("0004-") && n.ends_with(".md")),
            "{path:?}"
        );
        assert!(r.writes(backend).await.is_empty(), "{backend:?}");
        assert_eq!(outcome.url, None);
        r.finish().await;
    }
}
