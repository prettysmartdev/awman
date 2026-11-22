//! Workspace access intent and mount spec — Layer 0.
//!
//! `WorkspaceAccess` is what a command asks for (`--read-only-workspace`, a
//! workflow step's `read_only_workspace`, a pulled lease's `exec.read_only`).
//! `WorkspaceMountSpec` is the runtime-neutral description of the mounts that
//! intent requires. Layer 1 turns the spec into each runtime's own mounts and
//! refuses a launch it cannot enforce; Layer 2 decides the intent and the
//! prompt policy. Nothing here touches a filesystem or a runtime.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where the session workspace is mounted inside every agent container.
pub const WORKSPACE_GUEST_PATH: &str = "/workspace";

/// The writable, memory-backed scratch directory of a read-only launch.
pub const SCRATCH_TMPFS_PATH: &str = "/tmp";

/// The command-outputs directory, as named in the read-only prompt line.
/// Equal to `data::fs::command_outputs::OUTPUT_MOUNT_PATH`.
pub const OUTPUT_GUEST_PATH: &str = "/awman/output";

/// The catalogue flag (`--read-only-workspace`) on `exec prompt` and
/// `exec workflow`.
pub const READ_ONLY_WORKSPACE_FLAG: &str = "read-only-workspace";

/// The `/v1/status` capability advertised when the runtime can enforce a
/// read-only workspace. `lease_read_only` remains the WSP registration flag.
pub const READ_ONLY_WORKSPACE_CAPABILITY: &str = "read_only_workspace";

/// Container paths use Linux roots even when the host is Windows. Native
/// `Path::is_absolute` would require a Windows drive for `/tmp` on that host.
pub fn is_normalized_guest_path(path: &Path) -> bool {
    path.to_str().is_some_and(|value| {
        value.starts_with('/')
            && !value.starts_with("//")
            && !value.contains([':', ',', '\0'])
            && !value
                .split(['/', '\\'])
                .any(|part| part == "." || part == "..")
    })
}

/// Whether the agent may write to the session workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAccess {
    /// The workspace is mounted writable (the default).
    #[default]
    ReadWrite,
    /// The workspace is mounted read-only. A runtime that cannot enforce
    /// this must refuse the launch; it is never downgraded to `ReadWrite`.
    ReadOnly,
}

impl WorkspaceAccess {
    pub fn from_read_only(read_only: bool) -> Self {
        if read_only {
            Self::ReadOnly
        } else {
            Self::ReadWrite
        }
    }

    pub fn is_read_only(self) -> bool {
        self == Self::ReadOnly
    }

    /// Combine two requests for the same launch. Read-only always wins, so
    /// no source (flag, workflow step, lease) can widen another's request.
    pub fn strictest(self, other: Self) -> Self {
        Self::from_read_only(self.is_read_only() || other.is_read_only())
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadWrite => "read_write",
            Self::ReadOnly => "read_only",
        }
    }
}

/// The mounts a workspace intent requires, independent of any runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMountSpec {
    /// Host directory mounted as the workspace.
    pub host_path: PathBuf,
    /// Always [`WORKSPACE_GUEST_PATH`].
    pub guest_path: PathBuf,
    pub access: WorkspaceAccess,
    /// Guest paths that must be writable tmpfs mounts. `[/tmp]` for a
    /// read-only workspace, empty otherwise.
    pub tmpfs: Vec<PathBuf>,
}

impl WorkspaceMountSpec {
    pub fn new(host_path: impl Into<PathBuf>, access: WorkspaceAccess) -> Self {
        Self {
            host_path: host_path.into(),
            guest_path: PathBuf::from(WORKSPACE_GUEST_PATH),
            access,
            tmpfs: if access.is_read_only() {
                vec![PathBuf::from(SCRATCH_TMPFS_PATH)]
            } else {
                Vec::new()
            },
        }
    }

    /// Whether a mount at `guest` would expose the workspace: the workspace
    /// itself, anything inside it, or a parent that contains it. Under a
    /// read-only workspace every such mount must be read-only too.
    pub fn overlaps_guest(&self, guest: &Path) -> bool {
        guest.starts_with(&self.guest_path) || self.guest_path.starts_with(guest)
    }
}

/// The one system-prompt line of a read-only launch. `outputs_mounted` is
/// whether `/awman/output` is really mounted; `writable_contexts` are the
/// guest paths of the context directories mounted writable for this run
/// (for example `/awman/context/repo`), in mount order.
pub fn read_only_workspace_prompt_line(
    outputs_mounted: bool,
    writable_contexts: &[String],
) -> String {
    let contexts = writable_contexts.join(", ");
    let durable = match (outputs_mounted, writable_contexts.is_empty()) {
        (true, false) => format!(
            "Write anything that must outlive this run to {OUTPUT_GUEST_PATH} or to the mounted context directories ({contexts})."
        ),
        (true, true) => {
            format!("Write anything that must outlive this run to {OUTPUT_GUEST_PATH}.")
        }
        (false, false) => format!(
            "Write anything that must outlive this run to the mounted context directories ({contexts})."
        ),
        (false, true) => {
            "No durable writable directory is mounted, so report your results in your final message."
                .to_string()
        }
    };
    format!(
        "The workspace at {WORKSPACE_GUEST_PATH} is mounted read-only: you can read every file but cannot create, change or delete anything in it. {durable} Use {SCRATCH_TMPFS_PATH} for scratch files; it is discarded when the run ends."
    )
}

/// Append the read-only workspace section to a rendered system prompt (or
/// start one), using the same `\n---\n` separator as every other section.
pub fn with_read_only_workspace_section(
    system_prompt: Option<String>,
    outputs_mounted: bool,
    writable_contexts: &[String],
) -> String {
    let section = format!(
        "## Read-only workspace\n\n{}",
        read_only_workspace_prompt_line(outputs_mounted, writable_contexts)
    );
    match system_prompt {
        Some(prompt) if !prompt.is_empty() => format!("{prompt}\n---\n{section}"),
        _ => section,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_paths_use_linux_roots_on_every_host() {
        for valid in ["/", "/tmp", "/home/agent/.cache"] {
            assert!(is_normalized_guest_path(Path::new(valid)), "{valid}");
        }
        for invalid in [
            "",
            "tmp",
            "C:/tmp",
            "//server/share",
            "\\tmp",
            "/tmp/../workspace",
            "/tmp/./cache",
            "/tmp/..\\workspace",
            "/tmp:rw",
            "/tmp,size=1",
            "/tmp\0",
        ] {
            assert!(!is_normalized_guest_path(Path::new(invalid)), "{invalid:?}");
        }
    }

    #[test]
    fn access_defaults_to_read_write_and_serializes_snake_case() {
        assert_eq!(WorkspaceAccess::default(), WorkspaceAccess::ReadWrite);
        assert_eq!(
            serde_json::to_string(&WorkspaceAccess::ReadOnly).unwrap(),
            "\"read_only\""
        );
        let parsed: WorkspaceAccess = serde_json::from_str("\"read_write\"").unwrap();
        assert_eq!(parsed, WorkspaceAccess::ReadWrite);
        assert_eq!(WorkspaceAccess::ReadOnly.as_str(), "read_only");
    }

    #[test]
    fn strictest_never_widens_a_read_only_request() {
        use WorkspaceAccess::{ReadOnly, ReadWrite};
        assert_eq!(ReadWrite.strictest(ReadWrite), ReadWrite);
        assert_eq!(ReadWrite.strictest(ReadOnly), ReadOnly);
        assert_eq!(ReadOnly.strictest(ReadWrite), ReadOnly);
        assert_eq!(ReadOnly.strictest(ReadOnly), ReadOnly);
        assert!(WorkspaceAccess::from_read_only(true).is_read_only());
        assert!(!WorkspaceAccess::from_read_only(false).is_read_only());
    }

    #[test]
    fn mount_spec_adds_tmpfs_scratch_only_when_read_only() {
        let rw = WorkspaceMountSpec::new("/repo", WorkspaceAccess::ReadWrite);
        assert_eq!(rw.guest_path, Path::new("/workspace"));
        assert!(rw.tmpfs.is_empty());
        let ro = WorkspaceMountSpec::new("/repo", WorkspaceAccess::ReadOnly);
        assert_eq!(ro.host_path, Path::new("/repo"));
        assert_eq!(ro.tmpfs, vec![PathBuf::from("/tmp")]);
    }

    #[test]
    fn overlap_covers_children_parents_and_the_workspace_itself() {
        let spec = WorkspaceMountSpec::new("/repo", WorkspaceAccess::ReadOnly);
        assert!(spec.overlaps_guest(Path::new("/workspace")));
        assert!(spec.overlaps_guest(Path::new("/workspace/.git")));
        assert!(spec.overlaps_guest(Path::new("/")));
        assert!(!spec.overlaps_guest(Path::new("/workspace-other")));
        assert!(!spec.overlaps_guest(Path::new("/awman/output")));
    }

    #[test]
    fn prompt_line_names_only_the_locations_that_are_mounted() {
        let contexts = vec![
            "/awman/context/repo".to_string(),
            "/awman/context/workflow".to_string(),
        ];
        let both = read_only_workspace_prompt_line(true, &contexts);
        assert_eq!(
            both,
            "The workspace at /workspace is mounted read-only: you can read every file but cannot create, change or delete anything in it. Write anything that must outlive this run to /awman/output or to the mounted context directories (/awman/context/repo, /awman/context/workflow). Use /tmp for scratch files; it is discarded when the run ends."
        );
        assert!(!both.contains('\n'));

        let outputs = read_only_workspace_prompt_line(true, &[]);
        assert!(outputs.contains("to /awman/output."));
        assert!(!outputs.contains("context"));

        let only_contexts = read_only_workspace_prompt_line(false, &contexts[..1]);
        assert!(only_contexts.contains("context directories (/awman/context/repo)."));
        assert!(!only_contexts.contains("/awman/output"));

        let neither = read_only_workspace_prompt_line(false, &[]);
        assert!(neither.contains("No durable writable directory is mounted"));
        assert!(!neither.contains("/awman/output"));
        assert!(neither.contains("Use /tmp for scratch files"));
    }

    #[test]
    fn section_is_appended_after_existing_sections_or_starts_the_prompt() {
        let alone = with_read_only_workspace_section(None, true, &[]);
        assert!(alone.starts_with("## Read-only workspace\n\n"));
        let empty = with_read_only_workspace_section(Some(String::new()), true, &[]);
        assert_eq!(alone, empty);
        let appended = with_read_only_workspace_section(Some("## Outputs\n\nx".into()), true, &[]);
        assert!(appended.starts_with("## Outputs\n\nx\n---\n## Read-only workspace\n\n"));
    }

    #[test]
    fn output_path_matches_the_command_outputs_mount() {
        assert_eq!(
            OUTPUT_GUEST_PATH,
            crate::data::fs::command_outputs::OUTPUT_MOUNT_PATH
        );
    }
}
