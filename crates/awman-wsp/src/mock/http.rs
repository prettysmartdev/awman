use super::*;
use axum::{
    body::to_bytes,
    extract::{
        ws::{Message, WebSocketUpgrade},
        FromRequestParts, Request, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use serde::de::DeserializeOwned;
use std::net::SocketAddr;
use tokio::{net::TcpListener, task::JoinHandle};

pub struct ServingMock {
    pub base_url: String,
    pub addr: SocketAddr,
    handle: JoinHandle<()>,
}
impl Drop for ServingMock {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
pub(super) async fn serve(server: Arc<MockWorkServer>) -> Result<ServingMock, std::io::Error> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let router = Router::new().fallback(any(dispatch)).with_state(server);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(ServingMock {
        base_url: format!("http://{addr}"),
        addr,
        handle,
    })
}
fn fail(e: ApiError) -> Response {
    (StatusCode::from_u16(e.status()).unwrap(), Json(e)).into_response()
}
fn read<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(bytes).map_err(|_| error("invalid_request"))
}
fn identity(
    server: &MockWorkServer,
    headers: &HeaderMap,
) -> Result<(String, Option<String>), ApiError> {
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or_else(|| error("unauthorized"))?;
    let s = server.inner.lock().unwrap();
    let (link, expires) = s
        .access
        .get(token)
        .filter(|(_, at)| *at > Utc::now())
        .ok_or_else(|| error("unauthorized"))?;
    let _ = expires;
    let l = s
        .links
        .get(link)
        .filter(|l| !l.revoked && !l.denied)
        .ok_or_else(|| error("unauthorized"))?;
    Ok((link.clone(), l.worker.clone()))
}
fn lease_auth(
    server: &MockWorkServer,
    id: &str,
    worker: &str,
    headers: &HeaderMap,
) -> Result<Lease, ApiError> {
    let mut s = server.inner.lock().unwrap();
    expire(&mut s, Utc::now());
    let l = s
        .leases
        .get(id)
        .filter(|l| l.worker_id == worker)
        .ok_or_else(|| error("lease_lost"))?;
    #[cfg(test)]
    let ignore_epoch = s.fault == Some(TestFault::IgnoreEpoch);
    #[cfg(not(test))]
    let ignore_epoch = false;
    if !ignore_epoch
        && headers
            .get("x-wsp-epoch")
            .and_then(|epoch| epoch.to_str().ok())
            .and_then(|epoch| epoch.parse::<u64>().ok())
            != Some(l.lease.epoch)
    {
        return Err(error("lease_lost"));
    }
    Ok(l.lease.clone())
}
async fn dispatch(State(server): State<Arc<MockWorkServer>>, request: Request) -> Response {
    #[cfg(test)]
    let fault = server.inner.lock().unwrap().fault;
    #[cfg(test)]
    let path = request.uri().path().to_owned();
    let response = match dispatch_inner(server, request).await {
        Ok(r) => r,
        Err(e) => fail(e),
    };
    #[cfg(test)]
    if let Some(TestFault::Wire(suffix)) = fault {
        if path.ends_with(suffix) || (suffix == "error" && !response.status().is_success()) {
            let (parts, body) = response.into_parts();
            let bytes = to_bytes(body, 4 * 1024 * 1024).await.unwrap();
            if bytes.is_empty() {
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({"badCamelCase": true})),
                )
                    .into_response();
            }
            if let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) {
                value
                    .as_object_mut()
                    .unwrap()
                    .insert("badCamelCase".into(), Value::Bool(true));
                return Response::from_parts(
                    parts,
                    axum::body::Body::from(serde_json::to_vec(&value).unwrap()),
                );
            }
            return Response::from_parts(parts, axum::body::Body::from(bytes));
        }
    }
    response
}
async fn dispatch_inner(
    server: Arc<MockWorkServer>,
    request: Request,
) -> Result<Response, ApiError> {
    let (mut parts, body) = request.into_parts();
    if parts
        .headers
        .get(VERSION_HEADER)
        .and_then(|h| h.to_str().ok())
        != Some(WSP_VERSION)
    {
        return Err(error("unsupported_version"));
    }
    let path = parts.uri.path().trim_matches('/').to_owned();
    let segments: Vec<_> = path.split('/').collect();
    let method = parts.method.clone();
    let query: HashMap<String, String> =
        reqwest::Url::parse(&format!("http://localhost{}", parts.uri))
            .map_err(|_| error("invalid_request"))?
            .query_pairs()
            .into_owned()
            .collect();
    let public = path == "v1/token"
        || path == "v1/link"
        || matches!(segments.as_slice(), ["v1", "link", _, "token"]);
    let identity = if public {
        None
    } else {
        Some(identity(&server, &parts.headers)?)
    };
    if method == "GET" && matches!(segments.as_slice(), ["v1", "leases", _, "terminal"]) {
        let worker = identity
            .as_ref()
            .and_then(|(_, w)| w.as_deref())
            .ok_or_else(|| error("worker_not_found"))?;
        let lease = lease_auth(&server, segments[2], worker, &parts.headers)?;
        if let Some(code) = server
            .inner
            .lock()
            .unwrap()
            .terminal_errors
            .get(&lease.item.id)
        {
            return Err(error(code));
        }
        if !lease.terminal {
            return Err(error("terminal_disabled"));
        }
        if !parts
            .headers
            .get("sec-websocket-protocol")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|s| {
                s.split(',')
                    .any(|s| s.trim() == crate::terminal::SUBPROTOCOL)
            })
        {
            return Err(error("unsupported_subprotocol"));
        }
        let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &())
            .await
            .map_err(|_| error("invalid_request"))?;
        let mut outbound = server
            .inner
            .lock()
            .unwrap()
            .leases
            .get(&lease.id)
            .ok_or_else(|| error("lease_lost"))?
            .outbound
            .subscribe();
        let headers = parts.headers.clone();
        return Ok(upgrade.protocols([crate::terminal::SUBPROTOCOL]).on_upgrade(move |mut socket| async move {
            let mut tick=tokio::time::interval(std::time::Duration::from_millis(25));
            loop {
                tokio::select! {
                    _=tick.tick()=> {
                        if super::http::identity(&server,&headers).is_err() || lease_auth(&server,&lease.id,&worker_id(&server,&lease.id),&headers).is_err() { break; }
                    }
                    frame=outbound.recv()=> {
                        let Ok(frame)=frame else { break; };
                        if socket.send(Message::Text(serde_json::to_string(&frame).unwrap().into())).await.is_err() { break; }
                    }
                    incoming=socket.recv()=> {
                        match incoming {
                            Some(Ok(Message::Text(text)))=> {
                                if super::http::identity(&server,&headers).is_err() || lease_auth(&server,&lease.id,&worker_id(&server,&lease.id),&headers).is_err() { break; }
                                let Ok(frame)=serde_json::from_str::<LeaseTerminalFrame>(&text) else { break; };
                                let mut s=server.inner.lock().unwrap(); expire(&mut s,Utc::now());
                                if s.leases.get(&lease.id).is_none_or(|l| l.lease.epoch != lease.epoch) { break; }
                                s.frames.entry(lease.id.clone()).or_default().push(frame);
                            }
                            Some(Ok(Message::Ping(bytes)))=> { if socket.send(Message::Pong(bytes)).await.is_err() { break; } }
                            Some(Ok(Message::Pong(_)))=> {}
                            _=>break,
                        }
                    }
                }
            }
        }).into_response());
    }
    let bytes = to_bytes(body, 4 * 1024 * 1024)
        .await
        .map_err(|_| error("invalid_request"))?;
    match (method.as_str(), segments.as_slice()) {
        ("POST", ["v1", "link"]) => Ok(Json(server.link()).into_response()),
        ("POST", ["v1", "link", id, "token"]) => {
            let s = server.inner.lock().unwrap();
            let l = s.links.get(*id).ok_or_else(|| error("not_found"))?;
            if l.expires_at <= Utc::now() {
                return Err(error("expired"));
            }
            if l.denied || l.revoked {
                return Err(error("access_denied"));
            }
            Ok(match &l.refresh {
                Some(token) => Json(LinkToken {
                    refresh_token: token.clone(),
                })
                .into_response(),
                None => (StatusCode::ACCEPTED, Json(serde_json::json!({}))).into_response(),
            })
        }
        ("POST", ["v1", "token"]) => {
            Ok(Json(server.token(&read::<TokenRequest>(&bytes)?.refresh_token)?).into_response())
        }
        ("POST", ["v1", "workers", "register"]) => {
            let (link, previous) = identity.unwrap();
            let request: RegisterRequest = read(&bytes)?;
            if request.capacity == 0 {
                return Err(error("invalid_capacity"));
            }
            for k in request.labels.keys() {
                validate_key(k).map_err(|_| error("invalid_label"))?;
            }
            let mut s = server.inner.lock().unwrap();
            // Any label assigned by this server belongs to the server, including on other links.
            if request.labels.keys().any(|k| s.server_owned.contains(k)) {
                return Err(error("server_label_refused"));
            }
            let labels = s.links[&link].labels.clone();
            let worker = previous.unwrap_or_else(|| {
                s.next_id += 1;
                format!("worker_{}", s.next_id)
            });
            s.links.get_mut(&link).unwrap().worker = Some(worker.clone());
            Ok(Json(register_worker(&mut s, worker, request, labels)).into_response())
        }
        ("GET", ["v1", "queue"]) => {
            let worker = identity
                .and_then(|(_, w)| w)
                .ok_or_else(|| error("worker_not_found"))?;
            let limit = query
                .get("limit")
                .map(|v| v.parse::<usize>().map_err(|_| error("invalid_pagination")))
                .transpose()?
                .unwrap_or(50);
            Ok(Json(server.queue_at(
                &worker,
                Utc::now(),
                limit,
                query.get("cursor").map(String::as_str),
            )?)
            .into_response())
        }
        ("POST", ["v1", "claims"]) => {
            let worker = identity
                .and_then(|(_, w)| w)
                .ok_or_else(|| error("worker_not_found"))?;
            let req: ClaimRequest = read(&bytes)?;
            let wait = query
                .get("wait_ms")
                .map(|v| v.parse::<u64>().map_err(|_| error("invalid_wait_ms")))
                .transpose()?
                .unwrap_or(0)
                .min(30_000);
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(wait);
            loop {
                super::http::identity(&server, &parts.headers)?;
                let changed = server.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Some(lease) = server.claim(
                    &worker,
                    req.item_id.as_deref(),
                    Utc::now(),
                    Duration::seconds(3),
                )? {
                    return Ok((StatusCode::CREATED, Json(ClaimResponse { lease })).into_response());
                }
                if tokio::time::Instant::now() >= deadline {
                    return Ok(StatusCode::NO_CONTENT.into_response());
                }
                tokio::select! { _=changed=>{}, _=tokio::time::sleep_until(deadline)=>{}, _=tokio::time::sleep(std::time::Duration::from_millis(25))=>{} }
            }
        }
        ("POST", ["v1", "leases", id, operation]) => {
            let worker = identity
                .and_then(|(_, w)| w)
                .ok_or_else(|| error("worker_not_found"))?;
            let lease = lease_auth(&server, id, &worker, &parts.headers)?;
            let now = Utc::now();
            match *operation {
                "heartbeat" => Ok(Json(server.heartbeat(
                    id,
                    lease.epoch,
                    now,
                    Duration::seconds(3),
                    &read(&bytes)?,
                )?)
                .into_response()),
                "events" => {
                    let response = Json(EventsResponse {
                        accepted: server.events(
                            id,
                            lease.epoch,
                            now,
                            read::<EventBatch<Value>>(&bytes)?.events,
                        )?,
                    })
                    .into_response();
                    #[cfg(test)]
                    if server.inner.lock().unwrap().fault == Some(TestFault::EmptyEvents) {
                        return Ok(StatusCode::NO_CONTENT.into_response());
                    }
                    Ok(response)
                }
                "result" => {
                    server.result(id, lease.epoch, now, read(&bytes)?)?;
                    Ok(StatusCode::NO_CONTENT.into_response())
                }
                "release" => {
                    server.release(id, lease.epoch, now, read(&bytes)?)?;
                    Ok(StatusCode::NO_CONTENT.into_response())
                }
                "credentials" => Ok(Json(lease.item.repo.credentials.unwrap_or(Credentials {
                    kind: CredentialKind::None,
                    token: None,
                    username: None,
                    expires_at: None,
                }))
                .into_response()),
                _ => Err(error("not_found")),
            }
        }
        _ => Err(error("not_found")),
    }
}
fn worker_id(server: &MockWorkServer, id: &str) -> String {
    server.worker_id_for_lease(id).unwrap_or_default()
}
