use super::*;
use crate::data::{
    config::image_source::ImageSourceSpec,
    image_tags::{agent_image_tag, project_image_tag},
};
use crate::engine::image_prep::contracts::{identity, preparer, Sink, Store};
use awman_test_support::{TestCase, TestSandbox};
use std::sync::{atomic::Ordering, Arc, Mutex};

fn local() -> Option<ImageSourceSpec> {
    Some(ImageSourceSpec::AppleStore { reference: None })
}
#[derive(Default)]
struct Output {
    begins: Vec<String>,
    lines: Vec<String>,
    finishes: Vec<(String, Option<String>)>,
}
impl BuildOutputTarget for Output {
    fn begin(&mut self, tag: &str) {
        self.begins.push(tag.into());
    }
    fn line(&mut self, line: &str) {
        self.lines.push(line.into());
    }
    fn finish(&mut self, tag: &str, error: Option<&str>) {
        self.finishes.push((tag.into(), error.map(str::to_string)));
    }
}

#[tokio::test]
async fn missing_image_preflight_builds_once_routes_output_and_reuses_source_images() {
    let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::missing_image_preflight_builds_once_routes_output_and_reuses_source_images"));
    let store = Arc::new(Store::default());
    let tag = agent_image_tag(&s.work(), "claude");
    let base = project_image_tag(&s.work());
    let mut sink = Sink::default();
    let mut output = Output::default();
    prepare_importing_step_with_recovery(
        preparer(&s, store.clone(), true, local()),
        &tag,
        "claude",
        &mut sink,
        Some(&mut output),
    )
    .await
    .unwrap();
    assert_eq!(*store.built.lock().unwrap(), [base.clone(), tag.clone()]);
    assert_eq!(output.begins, [base, tag.clone()]);
    assert_eq!(output.lines, ["builder stdout", "builder stdout"]);
    assert!(output.finishes.iter().all(|(_, error)| error.is_none()));
    assert!(!sink.0.iter().any(|line| line == "builder stdout"));
    assert!(sink.0.iter().any(|line| line.contains("Building image")));
    // Installed image, then missing runtime image but present source image:
    // neither is a reason to rebuild the source store.
    for installed in [true, false] {
        if !installed {
            store.installed.lock().unwrap().clear();
        }
        prepare_importing_step_with_recovery(
            preparer(&s, store.clone(), true, local()),
            &tag,
            "claude",
            &mut sink,
            None,
        )
        .await
        .unwrap();
    }
    assert_eq!(store.built.lock().unwrap().len(), 2);
    s.finish();
}

#[tokio::test]
async fn failed_build_keeps_diagnostics_and_never_imports_and_apple_recovery_still_builds() {
    let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::failed_build_keeps_diagnostics_and_never_imports_and_apple_recovery_still_builds"));
    let store = Arc::new(Store::default());
    let tag = agent_image_tag(&s.work(), "claude");
    store.fail_build.store(true, Ordering::SeqCst);
    let mut sink = Sink::default();
    let mut output = Output::default();
    let error = prepare_importing_step_with_recovery(
        preparer(&s, store.clone(), true, local()),
        &tag,
        "claude",
        &mut sink,
        Some(&mut output),
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("builder stderr: invalid Dockerfile"));
    assert!(output.finishes[0]
        .1
        .as_ref()
        .unwrap()
        .contains("invalid Dockerfile"));
    assert!(store.imports.lock().unwrap().is_empty());
    store.fail_build.store(false, Ordering::SeqCst);
    store
        .installed
        .lock()
        .unwrap()
        .insert(tag.clone(), identity(&tag));
    store.apple_missing.store(true, Ordering::SeqCst);
    prepare_importing_step_with_recovery(
        preparer(&s, store.clone(), true, local()),
        &tag,
        "claude",
        &mut sink,
        None,
    )
    .await
    .unwrap();
    assert_eq!(store.imports.lock().unwrap().len(), 2);
    assert!(sink
        .0
        .iter()
        .any(|line| line.contains("Apple Containers is missing content")));
    assert!(sink.0.iter().any(|line| line == "builder stdout"));
    s.finish();
}

#[tokio::test]
async fn disabled_and_incompatible_sources_do_not_build_or_emit_build_warnings() {
    let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::disabled_and_incompatible_sources_do_not_build_or_emit_build_warnings"));
    let tag = agent_image_tag(&s.work(), "claude");
    for (enabled, source) in [
        (false, local()),
        (
            true,
            Some(ImageSourceSpec::Registry {
                registry: None,
                reference: None,
            }),
        ),
        (
            true,
            Some(ImageSourceSpec::Archive {
                path: s.work().join("image.tar"),
            }),
        ),
        (
            true,
            Some(ImageSourceSpec::DockerStore {
                host: Some("tcp://127.0.0.1:2375".into()),
                tls: None,
                reference: None,
            }),
        ),
    ] {
        let store = Arc::new(Store::default());
        store.fail_build.store(true, Ordering::SeqCst);
        let mut sink = Sink::default();
        prepare_importing_step_with_recovery(
            preparer(&s, store.clone(), enabled, source),
            &tag,
            "claude",
            &mut sink,
            None,
        )
        .await
        .unwrap();
        assert!(store.built.lock().unwrap().is_empty());
        assert_eq!(store.imports.lock().unwrap().len(), 1);
        assert!(!sink
            .0
            .iter()
            .any(|line| line.contains("cannot build Dockerfiles")));
    }
    s.finish();
}

struct WaitingSink {
    messages: Arc<Mutex<Vec<String>>>,
    waiting: Arc<tokio::sync::Notify>,
}
impl UserMessageSink for WaitingSink {
    fn write_message(&mut self, message: UserMessage) {
        if message.text.contains("Waiting for another awman process") {
            self.waiting.notify_one();
        }
        self.messages.lock().unwrap().push(message.text);
    }
    fn replay_queued(&mut self) {}
}
#[tokio::test]
async fn concurrent_preflight_holds_build_leases_until_import_finishes() {
    let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::concurrent_preflight_holds_build_leases_until_import_finishes"));
    let store = Arc::new(Store::default());
    store.hold_import.store(true, Ordering::SeqCst);
    let tag = agent_image_tag(&s.work(), "claude");
    let first = preparer(&s, store.clone(), true, local());
    let second = preparer(&s, store.clone(), true, local());
    let first_tag = tag.clone();
    let one = tokio::spawn(async move {
        prepare_importing_step_with_recovery(
            first,
            &first_tag,
            "claude",
            &mut Sink::default(),
            None,
        )
        .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        store.import_entered.notified(),
    )
    .await
    .unwrap();
    let waiting = Arc::new(tokio::sync::Notify::new());
    let mut sink = WaitingSink {
        messages: Arc::new(Mutex::new(vec![])),
        waiting: waiting.clone(),
    };
    let two = tokio::spawn(async move {
        prepare_importing_step_with_recovery(second, &tag, "claude", &mut sink, None).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), waiting.notified())
        .await
        .unwrap();
    assert_eq!(store.built.lock().unwrap().len(), 2);
    assert_eq!(store.imports.lock().unwrap().len(), 1);
    *store.release_import.0.lock().unwrap() = true;
    store.release_import.1.notify_all();
    one.await.unwrap().unwrap();
    two.await.unwrap().unwrap();
    assert_eq!(store.built.lock().unwrap().len(), 2);
    assert_eq!(store.imports.lock().unwrap().len(), 2);
    s.finish();
}
