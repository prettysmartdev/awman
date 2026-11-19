//! Layer 2 contracts for the builtin image-source choice, driven through
//! `resolve_builtin_inputs` (the call `ReadyCommand::run_with_frontend`
//! makes before it builds the `ReadyEngine`) with a scripted
//! `ReadyFrontend`. The importing runtime is the in-memory
//! `image_prep::contracts::Store`; continuation is proven by preparing the
//! image with an `AgentImagePreparer` built from the returned inputs, exactly
//! as `ReadyEngine::new` builds it. No Docker, registry or host agent is
//! contacted.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use awman_test_support::{TestCase, TestSandbox};

use super::source_setup::{
    build_choice_request, offer_image_source, prompt_allowed, spec_from_selection, SourceSetup,
    SourceSetupContext,
};
use super::{resolve_builtin_inputs, ReadyCommand, ReadyCommandFlags, SourceSetupPolicy};
use crate::command::dispatch::catalogue::{FrontendKind, RelayOrigin};
use crate::command::headless::HeadlessDefaults;
use crate::data::config::env::EnvSnapshot;
use crate::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
use crate::data::config::image_source_prompt::{
    BuilderPreview, ImageSourceAnswer, ImageSourceChoiceRequest, ImageSourceFieldId,
    ImageSourcePreview, ImageSourcePrompt, ImageSourceScope, ImageSourceSelection, ImageSubject,
    MissingSourceReason,
};
use crate::data::config::image_source_setup::image_source_scopes;
use crate::data::config::ResolvedBuiltinConfig;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session, SessionOpenOptions};
use crate::data::step_status::StepStatus;
use crate::engine::image_prep::contracts::{identity, Sink, Store};
use crate::engine::ready::{ReadyEngine, ReadyFrontend};

macro_rules! sandbox {
    ($name:literal) => {
        TestSandbox::for_test(TestCase::component(concat!(
            "command::commands::ready::source_setup_contracts::",
            $name
        )))
    };
}

// ─── Scripted frontend ────────────────────────────────────────────────────

/// Answers image-source prompts from a script and records every prompt.
/// Panics when asked more often than scripted, so "at most once" holds.
#[derive(Default)]
struct Scripted {
    answers: VecDeque<ImageSourceAnswer>,
    prompts: Vec<ImageSourcePrompt>,
    messages: Vec<UserMessage>,
}

impl Scripted {
    fn new(answers: impl IntoIterator<Item = ImageSourceAnswer>) -> Self {
        Self {
            answers: answers.into_iter().collect(),
            ..Default::default()
        }
    }
    fn choose(&self) -> &ImageSourceChoiceRequest {
        match self.prompts.first() {
            Some(ImageSourcePrompt::Choose(request)) => request,
            other => panic!("first prompt is not Choose: {other:?}"),
        }
    }
    fn confirm(&self) -> &ImageSourcePreview {
        match self.prompts.get(1) {
            Some(ImageSourcePrompt::Confirm(preview)) => preview,
            other => panic!("second prompt is not Confirm: {other:?}"),
        }
    }
    fn texts(&self, level: MessageLevel) -> Vec<&str> {
        self.messages
            .iter()
            .filter(|m| m.level == level)
            .map(|m| m.text.as_str())
            .collect()
    }
}

impl UserMessageSink for Scripted {
    fn write_message(&mut self, message: UserMessage) {
        self.messages.push(message);
    }
    fn replay_queued(&mut self) {}
}

impl crate::engine::agent::AgentImageFrontend for Scripted {
    fn report_step_status(&mut self, _: &crate::data::setup_step::SetupStep, _: StepStatus) {}
    fn container_frontend(
        &mut self,
    ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
        panic!("source setup must not launch a container")
    }
}

impl ReadyFrontend for Scripted {
    fn ask_create_dockerfile(
        &mut self,
        _: &Path,
    ) -> Result<bool, crate::engine::error::EngineError> {
        panic!("source setup must not ask for a Dockerfile")
    }
    fn ask_run_audit_on_template(&mut self) -> Result<bool, crate::engine::error::EngineError> {
        panic!("source setup must not start an audit")
    }
    fn report_phase(&mut self, _: &crate::data::ready_phase::ReadyPhase) {}
    fn report_summary(&mut self, _: &crate::data::ready_summary::ReadySummary) {}
    fn ask_image_source(&mut self, prompt: &ImageSourcePrompt) -> ImageSourceAnswer {
        self.prompts.push(prompt.clone());
        self.answers
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected image-source prompt: {prompt:?}"))
    }
}

// ─── Fixture ──────────────────────────────────────────────────────────────

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

/// A session rooted at the sandbox work tree, as a Git repository (repo
/// scope offered) or a plain directory (global only).
fn session(s: &TestSandbox, git: bool) -> Session {
    let root = s.work();
    std::fs::create_dir_all(root.join(".awman")).unwrap();
    std::fs::write(root.join("Dockerfile.dev"), "FROM scratch\n").unwrap();
    let opts = SessionOpenOptions {
        env: Some(env(s)),
        ..Default::default()
    };
    if git {
        Session::open_at_git_root(root.clone(), root, opts).unwrap()
    } else {
        Session::open_at_directory_root(root, opts).unwrap()
    }
}

fn global_path(s: &TestSandbox) -> PathBuf {
    crate::data::config::global_config_path(&env(s)).unwrap()
}

fn repo_path(s: &TestSandbox) -> PathBuf {
    crate::data::config::repo_config_path(&s.work())
}

fn write(path: &Path, json: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json).unwrap();
}

/// File bytes, or `None` when absent.
fn bytes(path: &Path) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(content) => Some(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("read {}: {e}", path.display()),
    }
}

fn engines(
    s: &TestSandbox,
    session: &Session,
    runtime: Arc<Store>,
) -> crate::command::dispatch::Engines {
    use crate::data::fs::{ApiPaths, AuthPathResolver};
    use crate::engine::{
        agent::AgentEngine, auth::AuthEngine, git::GitEngine, overlay::OverlayEngine,
    };
    let env = env(s);
    let auth = AuthPathResolver::at_home(s.home());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()));
    crate::command::dispatch::Engines {
        runtime: runtime.clone(),
        container_runtime: None,
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::with_env(env.clone())),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(
            auth,
            ApiPaths::from_env(&env).unwrap(),
        )),
        agent_engine: Arc::new(AgentEngine::new(overlay, runtime)),
        workflow_state_store: Arc::new(crate::data::WorkflowStateStore::new(session)),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
        ephemeral_secrets: Arc::new(Default::default()),
        lease_workflow_review: None,
    }
}

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn agent(name: &str) -> AgentName {
    AgentName::new(name).unwrap()
}

fn tag(s: &TestSandbox, name: &str) -> String {
    crate::data::image_tags::agent_image_tag(&s.work(), name)
}

fn flags() -> ReadyCommandFlags {
    ReadyCommandFlags {
        refresh: false,
        build: false,
        no_cache: false,
        non_interactive: false,
        allow_docker: false,
        json: false,
        agent: None,
        prepare_only: false,
    }
}

fn select(
    kind: ImageSourceKind,
    fields: &[(ImageSourceFieldId, &str)],
    scope: ImageSourceScope,
    enable_builder: bool,
) -> ImageSourceAnswer {
    ImageSourceAnswer::Selected(ImageSourceSelection {
        kind,
        fields: fields
            .iter()
            .map(|(id, value)| (*id, (*value).to_string()))
            .collect(),
        scope,
        enable_builder,
    })
}

fn docker_local(scope: ImageSourceScope) -> ImageSourceAnswer {
    select(ImageSourceKind::DockerStore, &[], scope, false)
}

/// The production call site: classify, maybe offer, re-resolve.
fn resolve(
    session: &mut Session,
    store: &Arc<Store>,
    flags: &ReadyCommandFlags,
    headless: Option<&HeadlessDefaults>,
    local_user: bool,
    frontend: &mut Scripted,
) -> Option<ResolvedBuiltinConfig> {
    let name = agent("claude");
    resolve_builtin_inputs(
        session,
        store.as_ref(),
        &name,
        SourceSetupPolicy {
            flags,
            headless,
            local_user,
        },
        frontend,
    )
    .unwrap()
}

/// Interactive local CLI/TUI call.
fn resolve_local(
    session: &mut Session,
    store: &Arc<Store>,
    flags: &ReadyCommandFlags,
    frontend: &mut Scripted,
) -> ResolvedBuiltinConfig {
    resolve(session, store, flags, None, true, frontend).expect("importing runtime")
}

/// Prepare `agent`'s image from `resolved`, as `ReadyEngine` would.
async fn prepare(
    s: &TestSandbox,
    session: &Session,
    store: &Arc<Store>,
    resolved: ResolvedBuiltinConfig,
    name: &str,
) -> crate::data::ready_image::ReadyImageResult {
    let mut preparer = crate::engine::image_prep::AgentImagePreparer::new(
        resolved,
        store.clone(),
        s.root().join("data"),
        crate::engine::image_prep::ImageBuildContext::for_session(session, "claude"),
    )
    .with_definition_env(session.env().clone());
    let tag = tag(s, name);
    preparer
        .prepare(
            &crate::engine::image_prep::AgentImageRequest {
                tag: &tag,
                agent: name,
                policy: crate::engine::image_prep::BuildPolicy::Ready {
                    build: false,
                    no_cache: false,
                    refresh: false,
                },
            },
            &mut Sink::default(),
        )
        .await
        .unwrap()
}

// ─── Choices and specs ────────────────────────────────────────────────────

#[test]
fn each_source_kind_builds_the_expected_spec_and_slot() {
    let s = sandbox!("each_source_kind_builds_the_expected_spec_and_slot");
    let store = Arc::new(Store::default());
    let home = s.home();
    let work = s.work();
    let claude = tag(&s, "claude");
    let mut cases = vec![
        (
            docker_local(ImageSourceScope::Global),
            ImageSourceSpec::DockerStore {
                host: None,
                tls: None,
                reference: None,
            },
            "imageSource".to_string(),
        ),
        (
            select(
                ImageSourceKind::DockerStore,
                &[
                    (ImageSourceFieldId::Host, " unix:///run/alt.sock "),
                    (ImageSourceFieldId::Reference, "mine/claude:1"),
                ],
                ImageSourceScope::Global,
                false,
            ),
            ImageSourceSpec::DockerStore {
                host: Some("unix:///run/alt.sock".into()),
                tls: None,
                reference: Some("mine/claude:1".into()),
            },
            // A fixed reference names one image: never the default.
            format!("images/{claude}"),
        ),
        (
            select(
                ImageSourceKind::Archive,
                &[(ImageSourceFieldId::Path, "img/claude.tar")],
                ImageSourceScope::Global,
                false,
            ),
            ImageSourceSpec::Archive {
                path: work.join("img/claude.tar"),
            },
            format!("images/{claude}"),
        ),
        (
            select(
                ImageSourceKind::Archive,
                &[(ImageSourceFieldId::Path, "~/claude.tar")],
                ImageSourceScope::Global,
                false,
            ),
            ImageSourceSpec::Archive {
                path: home.join("claude.tar"),
            },
            format!("images/{claude}"),
        ),
        (
            select(
                ImageSourceKind::Registry,
                &[(ImageSourceFieldId::Registry, "ghcr.io")],
                ImageSourceScope::Global,
                false,
            ),
            ImageSourceSpec::Registry {
                registry: Some("ghcr.io".into()),
                reference: None,
            },
            "imageSource".to_string(),
        ),
    ];
    if cfg!(target_os = "macos") {
        cases.push((
            select(
                ImageSourceKind::AppleStore,
                &[],
                ImageSourceScope::Global,
                false,
            ),
            ImageSourceSpec::AppleStore { reference: None },
            "imageSource".to_string(),
        ));
    }
    for (answer, expected, key) in cases {
        remove(&global_path(&s));
        let mut session = session(&s, false);
        let mut frontend = Scripted::new([answer, ImageSourceAnswer::Confirmed]);
        let resolved = resolve_local(&mut session, &store, &flags(), &mut frontend);
        let builtin = &json(&global_path(&s))["runtime"]["builtin"];
        let written = match key.split_once('/') {
            Some((_, tag)) => &builtin["images"][tag],
            None => &builtin[key.as_str()],
        };
        assert_eq!(*written, serde_json::to_value(&expected).unwrap(), "{key}");
        // The engine inputs are the re-resolved ones.
        let effective = resolved
            .images
            .get(&claude)
            .or(resolved.image_source.as_ref());
        assert_eq!(effective, Some(&expected));
        assert_eq!(frontend.prompts.len(), 2);
    }
    s.finish();
}

#[test]
fn choices_offer_apple_local_only_on_macos_and_builder_only_for_buildable_local_stores() {
    let s = sandbox!(
        "choices_offer_apple_local_only_on_macos_and_builder_only_for_buildable_local_stores"
    );
    let session = session(&s, true);
    let subject = ImageSubject {
        agent: "claude".into(),
        tag: tag(&s, "claude"),
        is_default: true,
        installed: false,
    };
    let request = |session: &Session| {
        let config = session.effective_config();
        let status = config.image_source_status(&subject.tag, true);
        let scopes =
            image_source_scopes(&config, Some(&s.work()), &status, &subject.tag, true).unwrap();
        build_choice_request(&config, subject.clone(), &status, vec![], &scopes).unwrap()
    };
    let r = request(&session);
    let kinds: Vec<ImageSourceKind> = r.choices.iter().map(|c| c.kind).collect();
    let mut expected = vec![ImageSourceKind::DockerStore];
    if cfg!(target_os = "macos") {
        expected.push(ImageSourceKind::AppleStore);
    }
    expected.extend([ImageSourceKind::Archive, ImageSourceKind::Registry]);
    assert_eq!(kinds, expected);
    assert_eq!(r.reason, MissingSourceReason::Unconfigured);
    assert_eq!(
        r.scopes.iter().map(|o| o.scope).collect::<Vec<_>>(),
        [ImageSourceScope::Global, ImageSourceScope::Repo]
    );
    for choice in &r.choices {
        let buildable = (choice.kind == ImageSourceKind::DockerStore && cfg!(unix))
            || choice.kind == ImageSourceKind::AppleStore;
        assert_eq!(
            choice.builder_offer.is_some(),
            buildable,
            "{:?}",
            choice.kind
        );
    }
    let archive = r
        .choices
        .iter()
        .find(|c| c.kind == ImageSourceKind::Archive)
        .unwrap();
    assert!(archive.fields.iter().all(|f| f.required));
    // An already enabled builder is not offered again, and is explained.
    write(
        &global_path(&s),
        r#"{"runtime":{"builtin":{"builder":{"enabled":true}}}}"#,
    );
    let reopened = self::session(&s, true);
    let on = request(&reopened);
    assert!(on.choices.iter().all(|c| c.builder_offer.is_none()));
    assert!(on.notes.iter().any(|n| n.contains("already enabled")));
    s.finish();
}

#[test]
fn local_choice_does_not_enable_the_builder_unless_explicitly_chosen() {
    let s = sandbox!("local_choice_does_not_enable_the_builder_unless_explicitly_chosen");
    let store = Arc::new(Store::default());
    let cases = [
        // (answer, builder key expected)
        (
            select(
                ImageSourceKind::DockerStore,
                &[],
                ImageSourceScope::Global,
                false,
            ),
            false,
        ),
        (
            select(
                ImageSourceKind::DockerStore,
                &[],
                ImageSourceScope::Global,
                true,
            ),
            cfg!(unix),
        ),
        (
            select(
                ImageSourceKind::DockerStore,
                &[(ImageSourceFieldId::Host, "tcp://docker.example:2375")],
                ImageSourceScope::Global,
                true,
            ),
            false,
        ),
        (
            select(
                ImageSourceKind::Registry,
                &[],
                ImageSourceScope::Global,
                true,
            ),
            false,
        ),
        (
            select(
                ImageSourceKind::Archive,
                &[(ImageSourceFieldId::Path, "/c.tar")],
                ImageSourceScope::Global,
                true,
            ),
            false,
        ),
    ];
    for (answer, builder) in cases {
        remove(&global_path(&s));
        let asked = matches!(&answer, ImageSourceAnswer::Selected(sel) if sel.enable_builder);
        let mut session = session(&s, false);
        let mut frontend = Scripted::new([answer, ImageSourceAnswer::Confirmed]);
        let resolved = resolve_local(&mut session, &store, &flags(), &mut frontend);
        let written = json(&global_path(&s));
        assert_eq!(
            written["runtime"]["builtin"]["builder"]["enabled"].as_bool(),
            builder.then_some(true),
            "{written}"
        );
        assert_eq!(resolved.builder_enabled, builder);
        let preview = frontend.confirm();
        assert_eq!(preview.fragment.contains("builder"), builder);
        if builder {
            assert_eq!(preview.builder, BuilderPreview::Enabled);
        } else {
            assert_eq!(
                preview.builder,
                BuilderPreview::Unchanged { effective: false }
            );
        }
        // A "yes" that cannot be honoured is explained, never silent.
        assert_eq!(
            preview
                .notes
                .iter()
                .any(|n| n.contains("builder was not enabled")),
            asked && !builder,
            "{:?}",
            preview.notes
        );
    }
    s.finish();
}

#[test]
fn selected_scope_decides_the_file_written() {
    let s = sandbox!("selected_scope_decides_the_file_written");
    let store = Arc::new(Store::default());
    for (scope, written, other) in [
        (ImageSourceScope::Repo, repo_path(&s), global_path(&s)),
        (ImageSourceScope::Global, global_path(&s), repo_path(&s)),
    ] {
        write(&global_path(&s), r#"{"runtime":{"builtin":{"vcpus":2}}}"#);
        write(&repo_path(&s), r#"{"runtime":{"builtin":{"vcpus":3}}}"#);
        let untouched = bytes(&other);
        let mut session = session(&s, true);
        let mut frontend = Scripted::new([docker_local(scope), ImageSourceAnswer::Confirmed]);
        resolve_local(&mut session, &store, &flags(), &mut frontend);
        let doc = json(&written);
        assert_eq!(
            doc["runtime"]["builtin"]["imageSource"]["type"],
            "docker-local"
        );
        assert!(doc["runtime"]["builtin"]["vcpus"].is_number(), "{doc}");
        assert_eq!(bytes(&other), untouched);
        assert_eq!(frontend.confirm().path, written);
        assert_eq!(frontend.confirm().scope, scope);
        let saved = frontend.texts(MessageLevel::Info);
        assert!(
            saved.iter().any(
                |t| t.starts_with("ready: saved runtime.builtin.imageSource to")
                    && t.contains(&format!("({} config); continuing.", scope.as_str()))
            ),
            "{saved:?}"
        );
    }
    s.finish();
}

#[test]
fn validation_failure_leaves_config_unchanged_and_does_not_reask() {
    let s = sandbox!("validation_failure_leaves_config_unchanged_and_does_not_reask");
    let store = Arc::new(Store::default());
    write(&global_path(&s), r#"{"runtime":{"builtin":{"vcpus":2}}}"#);
    let before = bytes(&global_path(&s));
    for (answer, needle) in [
        (
            select(
                ImageSourceKind::Archive,
                &[(ImageSourceFieldId::Path, "  ")],
                ImageSourceScope::Global,
                false,
            ),
            "Archive path is required",
        ),
        // Repo scope is not offered outside a Git repository.
        (
            docker_local(ImageSourceScope::Repo),
            "that choice was not offered",
        ),
        // Layer 0 rejects the spec shape.
        (
            select(
                ImageSourceKind::DockerStore,
                &[(ImageSourceFieldId::Host, "http://docker")],
                ImageSourceScope::Global,
                false,
            ),
            "must start with one of",
        ),
    ] {
        let mut session = session(&s, false);
        let mut frontend = Scripted::new([answer]);
        let resolved = resolve_local(&mut session, &store, &flags(), &mut frontend);
        assert_eq!(frontend.prompts.len(), 1, "exactly one Choose, no Confirm");
        assert!(matches!(frontend.prompts[0], ImageSourcePrompt::Choose(_)));
        let errors = frontend.texts(MessageLevel::Error);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].starts_with("ready: image source not saved: "),
            "{}",
            errors[0]
        );
        assert!(errors[0].contains(needle), "{}", errors[0]);
        assert!(
            errors[0].ends_with("Configuration unchanged."),
            "{}",
            errors[0]
        );
        assert_eq!(bytes(&global_path(&s)), before);
        assert_eq!(resolved.image_source, None);
    }
    assert_eq!(bytes(&repo_path(&s)), None);
    s.finish();
}

// ─── Declining, cancelling, ended input ───────────────────────────────────

#[tokio::test]
async fn leave_unconfigured_writes_nothing_and_keeps_an_installed_image_usable() {
    let s = sandbox!("leave_unconfigured_writes_nothing_and_keeps_an_installed_image_usable");
    let store = Arc::new(Store::default());
    let claude = tag(&s, "claude");
    store
        .installed
        .lock()
        .unwrap()
        .insert(claude.clone(), identity(&claude));
    let mut session = session(&s, true);
    let mut frontend = Scripted::new([ImageSourceAnswer::LeaveUnconfigured]);
    let resolved = resolve_local(&mut session, &store, &flags(), &mut frontend);
    let request = frontend.choose();
    assert!(request.subject.installed);
    assert!(request
        .notes
        .iter()
        .any(|n| n.contains("already installed and stays usable")));
    assert_eq!(
        frontend.texts(MessageLevel::Info),
        ["ready: image source left unconfigured."]
    );
    assert_eq!(bytes(&global_path(&s)), None);
    assert_eq!(bytes(&repo_path(&s)), None);
    assert_eq!(resolved.image_source, None);
    let result = prepare(&s, &session, &store, resolved, "claude").await;
    assert_eq!(result.import, StepStatus::Done);
    assert!(store.imports.lock().unwrap().is_empty());
    s.finish();
}

#[test]
fn declining_the_preview_writes_nothing() {
    let s = sandbox!("declining_the_preview_writes_nothing");
    let store = Arc::new(Store::default());
    let mut session = session(&s, true);
    let mut frontend = Scripted::new([
        docker_local(ImageSourceScope::Global),
        ImageSourceAnswer::LeaveUnconfigured,
    ]);
    let resolved = resolve_local(&mut session, &store, &flags(), &mut frontend);
    assert_eq!(frontend.prompts.len(), 2);
    assert_eq!(frontend.confirm().field, "runtime.builtin.imageSource");
    assert_eq!(resolved.image_source, None);
    assert_eq!(bytes(&global_path(&s)), None);
    assert_eq!(bytes(&repo_path(&s)), None);
    assert_eq!(
        frontend.texts(MessageLevel::Info),
        ["ready: image source left unconfigured."]
    );
    s.finish();
}

#[tokio::test]
async fn cancel_at_either_stage_writes_nothing_and_ready_reports_the_missing_source() {
    let s = sandbox!("cancel_at_either_stage_writes_nothing_and_ready_reports_the_missing_source");
    let store = Arc::new(Store::default());
    for script in [
        vec![ImageSourceAnswer::Cancelled],
        vec![
            docker_local(ImageSourceScope::Global),
            ImageSourceAnswer::Cancelled,
        ],
        // An answer that does not belong to its stage is a cancel too.
        vec![ImageSourceAnswer::Confirmed],
        vec![
            docker_local(ImageSourceScope::Global),
            docker_local(ImageSourceScope::Global),
        ],
    ] {
        let asked = script.len();
        let mut session = session(&s, true);
        let mut frontend = Scripted::new(script);
        let resolved = resolve_local(&mut session, &store, &flags(), &mut frontend);
        assert_eq!(frontend.prompts.len(), asked);
        assert_eq!(
            frontend.texts(MessageLevel::Info),
            ["ready: image source setup cancelled; configuration unchanged."]
        );
        assert_eq!(bytes(&global_path(&s)), None);
        assert_eq!(bytes(&repo_path(&s)), None);
        let result = prepare(&s, &session, &store, resolved, "claude").await;
        match &result.import {
            StepStatus::Failed(text) => {
                assert!(text.contains("no image source is configured for"), "{text}")
            }
            other => panic!("expected the missing-source failure, got {other:?}"),
        }
    }
    s.finish();
}

#[test]
fn eof_at_either_stage_writes_nothing() {
    let s = sandbox!("eof_at_either_stage_writes_nothing");
    let store = Arc::new(Store::default());
    for script in [
        vec![ImageSourceAnswer::Eof],
        vec![
            docker_local(ImageSourceScope::Global),
            ImageSourceAnswer::Eof,
        ],
    ] {
        let mut session = session(&s, true);
        let mut frontend = Scripted::new(script);
        let resolved = resolve_local(&mut session, &store, &flags(), &mut frontend);
        assert_eq!(
            frontend.texts(MessageLevel::Info),
            ["ready: input ended; configuration unchanged."]
        );
        assert_eq!(resolved.image_source, None);
        assert_eq!(bytes(&global_path(&s)), None);
        assert_eq!(bytes(&repo_path(&s)), None);
    }
    s.finish();
}

// ─── Once per run, and continuation ───────────────────────────────────────

#[test]
fn at_most_one_choose_and_one_confirm_per_run() {
    let s = sandbox!("at_most_one_choose_and_one_confirm_per_run");
    let store = Arc::new(Store::default());
    let mut session = session(&s, true);
    std::fs::write(s.work().join(".awman/Dockerfile.codex"), "FROM scratch\n").unwrap();
    let build = ReadyCommandFlags {
        build: true,
        ..flags()
    };
    // Two images lack a source; the script has exactly two answers and the
    // fake panics on a third prompt.
    let mut frontend = Scripted::new([
        docker_local(ImageSourceScope::Global),
        ImageSourceAnswer::Confirmed,
    ]);
    resolve_local(&mut session, &store, &build, &mut frontend);
    assert_eq!(frontend.prompts.len(), 2);
    let request = frontend.choose();
    assert_eq!(request.subject.tag, tag(&s, "claude"));
    assert_eq!(
        request
            .also_missing
            .iter()
            .map(|m| m.tag.clone())
            .collect::<Vec<_>>(),
        [tag(&s, "codex")]
    );
    let preview = frontend.confirm();
    assert_eq!(preview.effective.len(), 2);
    assert!(preview
        .effective
        .iter()
        .all(|e| e.source.as_ref().is_some_and(|src| src.from_this_write)));
    // The next run finds a configured source and never asks.
    let mut silent = Scripted::default();
    resolve_local(&mut session, &store, &build, &mut silent);
    assert!(silent.prompts.is_empty());
    s.finish();
}

#[tokio::test]
async fn ready_continues_with_the_saved_source_in_the_same_invocation() {
    let s = sandbox!("ready_continues_with_the_saved_source_in_the_same_invocation");
    let store = Arc::new(Store::default());
    let mut session = session(&s, true);
    assert_eq!(
        ReadyEngine::builtin_inputs(&session, store.as_ref())
            .unwrap()
            .unwrap()
            .image_source,
        None
    );
    let mut frontend = Scripted::new([
        select(
            ImageSourceKind::Registry,
            &[(ImageSourceFieldId::Registry, "registry.example")],
            ImageSourceScope::Repo,
            false,
        ),
        ImageSourceAnswer::Confirmed,
    ]);
    let resolved = resolve_local(&mut session, &store, &flags(), &mut frontend);
    let chosen = ImageSourceSpec::Registry {
        registry: Some("registry.example".into()),
        reference: None,
    };
    assert_eq!(resolved.image_source, Some(chosen.clone()));
    // The session itself was reloaded from disk.
    assert_eq!(
        session.effective_config().builtin_runtime().image_source,
        Some(chosen.clone())
    );
    assert_eq!(
        json(&repo_path(&s))["runtime"]["builtin"]["imageSource"]["type"],
        "registry"
    );
    let result = prepare(&s, &session, &store, resolved, "claude").await;
    assert_eq!(result.import, StepStatus::Done, "{result:?}");
    let requests = store.import_requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].source, chosen);
    assert_eq!(requests[0].tag, tag(&s, "claude"));
    s.finish();
}

#[test]
fn build_offers_an_images_entry_for_a_non_default_agent_and_leaves_the_default_source() {
    let s = sandbox!(
        "build_offers_an_images_entry_for_a_non_default_agent_and_leaves_the_default_source"
    );
    let store = Arc::new(Store::default());
    write(
        &global_path(&s),
        r#"{"runtime":{"builtin":{"imageSource":{"type":"archive","path":"/claude.tar"}}}}"#,
    );
    let global = bytes(&global_path(&s));
    let mut session = session(&s, true);
    std::fs::write(s.work().join(".awman/Dockerfile.codex"), "FROM scratch\n").unwrap();
    let build = ReadyCommandFlags {
        build: true,
        ..flags()
    };
    // Without --build only the default image is prepared, and it has one.
    let mut silent = Scripted::default();
    resolve_local(&mut session, &store, &flags(), &mut silent);
    assert!(silent.prompts.is_empty());
    let mut frontend = Scripted::new([
        docker_local(ImageSourceScope::Repo),
        ImageSourceAnswer::Confirmed,
    ]);
    let resolved = resolve_local(&mut session, &store, &build, &mut frontend);
    let codex = tag(&s, "codex");
    let request = frontend.choose();
    assert_eq!(request.subject.tag, codex);
    assert!(!request.subject.is_default);
    assert_eq!(
        request.reason,
        MissingSourceReason::DefaultPinned {
            kind: ImageSourceKind::Archive
        }
    );
    assert_eq!(request.scopes[0].scope, ImageSourceScope::Repo);
    assert_eq!(
        frontend.confirm().field,
        format!("runtime.builtin.images[\"{codex}\"]")
    );
    assert_eq!(
        json(&repo_path(&s))["runtime"]["builtin"]["images"][codex.as_str()]["type"],
        "docker-local"
    );
    assert_eq!(
        bytes(&global_path(&s)),
        global,
        "the default source is unchanged"
    );
    assert_eq!(
        resolved.image_source,
        Some(ImageSourceSpec::Archive {
            path: "/claude.tar".into()
        })
    );
    assert!(resolved.images.contains_key(&codex));
    s.finish();
}

#[test]
fn a_configured_source_that_fails_is_never_offered_for_replacement() {
    let s = sandbox!("a_configured_source_that_fails_is_never_offered_for_replacement");
    let store = Arc::new(Store::default());
    // Points at an archive that does not exist, and a stopped Docker host.
    for json in [
        r#"{"runtime":{"builtin":{"imageSource":{"type":"archive","path":"/missing/claude.tar"}}}}"#,
        r#"{"runtime":{"builtin":{"imageSource":{"type":"docker-local","host":"unix:///nowhere/docker.sock"}}}}"#,
    ] {
        write(&global_path(&s), json);
        let before = bytes(&global_path(&s));
        let mut session = session(&s, true);
        let mut frontend = Scripted::default();
        let resolved = resolve_local(&mut session, &store, &flags(), &mut frontend);
        assert!(frontend.prompts.is_empty());
        assert!(frontend.messages.is_empty());
        assert!(resolved.image_source.is_some());
        assert_eq!(bytes(&global_path(&s)), before);
    }
    s.finish();
}

// ─── Never prompt, never write ────────────────────────────────────────────

#[test]
fn headless_flag_never_prompts_or_writes() {
    let s = sandbox!("headless_flag_never_prompts_or_writes");
    let store = Arc::new(Store::default());
    let mut session = session(&s, true);
    let headless = ReadyCommandFlags {
        non_interactive: true,
        ..flags()
    };
    let mut frontend = Scripted::default();
    let resolved = resolve(&mut session, &store, &headless, None, true, &mut frontend).unwrap();
    assert!(frontend.prompts.is_empty());
    assert_eq!(resolved.image_source, None);
    assert_eq!(bytes(&global_path(&s)), None);
    assert_eq!(bytes(&repo_path(&s)), None);
    s.finish();
}

#[test]
fn json_never_prompts_or_writes() {
    let s = sandbox!("json_never_prompts_or_writes");
    let store = Arc::new(Store::default());
    let mut session = session(&s, true);
    // `--json` implies `--headless` in the catalogue; the gate also refuses
    // `--json` on its own.
    let json_flags = ReadyCommandFlags {
        json: true,
        ..flags()
    };
    let mut frontend = Scripted::default();
    let resolved = resolve(&mut session, &store, &json_flags, None, true, &mut frontend).unwrap();
    assert!(frontend.prompts.is_empty());
    assert_eq!(resolved.image_source, None);
    assert_eq!(bytes(&global_path(&s)), None);
    assert_eq!(bytes(&repo_path(&s)), None);
    s.finish();
}

#[test]
fn api_caller_never_prompts_or_writes() {
    let s = sandbox!("api_caller_never_prompts_or_writes");
    let store = Arc::new(Store::default());
    let mut session = session(&s, true);
    let api = HeadlessDefaults::api();
    for (headless, local_user) in [(Some(&api), false), (Some(&api), true), (None, false)] {
        let mut frontend = Scripted::default();
        let resolved = resolve(
            &mut session,
            &store,
            &flags(),
            headless,
            local_user,
            &mut frontend,
        )
        .unwrap();
        assert!(frontend.prompts.is_empty());
        assert_eq!(resolved.image_source, None);
    }
    assert_eq!(bytes(&global_path(&s)), None);
    assert_eq!(bytes(&repo_path(&s)), None);
    s.finish();
}

#[test]
fn a_hand_built_command_fails_closed() {
    let s = sandbox!("a_hand_built_command_fails_closed");
    let session = session(&s, false);
    let engines = engines(&s, &session, Arc::new(Store::default()));
    let command = ReadyCommand::new(flags(), engines, session);
    assert!(!command.local_user);
    assert!(command.headless.is_none());
    let command = command
        .with_local_user(true)
        .with_headless(Some(HeadlessDefaults::api()));
    assert!(!prompt_allowed(
        &command.flags,
        command.headless.as_ref(),
        command.local_user
    ));
    s.finish();
}

#[test]
fn prompt_gate_is_closed_for_every_non_local_caller() {
    use crate::data::daemon_state::{CommandListener, FeatureName};
    let gate = |kind: FrontendKind, flags: &ReadyCommandFlags| {
        prompt_allowed(
            flags,
            ReadyCommand::headless_for(kind).as_ref(),
            kind.is_local_user(),
        )
    };
    let open = [
        FrontendKind::Cli,
        FrontendKind::Tui,
        FrontendKind::Relayed {
            origin: RelayOrigin::Cli,
        },
        FrontendKind::Relayed {
            origin: RelayOrigin::Tui,
        },
        FrontendKind::ENGINE_LOCAL,
    ];
    for kind in open {
        assert!(gate(kind, &flags()), "{kind:?}");
        for closed in [
            ReadyCommandFlags {
                non_interactive: true,
                ..flags()
            },
            ReadyCommandFlags {
                json: true,
                non_interactive: true,
                ..flags()
            },
        ] {
            assert!(!gate(kind, &closed), "{kind:?} {closed:?}");
        }
    }
    for feature in FeatureName::ALL {
        for listener in [CommandListener::Local, CommandListener::Network] {
            let kind = FrontendKind::Daemon { feature, listener };
            if kind == FrontendKind::ENGINE_LOCAL {
                continue;
            }
            assert!(!gate(kind, &flags()), "{kind:?}");
        }
    }
}

/// Production files (outside test modules) that call `name(` in any form —
/// method call, fully qualified trait call or free function — other than
/// its definition.
fn production_callers(name: &str) -> Vec<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let this_file = src.join("command/commands/ready/source_setup_contracts.rs");
    let call = format!("{name}(");
    let definition = format!("fn {name}(");
    let mut callers = Vec::new();
    let mut stack = vec![src];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy();
            let test_file =
                name == "tests.rs" || name.ends_with("_tests.rs") || name.ends_with("contracts.rs");
            if path.extension().is_none_or(|e| e != "rs") || path == this_file || test_file {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // Inline test modules sit after the first `#[cfg(test)]`.
            let production = text
                .find("#[cfg(test)]")
                .map_or(&text[..], |at| &text[..at]);
            let calls = production.matches(call.as_str()).count();
            let definitions = production.matches(definition.as_str()).count();
            if calls > definitions {
                callers.push(path);
            }
        }
    }
    callers.sort();
    callers
}

/// Squad, API session setup, workflow preflight and the engine never ask or
/// write a source: the only production caller of `ask_image_source` (in any
/// call form) and of the Layer 0 plan/write entry points is `ReadyCommand`'s
/// source setup.
#[test]
fn only_ready_command_asks_for_an_image_source() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let ready = vec![src.join("command/commands/ready/source_setup.rs")];
    assert_eq!(production_callers(concat!("ask_image", "_source")), ready);
    assert_eq!(
        production_callers(concat!("apply_image", "_source_plan")),
        ready
    );
    assert_eq!(
        production_callers(concat!("plan_image", "_source_edit")),
        ready
    );
}

#[test]
fn offer_without_a_missing_image_does_nothing() {
    let s = sandbox!("offer_without_a_missing_image_does_nothing");
    write(
        &global_path(&s),
        r#"{"runtime":{"builtin":{"imageSource":{"type":"registry"}}}}"#,
    );
    let mut session = session(&s, true);
    let mut frontend = Scripted::default();
    let setup = offer_image_source(
        SourceSetupContext {
            session: &mut session,
            agent: &agent("claude"),
            build: true,
            installed: &|_| false,
        },
        &mut frontend,
    )
    .unwrap();
    assert_eq!(setup, SourceSetup::Unchanged);
    assert!(frontend.prompts.is_empty());
    // A selection check never depends on the frontend.
    let request = ImageSourceChoiceRequest {
        subject: ImageSubject {
            agent: "claude".into(),
            tag: tag(&s, "claude"),
            is_default: true,
            installed: false,
        },
        also_missing: vec![],
        reason: MissingSourceReason::Unconfigured,
        choices: vec![],
        scopes: vec![],
        notes: vec![],
    };
    let selection = ImageSourceSelection {
        kind: ImageSourceKind::Registry,
        fields: Default::default(),
        scope: ImageSourceScope::Global,
        enable_builder: false,
    };
    assert_eq!(
        spec_from_selection(&selection, &request, None, None, &s.work()).unwrap_err(),
        "that choice was not offered"
    );
    s.finish();
}

fn remove(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("remove {}: {e}", path.display()),
    }
}

// ─── Review findings (fix-source-choice) ──────────────────────────────────

/// Interactive local call for `name` (as `ready --agent <name>`).
fn resolve_agent(
    session: &mut Session,
    store: &Arc<Store>,
    name: &str,
    frontend: &mut Scripted,
) -> ResolvedBuiltinConfig {
    resolve_builtin_inputs(
        session,
        store.as_ref(),
        &agent(name),
        SourceSetupPolicy {
            flags: &flags(),
            headless: None,
            local_user: true,
        },
        frontend,
    )
    .unwrap()
    .expect("importing runtime")
}

/// Review finding 1: an explicit reference equal to the displayed tag is
/// still fixed to one image. It is saved as that tag's `images` entry, so a
/// later `ready --agent codex` is offered its own source instead of reusing
/// Claude's image.
#[test]
fn a_reference_equal_to_the_tag_is_saved_for_that_image_only() {
    let s = sandbox!("a_reference_equal_to_the_tag_is_saved_for_that_image_only");
    let store = Arc::new(Store::default());
    let claude = tag(&s, "claude");
    let codex = tag(&s, "codex");
    for kind in [ImageSourceKind::DockerStore, ImageSourceKind::Registry] {
        remove(&global_path(&s));
        let mut session = session(&s, false);
        let mut fields = vec![(ImageSourceFieldId::Reference, claude.as_str())];
        if kind == ImageSourceKind::Registry {
            fields.push((ImageSourceFieldId::Registry, "ghcr.io"));
        }
        let mut first = Scripted::new([
            select(kind, &fields, ImageSourceScope::Global, false),
            ImageSourceAnswer::Confirmed,
        ]);
        let resolved = resolve_agent(&mut session, &store, "claude", &mut first);
        assert_eq!(
            first.confirm().field,
            format!("runtime.builtin.images[\"{claude}\"]"),
            "{kind}"
        );
        assert_eq!(resolved.image_source, None, "{kind}: no reusable default");
        assert!(resolved.images.contains_key(&claude), "{kind}");

        // Second invocation, another agent: no source applies to it.
        let mut second = Scripted::new([ImageSourceAnswer::LeaveUnconfigured]);
        let resolved = resolve_agent(&mut session, &store, "codex", &mut second);
        let request = second.choose();
        assert_eq!(request.subject.tag, codex, "{kind}");
        assert_eq!(request.reason, MissingSourceReason::Unconfigured, "{kind}");
        assert!(!resolved.images.contains_key(&codex), "{kind}");
        assert_eq!(resolved.image_source, None, "{kind}");
    }
    s.finish();
}

/// Review finding 1: a fixed default configured by hand belongs to the
/// configured agent, not to whichever agent `--agent` names.
#[test]
fn a_pinned_default_is_offered_for_replacement_when_another_agent_is_requested() {
    let s = sandbox!("a_pinned_default_is_offered_for_replacement_when_another_agent_is_requested");
    let store = Arc::new(Store::default());
    write(
        &global_path(&s),
        r#"{"agents":{"default":"claude"},"runtime":{"builtin":{"imageSource":{"type":"registry","registry":"ghcr.io","reference":"org/claude:1"}}}}"#,
    );
    let before = bytes(&global_path(&s));
    let mut session = session(&s, false);
    // The configured agent keeps its pinned default without a prompt.
    let mut silent = Scripted::default();
    resolve_agent(&mut session, &store, "claude", &mut silent);
    assert!(silent.prompts.is_empty());
    // `ready --agent codex`: the pinned Claude reference is not codex's source.
    let mut frontend = Scripted::new([ImageSourceAnswer::LeaveUnconfigured]);
    resolve_agent(&mut session, &store, "codex", &mut frontend);
    let request = frontend.choose();
    assert_eq!(request.subject.tag, tag(&s, "codex"));
    assert!(!request.subject.is_default);
    assert_eq!(
        request.reason,
        MissingSourceReason::DefaultPinned {
            kind: ImageSourceKind::Registry
        }
    );
    assert_eq!(bytes(&global_path(&s)), before);
    s.finish();
}

/// Review finding 2: outcome messages interpolate typed values, paths and
/// error text; none of their terminal controls reaches a frontend.
#[test]
fn source_setup_messages_carry_no_terminal_controls() {
    let s = sandbox!("source_setup_messages_carry_no_terminal_controls");
    let store = Arc::new(Store::default());
    let mut session = session(&s, true);
    let hostile = "\u{1b}]52;c;aGk=\u{7}\u{1b}[2Jtcp://x\nawman: saved";
    let mut frontend = Scripted::new([select(
        ImageSourceKind::DockerStore,
        &[(ImageSourceFieldId::Host, hostile)],
        ImageSourceScope::Repo,
        false,
    )]);
    resolve_local(&mut session, &store, &flags(), &mut frontend);
    let errors = frontend.texts(MessageLevel::Error);
    assert_eq!(errors.len(), 1, "{:?}", frontend.messages);
    assert!(errors[0].contains("not saved"), "{}", errors[0]);
    for message in &frontend.messages {
        assert!(
            !message.text.chars().any(char::is_control),
            "{:?}",
            message.text
        );
    }
    assert_eq!(bytes(&repo_path(&s)), None);
    assert_eq!(bytes(&global_path(&s)), None);
    s.finish();
}

/// Review finding 9: the builder opt-in follows the endpoint the person
/// selects, not the ambient DOCKER_HOST seen before they chose one.
#[cfg(unix)]
#[test]
fn builder_opt_in_follows_the_selected_docker_endpoint() {
    let s = sandbox!("builder_opt_in_follows_the_selected_docker_endpoint");
    let session = session(&s, true);
    let config = session.effective_config();
    let subject = ImageSubject {
        agent: "claude".into(),
        tag: tag(&s, "claude"),
        is_default: true,
        installed: false,
    };
    let status = config.image_source_status(&subject.tag, true);
    let scopes =
        image_source_scopes(&config, Some(&s.work()), &status, &subject.tag, true).unwrap();
    let request = build_choice_request(&config, subject, &status, vec![], &scopes).unwrap();
    let docker = request
        .choices
        .iter()
        .find(|c| c.kind == ImageSourceKind::DockerStore)
        .unwrap();
    assert!(
        docker.builder_offer.is_some(),
        "offered before the endpoint is chosen"
    );
    let selection = |host: Option<&str>| ImageSourceSelection {
        kind: ImageSourceKind::DockerStore,
        fields: host
            .map(|h| (ImageSourceFieldId::Host, h.to_string()))
            .into_iter()
            .collect(),
        scope: ImageSourceScope::Global,
        enable_builder: true,
    };
    let chosen = |host: Option<&str>, ambient: Option<&str>| {
        spec_from_selection(&selection(host), &request, ambient, None, &s.work()).unwrap()
    };
    let remote = Some("tcp://remote:2375");
    // Ambient endpoint remote, entered endpoint a local socket: builds.
    let local = chosen(Some("unix:///run/user/1000/docker.sock"), remote);
    assert!(local.enable_builder);
    assert!(local.notes.is_empty());
    // Blank host falls back to the remote ambient endpoint: refused, explained.
    let ambient = chosen(None, remote);
    assert!(!ambient.enable_builder);
    assert!(ambient
        .notes
        .iter()
        .any(|n| n.contains("builder was not enabled")));
    // Ambient local, entered TCP: refused.
    let tcp = chosen(
        Some("tcp://docker.example:2375"),
        Some("unix:///var/run/docker.sock"),
    );
    assert!(!tcp.enable_builder);
    // Blank host and no DOCKER_HOST: the local default socket builds.
    assert!(chosen(None, None).enable_builder);
    s.finish();
}

#[test]
fn local_source_errors_are_final_before_formatting_and_remote_retries_stay_typed() {
    use crate::engine::error::{EngineError, ImageRetryClass};
    use crate::engine::oci::docker_engine::LocalDockerUnavailable;
    for local in [
        LocalDockerUnavailable::Stopped {
            endpoint: "unix:///owned/docker.sock".into(),
            default_socket: false,
        },
        LocalDockerUnavailable::PermissionDenied {
            endpoint: "unix:///owned/docker.sock".into(),
        },
        LocalDockerUnavailable::InvalidSocket {
            endpoint: "unix:///owned/docker.sock".into(),
        },
    ] {
        let expected = local.to_string();
        let error = EngineError::io("/owned/docker.sock", std::io::Error::other(local));
        assert!(!crate::engine::oci::retry::is_retryable(&error));
        assert_eq!(super::finish_ready_error(error).to_string(), expected);
    }
    let error = EngineError::ImageSourceFailure {
        retry: ImageRetryClass::Transient,
        diagnostic: "remote transfer interrupted".into(),
    };
    let crate::command::error::CommandError::Engine(error) = super::finish_ready_error(error)
    else {
        panic!("remote failure lost its type")
    };
    assert!(crate::engine::oci::retry::is_retryable(&error));
}
