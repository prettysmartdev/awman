//! engine::container component tests (filled in by slice-layer1-engine).

#[test]
fn docker_and_apple_build_labels_inspect_filter_and_remove_images() {
    use awman::data::image_inputs::{InputsHash, RepoKey, INPUTS_LABEL, REPO_LABEL};
    use awman::engine::container::{BuildRequest, ContainerRuntime};
    let s = crate::support::sandbox(
        "engine::container::docker_and_apple_build_labels_inspect_filter_and_remove_images",
    );
    let image_id = format!("sha256:{}", "1".repeat(64));
    let platform = awman::data::oci_identity::OciPlatform::host_linux();
    for name in ["docker", "container"] {
        let fake = s.install_fake(crate::support::fake_cli_exe(), name);
        fake.on(&["build"]).stdout("built\n");
        let labels =
            serde_json::json!({INPUTS_LABEL: "inputs", REPO_LABEL: "example.test/team/repo"});
        let json = if name == "docker" {
            serde_json::json!([{"Id": image_id, "RepoTags": ["project:latest"], "Size": 123, "Config": {"Labels": labels}}])
        } else {
            serde_json::json!([{"configuration": {"name": "project:latest", "descriptor": {"digest": "sha256:index"}}, "variants": [{"digest": image_id, "size": 123, "platform": platform, "config": {"config": {"Labels": labels}}}]}])
        };
        fake.on(&["image", "inspect", "project:latest"])
            .stdout(&format!("{json}\n"));
        fake.on(&["image", "inspect", &image_id])
            .stdout(&format!("{json}\n"));
        fake.on(&["image", "inspect", "missing"])
            .stderr("No such image: missing")
            .exit(1);
        fake.on(&["image", "ls"])
            .stdout(&format!("{image_id}\n{image_id}\n"));
        fake.on(&["image", "list"])
            .stdout("[{\"configuration\":{\"name\":\"project:latest\"}}]\n");
        fake.on(&["rmi"]);
        fake.on(&["image", "rm"]);
        let mut values: std::collections::BTreeMap<String, String> = s
            .env()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        for key in ["TMPDIR", "TEMP", "TMP"] {
            values.insert(key.into(), s.root().display().to_string());
        }
        let env = awman::data::config::env::EnvSnapshot::from_full_env(values);
        let runtime = if name == "docker" {
            ContainerRuntime::docker_in(env)
        } else {
            ContainerRuntime::apple_in(env)
        };
        let repo = RepoKey::from_label("example.test/team/repo");
        let inputs = InputsHash::from_label("inputs");
        let request = BuildRequest::runtime(
            "project:latest",
            &s.work().join("Dockerfile.dev"),
            &s.work(),
            false,
        )
        .with_inputs(&inputs, &repo);
        runtime.build_image(&request, &mut |_| {}).unwrap();
        let image = runtime
            .image_with_inputs("project:latest", &inputs)
            .unwrap()
            .unwrap();
        assert_eq!(image.id, image_id);
        assert_eq!(image.size_bytes, 123);
        assert_eq!(image.labels[REPO_LABEL], repo.as_str());
        assert!(runtime
            .image_with_inputs("project:latest", &InputsHash::from_label("stale"))
            .unwrap()
            .is_none());
        assert!(runtime.inspect_image("missing").unwrap().is_none());
        assert_eq!(runtime.list_images_by_repo(&repo).unwrap().len(), 1);
        runtime.remove_image("project:latest").unwrap();
        let calls = fake.calls();
        let build = calls
            .iter()
            .find(|call| call.argv.first().is_some_and(|arg| arg == "build"))
            .unwrap();
        assert!(build
            .argv
            .windows(2)
            .any(|args| args == ["--label", "dev.awman.inputs=inputs"]));
        assert!(build
            .argv
            .windows(2)
            .any(|args| args == ["--label", "dev.awman.repo=example.test/team/repo"]));
        assert!(calls.iter().any(|call| if name == "docker" {
            call.argv == ["rmi", "project:latest"]
        } else {
            call.argv == ["image", "rm", "project:latest"]
        }));
    }
}

#[test]
fn docker_and_apple_deliver_git_cache_credentials_only_on_private_stdin() {
    use awman::data::secret::SecretString;
    use awman::engine::container::{background::BackgroundLaunchRequest, ContainerRuntime};
    let s = crate::support::sandbox(
        "engine::container::docker_and_apple_deliver_git_cache_credentials_only_on_private_stdin",
    );
    let secret = "test-secret-private-cache";
    s.redact(secret);
    for name in ["docker", "container"] {
        let fake = s.install_fake(crate::support::fake_cli_exe(), name);
        fake.on(&["run"]).stdout("private-cache-fixture\n");
        fake.on(&["exec", "-i"]).read_stdin();
        fake.on(&["stop"]);
        fake.on(&["rm"]);
        // Host CLI isolation admits a fake only inside this snapshot's temp
        // root. The sandbox's fake binaries and scratch dir are siblings.
        let mut values: std::collections::BTreeMap<String, String> = s
            .env()
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        for key in ["TMPDIR", "TEMP", "TMP"] {
            values.insert(key.into(), s.root().display().to_string());
        }
        let env = awman::data::config::env::EnvSnapshot::from_full_env(values)
            .with_invocation(s.work(), vec!["awman".into()]);
        let runtime = if name == "docker" {
            ContainerRuntime::docker_in(env)
        } else {
            ContainerRuntime::apple_in(env)
        };
        let container = runtime
            .launch_background(&BackgroundLaunchRequest {
                labels: &[],
                secret_env: &[],
                name: Some("private-cache-fixture"),
                image: "fixture:local",
                workdir: &s.work(),
                env: &Default::default(),
                overlays: &[],
                builtin_launch: None,
            })
            .unwrap();
        let seed = SecretString::new(format!(
            "protocol=https\nhost=git.example\npath=repo.git\nusername=user\npassword={secret}\n\n"
        ));
        container.seed_git_credentials(&seed).unwrap();
        container.kill().unwrap();
        let calls = fake.calls();
        let launch = calls
            .iter()
            .find(|call| call.argv.first().is_some_and(|arg| arg == "run"))
            .unwrap();
        assert_eq!(
            &launch.argv[launch.argv.len() - 4..],
            ["--entrypoint", "sleep", "fixture:local", "infinity"]
        );
        let exec = calls
            .iter()
            .find(|call| call.argv.first().is_some_and(|a| a == "exec"))
            .unwrap();
        assert_eq!(exec.stdin, seed.expose().as_bytes());
        for call in &calls {
            assert!(!format!("{:?} {:?}", call.argv, call.env).contains(secret));
            assert!(!call.argv.iter().any(|arg| arg == "-t" || arg == "--tty"));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn native_docker_attach_recovers_env_redaction_before_output_and_failure_tail() {
    use awman::data::message::{UserMessage, UserMessageSink};
    use awman::data::session::AgentHandle;
    use awman::engine::agent_runtime::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
    use awman::engine::container::ContainerRuntime;
    struct Frontend(Option<AgentIo>);
    impl UserMessageSink for Frontend {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    impl AgentFrontend for Frontend {
        fn report_status(&mut self, _: AgentStatus) {}
        fn report_progress(&mut self, _: AgentProgress) {}
        fn take_io(&mut self) -> AgentIo {
            self.0.take().unwrap()
        }
    }
    let mut s = crate::support::sandbox(
        "engine::container::native_docker_attach_recovers_env_redaction_before_output_and_failure_tail",
    );
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    s.env_mut().set("AWMAN_TEST_DOCKER", "1");
    const KEY: &str = "synthetic-native-attach-canary";
    docker
        .on(&[
            "inspect",
            "--format",
            "{{index .Config.Labels \"awman.output-secret-env\"}}",
        ])
        .stdout("CURSOR_API_KEY\n");
    docker
        .on(&["inspect", "--format", "{{json .Config.Env}}"])
        .stdout(&serde_json::to_string(&vec![format!("CURSOR_API_KEY={KEY}")]).unwrap());
    docker
        .on(&["attach"])
        .stdout(&format!("attached {KEY}\n"))
        .stderr(&format!("problem {KEY}\n"))
        .exit(1);
    let runtime = ContainerRuntime::docker_in(crate::support::env_snapshot(&s));
    let handle = AgentHandle {
        id: "fixture".into(),
        name: "fixture".into(),
        image_tag: "awman-cursor".into(),
        started_at: chrono::Utc::now(),
    };
    let instance = runtime.attach(&handle).unwrap();
    let (stdout, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (stderr, mut errors) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut execution = instance
        .run_with_frontend(Box::new(Frontend(Some(AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }))))
        .unwrap();
    let tail = execution.output_tail().unwrap();
    assert_eq!(execution.wait().await.unwrap().exit_code, 1);
    assert!(tail.wait_for_writers(std::time::Duration::from_secs(3)));
    let mut output = Vec::new();
    while let Ok(bytes) = received.try_recv() {
        output.extend(bytes);
    }
    assert_eq!(String::from_utf8(output).unwrap(), "attached ***\n");
    let mut stderr = Vec::new();
    while let Ok(bytes) = errors.try_recv() {
        stderr.extend(bytes);
    }
    assert_eq!(String::from_utf8(stderr).unwrap(), "problem ***\n");
    assert!(!tail.snapshot_text().contains(KEY));
    assert!(tail.snapshot_text().contains("attached ***"));
    // Losing required protection refuses attach rather than publishing raw bytes.
    docker
        .on(&["inspect", "--format", "{{json .Config.Env}}"])
        .stdout("[]\n");
    assert!(runtime.attach(&handle).is_err());
    s.finish();
}
