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
    ContainerOption, Entrypoint, EnvLiteral, EnvVar, ImageRef, OverlayPermission, OverlaySpec,
};

/// Everything a foreground setup/teardown step container needs.
#[derive(Debug, Clone)]
pub struct PhaseStepContainerSpec<'a> {
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
        options.extend(self.overlays.iter().cloned().map(ContainerOption::Overlay));

        let mut names: Vec<&String> = self.env.keys().collect();
        names.sort();
        options.extend(
            names
                .into_iter()
                .map(|name| ContainerOption::EnvPassthrough(EnvVar(name.clone()))),
        );

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::container::options::ResolvedContainerOptions;

    fn spec<'a>(
        workdir: &'a Path,
        overlays: &'a [OverlaySpec],
        env: &'a HashMap<String, String>,
        step_env: Option<&'a HashMap<String, String>>,
    ) -> PhaseStepContainerSpec<'a> {
        PhaseStepContainerSpec {
            image: "awman-proj:latest",
            workdir,
            overlays,
            env,
            command: "make setup && echo done",
            step_env,
        }
    }

    fn resolve(spec: &PhaseStepContainerSpec<'_>) -> ResolvedContainerOptions {
        ResolvedContainerOptions::resolve(spec.options()).expect("options resolve")
    }

    #[test]
    fn the_command_is_the_containers_only_process() {
        let env = HashMap::new();
        let opts = resolve(&spec(Path::new("/work/repo"), &[], &env, None));
        assert_eq!(
            opts.entrypoint.map(|e| e.0),
            Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "make setup && echo done".to_string()
            ])
        );
        assert!(opts.interactive, "a phase step container must get a PTY");
        assert!(
            opts.remove_on_exit,
            "the container must not outlive its command"
        );
        assert!(opts.seeded_prompt.is_none());
    }

    #[test]
    fn a_silent_command_is_never_killed_and_its_whole_output_is_kept() {
        let env = HashMap::new();
        let opts = resolve(&spec(Path::new("/w"), &[], &env, None));
        assert_eq!(opts.startup_grace, Some(Duration::MAX));
        assert!(opts.full_transcript);
    }

    #[test]
    fn the_workspace_is_mounted_read_write_and_is_the_working_directory() {
        let workdir = Path::new("/work/repo");
        let extra = OverlaySpec {
            host_path: PathBuf::from("/home/u/.ssh"),
            container_path: PathBuf::from("/root/.ssh"),
            permission: OverlayPermission::ReadOnly,
        };
        let env = HashMap::new();
        let overlays = [extra.clone()];
        let opts = resolve(&spec(workdir, &overlays, &env, None));
        assert_eq!(opts.working_dir.as_deref(), Some(workdir));
        assert_eq!(
            opts.overlays,
            vec![
                OverlaySpec {
                    host_path: workdir.to_path_buf(),
                    container_path: workdir.to_path_buf(),
                    permission: OverlayPermission::ReadWrite,
                },
                extra,
            ]
        );
    }

    #[test]
    fn env_overlay_values_are_passed_through_by_name_never_as_literals() {
        let env = HashMap::from([("GITHUB_TOKEN".to_string(), "s3cret".to_string())]);
        let opts = resolve(&spec(Path::new("/w"), &[], &env, None));
        assert_eq!(opts.env_passthrough, vec![EnvVar("GITHUB_TOKEN".into())]);
        assert!(
            opts.env_literal.iter().all(|l| !l.value.contains("s3cret")),
            "a host env() value must never become a KEY=VALUE literal"
        );
    }

    #[test]
    fn step_declared_env_becomes_sorted_literals() {
        let env = HashMap::new();
        let step_env = HashMap::from([
            ("B".to_string(), "2".to_string()),
            ("A".to_string(), "1".to_string()),
        ]);
        let opts = resolve(&spec(Path::new("/w"), &[], &env, Some(&step_env)));
        let literals: Vec<(String, String)> = opts
            .env_literal
            .iter()
            .map(|l| (l.key.clone(), l.value.clone()))
            .collect();
        assert_eq!(
            literals,
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
    }
}
