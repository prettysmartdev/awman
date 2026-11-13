use super::*;

pub(super) fn run(r: &mut Run, m: &Manifest) -> Result<()> {
    r.command("docker-version", "docker", &["version"])?;
    r.command("apple-service", "container", &["system", "status"])?;
    r.command("sbx-service", "sbx", &["ls"])?;
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
        format!("hypervisor,offline,apple-containers,docker-sbx,docker={socket}"),
    );
    r.xtask("install-runner", &["test", "install-runner"])?;
    r.command(
        "fetch-payload",
        "bash",
        &["tools/msb-payloads/fetch.sh", TARGET],
    )?;
    r.command(
        "verify-payload",
        "bash",
        &["tools/msb-payloads/verify.sh", TARGET],
    )?;
    let store = r.work.join("fixtures");
    fs::create_dir_all(&store)?;
    // Preparation can use the network. Tests receive only the resulting pinned bytes.
    // Resolve the tag once, then pull/save by immutable digest; record both provenance
    // and archive hashes. A future reviewer can reproduce this input independently.
    r.command(
        "pull-base",
        "docker",
        &["pull", "--platform", "linux/arm64", "alpine:3.22"],
    )?;
    let image = r.command(
        "base-provenance",
        "docker",
        &[
            "image",
            "inspect",
            "alpine:3.22",
            "--format",
            "{{index .RepoDigests 0}}",
        ],
    )?;
    let image = image.trim();
    ensure!(
        image.starts_with("alpine@sha256:") && image.len() == "alpine@sha256:".len() + 64,
        "base image did not resolve to an immutable digest"
    );
    let base = r.work.join("base.tar");
    r.command(
        "save-base",
        "docker",
        &["image", "save", "--output", base.to_str().unwrap(), image],
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
    fs::write(
        r.out.join("fixture-provenance.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"base_image": image, "base_archive_sha256": base_hash, "guest_archive_sha256": r.packet.fixture_sha256, "recipe_commit": r.packet.commit}),
        )?,
    )?;
    let previous = &m.previous_release[TARGET];
    let previous_path = store
        .join("previous-release")
        .join(TARGET)
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
    // The run-local pin is explicit evidence, not a silent edit to repository policy.
    let mut config: toml::Value =
        toml::from_str(&fs::read_to_string(r.root.join("ci/gates.toml"))?)?;
    let fixture_config = config["fixtures"]["guest-oci-base"]
        .as_table_mut()
        .context("fixture table")?;
    fixture_config.remove("pending");
    fixture_config.insert("sha256".into(), r.packet.fixture_sha256.clone().into());
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
