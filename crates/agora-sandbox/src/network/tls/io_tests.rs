use super::io::PrefixedIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn prefixed_io_reads_the_prefix_before_the_underlying_stream() {
    let (stream, mut peer) = tokio::io::duplex(64);
    peer.write_all(b"socket").await.unwrap();
    let mut prefixed = PrefixedIo::new(b"prefix-".to_vec(), stream);
    let mut output = [0_u8; 13];

    prefixed.read_exact(&mut output).await.unwrap();

    assert_eq!(&output, b"prefix-socket");
}

#[tokio::test]
async fn prefixed_io_forwards_writes_without_modification() {
    let (stream, mut peer) = tokio::io::duplex(64);
    let mut prefixed = PrefixedIo::new(Vec::new(), stream);

    prefixed.write_all(b"request").await.unwrap();
    let mut output = [0_u8; 7];
    peer.read_exact(&mut output).await.unwrap();

    assert_eq!(&output, b"request");
}
