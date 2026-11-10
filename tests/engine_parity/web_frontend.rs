//! Web uses owner-captured environments and the existing instance engine.
use super::multi_frontend::attach;
use crate::{
    command::integrations::IntegrationRegistry,
    data::{annotations::Annotations, frontend_protocol::*, launch_context::CreateLaunchContext},
    support::Daemon,
};
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_hello_uses_context_and_resumes_the_same_frontend() {
    let d = Daemon::start(IntegrationRegistry::compiled_in()).await;
    let project = tempfile::tempdir().unwrap();
    let c = d
        .host()
        .launch_contexts()
        .create(CreateLaunchContext {
            root: project.path().into(),
            label: None,
            default: true,
            env: d.hello(FrontendRole::Primary).env,
        })
        .unwrap();
    let project_root = c.root.clone();
    let mut h = d.hello(FrontendRole::Primary);
    h.kind = FrontendKindWire::Web;
    h.cwd = Default::default();
    h.argv.clear();
    h.env = Default::default();
    h.launch_context = Some(c.id);
    let id = d.host().create_instance(Annotations::new()).unwrap();
    // The shared attach helper verifies and consumes Welcome + Snapshot.
    let (tx, _rx) = attach(&d, &id, h.clone()).await;
    let frontend_id = d
        .host()
        .get(&id)
        .unwrap()
        .frontends
        .into_iter()
        .find(|f| f.role == FrontendRole::Primary)
        .unwrap()
        .frontend_id;
    assert_eq!(
        d.host().get(&id).unwrap().summary.primary_frontend_kind,
        Some(FrontendKindWire::Web)
    );
    assert!(d.host().tabs(&id).unwrap()[0]
        .session
        .cwd
        .starts_with(&project_root));
    tx.close().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    h.resume = Some(frontend_id.clone());
    h.launch_context = None;
    let (_tx, _rx) = attach(&d, &id, h).await;
    let primary = d
        .host()
        .get(&id)
        .unwrap()
        .frontends
        .into_iter()
        .find(|f| f.role == FrontendRole::Primary)
        .unwrap();
    assert_eq!(primary.frontend_id, frontend_id);
    assert!(!primary.reconnecting);
}
