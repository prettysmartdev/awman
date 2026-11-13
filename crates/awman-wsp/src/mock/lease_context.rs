//! Server-owned lease memory for the mock (the `lease_context` feature).
//!
//! The mock keeps one memory per repository URL. An item enqueued with
//! [`MockWorkServer::enqueue_with_context`] carries the current memory as its
//! `WorkItem.context`; a lease's `context_changes` (on a result or a release)
//! are applied to that memory before the call returns, so the next lease sees
//! them. A change set is applied whole or not at all: a metadata-only report
//! (the worker's `context_too_large` failure) or any invalid entry changes
//! nothing. Changes from a lost epoch never get here: `result` and `release`
//! fence the epoch first.
use super::{error, MockWorkServer, State};
use crate::{
    ApiError, ContextChangeKind, ContextChanges, ItemContext, ItemContextFile, LeaseResult,
    ReleaseRequest, WorkItem,
};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;

/// One repository's memory: relative path → content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextStore {
    pub files: BTreeMap<String, Vec<u8>>,
}

impl ContextStore {
    /// Decode a lease snapshot. `None` when any entry is not valid base64.
    pub fn from_item(context: &ItemContext) -> Option<Self> {
        let mut files = BTreeMap::new();
        for f in &context.files {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&f.content_base64)
                .ok()?;
            if files.insert(f.path.clone(), bytes).is_some() {
                return None;
            }
        }
        let store = Self { files };
        store.validate().ok()?;
        Some(store)
    }

    /// The snapshot a lease receives.
    pub fn to_item(&self, max_bytes: u64) -> ItemContext {
        ItemContext {
            files: self
                .files
                .iter()
                .map(|(path, bytes)| ItemContextFile {
                    path: path.clone(),
                    content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                })
                .collect(),
            max_bytes,
        }
    }

    /// Apply a change set atomically: the updated memory, or why it was refused.
    pub fn apply(&self, changes: &ContextChanges) -> Result<Self, String> {
        if changes.next_cursor.is_some() {
            return Err("context_changes.next_cursor must be null".into());
        }
        let mut next = self.clone();
        for item in &changes.items {
            if !valid_path(&item.path) {
                return Err(format!("invalid context path {:?}", item.path));
            }
            match item.change {
                ContextChangeKind::Deleted => {
                    if item.content_base64.is_some() {
                        return Err(format!("{}: a deletion carries no content", item.path));
                    }
                    next.files.remove(&item.path);
                }
                ContextChangeKind::Added | ContextChangeKind::Modified => {
                    let encoded = item.content_base64.as_deref().ok_or_else(|| {
                        format!("{}: no content (metadata-only report)", item.path)
                    })?;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|_| format!("{}: content is not valid base64", item.path))?;
                    if item
                        .size_bytes
                        .is_some_and(|size| size != bytes.len() as u64)
                    {
                        return Err(format!("{}: size_bytes does not match", item.path));
                    }
                    next.files.insert(item.path.clone(), bytes);
                }
            }
        }
        next.validate()?;
        Ok(next)
    }
    fn validate(&self) -> Result<(), String> {
        if self.files.len() > 1000 {
            return Err("context file count exceeds limit".into());
        }
        if self.files.values().map(|b| b.len() as u64).sum::<u64>() > 16 << 20 {
            return Err("context bytes exceed limit".into());
        }
        let mut names = std::collections::BTreeSet::new();
        let mut dirs = std::collections::BTreeSet::new();
        for path in self.files.keys() {
            if !valid_path(path) {
                return Err("invalid context path".into());
            }
            let folded = path.to_lowercase();
            if !names.insert(folded.clone()) || dirs.contains(&folded) {
                return Err("context path collision".into());
            }
            for (at, _) in folded.match_indices('/') {
                let parent = &folded[..at];
                if names.contains(parent) {
                    return Err("context file/directory collision".into());
                }
                dirs.insert(parent.to_owned());
            }
        }
        Ok(())
    }
}

/// The relative-path rules every party applies (no traversal, no absolute
/// path, no backslash, colon or control character, no empty segment).
pub(super) fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && path.split('/').count() <= 16
        && !path
            .chars()
            .any(|c| c == '\\' || c == ':' || c.is_ascii_control())
        && path.split('/').all(|s| {
            let upper = s.split('.').next().unwrap_or("").to_ascii_uppercase();
            let reserved = matches!(
                upper.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ) || ["COM", "LPT"].iter().any(|p| {
                upper.strip_prefix(p).is_some_and(|n| {
                    n.len() == 1 && n.as_bytes()[0].is_ascii_digit() || matches!(n, "¹" | "²" | "³")
                })
            });
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.len() <= 255
                && !s.ends_with(['.', ' '])
                && !reserved
        })
}

pub fn offer_allowed(features: &[String], item: &WorkItem) -> bool {
    item.context.is_none() || features.iter().any(|f| f == crate::features::LEASE_CONTEXT)
}

/// The memory held for `repo_url`: the context of any entry for that
/// repository (they are kept identical whenever a change set is applied).
fn memory(state: &State, repo_url: &str) -> Option<ContextStore> {
    state
        .items
        .iter()
        .rev()
        .filter(|e| e.item.repo.url == repo_url)
        .find_map(|e| e.item.context.as_ref())
        .and_then(ContextStore::from_item)
}

fn apply(state: &mut State, item: &str, changes: Option<&ContextChanges>) {
    let Some(changes) = changes else { return };
    let Some(entry) = state.items.iter().find(|e| e.item.id == item) else {
        return;
    };
    // Only an item that asked for memory may update it.
    if entry.item.context.is_none() {
        return;
    }
    let url = entry.item.repo.url.clone();
    let Some(current) = memory(state, &url) else {
        return;
    };
    let cap = entry
        .item
        .context
        .as_ref()
        .map_or(0, |c| c.max_bytes)
        .min(16 << 20);
    let Ok(next) = current.apply(changes) else {
        return;
    };
    if next.files.values().map(|b| b.len() as u64).sum::<u64>() > cap {
        return;
    }
    for e in state
        .items
        .iter_mut()
        .filter(|e| e.item.repo.url == url && e.item.context.is_some())
    {
        let max_bytes = e.item.context.as_ref().map_or(0, |c| c.max_bytes);
        e.item.context = Some(next.to_item(max_bytes));
    }
}

pub(super) fn on_result(state: &mut State, item: &str, result: &LeaseResult<Value>) {
    apply(state, item, result.context_changes.as_ref());
}
pub(super) fn on_release(state: &mut State, item: &str, request: &ReleaseRequest<Value>) {
    apply(state, item, request.context_changes.as_ref());
}

impl MockWorkServer {
    /// Enqueue `item` carrying the server's memory for its repository. When no
    /// memory exists yet, the item's own `context` seeds it. The item's
    /// `max_bytes` is kept.
    pub fn enqueue_with_context(&self, item: WorkItem, at: DateTime<Utc>) -> Result<(), ApiError> {
        self.enqueue_context_transaction(item, at, || {})
    }
    fn enqueue_context_transaction(
        &self,
        mut item: WorkItem,
        at: DateTime<Utc>,
        before_insert: impl FnOnce(),
    ) -> Result<(), ApiError> {
        let max_bytes = item
            .context
            .as_ref()
            .map_or(ItemContext::default_max_bytes(), |c| c.max_bytes);
        let mut state = self.inner.lock().unwrap();
        let held = memory(&state, &item.repo.url);
        if let Some(held) = held {
            item.context = Some(held.to_item(max_bytes));
        } else if item.context.is_none() {
            item.context = Some(ContextStore::default().to_item(max_bytes));
        }
        if item
            .context
            .as_ref()
            .is_some_and(|c| ContextStore::from_item(c).is_none())
        {
            return Err(error("invalid_context"));
        }
        before_insert();
        Self::enqueue_locked(&mut state, item, at)?;
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    /// The memory held for a repository, if any item for it carries context.
    pub fn context_for(&self, repo_url: &str) -> Option<ContextStore> {
        memory(&self.inner.lock().unwrap(), repo_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContextChangeItem;

    fn change(path: &str, change: ContextChangeKind, content: Option<&[u8]>) -> ContextChangeItem {
        ContextChangeItem {
            path: path.into(),
            change,
            size_bytes: content.map(|c| c.len() as u64),
            sha256: None,
            content_base64: content.map(|c| base64::engine::general_purpose::STANDARD.encode(c)),
        }
    }

    #[test]
    fn change_sets_apply_whole_or_not_at_all() {
        let store = ContextStore {
            files: BTreeMap::from([("a".into(), b"1".to_vec()), ("b".into(), b"2".to_vec())]),
        };
        let next = store
            .apply(&ContextChanges {
                items: vec![
                    change("a", ContextChangeKind::Modified, Some(b"3")),
                    change("b", ContextChangeKind::Deleted, None),
                    change("c/d", ContextChangeKind::Added, Some(b"4")),
                ],
                next_cursor: None,
            })
            .unwrap();
        assert_eq!(
            next.files,
            BTreeMap::from([("a".into(), b"3".to_vec()), ("c/d".into(), b"4".to_vec())])
        );
        for bad in [
            change("a", ContextChangeKind::Modified, None),
            change("../x", ContextChangeKind::Added, Some(b"x")),
            change("/x", ContextChangeKind::Added, Some(b"x")),
            change("b", ContextChangeKind::Deleted, Some(b"x")),
        ] {
            let set = ContextChanges {
                items: vec![change("z", ContextChangeKind::Added, Some(b"z")), bad],
                next_cursor: None,
            };
            assert!(store.apply(&set).is_err());
        }
    }

    #[test]
    fn resulting_memory_rejects_case_collisions_and_too_many_files_atomically() {
        let store = ContextStore {
            files: BTreeMap::from([("note".into(), b"old".to_vec())]),
        };
        for changes in [
            vec![change("NOTE", ContextChangeKind::Added, Some(b"new"))],
            (0..1000)
                .map(|i| change(&format!("f-{i}"), ContextChangeKind::Added, Some(b"")))
                .collect(),
        ] {
            assert!(store
                .apply(&ContextChanges {
                    items: changes,
                    next_cursor: None
                })
                .is_err());
        }
        assert_eq!(store.files["note"], b"old");
    }
    #[test]
    fn enqueue_holds_the_memory_transaction_through_insertion() {
        let server = std::sync::Arc::new(MockWorkServer::default());
        let mut first = crate::conformance::work_item("first");
        first.context = Some(
            ContextStore {
                files: BTreeMap::from([("note".into(), b"v0".to_vec())]),
            }
            .to_item(1024),
        );
        let url = first.repo.url.clone();
        server.enqueue_with_context(first, Utc::now()).unwrap();
        let (selected_tx, selected_rx) = std::sync::mpsc::channel();
        let (insert_tx, insert_rx) = std::sync::mpsc::channel();
        let enqueue_server = server.clone();
        let thread = std::thread::spawn(move || {
            enqueue_server
                .enqueue_context_transaction(
                    crate::conformance::work_item("second"),
                    Utc::now(),
                    || {
                        selected_tx.send(()).unwrap();
                        insert_rx
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                    },
                )
                .unwrap();
        });
        selected_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        // A result cannot update memory between snapshot selection and enqueue.
        let locked = matches!(
            server.inner.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        );
        insert_tx.send(()).unwrap();
        thread.join().unwrap();
        assert!(locked);
        apply(
            &mut server.inner.lock().unwrap(),
            "first",
            Some(&ContextChanges {
                items: vec![change("note", ContextChangeKind::Modified, Some(b"v1"))],
                next_cursor: None,
            }),
        );
        server
            .enqueue_with_context(crate::conformance::work_item("third"), Utc::now())
            .unwrap();
        assert_eq!(server.context_for(&url).unwrap().files["note"], b"v1");
    }
    #[test]
    fn only_capable_workers_are_offered_context() {
        let mut item = crate::conformance::work_item("memory");
        assert!(offer_allowed(&[], &item));
        item.context = Some(ContextStore::default().to_item(16));
        assert!(!offer_allowed(&[], &item));
        assert!(offer_allowed(
            &[crate::features::LEASE_CONTEXT.to_string()],
            &item
        ));
    }
}
