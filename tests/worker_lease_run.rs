//! Worker end to end (WI 0143): what a lease run is given and reports.
//!
//! Checklist items: B6 (an inline work-item body is handed to the run from
//! an owner-only file outside the clone; no work-item file is created in,
//! committed to or pushed from the repository), B7 (read-only support per
//! runtime: Docker and Apple Containers mount the workspace read-only,
//! the sandbox paradigm refuses and the lease is ineligible) and B10
//! (`outputs_mounted` follows the runtime's actual mount, and a mounted but
//! empty output directory is distinct from no mount).
//!
//! Running a real agent container against a read-only lease workspace needs
//! the Layer 2 lease runner and an agent image; see
//! `/awman/context/workflow/test-coverage.md` for what is covered where.

#[path = "helpers/smart_http.rs"]
mod smart_http;
#[path = "helpers/wsp_lease.rs"]
mod wsp_lease;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::ids::LeaseId;
use awman::data::message::RecordingMessageSink;
use awman::data::worker::job_store::JobStore;
use awman::engine::agent_runtime::{Capabilities, ImageAcquisition};
use awman::engine::container::ContainerRuntime;
use awman::engine::git::lease_transport::LeaseUrlPolicy;
use awman::engine::git::push_retry::PushRetryPolicy;
use awman::engine::git::GitEngine;
use awman::engine::worker::credential_helper::lease_transport;
use awman::engine::worker::lease_workspace::{LeaseCheckout, LeaseRuntimeSupport, LeaseWorkspace};
use awman::engine::worker::push::{LeasePush, PushObserver, PushRetry};
use awman::engine::worker::LeaseRef;
use smart_http::{files_containing, git, HelperScript, SmartHttpRepo};
use tokio_util::sync::CancellationToken;
use wsp_lease::{claim, item, labels, Wsp};

const MARKER: &str = "INLINE-BODY-MARKER-5c1e";

struct NoopObserver;
impl PushObserver for NoopObserver {
    fn retry(&self, _: &PushRetry) {}
}

fn sandbox_capabilities() -> Capabilities {
    Capabilities {
        arbitrary_host_mounts: false,
        kit_declarative: true,
        has_image_store: false,
        image_acquisition: ImageAcquisition::Kit,
        per_launch_vm_settings: false,
        reports_resource_stats: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inline_body_reaches_run_from_private_file_and_no_work_item_file_is_committed_real_git() {
    if !smart_http::http_backend_available() {
        assert_eq!(std::env::var("AWMAN_TEST_ALLOW_SKIP_REAL_GIT").as_deref(), Ok("1"), "git http-backend is required; install Git or explicitly set AWMAN_TEST_ALLOW_SKIP_REAL_GIT=1");
        eprintln!("skipping: git http-backend is not installed (explicit opt-out)");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = SmartHttpRepo::start(tmp.path(), None).await;
    let helper = HelperScript::install(&tmp.path().join("helper"));
    let mut work = item("01JINLINE", &repo.url);
    work.body = format!("# Work Item: inline\n\n{MARKER}\n");
    let work_root = tmp.path().join("worker");
    let ws = LeaseWorkspace::prepare(&work_root, &LeaseId::new()).unwrap();
    let inline = ws.inline_work_item(&work.id, &work.body).unwrap();
    // The run reads the body from here; it is outside the (mounted) clone.
    assert_eq!(std::fs::read_to_string(inline.path()).unwrap(), work.body);
    assert!(!inline.path().starts_with(ws.clone_dir()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(inline.path()), 0o600);
        assert_eq!(mode(inline.path().parent().unwrap()), 0o700);
    }

    let transport =
        lease_transport(&ws, &work, helper.program(), LeaseUrlPolicy::default()).unwrap();
    let clone = ws.clone_dir().to_path_buf();
    let (ws, transport) = {
        let repo_spec = work.repo.clone();
        tokio::task::spawn_blocking(move || {
            let mut sink = RecordingMessageSink::new();
            LeaseCheckout::new(&GitEngine::new(), &transport)
                .run(&ws, &repo_spec, &mut sink, &CancellationToken::new())
                .unwrap();
            (ws, transport)
        })
        .await
        .unwrap()
    };
    // The repository has no work-item file for this item, and the "agent"
    // works from the inline body without one.
    assert!(files_containing(&clone, MARKER).is_empty());
    let body = std::fs::read_to_string(inline.path()).unwrap();
    std::fs::write(
        clone.join("result.txt"),
        format!("implemented {} lines\n", body.lines().count()),
    )
    .unwrap();
    smart_http::commit_all(&clone, "implement inline item");
    let push_git = GitEngine::new()
        .isolate_remote_push(&clone, &repo.url)
        .unwrap()
        .with_lease_transport(transport.clone());
    LeasePush::new(
        Arc::new(push_git),
        work.repo.branch.clone(),
        PushRetryPolicy::for_lease(
            chrono::Utc::now() + chrono::Duration::minutes(5),
            Duration::from_secs(1),
        ),
        Duration::from_secs(1),
    )
    .run(&CancellationToken::new(), &NoopObserver)
    .await
    .unwrap();

    let tree = git(
        &repo.bare,
        &["ls-tree", "-r", "--name-only", "refs/heads/wsp/01JINLINE"],
    );
    assert!(tree.lines().any(|f| f == "result.txt"), "{tree}");
    assert!(
        !tree
            .lines()
            .any(|f| f.contains("work-item") || f.starts_with("aspec/work-items")),
        "no work-item file pushed: {tree}"
    );
    assert!(
        files_containing(&repo.bare, MARKER).is_empty(),
        "body never reached the repository"
    );
    let inline_path = inline.path().to_path_buf();
    drop(inline);
    assert!(!inline_path.exists(), "inline body removed with the run");
    drop((ws, transport));
}

#[test]
fn read_only_lease_support_per_runtime_and_sandbox_is_ineligible() {
    let mut work = awman_wsp::conformance::work_item("01JRO");
    work.exec.read_only = true;
    // Container runtimes mount the workspace read-only (with a writable
    // scratch dir) and advertise `lease_read_only`.
    for (name, runtime) in [
        ("docker", ContainerRuntime::docker()),
        ("apple-containers", ContainerRuntime::apple()),
    ] {
        let caps = runtime.capabilities();
        assert!(caps.read_only_workspace(), "{name}");
        let support = LeaseRuntimeSupport::from_capabilities(caps);
        assert!(support.check(&work, name).is_ok(), "{name}");
        assert!(
            support
                .features()
                .contains(&awman_wsp::features::LEASE_READ_ONLY),
            "{name}"
        );
    }
    // The sandbox paradigm cannot: fail closed before any work, and never
    // advertise the feature.
    let sandbox = LeaseRuntimeSupport::from_capabilities(&sandbox_capabilities());
    let refusal = sandbox.check(&work, "docker-sbx").unwrap_err();
    assert!(refusal.0.contains("read-only"), "{refusal:?}");
    assert!(!sandbox
        .features()
        .contains(&awman_wsp::features::LEASE_READ_ONLY));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outputs_mounted_true_when_dir_mounted_false_otherwise_and_empty_dir_distinct() {
    let docker = ContainerRuntime::docker();
    let docker_caps = docker.capabilities();
    let sandbox_caps = sandbox_capabilities();
    // What the runtime actually mounts, not what was requested.
    assert!(docker_caps.mounts_outputs(true));
    assert!(
        !docker_caps.mounts_outputs(false),
        "nothing requested, nothing mounted"
    );
    assert!(
        !sandbox_caps.mounts_outputs(true),
        "requested but the sandbox cannot mount it"
    );

    // Durable per-command record: unknown, true, false are distinct.
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::open(tmp.path()).unwrap());
    let jobs = JobStore::open(store).unwrap();
    assert_eq!(
        jobs.outputs_mounted("cmd_00000000000000000000000000000001")
            .unwrap(),
        None
    );
    jobs.set_outputs_mounted(
        "cmd_00000000000000000000000000000001",
        docker_caps.mounts_outputs(true),
    )
    .unwrap();
    jobs.set_outputs_mounted(
        "cmd_00000000000000000000000000000002",
        sandbox_caps.mounts_outputs(true),
    )
    .unwrap();
    assert_eq!(
        jobs.outputs_mounted("cmd_00000000000000000000000000000001")
            .unwrap(),
        Some(true)
    );
    assert_eq!(
        jobs.outputs_mounted("cmd_00000000000000000000000000000002")
            .unwrap(),
        Some(false)
    );

    // On the wire: a mounted, empty output dir is `outputs_mounted: true`
    // with an empty `outputs` map; no mount is `false` with no map.
    let wsp = Wsp::start().await;
    let mut mounted = item("01JOUTM", "https://example.invalid/r.git");
    mounted.outputs = Some(awman_wsp::Outputs { max_bytes: 1024 });
    let mut unmounted = item("01JOUTU", "https://example.invalid/r.git");
    unmounted.outputs = Some(awman_wsp::Outputs { max_bytes: 1024 });
    wsp.enqueue(mounted);
    wsp.enqueue(unmounted);
    let worker = wsp.worker("w", 2, &[], labels(&[]), labels(&[])).await;
    for (caps, expect_mounted) in [(docker_caps, true), (&sandbox_caps, false)] {
        let lease = claim(&worker.session, None).await.unwrap();
        let mounted = caps.mounts_outputs(lease.item.outputs.is_some());
        assert_eq!(mounted, expect_mounted);
        worker
            .session
            .result(
                &LeaseRef::of(&lease),
                &awman_wsp::LeaseResult {
                    result: awman::data::fs::api_db::CommandResult::default(),
                    outputs: mounted.then(BTreeMap::new),
                    outputs_mounted: mounted,
                    duration_ms: 1,
                },
            )
            .await
            .unwrap();
    }
    let m = wsp.mock.result_for("01JOUTM").unwrap();
    let u = wsp.mock.result_for("01JOUTU").unwrap();
    assert!(
        m.outputs_mounted && m.outputs == Some(BTreeMap::new()),
        "{m:?}"
    );
    assert!(!u.outputs_mounted && u.outputs.is_none(), "{u:?}");
}
