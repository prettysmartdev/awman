//! HTTP over a local owner-authenticated socket or named pipe. No bearer key.
use super::HttpResponse;
use crate::engine::error::EngineError;
use std::path::Path;

pub async fn send(
    endpoint: &Path,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<HttpResponse, EngineError> {
    let failure = |e: String| EngineError::Other(format!("local daemon request: {e}"));
    #[cfg(unix)]
    let stream = tokio::net::UnixStream::connect(endpoint)
        .await
        .map_err(|e| failure(e.to_string()))?;
    #[cfg(windows)]
    let stream = tokio::net::windows::named_pipe::ClientOptions::new()
        .open(endpoint)
        .map_err(|e| failure(e.to_string()))?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(|e| failure(e.to_string()))?;
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    let result = async {
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost");
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|e| failure(e.to_string()))?
            .unwrap_or_default();
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let request = request
            .body(http_body_util::Full::new(hyper::body::Bytes::from(bytes)))
            .map_err(|e| failure(e.to_string()))?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| failure(e.to_string()))?;
        let status = response.status().as_u16();
        use http_body_util::BodyExt;
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| failure(e.to_string()))?
            .to_bytes();
        let body = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).to_string().into())
        };
        Ok(HttpResponse { status, body })
    }
    .await;
    task.abort();
    result
}

#[cfg(windows)]
pub type OwnerPipeFactory =
    fn(&Path, bool) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer>;
#[cfg(windows)]
static OWNER_PIPE: std::sync::OnceLock<OwnerPipeFactory> = std::sync::OnceLock::new();
/// The binary supplies the native ACL shim; library consumers fail closed without it.
#[cfg(windows)]
pub fn install_owner_pipe(factory: OwnerPipeFactory) {
    let _ = OWNER_PIPE.set(factory);
}
#[cfg(windows)]
pub fn owner_pipe(
    endpoint: &Path,
    first: bool,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, EngineError> {
    let factory = OWNER_PIPE
        .get()
        .ok_or_else(|| EngineError::Other("owner pipe ACL support is not installed".into()))?;
    factory(endpoint, first).map_err(|e| EngineError::Other(e.to_string()))
}
