//! Synchronous acquisition facade over cancellable asynchronous HTTP.
//!
//! Each source owns a current-thread executor. No detached transfer worker is
//! created: cancelling a connect, TLS handshake, header wait or body read drops
//! that request future and then the source's client/executor. The polling bound
//! is 20 ms, independent of the (potentially hour-long) acquisition budget.
use std::future::Future;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use super::retry::OperationControl;
use crate::engine::error::EngineError;

pub(super) enum Error {
    Control(EngineError),
    Http(reqwest::Error),
}
impl Error {
    pub(super) fn map_http(self, map: impl FnOnce(reqwest::Error) -> EngineError) -> EngineError {
        match self {
            Self::Control(e) => e,
            Self::Http(e) => map(e),
        }
    }
}

struct Executor(Option<tokio::runtime::Runtime>);
impl Drop for Executor {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            // Tokio cancels socket tasks immediately. The OS DNS resolver
            // can outlive a cancelled lookup; never wait without a bound for
            // that platform call while shutting down the acquisition.
            runtime.shutdown_timeout(Duration::from_millis(100));
        }
    }
}
#[derive(Clone)]
pub(super) struct Transport {
    executor: Arc<Executor>,
    control: OperationControl,
}
impl Transport {
    pub(super) fn new(control: OperationControl) -> Result<Self, EngineError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| EngineError::Network(format!("HTTP executor setup failed: {e}")))?;
        Ok(Self {
            executor: Arc::new(Executor(Some(runtime))),
            control,
        })
    }

    fn run<T>(&self, future: impl Future<Output = Result<T, reqwest::Error>>) -> Result<T, Error> {
        self.control.check().map_err(Error::Control)?;
        self.executor
            .0
            .as_ref()
            .expect("live HTTP executor")
            .block_on(async {
                tokio::pin!(future);
                loop {
                    tokio::select! {
                        biased;
                        _ = tokio::time::sleep(Duration::from_millis(20)) => {
                            self.control.check().map_err(Error::Control)?;
                        }
                        result = &mut future => {
                            self.control.check().map_err(Error::Control)?;
                            return result.map_err(Error::Http);
                        }
                    }
                }
            })
    }

    pub(super) fn send(&self, request: reqwest::RequestBuilder) -> Result<Response, Error> {
        let inner = self.run(async move { request.send().await })?;
        Ok(Response {
            inner,
            transport: self.clone(),
            pending: io::Cursor::new(Vec::new()),
        })
    }
}

pub(super) struct Response {
    inner: reqwest::Response,
    transport: Transport,
    pending: io::Cursor<Vec<u8>>,
}
impl Response {
    pub(super) fn status(&self) -> reqwest::StatusCode {
        self.inner.status()
    }
    pub(super) fn headers(&self) -> &reqwest::header::HeaderMap {
        self.inner.headers()
    }
    pub(super) fn content_length(&self) -> Option<u64> {
        self.inner.content_length()
    }
}
impl Read for Response {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.transport.control.check().map_err(io::Error::other)?;
        loop {
            let read = self.pending.read(buf)?;
            if read != 0 {
                return Ok(read);
            }
            match self.transport.run(self.inner.chunk()) {
                Ok(Some(bytes)) => self.pending = io::Cursor::new(bytes.to_vec()),
                Ok(None) => return Ok(0),
                Err(Error::Control(e)) => return Err(io::Error::other(e)),
                Err(Error::Http(e)) => {
                    return Err(io::Error::other(super::retry::error_chain(
                        &e.without_url(),
                    )))
                }
            }
        }
    }
}
