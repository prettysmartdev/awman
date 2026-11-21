//! An in-process OCI distribution registry on an owned loopback listener.
//!
//! Supports what awman and the conformance checks use: `GET /v2/`, blob
//! `HEAD`/`GET`, monolithic and chunked uploads (`POST` + `PATCH` + `PUT
//! ?digest=`), manifest `HEAD`/`GET`/`PUT` by tag or digest, and tag lists.
//! Digests are verified on upload. State is in memory and dies with the value.

use crate::fixtures::{sha256_hex, OciImage};
use crate::http::{HttpServer, Request, Response};
use crate::TestSandbox;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    blobs: BTreeMap<String, Vec<u8>>,
    /// repository → reference (tag or digest) → (media type, bytes)
    manifests: BTreeMap<String, BTreeMap<String, (String, Vec<u8>)>>,
    uploads: BTreeMap<String, Vec<u8>>,
    requests: Vec<String>,
}

/// A fake registry; stops when dropped.
pub struct FakeRegistry {
    server: HttpServer,
    state: Arc<Mutex<State>>,
}

impl FakeRegistry {
    /// Start on a listener from `sandbox.ports()`.
    pub fn start(sandbox: &TestSandbox) -> FakeRegistry {
        let listener = sandbox.ports().tcp_listener();
        let state = Arc::new(Mutex::new(State::default()));
        let handler_state = state.clone();
        let counter = Arc::new(AtomicU64::new(0));
        let server = HttpServer::start(
            listener,
            Arc::new(move |req| handle(&handler_state, &counter, req)),
        );
        FakeRegistry { server, state }
    }

    /// `127.0.0.1:<port>`.
    pub fn addr(&self) -> SocketAddr {
        self.server.addr()
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr())
    }

    /// Store an image under `repository:tag` directly.
    pub fn insert_image(&self, repository: &str, tag: &str, image: &OciImage) {
        let mut state = self.state.lock().unwrap();
        for blob in image.layers.iter().chain([&image.config]) {
            state.blobs.insert(blob.digest.clone(), blob.bytes.clone());
        }
        let entry = state.manifests.entry(repository.to_string()).or_default();
        let value = (
            image.manifest.media_type.clone(),
            image.manifest.bytes.clone(),
        );
        entry.insert(tag.to_string(), value.clone());
        entry.insert(image.manifest.digest.clone(), value);
    }

    /// Whether a blob with this digest is stored.
    pub fn has_blob(&self, digest: &str) -> bool {
        self.state.lock().unwrap().blobs.contains_key(digest)
    }

    /// `"<METHOD> <path>"` of every request so far.
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }
}

/// Split `/v2/<name>/<kind>/<rest>` where `<name>` may contain slashes.
fn route(path: &str) -> Option<(String, &'static str, String)> {
    let rest = path.strip_prefix("/v2/")?;
    for kind in ["blobs/uploads", "blobs", "manifests", "tags"] {
        let marker = format!("/{kind}/");
        if let Some(at) = rest.rfind(&marker) {
            let name = rest[..at].to_string();
            let tail = rest[at + marker.len()..].to_string();
            let kind: &'static str = match kind {
                "blobs/uploads" => "uploads",
                "blobs" => "blobs",
                "manifests" => "manifests",
                _ => "tags",
            };
            return Some((name, kind, tail));
        }
        if kind == "blobs/uploads" && rest.ends_with("/blobs/uploads") {
            return Some((
                rest.trim_end_matches("/blobs/uploads").to_string(),
                "uploads",
                String::new(),
            ));
        }
    }
    None
}

fn error(status: u16, code: &str) -> Response {
    Response::new(status)
        .header("Content-Type", "application/json")
        .body(format!(
            r#"{{"errors":[{{"code":"{code}","message":"{code}"}}]}}"#
        ))
}

fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

fn handle(state: &Mutex<State>, counter: &AtomicU64, req: Request) -> Response {
    let mut s = state.lock().unwrap();
    s.requests.push(format!("{} {}", req.method, req.path));
    if req.path == "/v2/" || req.path == "/v2" {
        return Response::new(200)
            .header("Docker-Distribution-API-Version", "registry/2.0")
            .body("{}");
    }
    let Some((name, kind, tail)) = route(&req.path) else {
        return error(404, "NAME_UNKNOWN");
    };
    match (req.method.as_str(), kind) {
        ("HEAD" | "GET", "blobs") => match s.blobs.get(&tail) {
            Some(bytes) => Response::new(200)
                .header("Docker-Content-Digest", tail.clone())
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", bytes.len().to_string())
                .body(if req.method == "GET" {
                    bytes.clone()
                } else {
                    Vec::new()
                }),
            None => error(404, "BLOB_UNKNOWN"),
        },
        ("POST", "uploads") => {
            if let Some(digest) = req.query_param("digest") {
                if digest_of(&req.body) != digest {
                    return error(400, "DIGEST_INVALID");
                }
                s.blobs.insert(digest.clone(), req.body);
                return Response::new(201)
                    .header("Location", format!("/v2/{name}/blobs/{digest}"))
                    .header("Docker-Content-Digest", digest);
            }
            let id = format!("u{}", counter.fetch_add(1, Ordering::SeqCst));
            s.uploads.insert(id.clone(), req.body);
            Response::new(202)
                .header("Location", format!("/v2/{name}/blobs/uploads/{id}"))
                .header("Range", "0-0")
                .header("Docker-Upload-UUID", id)
        }
        ("PATCH", "uploads") => match s.uploads.get_mut(&tail) {
            Some(data) => {
                data.extend_from_slice(&req.body);
                let end = data.len().saturating_sub(1);
                Response::new(202)
                    .header("Location", format!("/v2/{name}/blobs/uploads/{tail}"))
                    .header("Range", format!("0-{end}"))
            }
            None => error(404, "BLOB_UPLOAD_UNKNOWN"),
        },
        ("PUT", "uploads") => {
            let Some(mut data) = s.uploads.remove(&tail) else {
                return error(404, "BLOB_UPLOAD_UNKNOWN");
            };
            data.extend_from_slice(&req.body);
            let Some(digest) = req.query_param("digest") else {
                return error(400, "DIGEST_INVALID");
            };
            if digest_of(&data) != digest {
                return error(400, "DIGEST_INVALID");
            }
            s.blobs.insert(digest.clone(), data);
            Response::new(201)
                .header("Location", format!("/v2/{name}/blobs/{digest}"))
                .header("Docker-Content-Digest", digest)
        }
        ("HEAD" | "GET", "manifests") => match s.manifests.get(&name).and_then(|m| m.get(&tail)) {
            Some((media_type, bytes)) => Response::new(200)
                .header("Content-Type", media_type.clone())
                .header("Docker-Content-Digest", digest_of(bytes))
                .header("Content-Length", bytes.len().to_string())
                .body(if req.method == "GET" {
                    bytes.clone()
                } else {
                    Vec::new()
                }),
            None => error(404, "MANIFEST_UNKNOWN"),
        },
        ("PUT", "manifests") => {
            let digest = digest_of(&req.body);
            if tail.starts_with("sha256:") && tail != digest {
                return error(400, "DIGEST_INVALID");
            }
            let media_type = req
                .header("content-type")
                .unwrap_or("application/vnd.oci.image.manifest.v1+json")
                .to_string();
            let entry = s.manifests.entry(name.clone()).or_default();
            entry.insert(tail.clone(), (media_type.clone(), req.body.clone()));
            entry.insert(digest.clone(), (media_type, req.body));
            Response::new(201)
                .header("Location", format!("/v2/{name}/manifests/{digest}"))
                .header("Docker-Content-Digest", digest)
        }
        ("GET", "tags") if tail == "list" => {
            let tags: Vec<String> = s
                .manifests
                .get(&name)
                .map(|m| {
                    m.keys()
                        .filter(|k| !k.starts_with("sha256:"))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if tags.is_empty() {
                return error(404, "NAME_UNKNOWN");
            }
            Response::new(200)
                .header("Content-Type", "application/json")
                .body(serde_json::json!({"name": name, "tags": tags}).to_string())
        }
        _ => error(405, "UNSUPPORTED"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_nested_names() {
        assert_eq!(
            route("/v2/a/b/manifests/latest"),
            Some(("a/b".into(), "manifests", "latest".into()))
        );
        assert_eq!(
            route("/v2/a/blobs/uploads/"),
            Some(("a".into(), "uploads", String::new()))
        );
        assert_eq!(
            route("/v2/a/blobs/uploads/u1"),
            Some(("a".into(), "uploads", "u1".into()))
        );
        assert_eq!(
            route("/v2/a/blobs/sha256:00"),
            Some(("a".into(), "blobs", "sha256:00".into()))
        );
        assert_eq!(
            route("/v2/a/tags/list"),
            Some(("a".into(), "tags", "list".into()))
        );
        assert_eq!(route("/other"), None);
    }
}
