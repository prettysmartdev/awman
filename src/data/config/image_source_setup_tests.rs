//! Layer 0 contracts for the builtin image-source choice: classification,
//! slot and scope selection, preview, and the atomic write. Every file lives
//! in a private sandbox; nothing contacts Docker or a registry.
use super::*;
use crate::data::config::effective::ConfigLayers;
use crate::data::config::env::EnvSnapshot;
use crate::data::config::flags::FlagConfig;
use crate::data::config::image_source::ImageSourceKind;
use awman_test_support::{TestCase, TestSandbox};

macro_rules! sandbox {
    ($name:literal) => {
        TestSandbox::for_test(TestCase::component(concat!(
            "data::config::image_source_setup::tests::",
            $name
        )))
    };
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

fn global_path(s: &TestSandbox) -> PathBuf {
    global_config_path(&env(s)).unwrap()
}

fn repo_path(s: &TestSandbox) -> PathBuf {
    repo_config_path(&s.work())
}

fn write(path: &Path, json: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json).unwrap();
}

fn config(s: &TestSandbox) -> EffectiveConfig {
    let env = env(s);
    let layers = ConfigLayers::load(&env, Some(&s.work()), Default::default()).unwrap();
    EffectiveConfig::resolve(layers, FlagConfig::default(), env).unwrap()
}

fn docker() -> ImageSourceSpec {
    ImageSourceSpec::DockerStore {
        host: None,
        tls: None,
        reference: None,
    }
}

fn archive(path: &str) -> ImageSourceSpec {
    ImageSourceSpec::Archive { path: path.into() }
}

const CLAUDE: &str = "awman-app-claude:latest";
const CODEX: &str = "awman-app-codex:latest";

fn subjects() -> Vec<PreviewSubject> {
    vec![
        PreviewSubject {
            agent: "claude".into(),
            tag: CLAUDE.into(),
            is_default: true,
        },
        PreviewSubject {
            agent: "codex".into(),
            tag: CODEX.into(),
            is_default: false,
        },
    ]
}

/// Recursive object merge: `patch` keys win, objects merge key by key.
fn deep_merge(base: &mut serde_json::Value, patch: &serde_json::Value) {
    match (base, patch) {
        (serde_json::Value::Object(base), serde_json::Value::Object(patch)) => {
            for (key, value) in patch {
                deep_merge(
                    base.entry(key.clone()).or_insert(serde_json::Value::Null),
                    value,
                );
            }
        }
        (base, patch) => *base = patch.clone(),
    }
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn status_orders_images_entry_over_default_and_repo_over_global() {
    let s = sandbox!("status_orders_images_entry_over_default_and_repo_over_global");
    assert_eq!(
        config(&s).image_source_status(CLAUDE, true),
        ImageSourceStatus::Unconfigured
    );
    write(
        &global_path(&s),
        r#"{"runtime":{"builtin":{"imageSource":{"type":"registry"}}}}"#,
    );
    assert_eq!(
        config(&s).image_source_status(CLAUDE, true),
        ImageSourceStatus::Configured {
            slot: ImageSourceSlot::Default,
            layer: Layer::Global,
            spec: ImageSourceSpec::Registry {
                registry: None,
                reference: None
            },
        }
    );
    // Repo wins over global for the same field.
    write(
        &repo_path(&s),
        r#"{"runtime":{"builtin":{"imageSource":{"type":"docker-local"}}}}"#,
    );
    assert_eq!(
        config(&s).image_source_status(CLAUDE, true),
        ImageSourceStatus::Configured {
            slot: ImageSourceSlot::Default,
            layer: Layer::Repo,
            spec: docker(),
        }
    );
    // A matching images entry wins over any default, even a closer one.
    write(
        &global_path(&s),
        &format!(
            r#"{{"runtime":{{"builtin":{{"images":{{"{CLAUDE}":{{"type":"archive","path":"/a.tar"}}}}}}}}}}"#
        ),
    );
    let status = config(&s).image_source_status(CLAUDE, true);
    assert_eq!(
        status,
        ImageSourceStatus::Configured {
            slot: ImageSourceSlot::Image(CLAUDE.into()),
            layer: Layer::Global,
            spec: archive("/a.tar"),
        }
    );
    assert!(!status.needs_source());
    // A configured source that points nowhere is still configured.
    assert_eq!(
        config(&s).image_source_status(CODEX, false),
        ImageSourceStatus::Configured {
            slot: ImageSourceSlot::Default,
            layer: Layer::Repo,
            spec: docker(),
        }
    );
    s.finish();
}

#[test]
fn pinned_default_is_missing_only_for_non_default_agents() {
    let s = sandbox!("pinned_default_is_missing_only_for_non_default_agents");
    for (json, kind) in [
        (
            r#"{"runtime":{"builtin":{"imageSource":{"type":"archive","path":"/one.tar"}}}}"#,
            ImageSourceKind::Archive,
        ),
        (
            r#"{"runtime":{"builtin":{"imageSource":{"type":"docker-local","reference":"mine:1"}}}}"#,
            ImageSourceKind::DockerStore,
        ),
    ] {
        write(&global_path(&s), json);
        let config = config(&s);
        let default = config.image_source_status(CLAUDE, true);
        assert!(!default.needs_source(), "{default:?}");
        let other = config.image_source_status(CODEX, false);
        assert!(other.needs_source(), "{other:?}");
        assert!(
            matches!(&other, ImageSourceStatus::DefaultPinned { layer: Layer::Global, spec } if spec.kind() == kind),
            "{other:?}"
        );
    }
    // A default that follows the tag serves every agent.
    write(
        &global_path(&s),
        r#"{"runtime":{"builtin":{"imageSource":{"type":"docker-local"}}}}"#,
    );
    assert!(!config(&s).image_source_status(CODEX, false).needs_source());
    s.finish();
}

#[test]
fn slot_is_images_entry_for_single_image_specs_and_pinned_defaults() {
    let unconfigured = ImageSourceStatus::Unconfigured;
    assert_eq!(
        slot_for(&unconfigured, &docker(), CLAUDE),
        ImageSourceSlot::Default
    );
    for single in [
        archive("/a.tar"),
        ImageSourceSpec::Registry {
            registry: None,
            reference: Some("ghcr.io/me/claude:1".into()),
        },
        // Review finding 1: a reference equal to the tag is still fixed.
        ImageSourceSpec::DockerStore {
            host: None,
            tls: None,
            reference: Some(CLAUDE.into()),
        },
        ImageSourceSpec::Registry {
            registry: Some("ghcr.io".into()),
            reference: Some(CLAUDE.into()),
        },
    ] {
        assert_eq!(
            slot_for(&unconfigured, &single, CLAUDE),
            ImageSourceSlot::Image(CLAUDE.into())
        );
    }
    let pinned = ImageSourceStatus::DefaultPinned {
        layer: Layer::Global,
        spec: archive("/a.tar"),
    };
    assert_eq!(
        slot_for(&pinned, &docker(), CODEX),
        ImageSourceSlot::Image(CODEX.into())
    );
    assert_eq!(
        ImageSourceSlot::Image(CODEX.into()).field(),
        format!("runtime.builtin.images[\"{CODEX}\"]")
    );
    assert_eq!(
        ImageSourceSlot::Default.field(),
        "runtime.builtin.imageSource"
    );
}

#[test]
fn preview_fragment_is_the_exact_json_written() {
    let s = sandbox!("preview_fragment_is_the_exact_json_written");
    write(&repo_path(&s), r#"{"runtime":{"builtin":{"vcpus":4}}}"#);
    for (slot, spec, builder) in [
        (ImageSourceSlot::Default, docker(), false),
        (
            ImageSourceSlot::Image(CODEX.into()),
            archive("/x/codex.tar"),
            true,
        ),
    ] {
        let before = read_json(&repo_path(&s));
        let plan = plan_image_source_edit(
            &config(&s),
            Some(&s.work()),
            ImageSourceEdit {
                scope: ImageSourceScope::Repo,
                slot: slot.clone(),
                spec,
                enable_builder: builder,
            },
            &subjects()[..1]
                .iter()
                .cloned()
                .map(|mut subject| {
                    if let ImageSourceSlot::Image(tag) = &slot {
                        subject.tag = tag.clone();
                        subject.is_default = false;
                    }
                    subject
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(plan.preview.path, repo_path(&s));
        assert_eq!(plan.preview.field, slot.field());
        // Nothing is written by planning.
        assert_eq!(read_json(&repo_path(&s)), before);
        apply_image_source_plan(&plan).unwrap();
        let mut expected = before;
        deep_merge(
            &mut expected,
            &serde_json::from_str(&plan.preview.fragment).unwrap(),
        );
        assert_eq!(read_json(&repo_path(&s)), expected);
    }
    // The repo default now wins, so a global default is refused as shadowed
    // rather than previewed as the fix.
    let error = plan_image_source_edit(
        &config(&s),
        Some(&s.work()),
        ImageSourceEdit {
            scope: ImageSourceScope::Global,
            slot: ImageSourceSlot::Default,
            spec: docker(),
            enable_builder: false,
        },
        &subjects()[..1],
    )
    .unwrap_err();
    assert_eq!(
        error,
        ImageSourcePlanError::Shadowed {
            field: "runtime.builtin.imageSource".into(),
            scope: "global",
            winner: "repo",
        }
    );
    assert!(!global_path(&s).exists());
    s.finish();
}

#[test]
fn preview_reports_effective_source_per_image_and_builder_state() {
    let s = sandbox!("preview_reports_effective_source_per_image_and_builder_state");
    let plan = plan_image_source_edit(
        &config(&s),
        Some(&s.work()),
        ImageSourceEdit {
            scope: ImageSourceScope::Global,
            slot: ImageSourceSlot::Default,
            spec: docker(),
            enable_builder: true,
        },
        &subjects(),
    )
    .unwrap();
    assert_eq!(plan.preview.builder, BuilderPreview::Enabled);
    assert_eq!(plan.preview.effective.len(), 2);
    for image in &plan.preview.effective {
        let source = image.source.as_ref().unwrap();
        assert_eq!(source.kind, ImageSourceKind::DockerStore);
        assert_eq!(source.json, r#"{"type":"docker-local"}"#);
        assert_eq!(source.scope, "global");
        assert!(source.from_this_write);
    }
    let lines = plan.preview.effective_lines();
    assert_eq!(
        lines,
        vec![
            format!("{CLAUDE} → docker-local from runtime.builtin.imageSource (global config, this change)"),
            format!("{CODEX} → docker-local from runtime.builtin.imageSource (global config, this change)"),
            "builder: enabled by this change".to_string(),
        ]
    );
    // An archive for the default image only: the other image keeps no source.
    let plan = plan_image_source_edit(
        &config(&s),
        Some(&s.work()),
        ImageSourceEdit {
            scope: ImageSourceScope::Repo,
            slot: ImageSourceSlot::Image(CLAUDE.into()),
            spec: archive("/a.tar"),
            enable_builder: false,
        },
        &subjects(),
    )
    .unwrap();
    assert_eq!(
        plan.preview.builder,
        BuilderPreview::Unchanged { effective: false }
    );
    assert!(plan.preview.effective[1].source.is_none());
    assert_eq!(
        plan.preview.effective_lines(),
        vec![
            format!("{CLAUDE} → archive from runtime.builtin.images[\"{CLAUDE}\"] (repo config, this change)"),
            format!("{CODEX} → still no source"),
            "builder: stays off".to_string(),
        ]
    );
    assert!(!repo_path(&s).exists() && !global_path(&s).exists());
    s.finish();
}

#[cfg(unix)]
#[test]
fn repo_write_preserves_unrelated_keys_and_file_mode() {
    use std::os::unix::fs::PermissionsExt;
    let s = sandbox!("repo_write_preserves_unrelated_keys_and_file_mode");
    write(
        &repo_path(&s),
        r#"{"runtime":{"builtin":{"vcpus":4,"registries":{"r.example":{"insecure":true}}}},"agents":{"default":"claude"}}"#,
    );
    std::fs::set_permissions(repo_path(&s), std::fs::Permissions::from_mode(0o600)).unwrap();
    let plan = plan_image_source_edit(
        &config(&s),
        Some(&s.work()),
        ImageSourceEdit {
            scope: ImageSourceScope::Repo,
            slot: ImageSourceSlot::Default,
            spec: docker(),
            enable_builder: false,
        },
        &subjects()[..1],
    )
    .unwrap();
    apply_image_source_plan(&plan).unwrap();
    let written = read_json(&repo_path(&s));
    assert_eq!(written["runtime"]["builtin"]["vcpus"], 4);
    assert_eq!(
        written["runtime"]["builtin"]["registries"]["r.example"]["insecure"],
        true
    );
    assert_eq!(written["agents"]["default"], "claude");
    assert_eq!(
        written["runtime"]["builtin"]["imageSource"],
        serde_json::json!({"type": "docker-local"})
    );
    assert!(written["runtime"]["builtin"].get("builder").is_none());
    let mode = std::fs::metadata(repo_path(&s))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    assert!(!global_path(&s).exists(), "the global file is untouched");
    s.finish();
}

#[cfg(unix)]
#[test]
fn global_write_creates_the_file_and_follows_a_user_symlink() {
    let s = sandbox!("global_write_creates_the_file_and_follows_a_user_symlink");
    let edit = ImageSourceEdit {
        scope: ImageSourceScope::Global,
        slot: ImageSourceSlot::Default,
        spec: docker(),
        enable_builder: false,
    };
    // No global file yet: it is created.
    let plan = plan_image_source_edit(&config(&s), None, edit.clone(), &subjects()[..1]).unwrap();
    assert_eq!(plan.preview.path, global_path(&s));
    apply_image_source_plan(&plan).unwrap();
    assert_eq!(
        read_json(&global_path(&s))["runtime"]["builtin"]["imageSource"]["type"],
        "docker-local"
    );
    // A dotfiles symlink is followed and survives.
    let dotfiles = s.home().join("dotfiles/awman.json");
    write(&dotfiles, r#"{"agents":{"default":"claude"}}"#);
    std::fs::remove_file(global_path(&s)).unwrap();
    std::os::unix::fs::symlink(&dotfiles, global_path(&s)).unwrap();
    let plan = plan_image_source_edit(&config(&s), None, edit, &subjects()[..1]).unwrap();
    apply_image_source_plan(&plan).unwrap();
    assert!(std::fs::symlink_metadata(global_path(&s))
        .unwrap()
        .file_type()
        .is_symlink());
    let written = read_json(&dotfiles);
    assert_eq!(written["agents"]["default"], "claude");
    assert_eq!(
        written["runtime"]["builtin"]["imageSource"]["type"],
        "docker-local"
    );
    s.finish();
}

#[cfg(unix)]
#[test]
fn repo_write_refuses_a_symlinked_config() {
    let s = sandbox!("repo_write_refuses_a_symlinked_config");
    let outside = s.home().join("elsewhere.json");
    write(&outside, "{}");
    std::fs::create_dir_all(repo_path(&s).parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&outside, repo_path(&s)).unwrap();
    let error = plan_image_source_edit(
        &EffectiveConfig::default().with_env(env(&s)),
        Some(&s.work()),
        ImageSourceEdit {
            scope: ImageSourceScope::Repo,
            slot: ImageSourceSlot::Default,
            spec: docker(),
            enable_builder: false,
        },
        &subjects()[..1],
    )
    .unwrap_err();
    assert!(error.to_string().contains("symlink"), "{error}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "{}");
    s.finish();
}

#[test]
fn global_images_entry_is_not_offered_when_the_repo_defines_images() {
    let s = sandbox!("global_images_entry_is_not_offered_when_the_repo_defines_images");
    write(
        &global_path(&s),
        r#"{"runtime":{"builtin":{"imageSource":{"type":"archive","path":"/claude.tar"}}}}"#,
    );
    write(
        &repo_path(&s),
        r#"{"runtime":{"builtin":{"images":{"awman-app-other:latest":{"type":"registry"}}}}}"#,
    );
    let config = config(&s);
    let status = config.image_source_status(CODEX, false);
    assert!(matches!(status, ImageSourceStatus::DefaultPinned { .. }));
    let scopes = image_source_scopes(&config, Some(&s.work()), &status, CODEX, false).unwrap();
    assert_eq!(
        scopes.offered.iter().map(|o| o.scope).collect::<Vec<_>>(),
        vec![ImageSourceScope::Repo]
    );
    assert_eq!(scopes.shadowed.len(), 1);
    assert_eq!(scopes.shadowed[0].0.scope, ImageSourceScope::Global);
    assert_eq!(scopes.shadowed[0].1, Layer::Repo);
    let global_before = std::fs::read(global_path(&s)).unwrap();
    let error = plan_image_source_edit(
        &config,
        Some(&s.work()),
        ImageSourceEdit {
            scope: ImageSourceScope::Global,
            slot: ImageSourceSlot::Image(CODEX.into()),
            spec: docker(),
            enable_builder: false,
        },
        &[PreviewSubject {
            agent: "codex".into(),
            tag: CODEX.into(),
            is_default: false,
        }],
    )
    .unwrap_err();
    // The repo's map replaces the global one, so the repo is what wins.
    assert_eq!(
        error,
        ImageSourcePlanError::Shadowed {
            field: images_entry_field(CODEX),
            scope: "global",
            winner: "repo",
        }
    );
    assert_eq!(std::fs::read(global_path(&s)).unwrap(), global_before);
    s.finish();
}

#[test]
fn repo_builder_false_shadows_a_global_builder_opt_in() {
    let s = sandbox!("repo_builder_false_shadows_a_global_builder_opt_in");
    write(
        &repo_path(&s),
        r#"{"runtime":{"builtin":{"builder":{"enabled":false}}}}"#,
    );
    let plan = plan_image_source_edit(
        &config(&s),
        Some(&s.work()),
        ImageSourceEdit {
            scope: ImageSourceScope::Global,
            slot: ImageSourceSlot::Default,
            spec: docker(),
            enable_builder: true,
        },
        &subjects()[..1],
    )
    .unwrap();
    assert_eq!(
        plan.preview.builder,
        BuilderPreview::Shadowed { by: "repo".into() }
    );
    assert!(!plan.edit().enable_builder);
    assert!(!plan.preview.fragment.contains("builder"));
    assert_eq!(plan.preview.notes.len(), 1);
    apply_image_source_plan(&plan).unwrap();
    let written = read_json(&global_path(&s));
    assert!(written["runtime"]["builtin"].get("builder").is_none());
    assert_eq!(
        written["runtime"]["builtin"]["imageSource"]["type"],
        "docker-local"
    );
    s.finish();
}

#[test]
fn invalid_spec_or_file_is_rejected_before_any_write() {
    let s = sandbox!("invalid_spec_or_file_is_rejected_before_any_write");
    let good = config(&s);
    let plan = |spec: ImageSourceSpec| {
        plan_image_source_edit(
            &good,
            Some(&s.work()),
            ImageSourceEdit {
                scope: ImageSourceScope::Repo,
                slot: ImageSourceSlot::Default,
                spec,
                enable_builder: false,
            },
            &subjects()[..1],
        )
    };
    for (spec, needle) in [
        (
            ImageSourceSpec::Registry {
                registry: None,
                reference: Some("  ".into()),
            },
            "`reference` must not be empty",
        ),
        (
            ImageSourceSpec::DockerStore {
                host: Some("http://docker".into()),
                tls: None,
                reference: None,
            },
            "must start with one of",
        ),
    ] {
        let error = plan(spec).unwrap_err();
        assert!(
            matches!(error, ImageSourcePlanError::Invalid(_)),
            "{error:?}"
        );
        assert!(error.to_string().contains(needle), "{error}");
        assert!(
            error
                .to_string()
                .starts_with("runtime.builtin.imageSource:"),
            "{error}"
        );
    }
    assert!(!repo_path(&s).exists());
    // The repo file has a key awman does not know: nothing is planned or
    // written, and the file keeps its bytes.
    let bad = r#"{"runtime":{"builtin":{"bogus":1}}}"#;
    write(&repo_path(&s), bad);
    let error = plan_image_source_edit(
        &EffectiveConfig::default().with_env(env(&s)),
        Some(&s.work()),
        ImageSourceEdit {
            scope: ImageSourceScope::Repo,
            slot: ImageSourceSlot::Default,
            spec: docker(),
            enable_builder: false,
        },
        &subjects()[..1],
    )
    .unwrap_err();
    assert!(
        matches!(error, ImageSourcePlanError::Invalid(_)),
        "{error:?}"
    );
    assert!(error.to_string().contains("bogus"), "{error}");
    assert_eq!(std::fs::read_to_string(repo_path(&s)).unwrap(), bad);
    // Repo scope outside a Git repository.
    assert_eq!(
        plan_image_source_edit(
            &good,
            None,
            ImageSourceEdit {
                scope: ImageSourceScope::Repo,
                slot: ImageSourceSlot::Default,
                spec: docker(),
                enable_builder: false,
            },
            &subjects()[..1],
        )
        .unwrap_err(),
        ImageSourcePlanError::NoRepo
    );
    s.finish();
}

#[test]
fn apply_refuses_when_the_target_changed_after_the_plan() {
    let s = sandbox!("apply_refuses_when_the_target_changed_after_the_plan");
    let plan = plan_image_source_edit(
        &config(&s),
        Some(&s.work()),
        ImageSourceEdit {
            scope: ImageSourceScope::Global,
            slot: ImageSourceSlot::Default,
            spec: docker(),
            enable_builder: false,
        },
        &subjects()[..1],
    )
    .unwrap();
    let edited = r#"{"agents":{"default":"codex"}}"#;
    write(&global_path(&s), edited);
    let error = apply_image_source_plan(&plan).unwrap_err();
    assert!(error.to_string().contains("nothing was written"), "{error}");
    assert_eq!(std::fs::read_to_string(global_path(&s)).unwrap(), edited);
    s.finish();
}

#[test]
fn missing_message_names_tag_fields_files_and_values_on_one_line() {
    let repo = PathBuf::from("/r/.awman/config.json");
    let global = PathBuf::from("/h/.awman/config.json");
    let unconfigured = missing_image_source_message(&MissingImageSource {
        tag: CLAUDE,
        agent: "claude",
        reason: &ImageSourceStatus::Unconfigured,
        repo_config: Some(&repo),
        global_config: &global,
    });
    assert!(unconfigured.starts_with(&format!("no image source is configured for '{CLAUDE}'")));
    for needle in [
        "`runtime.builtin.imageSource`",
        &format!("`runtime.builtin.images[\"{CLAUDE}\"]`"),
        "/r/.awman/config.json or /h/.awman/config.json",
        r#"{"type":"docker-local"}"#,
        r#"{"type":"archive","path":"<file>"}"#,
        "`runtime.builtin.builder.enabled`",
        "awman ready",
    ] {
        assert!(unconfigured.contains(needle), "{needle}: {unconfigured}");
    }
    let pinned = missing_image_source_message(&MissingImageSource {
        tag: CODEX,
        agent: "codex",
        reason: &ImageSourceStatus::DefaultPinned {
            layer: Layer::Global,
            spec: archive("/a.tar"),
        },
        repo_config: None,
        global_config: &global,
    });
    assert!(
        pinned.contains("(archive) names a single image"),
        "{pinned}"
    );
    assert!(pinned.contains(&images_entry_field(CODEX)), "{pinned}");
    assert!(!pinned.contains("/r/.awman"), "{pinned}");
    assert!(pinned.contains("awman ready --build"), "{pinned}");
    for message in [&unconfigured, &pinned] {
        assert!(!message.chars().any(char::is_control), "{message}");
    }
}

fn global_default_edit() -> ImageSourceEdit {
    ImageSourceEdit {
        scope: ImageSourceScope::Global,
        slot: ImageSourceSlot::Default,
        spec: docker(),
        enable_builder: false,
    }
}

/// Review finding 4, scenario A: a closer layer that gained a source after
/// the session was opened is seen by the preview, which refuses the
/// shadowed global write instead of advertising it as effective.
#[test]
fn plan_reads_closer_layers_from_disk_not_the_session_snapshot() {
    let s = sandbox!("plan_reads_closer_layers_from_disk_not_the_session_snapshot");
    let snapshot = config(&s);
    write(
        &repo_path(&s),
        r#"{"runtime":{"builtin":{"imageSource":{"type":"registry"}}}}"#,
    );
    let error = plan_image_source_edit(
        &snapshot,
        Some(&s.work()),
        global_default_edit(),
        &subjects()[..1],
    )
    .unwrap_err();
    assert!(
        matches!(error, ImageSourcePlanError::Shadowed { winner: "repo", .. }),
        "{error:?}"
    );
    assert!(!global_path(&s).exists());
    s.finish();
}

/// Review finding 4, scenario A after approval: a repo override added while
/// the confirmation is open makes the write refuse; the global file is not
/// created.
#[test]
fn apply_refuses_when_a_closer_layer_changed_after_the_plan() {
    let s = sandbox!("apply_refuses_when_a_closer_layer_changed_after_the_plan");
    let plan = plan_image_source_edit(
        &config(&s),
        Some(&s.work()),
        global_default_edit(),
        &subjects()[..1],
    )
    .unwrap();
    write(
        &repo_path(&s),
        r#"{"runtime":{"builtin":{"builder":{"enabled":false}}}}"#,
    );
    let error = apply_image_source_plan(&plan).unwrap_err();
    assert!(error.to_string().contains("nothing was written"), "{error}");
    assert!(error.to_string().contains(".awman"), "{error}");
    assert!(!global_path(&s).exists());
    s.finish();
}

/// Review finding 4, scenario B: a concurrent write of the very field being
/// set is detected, not masked by re-applying this edit, and is kept.
#[test]
fn apply_refuses_a_concurrent_write_of_the_same_field() {
    let s = sandbox!("apply_refuses_a_concurrent_write_of_the_same_field");
    let plan = plan_image_source_edit(
        &config(&s),
        Some(&s.work()),
        global_default_edit(),
        &subjects()[..1],
    )
    .unwrap();
    let concurrent = r#"{"runtime":{"builtin":{"imageSource":{"type":"registry"}}}}"#;
    write(&global_path(&s), concurrent);
    let error = apply_image_source_plan(&plan).unwrap_err();
    assert!(error.to_string().contains("nothing was written"), "{error}");
    assert_eq!(
        std::fs::read_to_string(global_path(&s)).unwrap(),
        concurrent
    );
    s.finish();
}
