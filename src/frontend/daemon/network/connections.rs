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
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn counts_streams_until_their_owner_drops_them() {
        let clients = Arc::new(AtomicUsize::new(0));
        let acceptor = CountingAcceptor {
            inner: axum_server::accept::DefaultAcceptor::new(),
            clients: clients.clone(),
            closed: tokio_util::sync::CancellationToken::new(),
        };
        let (one, _) = tokio::io::duplex(16);
        let (two, _) = tokio::io::duplex(16);
        let (one, ()) = acceptor.accept(one, ()).await.unwrap();
        let (two, ()) = acceptor.accept(two, ()).await.unwrap();
        assert_eq!(clients.load(Ordering::Relaxed), 2);
        drop(one);
        assert_eq!(clients.load(Ordering::Relaxed), 1);
        drop(two);
        assert_eq!(clients.load(Ordering::Relaxed), 0);
    }
    #[tokio::test]
    async fn listener_cancellation_ends_upgraded_streams() {
        use tokio::io::AsyncReadExt;
        let closed = tokio_util::sync::CancellationToken::new();
        let acceptor = CountingAcceptor {
            inner: axum_server::accept::DefaultAcceptor::new(),
            clients: Arc::new(AtomicUsize::new(0)),
            closed: closed.clone(),
        };
        let (stream, _peer) = tokio::io::duplex(16);
        let (mut upgraded, ()) = acceptor.accept(stream, ()).await.unwrap();
        let read = tokio::spawn(async move { upgraded.read(&mut [0]).await.unwrap() });
        tokio::task::yield_now().await;
        closed.cancel();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), read)
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}
