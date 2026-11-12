//! The in-process fakes pass the same conformance checks the real servers
//! and tools run in the boundary tier.

use crate::{fake_cli_exe, sandbox};
use awman_test_support::conformance::{
    self, DockerCliUnderTest, RegistryUnderTest, WorkServerUnderTest,
};
use awman_test_support::fakes::{self, FakeGitHttp, FakeRegistry, FakeWorkServer};
use awman_test_support::fixtures::{OciImageBuilder, RepoBuilder};
use awman_test_support::{TestCase, TestSandbox};

#[test]
fn fake_registry_conforms() {
    let sandbox = sandbox("fakes::fake_registry_conforms");
    let registry = FakeRegistry::start(&sandbox);
    let report = conformance::registry(&RegistryUnderTest {
        addr: registry.addr(),
        repository: "awman-conformance/image".into(),
    });
    report.assert_conforms();
    assert!(report.passed.len() >= 10);
}

#[test]
fn fake_registry_serves_inserted_images() {
    let sandbox = sandbox("fakes::fake_registry_serves_inserted_images");
    let registry = FakeRegistry::start(&sandbox);
    let image = OciImageBuilder::new().layer(&[("a", b"b")]).build();
    registry.insert_image("lib/app", "v1", &image);
    assert!(registry.has_blob(&image.layers[0].digest));
    let report = conformance::registry(&RegistryUnderTest {
        addr: registry.addr(),
        repository: "lib/app".into(),
    });
    report.assert_conforms();
    assert!(registry.requests().iter().any(|r| r == "GET /v2/"));
}

#[test]
fn fake_docker_cli_conforms_with_the_baseline_rules() {
    let sandbox = sandbox("fakes::fake_docker_cli_conforms_with_the_baseline_rules");
    let docker = sandbox.install_fake(fake_cli_exe(), "docker");
    fakes::docker_cli_baseline(&docker);
    conformance::docker_cli(&DockerCliUnderTest {
        sandbox: &sandbox,
        cli: docker.exe().to_path_buf(),
    })
    .assert_conforms();
}

#[test]
fn a_fake_docker_without_rules_does_not_conform() {
    let sandbox = sandbox("fakes::a_fake_docker_without_rules_does_not_conform");
    let docker = sandbox.install_fake(fake_cli_exe(), "docker");
    let report = conformance::docker_cli(&DockerCliUnderTest {
        sandbox: &sandbox,
        cli: docker.exe().to_path_buf(),
    });
    assert!(!report.is_success());
    assert_eq!(report.failed.len(), 3);
}

#[test]
fn fake_git_http_serves_clone_and_push() {
    let sandbox = sandbox("fakes::fake_git_http_serves_clone_and_push");
    let repo = RepoBuilder::new(&sandbox, "src")
        .file("a.txt", "a\n")
        .init();
    repo.bare_remote("origin");
    let server = FakeGitHttp::start(&sandbox, &sandbox.root().join("remotes"));
    let url = server.url("origin.git");
    let clone_dir = sandbox.work().join("clone");
    sandbox
        .command("git")
        .args(["clone", "-q", &url, &clone_dir.to_string_lossy()])
        .run()
        .assert_success();
    assert_eq!(
        std::fs::read_to_string(clone_dir.join("a.txt")).unwrap(),
        "a\n"
    );
    std::fs::write(clone_dir.join("b.txt"), "b\n").unwrap();
    for args in [
        vec!["add", "b.txt"],
        vec!["commit", "-q", "-m", "b"],
        vec!["push", "-q", "origin", "main"],
    ] {
        sandbox
            .command("git")
            .args(&args)
            .current_dir(&clone_dir)
            .run()
            .assert_success();
    }
    let log = repo.git(&["ls-remote", "origin", "refs/heads/main"]);
    let pushed = sandbox
        .command("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&clone_dir)
        .run()
        .stdout_text();
    assert!(log.starts_with(pushed.trim()), "{log} vs {pushed}");
    drop(server);
}

#[test]
fn fake_work_server_conforms() {
    let sandbox = TestSandbox::for_test(TestCase::component("fakes::fake_work_server_conforms"));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let server = FakeWorkServer::start(&sandbox).await;
        let report = conformance::work_server(&WorkServerUnderTest {
            base_url: server.base_url().to_string(),
            admin: server.server().as_ref(),
        })
        .await;
        report.assert_conforms();
        server.shutdown().await;
    });
}
