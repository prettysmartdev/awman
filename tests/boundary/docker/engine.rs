//! A granted Docker Engine as the boundary tests use it: the supervised
//! docker CLI for arranging and observing state, the engine's own
//! `EnvSnapshot` for the code under test, and the base image every test
//! image is built from.
//!
//! The base image is a small shell image the runner pre-pulls (the job
//! names it with the host resource `docker-base-image=<reference>`; the
//! default is [`DEFAULT_BASE_IMAGE`]). When it is neither present nor
//! pullable the test is `BLOCKED[docker]`, never skipped.

use awman::data::config::env::EnvSnapshot;
use awman_test_support::capability::Docker;
use awman_test_support::{Granted, Output, OwnedCommand, TestSandbox};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::support;

/// Longest a single `docker build` may run (template installers download
/// agents; the tier budget governs everything else).
pub const BUILD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(900);

/// Base image used when the run names none.
pub const DEFAULT_BASE_IMAGE: &str = "busybox:1.36.1";
static RESOURCE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// The Docker Engine granted to one test.
pub struct Engine<'a> {
    pub sandbox: &'a TestSandbox,
    pub docker: Docker,
}

impl<'a> Engine<'a> {
    /// Require the engine for `sandbox` and add what children need to reach
    /// it. The caller keeps the sandbox for the whole test.
    pub fn grant(sandbox: &'a mut TestSandbox) -> Engine<'a> {
        let granted: Granted<Docker> = sandbox.require::<Docker>();
        sandbox.grant(&granted);
        let docker = (*granted).clone();
        Engine {
            sandbox: &*sandbox,
            docker,
        }
    }

    /// The docker CLI as a supervised child of the sandbox.
    pub fn command(&self) -> OwnedCommand {
        self.sandbox
            .command(&self.docker.cli)
            .env("DOCKER_HOST", &self.docker.host)
            .label("docker")
    }

    /// Run `docker <args>` to completion.
    pub fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).run()
    }

    /// Run `docker <args>`, panicking with its output unless it succeeds;
    /// returns trimmed stdout.
    pub fn run_ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        out.assert_success();
        out.stdout_text().trim().to_string()
    }

    /// The engine as awman's own code sees it (`docker` on `PATH`,
    /// `AWMAN_TEST_DOCKER=1`, `DOCKER_HOST`).
    pub fn snapshot(&self) -> EnvSnapshot {
        support::env_snapshot(self.sandbox)
    }

    /// A per-sandbox suffix suitable for Docker names, tags and ownership labels.
    pub fn run_id(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        std::process::id().hash(&mut hash);
        self.sandbox.root().hash(&mut hash);
        format!("{:016x}", hash.finish())
    }

    pub fn unique_name(&self, prefix: &str) -> String {
        format!(
            "{prefix}-{}-{:x}",
            self.run_id(),
            RESOURCE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )
    }

    pub fn unique_tag(&self, prefix: &str) -> String {
        format!(
            "{prefix}:{}-{:x}",
            self.run_id(),
            RESOURCE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Whether the engine has `image`.
    pub fn has_image(&self, image: &str) -> bool {
        self.run(&["image", "inspect", "--format", "{{.Id}}", image])
            .exit
            .success()
    }

    /// Whether a container with this name or id exists (running or not).
    pub fn container_exists(&self, id_or_name: &str) -> bool {
        self.run(&["container", "inspect", "--format", "{{.Id}}", id_or_name])
            .exit
            .success()
    }

    /// Force-remove a container; absent is fine.
    pub fn remove_container(&self, id_or_name: &str) {
        let _ = self.run(&["rm", "-f", id_or_name]);
    }

    /// Remove an image; absent is fine.
    pub fn remove_image(&self, image: &str) {
        let _ = self.run(&["rmi", image]);
    }

    fn resource_owner(&self, kind: &str, reference: &str) -> Option<String> {
        let format = "{{.Id}} {{index .Config.Labels \"awman.test.owner\"}}";
        let result = self.run(&[kind, "inspect", "--format", format, reference]);
        result
            .exit
            .success()
            .then(|| result.stdout_text().trim().to_string())
    }

    /// The reference every test image is built from, present in the engine.
    /// Pulls it when absent; `BLOCKED[docker]` when it cannot be had.
    pub fn base_image(&self) -> String {
        let reference = awman_test_support::runtime_env::get()
            .host_resource("docker-base-image")
            .filter(|r| !r.is_empty())
            .unwrap_or(DEFAULT_BASE_IMAGE)
            .to_string();
        if !self.has_image(&reference) {
            let pulled = self.run(&["pull", "--quiet", &reference]);
            if !pulled.exit.success() {
                panic!(
                    "BLOCKED[docker]: base image {reference} is neither present in the engine nor \
                     pullable ({}); pre-pull it in the job or name another with \
                     docker-base-image=<reference> in AWMAN_TEST_HOST_RESOURCES",
                    pulled.stderr_text().trim()
                );
            }
        }
        reference
    }

    /// Build `tag` from `dockerfile` text in a fresh context directory
    /// named `name` under the sandbox; returns (Dockerfile, context).
    pub fn write_context(&self, name: &str, dockerfile: &str) -> (PathBuf, PathBuf) {
        let context = self.sandbox.root().join("contexts").join(name);
        std::fs::create_dir_all(&context)
            .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: {}: {e}", context.display()));
        let file = context.join("Dockerfile");
        std::fs::write(&file, dockerfile)
            .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: {}: {e}", file.display()));
        (file, context)
    }
}

/// Removes the named containers and images when dropped, including when the
/// test panics, so a failing boundary test leaves the engine as it found it.
pub struct Cleanup<'e, 'a> {
    engine: &'e Engine<'a>,
    owner: String,
    containers: std::cell::RefCell<Vec<(String, Option<String>)>>,
    images: std::cell::RefCell<Vec<(String, Option<String>)>>,
}

impl<'e, 'a> Cleanup<'e, 'a> {
    pub fn new(engine: &'e Engine<'a>) -> Cleanup<'e, 'a> {
        Cleanup {
            engine,
            owner: format!(
                "{}-{:x}",
                engine.run_id(),
                RESOURCE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ),
            containers: Default::default(),
            images: Default::default(),
        }
    }

    pub fn owner_token(&self) -> &str {
        &self.owner
    }

    pub fn owner_label(&self) -> String {
        format!("awman.test.owner={}", self.owner)
    }

    /// Build with this fixture world's ownership label and register the tag
    /// before Docker can create it, so drop cleanup also covers failed setup.
    pub fn build_image(&self, tag: &str, dockerfile: &Path, context: &Path) {
        self.image(tag);
        let out = self
            .engine
            .command()
            .args([
                "build",
                "--label",
                &self.owner_label(),
                "--tag",
                tag,
                "--file",
            ])
            .arg(dockerfile)
            .arg(context)
            .deadline(BUILD_DEADLINE)
            .run();
        out.assert_success();
    }

    /// Remove this container when the guard drops.
    pub fn container(&self, name: &str) -> &Self {
        self.containers.borrow_mut().push((name.to_string(), None));
        self
    }

    /// Record the daemon ID after a successful create, closing the setup gap.
    pub fn created_container(&self, name: &str) -> &Self {
        let id = self
            .engine
            .resource_owner("container", name)
            .and_then(|details| details.split_whitespace().next().map(str::to_string));
        self.containers.borrow_mut().push((name.to_string(), id));
        self
    }

    /// Remove this image when the guard drops.
    pub fn image(&self, reference: &str) -> &Self {
        self.images.borrow_mut().push((reference.to_string(), None));
        self
    }

    /// Record the image content ID after this world creates or tags it.
    pub fn created_image(&self, reference: &str) -> &Self {
        let id = self
            .engine
            .resource_owner("image", reference)
            .and_then(|details| details.split_whitespace().next().map(str::to_string));
        self.images.borrow_mut().push((reference.to_string(), id));
        self
    }
}

impl Drop for Cleanup<'_, '_> {
    fn drop(&mut self) {
        for name in self.containers.borrow().iter() {
            let details = self.engine.resource_owner("container", &name.0);
            let Some(details) = details else { continue };
            let mut fields = details.split_whitespace();
            let Some(id) = fields.next() else { continue };
            let owner = fields.next().unwrap_or("");
            let recorded = name.1.as_deref() == Some(id);
            if recorded || owner == self.owner {
                self.engine.remove_container(id);
            }
        }
        for image in self.images.borrow().iter() {
            let details = self.engine.resource_owner("image", &image.0);
            let Some(details) = details else { continue };
            let mut fields = details.split_whitespace();
            let Some(id) = fields.next() else { continue };
            let owner = fields.next().unwrap_or("");
            let recorded = image.1.as_deref() == Some(id);
            if recorded || owner == self.owner {
                // Removing the unique tag drops only this reference. Never
                // force-remove a content ID shared by another image tag.
                self.engine.remove_image(&image.0);
            }
        }
    }
}
