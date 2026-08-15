use super::super::relay::relay_bidirectional;
use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct ScriptedIo {
    reads: VecDeque<io::Result<Vec<u8>>>,
}

impl ScriptedIo {
    fn new(reads: impl IntoIterator<Item = io::Result<Vec<u8>>>) -> Self {
        Self {
            reads: reads.into_iter().collect(),
        }
    }
}

impl AsyncRead for ScriptedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.reads.pop_front() {
            Some(Ok(bytes)) => {
                output.put_slice(&bytes);
                Poll::Ready(Ok(()))
            }
            Some(Err(error)) => Poll::Ready(Err(error)),
            None => Poll::Ready(Ok(())),
        }
    }
}

impl AsyncWrite for ScriptedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn relay_preserves_transferred_bytes_when_a_direction_fails() {
    let client = ScriptedIo::new([Ok(b"request".to_vec())]);
    let upstream = ScriptedIo::new([
        Ok(b"response".to_vec()),
        Err(io::Error::new(io::ErrorKind::ConnectionReset, "reset")),
    ]);

    let outcome = relay_bidirectional(client, upstream).await;

    assert_eq!(outcome.bytes_sent, 7);
    assert_eq!(outcome.bytes_received, 8);
    assert_eq!(
        outcome.error.unwrap().kind(),
        io::ErrorKind::ConnectionReset
    );
}
