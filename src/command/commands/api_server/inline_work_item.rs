//! Private staging of a pushed command's inline work item (`work_item` on
//! `POST /v1/worker/commands`).
//!
//! The body is caller-controlled text. It is written once, owner-only, with an
//! unpredictable name, under the command's own directory (which no agent
//! container mounts), and never reaches the durable arguments, events, logs or
//! process argv. The queued command ID is the durable lookup anchor, so a
//! restarted or re-queued command finds the same staged input and the same
//! source identity.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;
use crate::data::fs::api_paths::ApiPaths;
use crate::data::fs::private_files::{
    create_private_dir_new, create_private_file_new, open_private_file,
};
use crate::data::fs::work_item_draft::MAX_WORK_ITEM_BYTES;
use crate::data::work_item::{InlineWorkItemInput, StagedInlineWorkItem};

const STAGING_DIR: &str = "inline-work-item";
const META_FILE: &str = "meta.json";
/// Written next to the staging directory when a command is admitted with an
/// inline work item, and kept after the body is consumed. Its presence tells
/// "this command had an inline work item that is now missing" (deleted
/// staging, or a restart after cleanup) apart from "this command never had
/// one", so a command never runs without the work item it was admitted with.
const MARKER_FILE: &str = "inline-work-item.expected";
/// Metadata holds the source id and title only, never the body.
const MAX_META_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct Meta {
    source_id: String,
    title: String,
    body_file: String,
}

/// Stages and reloads inline work items under the API command directories.
#[derive(Clone)]
pub struct InlineWorkItemStore {
    paths: ApiPaths,
}

impl InlineWorkItemStore {
    pub fn new(paths: ApiPaths) -> Self {
        Self { paths }
    }

    fn dir(&self, session_id: &str, command_id: &str) -> PathBuf {
        self.paths
            .command_dir(session_id, command_id)
            .join(STAGING_DIR)
    }

    fn marker(&self, session_id: &str, command_id: &str) -> PathBuf {
        self.paths
            .command_dir(session_id, command_id)
            .join(MARKER_FILE)
    }

    /// Write `input` privately for `command_id`. The caller validates it.
    pub fn stage(
        &self,
        session_id: &str,
        command_id: &str,
        input: &InlineWorkItemInput,
    ) -> Result<StagedInlineWorkItem, DataError> {
        let dir = self.dir(session_id, command_id);
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
        }
        let marker = self.marker(session_id, command_id);
        create_private_file_new(&marker)?;
        if let Err(error) = create_private_dir_new(&dir) {
            let _ = std::fs::remove_file(&marker);
            return Err(error);
        }
        match write_staged(&dir, session_id, command_id, input) {
            Ok(staged) => Ok(staged),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&dir);
                let _ = std::fs::remove_file(&marker);
                Err(error)
            }
        }
    }

    /// The staged input of `command_id`, `None` when it never had one. A
    /// missing (already consumed or deleted), partial or tampered staging
    /// directory is an error: a command must never run without the work item
    /// it was admitted with.
    pub fn load(
        &self,
        session_id: &str,
        command_id: &str,
    ) -> Result<Option<StagedInlineWorkItem>, DataError> {
        let dir = self.dir(session_id, command_id);
        match std::fs::symlink_metadata(&dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return match std::fs::symlink_metadata(self.marker(session_id, command_id)) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    _ => Err(invalid(
                        &dir,
                        "the command was admitted with an inline work item that is no longer staged",
                    )),
                };
            }
            Err(e) => return Err(DataError::io(&dir, e)),
        }
        let meta_path = dir.join(META_FILE);
        let mut raw = Vec::new();
        open_private_file(&meta_path)?
            .take(MAX_META_BYTES + 1)
            .read_to_end(&mut raw)
            .map_err(|e| DataError::io(&meta_path, e))?;
        if raw.len() as u64 > MAX_META_BYTES {
            return Err(invalid(&meta_path, "staged metadata is too large"));
        }
        let meta: Meta = serde_json::from_slice(&raw)
            .map_err(|e| invalid(&meta_path, &format!("staged metadata is unreadable: {e}")))?;
        if Path::new(&meta.body_file).components().count() != 1 || meta.body_file.contains('/') {
            return Err(invalid(
                &meta_path,
                "staged body name is not a plain file name",
            ));
        }
        let body_path = dir.join(&meta.body_file);
        // Existence and privacy are checked here; the resolver re-reads it
        // without following links.
        open_private_file(&body_path)?;
        Ok(Some(StagedInlineWorkItem {
            source_id: meta.source_id,
            title: meta.title,
            body_path,
        }))
    }

    /// Admission rollback: the command was never queued, so forget it had
    /// an inline work item at all.
    pub fn discard(&self, session_id: &str, command_id: &str) {
        self.remove(session_id, command_id);
        let _ = std::fs::remove_file(self.marker(session_id, command_id));
    }

    /// Delete the staged body once the command reached a terminal result. The
    /// marker stays, so a re-run of the same command refuses to start.
    pub fn remove(&self, session_id: &str, command_id: &str) {
        let dir = self.dir(session_id, command_id);
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, command_id, "could not remove staged inline work item");
            }
        }
    }
}

fn write_staged(
    dir: &Path,
    session_id: &str,
    command_id: &str,
    input: &InlineWorkItemInput,
) -> Result<StagedInlineWorkItem, DataError> {
    if input.body.len() as u64 > MAX_WORK_ITEM_BYTES {
        return Err(invalid(dir, "work item body is too large"));
    }
    let body_file = format!("body-{}.md", uuid::Uuid::new_v4().simple());
    let body_path = dir.join(&body_file);
    write_new(&body_path, input.body.as_bytes())?;
    let source_id = source_id(session_id, command_id, input.id.as_deref());
    let meta = Meta {
        source_id: source_id.clone(),
        title: input.title.clone(),
        body_file,
    };
    let json = serde_json::to_vec(&meta).map_err(|e| DataError::Other(e.to_string()))?;
    // Metadata last: its presence means the staging is complete.
    write_new(&dir.join(META_FILE), &json)?;
    Ok(StagedInlineWorkItem {
        source_id,
        title: input.title.clone(),
        body_path,
    })
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), DataError> {
    let mut file = create_private_file_new(path)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| DataError::io(path, e))
}

fn invalid(path: &Path, reason: &str) -> DataError {
    DataError::InvalidPath {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    }
}

/// `inline:push:<session-id>:<percent-encoded-id>` when the caller supplied an
/// id, otherwise `inline:push:<session-id>:command:<command-id>`. An encoded
/// caller id never holds a raw `:`, so it can never name another command's
/// identity. The raw id never becomes a file name.
fn source_id(session_id: &str, command_id: &str, id: Option<&str>) -> String {
    match id {
        Some(id) => format!("inline:push:{session_id}:{}", percent_encode(id)),
        None => format!("inline:push:{session_id}:command:{command_id}"),
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(id: Option<&str>) -> InlineWorkItemInput {
        InlineWorkItemInput {
            title: "Fix the thing".into(),
            body: "SECRET body\n".into(),
            id: id.map(str::to_string),
        }
    }

    #[test]
    fn stage_load_remove_round_trip_is_private() {
        let root = tempfile::tempdir().unwrap();
        let store = InlineWorkItemStore::new(ApiPaths::from_root(root.path()));
        let staged = store.stage("s1", "c1", &input(Some("PROJ 7/x"))).unwrap();
        assert_eq!(staged.source_id, "inline:push:s1:PROJ%207%2Fx");
        assert_eq!(
            std::fs::read_to_string(&staged.body_path).unwrap(),
            "SECRET body\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                std::fs::metadata(&staged.body_path).unwrap().mode() & 0o777,
                0o600
            );
            let dir = staged.body_path.parent().unwrap();
            assert_eq!(std::fs::metadata(dir).unwrap().mode() & 0o777, 0o700);
        }
        // Same identity after a reload (restart / retry).
        let loaded = store.load("s1", "c1").unwrap().unwrap();
        assert_eq!(loaded.source_id, staged.source_id);
        assert_eq!(loaded.body_path, staged.body_path);
        assert_eq!(loaded.title, "Fix the thing");
        // Staging twice for one command never overwrites.
        assert!(store.stage("s1", "c1", &input(None)).is_err());
        // Consumed: a re-run must not proceed as if it never had one.
        store.remove("s1", "c1");
        assert!(store.load("s1", "c1").is_err());
        // Admission rollback forgets the command had one.
        store.discard("s1", "c1");
        assert!(store.load("s1", "c1").unwrap().is_none());
        assert!(store.load("s1", "never-had-one").unwrap().is_none());
    }

    #[test]
    fn identity_without_an_id_uses_the_command() {
        let root = tempfile::tempdir().unwrap();
        let store = InlineWorkItemStore::new(ApiPaths::from_root(root.path()));
        let a = store.stage("s1", "c1", &input(None)).unwrap();
        let b = store.stage("s1", "c2", &input(None)).unwrap();
        assert_eq!(a.source_id, "inline:push:s1:command:c1");
        assert_ne!(a.source_id, b.source_id);
        // A caller id equal to a command id (or to the `command:` form) never
        // shares that command's identity.
        for id in ["c1", "command:c1"] {
            let c = store
                .stage("s1", &format!("x-{id}"), &input(Some(id)))
                .unwrap();
            assert_ne!(c.source_id, a.source_id, "{id}");
        }
    }

    #[test]
    fn the_largest_admissible_title_and_id_load_back() {
        use crate::data::work_item::{MAX_INLINE_ID_BYTES, MAX_INLINE_TITLE_BYTES};
        let root = tempfile::tempdir().unwrap();
        let store = InlineWorkItemStore::new(ApiPaths::from_root(root.path()));
        // Every id byte expands threefold under percent encoding.
        let input = InlineWorkItemInput {
            title: "\u{1F600}".repeat(MAX_INLINE_TITLE_BYTES / 4),
            body: "b".into(),
            id: Some("/".repeat(MAX_INLINE_ID_BYTES)),
        };
        input.validate().unwrap();
        store.stage("s1", "c1", &input).unwrap();
        assert_eq!(store.load("s1", "c1").unwrap().unwrap().title, input.title);
    }

    #[test]
    fn a_partial_staging_directory_is_a_hard_error() {
        let root = tempfile::tempdir().unwrap();
        let store = InlineWorkItemStore::new(ApiPaths::from_root(root.path()));
        let staged = store.stage("s1", "c1", &input(None)).unwrap();
        std::fs::remove_file(&staged.body_path).unwrap();
        assert!(store.load("s1", "c1").is_err());
        // The whole staging directory deleted while queued is an error too.
        let staged = store.stage("s1", "c2", &input(None)).unwrap();
        std::fs::remove_dir_all(staged.body_path.parent().unwrap()).unwrap();
        assert!(store.load("s1", "c2").is_err());
    }
}
