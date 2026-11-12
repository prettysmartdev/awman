//! Foreground containers for workflow setup/teardown steps.
//!
//! A setup/teardown step has two ways to run. Headless, it is `exec`'d into a
//! [`BackgroundContainer`](super::BackgroundContainer) with no terminal. In
//! the foreground, which this module describes, the step's command *is* the
//! container's main process: the container is interactive (the frontend's
//! PTY is attached, as it is for an agent) and exits when the command does.
//!
//! Both shapes carry the same workspace mount, overlays and environment rules,
//! so they are defined side by side in this layer (see
//! `build_start_background_argv` for the headless one).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::engine::container::options::{
    ContainerOption, Entrypoint, EnvLiteral, ImageRef, OverlayPermission, OverlaySpec,
};

/// Everything a foreground setup/teardown step container needs.
#[derive(Clone)]
pub struct PhaseStepContainerSpec<'a> {
    /// Session-scoped settings, also required by interactive builtin steps.
    pub builtin_launch: Option<&'a crate::data::config::builtin_launch::BuiltinLaunchSettings>,
    /// The project's base image.
    pub image: &'a str,
    /// The workspace, mounted read-write at its own path and used as the
    /// working directory.
    pub workdir: &'a Path,
    /// The step's resolved overlays.
    pub overlays: &'a [OverlaySpec],
    /// Host-resolved `env()` overlay values. Passed through by name only.
    pub env: &'a HashMap<String, String>,
    /// The step's shell command, run with `sh -c`.
    pub command: &'a str,
    /// The step's own `env` entries from the workflow file.
    pub step_env: Option<&'a HashMap<String, String>>,
}

impl std::fmt::Debug for PhaseStepContainerSpec<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhaseStepContainerSpec")
            .field("image", &self.image)
            .field("workdir", &self.workdir)
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl PhaseStepContainerSpec<'_> {
    /// The container options for this step.
    ///
    /// - The entrypoint is `sh -c <command>`, so the command is the only thing
    ///   the container runs.
    /// - It is interactive, so the frontend's PTY is attached.
    /// - The startup-grace kill is disabled: an agent that prints nothing for
    ///   30s has failed to start, but `sleep 60` or a quiet `npm ci` is healthy.
    /// - The whole transcript is kept, for the step's failure file.
    /// - `env()` values stay out of argv: they are passed through by name and
    ///   the runtime CLI reads each value from its own environment, as the
    ///   headless path does. Step `env` entries are literals from the repo's
    ///   own workflow file and keep the `KEY=VALUE` form the headless `exec`
    ///   uses.
    pub(crate) fn options(&self) -> Vec<ContainerOption> {
        let workdir: PathBuf = self.workdir.to_path_buf();
        let mut options = vec![
            ContainerOption::Image(ImageRef::new(self.image)),
            ContainerOption::Interactive(true),
            ContainerOption::StartupGrace(Duration::MAX),
            ContainerOption::FullTranscript,
            ContainerOption::WorkingDir(workdir.clone()),
            ContainerOption::Overlay(OverlaySpec {
                host_path: workdir.clone(),
                container_path: workdir,
                permission: OverlayPermission::ReadWrite,
            }),
        ];
        if let Some(settings) = self.builtin_launch {
            options.push(ContainerOption::BuiltinLaunch(settings.clone()));
        }
        options.extend(self.overlays.iter().cloned().map(ContainerOption::Overlay));

        let mut names: Vec<&String> = self.env.keys().collect();
        names.sort();
        options.extend(names.into_iter().map(|name| ContainerOption::EnvOverride {
            key: name.clone(),
            value: crate::engine::auth::credential::SecretString::new(self.env[name].clone()),
        }));

        if let Some(step_env) = self.step_env {
            let mut literals: Vec<(&String, &String)> = step_env.iter().collect();
            literals.sort();
            options.extend(literals.into_iter().map(|(key, value)| {
                ContainerOption::EnvLiteral(EnvLiteral {
                    key: key.clone(),
                    value: value.clone(),
                })
            }));
        }

        options.push(ContainerOption::Entrypoint(Entrypoint::new([
            "sh",
            "-c",
            self.command,
        ])));
        options
    }
}
