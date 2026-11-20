//! Container runtime conformance: awman's [`ContainerRuntime`] driving one
//! container CLI (Docker, or a CLI-compatible implementation such as the
//! fake Docker CLI).
//!
//! Contracts checked (IDs from the slice's contract list, `L1-CTR-*`):
//! availability and the precise missing-CLI error, image presence/identity/
//! HOME, image builds (output streamed, failure typed with its exit code),
//! background containers (exec relays stdout/stderr/exit code, host values
//! reach the container by name only and never appear in any CLI argv,
//! explicit kill and drop both remove the container), owned-container
//! listing (awman's containers only), and typed errors for a missing image.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use super::{ensure, Report, Waiter};
use crate::data::config::env::EnvSnapshot;
use crate::engine::auth::credential::SecretString;
use crate::engine::container::background::BackgroundLaunchRequest;
use crate::engine::container::{BuildRequest, ContainerRuntime};
use crate::engine::error::EngineError;

/// `HOME` declared in the present image's config.
pub const PRESENT_HOME: &str = "/home/conformance";
/// The command a background exec check runs, and what it must report.
pub const EXEC_COMMAND: &str = "printf conformance-out; printf conformance-err >&2; exit 3";
/// The host value delivered to a background container by name.
pub const ENV_NAME: &str = "AWMAN_CONFORMANCE_VALUE";
pub const ENV_VALUE: &str = "value-by-name";
/// A secret delivered to a background container; it must never appear in argv.
pub const SECRET_NAME: &str = "AWMAN_CONFORMANCE_SECRET";
pub const SECRET_VALUE: &str = "test-secret-conformance-7f3a";
/// Reads both values back inside the container.
pub const ENV_COMMAND: &str =
    "printf '%s|%s' \"$AWMAN_CONFORMANCE_VALUE\" \"$AWMAN_CONFORMANCE_SECRET\"";
/// What [`ENV_COMMAND`] prints when both values arrived.
pub const ENV_EXPECTED: &str = "value-by-name|test-secret-conformance-7f3a";
/// A line every successful conformance build prints.
pub const BUILD_MARKER: &str = "awman-conformance-build-step";

/// The world a container runtime suite runs in. Every method arranges or
/// observes state; none of them call awman.
pub trait ContainerWorld: Waiter {
    /// Name of the implementation, e.g. `docker CLI (fake)`.
    fn subject(&self) -> String;
    fn present_image(&self) -> &str;
    fn absent_image(&self) -> &str;
    fn built_image(&self) -> &str;
    fn resource_name(&self, purpose: &str) -> String;
    fn owner_labels(&self) -> Vec<(String, String)>;
    /// Register before a production create, so even a failed launch is cleaned by label.
    fn track_container(&self, name: &str);
    fn build_labels(&self) -> Vec<(String, String)>;
    /// Environment under which `docker` resolves to the CLI under test.
    fn env(&self) -> EnvSnapshot;
    /// Environment under which no container CLI can be found.
    fn env_without_cli(&self) -> EnvSnapshot;
    /// Make the present image exist with `HOME=`[`PRESENT_HOME`]; return its id.
    fn given_present_image(&self) -> String;
    /// Make sure the absent and build image references do not exist.
    fn given_absent_images(&self);
    /// A build context whose Dockerfile builds the build image from the
    /// present image and prints [`BUILD_MARKER`]; `failing` makes the build
    /// fail with a non-zero exit after printing the marker.
    fn given_build_context(&self, failing: bool) -> (PathBuf, PathBuf);
    /// A running container that awman does not own; returns its name.
    fn given_foreign_container(&self) -> String;
    /// Whether a container with this id or name exists (running or not).
    fn container_exists(&self, id_or_name: &str) -> bool;
    /// Every argument vector the CLI was invoked with since the world was
    /// created (a real world records them with a logging shim on PATH).
    fn cli_argv_log(&self) -> Vec<Vec<String>>;
    /// Remove whatever the world created.
    fn cleanup(&self);
}

/// How long one runtime call may take before the check fails.
const STEP_LIMIT: Duration = Duration::from_secs(60);

/// Run every container runtime check against `world`.
pub fn container_runtime(world: &dyn ContainerWorld) -> Report {
    let mut report = Report::new(world.subject());
    let runtime = ContainerRuntime::docker_in(world.env());

    report.check("availability_reports_a_reachable_runtime", {
        runtime
            .availability()
            .map_err(|e| format!("reachable runtime reported unavailable: {e}"))
    });
    report.check("a_missing_cli_is_unavailable_naming_the_binary", {
        let missing = ContainerRuntime::docker_in(world.env_without_cli());
        match missing.availability() {
            Err(EngineError::ContainerRuntimeUnavailable { binary }) => {
                ensure(binary.ends_with("docker"), || {
                    format!("unavailable error names {binary:?}, not docker")
                })
            }
            other => Err(format!(
                "expected ContainerRuntimeUnavailable, got {other:?}"
            )),
        }
    });

    let present_id = world.given_present_image();
    world.given_absent_images();
    report.check(
        "image_presence_identity_and_home_are_read_from_the_store",
        check_images(world, &runtime, &present_id),
    );
    report.check("a_build_streams_output_and_creates_the_image", {
        check_build(world, &runtime, false)
    });
    report.check("a_failed_build_is_a_typed_error_with_its_exit_code", {
        check_build(world, &runtime, true)
    });
    report.check("background_exec_relays_stdout_stderr_and_exit_code", {
        check_background_exec(world, &runtime)
    });
    report.check(
        "host_values_reach_the_container_by_name_and_never_appear_in_argv",
        check_background_env(world, &runtime),
    );
    report.check("kill_and_drop_both_remove_a_background_container", {
        check_background_cleanup(world, &runtime)
    });
    report.check("starting_from_a_missing_image_is_image_not_found", {
        check_missing_image(world, &runtime)
    });
    report.check("listing_returns_awman_containers_only", {
        check_listing(world, &runtime)
    });
    world.cleanup();
    report
}

fn check_images(
    world: &dyn ContainerWorld,
    runtime: &ContainerRuntime,
    present_id: &str,
) -> Result<(), String> {
    ensure(runtime.image_exists(world.present_image()), || {
        format!("{} reported absent", world.present_image())
    })?;
    ensure(!runtime.image_exists(world.absent_image()), || {
        format!("{} reported present", world.absent_image())
    })?;
    let id = runtime.image_id(world.present_image());
    ensure(id.as_deref() == Some(present_id), || {
        format!("image id {id:?}, world says {present_id:?}")
    })?;
    ensure(runtime.image_id(world.absent_image()).is_none(), || {
        "an absent image has an id".to_string()
    })?;
    let home = runtime.image_home_dir(world.present_image());
    ensure(home.as_deref() == Some(PRESENT_HOME), || {
        format!("image HOME {home:?}, expected {PRESENT_HOME}")
    })?;
    ensure(
        runtime.image_home_dir(world.absent_image()).is_none(),
        || "an absent image has a HOME".to_string(),
    )
}

fn check_build(
    world: &dyn ContainerWorld,
    runtime: &ContainerRuntime,
    failing: bool,
) -> Result<(), String> {
    world.given_absent_images();
    let (dockerfile, context) = world.given_build_context(failing);
    let mut request = BuildRequest::runtime(world.built_image(), &dockerfile, &context, true);
    request.labels = world.build_labels().into_iter().collect();
    let mut lines = Vec::new();
    let result = runtime.build_image(&request, &mut |line| lines.push(line.to_string()));
    ensure(lines.iter().any(|l| l.contains(BUILD_MARKER)), || {
        format!("build output never showed {BUILD_MARKER:?}: {lines:?}")
    })?;
    match (failing, result) {
        (false, Ok(())) => ensure(runtime.image_exists(world.built_image()), || {
            format!("{} absent after a successful build", world.built_image())
        }),
        (false, Err(e)) => Err(format!("build failed: {e}")),
        (true, Err(EngineError::ImageBuildExitNonzero { tag, exit_code, .. })) => {
            ensure(tag == world.built_image() && exit_code != 0, || {
                format!("failure names tag {tag:?} exit {exit_code}")
            })?;
            ensure(!runtime.image_exists(world.built_image()), || {
                format!("{} present after a failed build", world.built_image())
            })
        }
        (true, other) => Err(format!("expected ImageBuildExitNonzero, got {other:?}")),
    }
}

fn launch<'a>(
    world: &'a dyn ContainerWorld,
    labels: &'a [(String, String)],
    workdir: &'a std::path::Path,
    env: &'a HashMap<String, String>,
    secrets: &'a [(String, SecretString)],
    name: &'a str,
) -> BackgroundLaunchRequest<'a> {
    BackgroundLaunchRequest {
        labels,
        secret_env: secrets,
        name: Some(name),
        image: world.present_image(),
        workdir,
        env,
        overlays: &[],
        builtin_launch: None,
    }
}

fn workdir(world: &dyn ContainerWorld) -> PathBuf {
    world.env().cwd().to_path_buf()
}

fn check_background_exec(
    world: &dyn ContainerWorld,
    runtime: &ContainerRuntime,
) -> Result<(), String> {
    let labels = world.owner_labels();
    let dir = workdir(world);
    let env = HashMap::new();
    let name_exec = world.resource_name("exec");
    world.track_container(&name_exec);
    let request = launch(world, &labels, &dir, &env, &[], &name_exec);
    let container = runtime
        .launch_background(&request)
        .map_err(|e| format!("start: {e}"))?;
    let id = container.container_id().to_string();
    let output = container
        .exec(EXEC_COMMAND, None)
        .map_err(|e| format!("exec: {e}"))?;
    container.kill().map_err(|e| format!("kill: {e}"))?;
    ensure(output.exit_code == 3, || {
        format!("exit code {} (expected 3)", output.exit_code)
    })?;
    ensure(output.stdout == "conformance-out", || {
        format!("stdout {:?}", output.stdout)
    })?;
    ensure(output.stderr == "conformance-err", || {
        format!("stderr {:?}", output.stderr)
    })?;
    ensure(!world.container_exists(&id), || {
        format!("container {id} still exists after kill")
    })
}

fn check_background_env(
    world: &dyn ContainerWorld,
    runtime: &ContainerRuntime,
) -> Result<(), String> {
    let labels = world.owner_labels();
    let dir = workdir(world);
    let env = HashMap::from([(ENV_NAME.to_string(), ENV_VALUE.to_string())]);
    let secrets = vec![(SECRET_NAME.to_string(), SecretString::new(SECRET_VALUE))];
    let name_env = world.resource_name("env");
    world.track_container(&name_env);
    let request = launch(world, &labels, &dir, &env, &secrets, &name_env);
    let container = runtime
        .launch_background(&request)
        .map_err(|e| format!("start: {e}"))?;
    let output = container.exec(ENV_COMMAND, None);
    container.kill().map_err(|e| format!("kill: {e}"))?;
    let output = output.map_err(|e| format!("exec: {e}"))?;
    ensure(output.stdout == ENV_EXPECTED, || {
        format!(
            "container saw {:?}, expected {ENV_EXPECTED:?}",
            output.stdout
        )
    })?;
    let leaks: Vec<Vec<String>> = world
        .cli_argv_log()
        .into_iter()
        .filter(|argv| {
            argv.iter()
                .any(|a| a.contains(SECRET_VALUE) || a.contains(ENV_VALUE))
        })
        .collect();
    ensure(leaks.is_empty(), || {
        format!("a value appeared in CLI argv: {leaks:?}")
    })
}

fn check_background_cleanup(
    world: &dyn ContainerWorld,
    runtime: &ContainerRuntime,
) -> Result<(), String> {
    let labels = world.owner_labels();
    let dir = workdir(world);
    let env = HashMap::new();
    let name_kill = world.resource_name("kill");
    world.track_container(&name_kill);
    let killed = runtime
        .launch_background(&launch(world, &labels, &dir, &env, &[], &name_kill))
        .map_err(|e| format!("start: {e}"))?;
    let killed_id = killed.container_id().to_string();
    ensure(world.container_exists(&killed_id), || {
        format!("{killed_id} not visible after start")
    })?;
    killed.kill().map_err(|e| format!("kill: {e}"))?;
    let name_drop = world.resource_name("drop");
    world.track_container(&name_drop);
    let dropped = runtime
        .launch_background(&launch(world, &labels, &dir, &env, &[], &name_drop))
        .map_err(|e| format!("start: {e}"))?;
    let dropped_id = dropped.container_id().to_string();
    drop(dropped);
    for id in [killed_id, dropped_id] {
        let gone = world.wait_until(&format!("container {id} removed"), STEP_LIMIT, &mut || {
            !world.container_exists(&id)
        });
        ensure(gone, || format!("container {id} still exists"))?;
    }
    Ok(())
}

fn check_missing_image(
    world: &dyn ContainerWorld,
    runtime: &ContainerRuntime,
) -> Result<(), String> {
    let labels = world.owner_labels();
    let dir = workdir(world);
    let env = HashMap::new();
    let name_missing = world.resource_name("missing");
    world.track_container(&name_missing);
    let mut request = launch(world, &labels, &dir, &env, &[], &name_missing);
    request.image = world.absent_image();
    match runtime.launch_background(&request) {
        Err(EngineError::ContainerImageNotFound { image }) => {
            ensure(image == world.absent_image(), || {
                format!("error names {image:?}")
            })
        }
        Err(other) => Err(format!("expected ContainerImageNotFound, got {other}")),
        Ok(container) => {
            let id = container.container_id().to_string();
            let _ = container.kill();
            Err(format!("started {id} from a missing image"))
        }
    }
}

fn check_listing(world: &dyn ContainerWorld, runtime: &ContainerRuntime) -> Result<(), String> {
    let foreign = world.given_foreign_container();
    let labels = world.owner_labels();
    let dir = workdir(world);
    let env = HashMap::new();
    let name_listed = world.resource_name("listed");
    world.track_container(&name_listed);
    let ours = runtime
        .launch_background(&launch(world, &labels, &dir, &env, &[], &name_listed))
        .map_err(|e| format!("start: {e}"))?;
    let listed = runtime.list_running_all();
    let ours_name = name_listed;
    ours.kill().map_err(|e| format!("kill: {e}"))?;
    let listed = listed.map_err(|e| format!("list: {e}"))?;
    ensure(listed.iter().any(|h| h.name == ours_name), || {
        format!("own container missing from {listed:?}")
    })?;
    ensure(!listed.iter().any(|h| h.name == foreign), || {
        format!("foreign container {foreign} listed: {listed:?}")
    })
}
