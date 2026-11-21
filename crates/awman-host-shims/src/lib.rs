//! Binary-side native integrations. The awman library keeps its safe-code boundary.
//! Only Layer 0 constants are imported from awman; engine contracts are adapted by
//! the binary package. No helper executable is produced or extracted.
#![deny(unsafe_code)]

mod worker;
pub use worker::dispatch_worker;

/// Prepare the static provider and retain SDK version metadata for an embedding
/// worker. Must run before entering the VM. Unsupported consumers fail closed.
#[cfg(awman_builtin)]
pub fn prepare_builtin_worker() -> Result<(), String> {
    version::retain();
    kernel::prepare()
}

#[cfg(awman_builtin)]
mod kernel;
#[cfg(awman_builtin)]
mod version;

#[cfg(target_os = "macos")]
mod apple_xpc;
/// Safe dictionary transport contract used by the native shim.
pub mod xpc;
#[cfg(target_os = "macos")]
pub use apple_xpc::NativeXpc;

#[cfg(windows)]
mod local_pipe;
#[cfg(windows)]
pub use local_pipe::{create as owner_pipe, verify_client};

#[cfg(windows)]
mod git_process;
#[cfg(windows)]
pub use git_process::spawn as spawn_git;

#[cfg(windows)]
mod metadata;
#[cfg(windows)]
pub use metadata::publish as publish_git_metadata;
/// Install the native filesystem providers: Git metadata publication and the
/// owner-only private-file boundary (see [`install_private_files`]). The binary
/// and the component test support call this once at startup.
#[cfg(windows)]
pub fn install_metadata_publisher() {
    awman::data::fs::git_metadata::GitMetadataCopy::install_windows_publisher(metadata::publish);
    awman::data::mount_authority::install_windows_source_identity(private_fs::source_identity);
    install_private_files();
}

#[cfg(windows)]
mod private_fs;
/// Rename an open private file within its own directory, replacing the
/// target entry (never writing through it), with POSIX semantics where
/// supported. The file needs DELETE access.
#[cfg(windows)]
pub use metadata::publish_replacing as publish_private_file;
#[cfg(windows)]
pub use private_fs::{
    create_dir as private_create_dir, create_file as private_create_file,
    final_path as private_final_path, source_identity as mount_source_identity,
    verify as private_verify,
};

/// Install the native Windows boundary used by `awman::data::fs::private_files`.
/// Without it every private store on Windows is refused before anything is written.
#[cfg(windows)]
pub fn install_private_files() {
    awman::data::fs::private_files::install_windows_native(
        awman::data::fs::private_files::WindowsNative {
            create_dir: private_fs::create_dir,
            create_file: private_fs::create_file,
            verify: private_fs::verify,
            final_path: private_fs::final_path,
            rename: metadata::publish_replacing,
        },
    );
}
