//! Layer 0 constants shared with the isolated builtin worker.

macro_rules! define_builtin_runtime_version {
    ($version:literal) => {
        /// Pinned Microsandbox SDK runtime metadata version.
        pub const MSB_VERSION: &str = $version;
        /// Protocol tag shared by worker discovery and sandbox ownership.
        pub const WORKER_PROTOCOL: &str = concat!($version, "/18/awman-1");
        /// Exact older worker protocols whose catalogs this build may upgrade.
        ///
        /// Entries are compared as literal strings only: there is no prefix or
        /// semantic-version comparison, and this list must never include a
        /// protocol newer than [`WORKER_PROTOCOL`].
        pub const UPGRADABLE_FROM: &[&str] = &["0.7.2/18/awman-1"];
    };
}

define_builtin_runtime_version!("0.7.8");

/// Encode the SDK metadata version in the fixed-width executable link section.
pub const fn version_bytes(version: &str) -> [u8; 5] {
    let source = version.as_bytes();
    assert!(
        source.len() == 5,
        "builtin runtime version must be exactly 5 bytes"
    );
    let mut bytes = [0; 5];
    let mut index = 0;
    while index < bytes.len() {
        bytes[index] = source[index];
        index += 1;
    }
    bytes
}

/// libkrun revision pinned by the Microsandbox runtime payload.
pub const LIBKRUN_COMMIT: &str = "b35c7e118d7a1e48c8fad103b258e64ff5c49147";
