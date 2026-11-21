//! T-F-01, T-F-03, T-F-06, T-F-07 — the CLI line-mode source prompt, driven
//! with injected input and a captured writer.

use std::collections::VecDeque;
use std::path::PathBuf;

use super::ask;
use crate::data::config::image_source::ImageSourceKind;
use crate::data::config::image_source_prompt::*;

enum Input {
    Line(&'static str),
    Interrupt,
}

/// Feeds scripted lines; once the script is empty the input has ended.
struct Script(VecDeque<Input>);

impl Script {
    fn new(items: impl IntoIterator<Item = Input>) -> Self {
        Self(items.into_iter().collect())
    }
    fn lines(items: &[&'static str]) -> Self {
        Self::new(items.iter().map(|l| Input::Line(l)))
    }
    fn read(&mut self, buf: &mut String) -> std::io::Result<usize> {
        match self.0.pop_front() {
            None => Ok(0),
            Some(Input::Line(line)) => {
                buf.push_str(line);
                buf.push('\n');
                Ok(buf.len())
            }
            Some(Input::Interrupt) => Err(std::io::ErrorKind::Interrupted.into()),
        }
    }
}

fn run(prompt: &ImageSourcePrompt, script: &mut Script) -> (ImageSourceAnswer, String) {
    let mut out = Vec::new();
    let answer = ask(prompt, &mut |buf| script.read(buf), &mut out);
    (answer, String::from_utf8(out).unwrap())
}

fn field(id: ImageSourceFieldId, label: &str, required: bool) -> ImageSourceField {
    ImageSourceField {
        id,
        label: label.into(),
        help: format!("help for {label}"),
        required,
    }
}

fn choice(
    kind: ImageSourceKind,
    fields: Vec<ImageSourceField>,
    builder: bool,
) -> ImageSourceChoice {
    ImageSourceChoice {
        kind,
        description: format!("describe {kind}"),
        fields,
        builder_offer: builder.then(|| "The builder can build missing images locally.".into()),
    }
}

fn request(scopes: usize) -> ImageSourceChoiceRequest {
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

fn preview() -> ImageSourcePreview {
    ImageSourcePreview {
        scope: ImageSourceScope::Repo,
        path: PathBuf::from("/repo/.awman/config.json"),
        field: "runtime.builtin.imageSource".into(),
        fragment: "{\n  \"runtime\": {\n    \"builtin\": {}\n  }\n}".into(),
        effective: vec![EffectiveImageSource {
            agent: "claude".into(),
            tag: "awman-claude:1".into(),
            source: Some(EffectiveSource {
                kind: ImageSourceKind::Archive,
                json: "{}".into(),
                field: "runtime.builtin.imageSource".into(),
                scope: "repo".into(),
                from_this_write: true,
            }),
        }],
        builder: BuilderPreview::Unchanged { effective: false },
        notes: vec!["A note.".into()],
    }
}

#[test]
fn cli_source_choice_returns_each_kind_with_its_fields() {
    let req = ImageSourcePrompt::Choose(request(2));
    // docker-local: host + reference given, scope 2 (global), builder yes.
    let (answer, shown) = run(
        &req,
        &mut Script::lines(&["1", " unix:///x.sock ", "img:1", "2", "y"]),
    );
    assert_eq!(
        answer,
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
    assert!(shown.contains("Import requires an image that already exists."));
    assert!(shown.contains("awman-claude:1"));
    assert!(shown.contains("/home/u/.awman/config.json"));
    assert!(shown.contains("The builder can build missing images locally."));
    assert!(shown.contains("[5] Leave unconfigured"));

    // docker-local with blank optional fields, default scope, builder default (no).
    let (answer, _) = run(&req, &mut Script::lines(&["1", "", "", "", ""]));
    assert_eq!(
        answer,
        selected(
            ImageSourceKind::DockerStore,
            &[],
            ImageSourceScope::Repo,
            false
        )
    );

    let (answer, _) = run(&req, &mut Script::lines(&["2", "ref", ""]));
    assert_eq!(
        answer,
        selected(
            ImageSourceKind::AppleStore,
            &[(ImageSourceFieldId::Reference, "ref")],
            ImageSourceScope::Repo,
            false
        )
    );
    let (answer, _) = run(&req, &mut Script::lines(&["3", "./a.tar", "2"]));
    assert_eq!(
        answer,
        selected(
            ImageSourceKind::Archive,
            &[(ImageSourceFieldId::Path, "./a.tar")],
            ImageSourceScope::Global,
            false
        )
    );
    let (answer, _) = run(&req, &mut Script::lines(&["4", "ghcr.io/o", "1"]));
    assert_eq!(
        answer,
        selected(
            ImageSourceKind::Registry,
            &[(ImageSourceFieldId::Registry, "ghcr.io/o")],
            ImageSourceScope::Repo,
            false
        )
    );
    // Blank required fields are passed through for Layer 2 to refuse.
    let (answer, _) = run(&req, &mut Script::lines(&["3", "", ""]));
    assert_eq!(
        answer,
        selected(ImageSourceKind::Archive, &[], ImageSourceScope::Repo, false)
    );
}

#[test]
fn cli_single_scope_is_shown_not_asked_and_builder_only_when_offered() {
    let req = ImageSourcePrompt::Choose(request(1));
    // Archive: no scope question, no builder question: exactly two lines read.
    let mut script = Script::lines(&["3", "./a.tar"]);
    let (answer, shown) = run(&req, &mut script);
    assert_eq!(
        answer,
        selected(
            ImageSourceKind::Archive,
            &[(ImageSourceFieldId::Path, "./a.tar")],
            ImageSourceScope::Repo,
            false
        )
    );
    assert!(shown.contains("/repo/.awman/config.json"));
    assert!(!shown.contains("Scope ["));
    assert!(!shown.contains("builder"));
}

#[test]
fn cli_leave_unconfigured_is_the_default_and_the_last_choice() {
    let req = ImageSourcePrompt::Choose(request(2));
    for line in ["", "5", "nonsense", "0", "9"] {
        let (answer, _) = run(&req, &mut Script::lines(&[line]));
        assert_eq!(answer, ImageSourceAnswer::LeaveUnconfigured, "{line:?}");
    }
}

#[test]
fn cli_pinned_default_and_other_missing_images_are_explained() {
    let mut req = request(1);
    req.reason = MissingSourceReason::DefaultPinned {
        kind: ImageSourceKind::Archive,
    };
    req.also_missing = vec![ImageSubject {
        agent: "codex".into(),
        tag: "awman-codex:1".into(),
        is_default: false,
        installed: false,
    }];
    let (_, shown) = run(&ImageSourcePrompt::Choose(req), &mut Script::lines(&[""]));
    assert!(shown.contains("pinned to one image (archive)"));
    assert!(shown.contains("awman-codex:1"));
}

#[test]
fn cli_confirm_shows_path_scope_fragment_and_effective_lines() {
    let prompt = ImageSourcePrompt::Confirm(preview());
    let (answer, shown) = run(&prompt, &mut Script::lines(&["y"]));
    assert_eq!(answer, ImageSourceAnswer::Confirmed);
    let p = preview();
    assert!(shown.contains("/repo/.awman/config.json"));
    assert!(shown.contains("repo config"));
    assert!(shown.contains("runtime.builtin.imageSource"));
    assert!(shown.contains("\"builtin\": {}"));
    for line in p.effective_lines() {
        assert!(shown.contains(&line), "missing {line:?}");
    }
    assert!(shown.contains("A note."));

    // Anything but yes (including Enter) declines.
    for line in ["", "n", "maybe"] {
        let (answer, _) = run(&prompt, &mut Script::lines(&[line]));
        assert_eq!(answer, ImageSourceAnswer::LeaveUnconfigured, "{line:?}");
    }
}

#[test]
fn cli_relay_interrupt_maps_to_cancelled() {
    let choose = ImageSourcePrompt::Choose(request(2));
    // At the menu, in a field, at the scope question, and at the builder question.
    for items in [
        vec![Input::Interrupt],
        vec![Input::Line("3"), Input::Interrupt],
        vec![Input::Line("3"), Input::Line("a"), Input::Interrupt],
        vec![
            Input::Line("1"),
            Input::Line(""),
            Input::Line(""),
            Input::Line("1"),
            Input::Interrupt,
        ],
    ] {
        let (answer, _) = run(&choose, &mut Script::new(items));
        assert_eq!(answer, ImageSourceAnswer::Cancelled);
    }
    let (answer, _) = run(
        &ImageSourcePrompt::Confirm(preview()),
        &mut Script::new([Input::Interrupt]),
    );
    assert_eq!(answer, ImageSourceAnswer::Cancelled);
}

#[test]
fn cli_closed_stdin_maps_to_eof() {
    let choose = ImageSourcePrompt::Choose(request(2));
    for lines in [&[][..], &["3"][..], &["3", "a"][..]] {
        let (answer, _) = run(&choose, &mut Script::lines(lines));
        assert_eq!(answer, ImageSourceAnswer::Eof, "{lines:?}");
    }
    let (answer, _) = run(
        &ImageSourcePrompt::Confirm(preview()),
        &mut Script::lines(&[]),
    );
    assert_eq!(answer, ImageSourceAnswer::Eof);
}

#[test]
fn cli_read_failure_other_than_interrupt_is_eof() {
    let mut out = Vec::new();
    let answer = ask(
        &ImageSourcePrompt::Confirm(preview()),
        &mut |_| Err(std::io::Error::other("broken pipe")),
        &mut out,
    );
    assert_eq!(answer, ImageSourceAnswer::Eof);
}

/// Terminal controls a hostile checkout name could carry: a CSI screen
/// clear, an OSC 52 clipboard write, and a newline faking a line of output.
const HOSTILE: &str = "/r/x\u{1b}[2J\u{1b}]52;c;aGk=\u{7}\nawman: saved; continuing.";

fn assert_terminal_safe(out: &str) {
    for control in ['\u{1b}', '\u{7}', '\u{9b}', '\u{9d}'] {
        assert!(
            !out.contains(control),
            "{control:?} reached the terminal: {out:?}"
        );
    }
    assert!(
        !out.lines().any(|l| l.starts_with("awman: saved")),
        "a value started its own line: {out:?}"
    );
}

/// Review finding 2: request and preview values are sanitized before they
/// reach stderr; typed answers are returned unchanged.
#[test]
fn cli_prompt_never_prints_terminal_controls_from_the_request() {
    let mut request = request(2);
    request.subject.tag = format!("tag{HOSTILE}");
    request.notes = vec![HOSTILE.into()];
    for scope in &mut request.scopes {
        scope.path = PathBuf::from(HOSTILE);
    }
    request.choices[2].fields[0].help = HOSTILE.into();
    let (answer, out) = run(
        &ImageSourcePrompt::Choose(request.clone()),
        &mut Script::lines(&["3", "./a.tar", "2"]),
    );
    assert_terminal_safe(&out);
    assert_eq!(
        answer,
        selected(
            ImageSourceKind::Archive,
            &[(ImageSourceFieldId::Path, "./a.tar")],
            ImageSourceScope::Global,
            false
        )
    );
    // Single scope: the destination line is printed instead of a menu.
    request.scopes.truncate(1);
    let (_, out) = run(
        &ImageSourcePrompt::Choose(request),
        &mut Script::lines(&[""]),
    );
    assert_terminal_safe(&out);

    let mut preview = preview();
    preview.path = PathBuf::from(HOSTILE);
    preview.notes = vec![HOSTILE.into()];
    preview.field = HOSTILE.into();
    let (answer, out) = run(
        &ImageSourcePrompt::Confirm(preview),
        &mut Script::lines(&["y"]),
    );
    assert_terminal_safe(&out);
    assert_eq!(answer, ImageSourceAnswer::Confirmed);
}
