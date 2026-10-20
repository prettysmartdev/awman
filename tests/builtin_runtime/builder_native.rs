//! WI 0125 opt-in native builder scenarios: the real `awman` binary builds a
//! disposable project image and agent image into a real local source store
//! (Docker Engine over a Unix socket, or Apple Containers), imports them into
//! the builtin store, boots a real builtin guest that reads markers baked into
//! both images, rebuilds after the markers change, and finally runs the
//! cached image again with the builder service gone.
//!
//! Hardware tests (`builtin_hw_*`): skipped by `make test-fast`, and they
//! report SKIP/BLOCKED instead of passing when a prerequisite is missing.
//!
//! | gate | inputs |
//! |---|---|
//! | `AWMAN_TEST_BUILTIN=1` (KVM / hypervisor entitlement, feature binary) | as every `builtin_hw_*` test; no fixture archive is needed |
//! | Docker: `AWMAN_TEST_DOCKER=1` | `AWMAN_TEST_BUILDER_DOCKER_HOST=unix:///…` names the Engine socket the test may build in (explicit, never assumed); `AWMAN_TEST_DOCKER_CLI` optionally names the `docker` CLI (default: first on `PATH`) |
//! | Apple: `AWMAN_TEST_APPLE_CONTAINER=1`, macOS on Apple Silicon | a running Apple Containers service; `AWMAN_TEST_APPLE_CLI` (default `/usr/local/bin/container`) |
//! | both | `AWMAN_TEST_BUILDER_BASE_IMAGE` (default `alpine:3.20`): the base the project Dockerfile starts `FROM`; the store pulls it if absent |
//!
//! A native gate that is off (or an unsupported host for Apple) is a SKIP. A
//! native gate that is on with a missing input, CLI or service is BLOCKED and,
//! under `AWMAN_TEST_BUILTIN_REQUIRE_HW=1`, a failure. Neither ever passes.
//!
//! Only the harness drives the store directly (preflight, image IDs for
//! cleanup, stopping/starting the Apple service). awman reaches the builder
//! through a wrapper on a private `PATH`; when the builder must be unavailable
//! that wrapper is replaced by a trap that records every call and fails.
//! Every image the test builds carries a per-run unique name and is removed
//! from the source store when the test ends.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use crate::lifecycle::{awman, install_ready_ping_stub, run_bounded};

const MARKER_BASE: &str = "/etc/awman-wi125-base";
const MARKER_AGENT: &str = "/etc/awman-wi125-agent";

/// Synthetic `claude`: prints the prompt and both image markers.
const SYNTHETIC_AGENT: &str = r#"#!/bin/sh
prompt=""
if ! IFS= read -r -t 20 prompt; then prompt="$*"; fi
printf 'WI125_PROMPT=%s\n' "$prompt"
printf 'WI125_BASE=%s\n' "$(cat /etc/awman-wi125-base 2>/dev/null || echo missing)"
printf 'WI125_AGENT=%s\n' "$(cat /etc/awman-wi125-agent 2>/dev/null || echo missing)"
exit 0
"#;

fn truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn record(test: &str, outcome: &str, detail: &str) {
    eprintln!("{outcome}: {test}: {detail}");
    if let Ok(path) = std::env::var("AWMAN_TEST_BUILTIN_REPORT") {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{outcome}\t{test}\t{}", detail.replace('\n', " "));
        }
    }
}

/// An opt-in gate that is off: SKIP, never a pass and never a failure.
fn skip<T>(test: &str, reason: &str) -> Option<T> {
    record(test, "SKIP", reason);
    None
}

/// An opted-in prerequisite that is missing: BLOCKED, never a pass, and a
/// failure on hardware CI (`AWMAN_TEST_BUILTIN_REQUIRE_HW=1`).
fn blocked<T>(test: &str, reason: &str) -> Option<T> {
    record(test, "BLOCKED", reason);
    assert!(
        !truthy("AWMAN_TEST_BUILTIN_REQUIRE_HW"),
        "{test}: required (AWMAN_TEST_BUILTIN_REQUIRE_HW=1) but: {reason}"
    );
    None
}

/// The Apple scenario's gate: an unsupported host is a skip (checked before
/// the builtin gate, so a Linux KVM host does not report it blocked), then
/// the builtin hardware gate.
fn apple_host_gate() -> Option<PathBuf> {
    if !(cfg!(target_os = "macos") && std::env::consts::ARCH == "aarch64") {
        record(
            APPLE_TEST,
            "SKIP",
            "Apple Containers needs macOS on Apple Silicon; unsupported host",
        );
        return None;
    }
    builtin_gate(APPLE_TEST)
}

/// The builtin hardware gate, as `lifecycle::gate` but without the promoted
/// fixture archive: these scenarios build their own images.
fn builtin_gate(test: &str) -> Option<PathBuf> {
    if !truthy("AWMAN_TEST_BUILTIN") {
        record(
            test,
            "SKIP",
            "set AWMAN_TEST_BUILTIN=1 (opt-in hardware gate)",
        );
        assert!(
            !truthy("AWMAN_TEST_BUILTIN_REQUIRE_HW"),
            "{test}: hardware gate is required"
        );
        return None;
    }
    let binary = std::env::var_os("AWMAN_TEST_BUILTIN_ARTIFACT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_awman")));
    if !binary.is_file() {
        return blocked(
            test,
            "AWMAN_TEST_BUILTIN_ARTIFACT does not name an awman executable",
        );
    }
    if let Err(reason) = crate::gate::hypervisor(&binary) {
        return blocked(test, &reason);
    }
    let info = Command::new(&binary)
        .arg("__awman-builtin-info")
        .output()
        .expect("query the exact awman artifact");
    if !info.status.success() {
        return blocked(
            test,
            "the selected awman artifact lacks the builtin provider; run `make test-builtin`",
        );
    }
    Some(binary)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn text(output: &Output) -> String {
    format!(
        "--stdout--\n{}\n--stderr--\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The source store a scenario builds into.
enum Store {
    Docker {
        cli: PathBuf,
        /// The real Engine socket, `unix://…`.
        real_host: String,
    },
    Apple {
        cli: PathBuf,
    },
}

impl Store {
    fn helper(&self) -> &'static str {
        match self {
            Store::Docker { .. } => "docker",
            Store::Apple { .. } => "container",
        }
    }

    fn cli(&self) -> &Path {
        match self {
            Store::Docker { cli, .. } | Store::Apple { cli } => cli,
        }
    }

    /// A harness-only command against the real store.
    fn harness(&self) -> Command {
        match self {
            Store::Docker { cli, real_host } => {
                let mut command = Command::new(cli);
                command
                    .args(["--host", real_host])
                    .env("DOCKER_HOST", real_host)
                    .env_remove("DOCKER_CONTEXT");
                command
            }
            Store::Apple { cli } => Command::new(cli),
        }
    }

    /// Docker image ID of `reference`, for cleanup of superseded builds.
    fn image_id(&self, reference: &str) -> Option<String> {
        let Store::Docker { .. } = self else {
            return None;
        };
        let out = self
            .harness()
            .args(["image", "inspect", "--format", "{{.Id}}", reference])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .filter(|id| !id.is_empty())
    }
}

/// Removes every image this run built from the source store, however the
/// test ends. Declared before any service-restart guard so it drops last.
struct Cleanup<'a> {
    store: &'a Store,
    references: Vec<String>,
    ids: std::cell::RefCell<Vec<String>>,
}

impl Cleanup<'_> {
    fn remember_ids(&self) {
        for reference in &self.references {
            if let Some(id) = self.store.image_id(reference) {
                let mut ids = self.ids.borrow_mut();
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        // Agent before base: the agent image is layered on the base.
        for reference in self.references.iter().rev() {
            let mut command = self.store.harness();
            match self.store {
                Store::Docker { .. } => command.args(["image", "rm", "-f", reference]),
                Store::Apple { .. } => command.args(["rmi", reference]),
            };
            let _ = command.output();
        }
        for id in self.ids.borrow().iter().rev() {
            let _ = self
                .store
                .harness()
                .args(["image", "rm", "-f", id])
                .output();
        }
    }
}

/// Restarts the Apple service however the test ends.
struct AppleRestart(PathBuf);
impl Drop for AppleRestart {
    fn drop(&mut self) {
        let _ = Command::new(&self.0).args(["system", "start"]).status();
    }
}

/// One disposable project wired to a native builder.
struct Scenario {
    _temp: tempfile::TempDir,
    _state: tempfile::TempDir,
    binary: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    /// PATH with a wrapper that reaches the real builder CLI.
    live_bin: PathBuf,
    /// PATH where `docker`/`container`/`sbx` are traps.
    offline_bin: PathBuf,
    trap_log: PathBuf,
    project_tag: String,
    agent_tag: String,
    base_image: String,
}

impl Scenario {
    /// `source` is the `builtin.imageSource` value; `temp` is a short temp
    /// directory created by the caller (it may already hold a socket link).
    fn new(
        binary: &Path,
        store: &Store,
        temp: tempfile::TempDir,
        source: serde_json::Value,
    ) -> Self {
        let state = tempfile::Builder::new()
            .prefix("awb.")
            .tempdir_in("/tmp")
            .unwrap();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        // The folder name is the image stem: lowercase, unique per run.
        let stem = format!(
            "awwi125{}{}{nanos}",
            store.helper().chars().next().unwrap(),
            std::process::id()
        );
        let repo = temp.path().join(&stem);
        let home = temp.path().join("home");
        let live_bin = temp.path().join("live-bin");
        let offline_bin = temp.path().join("offline-bin");
        for dir in [
            &repo.join(".awman"),
            &home.join(".awman"),
            &live_bin,
            &offline_bin,
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("git is a required test tool")
            .success());
        let agent = temp.path().join("claude");
        write_executable(&agent, SYNTHETIC_AGENT);
        std::fs::write(
            repo.join(".awman/config.json"),
            serde_json::to_vec(&serde_json::json!({
                "agent": "claude", "auth": "none", "autoAgentAuthAccepted": true,
                "overlays": [format!("dir({}:/usr/local/bin/claude:ro)", agent.display())]
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            home.join(".awman/config.json"),
            serde_json::to_vec(&serde_json::json!({
                "runtime": "builtin-experimental", "default_agent": "claude",
                "auth": "none", "autoAgentAuthAccepted": true,
                "builtin": {
                    "stateDir": state.path().join("s"),
                    "builder": {"enabled": true},
                    "imageSource": source,
                    "network": {"mode": "none"}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        // The live PATH reaches the real builder through a wrapper (not a
        // symlink, so the CLI still finds its own install root).
        write_executable(
            &live_bin.join(store.helper()),
            &format!("#!/bin/sh\nexec '{}' \"$@\"\n", store.cli().display()),
        );
        let trap_log = temp.path().join("helper-called");
        for helper in ["docker", "container", "sbx"] {
            write_executable(
                &offline_bin.join(helper),
                &format!(
                    "#!/bin/sh\nprintf '%s %s\\n' {helper} \"$*\" >> '{}'\nexit 91\n",
                    trap_log.display()
                ),
            );
            if helper != store.helper() {
                // Only the matching builder is ever reachable.
                std::fs::copy(offline_bin.join(helper), live_bin.join(helper)).unwrap();
            }
        }
        for bin in [&live_bin, &offline_bin] {
            install_ready_ping_stub(bin, "claude", &temp.path().join("ready-pings"));
        }
        let scenario = Self {
            project_tag: format!("awman-{stem}:latest"),
            agent_tag: format!("awman-{stem}-claude:latest"),
            base_image: std::env::var("AWMAN_TEST_BUILDER_BASE_IMAGE")
                .unwrap_or_else(|_| "alpine:3.20".into()),
            _temp: temp,
            _state: state,
            binary: binary.to_path_buf(),
            repo,
            home,
            live_bin,
            offline_bin,
            trap_log,
        };
        scenario.write_markers("v1");
        scenario
    }

    fn marker(&self, which: &str, version: &str) -> String {
        format!(
            "{which}-{version}-{}",
            self.repo.file_name().unwrap().to_string_lossy()
        )
    }

    /// Project and agent Dockerfiles that bake `version` markers into each layer.
    fn write_markers(&self, version: &str) {
        std::fs::write(
            self.repo.join("Dockerfile.dev"),
            format!(
                "FROM {}\nRUN printf '%s\\n' '{}' > {MARKER_BASE}\n",
                self.base_image,
                self.marker("base", version)
            ),
        )
        .unwrap();
        std::fs::write(
            self.repo.join(".awman/Dockerfile.claude"),
            format!(
                "FROM {}\nRUN printf '%s\\n' '{}' > {MARKER_AGENT}\n",
                self.project_tag,
                self.marker("agent", version)
            ),
        )
        .unwrap();
    }

    fn awman(&self, bin: &Path, args: &[&str]) -> Command {
        let mut command = awman(&self.binary, &self.repo, &self.home, bin);
        command.args(args);
        command
    }

    fn ready(&self, bin: &Path, extra: &[&str]) -> serde_json::Value {
        let mut args = vec!["ready", "--json"];
        args.extend_from_slice(extra);
        let out = run_bounded(self.awman(bin, &args), Duration::from_secs(900));
        assert!(
            out.status.success(),
            "awman {args:?} failed:\n{}",
            text(&out)
        );
        let json: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("ready --json is not JSON ({e}):\n{}", text(&out)));
        assert_eq!(json["ready"], true, "{json}");
        json
    }

    /// The default agent's per-image result from `ready --json`.
    fn image_result<'j>(&self, json: &'j serde_json::Value) -> &'j serde_json::Value {
        json["image_results"]
            .as_array()
            .and_then(|all| all.iter().find(|r| r["tag"] == self.agent_tag.as_str()))
            .unwrap_or_else(|| panic!("no image result for {}: {json}", self.agent_tag))
    }

    /// Boot a builtin guest from the installed image; it must see `version`.
    fn exec_sees(&self, bin: &Path, version: &str) {
        let prompt = format!("wi125 {version}");
        let out = run_bounded(
            self.awman(
                bin,
                &[
                    "exec",
                    "prompt",
                    "--non-interactive",
                    "--agent",
                    "claude",
                    &prompt,
                ],
            ),
            Duration::from_secs(300),
        );
        assert!(out.status.success(), "builtin exec failed:\n{}", text(&out));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&format!("WI125_PROMPT={prompt}")),
            "{}",
            text(&out)
        );
        for which in ["base", "agent"] {
            let expected = format!(
                "WI125_{}={}",
                which.to_uppercase(),
                self.marker(which, version)
            );
            assert!(
                stdout.contains(&expected),
                "guest did not see {expected}:\n{}",
                text(&out)
            );
        }
    }

    fn assert_no_helper(&self, phase: &str) {
        assert!(
            !self.trap_log.exists(),
            "{phase}: awman invoked a host container helper: {}",
            std::fs::read_to_string(&self.trap_log).unwrap_or_default()
        );
    }

    /// Steps shared by both stores, up to the point where the builder is
    /// taken away; `take_builder_away` does that.
    fn run(&self, cleanup: &Cleanup<'_>, take_builder_away: impl FnOnce()) {
        // 1. Plain ready: both images are missing from the store, so the
        //    builder builds base then agent, and the agent image is imported.
        let first = self.ready(&self.live_bin, &[]);
        cleanup.remember_ids();
        let result = self.image_result(&first);
        assert_eq!(result["build"], "Done", "{first}");
        assert_eq!(result["import"], "Done", "{first}");
        assert!(
            first["base_image_results"].as_array().is_some_and(|b| b
                .iter()
                .any(|r| r["tag"] == self.project_tag.as_str() && r["build"] == "Done")),
            "the project base image was not built: {first}"
        );
        // 2. The guest boots from the import; launching never needs the builder.
        self.exec_sees(&self.offline_bin, "v1");
        self.assert_no_helper("first launch");

        // 3. Change both markers and rebuild. Plain ready reuses an image the
        //    store already holds, so the rebuild is requested with --build.
        self.write_markers("v2");
        let rebuilt = self.ready(&self.live_bin, &["--build"]);
        cleanup.remember_ids();
        let result = self.image_result(&rebuilt);
        assert_eq!(result["build"], "Done", "{rebuilt}");
        assert_eq!(result["import"], "Done", "{rebuilt}");
        self.exec_sees(&self.offline_bin, "v2");
        self.assert_no_helper("launch after rebuild");

        // 4. The builder service is gone: the cached image still boots.
        take_builder_away();
        self.exec_sees(&self.offline_bin, "v2");
        self.assert_no_helper("offline launch");

        // 5. Plain ready warns that the builder is unreachable and keeps the
        //    installed image instead of failing.
        let offline = self.ready(&self.offline_bin, &[]);
        let result = self.image_result(&offline);
        assert!(
            result["build"].get("Warn").is_some(),
            "an unreachable builder must be a warning: {offline}"
        );
        assert_eq!(result["import"], "Done", "{offline}");
        self.exec_sees(&self.offline_bin, "v2");
    }
}

const DOCKER_TEST: &str =
    "builtin_hw_builder_docker_local_build_import_rebuild_then_runs_without_builder";

#[test]
fn builtin_hw_builder_docker_local_build_import_rebuild_then_runs_without_builder() {
    let Some(binary) = builtin_gate(DOCKER_TEST) else {
        return;
    };
    let Some(store) = docker_store() else {
        return;
    };
    let Store::Docker { real_host, .. } = &store else {
        unreachable!()
    };
    // awman sees the Engine only through a link the test controls; removing
    // the link takes the builder (and the source store) away without changing
    // the configured endpoint the installed image was recorded with.
    let temp = tempfile::Builder::new()
        .prefix("awbd.")
        .tempdir_in("/tmp")
        .unwrap();
    let link = temp.path().join("d.sock");
    std::os::unix::fs::symlink(real_host.trim_start_matches("unix://"), &link).unwrap();
    let scenario = Scenario::new(
        &binary,
        &store,
        temp,
        serde_json::json!({"type": "docker-local", "host": format!("unix://{}", link.display())}),
    );
    let cleanup = Cleanup {
        store: &store,
        references: vec![scenario.project_tag.clone(), scenario.agent_tag.clone()],
        ids: Default::default(),
    };
    scenario.run(&cleanup, || {
        std::fs::remove_file(&link).unwrap();
    });
    eprintln!("PASS: {DOCKER_TEST}");
}

fn docker_store() -> Option<Store> {
    if !truthy("AWMAN_TEST_DOCKER") {
        return skip(
            DOCKER_TEST,
            "set AWMAN_TEST_DOCKER=1 (opt-in real Docker gate)",
        );
    }
    let Some(real_host) = std::env::var("AWMAN_TEST_BUILDER_DOCKER_HOST")
        .ok()
        .filter(|h| h.starts_with("unix://"))
    else {
        return blocked(
            DOCKER_TEST,
            "AWMAN_TEST_BUILDER_DOCKER_HOST must name the unix:// Docker Engine socket this test \
             may build in",
        );
    };
    if !Path::new(real_host.trim_start_matches("unix://")).exists() {
        return blocked(DOCKER_TEST, &format!("{real_host} does not exist"));
    }
    let Some(cli) = std::env::var_os("AWMAN_TEST_DOCKER_CLI")
        .map(PathBuf::from)
        .or_else(|| find_on_path("docker"))
        .filter(|p| p.is_file())
    else {
        return blocked(DOCKER_TEST, "no docker CLI (set AWMAN_TEST_DOCKER_CLI)");
    };
    let unreachable = format!("the Docker Engine at {real_host} is not reachable");
    let store = Store::Docker { cli, real_host };
    let ok = store
        .harness()
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        return blocked(DOCKER_TEST, &unreachable);
    }
    Some(store)
}

const APPLE_TEST: &str =
    "builtin_hw_builder_apple_local_build_import_rebuild_then_runs_without_builder";

#[test]
fn builtin_hw_builder_apple_local_build_import_rebuild_then_runs_without_builder() {
    let Some(binary) = apple_host_gate() else {
        return;
    };
    let Some(store) = apple_store() else {
        return;
    };
    let temp = tempfile::Builder::new()
        .prefix("awba.")
        .tempdir_in("/tmp")
        .unwrap();
    let scenario = Scenario::new(
        &binary,
        &store,
        temp,
        serde_json::json!({"type": "apple-local"}),
    );
    let cleanup = Cleanup {
        store: &store,
        references: vec![scenario.project_tag.clone(), scenario.agent_tag.clone()],
        ids: Default::default(),
    };
    // Declared after `cleanup`, so it drops (restarting the service) first,
    // even on a panic: removing the images needs the service.
    let restart = std::cell::RefCell::new(None);
    scenario.run(&cleanup, || {
        *restart.borrow_mut() = Some(AppleRestart(store.cli().to_path_buf()));
        let status = store
            .harness()
            .args(["system", "stop"])
            .status()
            .expect("run the Apple Containers CLI (test harness only)");
        assert!(status.success(), "container system stop failed");
    });
    drop(restart);
    eprintln!("PASS: {APPLE_TEST}");
}

fn apple_store() -> Option<Store> {
    if !truthy("AWMAN_TEST_APPLE_CONTAINER") {
        return skip(
            APPLE_TEST,
            "set AWMAN_TEST_APPLE_CONTAINER=1 (opt-in real Apple Containers gate)",
        );
    }
    let cli = std::env::var_os("AWMAN_TEST_APPLE_CLI")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/container"));
    if !cli.is_file() {
        return blocked(
            APPLE_TEST,
            &format!(
                "no Apple Containers CLI at {} (set AWMAN_TEST_APPLE_CLI)",
                cli.display()
            ),
        );
    }
    let store = Store::Apple { cli };
    let running = store
        .harness()
        .args(["system", "status"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !running {
        return blocked(
            APPLE_TEST,
            "the Apple Containers service is not running (`container system start`)",
        );
    }
    Some(store)
}
