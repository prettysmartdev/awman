//! engine::container component tests (filled in by slice-layer1-engine).

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
