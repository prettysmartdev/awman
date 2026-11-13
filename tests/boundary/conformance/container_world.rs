//! The container runtime conformance world backed by the real Docker
//! Engine: awman's `ContainerRuntime` driving the real docker CLI.
//!
//! Everything the suite arranges is named with the suite's own constants
//! (`awman-conformance/…`) or a `boundary-foreign-` prefix, and removed by
//! [`ContainerWorld::cleanup`] and by the test's [`Cleanup`] guard.

use super::argv_watch::ArgvWatcher;
use super::waiter::PollWaiter;
use crate::docker::engine::Engine;
use awman::data::config::env::EnvSnapshot;
use awman::engine::conformance::container::{
    ContainerWorld, ABSENT_IMAGE, BUILD_MARKER, BUILT_IMAGE, PRESENT_HOME, PRESENT_IMAGE,
};
use awman::engine::conformance::Waiter;
use awman_test_support::EnvMap;
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

/// Name of the container the world keeps running and awman does not own.
const FOREIGN_CONTAINER: &str = "boundary-foreign-conformance";

pub struct RealDockerWorld<'e, 'a> {
    engine: &'e Engine<'a>,
    watcher: ArgvWatcher,
    contexts: RefCell<usize>,
}

impl<'e, 'a> RealDockerWorld<'e, 'a> {
    pub fn new(engine: &'e Engine<'a>) -> RealDockerWorld<'e, 'a> {
        RealDockerWorld {
            engine,
            watcher: ArgvWatcher::start(engine.sandbox),
            contexts: RefCell::new(0),
        }
    }
}

impl Waiter for RealDockerWorld<'_, '_> {
    fn wait_until(&self, what: &str, limit: Duration, condition: &mut dyn FnMut() -> bool) -> bool {
        PollWaiter.wait_until(what, limit, condition)
    }
}

impl ContainerWorld for RealDockerWorld<'_, '_> {
    fn subject(&self) -> String {
        format!("docker CLI (real engine {})", self.engine.docker.host)
    }

    fn env(&self) -> EnvSnapshot {
        self.engine.snapshot()
    }

    fn env_without_cli(&self) -> EnvSnapshot {
        // The sandbox's own empty `bin` directory is the only PATH entry, so
        // no docker CLI can be found while `AWMAN_TEST_DOCKER=1` still opts in.
        let mut env: EnvMap = self.engine.sandbox.env().clone();
        env.set("PATH", self.engine.sandbox.bin());
        let map = env
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        EnvSnapshot::from_full_env(map)
            .with_invocation(self.engine.sandbox.work(), vec!["awman".to_string()])
    }

    fn given_present_image(&self) -> String {
        let base = self.engine.base_image();
        let (dockerfile, context) = self.engine.write_context(
            "present",
            &format!("FROM {base}\nENV HOME={PRESENT_HOME}\n"),
        );
        self.engine.build(PRESENT_IMAGE, &dockerfile, &context);
        self.engine
            .run_ok(&["image", "inspect", "--format", "{{.Id}}", PRESENT_IMAGE])
    }

    fn given_absent_images(&self) {
        for image in [ABSENT_IMAGE, BUILT_IMAGE] {
            self.engine.remove_image(image);
        }
    }

    fn given_build_context(&self, failing: bool) -> (PathBuf, PathBuf) {
        let n = {
            let mut n = self.contexts.borrow_mut();
            *n += 1;
            *n
        };
        let tail = if failing { "RUN false\n" } else { "" };
        self.engine.write_context(
            &format!("build-{n}"),
            &format!("FROM {PRESENT_IMAGE}\nRUN echo {BUILD_MARKER}\n{tail}"),
        )
    }

    fn given_foreign_container(&self) -> String {
        self.engine.remove_container(FOREIGN_CONTAINER);
        self.engine.run_ok(&[
            "run",
            "-d",
            "--name",
            FOREIGN_CONTAINER,
            PRESENT_IMAGE,
            "sleep",
            "3600",
        ]);
        FOREIGN_CONTAINER.to_string()
    }

    fn container_exists(&self, id_or_name: &str) -> bool {
        self.engine.container_exists(id_or_name)
    }

    fn cli_argv_log(&self) -> Vec<Vec<String>> {
        self.watcher.seen()
    }

    fn cleanup(&self) {
        self.watcher.stop();
        self.engine.remove_container(FOREIGN_CONTAINER);
        for name in self.engine.container_names() {
            if name.starts_with("awman-conformance-") {
                self.engine.remove_container(&name);
            }
        }
        for image in [PRESENT_IMAGE, BUILT_IMAGE, ABSENT_IMAGE] {
            self.engine.remove_image(image);
        }
    }
}
