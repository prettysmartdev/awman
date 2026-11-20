//! The container runtime conformance world backed by the real Docker
//! Engine: awman's `ContainerRuntime` driving the real docker CLI.
//!
//! Everything the suite arranges is named with the suite's own constants
//! (`awman-conformance/…`) or a `boundary-foreign-` prefix, and removed by
//! [`ContainerWorld::cleanup`] and by the test's [`Cleanup`] guard.

use super::argv_watch::ArgvWatcher;
use super::waiter::PollWaiter;
use crate::docker::engine::{Cleanup, Engine};
use awman::data::config::env::EnvSnapshot;
use awman::engine::conformance::container::{ContainerWorld, BUILD_MARKER, PRESENT_HOME};
use awman::engine::conformance::Waiter;
use awman_test_support::EnvMap;
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

pub struct RealDockerWorld<'e, 'a> {
    engine: &'e Engine<'a>,
    cleanup: Cleanup<'e, 'a>,
    present: String,
    absent: String,
    built: String,
    watcher: ArgvWatcher,
    contexts: RefCell<usize>,
}

impl<'e, 'a> RealDockerWorld<'e, 'a> {
    pub fn new(engine: &'e Engine<'a>) -> RealDockerWorld<'e, 'a> {
        RealDockerWorld {
            engine,
            cleanup: Cleanup::new(engine),
            present: engine.unique_name("awman-conformance-present"),
            absent: engine.unique_name("awman-conformance-absent"),
            built: engine.unique_name("awman-conformance-built"),
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
    fn present_image(&self) -> &str {
        &self.present
    }
    fn absent_image(&self) -> &str {
        &self.absent
    }
    fn built_image(&self) -> &str {
        &self.built
    }
    fn resource_name(&self, purpose: &str) -> String {
        self.engine
            .unique_name(&format!("awman-conformance-{purpose}"))
    }
    fn owner_labels(&self) -> Vec<(String, String)> {
        vec![("awman.test.owner".into(), self.cleanup.owner_token().into())]
    }
    fn build_labels(&self) -> Vec<(String, String)> {
        self.cleanup.image(&self.built);
        self.owner_labels()
    }
    fn track_container(&self, name: &str) {
        self.cleanup.container(name);
    }
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
        self.cleanup
            .build_image(&self.present, &dockerfile, &context);
        self.engine
            .run_ok(&["image", "inspect", "--format", "{{.Id}}", &self.present])
    }

    fn given_absent_images(&self) {
        for image in [&self.absent, &self.built] {
            if self.engine.has_image(image) {
                let expected = self.cleanup.owner_label();
                let actual = self.engine.run_ok(&[
                    "image",
                    "inspect",
                    "--format",
                    "{{index .Config.Labels \"awman.test.owner\"}}",
                    image,
                ]);
                assert_eq!(format!("awman.test.owner={actual}"), expected);
                self.engine.remove_image(image);
            }
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
            &format!("FROM {}\nRUN echo {BUILD_MARKER}\n{tail}", self.present),
        )
    }

    fn given_foreign_container(&self) -> String {
        let name = self.engine.unique_name("boundary-foreign-conformance");
        self.cleanup.container(&name);
        self.engine.run_ok(&[
            "run",
            "-d",
            "--name",
            &name,
            "--label",
            &self.cleanup.owner_label(),
            &self.present,
            "sleep",
            "3600",
        ]);
        self.cleanup.created_container(&name);
        name
    }

    fn container_exists(&self, id_or_name: &str) -> bool {
        self.engine.container_exists(id_or_name)
    }

    fn cli_argv_log(&self) -> Vec<Vec<String>> {
        self.watcher.seen()
    }

    fn cleanup(&self) {
        self.watcher.stop();
    }
}
