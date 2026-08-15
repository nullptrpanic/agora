use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub(super) struct RelayOutcome {
    pub(super) bytes_sent: u64,
    pub(super) bytes_received: u64,
    pub(super) error: Option<io::Error>,
}

struct CountingIo<T> {
    inner: T,
    bytes_written: u64,
}

impl<T> CountingIo<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            bytes_written: 0,
        }
    }
}

impl<T> AsyncRead for CountingIo<T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(context, output)
    }
}

impl<T> AsyncWrite for CountingIo<T>
where
    T: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(context, bytes) {
            Poll::Ready(Ok(written)) => {
                self.bytes_written = self.bytes_written.saturating_add(written as u64);
                Poll::Ready(Ok(written))
            }
            result => result,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

pub(super) async fn relay_bidirectional<A, B>(client: A, upstream: B) -> RelayOutcome
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let mut client = CountingIo::new(client);
    let mut upstream = CountingIo::new(upstream);
    let error = tokio::io::copy_bidirectional(&mut client, &mut upstream)
        .await
        .err();
    RelayOutcome {
        bytes_sent: upstream.bytes_written,
        bytes_received: client.bytes_written,
        error,
    }
}
