//! Optional conformance for the `lease_context` feature: a server offers
//! context-bearing items only to workers that registered the feature, and
//! applies a lease's `context_changes` (from a result or a release) before the
//! next lease of the same memory is offered. Changes from a lost epoch and
//! metadata-only reports are never applied.
//!
//! Servers that do not implement the feature are not required to pass this
//! suite; base conformance (`conformance::run`) is unaffected.
use super::{AdminFuture, ConformanceAdmin, Report};
use crate::*;
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Fixtures for the optional lease-context probes.
pub trait LeaseContextAdmin: ConformanceAdmin {
    /// Enqueue `item` carrying the server's current memory for
    /// `item.repo.url` as `item.context` (keeping its `max_bytes`). When the
    /// server holds no memory for that repository yet, `item.context` seeds it.
    fn enqueue_with_context(&self, item: WorkItem) -> AdminFuture<'_>;
}

pub fn cases() -> &'static [&'static str] {
    &[
        "context_offered_only_to_capable_workers",
        "result_changes_reach_the_next_lease",
        "release_changes_reach_the_next_lease",
        "lost_epoch_changes_are_discarded",
        "metadata_only_changes_are_not_applied",
    ]
}

type Files = BTreeMap<String, Vec<u8>>;

fn files(pairs: &[(&str, &str)]) -> Files {
    pairs
        .iter()
        .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
        .collect()
}

fn encode(files: &Files) -> ItemContext {
    ItemContext {
        files: files
            .iter()
            .map(|(path, bytes)| ItemContextFile {
                path: path.clone(),
                content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            })
            .collect(),
        max_bytes: 4096,
    }
}

fn decode(context: Option<&ItemContext>) -> Result<Files, String> {
    let context = context.ok_or("the lease carries no context")?;
    context
        .files
        .iter()
        .map(|f| {
            base64::engine::general_purpose::STANDARD
                .decode(&f.content_base64)
                .map(|b| (f.path.clone(), b))
                .map_err(|_| format!("{}: content is not valid base64", f.path))
        })
        .collect()
}

fn item(id: &str, memory: &str, seed: Option<&Files>) -> WorkItem {
    let mut item = super::work_item(id);
    item.repo.url = format!("https://example.invalid/{memory}.git");
    item.context = Some(encode(seed.unwrap_or(&Files::new())));
    item
}

fn change(path: &str, kind: ContextChangeKind, content: Option<&str>) -> ContextChangeItem {
    ContextChangeItem {
        path: path.into(),
        change: kind,
        size_bytes: content.map(|c| c.len() as u64),
        sha256: None,
        content_base64: content.map(|c| base64::engine::general_purpose::STANDARD.encode(c)),
    }
}

/// Modify `a.md`, delete `gone.md`, add `dir/new.md`.
fn edits() -> ContextChanges {
    ContextChanges {
        items: vec![
            change("a.md", ContextChangeKind::Modified, Some("two")),
            change("dir/new.md", ContextChangeKind::Added, Some("new")),
            change("gone.md", ContextChangeKind::Deleted, None),
        ],
        next_cursor: None,
    }
}

fn seed() -> Files {
    files(&[("a.md", "one"), ("gone.md", "bye")])
}

fn edited() -> Files {
    files(&[("a.md", "two"), ("dir/new.md", "new")])
}

fn result(changes: Option<ContextChanges>, outcome: &str) -> LeaseResult<Value> {
    LeaseResult {
        context_changes: changes,
        workflow_context: None,
        result: json!({"outcome": outcome, "exit_code": if outcome == "done" { 0 } else { 1 }}),
        outputs: None,
        outputs_mounted: false,
        duration_ms: 1,
    }
}

async fn linked(
    base: &str,
    admin: &dyn LeaseContextAdmin,
    name: &str,
    features: Vec<String>,
) -> Result<Client, String> {
    let e = |e: Error| e.to_string();
    let linker = Client::new(base, String::new()).map_err(e)?;
    let link = linker.start_link().await.map_err(e)?;
    admin
        .approve_link(
            &link.link_id,
            BTreeMap::from([("owner".into(), name.into())]),
        )
        .await;
    let token = linker
        .poll_link(&link.link_id)
        .await
        .map_err(e)?
        .ok_or("link was not approved")?;
    let client = Client::new(base, token.refresh_token).map_err(e)?;
    client
        .register(&RegisterRequest {
            name: name.into(),
            labels: BTreeMap::new(),
            capacity: 100,
            features,
        })
        .await
        .map_err(e)?;
    Ok(client)
}

fn ensure(ok: bool, what: &str) -> Result<(), String> {
    ok.then_some(()).ok_or_else(|| what.to_owned())
}

async fn claim(client: &Client, id: &str) -> Result<Lease, String> {
    client
        .claim(Some(id), 0)
        .await
        .map_err(|e| format!("claim {id}: {e}"))?
        .ok_or_else(|| format!("claim {id}: nothing offered"))
}

/// Claim `next` and check the memory it carries.
async fn next_lease_sees(
    client: &Client,
    admin: &dyn LeaseContextAdmin,
    id: &str,
    memory: &str,
    expected: &Files,
) -> Result<(), String> {
    admin.enqueue_with_context(item(id, memory, None)).await;
    let lease = claim(client, id).await?;
    let seen = decode(lease.item.context.as_ref())?;
    client
        .result(&lease.id, &result(None, "done"))
        .await
        .map_err(|e| e.to_string())?;
    ensure(
        &seen == expected,
        &format!(
            "next lease saw {:?}, expected {:?}",
            keys(&seen),
            keys(expected)
        ),
    )
}

fn keys(files: &Files) -> Vec<(&str, String)> {
    files
        .iter()
        .map(|(p, b)| (p.as_str(), String::from_utf8_lossy(b).into_owned()))
        .collect()
}

pub async fn run(base_url: &str, admin: &dyn LeaseContextAdmin) -> Report {
    let mut report = Report::default();
    for &case in cases() {
        match run_case(base_url, admin, case).await {
            Ok(()) => report.passed.push(case),
            Err(e) => report.failed.push((case, e)),
        }
    }
    report
}

/// Run one probe with fresh workers and a fresh memory.
pub async fn run_case(
    base_url: &str,
    admin: &dyn LeaseContextAdmin,
    case: &str,
) -> Result<(), String> {
    ensure(cases().contains(&case), "unknown case")?;
    let prefix = format!(
        "wspctx_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let all: Vec<String> = features::ALL.iter().map(|f| f.to_string()).collect();
    let capable = linked(base_url, admin, &format!("{prefix}_a"), all.clone()).await?;
    let other = linked(base_url, admin, &format!("{prefix}_b"), all.clone()).await?;
    let memory = format!("{prefix}_{case}");
    let id = format!("{memory}_1");
    let next = format!("{memory}_2");
    admin
        .enqueue_with_context(item(&id, &memory, Some(&seed())))
        .await;
    match case {
        "context_offered_only_to_capable_workers" => {
            let plain_features: Vec<String> = all
                .into_iter()
                .filter(|f| f != features::LEASE_CONTEXT)
                .collect();
            let plain = linked(base_url, admin, &format!("{prefix}_c"), plain_features).await?;
            let queue = plain.queue(500, None).await.map_err(|e| e.to_string())?;
            ensure(
                !queue.items.iter().any(|i| i.id == id),
                "item offered in the queue of a worker without lease_context",
            )?;
            ensure(
                plain
                    .claim(Some(&id), 0)
                    .await
                    .is_err_and(|e| e.is_ineligible()),
                "a worker without lease_context could claim a context item",
            )?;
            let lease = claim(&capable, &id).await?;
            let seen = decode(lease.item.context.as_ref())?;
            capable
                .result(&lease.id, &result(None, "done"))
                .await
                .map_err(|e| e.to_string())?;
            ensure(
                seen == seed(),
                "the lease did not carry the server's memory",
            )
        }
        "result_changes_reach_the_next_lease" => {
            let lease = claim(&capable, &id).await?;
            capable
                .result(&lease.id, &result(Some(edits()), "done"))
                .await
                .map_err(|e| e.to_string())?;
            // Another worker (another host) observes the update.
            next_lease_sees(&other, admin, &next, &memory, &edited()).await
        }
        "release_changes_reach_the_next_lease" => {
            let lease = claim(&capable, &id).await?;
            capable
                .release(
                    &lease.id,
                    &ReleaseRequest {
                        context_changes: Some(edits()),
                        reason: ReleaseReason::Error,
                        message: Some("failed run with safe memory changes".into()),
                        result: Some(json!({"outcome": "error", "exit_code": 1})),
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            next_lease_sees(&other, admin, &next, &memory, &edited()).await
        }
        "lost_epoch_changes_are_discarded" => {
            let lease = claim(&capable, &id).await?;
            let stale = capable.with_lease_epoch(&lease.id, lease.epoch);
            admin.expire_lease_reusing_id(&lease.id).await;
            let current = claim(&capable, &id).await?;
            ensure(
                stale
                    .result(&lease.id, &result(Some(edits()), "done"))
                    .await
                    .is_err_and(|e| e.is_lease_lost()),
                "a result from a lost epoch was accepted",
            )?;
            capable
                .result(&current.id, &result(None, "done"))
                .await
                .map_err(|e| e.to_string())?;
            next_lease_sees(&other, admin, &next, &memory, &seed()).await
        }
        "metadata_only_changes_are_not_applied" => {
            let lease = claim(&capable, &id).await?;
            let mut changes = edits();
            for item in &mut changes.items {
                item.content_base64 = None;
            }
            let mut failed = result(Some(changes), "error");
            failed.result["failure"] = json!({
                "stage": "context",
                "code": codes::CONTEXT_TOO_LARGE,
                "message": "changed context is over the cap",
            });
            capable
                .result(&lease.id, &failed)
                .await
                .map_err(|e| e.to_string())?;
            next_lease_sees(&other, admin, &next, &memory, &seed()).await
        }
        _ => Err("unknown case".into()),
    }
}

#[cfg(feature = "mock")]
impl LeaseContextAdmin for crate::mock::MockWorkServer {
    fn enqueue_with_context(&self, item: WorkItem) -> AdminFuture<'_> {
        Box::pin(async move {
            self.enqueue_with_context(item, crate::wire::now())
                .expect("valid fixture");
        })
    }
}
