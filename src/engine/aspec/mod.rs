//! `engine::aspec` — fetching the canonical `aspec/` template.
//!
//! Layer 1 (WI 0114 F-29, decision Q1). The download was a Layer 0 "network
//! helper", which is the one thing `src/data/` may not do. The URL itself is
//! still a Layer 0 constant — it is a fixed external address, not a decision
//! this engine makes.

use std::path::Path;

use thiserror::Error;

use crate::data::fs::repo_tree::{RepoRelPath, RepoTree};
use crate::data::init_files::{InitFile, InitFileKind};
use crate::data::network::ASPEC_TARBALL_URL;
use crate::engine::remote::{HttpClientOptions, HttpCore};

#[derive(Debug, Error)]
pub enum AspecError {
    #[error("network download failed: {0}")]
    DownloadFailed(String),
    #[error("tarball extraction failed: {0}")]
    ExtractFailed(String),
}

/// Downloads and unpacks the `aspec/` template tarball.
///
/// Constructed over a URL so a test (or a fork) can point it somewhere else;
/// [`AspecDownloader::canonical`] uses the shipped [`ASPEC_TARBALL_URL`].
pub struct AspecDownloader {
    url: String,
    /// A fixed archive served instead of fetching `url` (offline mirrors
    /// and tests).
    archive: Option<Vec<u8>>,
}

impl AspecDownloader {
    /// Time allowed to establish the connection to the tarball host.
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    /// Time allowed for the whole download.
    const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            archive: None,
        }
    }

    /// The downloader for the canonical aspec repository tarball.
    pub fn canonical() -> Self {
        Self::new(ASPEC_TARBALL_URL)
    }

    /// A downloader that serves `archive` instead of fetching anything.
    pub fn from_archive(archive: Vec<u8>) -> Self {
        Self {
            url: String::new(),
            archive: Some(archive),
        }
    }

    /// The URL this downloader fetches from.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Download the tarball into memory.
    pub async fn download(&self) -> Result<Vec<u8>, AspecError> {
        if let Some(archive) = &self.archive {
            return Ok(archive.clone());
        }
        HttpCore::refuse_public_host_under_test_isolation(&self.url)
            .map_err(AspecError::DownloadFailed)?;
        let client = HttpCore::client(
            &HttpClientOptions::default()
                .with_timeouts(Self::CONNECT_TIMEOUT, Self::READ_TIMEOUT)
                .with_user_agent("awman"),
        )
        .map_err(|e| AspecError::DownloadFailed(format!("client init: {e}")))?;
        let url = &self.url;
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| AspecError::DownloadFailed(format!("GET {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(AspecError::DownloadFailed(format!(
                "HTTP {} when downloading aspec tarball",
                resp.status()
            )));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AspecError::DownloadFailed(format!("read body: {e}")))?;
        Ok(bytes.to_vec())
    }

    /// Extract the `aspec/` directory from a gzipped tarball into `dest`.
    /// Extract the `aspec/` directory from a gzipped tarball into the
    /// repository's `aspec/`, appending one [`InitFile`] per file written or
    /// found unchanged to `files` — also on failure, so a partial extraction
    /// is still fully reported.
    ///
    /// The tarball from GitHub has a top-level directory like
    /// `prettysmartdev-aspec-<sha>/`. Look for entries under `<top>/aspec/` and
    /// strip that prefix. Only regular files and directories are applied, and
    /// every write goes through [`RepoTree`]: an entry that would traverse
    /// `..`, a link or special entry in the archive, and a linked directory in
    /// the repository are never followed.
    pub fn extract(
        tarball_bytes: &[u8],
        tree: &RepoTree,
        files: &mut Vec<InitFile>,
    ) -> Result<(), AspecError> {
        use flate2::read::GzDecoder;
        use std::io::Read;
        use tar::Archive;

        let aspec = RepoRelPath::new(Path::new("aspec")).expect("a plain name");
        let decoder = GzDecoder::new(tarball_bytes);
        let mut archive = Archive::new(decoder);
        let mut extracted = 0u64;

        let entries = archive
            .entries()
            .map_err(|e| AspecError::ExtractFailed(format!("read entries: {e}")))?;

        for entry in entries {
            let mut entry =
                entry.map_err(|e| AspecError::ExtractFailed(format!("read entry: {e}")))?;
            let path = entry
                .path()
                .map_err(|e| AspecError::ExtractFailed(format!("read entry path: {e}")))?
                .into_owned();
            let path_str = path.to_string_lossy().to_string();
            let components: Vec<&str> = path_str.split('/').collect();
            if components.len() < 2 || components[1] != "aspec" {
                continue;
            }
            let relative: String = components[2..].join("/");
            let entry_type = entry.header().entry_type();
            let target = if relative.trim_matches('/').is_empty() {
                aspec.clone()
            } else {
                match RepoRelPath::new(Path::new(&relative)) {
                    Some(rel) => aspec.join(&rel),
                    None => {
                        tracing::warn!(entry = %path_str, "aspec template entry escapes aspec/; skipped");
                        continue;
                    }
                }
            };
            if entry_type.is_dir() {
                tree.create_dir_all(&target)
                    .map_err(|e| AspecError::ExtractFailed(e.to_string()))?;
                continue;
            }
            if !entry_type.is_file() {
                tracing::warn!(entry = %path_str, "aspec template entry is not a regular file; skipped");
                continue;
            }
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|e| AspecError::ExtractFailed(format!("read {path_str}: {e}")))?;
            let outcome = tree
                .write(&target, &bytes)
                .map_err(|e| AspecError::ExtractFailed(e.to_string()))?;
            files.push(InitFile::new(
                target.display(),
                InitFileKind::AspecFile,
                outcome.into(),
            ));
            extracted += 1;
        }
        if extracted == 0 {
            return Err(AspecError::ExtractFailed(
                "no aspec/ files found in tarball".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::data::init_files::InitFileStatus;
    use awman_test_support::{TestCase, TestSandbox};

    /// A gzipped template tarball. `(path, Some(body))` is a regular file,
    /// `(path, None)` a symlink to `/etc/passwd`. Paths are written into the
    /// header verbatim so an escaping name can be expressed.
    pub(crate) fn archive(entries: &[(&str, Option<&str>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, body) in entries {
            let mut header = tar::Header::new_gnu();
            let name = &mut header.as_gnu_mut().unwrap().name;
            name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_mode(0o644);
            match body {
                Some(body) => {
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_size(body.len() as u64);
                    header.set_cksum();
                    builder.append(&header, body.as_bytes()).unwrap();
                }
                None => {
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_link_name("/etc/passwd").unwrap();
                    header.set_size(0);
                    header.set_cksum();
                    builder.append(&header, std::io::empty()).unwrap();
                }
            }
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extraction_reports_every_file_and_never_follows_links_or_escapes() {
        let sandbox = TestSandbox::for_test(TestCase::component(
            "engine::aspec::tests::extraction_reports_every_file_and_never_follows_links_or_escapes",
        ));
        let root = sandbox.work().join("repo");
        std::fs::create_dir_all(root.join("aspec/work-items")).unwrap();
        std::fs::write(root.join("aspec/foundation.md"), "old").unwrap();
        std::fs::write(root.join("aspec/work-items/0000-template.md"), "same").unwrap();
        let tree = RepoTree::open(&root).unwrap();
        let bytes = archive(&[
            ("top/aspec/foundation.md", Some("new")),
            ("top/aspec/work-items/0000-template.md", Some("same")),
            ("top/aspec/devops/testing.md", Some("tests")),
            ("top/aspec/link.md", None),
            ("top/aspec/../escape.md", Some("escape")),
            ("top/README.md", Some("not aspec")),
        ]);
        let mut files = Vec::new();
        AspecDownloader::extract(&bytes, &tree, &mut files).unwrap();
        let listed: Vec<_> = files
            .iter()
            .map(|f| (f.path.as_str(), f.kind, f.status.clone(), f.uncommitted))
            .collect();
        assert_eq!(
            listed,
            [
                (
                    "aspec/foundation.md",
                    InitFileKind::AspecFile,
                    InitFileStatus::Updated,
                    true
                ),
                (
                    "aspec/work-items/0000-template.md",
                    InitFileKind::AspecFile,
                    InitFileStatus::Kept,
                    false
                ),
                (
                    "aspec/devops/testing.md",
                    InitFileKind::AspecFile,
                    InitFileStatus::Created,
                    true
                ),
            ]
        );
        assert_eq!(
            std::fs::read_to_string(root.join("aspec/foundation.md")).unwrap(),
            "new"
        );
        assert!(std::fs::symlink_metadata(root.join("aspec/link.md")).is_err());
        assert!(!root.join("escape.md").exists());
        assert!(!root.join("README.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn extraction_refuses_a_linked_aspec_directory() {
        let sandbox = TestSandbox::for_test(TestCase::component(
            "engine::aspec::tests::extraction_refuses_a_linked_aspec_directory",
        ));
        let root = sandbox.work().join("repo");
        let outside = sandbox.work().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("aspec")).unwrap();
        let tree = RepoTree::open(&root).unwrap();
        let mut files = Vec::new();
        let err = AspecDownloader::extract(
            &archive(&[("top/aspec/foundation.md", Some("x"))]),
            &tree,
            &mut files,
        )
        .unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert!(files.is_empty());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }
}
