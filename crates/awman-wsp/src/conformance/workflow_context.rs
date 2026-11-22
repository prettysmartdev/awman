//! Optional conformance probes for requested workflow artifact attachments.
use super::{ConformanceAdmin, Report};
use crate::*;
use serde_json::json;
use std::{future::Future, pin::Pin};

pub trait WorkflowContextAdmin: ConformanceAdmin {
    fn workflow_context_result<'a>(
        &'a self,
        item: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<WorkflowContextResult>> + Send + 'a>>;
}

pub fn cases() -> &'static [&'static str] {
    &[
        "workflow_context_offered_only_to_capable_workers",
        "workflow_artifact_reaches_server",
        "lost_epoch_artifacts_are_discarded",
    ]
}

fn ensure(ok: bool, message: &str) -> Result<(), String> {
    ok.then_some(()).ok_or_else(|| message.to_owned())
}

async fn linked(
    base: &str,
    admin: &dyn ConformanceAdmin,
    name: &str,
    features: Vec<String>,
) -> Result<Client, String> {
    let client = Client::new(base, String::new()).map_err(|error| error.to_string())?;
    let link = client
        .start_link()
        .await
        .map_err(|error| error.to_string())?;
    admin.approve_link(&link.link_id, Default::default()).await;
    let token = client
        .poll_link(&link.link_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("link was not approved")?;
    let worker = Client::new(base, token.refresh_token).map_err(|error| error.to_string())?;
    worker
        .register(&RegisterRequest {
            name: name.to_owned(),
            labels: Default::default(),
            capacity: 1,
            features,
        })
        .await
        .map_err(|error| error.to_string())?;
    Ok(worker)
}

fn item(id: &str) -> WorkItem {
    let mut item = super::work_item(id);
    item.workflow_context = Some(ItemWorkflowContext { max_bytes: 1024 });
    item
}

pub async fn run(base_url: &str, admin: &dyn WorkflowContextAdmin) -> Report {
    let mut report = Report::default();
    for &case in cases() {
        match run_case(base_url, admin, case).await {
            Ok(()) => report.passed.push(case),
            Err(error) => report.failed.push((case, error)),
        }
    }
    report
}

pub async fn run_case(
    base_url: &str,
    admin: &dyn WorkflowContextAdmin,
    case: &str,
) -> Result<(), String> {
    ensure(cases().contains(&case), "unknown workflow-context case")?;
    let suffix = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let id = format!("workflow_{suffix}_{case}");
    let capable = linked(
        base_url,
        admin,
        &format!("{id}_capable"),
        vec![features::LEASE_WORKFLOW_CONTEXT.to_owned()],
    )
    .await?;
    admin.enqueue(item(&id)).await;
    match case {
        "workflow_context_offered_only_to_capable_workers" => {
            let plain = linked(base_url, admin, &format!("{id}_plain"), vec![]).await?;
            let queue = plain
                .queue(50, None)
                .await
                .map_err(|error| error.to_string())?;
            ensure(
                !queue.items.iter().any(|entry| entry.id == id),
                "workflow-context item was offered to a worker without the capability",
            )?;
            match plain.claim(Some(&id), 0).await {
                Ok(None) | Err(_) => {}
                Ok(Some(_)) => {
                    return Err(
                        "workflow-context item was directly claimable without the capability"
                            .into(),
                    )
                }
            }
            let lease = capable
                .claim(Some(&id), 0)
                .await
                .map_err(|error| error.to_string())?
                .ok_or("capable worker was not offered the item")?;
            capable
                .release(
                    &lease.id,
                    &ReleaseRequest::<serde_json::Value> {
                        context_changes: None,
                        reason: ReleaseReason::Error,
                        message: None,
                        result: None,
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        "workflow_artifact_reaches_server" => {
            let lease = capable
                .claim(Some(&id), 0)
                .await
                .map_err(|error| error.to_string())?
                .ok_or("capable worker was not offered the item")?;
            let attachment = WorkflowContextResult {
                collection_error: None,
                items: vec![WorkflowContextItem {
                    path: "attached-note.md".into(),
                    size_bytes: 1,
                    sha256: "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
                        .into(),
                    modified_at: None,
                    content_base64: Some("eA==".into()),
                }],
                next_cursor: None,
                truncated: false,
            };
            capable
                .result(
                    &lease.id,
                    &LeaseResult {
                        context_changes: None,
                        workflow_context: Some(attachment.clone()),
                        result: json!({"outcome":"done"}),
                        outputs: None,
                        outputs_mounted: false,
                        duration_ms: 1,
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            ensure(
                admin.workflow_context_result(&id).await.as_ref() == Some(&attachment),
                "reference server did not retain the attached workflow note",
            )
        }
        "lost_epoch_artifacts_are_discarded" => {
            let lease = capable
                .claim(Some(&id), 0)
                .await
                .map_err(|error| error.to_string())?
                .ok_or("capable worker was not offered the item")?;
            let old = capable.with_lease_epoch(&lease.id, lease.epoch);
            admin.expire_lease_reusing_id(&lease.id).await;
            let next = capable
                .claim(Some(&id), 0)
                .await
                .map_err(|error| error.to_string())?
                .ok_or("expired item was not reoffered")?;
            ensure(
                next.id != lease.id || next.epoch > lease.epoch,
                "reissued lease did not advance its epoch",
            )?;
            let stale = old
                .result(
                    &lease.id,
                    &LeaseResult {
                        context_changes: None,
                        workflow_context: Some(WorkflowContextResult {
                            collection_error: None,
items: vec![WorkflowContextItem { path: "lost.md".into(), size_bytes: 1, sha256: "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881".into(), modified_at: None, content_base64: Some("eA==".into()) }],
                            next_cursor: None,
                            truncated: false,
                        }),
                        result: json!({"outcome":"done"}),
                        outputs: None,
                        outputs_mounted: false,
                        duration_ms: 1,
                    },
                )
                .await;
            ensure(stale.is_err(), "stale epoch result was accepted")?;
            ensure(
                admin.workflow_context_result(&id).await.is_none(),
                "reference server retained artifacts from a lost epoch",
            )?;
            capable
                .release(
                    &next.id,
                    &ReleaseRequest::<serde_json::Value> {
                        context_changes: None,
                        reason: ReleaseReason::Error,
                        message: None,
                        result: None,
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        _ => unreachable!(),
    }
}
