//! Layer 2 policy for the interactive builtin image-source choice (WI 0158).
//!
//! `ReadyCommand` calls [`offer_image_source`] once, before the
//! `ReadyEngine` is built. It classifies the images this run prepares from
//! configuration alone, asks the frontend at most once per stage, gathers the
//! selection into an [`ImageSourceSpec`], and lets Layer 0 validate, preview
//! and write it. After a successful write the session is reloaded so the
//! engine is built from the new settings in the same invocation. Every other
//! outcome leaves configuration untouched and `ready` continues with its
//! ordinary missing-source result.

use std::path::{Path, PathBuf};

use crate::command::commands::ready::ReadyCommandFlags;
use crate::command::error::CommandError;
use crate::command::headless::HeadlessDefaults;
use crate::data::config::effective::{EffectiveConfig, Layer};
use crate::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
use crate::data::config::image_source_prompt::{
    ImageSourceAnswer, ImageSourceChoice, ImageSourceChoiceRequest, ImageSourceField,
    ImageSourceFieldId, ImageSourcePrompt, ImageSourceScope, ImageSourceSelection, ImageSubject,
    MissingSourceReason,
};
use crate::data::config::image_source_setup::{
    apply_image_source_plan, image_source_scopes, plan_image_source_edit, slot_for,
    ImageSourceEdit, ImageSourceScopes, ImageSourceStatus, PreviewSubject,
};
use crate::data::message::{MessageLevel, UserMessage};
use crate::data::session::{AgentName, Session};
use crate::engine::ready::ReadyFrontend;

/// Whether this invocation may ask a person and write configuration:
/// only a local user's own CLI/TUI session, never `--headless`, `--json` or
/// an API caller (which always has headless defaults).
pub(super) fn prompt_allowed(
    flags: &ReadyCommandFlags,
    headless: Option<&HeadlessDefaults>,
    local_user: bool,
) -> bool {
    local_user && headless.is_none() && !flags.non_interactive && !flags.json
}

/// One image this `ready` run will prepare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PreparedImage {
    pub agent: String,
    pub tag: String,
    pub is_default: bool,
}

/// The requested agent's image, then (only with `--build`) every other agent
/// that has a `.awman/Dockerfile.<agent>`, in discovery order: the same set
/// and order `ReadyEngine` prepares.
///
/// `is_default` is whether the image belongs to the session's *configured*
/// agent (`agents.default`, else the fallback), not the one this invocation
/// names with `--agent`: a pinned `imageSource` was set up for the configured
/// agent, so `ready --agent <other>` must not treat it as that agent's source.
/// Workflow readiness classifies against the same agent.
pub(super) fn prepared_images(
    session: &Session,
    agent: &AgentName,
    build: bool,
) -> Vec<PreparedImage> {
    let git_root = session.git_root();
    let tag = |name: &str| crate::data::image_tags::agent_image_tag(git_root, name);
    let configured = session.configured_agent();
    let mut images = vec![PreparedImage {
        agent: agent.as_str().to_string(),
        tag: tag(agent.as_str()),
        is_default: agent == &configured,
    }];
    if build {
        let others = crate::data::repo_dockerfile_paths::RepoDockerfilePaths::new(git_root)
            .discover_agent_dockerfiles();
        for (name, _) in others {
            if name != agent.as_str() {
                images.push(PreparedImage {
                    tag: tag(&name),
                    is_default: name == configured.as_str(),
                    agent: name,
                });
            }
        }
    }
    images
}

const BUILDER_OFFER: &str = "Also let awman build this image when it is missing? This sets \
runtime.builtin.builder.enabled to true; awman ready then builds into the selected store before \
importing.";

const DOCKER_BUILDER_OFFER: &str = "Also let awman build this image when it is missing? This sets \
runtime.builtin.builder.enabled to true; awman ready then builds into the selected store before \
importing. Only a local unix:// Docker socket can build: the entered host, or DOCKER_HOST when \
blank.";

const BUILDER_NOT_ENABLED: &str = "The builder was not enabled: it builds only into a local \
Docker socket or the Apple Containers store.";

/// Whether a Docker endpoint string names a local Unix socket (blank means
/// the platform default, which is local on Unix). A string test only: no
/// socket is touched.
fn is_local_docker_endpoint(endpoint: Option<&str>) -> bool {
    cfg!(unix)
        && endpoint
            .map(str::trim)
            .is_none_or(|e| e.is_empty() || e.starts_with("unix://"))
}

fn field(id: ImageSourceFieldId, label: &str, help: String, required: bool) -> ImageSourceField {
    ImageSourceField {
        id,
        label: label.into(),
        help,
        required,
    }
}

/// The offered source kinds, in display order.
fn choices(config: &EffectiveConfig, tag: &str) -> Vec<ImageSourceChoice> {
    let builder_on = config.builtin_runtime().builder_enabled();
    let reference = || {
        field(
            ImageSourceFieldId::Reference,
            "Image reference",
            format!("Blank uses the awman tag {tag}."),
            false,
        )
    };
    let mut choices = vec![ImageSourceChoice {
        kind: ImageSourceKind::DockerStore,
        description:
            "Export an existing image from a Docker Engine (default: the local Docker socket)"
                .into(),
        fields: vec![
            field(
                ImageSourceFieldId::Host,
                "Docker host",
                "Blank uses DOCKER_HOST or the default local socket; otherwise unix://, tcp:// or \
                 npipe://. For a TLS tcp:// host, add runtime.builtin.imageSource.tls by hand."
                    .into(),
                false,
            ),
            reference(),
        ],
        // Offered whenever a local socket could be selected: whether the
        // endpoint can build is decided from the host the person enters (or
        // DOCKER_HOST when blank) in `spec_from_selection`, not here.
        builder_offer: (cfg!(unix) && !builder_on).then(|| DOCKER_BUILDER_OFFER.to_string()),
    }];
    if cfg!(target_os = "macos") {
        choices.push(ImageSourceChoice {
            kind: ImageSourceKind::AppleStore,
            description: "Export an existing image from the Apple Containers image store".into(),
            fields: vec![reference()],
            builder_offer: (!builder_on).then(|| BUILDER_OFFER.to_string()),
        });
    }
    choices.push(ImageSourceChoice {
        kind: ImageSourceKind::Archive,
        description: "Load an image tar written by `docker save` (or an OCI layout tar)".into(),
        fields: vec![field(
            ImageSourceFieldId::Path,
            "Archive path",
            format!("A tar written by `docker save {tag} -o <file>`."),
            true,
        )],
        builder_offer: None,
    });
    choices.push(ImageSourceChoice {
        kind: ImageSourceKind::Registry,
        description: "Pull the image from an OCI registry".into(),
        fields: vec![
            field(
                ImageSourceFieldId::Registry,
                "Registry host",
                "Blank uses the registry named in the reference (Docker Hub when it names none)."
                    .into(),
                false,
            ),
            reference(),
        ],
        builder_offer: None,
    });
    choices
}

/// Build the stage-1 request for `subject`, whose `status` needs a source.
/// `None` when no scope can take the write.
pub(super) fn build_choice_request(
    config: &EffectiveConfig,
    subject: ImageSubject,
    status: &ImageSourceStatus,
    also_missing: Vec<ImageSubject>,
    scopes: &ImageSourceScopes,
) -> Option<ImageSourceChoiceRequest> {
    if scopes.offered.is_empty() {
        return None;
    }
    let reason = match status {
        ImageSourceStatus::DefaultPinned { spec, .. } => {
            MissingSourceReason::DefaultPinned { kind: spec.kind() }
        }
        _ => MissingSourceReason::Unconfigured,
    };
    let tag = subject.tag.clone();
    let mut notes = vec![
        "awman imports builtin images from an explicit source; it does not build them unless \
         the builder is enabled."
            .to_string(),
        "docker-local and apple-local import an image that already exists in that store."
            .to_string(),
    ];
    if subject.installed {
        notes.push(format!(
            "An image for {tag} is already installed and stays usable if you leave this \
             unconfigured."
        ));
    }
    if config.builtin_runtime().builder_enabled() {
        notes.push(
            "The builtin builder is already enabled (runtime.builtin.builder.enabled): a \
             docker-local or apple-local source lets awman ready build a missing image."
                .into(),
        );
    }
    if let MissingSourceReason::DefaultPinned { kind } = &reason {
        notes.push(format!(
            "runtime.builtin.imageSource ({kind}) names a single image, so {tag} needs its own \
             runtime.builtin.images entry; the default source is not changed."
        ));
    }
    let repo_path = scopes
        .offered
        .iter()
        .chain(scopes.shadowed.iter().map(|(option, _)| option))
        .find(|option| option.scope == ImageSourceScope::Repo)
        .map(|option| option.path.clone());
    for (option, winner) in &scopes.shadowed {
        if let (ImageSourceScope::Global, Layer::Repo, Some(repo)) =
            (option.scope, winner, &repo_path)
        {
            notes.push(format!(
                "The global config is not offered: {} defines runtime.builtin.images, which \
                 replaces the global map.",
                repo.display()
            ));
        }
    }
    if !also_missing.is_empty() {
        let tags: Vec<&str> = also_missing.iter().map(|s| s.tag.as_str()).collect();
        notes.push(format!(
            "Also without a source: {}. A default source without a fixed reference covers them \
             too.",
            tags.join(", ")
        ));
    }
    Some(ImageSourceChoiceRequest {
        choices: choices(config, &tag),
        subject,
        also_missing,
        reason,
        scopes: scopes.offered.clone(),
        notes,
    })
}

/// A spec built from a selection, plus the honoured builder answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChosenSource {
    pub spec: ImageSourceSpec,
    pub scope: ImageSourceScope,
    pub enable_builder: bool,
    /// Sentences for the preview (the builder was not enabled, …).
    pub notes: Vec<String>,
}

/// Build the spec from the raw selection. Shape validation is Layer 0's job;
/// this only checks the selection against the request. Errors are
/// user-facing sentences.
pub(super) fn spec_from_selection(
    selection: &ImageSourceSelection,
    request: &ImageSourceChoiceRequest,
    docker_host: Option<&str>,
    home: Option<&Path>,
    working_dir: &Path,
) -> Result<ChosenSource, String> {
    let choice = request
        .choices
        .iter()
        .find(|c| c.kind == selection.kind)
        .ok_or_else(|| "that choice was not offered".to_string())?;
    if !request.scopes.iter().any(|s| s.scope == selection.scope) {
        return Err("that choice was not offered".into());
    }
    let value = |id: ImageSourceFieldId| -> Result<Option<String>, String> {
        let Some(declared) = choice.fields.iter().find(|f| f.id == id) else {
            return Ok(None);
        };
        match selection
            .fields
            .get(&id)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
        {
            Some(v) => Ok(Some(v.to_string())),
            None if declared.required => Err(format!("{} is required", declared.label)),
            None => Ok(None),
        }
    };
    let host = value(ImageSourceFieldId::Host)?;
    let reference = value(ImageSourceFieldId::Reference)?;
    let registry = value(ImageSourceFieldId::Registry)?;
    let path = value(ImageSourceFieldId::Path)?;
    let spec = match selection.kind {
        ImageSourceKind::DockerStore => ImageSourceSpec::DockerStore {
            host: host.clone(),
            tls: None,
            reference,
        },
        ImageSourceKind::AppleStore => ImageSourceSpec::AppleStore { reference },
        ImageSourceKind::Archive => ImageSourceSpec::Archive {
            path: resolve_path(path.as_deref().unwrap_or_default(), home, working_dir),
        },
        ImageSourceKind::Registry => ImageSourceSpec::Registry {
            registry,
            reference,
        },
    };
    // The endpoint this source will actually use: the entered host, else
    // DOCKER_HOST, else the platform default.
    let host_is_local = selection.kind != ImageSourceKind::DockerStore
        || is_local_docker_endpoint(host.as_deref().or(docker_host));
    let enable_builder =
        selection.enable_builder && choice.builder_offer.is_some() && host_is_local;
    let mut notes = Vec::new();
    if selection.enable_builder && !enable_builder {
        notes.push(BUILDER_NOT_ENABLED.to_string());
    }
    Ok(ChosenSource {
        spec,
        scope: selection.scope,
        enable_builder,
        notes,
    })
}

/// `~/` is expanded with `home`; a relative path is joined to `working_dir`.
fn resolve_path(raw: &str, home: Option<&Path>, working_dir: &Path) -> PathBuf {
    let expanded = match (raw.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(raw),
    };
    if expanded.as_os_str().is_empty() || expanded.is_absolute() {
        expanded
    } else {
        working_dir.join(expanded)
    }
}

/// What [`offer_image_source`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SourceSetup {
    /// Nothing to offer, not allowed to ask, or no answer that writes.
    /// Configuration unchanged.
    Unchanged,
    /// Configuration was written and the session reloaded.
    Saved {
        scope: ImageSourceScope,
        path: PathBuf,
        field: String,
    },
}

pub(super) struct SourceSetupContext<'a> {
    pub session: &'a mut Session,
    pub agent: &'a AgentName,
    /// `--build`: non-default agents' images are prepared too.
    pub build: bool,
    /// Whether an image is installed in the runtime; injected so tests need
    /// no runtime.
    pub installed: &'a dyn Fn(&str) -> bool,
}

/// Report one source-setup outcome. These lines interpolate paths, tags and
/// error text, so every control character (an ESC starting a terminal
/// sequence, a line break that would fake a new line) becomes U+FFFD before
/// the message reaches any frontend.
fn info(frontend: &mut dyn ReadyFrontend, level: MessageLevel, text: String) {
    let text = text
        .chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect();
    frontend.write_message(UserMessage { level, text });
}

/// The no-write outcome of a non-writing answer, with its message.
fn no_write(frontend: &mut dyn ReadyFrontend, answer: &ImageSourceAnswer) -> SourceSetup {
    let text = match answer {
        ImageSourceAnswer::LeaveUnconfigured => Some("ready: image source left unconfigured."),
        ImageSourceAnswer::Eof => Some("ready: input ended; configuration unchanged."),
        ImageSourceAnswer::Unavailable => None,
        // `Cancelled`, and an answer that does not belong to the stage.
        _ => Some("ready: image source setup cancelled; configuration unchanged."),
    };
    if let Some(text) = text {
        info(frontend, MessageLevel::Info, text.to_string());
    }
    SourceSetup::Unchanged
}

fn not_saved(frontend: &mut dyn ReadyFrontend, reason: &str) -> SourceSetup {
    info(
        frontend,
        MessageLevel::Error,
        format!("ready: image source not saved: {reason}. Configuration unchanged."),
    );
    SourceSetup::Unchanged
}

/// Classify, ask (once), plan, write, reload. Never returns an error for a
/// declined, cancelled or invalid choice; `Err` only when configuration was
/// written but cannot be reloaded.
pub(super) fn offer_image_source(
    ctx: SourceSetupContext<'_>,
    frontend: &mut dyn ReadyFrontend,
) -> Result<SourceSetup, CommandError> {
    let config = ctx.session.effective_config();
    let images = prepared_images(ctx.session, ctx.agent, ctx.build);
    let mut missing: Vec<(PreparedImage, ImageSourceStatus)> = images
        .iter()
        .map(|image| {
            let status = config.image_source_status(&image.tag, image.is_default);
            (image.clone(), status)
        })
        .filter(|(_, status)| status.needs_source())
        .collect();
    if missing.is_empty() {
        return Ok(SourceSetup::Unchanged);
    }
    let (subject_image, status) = missing.remove(0);
    let as_subject = |image: &PreparedImage| ImageSubject {
        agent: image.agent.clone(),
        tag: image.tag.clone(),
        is_default: image.is_default,
        installed: (ctx.installed)(&image.tag),
    };
    let subject = as_subject(&subject_image);
    let also_missing = missing.iter().map(|(image, _)| as_subject(image)).collect();
    let git_root = ctx
        .session
        .is_git_repo()
        .then(|| ctx.session.git_root().to_path_buf());
    let scopes = match image_source_scopes(
        &config,
        git_root.as_deref(),
        &status,
        &subject.tag,
        subject.is_default,
    ) {
        Ok(scopes) => scopes,
        Err(e) => {
            info(
                frontend,
                MessageLevel::Warning,
                format!(
                    "ready: cannot offer an image source for {}: {e}",
                    subject.tag
                ),
            );
            return Ok(SourceSetup::Unchanged);
        }
    };
    let docker_host = ctx.session.env().get("DOCKER_HOST").map(str::to_string);
    let Some(request) =
        build_choice_request(&config, subject.clone(), &status, also_missing, &scopes)
    else {
        let winners: Vec<String> = scopes
            .shadowed
            .iter()
            .map(|(option, layer)| {
                format!(
                    "{} is overridden by the {} config",
                    option.path.display(),
                    layer.as_str()
                )
            })
            .collect();
        info(
            frontend,
            MessageLevel::Warning,
            format!(
                "ready: not offering to configure an image source for {}: {}.",
                subject.tag,
                winners.join("; ")
            ),
        );
        return Ok(SourceSetup::Unchanged);
    };

    // Stage 1: choose.
    let selection = match frontend.ask_image_source(&ImageSourcePrompt::Choose(request.clone())) {
        ImageSourceAnswer::Selected(selection) => selection,
        other => return Ok(no_write(frontend, &other)),
    };
    let home = ctx.session.env().home_dir();
    let chosen = match spec_from_selection(
        &selection,
        &request,
        docker_host.as_deref(),
        home.as_deref(),
        ctx.session.working_dir(),
    ) {
        Ok(chosen) => chosen,
        Err(reason) => return Ok(not_saved(frontend, &reason)),
    };
    let subjects: Vec<PreviewSubject> = std::iter::once(&subject_image)
        .chain(images.iter().filter(|i| i.tag != subject_image.tag))
        .map(|i| PreviewSubject {
            agent: i.agent.clone(),
            tag: i.tag.clone(),
            is_default: i.is_default,
        })
        .collect();
    let edit = ImageSourceEdit {
        scope: chosen.scope,
        slot: slot_for(&status, &chosen.spec, &subject.tag),
        spec: chosen.spec,
        enable_builder: chosen.enable_builder,
    };
    let mut plan = match plan_image_source_edit(&config, git_root.as_deref(), edit, &subjects) {
        Ok(plan) => plan,
        Err(e) => return Ok(not_saved(frontend, &e.to_string())),
    };
    plan.preview.notes.splice(0..0, chosen.notes);

    // Stage 2: confirm the exact write.
    match frontend.ask_image_source(&ImageSourcePrompt::Confirm(plan.preview.clone())) {
        ImageSourceAnswer::Confirmed => {}
        other => return Ok(no_write(frontend, &other)),
    }
    if let Err(e) = apply_image_source_plan(&plan) {
        info(
            frontend,
            MessageLevel::Error,
            format!("ready: image source not saved: {e}"),
        );
        return Ok(SourceSetup::Unchanged);
    }
    let field = plan.preview.field.clone();
    let path = plan.path().to_path_buf();
    let scope = plan.preview.scope;
    ctx.session.reload_config().map_err(|e| {
        CommandError::Other(format!(
            "saved {field} to {}, but the configuration could not be reloaded: {e}",
            path.display()
        ))
    })?;
    info(
        frontend,
        MessageLevel::Info,
        format!(
            "ready: saved {field} to {} ({} config); continuing.",
            path.display(),
            scope.as_str()
        ),
    );
    Ok(SourceSetup::Saved { scope, path, field })
}
