//! T-F-02, T-F-04, T-F-05 — the TUI / relayed-CLI source dialogs, plus the
//! adapter on `InstanceCommandFrontend`.

use std::collections::VecDeque;
use std::path::PathBuf;

use super::{ask_prompt, confirm_body, context_lines, DONT_SAVE_KEY, SAVE_KEY};
use crate::command::instance::feeds::TabCommandFeeds;
use crate::command::instance::frontend::{DialogChannels, InstanceCommandFrontend};
use crate::data::config::image_source::ImageSourceKind;
use crate::data::config::image_source_prompt::*;
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::engine::ready::frontend::ReadyFrontend;

fn scripted(
    responses: impl IntoIterator<Item = DialogResponse>,
) -> (
    impl FnMut(DialogRequest) -> Option<DialogResponse>,
    std::rc::Rc<std::cell::RefCell<Vec<DialogRequest>>>,
) {
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let log = seen.clone();
    let mut queue: VecDeque<DialogResponse> = responses.into_iter().collect();
    (
        move |request| {
            log.borrow_mut().push(request);
            queue.pop_front()
        },
        seen,
    )
}

fn field(id: ImageSourceFieldId, label: &str, required: bool) -> ImageSourceField {
    ImageSourceField {
        id,
        label: label.into(),
        help: format!("help for {label}"),
        required,
    }
}

fn request(scopes: usize) -> ImageSourceChoiceRequest {
    let choice = |kind, fields, builder: bool| ImageSourceChoice {
        kind,
        description: format!("describe {kind}"),
        fields,
        builder_offer: builder.then(|| "The builder can build missing images.".into()),
    };
    let all = [
        ImageSourceScopeOption {
            scope: ImageSourceScope::Repo,
            path: PathBuf::from("/repo/.awman/config.json"),
        },
        ImageSourceScopeOption {
            scope: ImageSourceScope::Global,
            path: PathBuf::from("/home/u/.awman/config.json"),
        },
    ];
    ImageSourceChoiceRequest {
        subject: ImageSubject {
            agent: "claude".into(),
            tag: "awman-claude:1".into(),
            is_default: true,
            installed: false,
        },
        also_missing: vec![],
        reason: MissingSourceReason::Unconfigured,
        choices: vec![
            choice(
                ImageSourceKind::DockerStore,
                vec![
                    field(ImageSourceFieldId::Host, "Docker host", false),
                    field(ImageSourceFieldId::Reference, "Image reference", false),
                ],
                true,
            ),
            choice(
                ImageSourceKind::AppleStore,
                vec![field(
                    ImageSourceFieldId::Reference,
                    "Image reference",
                    false,
                )],
                false,
            ),
            choice(
                ImageSourceKind::Archive,
                vec![field(ImageSourceFieldId::Path, "Archive path", true)],
                false,
            ),
            choice(
                ImageSourceKind::Registry,
                vec![field(ImageSourceFieldId::Registry, "Registry", true)],
                false,
            ),
        ],
        scopes: all[..scopes].to_vec(),
        notes: vec!["Import requires an image that already exists.".into()],
    }
}

fn preview() -> ImageSourcePreview {
    ImageSourcePreview {
        scope: ImageSourceScope::Global,
        path: PathBuf::from("/home/u/.awman/config.json"),
        field: "runtime.builtin.imageSource".into(),
        fragment: "{\n  \"runtime\": {}\n}".into(),
        effective: vec![EffectiveImageSource {
            agent: "claude".into(),
            tag: "awman-claude:1".into(),
            source: None,
        }],
        builder: BuilderPreview::Enabled,
        notes: vec!["A note.".into()],
    }
}

fn selected(
    kind: ImageSourceKind,
    fields: &[(ImageSourceFieldId, &str)],
    scope: ImageSourceScope,
    enable_builder: bool,
) -> ImageSourceAnswer {
    ImageSourceAnswer::Selected(ImageSourceSelection {
        kind,
        fields: fields.iter().map(|(k, v)| (*k, v.to_string())).collect(),
        scope,
        enable_builder,
    })
}

fn text(s: &str) -> DialogResponse {
    DialogResponse::Text(s.into())
}

#[test]
fn tui_source_choice_returns_each_kind_with_its_fields() {
    let prompt = ImageSourcePrompt::Choose(request(2));

    // docker-local: both fields, global scope, builder yes.
    let (mut ask, seen) = scripted([
        DialogResponse::Index(1),
        text(" unix:///x.sock "),
        text("img:1"),
        DialogResponse::Index(1),
        DialogResponse::Yes,
    ]);
    assert_eq!(
        ask_prompt(&prompt, &mut ask),
        selected(
            ImageSourceKind::DockerStore,
            &[
                (ImageSourceFieldId::Host, "unix:///x.sock"),
                (ImageSourceFieldId::Reference, "img:1")
            ],
            ImageSourceScope::Global,
            true
        )
    );
    let seen = seen.borrow();
    // Menu: "Leave unconfigured" first (the Enter default), then every kind.
    let DialogRequest::ListPicker { items, .. } = &seen[0] else {
        panic!("first dialog is the kind picker");
    };
    assert_eq!(items.len(), 5);
    assert_eq!(items[0], "Leave unconfigured");
    assert!(items[1].starts_with("docker-local"));
    // Scope picker shows scope and path; builder uses the offer text.
    let DialogRequest::ListPicker { items, .. } = &seen[3] else {
        panic!("scope picker");
    };
    assert!(items[1].contains("global") && items[1].contains("/home/u/.awman/config.json"));
    let DialogRequest::YesNo { body, .. } = &seen[4] else {
        panic!("builder question");
    };
    assert_eq!(body, "The builder can build missing images.");
    drop(seen);

    // apple-local, blank optional field omitted, builder not offered.
    let (mut ask, seen) = scripted([DialogResponse::Index(2), text(""), DialogResponse::Index(0)]);
    assert_eq!(
        ask_prompt(&prompt, &mut ask),
        selected(
            ImageSourceKind::AppleStore,
            &[],
            ImageSourceScope::Repo,
            false
        )
    );
    assert_eq!(seen.borrow().len(), 3);

    // archive and registry.
    let (mut ask, _) = scripted([
        DialogResponse::Index(3),
        text("./a.tar"),
        DialogResponse::Index(1),
    ]);
    assert_eq!(
        ask_prompt(&prompt, &mut ask),
        selected(
            ImageSourceKind::Archive,
            &[(ImageSourceFieldId::Path, "./a.tar")],
            ImageSourceScope::Global,
            false
        )
    );
    let (mut ask, _) = scripted([
        DialogResponse::Index(4),
        text("ghcr.io/o"),
        DialogResponse::Index(0),
    ]);
    assert_eq!(
        ask_prompt(&prompt, &mut ask),
        selected(
            ImageSourceKind::Registry,
            &[(ImageSourceFieldId::Registry, "ghcr.io/o")],
            ImageSourceScope::Repo,
            false
        )
    );
    // A blank required field is passed through for Layer 2 to refuse.
    let (mut ask, _) = scripted([DialogResponse::Index(3), text(""), DialogResponse::Index(0)]);
    assert_eq!(
        ask_prompt(&prompt, &mut ask),
        selected(ImageSourceKind::Archive, &[], ImageSourceScope::Repo, false)
    );
}

#[test]
fn tui_single_scope_is_not_asked_and_no_builder_is_a_normal_answer() {
    let prompt = ImageSourcePrompt::Choose(request(1));
    let (mut ask, seen) = scripted([
        DialogResponse::Index(1),
        text(""),
        text(""),
        DialogResponse::No,
    ]);
    assert_eq!(
        ask_prompt(&prompt, &mut ask),
        selected(
            ImageSourceKind::DockerStore,
            &[],
            ImageSourceScope::Repo,
            false
        )
    );
    // picker, two fields, builder: no scope dialog.
    assert_eq!(seen.borrow().len(), 4);
    let lines = context_lines(&request(1));
    assert!(lines.iter().any(|l| l.contains("/repo/.awman/config.json")));
    assert!(lines
        .iter()
        .any(|l| l == "Import requires an image that already exists."));
    assert!(lines.iter().any(|l| l.contains("awman-claude:1")));
}

#[test]
fn tui_leave_unconfigured_is_a_choice_and_returns_the_typed_answer() {
    let prompt = ImageSourcePrompt::Choose(request(2));
    let (mut ask, seen) = scripted([DialogResponse::Index(0)]);
    assert_eq!(
        ask_prompt(&prompt, &mut ask),
        ImageSourceAnswer::LeaveUnconfigured
    );
    assert_eq!(seen.borrow().len(), 1);
}

#[test]
fn tui_confirm_dialog_body_contains_the_preview() {
    let p = preview();
    let body = confirm_body(&p);
    assert!(body.contains("/home/u/.awman/config.json"));
    assert!(body.contains("global config"));
    assert!(body.contains("runtime.builtin.imageSource"));
    assert!(body.contains(&p.fragment));
    for line in p.effective_lines() {
        assert!(body.contains(&line), "missing {line:?}");
    }
    assert!(body.contains("A note."));

    let (mut ask, seen) = scripted([DialogResponse::Char(SAVE_KEY)]);
    assert_eq!(
        ask_prompt(&ImageSourcePrompt::Confirm(p.clone()), &mut ask),
        ImageSourceAnswer::Confirmed
    );
    let DialogRequest::Review(review) = &seen.borrow()[0] else {
        panic!("confirm is a scrolling review dialog");
    };
    assert_eq!(
        review.keys.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
        vec![SAVE_KEY, DONT_SAVE_KEY]
    );
    let (mut ask, _) = scripted([DialogResponse::Char(DONT_SAVE_KEY)]);
    assert_eq!(
        ask_prompt(&ImageSourcePrompt::Confirm(p), &mut ask),
        ImageSourceAnswer::LeaveUnconfigured
    );
}

#[test]
fn tui_escape_maps_to_cancelled() {
    let choose = ImageSourcePrompt::Choose(request(2));
    for first in [
        DialogResponse::Cancel,
        DialogResponse::Dismissed,
        DialogResponse::Interrupted,
        DialogResponse::Yes,
    ] {
        let (mut ask, _) = scripted([first]);
        assert_eq!(ask_prompt(&choose, &mut ask), ImageSourceAnswer::Cancelled);
    }
    // Esc in a field, at the scope picker, at the builder question.
    let (mut ask, _) = scripted([DialogResponse::Index(3), DialogResponse::Cancel]);
    assert_eq!(ask_prompt(&choose, &mut ask), ImageSourceAnswer::Cancelled);
    let (mut ask, _) = scripted([
        DialogResponse::Index(3),
        text("a"),
        DialogResponse::Dismissed,
    ]);
    assert_eq!(ask_prompt(&choose, &mut ask), ImageSourceAnswer::Cancelled);
    let (mut ask, _) = scripted([
        DialogResponse::Index(1),
        text(""),
        text(""),
        DialogResponse::Index(0),
        DialogResponse::Cancel,
    ]);
    assert_eq!(ask_prompt(&choose, &mut ask), ImageSourceAnswer::Cancelled);
    // A failed dialog (channel closed) is a cancel, as is Esc on the preview.
    let (mut ask, _) = scripted([]);
    assert_eq!(ask_prompt(&choose, &mut ask), ImageSourceAnswer::Cancelled);
    let (mut ask, _) = scripted([DialogResponse::Dismissed]);
    assert_eq!(
        ask_prompt(&ImageSourcePrompt::Confirm(preview()), &mut ask),
        ImageSourceAnswer::Cancelled
    );
}

// ─── Adapter on InstanceCommandFrontend ───────────────────────────────────

struct Harness {
    frontend: InstanceCommandFrontend,
    requests: std::sync::mpsc::Receiver<DialogRequest>,
    responses: std::sync::mpsc::Sender<DialogResponse>,
    status_log: crate::command::instance::feeds::SharedStatusLog,
}

fn harness(command: &str) -> Harness {
    let shared = TabCommandFeeds::new();
    let status_log = shared.status_log.clone();
    let (request_tx, requests) = std::sync::mpsc::channel();
    let (responses, response_rx) = std::sync::mpsc::channel();
    let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let frontend = InstanceCommandFrontend::new(
        crate::command::dispatch::parsed_input::parse(
            command,
            crate::command::dispatch::catalogue::CommandCatalogue::get(),
        )
        .unwrap(),
        DialogChannels::new(request_tx, response_rx),
        crate::engine::agent_runtime::AgentIo {
            stdout: stdout.clone(),
            stderr: stdout,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        },
        shared,
    );
    Harness {
        frontend,
        requests,
        responses,
        status_log,
    }
}

#[test]
fn tui_adapter_returns_the_answer_unchanged_and_logs_the_context() {
    let mut h = harness("ready");
    for response in [
        DialogResponse::Index(3),
        text("./a.tar"),
        DialogResponse::Index(1),
    ] {
        h.responses.send(response).unwrap();
    }
    let prompt = ImageSourcePrompt::Choose(request(2));
    let answer = ReadyFrontend::ask_image_source(&mut h.frontend, &prompt);
    assert_eq!(
        answer,
        selected(
            ImageSourceKind::Archive,
            &[(ImageSourceFieldId::Path, "./a.tar")],
            ImageSourceScope::Global,
            false
        )
    );
    assert_eq!(h.requests.try_iter().count(), 3);
    let log = h.status_log.lock().unwrap();
    assert!(log.iter().any(|l| l.text.contains("awman-claude:1")));
    assert!(log
        .iter()
        .any(|l| l.text == "Import requires an image that already exists."));
    drop(log);

    // Stage 2 through the same adapter.
    h.responses.send(DialogResponse::Char(SAVE_KEY)).unwrap();
    assert_eq!(
        ReadyFrontend::ask_image_source(&mut h.frontend, &ImageSourcePrompt::Confirm(preview())),
        ImageSourceAnswer::Confirmed
    );
}

#[test]
fn tui_adapter_cancels_when_the_command_is_cancelled() {
    let shared = TabCommandFeeds::new();
    let cancel = shared.command_cancel.clone();
    let mut h = harness("ready");
    h.frontend.command_cancel = cancel.clone();
    cancel.cancel();
    assert_eq!(
        ReadyFrontend::ask_image_source(&mut h.frontend, &ImageSourcePrompt::Choose(request(2))),
        ImageSourceAnswer::Cancelled
    );
}

#[test]
fn relayed_headless_cli_never_opens_a_dialog() {
    let mut h = harness("ready --headless");
    h.frontend = h
        .frontend
        .with_kind(crate::command::dispatch::catalogue::FrontendKind::Cli);
    assert_eq!(
        ReadyFrontend::ask_image_source(&mut h.frontend, &ImageSourcePrompt::Choose(request(2))),
        ImageSourceAnswer::Unavailable
    );
    assert_eq!(h.requests.try_iter().count(), 0);
    assert!(h.status_log.lock().unwrap().is_empty());
}

const HOSTILE: &str = "/r/x\u{1b}[2J\u{1b}]52;c;aGk=\u{7}\nawman: saved; continuing.";

/// Every human-visible string of a dialog request.
fn dialog_texts(request: &DialogRequest) -> Vec<String> {
    match request {
        DialogRequest::ListPicker { title, items } => {
            let mut texts = vec![title.clone()];
            texts.extend(items.iter().cloned());
            texts
        }
        DialogRequest::TextInput { title, prompt, .. } => vec![title.clone(), prompt.clone()],
        DialogRequest::YesNo { title, body } => vec![title.clone(), body.clone()],
        DialogRequest::Review(review) => {
            let body = match &review.body {
                crate::data::dialog::ReviewBody::Exact(text)
                | crate::data::dialog::ReviewBody::Prose(text) => text.clone(),
                other => panic!("unexpected review body {other:?}"),
            };
            vec![review.title.clone(), review.note.clone(), body]
        }
        other => panic!("unexpected dialog {other:?}"),
    }
}

fn assert_no_controls(text: &str) {
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "control character in {text:?}"
    );
    assert!(
        !text.lines().any(|l| l.starts_with("awman: saved")),
        "a value started its own line: {text:?}"
    );
}

/// Review finding 2: the status-log context and every dialog sent to the TUI
/// or over the relay carry no terminal controls from the request.
#[test]
fn dialogs_and_context_never_carry_terminal_controls() {
    let mut h = harness("ready");
    let mut request = request(2);
    request.subject.tag = format!("tag{HOSTILE}");
    request.notes = vec![HOSTILE.into()];
    request.choices[2].fields[0].help = HOSTILE.into();
    request.choices[0].builder_offer = Some(HOSTILE.into());
    for scope in &mut request.scopes {
        scope.path = PathBuf::from(HOSTILE);
    }
    for response in [
        DialogResponse::Index(1),
        text(""),
        text(""),
        DialogResponse::Index(0),
        DialogResponse::No,
    ] {
        h.responses.send(response).unwrap();
    }
    let answer = ReadyFrontend::ask_image_source(
        &mut h.frontend,
        &ImageSourcePrompt::Choose(request.clone()),
    );
    assert!(
        matches!(answer, ImageSourceAnswer::Selected(_)),
        "{answer:?}"
    );
    let sent: Vec<DialogRequest> = h.requests.try_iter().collect();
    assert_eq!(sent.len(), 5);
    for dialog in &sent {
        for text in dialog_texts(dialog) {
            assert_no_controls(&text);
        }
    }
    for line in h.status_log.lock().unwrap().iter() {
        assert_no_controls(&line.text);
        assert!(!line.text.contains('\n'), "{:?}", line.text);
    }
    // The archive field's hostile help text reaches its dialog cleaned.
    request.scopes.truncate(1);
    let (mut ask, seen) = scripted([DialogResponse::Index(3), text("./a.tar")]);
    ask_prompt(&ImageSourcePrompt::Choose(request), &mut ask);
    for dialog in seen.borrow().iter() {
        for text in dialog_texts(dialog) {
            assert_no_controls(&text);
        }
    }

    let mut preview = preview();
    preview.path = PathBuf::from(HOSTILE);
    preview.field = HOSTILE.into();
    preview.notes = vec![HOSTILE.into()];
    let (mut ask, seen) = scripted([DialogResponse::Char(SAVE_KEY)]);
    assert_eq!(
        ask_prompt(&ImageSourcePrompt::Confirm(preview), &mut ask),
        ImageSourceAnswer::Confirmed
    );
    for text in dialog_texts(&seen.borrow()[0]) {
        assert_no_controls(&text);
    }
}

/// Review finding 8: the picker's first entry is "Leave unconfigured", so the
/// TUI's initial selection and the relayed CLI's Enter (item 1) leave the
/// source unconfigured instead of picking docker-local.
#[test]
fn first_enter_on_the_source_picker_leaves_the_source_unconfigured() {
    let prompt = ImageSourcePrompt::Choose(request(2));
    // What the TUI answers for Enter on its initial selection, and the relay
    // for an empty line (`pick_numbered(.., 1)` → item 1 → index 0).
    let (mut ask, seen) = scripted([DialogResponse::Index(0)]);
    assert_eq!(
        ask_prompt(&prompt, &mut ask),
        ImageSourceAnswer::LeaveUnconfigured
    );
    let DialogRequest::ListPicker { items, .. } = &seen.borrow()[0] else {
        panic!("kind picker");
    };
    assert_eq!(items[0], "Leave unconfigured");
}
