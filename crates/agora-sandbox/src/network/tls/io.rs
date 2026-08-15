use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub(in crate::network) struct PrefixedIo<T> {
    prefix: io::Cursor<Vec<u8>>,
    inner: T,
}

impl<T> PrefixedIo<T> {
    pub(in crate::network) fn new(prefix: Vec<u8>, inner: T) -> Self {
        Self {
            prefix: io::Cursor::new(prefix),
            inner,
        }
    }
}

impl<T> AsyncRead for PrefixedIo<T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let position = usize::try_from(self.prefix.position()).unwrap_or(usize::MAX);
        let prefix = self.prefix.get_ref();
        if position < prefix.len() && output.remaining() > 0 {
            let length = output.remaining().min(prefix.len() - position);
            output.put_slice(&prefix[position..position + length]);
            self.prefix.set_position((position + length) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(context, output)
    }
}

impl<T> AsyncWrite for PrefixedIo<T>
where
    T: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.inner).poll_write(context, bytes)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}
