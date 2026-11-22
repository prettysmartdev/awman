use super::*;

pub(super) fn run(r: &mut Run, m: &Manifest) -> Result<()> {
    r.command("docker-version", "docker", &["version"])?;
    if r.platform.apple() {
        r.command("apple-service", "container", &["system", "status"])?;
        r.command("sbx-service", "sbx", &["ls"])?;
    }
    let socket = r.command(
        "docker-context",
        "docker",
        &[
            "context",
            "inspect",
            "--format",
            "{{.Endpoints.docker.Host}}",
        ],
    )?;
    let socket = socket.trim();
    ensure!(
        socket.starts_with("unix:///"),
        "Docker context must use a local Unix socket"
    );
    r.env.insert(
        "AWMAN_TEST_HOST_RESOURCES".into(),
        format!(
            "hypervisor,offline,docker={socket}{}",
            if r.platform.apple() {
                ",apple-containers,docker-sbx"
            } else {
                ""
            }
        ),
    );
    r.env.insert("DOCKER_HOST".into(), socket.into());
    if !r.platform.apple() {
        let id = r.command(
            "start-registry",
            "docker",
            &[
                "run",
                "--detach",
                "--rm",
                "--publish",
                "127.0.0.1::5000",
                "--env",
                "REGISTRY_STORAGE_DELETE_ENABLED=true",
                "registry:2.8.3",
            ],
        )?;
        let id = id.trim().to_string();
        ensure!(
            id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid registry container ID"
        );
        r.registry = Some(id.clone());
        let address = r.command("registry-address", "docker", &["port", &id, "5000/tcp"])?;
        let address = address.trim();
        let addr: std::net::SocketAddr = address.parse().context("registry loopback address")?;
        ensure!(
            addr.ip().is_loopback(),
            "registry must bind only to loopback"
        );
        r.command(
            "registry-ready",
            "curl",
            &[
                "--fail",
                "--silent",
                "--show-error",
                "--retry",
                "10",
                "--retry-connrefused",
                "--retry-all-errors",
                "--retry-max-time",
                "30",
                "--retry-delay",
                "1",
                "--max-time",
                "10",
                &format!("http://{address}/v2/"),
            ],
        )?;
        r.env
            .get_mut("AWMAN_TEST_HOST_RESOURCES")
            .unwrap()
            .push_str(&format!(",local-registry={address}"));
    }
    r.xtask("install-runner", &["test", "install-runner"])?;
    r.command(
        "fetch-payload",
        "bash",
        &["tools/msb-payloads/fetch.sh", r.platform.target],
    )?;
    r.command(
        "verify-payload",
        "bash",
        &["tools/msb-payloads/verify.sh", r.platform.target],
    )?;
    let store = r.work.join("fixtures");
    fs::create_dir_all(&store)?;
    // Preparation can use the network. Tests receive only the resulting pinned bytes.
    // Pull the reviewed immutable reference, then record the local archive bytes.
    let pinned_image = fs::read_to_string(r.root.join("ci/fixture-base-image.txt"))?;
    let pinned_image = pinned_image.trim();
    ensure!(
        pinned_image.starts_with("alpine@sha256:")
            && pinned_image.len() == "alpine@sha256:".len() + 64
            && pinned_image["alpine@sha256:".len()..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit()),
        "ci/fixture-base-image.txt must pin an Alpine SHA-256 digest"
    );
    r.command(
        "pull-base",
        "docker",
        &[
            "pull",
            "--platform",
            r.platform.image_platform,
            pinned_image,
        ],
    )?;
    let image = r.command(
        "base-provenance",
        "docker",
        &[
            "image",
            "inspect",
            pinned_image,
            "--format",
            "{{json .RepoDigests}}",
        ],
    )?;
    let digests: Vec<String> = serde_json::from_str(image.trim())?;
    ensure!(
        digests.iter().any(|digest| digest == pinned_image),
        "base image digest differs from ci/fixture-base-image.txt"
    );
    let base = r.work.join("base.tar");
    r.command(
        "save-base",
        "docker",
        &[
            "image",
            "save",
            "--output",
            base.to_str().unwrap(),
            pinned_image,
        ],
    )?;
    let base_hash = sha256_file(&base)?;
    let built = r.work.join("fixture-build");
    r.xtask(
        "build-fixture",
        &[
            "fixtures",
            "build",
            "guest-oci-base",
            "--base",
            base.to_str().unwrap(),
            "--sha256",
            &base_hash,
            "--out",
            built.to_str().unwrap(),
        ],
    )?;
    let fixture = store.join("guest-oci-base.tar");
    fs::copy(built.join("fixture-oci.tar"), &fixture)?;
    r.packet.fixture_sha256 = sha256_file(&fixture)?;
    fs::create_dir_all(r.out.join("fixtures"))?;
    fs::copy(&fixture, r.out.join("fixtures/guest-oci-base.tar"))?;
    let docker_fixture = store.join("guest-docker-base.tar");
    fs::copy(built.join("fixture-docker.tar"), &docker_fixture)?;
    r.packet.docker_fixture_sha256 = sha256_file(&docker_fixture)?;
    fs::copy(
        &docker_fixture,
        r.out.join("fixtures/guest-docker-base.tar"),
    )?;
    fs::write(
        r.out.join("fixture-provenance.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"base_image": pinned_image, "base_archive_sha256": base_hash, "guest_archive_sha256": r.packet.fixture_sha256, "docker_archive_sha256": r.packet.docker_fixture_sha256, "recipe_commit": r.packet.commit}),
        )?,
    )?;
    if let Some(previous) = m.previous_release.get(r.platform.target) {
        let previous_path = store
            .join("previous-release")
            .join(r.platform.target)
            .join(&previous.asset);
        let url = format!(
            "https://github.com/prettysmartdev/awman/releases/download/{}/{}",
            previous.version, previous.asset
        );
        r.xtask(
            "fetch-previous",
            &[
                "fixtures",
                "build",
                "previous-published",
                "--url",
                &url,
                "--sha256",
                previous
                    .sha256
                    .as_deref()
                    .context("published previous release must be pinned")?,
                "--out",
                previous_path.to_str().unwrap(),
            ],
        )?;
    }
    // The run-local pin is explicit evidence, not a silent edit to repository policy.
    let mut config: toml::Value =
        toml::from_str(&fs::read_to_string(r.root.join("ci/gates.toml"))?)?;
    for (id, hash) in [
        ("guest-oci-base", &r.packet.fixture_sha256),
        ("guest-docker-base", &r.packet.docker_fixture_sha256),
    ] {
        let fixture_config = config["fixtures"][id]
            .as_table_mut()
            .context("fixture table")?;
        fixture_config.remove("pending");
        fixture_config.insert("sha256".into(), hash.clone().into());
    }
    let config_path = r.out.join("run-config.toml");
    fs::write(&config_path, toml::to_string_pretty(&config)?)?;
    r.env.insert(
        "AWMAN_TEST_RUN_CONFIG".into(),
        config_path.display().to_string(),
    );
    r.env
        .insert("AWMAN_TEST_FIXTURES".into(), store.display().to_string());
    Ok(())
}
