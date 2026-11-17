//! Optional conformance probes for lease exec parity fields.
use super::{AdminFuture, ConformanceAdmin, Report};
use crate::*;
use serde_json::Value;
use std::collections::BTreeMap;

pub trait LeaseExecAdmin: ConformanceAdmin {
    fn deny_link<'a>(&'a self, id: &'a str) -> AdminFuture<'a>;
}

pub fn cases() -> &'static [&'static str] {
    &[
        "exec_secrets_round_trip",
        "exec_prerequisites_round_trip",
        "exec_workflows_dynamic_round_trip",
        "exec_overlays_round_trip",
        "exec_fields_offered_only_to_capable_workers",
        "incapable_worker_releases_ineligible",
        "link_refused_is_403",
    ]
}

pub async fn run(base_url: &str, admin: &dyn LeaseExecAdmin) -> Report {
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
    admin: &dyn LeaseExecAdmin,
    case: &str,
) -> Result<(), String> {
    if !cases().contains(&case) {
        return Err("unknown lease exec case".into());
    }
    if case == "link_refused_is_403" {
        return refused_link(base_url, admin).await;
    }
    let id = format!(
        "wsp_exec_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let mut item = super::work_item(&id);
    let feature = match case {
        "exec_secrets_round_trip"
        | "exec_fields_offered_only_to_capable_workers"
        | "incapable_worker_releases_ineligible" => {
            item.exec.secrets = ExecSecrets::new([(
                "TOKEN".into(),
                crate::secret::SecretString::new("wire-secret"),
            )])
            .map_err(|e| e.to_string())?;
            features::LEASE_SECRETS
        }
        "exec_prerequisites_round_trip" => {
            item.exec.prerequisites = Some(ExecPrerequisites {
                git_identity: Some(ExecGitIdentity {
                    name: "Test User".into(),
                    email: "test@example.invalid".into(),
                }),
                ssh: Some(ExecCredentialChoice::Deny),
                github_token: None,
            });
            features::LEASE_PREREQUISITES
        }
        "exec_workflows_dynamic_round_trip" => {
            item.exec.workflows_dynamic = Some(ExecWorkflowsDynamic {
                agents_to_models: Some(BTreeMap::from([("claude".into(), vec!["model-x".into()])])),
                default_leader: Some("claude:model-x".into()),
                guidance: Some(vec!["guide".into()]),
            });
            features::LEASE_WORKFLOWS_DYNAMIC
        }
        "exec_overlays_round_trip" => {
            item.exec.overlays = vec![
                "env(TOKEN)".into(),
                "skill(planner)".into(),
                "custom(kind)".into(),
            ];
            features::LEASE_SECRETS
        }
        _ => features::LEASE_SECRETS,
    };
    admin.enqueue(item.clone()).await;
    let mut supported: Vec<String> = features::ALL.iter().map(|f| (*f).to_owned()).collect();
    if case == "incapable_worker_releases_ineligible" {
        supported.retain(|entry| entry != feature);
    }
    let worker = linked(base_url, admin, &format!("{id}_worker"), supported.clone()).await?;
    if case == "incapable_worker_releases_ineligible" {
        return incapable_release(base_url, admin, &worker, &id).await;
    }
    if case == "exec_fields_offered_only_to_capable_workers" {
        supported.retain(|entry| entry != feature);
        let incapable = linked(base_url, admin, &format!("{id}_plain"), supported).await?;
        let queue = incapable.queue(0, None).await.map_err(|e| e.to_string())?;
        if queue.items.iter().any(|entry| entry.id == id) {
            return Err("exec item offered to worker without feature".into());
        }
        let lease = worker
            .claim(Some(&id), 0)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("capable worker was not offered item")?;
        return worker
            .release(
                &lease.id,
                &ReleaseRequest::<Value> {
                    context_changes: None,
                    reason: ReleaseReason::Ineligible,
                    message: None,
                    result: None,
                },
            )
            .await
            .map_err(|e| e.to_string());
    }
    let lease = worker
        .claim(Some(&id), 0)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("item was not offered")?;
    let equal = match case {
        "exec_secrets_round_trip" => lease.item.exec.secrets == item.exec.secrets,
        "exec_prerequisites_round_trip" => lease.item.exec.prerequisites == item.exec.prerequisites,
        "exec_workflows_dynamic_round_trip" => {
            lease.item.exec.workflows_dynamic == item.exec.workflows_dynamic
        }
        "exec_overlays_round_trip" => lease.item.exec.overlays == item.exec.overlays,
        _ => false,
    };
    if !equal {
        return Err("claimed exec field differed from enqueued field".into());
    }
    worker
        .release(
            &lease.id,
            &ReleaseRequest::<Value> {
                context_changes: None,
                reason: ReleaseReason::Error,
                message: None,
                result: None,
            },
        )
        .await
        .map_err(|e| e.to_string())
}

async fn linked(
    base: &str,
    admin: &dyn ConformanceAdmin,
    name: &str,
    features: Vec<String>,
) -> Result<Client, String> {
    let linker = Client::new(base, String::new()).map_err(|e| e.to_string())?;
    let link = linker.start_link().await.map_err(|e| e.to_string())?;
    admin.approve_link(&link.link_id, BTreeMap::new()).await;
    let token = linker
        .poll_link(&link.link_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("link not approved")?;
    let client = Client::new(base, token.refresh_token).map_err(|e| e.to_string())?;
    client
        .register(&RegisterRequest {
            name: name.into(),
            labels: BTreeMap::new(),
            capacity: 1,
            features,
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(client)
}

/// A worker without the item's feature is either never offered it, or (if
/// the server offers it anyway) releases it `ineligible`; the server then
/// never re-offers it to that worker and still delivers it to a capable one.
async fn incapable_release(
    base: &str,
    admin: &dyn LeaseExecAdmin,
    worker: &Client,
    id: &str,
) -> Result<(), String> {
    let ineligible = ReleaseRequest::<Value> {
        context_changes: None,
        reason: ReleaseReason::Ineligible,
        message: Some("this worker does not support the item's exec fields".into()),
        result: None,
    };
    let offered = |outcome: Result<Option<Lease>, Error>| match outcome {
        Ok(lease) => Ok(lease),
        Err(Error::Conflict { .. }) => Ok(None),
        Err(error) => Err(error.to_string()),
    };
    if let Some(lease) = offered(worker.claim(Some(id), 0).await)? {
        worker
            .release(&lease.id, &ineligible)
            .await
            .map_err(|e| e.to_string())?;
        if offered(worker.claim(Some(id), 0).await)?.is_some() {
            return Err("item re-offered to a worker that released it ineligible".into());
        }
    }
    let capable = linked(
        base,
        admin,
        &format!("{id}_capable"),
        features::ALL.iter().map(|f| (*f).to_owned()).collect(),
    )
    .await?;
    let lease = offered(capable.claim(Some(id), 0).await)?
        .ok_or("item not delivered to a capable worker")?;
    capable
        .release(
            &lease.id,
            &ReleaseRequest::<Value> {
                context_changes: None,
                reason: ReleaseReason::Error,
                message: None,
                result: None,
            },
        )
        .await
        .map_err(|e| e.to_string())
}

async fn refused_link(base: &str, admin: &dyn LeaseExecAdmin) -> Result<(), String> {
    let client = Client::new(base, String::new()).map_err(|e| e.to_string())?;
    let link = client.start_link().await.map_err(|e| e.to_string())?;
    admin.deny_link(&link.link_id).await;
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?
        .post(format!("{base}/v1/link/{}/token", link.link_id))
        .header(VERSION_HEADER, WSP_VERSION)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let body: ApiError = response.json().await.map_err(|e| e.to_string())?;
    if status.as_u16() != 403 || body.code != codes::LINK_DENIED {
        return Err(format!(
            "expected 403 access_denied, got {} {}",
            status, body.code
        ));
    }
    Ok(())
}
