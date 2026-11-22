#![allow(dead_code)]
//! WI 0152 boundary proof, shared by every runtime that can mount a
//! workspace read-only: a real container whose workspace is read-only
//! cannot change it, while `/tmp`, `/awman/output`, a mounted context
//! directory and the agent home stay writable.
//!
//! The launch goes through the runtime's own `build` with the option set the
//! engine produces for `--read-only-workspace` (read-only `/workspace`
//! overlay, writable output and context overlays, tmpfs `/tmp`, read-only
//! intent), so the runtime's real argv/mount planning is what is proven.

use awman::command::commands::api_server::event_bus::EventBus;
use awman::engine::agent_runtime::{AgentRuntimeEngine, ResolvedAgentOptions};
use awman::engine::container::options::{
    ContainerOption, Entrypoint, ImageRef, OverlayPermission, OverlaySpec, ResolvedContainerOptions,
};
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// What the container checks, one distinct exit code per violated guarantee.
const READ_ONLY_SCRIPT: &str = r#"
if touch /workspace/forbidden 2>/dev/null; then exit 71; fi
if mkdir /workspace/forbidden-dir 2>/dev/null; then exit 72; fi
if sh -c 'echo changed >> /workspace/existing.txt' 2>/dev/null; then exit 73; fi
if rm /workspace/existing.txt 2>/dev/null; then exit 74; fi
if touch /workspace/sub/forbidden 2>/dev/null; then exit 75; fi
echo scratch > /tmp/scratch || exit 76
grep -q ' /tmp tmpfs ' /proc/mounts || exit 77
echo output > /awman/output/proof || exit 78
echo context > /awman/context/repo/note || exit 79
mkdir -p "$HOME/.cache/agent" && echo home > "$HOME/.cache/agent/state" || exit 80
"#;

const CONTROL_SCRIPT: &str =
    "echo writable > /workspace/control; echo output > /awman/output/proof";

pub struct Proof<'a> {
    pub runtime: Arc<dyn AgentRuntimeEngine>,
    pub image: &'a str,
    /// A private directory (under the test sandbox) for the host side.
    pub root: &'a Path,
    /// Run `builtin` launches need launch settings; every other runtime
    /// ignores them.
    pub builtin: bool,
}

struct Host {
    workspace: std::path::PathBuf,
    output: std::path::PathBuf,
    context: std::path::PathBuf,
}

fn host(root: &Path, name: &str) -> Host {
    let base = root.join(name);
    let workspace = base.join("workspace");
    let output = base.join("output");
    let context = base.join("context");
    std::fs::create_dir_all(workspace.join("sub")).unwrap();
    std::fs::create_dir_all(&output).unwrap();
    std::fs::create_dir_all(&context).unwrap();
    std::fs::write(workspace.join("existing.txt"), "original\n").unwrap();
    Host {
        workspace,
        output,
        context,
    }
}

fn overlay(host: &Path, guest: &str, permission: OverlayPermission) -> ContainerOption {
    ContainerOption::Overlay(OverlaySpec {
        host_path: host.to_path_buf(),
        container_path: guest.into(),
        permission,
    })
}

fn options(proof: &Proof<'_>, host: &Host, read_only: bool) -> ResolvedContainerOptions {
    let script = if read_only {
        READ_ONLY_SCRIPT
    } else {
        CONTROL_SCRIPT
    };
    let mut list = vec![
        ContainerOption::Image(ImageRef::new(proof.image)),
        ContainerOption::Entrypoint(Entrypoint::new(["/bin/sh", "-c", script])),
        ContainerOption::Interactive(false),
        overlay(
            &host.workspace,
            "/workspace",
            if read_only {
                OverlayPermission::ReadOnly
            } else {
                OverlayPermission::ReadWrite
            },
        ),
        overlay(&host.output, "/awman/output", OverlayPermission::ReadWrite),
    ];
    if read_only {
        list.push(overlay(
            &host.context,
            "/awman/context/repo",
            OverlayPermission::ReadWrite,
        ));
        list.push(ContainerOption::ReadOnlyWorkspace(true));
        list.push(ContainerOption::Tmpfs("/tmp".into()));
    }
    let mut options = ResolvedContainerOptions::resolve(list).unwrap();
    if proof.builtin {
        options.builtin_launch = Some(Box::new(
            awman::data::config::builtin_launch::BuiltinLaunchSettings::default(),
        ));
    }
    options
}

async fn run(proof: &Proof<'_>, options: ResolvedContainerOptions) -> i32 {
    let instance = proof
        .runtime
        .build(ResolvedAgentOptions::Container(Box::new(options)))
        .unwrap_or_else(|e| panic!("the runtime refused the launch: {e}"));
    let bus = EventBus::new(1024);
    let frontend = ApiDispatchFrontend::new(
        "exec prompt",
        &["--headless".into(), "probe".into()],
        bus.sender(),
    );
    let mut execution = instance.run_with_frontend(Box::new(frontend)).unwrap();
    tokio::time::timeout(Duration::from_secs(90), execution.wait())
        .await
        .expect("the launch deadline")
        .unwrap()
        .exit_code
}

/// Run the writable control and the read-only launch; panics with the
/// violated guarantee when the runtime does not enforce it.
pub fn prove(proof: &Proof<'_>) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(prove_async(proof));
}

/// [`prove`] for callers already inside a Tokio runtime.
pub async fn prove_async(proof: &Proof<'_>) {
    {
        // Control: the same shape without the option can write the
        // workspace, so a read-only failure below is the option's doing.
        let control = host(proof.root, "control");
        let code = run(proof, options(proof, &control, false)).await;
        assert_eq!(code, 0, "the writable control launch failed");
        assert!(control.workspace.join("control").exists(), "control write");

        let ro = host(proof.root, "read-only");
        let code = run(proof, options(proof, &ro, true)).await;
        let reason = match code {
            0 => "",
            71 => "touch created a file in the read-only workspace",
            72 => "mkdir created a directory in the read-only workspace",
            73 => "an existing workspace file was appended to",
            74 => "an existing workspace file was deleted",
            75 => "a file was created in a workspace subdirectory",
            76 => "/tmp is not writable",
            77 => "/tmp is not a tmpfs mount",
            78 => "/awman/output is not writable",
            79 => "the mounted context directory is not writable",
            80 => "the agent home is not writable",
            other => panic!("the probe exited {other} instead of proving the mounts"),
        };
        assert!(reason.is_empty(), "{reason}");
        // The host agrees with what the container reported.
        assert_eq!(
            std::fs::read_to_string(ro.workspace.join("existing.txt")).unwrap(),
            "original\n"
        );
        assert!(!ro.workspace.join("forbidden").exists());
        assert!(!ro.workspace.join("forbidden-dir").exists());
        assert!(!ro.workspace.join("sub/forbidden").exists());
        assert_eq!(
            std::fs::read_to_string(ro.output.join("proof"))
                .unwrap()
                .trim(),
            "output"
        );
        assert_eq!(
            std::fs::read_to_string(ro.context.join("note"))
                .unwrap()
                .trim(),
            "context"
        );
    }
}
