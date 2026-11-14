//! Inline work item DTOs (Layer 0).
//!
//! An inline work item arrives with the request (`work_item` on a pushed API
//! command, or a pulled lease body) instead of being fetched from a source.
//! It is read-only execution input. Debug output never includes the body.

use std::fmt;
use std::path::PathBuf;

use crate::data::fs::work_item_draft::MAX_WORK_ITEM_BYTES;

/// Longest accepted inline title, in bytes.
pub const MAX_INLINE_TITLE_BYTES: usize = 1024;
/// Longest accepted inline id, in bytes (it is stored percent-encoded, so up
/// to three times longer).
pub const MAX_INLINE_ID_BYTES: usize = 256;

/// Caller-supplied work item: `{ title, body, id? }`.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlineWorkItemInput {
    pub title: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl InlineWorkItemInput {
    /// Reject a blank title, a blank supplied id, or an oversized body.
    pub fn validate(&self) -> Result<(), String> {
        if self.title.trim().is_empty() {
            return Err("work_item.title must not be blank".into());
        }
        if self.id.as_deref().is_some_and(|id| id.trim().is_empty()) {
            return Err("work_item.id must not be blank when supplied".into());
        }
        if self.title.len() > MAX_INLINE_TITLE_BYTES {
            return Err(format!(
                "work_item.title exceeds the {MAX_INLINE_TITLE_BYTES}-byte limit"
            ));
        }
        if self
            .id
            .as_deref()
            .is_some_and(|id| id.len() > MAX_INLINE_ID_BYTES)
        {
            return Err(format!(
                "work_item.id exceeds the {MAX_INLINE_ID_BYTES}-byte limit"
            ));
        }
        if self.body.len() as u64 > MAX_WORK_ITEM_BYTES {
            return Err(format!(
                "work_item.body exceeds the {MAX_WORK_ITEM_BYTES}-byte limit"
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for InlineWorkItemInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InlineWorkItemInput")
            .field("title", &self.title)
            .field(
                "body",
                &format_args!("<{} bytes redacted>", self.body.len()),
            )
            .field("id", &self.id)
            .finish()
    }
}

/// An inline work item whose body has been written to a private file.
#[derive(Clone)]
pub struct StagedInlineWorkItem {
    pub source_id: String,
    pub title: String,
    pub body_path: PathBuf,
}

impl fmt::Debug for StagedInlineWorkItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StagedInlineWorkItem")
            .field("source_id", &self.source_id)
            .field("title", &self.title)
            .field("body_path", &self.body_path)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_the_body() {
        let item = InlineWorkItemInput {
            title: "t".into(),
            body: "SECRET BODY".into(),
            id: None,
        };
        assert!(!format!("{item:?}").contains("SECRET"));
    }

    #[test]
    fn validation_rejects_blank_title_blank_id_and_oversize() {
        let ok = InlineWorkItemInput {
            title: "t".into(),
            body: "b".into(),
            id: Some("x".into()),
        };
        assert!(ok.validate().is_ok());
        assert!(InlineWorkItemInput {
            title: " ".into(),
            ..ok.clone()
        }
        .validate()
        .is_err());
        assert!(InlineWorkItemInput {
            id: Some(" ".into()),
            ..ok.clone()
        }
        .validate()
        .is_err());
        let big = InlineWorkItemInput {
            body: "x".repeat(MAX_WORK_ITEM_BYTES as usize + 1),
            ..ok
        };
        assert!(big.validate().is_err());
    }

    #[test]
    fn title_and_id_lengths_are_bounded() {
        let at_limit = InlineWorkItemInput {
            title: "a".repeat(MAX_INLINE_TITLE_BYTES),
            body: "b".into(),
            id: Some("/".repeat(MAX_INLINE_ID_BYTES)),
        };
        assert!(at_limit.validate().is_ok());
        assert!(InlineWorkItemInput {
            title: "a".repeat(MAX_INLINE_TITLE_BYTES + 1),
            ..at_limit.clone()
        }
        .validate()
        .is_err());
        assert!(InlineWorkItemInput {
            id: Some("a".repeat(MAX_INLINE_ID_BYTES + 1)),
            ..at_limit
        }
        .validate()
        .is_err());
    }

    #[test]
    fn unknown_fields_are_refused() {
        let r: Result<InlineWorkItemInput, _> =
            serde_json::from_str(r#"{"title":"t","body":"b","extra":1}"#);
        assert!(r.is_err());
    }
}
