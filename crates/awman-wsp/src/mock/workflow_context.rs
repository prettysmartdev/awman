//! Owned by wsp-workflow-artifacts: artifact recording and offer eligibility.
use crate::{LeaseResult, WorkItem};
use serde_json::Value;
pub fn offer_allowed(features: &[String], item: &WorkItem) -> bool {
    item.workflow_context.is_none()
        || features
            .iter()
            .any(|f| f == crate::features::LEASE_WORKFLOW_CONTEXT)
}
pub(super) fn on_result(
    state: &mut super::State,
    item: &str,
    result: &LeaseResult<Value>,
) -> Result<(), crate::ApiError> {
    let Some(attachment) = &result.workflow_context else {
        return Ok(());
    };
    let request = state
        .items
        .iter()
        .find(|e| e.item.id == item)
        .and_then(|e| e.item.workflow_context.as_ref())
        .ok_or_else(|| super::error("unexpected_workflow_context"))?;
    validate(attachment, request.max_bytes).map_err(|_| super::error("invalid_workflow_context"))
}
fn validate(attachment: &crate::WorkflowContextResult, max_bytes: u64) -> Result<(), ()> {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    if attachment.collection_error.is_some() && !attachment.truncated {
        return Err(());
    }
    if attachment.next_cursor.is_some() {
        return Err(());
    }
    let mut bytes = 0u64;
    let mut names = std::collections::BTreeSet::new();
    for item in &attachment.items {
        if !super::lease_context::valid_path(&item.path) || !names.insert(&item.path) {
            return Err(());
        }
        if let Some(encoded) = &item.content_base64 {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| ())?;
            bytes = bytes.checked_add(decoded.len() as u64).ok_or(())?;
            if bytes > max_bytes.min(16 << 20)
                || decoded.len() as u64 != item.size_bytes
                || Sha256::digest(&decoded)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
                    != item.sha256
            {
                return Err(());
            }
        } else if !attachment.truncated {
            return Err(());
        }
    }
    Ok(())
}

impl super::MockWorkServer {
    /// Inspect the attachment accepted by the reference server in tests and
    /// optional conformance probes.
    pub fn workflow_context_result(&self, item: &str) -> Option<crate::WorkflowContextResult> {
        self.inner
            .lock()
            .unwrap()
            .results
            .get(item)
            .and_then(|result| result.workflow_context.clone())
    }
}

#[cfg(feature = "mock")]
impl crate::conformance::workflow_context::WorkflowContextAdmin for super::MockWorkServer {
    fn workflow_context_result<'a>(
        &'a self,
        item: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<crate::WorkflowContextResult>> + Send + 'a>,
    > {
        Box::pin(async move { self.workflow_context_result(item) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_artifact_content_hash_size_budget_and_complete_envelope() {
        let attachment = crate::WorkflowContextResult {
            collection_error: None,
            items: vec![crate::WorkflowContextItem {
                path: "note".into(),
                size_bytes: 1,
                sha256: "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881".into(),
                modified_at: None,
                content_base64: Some("eA==".into()),
            }],
            next_cursor: None,
            truncated: false,
        };
        assert!(validate(&attachment, 1).is_ok());
        assert!(validate(&attachment, 0).is_err());
        let mut bad = attachment.clone();
        bad.items[0].sha256 = "wrong".into();
        assert!(validate(&bad, 1).is_err());
        let mut bad = attachment.clone();
        bad.items[0].size_bytes = 2;
        assert!(validate(&bad, 1).is_err());
        let mut bad = attachment.clone();
        bad.next_cursor = Some("next".into());
        assert!(validate(&bad, 1).is_err());
        let mut bad = attachment;
        bad.items[0].content_base64 = None;
        assert!(validate(&bad, 1).is_err());
    }
}
