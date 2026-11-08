use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::*;
use crate::data::ids::TabId;
use crate::data::instance_snapshot::AnnotationTarget;

fn ann(pairs: &[(&str, &str)]) -> Annotations {
    Annotations::try_from_pairs(pairs.iter().copied()).unwrap()
}

#[derive(Clone, Copy)]
enum Behaviour {
    Claim,
    Panic,
}

struct FakePresenter {
    name: &'static str,
    behaviour: Behaviour,
    calls: AtomicUsize,
    handle: Mutex<Option<PresenterHandle>>,
}
impl HostPresenter for FakePresenter {
    fn name(&self) -> &'static str {
        self.name
    }
    fn bind(&self, handle: PresenterHandle) {
        *self.handle.lock().unwrap() = Some(handle);
    }
    fn on_ui_request(&self, _: &InstanceId, _: &FrontendId, _: &UiRequest) -> UiOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.behaviour {
            Behaviour::Claim => UiOutcome::claimed(self.name),
            Behaviour::Panic => panic!("boom"),
        }
    }
    fn on_instance_event(&self, _: &InstanceId, _: &PresenterEvent) {}
}

struct Fake {
    name: &'static str,
    detects: HostRequirement,
    presenter: Option<Arc<FakePresenter>>,
}
impl Integration for Fake {
    fn name(&self) -> &'static str {
        self.name
    }
    fn detect(&self, _: &EnvSnapshot, _: &EffectiveConfig) -> HostRequirement {
        self.detects.clone()
    }
    fn presenter(&self) -> Option<Arc<dyn HostPresenter>> {
        self.presenter.clone().map(|p| p as Arc<dyn HostPresenter>)
    }
}

fn presenter(name: &'static str, behaviour: Behaviour) -> Arc<FakePresenter> {
    Arc::new(FakePresenter {
        name,
        behaviour,
        calls: AtomicUsize::new(0),
        handle: Mutex::new(None),
    })
}
fn fake(name: &'static str, detects: HostRequirement, p: Option<Arc<FakePresenter>>) -> Arc<Fake> {
    Arc::new(Fake {
        name,
        detects,
        presenter: p,
    })
}
fn sink() -> (NoticeSink, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    (Arc::new(move |_, t| s.lock().unwrap().push(t)), seen)
}
fn req() -> UiRequest {
    UiRequest::NextTab
}
fn route(reg: &IntegrationRegistry, hello: &Annotations, n: &NoticeSink) -> UiOutcome {
    reg.route_ui_request(hello, &InstanceId::new(), &FrontendId::new(), &req(), n)
}
fn detect(reg: &IntegrationRegistry) -> Result<Detected, AnnotationError> {
    reg.detect_all(&EnvSnapshot::empty(), &EffectiveConfig::default())
}

#[test]
fn compiled_in_is_empty_and_detects_nothing() {
    let reg = IntegrationRegistry::compiled_in();
    assert!(reg.names().is_empty());
    let d = detect(&reg).unwrap();
    assert_eq!(d.requirement, HostRequirement::None);
    assert!(d.annotations.is_empty() && d.required_by.is_empty());
}

#[test]
fn routes_by_hello_namespace() {
    let (a, b) = (
        presenter("alpha", Behaviour::Claim),
        presenter("beta", Behaviour::Claim),
    );
    let reg = IntegrationRegistry::with(vec![
        fake("alpha", HostRequirement::None, Some(a.clone())),
        fake("beta", HostRequirement::None, Some(b.clone())),
    ]);
    let (n, seen) = sink();
    let out = route(&reg, &ann(&[("beta/id", "1")]), &n);
    assert!(out.claimed_by_host);
    assert_eq!(out.claimed_by.as_deref(), Some("beta"));
    assert_eq!(a.calls.load(Ordering::SeqCst), 0);
    assert_eq!(b.calls.load(Ordering::SeqCst), 1);
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn unclaimed_frontend_gets_one_notice() {
    let reg = IntegrationRegistry::with(vec![fake(
        "alpha",
        HostRequirement::None,
        Some(presenter("alpha", Behaviour::Claim)),
    )]);
    let (n, seen) = sink();
    let out = route(&reg, &ann(&[("other/x", "1")]), &n);
    assert_eq!(out, UiOutcome::unclaimed());
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn panicking_presenter_falls_back_with_one_notice() {
    let reg = IntegrationRegistry::with(vec![fake(
        "alpha",
        HostRequirement::None,
        Some(presenter("alpha", Behaviour::Panic)),
    )]);
    let (n, seen) = sink();
    let out = route(&reg, &ann(&[("alpha/x", "1")]), &n);
    assert!(!out.claimed_by_host);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn supervisor_panic_yields_unclaimed_and_one_notice() {
    let p = presenter("alpha", Behaviour::Panic);
    let (n, seen) = sink();
    let sup = PresenterSupervisor::new(vec![p.clone()], Arc::new(NullWriter), n);
    let p: Arc<dyn HostPresenter> = p;
    let out = sup
        .ui_request(&p, &InstanceId::new(), &FrontendId::new(), &req())
        .await;
    assert!(!out.claimed_by_host);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

struct NullWriter;
impl AnnotationWriter for NullWriter {
    fn set(&self, _: &AnnotationTarget, _: &str, _: &Annotations) -> Result<(), AnnotationError> {
        Ok(())
    }
    fn clear(&self, _: &AnnotationTarget, _: &str, _: &str) -> Result<(), AnnotationError> {
        Ok(())
    }
}

#[derive(Default)]
struct RecordingWriter(Mutex<Vec<String>>);
impl AnnotationWriter for RecordingWriter {
    fn set(&self, _: &AnnotationTarget, ns: &str, p: &Annotations) -> Result<(), AnnotationError> {
        self.0.lock().unwrap().push(format!("set {ns} {}", p.len()));
        Ok(())
    }
    fn clear(&self, _: &AnnotationTarget, ns: &str, key: &str) -> Result<(), AnnotationError> {
        self.0.lock().unwrap().push(format!("clear {ns} {key}"));
        Ok(())
    }
}

#[test]
fn presenter_handle_cannot_write_foreign_namespace() {
    let w = Arc::new(RecordingWriter::default());
    let p = presenter("rex", Behaviour::Claim);
    let (n, _) = sink();
    let _sup = PresenterSupervisor::new(vec![p.clone()], w.clone(), n);
    let handle = p.handle.lock().unwrap().clone().expect("bound");
    assert_eq!(handle.namespace(), "rex");
    let t = AnnotationTarget::Instance;
    handle.set(&t, "rex/window", "1").unwrap();
    handle.clear(&t, "rex/window").unwrap();
    assert!(matches!(
        handle.set(&t, "other/window", "1"),
        Err(AnnotationError::ForeignNamespace { .. })
    ));
    assert!(matches!(
        handle.clear(&t, "rexx/window"),
        Err(AnnotationError::ForeignNamespace { .. })
    ));
    assert!(matches!(
        handle.set(
            &AnnotationTarget::Tab {
                tab_id: TabId::new()
            },
            "nonsense",
            "1"
        ),
        Err(AnnotationError::ForeignNamespace { .. })
    ));
    assert_eq!(
        *w.0.lock().unwrap(),
        vec!["set rex 1", "clear rex rex/window"]
    );
}

#[test]
fn detect_merges_disjoint_namespaces_and_keeps_strongest() {
    let reg = IntegrationRegistry::with(vec![
        fake(
            "alpha",
            HostRequirement::Optional(ann(&[("alpha/a", "1")])),
            None,
        ),
        fake(
            "beta",
            HostRequirement::Required(ann(&[("beta/b", "2")])),
            None,
        ),
        fake("gamma", HostRequirement::None, None),
    ]);
    let d = detect(&reg).unwrap();
    assert_eq!(d.annotations, ann(&[("alpha/a", "1"), ("beta/b", "2")]));
    assert!(d.requirement.requires_daemon());
    assert_eq!(d.requirement.annotations(), Some(&d.annotations));
    assert_eq!(d.required_by, vec!["beta"]);
}

#[test]
fn detect_rejects_namespace_clash() {
    // Two integrations sharing a name (hence a namespace) clash on the same key.
    let reg = IntegrationRegistry::with(vec![
        fake(
            "alpha",
            HostRequirement::Optional(ann(&[("alpha/a", "1")])),
            None,
        ),
        fake(
            "alpha",
            HostRequirement::Optional(ann(&[("alpha/a", "1")])),
            None,
        ),
    ]);
    assert!(matches!(
        detect(&reg),
        Err(AnnotationError::NamespaceClash { .. })
    ));
}

#[test]
fn detect_rejects_foreign_namespace_keys() {
    let reg = IntegrationRegistry::with(vec![fake(
        "alpha",
        HostRequirement::Optional(ann(&[("beta/a", "1")])),
        None,
    )]);
    assert!(matches!(
        detect(&reg),
        Err(AnnotationError::ForeignNamespace { .. })
    ));
}

#[test]
fn default_hooks_are_unsupported() {
    let f = fake("alpha", HostRequirement::None, None);
    let (env, cfg) = (EnvSnapshot::empty(), EffectiveConfig::default());
    let ctx = IntegrationContext {
        env: &env,
        config: &cfg,
        session: None,
    };
    assert!(f.setup(&ctx).is_err() && f.remove(&ctx).is_err() && f.status(&ctx).is_err());
    assert!(IntegrationRegistry::with(vec![f]).get("alpha").is_some());
}
