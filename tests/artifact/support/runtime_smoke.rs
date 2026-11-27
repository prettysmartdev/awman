//! Exact-asset sessions on an explicitly granted, owned container service.
use super::{artifact, runtime_owned, sandbox};
use awman_test_support::capability::{AppleContainers, Docker, GuestFixtures};
use awman_test_support::fixtures::RepoBuilder;
use awman_test_support::{Exit, TestSandbox};
use runtime_owned::{Reservation, Service};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

struct Cli<'a> {
    sandbox: &'a TestSandbox,
    exe: PathBuf,
}
impl Service for Cli<'_> {
    fn output(&self, args: &[&str]) -> anyhow::Result<Vec<u8>> {
        let o = self
            .sandbox
            .command(&self.exe)
            .args(args)
            .deadline(Duration::from_secs(120))
            .run();
        anyhow::ensure!(
            o.exit.success(),
            "runtime command failed: {args:?}: {}",
            o.stderr_text()
        );
        Ok(o.stdout)
    }
}
struct Owned<'a> {
    r: Reservation,
    cli: Cli<'a>,
    out: PathBuf,
    cleaned: bool,
}
impl Owned<'_> {
    fn clean(&mut self) -> anyhow::Result<()> {
        runtime_owned::reconcile(&self.r, &self.cli, &self.out)?;
        self.cleaned = true;
        Ok(())
    }
}
impl Drop for Owned<'_> {
    fn drop(&mut self) {
        if !self.cleaned {
            if let Err(e) = self.clean() {
                eprintln!("owned runtime cleanup failed: {e:#}");
            }
        }
    }
}

fn mount_sources(v: &Value) -> Vec<PathBuf> {
    let row = v.get(0).expect("runtime inspect must return an array");
    let mounts = row
        .get("Mounts")
        .or_else(|| row.pointer("/configuration/mounts"))
        .and_then(Value::as_array)
        .expect("runtime inspect must expose mounts");
    mounts
        .iter()
        .filter_map(|m| {
            let source = m
                .get("Source")
                .or_else(|| m.get("source"))
                .and_then(Value::as_str)
                .expect("runtime mount must expose its host source");
            // Guest tmpfs mounts have no host source.
            if source.is_empty() || source == "tmpfs" {
                None
            } else {
                Some(PathBuf::from(source))
            }
        })
        .collect()
}

pub fn smoke(runtime: &str, id: &'static str) {
    let mut s = sandbox(id);
    // Check the named service first, so a bare invocation reports its missing
    // capability even when no release asset was supplied.
    let cli = if runtime == "apple" {
        let cap = s.require::<AppleContainers>();
        let cli = cap.cli.clone();
        s.grant(&cap);
        cli
    } else {
        let cap = s.require::<Docker>();
        let cli = cap.cli.clone();
        s.grant(&cap);
        cli
    };
    let a = artifact(&mut s);
    let fixtures = s.require::<GuestFixtures>();
    let env = awman_test_support::runtime_env::get();
    let out = env
        .evidence_dir
        .as_ref()
        .unwrap_or_else(|| panic!("BLOCKED[{runtime}]: kit runtime reservation required"));
    let root = env.tmproot.as_ref().expect("kit temporary root");
    let mut r = Reservation::read(out, root)
        .unwrap_or_else(|e| panic!("BLOCKED[fixture-ownership]: reservation: {e:#}"));
    assert_eq!(r.runtime, runtime);
    assert_eq!(
        cli.canonicalize().unwrap(),
        r.cli_path,
        "BLOCKED[fixture-ownership]: artifact service differs from the kit probe"
    );
    assert_eq!(
        awman_test_support::fixtures::sha256_file(&cli).unwrap(),
        r.cli_sha256,
        "service CLI changed before the test"
    );
    if let Some(engine) = &r.engine {
        assert_eq!(
            s.env().get("DOCKER_HOST").unwrap().to_string_lossy(),
            *engine,
            "runtime endpoint differs from reservation"
        );
    }
    assert!(!r.armed, "reservation was already consumed");
    assert!(
        chrono::DateTime::parse_from_rfc3339(&r.deadline).unwrap() > chrono::Utc::now(),
        "runtime reservation expired"
    );
    let cli = Cli {
        sandbox: &s,
        exe: cli,
    };
    assert_eq!(
        runtime_owned::inventory(&r, &cli).unwrap().len(),
        0,
        "BLOCKED[fixture-ownership]: reserved names already exist"
    );
    r.armed = true;
    r.write(out).unwrap();
    let mut owned = Owned {
        r,
        cli,
        out: out.clone(),
        cleaned: false,
    };
    let private = s.work().join("owned-fixture.tar");
    runtime_owned::private_archive(
        &fixtures.paths["guest-oci-base"],
        &private,
        &owned.r.image_tag,
        runtime == "docker",
    )
    .unwrap();
    s.command(&owned.cli.exe)
        .args(if runtime == "apple" {
            vec!["image", "load", "--input"]
        } else {
            vec!["load", "--input"]
        })
        .arg(&private)
        .deadline(Duration::from_secs(120))
        .run()
        .assert_success();
    assert!(
        runtime_owned::inventory(&owned.r, &owned.cli)
            .unwrap()
            .images
            .contains(&owned.r.image_tag),
        "local OCI load did not install the reserved tag; no network fallback is permitted"
    );
    let repo = RepoBuilder::new(&s, &owned.r.project)
        .file(
            "Dockerfile.dev",
            format!("FROM {}\nLABEL {}\n", owned.r.image_tag, owned.r.label),
        )
        .file(
            ".awman/Dockerfile.claude",
            format!("FROM awman-{}:latest\n", owned.r.project),
        )
        .init();
    let synthetic = s.root().join("synthetic-agent");
    std::fs::write(&synthetic, "#!/bin/sh\nIFS= read -r prompt\nprintf 'GUEST_PROMPT=%s\\n' \"$prompt\"\nprintf 'GUEST_OS=%s\\n' \"$(uname -s)\"\necho guest-stderr >&2\necho SMOKE_AWAIT\ni=0\nwhile [ ! -f .smoke-release ]; do\n i=$((i+1)); [ \"$i\" -lt 90 ] || exit 74\n sleep 1\ndone\nexit 37\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&synthetic, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        repo.path().join(".awman/config.json"),
        serde_json::to_vec(&json!({
            "agents":{"default":"claude","auth":{"mode":"none","refresh":{"enabled":false}}},
            "overlays":[format!("dir({}:/usr/local/bin/claude:ro)", synthetic.display())]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        s.awman_home().join("config.json"),
        serde_json::to_vec(&json!({"runtime":{"kind":runtime}})).unwrap(),
    )
    .unwrap();
    // ImagesOnly retains runtime selection and prepare/build, and cannot invoke
    // a host provider ping. The synthetic executable is never on host PATH.
    let ready = s
        .command(&a.exe)
        .current_dir(repo.path())
        .args(["ready", "--prepare-only", "--json"])
        .deadline(Duration::from_secs(300))
        .run();
    ready.assert_success();
    assert_eq!(
        serde_json::from_slice::<Value>(&ready.stdout).unwrap()["ready"],
        true
    );
    let mut task = s
        .command(&a.exe)
        .current_dir(repo.path())
        .args(["exec", "prompt", "-n", "--agent", "claude", "smoke-proof"])
        .deadline(Duration::from_secs(120))
        .spawn();
    task.wait_for_stdout("SMOKE_AWAIT", Duration::from_secs(90));
    let running = runtime_owned::inventory(&owned.r, &owned.cli).unwrap();
    assert_eq!(
        running.containers.len(),
        1,
        "a real owned container must be running"
    );
    let inspection = owned
        .cli
        .output(&["inspect", &running.containers[0]])
        .unwrap();
    let sources = mount_sources(&serde_json::from_slice::<Value>(&inspection).unwrap());
    assert!(!sources.is_empty(), "no host mounts observed");
    let sandbox_root = s.root().canonicalize().unwrap();
    let repo_root = repo.path().canonicalize().unwrap();
    for source in &sources {
        let canonical = source.canonicalize().unwrap();
        assert!(
            canonical == repo_root || !repo_root.starts_with(&canonical),
            "mount contains a parent of the private repository: {}",
            source.display()
        );
        assert!(
            source.canonicalize().unwrap().starts_with(&sandbox_root),
            "mount escaped private test state: {}",
            source.display()
        );
    }
    assert!(
        sources
            .iter()
            .any(|p| p.canonicalize().unwrap() == repo.path().canonicalize().unwrap()),
        "private repo was not mounted"
    );
    std::fs::write(repo.path().join(".smoke-release"), "release\n").unwrap();
    assert_eq!(task.wait(Duration::from_secs(30)), Exit::Code(37));
    let stdout = task.stdout_so_far();
    let stderr = task.stderr_so_far();
    assert!(stdout.contains("GUEST_PROMPT=smoke-proof"), "{stdout}");
    assert!(stdout.contains("GUEST_OS=Linux"), "{stdout}");
    assert!(stderr.contains("guest-stderr"), "{stderr}");
    drop(task);
    owned.clean().unwrap();
    std::fs::write(out.join("runtime-smoke.json"), serde_json::to_vec_pretty(&json!({
        "schema":1,"runtime":runtime,"owner":owned.r.owner,"project":owned.r.project,
        "artifact_sha256":a.sha256,"ready_exit":0,"task_exit":37,"guest_os":"Linux",
        "prompt_observed":true,"stderr_observed":true,"mount_sources":sources,
        "containers":running.containers,"images":owned.r.images(),"removed":true,"verified_absent":true
    })).unwrap()).unwrap();
    drop(owned);
    drop(repo);
    s.finish();
}
