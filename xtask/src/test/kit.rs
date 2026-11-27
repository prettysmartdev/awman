//! Relocatable exact-asset native tests. No repository or Cargo is used at run time.
use super::*;
use crate::evidence::{actions, digest, junit, schema::Outcome};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Parser)]
struct BuildArgs {
    #[arg(long)]
    native: String,
    #[arg(long)]
    commit: String,
    #[arg(long)]
    out: PathBuf,
}
#[derive(Parser)]
struct RunArgs {
    #[arg(long)]
    native: Option<String>,
    #[arg(long)]
    kit: PathBuf,
    #[arg(long)]
    artifact: PathBuf,
    #[arg(long)]
    artifact_sha256: String,
    #[arg(long)]
    fixtures: PathBuf,
    #[arg(long)]
    tmproot: PathBuf,
    #[arg(long)]
    out: PathBuf,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Kit {
    schema: u32,
    native_id: String,
    commit: String,
    tree: String,
    triple: String,
    manifest_sha256: String,
    plan_sha256: String,
    policy_sha256: String,
    nextest_version: String,
    tests: BTreeSet<String>,
    filter: String,
    files: BTreeMap<String, String>,
    fixtures: Vec<Fixture>,
    previous_release: Option<serde_json::Value>,
    budget_seconds: u64,
    socket_suffix_bytes: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    id: String,
    file: String,
    arch: String,
    sha256: String,
}
fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    })
}
fn files(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = std::fs::symlink_metadata(&path)?;
        ensure!(!meta.file_type().is_symlink(), "kit contains symlink");
        if meta.is_dir() {
            files(root, &path, out)?;
        } else {
            ensure!(meta.is_file(), "kit contains special file");
            let name = path
                .strip_prefix(root)?
                .to_str()
                .context("non UTF-8 kit path")?
                .replace('\\', "/");
            out.insert(name, digest::sha256_file(&path)?);
        }
    }
    Ok(())
}
fn runner(kit: &Path, verb: &str) -> Command {
    let mut c =
        Command::new(kit.join(format!("bin/cargo-nextest{}", std::env::consts::EXE_SUFFIX)));
    c.arg("nextest").arg(verb);
    c
}
fn selection(kit: &Path, c: &mut Command, filter: &str) {
    selection_at(kit, &kit.join("workspace"), c, filter);
}
fn selection_at(kit: &Path, workspace: &Path, c: &mut Command, filter: &str) {
    c.current_dir(workspace);
    if kit
        .join("payload/target/nextest/cargo-metadata.json")
        .exists()
    {
        c.arg("--cargo-metadata")
            .arg(kit.join("payload/target/nextest/cargo-metadata.json"))
            .arg("--binaries-metadata")
            .arg(kit.join("payload/target/nextest/binaries-metadata.json"))
            .arg("--target-dir-remap")
            .arg(kit.join("payload/target"));
    } else {
        c.arg("--archive-file")
            .arg(kit.join("tests.tar.zst"))
            .arg("--extract-to")
            .arg(kit.join("payload"));
    }
    c.arg("--workspace-remap")
        .arg(workspace)
        .arg("--config-file")
        .arg(kit.join("nextest.toml"))
        .args(["--user-config-file", "none", "-E", filter]);
}
pub(super) fn build(root: &Path, args: &[String]) -> Result<ExitCode> {
    let a =
        BuildArgs::try_parse_from(std::iter::once("kit".to_string()).chain(args.iter().cloned()))?;
    let m = crate::manifest::load(root)?;
    let plan_bytes = crate::gates::release_plan::encoded(&m)?;
    let native = m
        .release
        .native
        .get(&a.native)
        .context("unknown native obligation")?;
    for id in &native.fixtures {
        let f = &m.fixtures[id];
        ensure!(
            f.pending.is_none() && f.sha256.as_deref().is_some_and(digest::is_sha256),
            "fixture-digest: {id} needs a reviewed published pin"
        );
    }
    let source = actions::source(root)?;
    ensure!(
        source["clean"] == true && source["commit"] == a.commit,
        "kit needs exact clean source commit"
    );
    let asset = m
        .builds
        .values()
        .find(|b| b.asset.as_ref() == Some(&native.asset))
        .context("asset build")?;
    let triple = &m.platforms[&asset.platform].triple;
    ensure!(
        host_triple()? == *triple,
        "kit must be compiled on its native target"
    );
    require_nextest(root, &m)?;
    let out = absolute(&a.out)?;
    ensure!(
        !out.exists(),
        "kit output already exists; use a fresh directory"
    );
    for d in ["bin", "workspace/crates/awman-cli", "payload"] {
        std::fs::create_dir_all(out.join(d))?;
    }
    let tests: BTreeSet<_> = crate::manifest::release_policy::native_tests(&a.native)?
        .into_iter()
        .collect();
    let filter = tests
        .iter()
        .map(|s| format!("test(={})", s.split_once(' ').expect("test ID").1))
        .collect::<Vec<_>>()
        .join(" | ");
    let mut compile = Command::new("cargo");
    compile
        .current_dir(root)
        .env("PATH", tool_path(root))
        .args([
            "nextest",
            "archive",
            "--locked",
            "-p",
            "awman-cli",
            "--test",
            "artifact",
            "--archive-file",
        ])
        .arg(out.join("tests.tar.zst"));
    ensure!(execute(&mut compile)? == 0, "native kit compilation failed");
    let mut companion = Command::new("cargo");
    companion.current_dir(root).args([
        "build",
        "--locked",
        "-p",
        "awman-cli",
        "--bin",
        "awman-fake-cli",
    ]);
    ensure!(
        execute(&mut companion)? == 0,
        "kit companion compilation failed"
    );

    std::fs::copy(
        std::env::current_exe()?,
        out.join("bin")
            .join(format!("xtask{}", std::env::consts::EXE_SUFFIX)),
    )?;
    std::fs::copy(
        tools_dir(root).join(format!("cargo-nextest{}", std::env::consts::EXE_SUFFIX)),
        out.join("bin")
            .join(format!("cargo-nextest{}", std::env::consts::EXE_SUFFIX)),
    )?;
    std::fs::write(out.join("workspace/Cargo.toml"), "[workspace]\n")?;
    std::fs::write(out.join("nextest.toml"), format!("nextest-version = \"{}\"\n[profile.default]\nretries = 0\nfail-fast = false\ntest-threads = 1\nslow-timeout = {{ period = \"1s\", terminate-after = {} }}\n[profile.default.junit]\npath = \"junit.xml\"\n", m.tools["nextest"].version, native.suites.iter().map(|id| m.suites[id].budget_seconds.or(m.tiers["artifact"].budget_seconds).unwrap_or(300)).max().context("artifact deadline")?))?;
    // Only the necessary fixture is visible to GuestFixtures. The runtime's
    // logical name remains guest-oci-base; evidence retains its concrete ID.
    let mut config = toml::Table::new();
    for key in ["tiers", "capabilities"] {
        let raw: toml::Table = std::fs::read_to_string(root.join("ci/gates.toml"))?.parse()?;
        config.insert(key.into(), raw[key].clone());
    }
    let mut fixtures = Vec::new();
    for id in &native.fixtures {
        let f = &m.fixtures[id];
        ensure!(
            f.pending.is_none(),
            "fixture-digest: {id} is pending publication"
        );
        let sha = f
            .sha256
            .clone()
            .filter(|s| digest::is_sha256(s))
            .context("fixture-digest: missing native fixture pin")?;
        fixtures.push(Fixture {
            id: id.clone(),
            file: f.file.clone().context("fixture file")?,
            arch: f.arch.clone().context("fixture arch")?,
            sha256: sha.clone(),
        });
        config.insert(
            "fixtures".into(),
            toml::Value::Table(toml::Table::from_iter([(
                "guest-oci-base".into(),
                toml::Value::Table(toml::Table::from_iter([
                    ("sha256".into(), sha.into()),
                    ("file".into(), f.file.clone().unwrap().into()),
                    (
                        "tiers".into(),
                        toml::Value::Array(vec!["guest".into(), "artifact".into()]),
                    ),
                ])),
            )])),
        );
    }
    let predecessor = if let Some(t) = &native.previous_release {
        let p = &m.previous_release[t];
        ensure!(
            p.pending.is_none() && p.sha256.as_deref().is_some_and(digest::is_sha256),
            "fixture-digest: predecessor pin unavailable"
        );
        config.insert(
            "previous_release".into(),
            toml::Value::try_from(BTreeMap::from([(t, p)]))?,
        );
        Some(serde_json::json!({"triple":t,"asset":p.asset,"sha256":p.sha256}))
    } else {
        None
    };
    std::fs::write(out.join("run-config.toml"), toml::to_string(&config)?)?;
    let mut listing = runner(&out, "list");
    selection(&out, &mut listing, &filter);
    listing.args(["--message-format", "json"]);
    let listing = listing.output()?;
    ensure!(
        listing.status.success(),
        "kit standalone inventory failed: {}",
        String::from_utf8_lossy(&listing.stderr)
    );
    ensure!(
        actions::expected(&serde_json::from_slice(&listing.stdout)?)? == tests,
        "kit inventory differs from required native test IDs"
    );
    std::fs::write(out.join("expected.json"), listing.stdout)?;
    std::fs::copy(
        std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .map(|p| if p.is_absolute() { p } else { root.join(p) })
            .unwrap_or_else(|| root.join("target"))
            .join("debug")
            .join(format!("awman-fake-cli{}", std::env::consts::EXE_SUFFIX)),
        out.join("payload/target/debug")
            .join(format!("awman-fake-cli{}", std::env::consts::EXE_SUFFIX)),
    )?;
    std::fs::write(
        out.join("capabilities.json"),
        serde_json::to_vec_pretty(&capability_contract(&m, native))?,
    )?;
    let mut contents = BTreeMap::new();
    files(&out, &out, &mut contents)?;
    let kit = Kit {
        schema: 1,
        native_id: a.native,
        commit: a.commit,
        tree: source["tree"].as_str().context("tree")?.into(),
        triple: triple.clone(),
        manifest_sha256: m.version().into(),
        plan_sha256: digest::sha256(&plan_bytes),
        policy_sha256: crate::manifest::release_policy::digest(&m)?,
        nextest_version: m.tools["nextest"].version.clone(),
        tests,
        filter,
        files: contents,
        fixtures,
        previous_release: predecessor,
        budget_seconds: native.budget_seconds,
        socket_suffix_bytes: super::temp_root::KIT_SOCKET_SUFFIX_BYTES,
    };
    std::fs::write(out.join("kit.json"), serde_json::to_vec_pretty(&kit)?)?;
    let mut digests = kit.files.clone();
    digests.insert(
        "kit.json".into(),
        digest::sha256_file(&out.join("kit.json"))?,
    );
    std::fs::write(
        out.join("digests.json"),
        serde_json::to_vec_pretty(&digests)?,
    )?;
    Ok(ExitCode::SUCCESS)
}
pub(super) fn run(args: &[String]) -> Result<ExitCode> {
    let a = RunArgs::try_parse_from(
        std::iter::once("kit-run".to_string()).chain(args.iter().cloned()),
    )?;
    std::fs::create_dir_all(&a.out)?;
    let started = chrono::Utc::now();
    let mut result = serde_json::json!({"schema":1,"native_id":null,"commit":null,"tree":null,"artifact_sha256":a.artifact_sha256,"kit_sha256":null,"outcome":"blocked","reason":null,"reason_code":"kit-corrupt","tests":[],"host":{"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"os_build":null},"started":started,"finished":null});
    let mut sbx_check = a.native.as_deref() == Some(crate::manifest::release_policy::SBX_NATIVE);
    let checked = (|| -> Result<()> {
        let root = a.kit.canonicalize()?;
        let kit: Kit = serde_json::from_slice(&std::fs::read(root.join("kit.json"))?)?;
        sbx_check |= kit.native_id == crate::manifest::release_policy::SBX_NATIVE;
        ensure!(
            a.native.as_ref().is_none_or(|id| id == &kit.native_id),
            "kit-corrupt: runner check differs"
        );
        let mut actual_files = BTreeMap::new();
        files(&root, &root, &mut actual_files)?;
        actual_files.remove("digests.json");
        let digests: BTreeMap<String, String> =
            serde_json::from_slice(&std::fs::read(root.join("digests.json"))?)?;
        ensure!(
            digests == actual_files,
            "kit-corrupt: directory digest manifest differs"
        );
        actual_files.remove("kit.json");
        ensure!(
            actual_files == kit.files,
            "kit-corrupt: missing, extra or modified runtime file"
        );
        ensure!(
            kit.schema == 1 && !kit.tests.is_empty(),
            "kit-corrupt: unsupported or empty kit"
        );
        let expected: BTreeSet<_> = crate::manifest::release_policy::native_tests(&kit.native_id)?
            .into_iter()
            .collect();
        ensure!(
            kit.tests == expected
                && kit.filter == crate::manifest::release_policy::native_filter(&kit.native_id)?,
            "kit-corrupt: required selection differs"
        );
        let essential = [
            "bin/xtask",
            "bin/cargo-nextest",
            "run-config.toml",
            "capabilities.json",
            "nextest.toml",
            "expected.json",
            "workspace/Cargo.toml",
            "payload/target/nextest/cargo-metadata.json",
            "payload/target/nextest/binaries-metadata.json",
        ];
        for name in essential {
            let name = if name.starts_with("bin/") {
                format!("{name}{}", std::env::consts::EXE_SUFFIX)
            } else {
                name.into()
            };
            ensure!(
                kit.files.contains_key(&name),
                "kit-corrupt: unbound essential file {name}"
            );
        }
        for (name, sha) in &kit.files {
            let p = Path::new(name);
            ensure!(
                !p.is_absolute()
                    && p.components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                "kit-corrupt: invalid file path"
            );
            let path = root.join(p);
            let meta = std::fs::symlink_metadata(&path)?;
            ensure!(
                meta.is_file()
                    && !meta.file_type().is_symlink()
                    && path.canonicalize()?.starts_with(&root)
                    && digest::sha256_file(&path)? == *sha,
                "kit-corrupt: file digest mismatch {name}"
            );
        }
        result["native_id"] = serde_json::json!(kit.native_id);
        result["commit"] = serde_json::json!(kit.commit);
        result["tree"] = serde_json::json!(kit.tree);
        result["kit_sha256"] = serde_json::json!(digest::sha256_file(&root.join("kit.json"))?);
        result["manifest_sha256"] = serde_json::json!(kit.manifest_sha256);
        result["plan_sha256"] = serde_json::json!(kit.plan_sha256);
        result["policy_sha256"] = serde_json::json!(kit.policy_sha256);
        artifact_input(&Options {
            artifact: Some(a.artifact.clone()),
            artifact_sha256: Some(a.artifact_sha256.clone()),
            ..Options::default()
        })?;
        ensure!(
            a.tmproot.is_absolute() && a.tmproot.is_dir(),
            "socket-budget: private temporary root required"
        );
        #[cfg(unix)]
        ensure!(
            super::temp_root::kit_guest_socket_len(&a.tmproot)?
                <= if cfg!(target_os = "macos") {
                    super::temp_root::APPLE_SOCKET_LIMIT
                } else {
                    107
                },
            "socket-budget: root too long"
        );
        for f in &kit.fixtures {
            ensure!(
                digest::sha256_file(&a.fixtures.join(&f.file))? == f.sha256,
                "fixture-digest: {}",
                f.id
            );
            verify_oci_arch(&a.fixtures.join(&f.file), &f.arch)?;
            ensure!(
                f.arch
                    == if kit.triple.starts_with("x86_64") {
                        "amd64"
                    } else {
                        "arm64"
                    },
                "fixture-digest: architecture mismatch"
            );
        }
        if let Some(p) = &kit.previous_release {
            let path = a
                .fixtures
                .join("previous-release")
                .join(p["triple"].as_str().context("predecessor target")?)
                .join(p["asset"].as_str().context("predecessor asset")?);
            ensure!(
                digest::sha256_file(&path)?
                    == p["sha256"].as_str().context("predecessor digest")?,
                "fixture-digest: predecessor mismatch"
            );
        }
        #[cfg(target_os = "linux")]
        if !sbx_check {
            let kvm = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm");
            ensure!(kvm.is_ok(), "no-hypervisor: read/write /dev/kvm required");
        }
        #[cfg(target_os = "macos")]
        if !sbx_check {
            let hv = Command::new("/usr/sbin/sysctl")
                .args(["-n", "kern.hv_support"])
                .output()?;
            ensure!(
                hv.status.success() && String::from_utf8_lossy(&hv.stdout).trim() == "1",
                "no-hypervisor: HVF unavailable"
            );
        }
        result["env_contract_sha256"] =
            serde_json::json!(digest::sha256_file(&root.join("capabilities.json"))?);
        result["fixtures"] = serde_json::to_value(&kit.fixtures)?;
        result["previous_release"] = serde_json::json!(kit.previous_release);
        let version = {
            let mut c = Command::new(
                root.join(format!("bin/cargo-nextest{}", std::env::consts::EXE_SUFFIX)),
            );
            c.args(["nextest", "--version"]);
            c
        }
        .output()?;
        ensure!(
            version.status.success()
                && String::from_utf8_lossy(&version.stdout).contains(&kit.nextest_version),
            "kit-corrupt: runner version mismatch"
        );
        let qualification = if sbx_check {
            Some(crate::release::sbx::probe()?)
        } else {
            None
        };
        let capabilities: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("capabilities.json"))?)?;
        let grants: Vec<String> = serde_json::from_value(capabilities["grants"].clone())?;
        let expected_grants = if sbx_check {
            vec!["docker-sbx", "sbx-test-identity", "public-network"]
        } else {
            vec!["hypervisor", "offline"]
        };
        ensure!(
            grants == expected_grants,
            "kit-corrupt: invalid native capability grant"
        );
        if sbx_check {
            crate::release::sbx::allocate(&a.out, &a.tmproot)?;
        }
        let home = tempfile::tempdir_in(&a.tmproot)?;
        let isolation = super::isolation::Run::for_home(home.path().to_owned())?;
        let workspace = home.path().join("workspace");
        // Every exit from the selection below, including the deadline and
        // early errors, reaches the owned-process sweep in `finish` before
        // the run's roots are removed; a leak is a recorded failure.
        let ran = (|| -> Result<(std::process::ExitStatus, Vec<crate::evidence::schema::ResultRecord>)> {
            std::fs::create_dir_all(workspace.join("crates/awman-cli"))?;
            std::fs::write(workspace.join("Cargo.toml"), "[workspace]\n")?;
            let mut command = runner(&root, "list");
            selection_at(&root, &workspace, &mut command, &kit.filter);
            command.args(["--message-format", "json"]);
            isolation.apply(&mut command);
            let listed = command.output()?;
            ensure!(
                listed.status.success()
                    && actions::expected(&serde_json::from_slice(&listed.stdout)?)? == kit.tests,
                "kit-corrupt: relocated inventory differs"
            );
            std::fs::write(a.out.join("expected.json"), &listed.stdout)?;
            let junit_path = workspace.join("target/nextest/default/junit.xml");
            if junit_path.exists() {
                std::fs::remove_file(&junit_path)?;
            }
            let mut command = runner(&root, "run");
            selection_at(&root, &workspace, &mut command, &kit.filter);
            command.args([
                "--profile",
                "default",
                "--no-fail-fast",
                "--no-tests",
                "fail",
            ]);
            isolation.apply(&mut command);
            command
                .env("AWMAN_TEST_ARTIFACT", &a.artifact)
                .env("AWMAN_TEST_ARTIFACT_SHA256", &a.artifact_sha256)
                .env("AWMAN_TEST_FIXTURES", absolute(&a.fixtures)?)
                .env("AWMAN_TEST_RUN_CONFIG", root.join("run-config.toml"))
                .env("AWMAN_TEST_TIER", "artifact")
                .env("AWMAN_TEST_TMPROOT", &a.tmproot)
                .env("AWMAN_TEST_EVIDENCE_DIR", absolute(&a.out)?)
                .env("AWMAN_TEST_HOST_RESOURCES", grants.join(","));
            if sbx_check { crate::release::sbx::grant(&mut command)?; }
            let log = std::fs::File::create(a.out.join("runner.log"))?;
            command.stdout(log.try_clone()?).stderr(log);
            // Its own process group, so the deadline stops every descendant.
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            let mut child = command.spawn()?;
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(kit.budget_seconds);
            let status = loop {
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                if std::time::Instant::now() >= deadline {
                    // Signal the group before reaping its leader, while the
                    // group ID cannot have been reused.
                    #[cfg(unix)]
                    let _ = nix::sys::signal::killpg(
                        nix::unistd::Pid::from_raw(child.id() as i32),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                    let _ = child.kill();
                    child.wait()?;
                    return Err(crate::release::error::Stop::new("timeout", "native kit deadline").into());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            };
            result["runner_log_sha256"] =
                serde_json::json!(digest::sha256_file(&a.out.join("runner.log"))?);
            result["command_exit"] = serde_json::json!(status.code());
            let xml = std::fs::read_to_string(&junit_path).context("kit-corrupt: missing JUnit")?;
            std::fs::write(a.out.join("junit.xml"), &xml)?;
            let records = junit::parse(&xml)?;
            let got: BTreeSet<_> = records.iter().map(|r| r.test.clone()).collect();
            result["tests"] = serde_json::json!(records
                .iter()
                .map(
                    |r| serde_json::json!({"id":r.test,"outcome":r.outcome,"duration_ms":r.duration_ms})
                )
                .collect::<Vec<_>>());
            result["expected_inventory_sha256"] =
                serde_json::json!(digest::sha256_file(&a.out.join("expected.json"))?);
            result["junit_sha256"] =
                serde_json::json!(digest::sha256_file(&a.out.join("junit.xml"))?);
            ensure!(got == kit.tests, "kit-corrupt: JUnit inventory differs");
            Ok((status, records))
        })();
        let cleaned = if sbx_check {
            crate::release::sbx::cleanup(&a.out, &a.tmproot)
        } else {
            Ok(false)
        };
        let swept = isolation.finish();
        enforce_cleanup(cleaned, swept)?;
        let (status, records) = ran?;
        if records.iter().any(|r| r.outcome == Outcome::Blocked) {
            return Err(crate::release::error::Stop::new("native-blocked", "required test reported blocked; inspect runner.log and JUnit for the missing capability").into());
        }
        ensure!(
            status.success() && records.iter().all(|r| r.outcome == Outcome::Passed),
            "native tests failed"
        );
        if let Some(q) = qualification {
            result["sbx"] = crate::release::sbx::collect(&a.out, &q)?;
        }
        Ok(())
    })();
    let code = match checked {
        Ok(()) => {
            result["outcome"] = serde_json::json!("passed");
            result["reason_code"] = serde_json::Value::Null;
            0
        }
        Err(e) => {
            let reason = format!("{e:#}");
            let code = blocked_reason(sbx_check, &e);
            result["outcome"] =
                serde_json::json!(if code.is_some() { "blocked" } else { "failed" });
            result["reason"] = serde_json::json!(reason);
            result["reason_code"] = serde_json::json!(code);
            if code.is_some() {
                2
            } else {
                1
            }
        }
    };
    let os = Command::new("/usr/bin/uname").arg("-v").output().ok();
    result["host"]["os_build"] =
        serde_json::json!(os.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()));
    result["finished"] = serde_json::json!(chrono::Utc::now());
    let path = a.out.join("result.json");
    let tmp = a.out.join("result.json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(&result)?)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)?;
    Ok(ExitCode::from(code))
}

/// Preserve the two offline environment contracts byte-for-byte; derive host grants
/// from the selected suites rather than granting every native run a hypervisor.
fn capability_contract(
    m: &Manifest,
    native: &crate::manifest::schema::Native,
) -> serde_json::Value {
    let grants: Vec<_> = [
        "hypervisor",
        "offline",
        "docker-sbx",
        "sbx-test-identity",
        "public-network",
    ]
    .into_iter()
    .filter(|cap| {
        native
            .suites
            .iter()
            .any(|id| m.suites[id].requires.iter().any(|c| c == cap))
    })
    .collect();
    serde_json::json!({"schema":1,"tier":"artifact","grants":grants,"private_home":true,
        "offline_enforced":if grants.contains(&"offline") {serde_json::json!("per-child")} else {serde_json::Value::Null}})
}
fn enforce_cleanup(cleaned: Result<bool>, swept: Result<()>) -> Result<()> {
    let leaked =
        cleaned.map_err(|e| anyhow::anyhow!("SBX cleanup unverified; result void: {e:#}"))?;
    ensure!(!leaked, "owned SBX sandbox survived its test; result void");
    swept.map_err(|e| anyhow::anyhow!("owned-process cleanup failed; result void: {e:#}"))
}

fn blocked_reason(sbx: bool, error: &anyhow::Error) -> Option<&'static str> {
    let infrastructure = [
        "timeout",
        "native-blocked",
        "no-sbx-cli",
        "sbx-cli-incompatible",
        "sbx-identity-missing",
        "sbx-not-signed-in",
        "sbx-identity-not-isolated",
        "sbx-network",
    ];
    let legacy = [
        "no-hypervisor",
        "fixture-digest",
        "socket-budget",
        "artifact-digest",
        "kit-corrupt",
    ];
    if let Some(stop) = error.downcast_ref::<crate::release::error::Stop>() {
        return infrastructure.into_iter().find(|code| stop.code == *code);
    }
    if sbx {
        return None;
    }
    // Existing offline-kit diagnostics retain their prefix contract.
    let reason = format!("{error:#}");
    legacy
        .into_iter()
        .find(|code| reason.starts_with(&format!("{code}:")))
}

/// Inspect content-addressed OCI config bytes without extracting host paths.
fn verify_oci_arch(path: &Path, arch: &str) -> Result<()> {
    use std::io::Read;
    let mut archive = tar::Archive::new(std::fs::File::open(path)?);
    let mut found = false;
    let mut count = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        count += 1;
        ensure!(count <= 100_000, "fixture-digest: oversized OCI archive");
        let name = entry.path()?.into_owned();
        ensure!(
            !name.is_absolute()
                && !name
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "fixture-digest: unsafe OCI path"
        );
        ensure!(
            entry.header().entry_type().is_file() || entry.header().entry_type().is_dir(),
            "fixture-digest: OCI links/devices forbidden"
        );
        if entry.header().entry_type().is_file()
            && entry.size() <= 1024 * 1024
            && name.starts_with("blobs/sha256")
        {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(actual) = v["architecture"].as_str() {
                    ensure!(actual == arch, "fixture-digest: OCI architecture mismatch");
                    found = true;
                }
            }
        }
    }
    ensure!(
        found,
        "fixture-digest: OCI archive lacks an architecture config"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_sbx_native_kit_receives_sbx_and_network_grants() {
        let m = crate::manifest::load(&crate::gates::root()).unwrap();
        let old = serde_json::json!({"schema":1,"tier":"artifact","grants":["hypervisor","offline"],"private_home":true,"offline_enforced":"per-child"});
        for id in ["native.macos-arm64", "native.linux-x64"] {
            assert_eq!(
                serde_json::to_vec_pretty(&capability_contract(&m, &m.release.native[id])).unwrap(),
                serde_json::to_vec_pretty(&old).unwrap()
            );
        }
        let sbx = capability_contract(
            &m,
            &m.release.native[crate::manifest::release_policy::SBX_NATIVE],
        );
        assert_eq!(
            sbx["grants"],
            serde_json::json!(["docker-sbx", "sbx-test-identity", "public-network"])
        );
        assert!(sbx["offline_enforced"].is_null());
    }
    #[test]
    fn sbx_prerequisites_and_deadlines_block_but_kit_or_product_defects_fail() {
        for reason in [
            "no-sbx-cli",
            "sbx-cli-incompatible",
            "sbx-identity-missing",
            "sbx-not-signed-in",
            "sbx-identity-not-isolated",
            "sbx-network",
            "timeout",
            "native-blocked",
        ] {
            assert_eq!(
                blocked_reason(
                    true,
                    &crate::release::error::Stop::new(reason, "prerequisite").into()
                ),
                Some(reason)
            );
        }
        for reason in [
            "kit-corrupt: invalid selection",
            "artifact-digest: mismatch",
            "native tests failed",
            "product timeout in sbx-network diagnostic",
            "timeout; owned SBX sandbox survived its test",
            "owned SBX sandbox survived its test; result void",
        ] {
            assert_eq!(blocked_reason(true, &anyhow::anyhow!(reason)), None);
        }
        assert_eq!(
            blocked_reason(false, &anyhow::anyhow!("kit-corrupt: missing JUnit")),
            Some("kit-corrupt")
        );
    }
    #[test]
    fn cleanup_failure_overrides_even_a_typed_blocked_primary_error() {
        for result in [
            enforce_cleanup(Ok(true), Ok(())),
            enforce_cleanup(
                Err(crate::release::error::Stop::new("timeout", "cleanup timeout").into()),
                Ok(()),
            ),
            enforce_cleanup(Ok(false), Err(anyhow::anyhow!("descendant leak"))),
        ] {
            let error = result.unwrap_err();
            assert_eq!(blocked_reason(true, &error), None);
        }
    }

    #[test]
    fn oci_fixture_architecture_is_checked_from_the_archive_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oci.tar");
        let mut tar = tar::Builder::new(std::fs::File::create(&path).unwrap());
        let config = br#"{"architecture":"amd64","os":"linux"}"#;
        let mut header = tar::Header::new_gnu();
        header.set_size(config.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        tar.append_data(&mut header, "blobs/sha256/config", config.as_slice())
            .unwrap();
        tar.finish().unwrap();
        drop(tar);
        verify_oci_arch(&path, "amd64").unwrap();
        assert!(verify_oci_arch(&path, "arm64").is_err());
    }
    #[test]
    fn native_selection_uses_standalone_metadata_and_disables_user_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("payload/target/nextest")).unwrap();
        std::fs::write(
            dir.path()
                .join("payload/target/nextest/cargo-metadata.json"),
            b"{}",
        )
        .unwrap();
        let mut c = runner(dir.path(), "run");
        selection(dir.path(), &mut c, "test(=signed_boot::required)");
        let args = describe(&c);
        assert!(args.contains("--cargo-metadata"));
        assert!(args.contains("--binaries-metadata"));
        assert!(args.contains("--target-dir-remap"));
        assert!(args.contains("--user-config-file none"));
        assert!(!args.contains("--archive-file"));
    }
}
