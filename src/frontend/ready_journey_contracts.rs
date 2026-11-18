//! Complete ready commands against an importing store and real presentation
//! adapters. Only the host-ping capability and runtime installation are faked.
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use awman_test_support::{TestCase, TestSandbox};

use crate::command::commands::api_server::event_bus::EventBus;
use crate::command::commands::ready::{
    ReadyCommand, ReadyCommandFlags, ReadyCommandFrontend, ReadyOutcome,
};
use crate::command::commands::Command;
use crate::command::dispatch::{CommandOutcome, Engines};
use crate::command::headless::HeadlessDefaults;
use crate::command::instance::feeds::TabCommandFeeds;
use crate::command::instance::frontend::{DialogChannels, InstanceCommandFrontend};
use crate::data::config::env::EnvSnapshot;
use crate::data::config::image_source::ImageSourceKind;
use crate::data::config::image_source_prompt::{ImageSourceAnswer, ImageSourcePrompt};
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::ready_summary::ReadySummary;
use crate::data::session::{Session, SessionOpenOptions};
use crate::data::setup_step::SetupStep;
use crate::data::step_status::StepStatus;
use crate::engine::agent::AgentImageFrontend;
use crate::engine::error::EngineError;
use crate::engine::image_prep::contracts::Store;
use crate::engine::ready::{LocalAgentPingResult, ReadyFrontend, ReadyHostPing, ReadyPhase};
use crate::frontend::api::command_frontend::ApiDispatchFrontend;
use crate::frontend::cli::CliFrontend;
use crate::frontend::tui::command_frontend::TuiPresentation;

struct Ping;
#[async_trait::async_trait]
impl ReadyHostPing for Ping {
    async fn ping(&self, _: &crate::engine::agent::ResolvedAgent) -> LocalAgentPingResult {
        LocalAgentPingResult::Ok {
            greeting: "Hello".into(),
            response: "Hi".into(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Cli,
    Tui,
    Api,
    Json,
}
#[derive(Default)]
struct Trace {
    prompts: usize,
    messages: Vec<String>,
    progress: Vec<StepStatus>,
    summary: Option<ReadySummary>,
    cli_stderr: Vec<u8>,
}
struct Recording {
    inner: Box<dyn ReadyCommandFrontend>,
    trace: Arc<Mutex<Trace>>,
    cli_archive: Option<String>,
    json: bool,
}
impl UserMessageSink for Recording {
    fn write_message(&mut self, message: UserMessage) {
        self.trace
            .lock()
            .unwrap()
            .messages
            .push(message.text.clone());
        self.inner.write_message(message);
    }
    fn replay_queued(&mut self) {
        self.inner.replay_queued();
    }
}
impl AgentImageFrontend for Recording {
    fn report_step_status(&mut self, step: &SetupStep, status: StepStatus) {
        self.trace.lock().unwrap().progress.push(status.clone());
        self.inner.report_step_status(step, status);
    }
    fn container_frontend(
        &mut self,
    ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
        panic!("ready must not execute an agent container");
    }
}
impl crate::command::commands::agent_trust::AgentTrustFrontend for Recording {}
impl ReadyFrontend for Recording {
    fn ask_create_dockerfile(&mut self, _: &Path) -> Result<bool, EngineError> {
        panic!("the fixture already has a Dockerfile");
    }
    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        panic!("the fixture must not request an audit");
    }
    fn ask_image_source(&mut self, prompt: &ImageSourcePrompt) -> ImageSourceAnswer {
        self.trace.lock().unwrap().prompts += 1;
        if let Some(archive) = &self.cli_archive {
            // Use the actual CLI prompt parser/preview writer with owned input.
            let mut lines: VecDeque<String> = match prompt {
                ImageSourcePrompt::Choose(request) => {
                    let choice = request
                        .choices
                        .iter()
                        .position(|c| c.kind == ImageSourceKind::Archive)
                        .unwrap();
                    [
                        format!("{}\n", choice + 1),
                        format!("{archive}\n"),
                        format!("{}\n", request.scopes.iter().position(|s| s.scope == crate::data::config::image_source_prompt::ImageSourceScope::Repo).unwrap() + 1),
                    ]
                    .into()
                }
                ImageSourcePrompt::Confirm(_) => ["y\n".into()].into(),
            };
            let mut stderr = Vec::new();
            let answer = crate::frontend::cli::per_command::ready_source_prompt::ask(
                prompt,
                &mut |out| {
                    let line = lines.pop_front().expect("unexpected CLI read");
                    out.push_str(&line);
                    Ok(line.len())
                },
                &mut stderr,
            );
            self.trace.lock().unwrap().cli_stderr.extend(stderr);
            answer
        } else {
            ReadyFrontend::ask_image_source(self.inner.as_mut(), prompt)
        }
    }
    fn report_phase(&mut self, phase: &ReadyPhase) {
        self.inner.report_phase(phase);
    }
    fn report_summary(&mut self, summary: &ReadySummary) {
        let mut trace = self.trace.lock().unwrap();
        trace.summary = Some(summary.clone());
        if !self.json {
            // The production CLI writer, including outcome, injected with stderr.
            crate::frontend::cli::per_command::ready::write_summary(
                summary,
                80,
                &mut trace.cli_stderr,
            )
            .unwrap();
        }
        drop(trace);
        self.inner.report_summary(summary);
    }
}

fn env(s: &TestSandbox) -> EnvSnapshot {
    EnvSnapshot::from_full_env(
        s.env()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect(),
    )
}
fn engines(s: &TestSandbox, session: &Session, store: Arc<Store>) -> Engines {
    use crate::data::fs::{ApiPaths, AuthPathResolver};
    use crate::engine::{
        agent::AgentEngine, auth::AuthEngine, git::GitEngine, overlay::OverlayEngine,
    };
    let env = env(s);
    let auth = AuthPathResolver::at_home(s.home());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()));
    Engines {
        runtime: store.clone(),
        container_runtime: None,
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::with_env(env.clone())),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(
            auth,
            ApiPaths::from_env(&env).unwrap(),
        )),
        agent_engine: Arc::new(AgentEngine::new(overlay, store)),
        workflow_state_store: Arc::new(crate::data::WorkflowStateStore::new(session)),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
        ephemeral_secrets: Arc::new(Default::default()),
        lease_workflow_review: None,
    }
}

struct Run {
    outcome: ReadyOutcome,
    trace: Arc<Mutex<Trace>>,
    tui: String,
    api: Vec<String>,
    dialogs: Vec<DialogRequest>,
}
async fn run(
    s: &TestSandbox,
    store: Arc<Store>,
    mode: Mode,
    choose: bool,
    selected: Option<&str>,
) -> Run {
    let session = Session::open_at_git_root(
        s.work(),
        s.work(),
        SessionOpenOptions {
            env: Some(env(s)),
            ..Default::default()
        },
    )
    .unwrap();
    let engines = engines(s, &session, store);
    let trace = Arc::new(Mutex::new(Trace::default()));
    let shared = TabCommandFeeds::new();
    let log = shared.status_log.clone();
    let (dialog_tx, requests) = std::sync::mpsc::channel();
    let (responses, dialog_rx) = std::sync::mpsc::channel();
    let archive = s.work().join("source.tar").to_string_lossy().into_owned();
    if choose && matches!(mode, Mode::Tui) {
        let archive_index = if cfg!(target_os = "macos") { 3 } else { 2 };
        for answer in [
            DialogResponse::Index(archive_index),
            DialogResponse::Text(archive.clone()),
            DialogResponse::Index(1),
            DialogResponse::Char('s'),
        ] {
            responses.send(answer).unwrap();
        }
    }
    let bus = EventBus::new(256);
    let mut events = bus.subscribe();
    let inner: Box<dyn ReadyCommandFrontend> = match mode {
        Mode::Cli | Mode::Json => {
            let args = if matches!(mode, Mode::Json) {
                vec!["awman", "ready", "--json"]
            } else if choose {
                vec!["awman", "ready"]
            } else {
                vec!["awman", "ready", "--headless"]
            };
            let matches = crate::command::dispatch::catalogue::CommandCatalogue::get()
                .build_clap_command()
                .try_get_matches_from(args)
                .unwrap();
            Box::new(CliFrontend::new(matches))
        }
        Mode::Api => Box::new(ApiDispatchFrontend::new("ready", &[], bus.sender())),
        Mode::Tui => {
            let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
            let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
            Box::new(
                InstanceCommandFrontend::new(
                    crate::command::dispatch::parsed_input::parse(
                        "ready",
                        crate::command::dispatch::catalogue::CommandCatalogue::get(),
                    )
                    .unwrap(),
                    DialogChannels::new(dialog_tx, dialog_rx),
                    crate::engine::agent_runtime::AgentIo {
                        stdout: stdout.clone(),
                        stderr: stdout,
                        stdin_tx,
                        stdin_rx,
                        resize: None,
                        initial_size: None,
                    },
                    shared,
                )
                .with_presentation(Arc::new(TuiPresentation)),
            )
        }
    };
    let command = ReadyCommand::new(
        ReadyCommandFlags {
            refresh: false,
            build: false,
            no_cache: false,
            non_interactive: !choose,
            allow_docker: false,
            json: matches!(mode, Mode::Json),
            agent: selected.map(str::to_string),
        },
        engines,
        session,
    )
    .with_local_user(!matches!(mode, Mode::Api))
    .with_headless(matches!(mode, Mode::Api).then(HeadlessDefaults::api))
    .with_host_ping(Arc::new(Ping));
    let outcome = command
        .run_with_frontend(Box::new(Recording {
            inner,
            trace: trace.clone(),
            cli_archive: (choose && matches!(mode, Mode::Cli)).then_some(archive),
            json: matches!(mode, Mode::Json),
        }))
        .await
        .unwrap();
    let tui = log
        .lock()
        .unwrap()
        .iter()
        .map(|l| l.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let mut api = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let crate::data::execution_event::EventPayload::StatusMessage { message, .. } =
            event.payload
        {
            api.push(message);
        }
    }
    Run {
        outcome,
        trace,
        tui,
        api,
        dialogs: requests.try_iter().collect(),
    }
}
fn setup(s: &TestSandbox, source: Option<serde_json::Value>) {
    std::fs::create_dir_all(s.work().join(".awman")).unwrap();
    std::fs::create_dir_all(s.work().join("aspec")).unwrap();
    std::fs::write(s.work().join("Dockerfile.dev"), "FROM scratch\n").unwrap();
    std::fs::write(s.work().join(".awman/Dockerfile.claude"), "FROM scratch\n").unwrap();
    let mut config = serde_json::json!({"agents":{"default":"claude"},"runtime":{}});
    let global = crate::data::config::global_config_path(&env(s)).unwrap();
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(global, r#"{"runtime":{"kind":"builtin"}}"#).unwrap();
    if let Some(source) = source {
        config["runtime"]["builtin"] = serde_json::json!({"imageSource":source});
    }
    std::fs::write(s.work().join(".awman/config.json"), config.to_string()).unwrap();
}
fn exit(outcome: ReadyOutcome) -> i32 {
    CommandOutcome::Ready(outcome).exit_code()
}
fn flat(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_graphic()).collect()
}
fn assert_failure(run: Run, cause: &str, mode: Mode) {
    assert!(!run.outcome.ready);
    let trace = run.trace.lock().unwrap();
    assert_eq!(trace.prompts, 0, "{mode:?}");
    assert!(
        trace
            .messages
            .iter()
            .all(|m| !m.contains(cause) && !m.contains("ready: complete")),
        "{:?}",
        trace.messages
    );
    assert!(trace.progress.iter().all(|s| match s {
        StepStatus::Failed(s) | StepStatus::Warn(s) => s.is_empty(),
        _ => true,
    }));
    let summary = trace.summary.as_ref().unwrap();
    assert!(!summary.is_ready());
    let shown = match mode {
        Mode::Tui => run.tui,
        Mode::Api => run.api.join("\n"),
        _ => String::from_utf8(trace.cli_stderr.clone()).unwrap(),
    };
    if !matches!(mode, Mode::Json) {
        assert_eq!(
            flat(&shown).matches(&flat(cause)).count(),
            1,
            "{mode:?}: {shown}"
        );
        assert_eq!(shown.matches("Tip:").count(), 1, "{mode:?}: {shown}");
        assert!(!shown.contains("io error at"));
        assert!(flat(&shown).contains("Importimage:claude"));
    }
    assert!(run.dialogs.is_empty());
    let stdout = crate::frontend::cli::format_outcome(
        &CommandOutcome::Ready(run.outcome.clone()),
        matches!(mode, Mode::Json),
    );
    if matches!(mode, Mode::Json) {
        assert!(trace.cli_stderr.is_empty());
        let doc: serde_json::Value = serde_json::from_str(&stdout.unwrap()).unwrap();
        assert_eq!(doc["ready"], false);
        assert_eq!(doc["steps"]["image_source"]["message"], cause);
        assert_eq!(doc["steps"]["dev_image"]["message"], cause);
        assert_eq!(doc["image_results"][0]["import"]["Failed"], cause);
        for key in [
            "docker_daemon",
            "dockerfile",
            "aspec_folder",
            "work_items_config",
            "local_agent",
            "dev_image",
            "refresh",
            "image_rebuild",
            "image_source",
        ] {
            assert!(doc["steps"].get(key).is_some(), "{key}");
        }
        assert!(!doc.to_string().contains('┌'));
    } else {
        assert!(stdout.is_none());
    }
    assert_eq!(exit(run.outcome), 1);
}

#[tokio::test]
async fn missing_source_finishes_the_command_with_matching_cli_tui_api_and_json_failures() {
    let s=TestSandbox::for_test(TestCase::component("frontend::ready_journey_contracts::missing_source_finishes_the_command_with_matching_cli_tui_api_and_json_failures"));
    setup(&s, None);
    let before = std::fs::read(s.work().join(".awman/config.json")).unwrap();
    for mode in [Mode::Cli, Mode::Tui, Mode::Api, Mode::Json] {
        let run = run(&s, Arc::new(Store::default()), mode, false, None).await;
        let cause = match &run.outcome.agent_image {
            StepStatus::Failed(cause) => cause.clone(),
            other => panic!("{other:?}"),
        };
        assert!(cause.contains("runtime.builtin.imageSource"));
        assert!(cause.contains("runtime.builtin.images["));
        assert_failure(run, &cause, mode);
        assert_eq!(
            std::fs::read(s.work().join(".awman/config.json")).unwrap(),
            before
        );
        assert_eq!(
            std::fs::read_to_string(s.home().join(".awman/config.json")).unwrap(),
            r#"{"runtime":{"kind":"builtin"}}"#
        );
    }
}

struct Space;
impl crate::engine::oci::DiskSpace for Space {
    fn available(&self, _: &Path) -> Option<u64> {
        Some(u64::MAX)
    }
}

struct Acquiring {
    acquirer: crate::engine::oci::CachingAcquirer,
}
impl crate::engine::image_prep::ImageImporter for Acquiring {
    fn import(
        &self,
        request: &crate::engine::agent_runtime::ImageImportRequest,
        _: &crate::engine::oci::CancelToken,
        _: &mut dyn UserMessageSink,
    ) -> Result<crate::engine::agent_runtime::ImportedImage, EngineError> {
        use crate::engine::oci::{AcquirePolicy, AcquireRequest, ImageAcquirer};
        let acquired = self.acquirer.acquire(
            &AcquireRequest {
                tag: request.tag.clone(),
                source: request.source.clone(),
                platform: request.platform.clone(),
                policy: AcquirePolicy::IfMissing,
                registries: request.registries.clone(),
            },
            &mut |_| {},
        )?;
        Ok(crate::engine::agent_runtime::ImportedImage {
            tag: request.tag.clone(),
            identity: acquired.identity.clone(),
            home_dir: None,
            user: None,
        })
    }
}
#[cfg(unix)]
#[tokio::test]
async fn stopped_local_docker_is_final_and_rendered_once_across_the_whole_ready_journey() {
    let s=TestSandbox::for_test(TestCase::component("frontend::ready_journey_contracts::stopped_local_docker_is_final_and_rendered_once_across_the_whole_ready_journey"));
    let endpoint = format!("unix://{}", s.ports().unix_socket("stopped.sock").display());
    setup(
        &s,
        Some(serde_json::json!({"type":"docker-local","host":endpoint})),
    );
    let before = std::fs::read(s.work().join(".awman/config.json")).unwrap();
    let cause =
        format!("Docker is not running at {endpoint}. Start Docker, then run `awman ready` again.");
    for mode in [Mode::Cli, Mode::Tui, Mode::Api, Mode::Json] {
        let store = Arc::new(Store {
            importer: Some(Arc::new(Acquiring {
                acquirer: crate::engine::oci::CachingAcquirer::new(
                    &s.root().join("cache"),
                    crate::engine::oci::AcquireLimits::default(),
                    env(&s),
                    Arc::new(Space),
                ),
            })),
            ..Default::default()
        });
        // Interactive CLI/TUI still must not offer to replace a configured source.
        let interactive = matches!(mode, Mode::Cli | Mode::Tui);
        let run = run(&s, store.clone(), mode, interactive, None).await;
        assert_failure(run, &cause, mode);
        assert_eq!(store.import_requests.lock().unwrap().len(), 1);
        assert!(store.built.lock().unwrap().is_empty());
        assert_eq!(
            std::fs::read(s.work().join(".awman/config.json")).unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn interactive_cli_and_tui_save_then_finish_the_same_command_and_api_json_reuse_it() {
    let s=TestSandbox::for_test(TestCase::component("frontend::ready_journey_contracts::interactive_cli_and_tui_save_then_finish_the_same_command_and_api_json_reuse_it"));
    for interactive in [Mode::Cli, Mode::Tui] {
        setup(&s, None);
        awman_test_support::fixtures::OciImageBuilder::new()
            .layer(&[("etc/journey", b"ready")])
            .build()
            .write_archive(&s.work().join("source.tar"));
        let store = Arc::new(Store {
            importer: Some(Arc::new(Acquiring {
                acquirer: crate::engine::oci::CachingAcquirer::new(
                    &s.root().join("cache"),
                    crate::engine::oci::AcquireLimits::default(),
                    env(&s),
                    Arc::new(Space),
                ),
            })),
            ..Default::default()
        });
        let completed = run(&s, store.clone(), interactive, true, None).await;
        assert!(completed.outcome.ready, "{:?}", completed.outcome);
        assert_eq!(completed.trace.lock().unwrap().prompts, 2);
        assert_eq!(exit(completed.outcome), 0);
        {
            let requests = store.import_requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(
                matches!(&requests[0].source,crate::data::config::image_source::ImageSourceSpec::Archive {path} if path==&s.work().join("source.tar"))
            );
        }
        let saved = std::fs::read(s.work().join(".awman/config.json")).unwrap();
        for mode in [Mode::Api, Mode::Json] {
            let reused = run(&s, store.clone(), mode, false, None).await;
            assert!(reused.outcome.ready, "{:?}", reused.outcome);
            assert_eq!(reused.trace.lock().unwrap().prompts, 0);
            let value = reused.outcome.to_legacy_json();
            assert_eq!(value["ready"], true);
            assert_eq!(exit(reused.outcome), 0);
            assert_eq!(
                std::fs::read(s.work().join(".awman/config.json")).unwrap(),
                saved
            );
        }
    }
}

#[tokio::test]
async fn a_different_required_agent_cannot_import_the_configured_agents_pinned_default() {
    let s=TestSandbox::for_test(TestCase::component("frontend::ready_journey_contracts::a_different_required_agent_cannot_import_the_configured_agents_pinned_default"));
    setup(
        &s,
        Some(serde_json::json!({"type":"archive","path":s.work().join("claude.tar")})),
    );
    let store = Arc::new(Store::default());
    let result = run(&s, store.clone(), Mode::Json, false, Some("codex")).await;
    assert!(!result.outcome.ready, "{:?}", result.outcome);
    assert!(!result.outcome.image_results[0].optional);
    assert_eq!(exit(result.outcome), 1);
    assert!(store.imports.lock().unwrap().is_empty());
    assert!(store.built.lock().unwrap().is_empty());

    // Declining setup must still leave a previously installed image usable.
    let tag = crate::data::image_tags::agent_image_tag(&s.work(), "codex");
    store.installed.lock().unwrap().insert(
        tag.clone(),
        crate::engine::image_prep::contracts::identity(&tag),
    );
    let kept = run(&s, store.clone(), Mode::Json, false, Some("codex")).await;
    assert!(kept.outcome.ready, "{:?}", kept.outcome);
    assert_eq!(exit(kept.outcome), 0);
    assert!(store.imports.lock().unwrap().is_empty());
}
