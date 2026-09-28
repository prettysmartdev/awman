//! Apple Containers image store — BLOCKED for the strict runtime.
//!
//! Apple documents no public, versioned API for reading or exporting images
//! from the Apple Containers content store: the `container` CLI talks to
//! `container-apiserver` / `container-core-images` over private XPC, and the
//! on-disk store layout carries no compatibility guarantee. The strict
//! builtin runtime may neither execute the `container` helper nor scrape a
//! private store, so there is no conforming adapter to ship yet.
//!
//! This adapter therefore refuses with `ImageSourceBlocked` and says exactly
//! what the user can do instead. It never runs `container` and never falls
//! back to another source on its own.

use crate::data::config::image_source::ImageSourceKind;
use crate::engine::error::EngineError;

/// Why an Apple-store acquisition cannot proceed, with the supported route.
pub(super) fn blocked(reference: &str) -> EngineError {
    let platform_note = if cfg!(target_os = "macos") {
        ""
    } else {
        " (Apple Containers exists only on macOS in any case)"
    };
    EngineError::ImageSourceBlocked {
        source_kind: ImageSourceKind::AppleStore,
        reason: format!(
            "Apple provides no supported in-process API to export images from the Apple \
             Containers store, and awman's builtin runtime does not run the `container` \
             helper or read the store's private files{platform_note}. Export the image \
             yourself with `container image save {reference} -o <file>` (or build it with \
             Docker), then set the image source to \
             {{\"type\":\"archive\",\"path\":\"<file>\"}} and run `awman ready` again."
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
            }
            other => panic!("expected ImageSourceBlocked, got {other:?}"),
        }
    }
}
