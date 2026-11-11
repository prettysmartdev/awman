//! Reusable HTTP conformance probes. The admin adapter provisions fixtures out of band.
use crate::*;
use serde_json::{json, Value};
use std::{collections::BTreeMap, future::Future, pin::Pin};
pub type AdminFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
/// Object-safe async administration; implementations may call a server's own test API.
pub trait ConformanceAdmin: Sync {
    fn enqueue(&self, item: WorkItem) -> AdminFuture<'_>;
    /// Set fixture age explicitly, including equal timestamps for preference ties.
    fn enqueue_at(&self, item: WorkItem, at: chrono::DateTime<chrono::Utc>) -> AdminFuture<'_>;
    /// Expire this lease so the item can be claimed again. A server that
    /// reuses lease IDs keeps this ID for the next claim, with a higher epoch;
    /// a server that never reuses IDs issues a new one. Either way every call
    /// on the old lease must then answer `409 lease_lost`.
    fn expire_lease_reusing_id<'a>(&'a self, id: &'a str) -> AdminFuture<'a>;
    /// Observation hook required for servers returning empty event acknowledgements.
    /// Return the stored event count to verify deduplication without {accepted}.
    fn event_count<'a>(
        &'a self,
        _id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<usize>> + Send + 'a>> {
        Box::pin(async { None })
    }
    fn approve_link<'a>(&'a self, id: &'a str, labels: BTreeMap<String, String>)
        -> AdminFuture<'a>;
    fn expire_leases(&self) -> AdminFuture<'_>;
    fn request_cancel<'a>(&'a self, id: &'a str) -> AdminFuture<'a>;
}
#[derive(Debug, Default)]
pub struct Report {
    pub passed: Vec<&'static str>,
    pub failed: Vec<(&'static str, String)>,
}
impl Report {
    pub fn is_success(&self) -> bool {
        self.failed.is_empty()
    }
}
pub fn cases() -> &'static [&'static str] {
    &[
        "linking_and_register",
        "affinity_in",
        "affinity_not_in",
        "affinity_exists",
        "affinity_does_not_exist",
        "priority_age_preference",
        "claim_race",
        "expiry_fencing",
        "stale_epoch_is_fenced",
        "idempotent_events",
        "result_once",
        "wire_conventions",
        "long_poll",
        "release",
        "credentials",
        "heartbeat_cancel",
    ]
}
/// Test fixture shared by the conformance runner and server implementers.
pub fn work_item(id: &str) -> WorkItem {
    WorkItem {
        id: id.into(),
        title: id.into(),
        summary: Some("conformance fixture".into()),
        kind: WorkKind::Prompt,
        body: "test".into(),
        source: None,
        repo: Repo {
            url: "https://example.invalid/test.git".into(),
            base_ref: "main".into(),
            branch: format!("wsp/{id}"),
            merge_refs: vec![],
            credentials: None,
        },
        exec: Exec {
            dynamic: false,
            leader: None,
            workflow_toml: None,
            agent: Some("test".into()),
            model: None,
            read_only: false,
            overlays: vec![],
            interaction: None,
            yolo: true,
            timeout_ms: Some(1000),
        },
        push: None,
        outputs: None,
        labels: BTreeMap::new(),
        priority: 0,
        affinity: Affinity::default(),
    }
}
async fn linked(base: &str, admin: &dyn ConformanceAdmin, name: &str) -> Result<Client, Error> {
    let linker = Client::new(base, String::new())?;
    let link = linker.start_link().await?;
    if linker.poll_link(&link.link_id).await?.is_some() {
        return Err(Error::InvalidResponse);
    }
    admin
        .approve_link(
            &link.link_id,
            BTreeMap::from([("owner".into(), name.into())]),
        )
        .await;
    let token = linker
        .poll_link(&link.link_id)
        .await?
        .ok_or(Error::InvalidResponse)?;
    let client = Client::new(base, token.refresh_token)?;
    let reg = client
        .register(&RegisterRequest {
            name: name.into(),
            labels: BTreeMap::from([
                ("pool".into(), "test".into()),
                ("awman/os".into(), "linux".into()),
            ]),
            capacity: 100,
            features: features::ALL.iter().map(|s| s.to_string()).collect(),
        })
        .await?;
    if reg.server_labels.get("owner").map(String::as_str) != Some(name)
        || reg.heartbeat_ms == 0
        || reg.poll_ms == 0
    {
        return Err(Error::InvalidResponse);
    }
    Ok(client)
}
/// Runs against an isolated test queue. `base_url` is the only transport configuration.
/// Fixture IDs use a unique prefix so a runner may be repeated on a persistent server.
pub async fn run(base_url: &str, admin: &dyn ConformanceAdmin) -> Report {
    let mut report = Report::default();
    let prefix = format!(
        "wsp_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let first = match linked(base_url, admin, &format!("{prefix}_a")).await {
        Ok(c) => {
            report.passed.push(cases()[0]);
            c
        }
        Err(e) => {
            report.failed.push((cases()[0], e.to_string()));
            return report;
        }
    };
    let second = match linked(base_url, admin, &format!("{prefix}_b")).await {
        Ok(c) => c,
        Err(e) => {
            report.failed.push(("second_worker", e.to_string()));
            return report;
        }
    };
    for &case in &cases()[1..] {
        match probe(case, &prefix, &first, &second, admin).await {
            Ok(()) => report.passed.push(case),
            Err(e) => report.failed.push((case, e.to_string())),
        }
    }
    report
}

/// Run one named probe with fresh workers and fixture identifiers. This lets
/// server implementers test individual protocol failures independently.
pub async fn run_case(
    base_url: &str,
    admin: &dyn ConformanceAdmin,
    case: &str,
) -> Result<(), Error> {
    if !cases().contains(&case) {
        return Err(Error::InvalidResponse);
    }
    let prefix = format!(
        "wsp_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let a = linked(base_url, admin, &format!("{prefix}_a")).await?;
    if case == "linking_and_register" {
        return Ok(());
    }
    let b = linked(base_url, admin, &format!("{prefix}_b")).await?;
    probe(case, &prefix, &a, &b, admin).await
}
fn check(ok: bool) -> Result<(), Error> {
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidResponse)
    }
}
fn heartbeat() -> HeartbeatRequest {
    HeartbeatRequest {
        state: HeartbeatState::Running,
        step: None,
        progress: None,
    }
}
fn result() -> LeaseResult<Value> {
    LeaseResult {
        result: json!({"outcome":"done","exit_code":0,"final_message":"complete"}),
        outputs: Some(BTreeMap::new()),
        outputs_mounted: true,
        duration_ms: 1,
    }
}
async fn probe(
    case: &str,
    prefix: &str,
    a: &Client,
    b: &Client,
    admin: &dyn ConformanceAdmin,
) -> Result<(), Error> {
    let id = format!("{prefix}_{case}");
    if case.starts_with("affinity_") {
        let op = match case {
            "affinity_in" => MatchOp::In,
            "affinity_not_in" => MatchOp::NotIn,
            "affinity_exists" => MatchOp::Exists,
            _ => MatchOp::DoesNotExist,
        };
        let make_match = |eligible: bool| Match {
            key: match (op, eligible) {
                (MatchOp::Exists, false) | (MatchOp::DoesNotExist, true) => "missing",
                _ => "pool",
            }
            .into(),
            op,
            values: if matches!(op, MatchOp::In | MatchOp::NotIn) {
                let matching_value = matches!(op, MatchOp::In) == eligible;
                vec![if matching_value { "test" } else { "different" }.into()]
            } else {
                vec![]
            },
        };
        let mut yes = work_item(&id);
        yes.affinity.required = vec![make_match(true)];
        admin.enqueue(yes).await;
        let no_id = format!("{id}_no");
        let mut no = work_item(&no_id);
        no.affinity.required = vec![make_match(false)];
        admin.enqueue(no).await;
        // A matching first condition cannot mask another required condition.
        let conjunction_id = format!("{id}_conjunction");
        let mut conjunction = work_item(&conjunction_id);
        conjunction.affinity.required = vec![
            make_match(true),
            Match {
                key: "pool".into(),
                op: MatchOp::In,
                values: vec!["different".into()],
            },
        ];
        admin.enqueue(conjunction).await;
        let queue = a.queue(500, None).await?;
        check(
            queue.items.iter().any(|i| i.id == id)
                && !queue
                    .items
                    .iter()
                    .any(|i| i.id == no_id || i.id == conjunction_id),
        )?;
        for refused in [&no_id, &conjunction_id] {
            check(
                a.claim(Some(refused), 0)
                    .await
                    .is_err_and(|e| e.is_ineligible()),
            )?;
        }
        if op == MatchOp::NotIn {
            let missing_id = format!("{id}_missing");
            let mut missing = work_item(&missing_id);
            missing.affinity.required = vec![Match {
                key: "missing".into(),
                op,
                values: vec!["test".into()],
            }];
            admin.enqueue(missing).await;
            check(
                a.queue(500, None)
                    .await?
                    .items
                    .iter()
                    .any(|i| i.id == missing_id),
            )?;
            let lease = a
                .claim(Some(&missing_id), 0)
                .await?
                .ok_or(Error::InvalidResponse)?;
            a.result(&lease.id, &result()).await?;
        }
        let lease = a.claim(Some(&id), 0).await?.ok_or(Error::InvalidResponse)?;
        a.result(&lease.id, &result()).await?;
        return Ok(());
    }
    if case == "priority_age_preference" {
        let old = chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .map_err(|_| Error::InvalidResponse)?
            .with_timezone(&chrono::Utc);
        let young = old + chrono::Duration::seconds(1);
        // Priority outranks age; age outranks preference. Equal-aged items
        // use preference. Plain sorts first lexically if preference is ignored.
        for (suffix, priority, weight, at) in [
            ("older", 0, 0, old),
            ("priority", 1, 0, young),
            ("z_preferred", 0, 100, young),
            ("a_plain", 0, 0, young),
        ] {
            let mut item = work_item(&format!("{id}_{suffix}"));
            item.priority = priority;
            if weight > 0 {
                item.affinity.preferred = vec![Preferred {
                    weight,
                    matches: vec![Match {
                        key: "pool".into(),
                        op: MatchOp::Exists,
                        values: vec![],
                    }],
                }];
            }
            admin.enqueue_at(item, at).await;
        }
        let queue = a.queue(500, None).await?;
        let ids: Vec<_> = queue
            .items
            .iter()
            .filter(|i| i.id.starts_with(&id))
            .map(|i| i.id.clone())
            .collect();
        let expected: Vec<_> = ["priority", "older", "z_preferred", "a_plain"]
            .map(|suffix| format!("{id}_{suffix}"))
            .into_iter()
            .collect();
        check(ids == expected)?;
        for item in ids {
            let l = a
                .claim(Some(&item), 0)
                .await?
                .ok_or(Error::InvalidResponse)?;
            a.result(&l.id, &result()).await?;
        }
        return Ok(());
    }
    if case == "wire_conventions" {
        return wire_probe(&id, a, admin).await;
    }
    admin.enqueue(work_item(&id)).await;
    if case == "claim_race" {
        let (x, y) = tokio::join!(a.claim(Some(&id), 0), b.claim(Some(&id), 0));
        return match (x, y) {
            (Ok(Some(l)), Err(e)) if e.is_taken() => a.result(&l.id, &result()).await,
            (Err(e), Ok(Some(l))) if e.is_taken() => b.result(&l.id, &result()).await,
            _ => Err(Error::InvalidResponse),
        };
    }
    let lease = a.claim(Some(&id), 0).await?.ok_or(Error::InvalidResponse)?;
    match case {
        "expiry_fencing" => {
            admin.expire_leases().await;
            check(
                a.heartbeat(&lease.id, &heartbeat())
                    .await
                    .is_err_and(|e| e.is_lease_lost()),
            )?;
            check(
                a.result(&lease.id, &result())
                    .await
                    .is_err_and(|e| e.is_lease_lost()),
            )?;
            let next = b.claim(Some(&id), 0).await?.ok_or(Error::InvalidResponse)?;
            check(next.epoch > lease.epoch)?;
            b.result(&next.id, &result()).await
        }
        "stale_epoch_is_fenced" => {
            let old = a.with_lease_epoch(&lease.id, lease.epoch);
            admin.expire_lease_reusing_id(&lease.id).await;
            let next = a.claim(Some(&id), 0).await?.ok_or(Error::InvalidResponse)?;
            // A reused ID must carry a higher epoch; a fresh ID fences by itself.
            check(next.id != lease.id || next.epoch > lease.epoch)?;
            check(
                old.heartbeat(&lease.id, &heartbeat())
                    .await
                    .is_err_and(|e| e.is_lease_lost()),
            )?;
            check(
                old.events(&lease.id, vec![json!({"attempt":1,"sequence":1})])
                    .await
                    .is_err_and(|e| e.is_lease_lost()),
            )?;
            check(
                old.credentials(&lease.id)
                    .await
                    .is_err_and(|e| e.is_lease_lost()),
            )?;
            check(
                old.result(&lease.id, &result())
                    .await
                    .is_err_and(|e| e.is_lease_lost()),
            )?;
            check(
                old.release(
                    &lease.id,
                    &ReleaseRequest::<Value> {
                        reason: ReleaseReason::Error,
                        message: None,
                        result: None,
                    },
                )
                .await
                .is_err_and(|e| e.is_lease_lost()),
            )?;
            a.result(&next.id, &result()).await
        }
        "idempotent_events" => {
            let event = json!({"attempt":1,"sequence":1,"type":"workflow_state","data":{}});
            let other_attempt = json!({"attempt":2,"sequence":1,"type":"workflow_state","data":{}});
            let other_sequence =
                json!({"attempt":1,"sequence":2,"type":"workflow_state","data":{}});
            let batch = json!({"events":[event.clone(), event, other_attempt, other_sequence]});
            let path = format!("v1/leases/{}/events", lease.id);
            let first = raw(
                a,
                reqwest::Method::POST,
                &path,
                Some(&batch),
                Some(lease.epoch),
                true,
            )
            .await?;
            check(first.0.is_success())?;
            let has_first_count = first.1.is_some();
            if let Some(body) = first.1 {
                check(body["accepted"].as_u64() == Some(3))?;
            }
            let again = raw(
                a,
                reqwest::Method::POST,
                &path,
                Some(&batch),
                Some(lease.epoch),
                true,
            )
            .await?;
            check(again.0.is_success())?;
            let has_second_count = again.1.is_some();
            if let Some(body) = again.1 {
                check(body["accepted"].as_u64() == Some(0))?;
            }
            match admin.event_count(&lease.id).await {
                Some(count) => check(count == 3)?,
                // Without a count in responses, admin inspection is needed to
                // prove idempotence rather than merely accepting two batches.
                None => check(has_first_count && has_second_count)?,
            }
            a.result(&lease.id, &result()).await
        }
        "result_once" => {
            a.result(&lease.id, &result()).await?;
            check(
                a.result(&lease.id, &result())
                    .await
                    .is_err_and(|e| e.is_lease_lost()),
            )
        }
        "release" => {
            a.release(
                &lease.id,
                &ReleaseRequest {
                    reason: ReleaseReason::Ineligible,
                    message: Some("test".into()),
                    result: Some(json!({"outcome":"paused"})),
                },
            )
            .await?;
            check(
                a.claim(Some(&id), 0)
                    .await
                    .is_err_and(|e| e.is_ineligible()),
            )?;
            let l = b.claim(Some(&id), 0).await?.ok_or(Error::InvalidResponse)?;
            b.result(&l.id, &result()).await
        }
        "credentials" => {
            a.credentials(&lease.id).await?;
            a.result(&lease.id, &result()).await?;
            check(
                a.credentials(&lease.id)
                    .await
                    .is_err_and(|e| e.is_lease_lost()),
            )
        }
        "heartbeat_cancel" => {
            admin.request_cancel(&lease.id).await;
            check(a.heartbeat(&lease.id, &heartbeat()).await?.cancel)?;
            a.result(&lease.id, &result()).await
        }
        "long_poll" => {
            // A specific claim is immediate even with a wait budget.
            a.result(&lease.id, &result()).await?;
            let start = tokio::time::Instant::now();
            check(a.claim(None, 50).await?.is_none())?;
            check(start.elapsed() >= std::time::Duration::from_millis(40))?;
            let arriving_id = format!("{id}_arriving");
            let (claim, ()) = tokio::join!(a.claim(None, 2000), async {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                admin.enqueue(work_item(&arriving_id)).await;
            });
            let arriving = claim?.ok_or(Error::InvalidResponse)?;
            check(arriving.item.id == arriving_id)?;
            a.result(&arriving.id, &result()).await
        }
        _ => Err(Error::InvalidResponse),
    }
}
#[cfg(feature = "mock")]
impl ConformanceAdmin for crate::mock::MockWorkServer {
    fn enqueue_at(&self, item: WorkItem, at: chrono::DateTime<chrono::Utc>) -> AdminFuture<'_> {
        Box::pin(async move {
            self.enqueue(item, at).expect("valid fixture");
        })
    }
    fn expire_lease_reusing_id<'a>(&'a self, id: &'a str) -> AdminFuture<'a> {
        Box::pin(async move {
            self.expire_lease_reusing_id(id);
        })
    }
    fn event_count<'a>(
        &'a self,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<usize>> + Send + 'a>> {
        Box::pin(async move { self.event_count(id) })
    }
    fn enqueue(&self, item: WorkItem) -> AdminFuture<'_> {
        Box::pin(async move {
            self.enqueue(item, crate::wire::now())
                .expect("valid fixture");
        })
    }
    fn approve_link<'a>(
        &'a self,
        id: &'a str,
        labels: BTreeMap<String, String>,
    ) -> AdminFuture<'a> {
        Box::pin(async move {
            self.approve_link(id, labels);
        })
    }
    fn expire_leases(&self) -> AdminFuture<'_> {
        Box::pin(async move {
            self.expire_leases();
        })
    }
    fn request_cancel<'a>(&'a self, id: &'a str) -> AdminFuture<'a> {
        Box::pin(async move {
            self.request_cancel(id, "conformance cancellation");
        })
    }
}

/// Walk protocol fields while keeping caller-owned maps and payloads opaque.
/// Used by raw HTTP probes and serialization tests, before serde can discard fields.
pub fn walk(v: &Value, f: &mut impl FnMut(&str, &Value)) {
    match v {
        Value::Object(map) => {
            for (k, v) in map {
                f(k, v);
                if !matches!(
                    k.as_str(),
                    "labels" | "server_labels" | "outputs" | "progress" | "data"
                ) {
                    walk(v, f);
                }
            }
        }
        Value::Array(values) => {
            for v in values {
                walk(v, f);
            }
        }
        _ => {}
    }
}
fn snake(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
fn validate_message(value: &Value) -> Result<(), Error> {
    let mut valid = true;
    walk(value, &mut |key, value| {
        valid &= snake(key);
        if key.ends_with("_ms") {
            valid &= value.is_null() || value.is_u64();
        }
        if key.ends_with("_at") {
            valid &= value.is_null()
                || value
                    .as_str()
                    .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok());
        }
        if matches!(
            key,
            "kind" | "state" | "reason" | "interaction" | "outcome" | "type" | "code"
        ) {
            valid &= value.is_null() || value.as_str().is_some_and(snake);
        }
        if key == "op" {
            valid &= value
                .as_str()
                .is_some_and(|s| matches!(s, "In" | "NotIn" | "Exists" | "DoesNotExist"));
        }
        if key == "features" {
            valid &= value
                .as_array()
                .is_some_and(|a| a.iter().all(|v| v.as_str().is_some_and(snake)));
        }
        if matches!(key, "labels" | "server_labels") {
            valid &= value.as_object().is_some_and(|m| {
                m.iter()
                    .all(|(k, v)| labels::validate_key(k).is_ok() && v.is_string())
            });
        }
    });
    check(valid)
}
// No policy or server routing: this is an outbound conformance transport. Inspect
// bytes before typed decoding, including unknown fields and empty acknowledgements.
async fn raw(
    client: &Client,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
    epoch: Option<u64>,
    authenticated: bool,
) -> Result<(reqwest::StatusCode, Option<Value>), Error> {
    if let Some(body) = body {
        validate_message(body)?;
    }
    let mut request = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(40))
        .build()?
        .request(method, client.url(path)?)
        .header(VERSION_HEADER, WSP_VERSION);
    if authenticated {
        request = request.bearer_auth(client.token().await?);
    }
    if let Some(epoch) = epoch {
        request = request.header("x-wsp-epoch", epoch);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send().await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    let value = if bytes.is_empty() {
        None
    } else {
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse)?;
        validate_message(&value)?;
        Some(value)
    };
    if !status.is_success() {
        let error = value.as_ref().ok_or(Error::InvalidResponse)?;
        check(error["error"].is_string() && error["code"].as_str().is_some_and(snake))?;
    }
    Ok((status, value))
}
fn decoded<T: serde::de::DeserializeOwned>(
    response: (reqwest::StatusCode, Option<Value>),
) -> Result<T, Error> {
    check(response.0.is_success())?;
    serde_json::from_value(response.1.ok_or(Error::InvalidResponse)?)
        .map_err(|_| Error::InvalidResponse)
}
async fn wire_probe(id: &str, base: &Client, admin: &dyn ConformanceAdmin) -> Result<(), Error> {
    const GET: reqwest::Method = reqwest::Method::GET;
    const POST: reqwest::Method = reqwest::Method::POST;
    let link: LinkStart = decoded(raw(base, POST, "v1/link", None, None, false).await?)?;
    let poll_path = format!("v1/link/{}/token", link.link_id);
    let pending = raw(base, POST, &poll_path, None, None, false).await?;
    check(matches!(pending.0.as_u16(), 202 | 204))?;
    admin
        .approve_link(&link.link_id, BTreeMap::from([("owner".into(), id.into())]))
        .await;
    let token: LinkToken = decoded(raw(base, POST, &poll_path, None, None, false).await?)?;
    let client = Client::new(base.url("")?.as_str(), token.refresh_token.clone())?;
    let access: AccessToken = decoded(
        raw(
            &client,
            POST,
            "v1/token",
            Some(&json!({"refresh_token": token.refresh_token})),
            None,
            false,
        )
        .await?,
    )?;
    check(!access.access_token.is_empty())?;
    let reg = raw(
        &client,
        POST,
        "v1/workers/register",
        Some(&json!({
            "name": id, "labels": {"awman/os": "linux", "pool": "test"},
            "capacity": 10, "features": features::ALL,
        })),
        None,
        true,
    )
    .await?;
    let reg_body = reg.1.as_ref().ok_or(Error::InvalidResponse)?;
    check(reg_body["heartbeat_ms"].is_u64() && reg_body["poll_ms"].is_u64())?;
    let _: RegisterResponse = decoded(reg)?;
    admin.enqueue(work_item(id)).await;
    let queue = raw(&client, GET, "v1/queue?limit=500", None, None, true).await?;
    let list = queue.1.as_ref().ok_or(Error::InvalidResponse)?;
    check(
        list["items"].is_array()
            && list
                .get("next_cursor")
                .is_some_and(|v| v.is_null() || v.is_string()),
    )?;
    let _: List<QueueItem> = decoded(queue)?;
    let claim = raw(
        &client,
        POST,
        "v1/claims?wait_ms=0",
        Some(&json!({"item_id": id})),
        None,
        true,
    )
    .await?;
    check(claim.0 == reqwest::StatusCode::CREATED)?;
    let lease: ClaimResponse = decoded(claim)?;
    let path = format!("v1/leases/{}", lease.lease.id);
    let epoch = Some(lease.lease.epoch);
    let _: HeartbeatResponse = decoded(
        raw(
            &client,
            POST,
            &format!("{path}/heartbeat"),
            Some(&json!({"state":"awaiting_input","step":"test","progress":null})),
            epoch,
            true,
        )
        .await?,
    )?;
    let events = raw(
        &client,
        POST,
        &format!("{path}/events"),
        Some(&json!({"events":[{"attempt":1,"sequence":1,"type":"workflow_state","data":{}}]})),
        epoch,
        true,
    )
    .await?;
    check(events.0.is_success())?;
    if let Some(body) = events.1 {
        check(body["accepted"].is_u64())?;
    }
    let _: Credentials = decoded(
        raw(
            &client,
            POST,
            &format!("{path}/credentials"),
            None,
            epoch,
            true,
        )
        .await?,
    )?;
    let payload = serde_json::to_value(result()).map_err(|_| Error::InvalidResponse)?;
    check(
        raw(
            &client,
            POST,
            &format!("{path}/result"),
            Some(&payload),
            epoch,
            true,
        )
        .await?
        .0
        .is_success(),
    )?;
    // Ended-lease calls must use the same structured error shape.
    let error = raw(
        &client,
        POST,
        &format!("{path}/heartbeat"),
        Some(&json!({"state":"running"})),
        epoch,
        true,
    )
    .await?;
    check(
        error.0 == reqwest::StatusCode::CONFLICT
            && error.1.as_ref().is_some_and(|v| v["code"] == "lease_lost"),
    )?;
    let release_id = format!("{id}_release");
    admin.enqueue(work_item(&release_id)).await;
    let lease: ClaimResponse = decoded(
        raw(
            &client,
            POST,
            "v1/claims?wait_ms=0",
            Some(&json!({"item_id": release_id})),
            None,
            true,
        )
        .await?,
    )?;
    let response = raw(
        &client,
        POST,
        &format!("v1/leases/{}/release", lease.lease.id),
        Some(&json!({"reason":"user_declined","message":"test"})),
        Some(lease.lease.epoch),
        true,
    )
    .await?;
    check(response.0.is_success())?;
    // Release requeues the fixture; finish it so subsequent automatic-claim probes
    // still run against an empty eligible queue.
    let lease = client
        .claim(Some(&release_id), 0)
        .await?
        .ok_or(Error::InvalidResponse)?;
    client.result(&lease.id, &result()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_walker_rejects_units_timestamps_enums_and_invalid_labels() {
        for value in [
            json!({"heartbeat_ms":1.5}),
            json!({"heartbeat_ms":"100"}),
            json!({"expires_at":123}),
            json!({"expires_at":"yesterday"}),
            json!({"kind":"Workflow"}),
            json!({"state":"awaitingInput"}),
            json!({"labels":{"UPPER":"value"}}),
            json!({"server_labels":{"bare.dotted":"value"}}),
            json!({"badCamelCase":true}),
        ] {
            assert!(validate_message(&value).is_err(), "{value}");
        }
        assert!(validate_message(&json!({"labels":{"awman/os":"linux"}})).is_ok());
    }
}
