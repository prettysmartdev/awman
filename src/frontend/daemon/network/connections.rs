//! Count stream lifetimes, including streams handed to WebSocket upgrades.
//! Hyper's connection count ends when an upgrade leaves the HTTP server.
use axum_server::accept::Accept;
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Clone)]
pub(super) struct CountingAcceptor<A> {
    pub inner: A,
    pub clients: Arc<AtomicUsize>,
    pub closed: tokio_util::sync::CancellationToken,
}
impl<A, I, S> Accept<I, S> for CountingAcceptor<A>
where
    A: Accept<I, S>,
    A::Future: Send + 'static,
{
    type Stream = CountedStream<A::Stream>;
    type Service = A::Service;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;
    fn accept(&self, stream: I, service: S) -> Self::Future {
        let accepting = self.inner.accept(stream, service);
        let clients = self.clients.clone();
        let cancelled = Box::pin(self.closed.clone().cancelled_owned());
        Box::pin(async move {
            let (inner, service) = accepting.await?;
            clients.fetch_add(1, Ordering::Relaxed);
            Ok((
                CountedStream {
                    inner,
                    clients,
                    cancelled,
                },
                service,
            ))
        })
    }
}
pub(super) struct CountedStream<I> {
    inner: I,
    clients: Arc<AtomicUsize>,
    cancelled: Pin<Box<tokio_util::sync::WaitForCancellationFutureOwned>>,
}
impl<I> Drop for CountedStream<I> {
    fn drop(&mut self) {
        self.clients.fetch_sub(1, Ordering::Relaxed);
    }
}
impl<I: AsyncRead + Unpin> AsyncRead for CountedStream<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl<I: AsyncWrite + Unpin> AsyncWrite for CountedStream<I> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
}
