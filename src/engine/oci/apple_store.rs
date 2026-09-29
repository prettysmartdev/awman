//! Apple Containers image store — an explicit, typed blocker for the strict
//! builtin runtime.
//!
//! # What the pinned upstream sources say (evidence dated 2026-09-28)
//!
//! The `container` project (apple/container, release **1.4.1**, tag commit
//! `9a8917ca2da5cd6ba059b9ba5ca5a74892e9bb7d`, published 2026-09-09) keeps
//! its images in a content store owned by the launch agent helper
//! `container-core-images`. The CLI's `container image save` does not read
//! that store itself: it sends an XPC request to the helper and the helper
//! writes an OCI image layout tar to a caller-supplied path
//! (`Sources/Services/ContainerAPIService/Client/ClientImage.swift`,
//! `ClientImage.save`, and `Sources/Services/ContainerImagesService/Server/ImagesService.swift`,
//! `ImagesService.save`, which archives the store's OCI layout directory with
//! a PAX tar writer). The wire contract is:
//!
//! | element | value in 1.4.1 |
//! |---|---|
//! | mach service | `com.apple.container.core.container-core-images` |
//! | transport | `xpc_connection_create_mach_service` + `xpc_connection_send_message_with_reply` (libxpc, public C API) |
//! | route key | `com.apple.container.xpc.route` = `imageSave` (Swift `ImagesServiceXPCRoute` raw value) |
//! | request keys | `imageDescriptions` (JSON `[ImageDescription]`), `filePath` (output path), `ociPlatform` (JSON `Platform`) |
//! | error key | `com.apple.container.xpc.error` (JSON `{code, message}`) |
//! | protocol version | **none**: no version field, no negotiation; the apiserver only reports its release version through a separate `apiServerVersion` ping |
//!
//! Source hashes recorded for this evidence (SHA-256 of the raw files at the
//! tag): `ClientImage.swift` `97917ba5…61fc0`, `ImageServiceXPCRoutes.swift`
//! `1b0710f2…b77c`, `ImageServiceXPCKeys.swift` `119b0fdd…a65f`,
//! `ImagesService.swift` `4bf795bf…52ae`, `XPCMessage.swift` in
//! `Sources/ContainerXPC`.
//!
//! # Why there is still no conforming adapter
//!
//! The strict runtime may only acquire images through a **versioned,
//! in-process** API linked into `awman`. Against the evidence above:
//!
//! 1. The route and key names are raw values of Swift enums inside the
//!    `ContainerImagesService` package. Apple documents the CLI and the
//!    `container-apiserver` launch agent as the product interface; the XPC
//!    message schema carries no protocol version and no compatibility
//!    promise, so a Rust re-implementation of it would be pinned to a byte
//!    layout that any release may change without notice.
//! 2. The Swift client libraries are Swift Package Manager sources, not a
//!    framework with a C ABI. Linking them in-process would require a Swift
//!    toolchain in the awman build plus an FFI shim; speaking libxpc
//!    directly from Rust requires `unsafe` FFI, which this crate forbids
//!    (`#![forbid(unsafe_code)]`) and which has no native validation here.
//! 3. The helper writes the export to a path it is handed. That is
//!    acceptable for a bridge (the path would be awman's private staging
//!    directory), but it makes the helper — not awman — the process that
//!    reads the store, so the strict rules against scraping the private
//!    store remain satisfied only if the XPC route is used, never the
//!    on-disk layout under `~/Library/Application Support/com.apple.container`.
//!
//! Executing the `container` CLI, shipping a separate bridge executable or
//! dynamic library, or reading the store's private files are all excluded by
//! `aspec/architecture/security.md`; none of them is an acceptable fallback.
//!
//! # What this module does instead
//!
//! It records the contract above as typed data ([`BridgeContract`],
//! [`REQUIRED_SERVICE_VERSIONS`]), reports feasibility as a typed
//! [`Feasibility`] value, and refuses every acquisition with
//! `ImageSourceBlocked` and an actionable message. Any future bridge must
//! (a) verify the helper's release version is one of
//! [`REQUIRED_SERVICE_VERSIONS`] before sending `imageSave`, (b) validate the
//! exported bytes with the same archive validator every other source uses,
//! and (c) be proven on Apple Silicon against the real helper. Until then the
//! WI 0119 acceptance row stays FAIL (adapter absent) with the native
//! investigation BLOCKED.

use crate::data::config::image_source::ImageSourceKind;
use crate::engine::error::EngineError;

/// The upstream release whose sources this contract was read from.
pub const EVIDENCE_UPSTREAM_TAG: &str = "1.4.1";
/// Commit the release tag resolves to.
pub const EVIDENCE_UPSTREAM_COMMIT: &str = "9a8917ca2da5cd6ba059b9ba5ca5a74892e9bb7d";
/// When the sources were read.
pub const EVIDENCE_DATE: &str = "2026-09-28";

/// The only helper release versions a future bridge may speak to. A bridge
/// must compare the helper's reported release version against this list
/// *before* sending any image route, because the XPC schema has no version
/// of its own.
pub const REQUIRED_SERVICE_VERSIONS: &[&str] = &["1.4.1"];

/// The XPC contract of the image helper as pinned in the upstream sources.
/// Data only: nothing here opens a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeContract {
    /// Mach service name of the image helper.
    pub mach_service: &'static str,
    /// Dictionary key carrying the route name.
    pub route_key: &'static str,
    /// Route that exports images to a path.
    pub save_route: &'static str,
    /// Route that lists images (needed to obtain `ImageDescription`s).
    pub list_route: &'static str,
    /// Request key: JSON-encoded `[ImageDescription]`.
    pub descriptions_key: &'static str,
    /// Request key: output file path written by the helper.
    pub file_path_key: &'static str,
    /// Request key: JSON-encoded platform selector.
    pub platform_key: &'static str,
    /// Reply key carrying a JSON `{code, message}` error.
    pub error_key: &'static str,
    /// Whether the protocol carries its own version (it does not).
    pub protocol_versioned: bool,
}

/// The contract as read from release [`EVIDENCE_UPSTREAM_TAG`].
pub const CONTRACT: BridgeContract = BridgeContract {
    mach_service: "com.apple.container.core.container-core-images",
    route_key: "com.apple.container.xpc.route",
    save_route: "imageSave",
    list_route: "imageList",
    descriptions_key: "imageDescriptions",
    file_path_key: "filePath",
    platform_key: "ociPlatform",
    error_key: "com.apple.container.xpc.error",
    protocol_versioned: false,
};

/// Why an in-process bridge is not shipped. Every variant is an exact,
/// checkable statement; none is "not implemented yet".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocker {
    /// The XPC message schema has no version field and no stability promise.
    UnversionedProtocol,
    /// The client is a Swift package, not a linkable C-ABI library.
    NoLinkableCAbi,
    /// Speaking libxpc from Rust needs `unsafe` FFI, which the crate forbids.
    RequiresUnsafeFfi,
    /// This build does not target macOS at all.
    NotMacOs,
    /// No Apple Silicon host with the helper installed has validated a bridge.
    NoNativeValidation,
}

impl Blocker {
    /// One line a user or reviewer can act on.
    pub fn describe(self) -> &'static str {
        match self {
            Self::UnversionedProtocol => {
                "the container-core-images XPC schema carries no protocol version or compatibility \
                 promise (routes and keys are raw Swift enum values)"
            }
            Self::NoLinkableCAbi => {
                "Apple ships the client as Swift Package Manager sources, not a framework with a C \
                 ABI that awman could link"
            }
            Self::RequiresUnsafeFfi => {
                "reaching the helper from Rust needs libxpc FFI, which awman forbids \
                 (`#![forbid(unsafe_code)]`) without a reviewed, natively validated exception"
            }
            Self::NotMacOs => "Apple Containers exists only on macOS",
            Self::NoNativeValidation => {
                "no Apple Silicon host with a supported container-core-images release has \
                 validated exported bytes, platform/config/layer identity or error handling"
            }
        }
    }
}

/// The feasibility verdict for this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Feasibility {
    /// A conforming bridge could be linked and has been validated. Never
    /// produced today; the variant exists so callers must handle it.
    Available { service_version: String },
    /// No conforming bridge; the blockers are listed most fundamental first.
    Blocked(Vec<Blocker>),
}

/// The verdict for the current target, from the recorded evidence.
pub fn feasibility() -> Feasibility {
    let mut blockers = vec![
        Blocker::UnversionedProtocol,
        Blocker::NoLinkableCAbi,
        Blocker::RequiresUnsafeFfi,
    ];
    if !cfg!(target_os = "macos") {
        blockers.push(Blocker::NotMacOs);
    }
    blockers.push(Blocker::NoNativeValidation);
    Feasibility::Blocked(blockers)
}

/// Why an Apple-store acquisition cannot proceed, with the supported route.
pub(super) fn blocked(reference: &str) -> EngineError {
    let blockers = match feasibility() {
        Feasibility::Blocked(b) => b,
        Feasibility::Available { .. } => Vec::new(),
    };
    let why = blockers
        .iter()
        .map(|b| b.describe())
        .collect::<Vec<_>>()
        .join("; ");
    EngineError::ImageSourceBlocked {
        source_kind: ImageSourceKind::AppleStore,
        reason: format!(
            "awman's builtin runtime has no in-process bridge to the Apple Containers image \
             store: {why}. awman does not run the `container` helper, ship a separate bridge, or \
             read the store's private files (evidence: apple/container {EVIDENCE_UPSTREAM_TAG} \
             sources, {EVIDENCE_DATE}). Export the image yourself with `container image save \
             {reference} -o <file>` (an OCI layout tar) or build it with Docker, then set the \
             image source to {{\"type\":\"archive\",\"path\":\"<file>\"}} and run `awman ready` \
             again."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_error_names_the_supported_route_and_runs_nothing() {
        match blocked("awman-x-claude:latest") {
            EngineError::ImageSourceBlocked {
                source_kind,
                reason,
            } => {
                assert_eq!(source_kind, ImageSourceKind::AppleStore);
                assert!(reason.contains("container image save awman-x-claude:latest -o <file>"));
                assert!(reason.contains("\"type\":\"archive\""));
                assert!(reason.contains("does not run the `container`"));
                assert!(reason.contains(EVIDENCE_UPSTREAM_TAG));
                assert!(reason.contains(EVIDENCE_DATE));
            }
            other => panic!("expected ImageSourceBlocked, got {other:?}"),
        }
    }

    #[test]
    fn feasibility_is_a_typed_blocker_list_never_available() {
        match feasibility() {
            Feasibility::Blocked(blockers) => {
                assert_eq!(blockers[0], Blocker::UnversionedProtocol);
                assert!(blockers.contains(&Blocker::RequiresUnsafeFfi));
                assert!(blockers.contains(&Blocker::NoNativeValidation));
                assert_eq!(
                    blockers.contains(&Blocker::NotMacOs),
                    !cfg!(target_os = "macos")
                );
                for b in blockers {
                    assert!(!b.describe().is_empty());
                }
            }
            Feasibility::Available { .. } => panic!("no bridge is available"),
        }
    }

    #[test]
    fn the_recorded_contract_is_unversioned_and_names_the_image_helper() {
        let contract = CONTRACT;
        assert!(!contract.protocol_versioned);
        assert_eq!(
            contract.mach_service,
            "com.apple.container.core.container-core-images"
        );
        assert_eq!(contract.save_route, "imageSave");
        assert_eq!(contract.route_key, "com.apple.container.xpc.route");
        let versions: Vec<&str> = REQUIRED_SERVICE_VERSIONS.to_vec();
        assert_eq!(versions, vec!["1.4.1"]);
        let commit: String = EVIDENCE_UPSTREAM_COMMIT.to_string();
        assert_eq!(commit.len(), 40);
    }
}
