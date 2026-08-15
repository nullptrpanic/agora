use super::*;
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};

async fn test_proxy(response: Vec<u8>) -> (HttpProxy, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap().to_string().parse().unwrap();
    let (request_tx, request_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut buffer = [0_u8; 512];
            let read = stream.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let _ = request_tx.send(String::from_utf8(request).unwrap());
        if !response.is_empty() {
            stream.write_all(&response).await.unwrap();
        }
    });
    (proxy, request_rx)
}

#[tokio::test]
async fn tunnel_sends_connect_and_optional_basic_auth() {
    let (proxy, request) = test_proxy(b"HTTP/1.1 200 OK\r\n\r\n".to_vec()).await;
    let proxy = format!("user:password@{}", proxy.address())
        .parse()
        .unwrap();

    connect_tunnel(&proxy, "wss://[::1]:9443/callback")
        .await
        .unwrap();

    let request = request.await.unwrap();
    assert!(request.starts_with("CONNECT [::1]:9443 HTTP/1.1\r\n"));
    assert!(request.contains("Host: [::1]:9443\r\n"));
    assert!(request.contains("Proxy-Authorization: Basic dXNlcjpwYXNzd29yZA==\r\n"));

    let (proxy, request) = test_proxy(b"HTTP/1.1 204 No Content\r\n\r\n".to_vec()).await;
    connect_tunnel(&proxy, "ws://example.test/path")
        .await
        .unwrap();
    assert!(!request.await.unwrap().contains("Proxy-Authorization"));
}

#[tokio::test]
async fn tunnel_reports_invalid_targets_and_proxy_responses() {
    let proxy: HttpProxy = "127.0.0.1:1".parse().unwrap();
    assert!(connect_tunnel(&proxy, "not a URL").await.is_err());
    assert!(
        connect_tunnel(&proxy, "file:///tmp/socket")
            .await
            .unwrap_err()
            .to_string()
            .contains("host")
    );
    assert!(
        connect_tunnel(&proxy, "custom://example.test/path")
            .await
            .unwrap_err()
            .to_string()
            .contains("port")
    );

    for (response, expected) in [
        (Vec::new(), "closed"),
        (vec![b'a'; MAX_CONNECT_RESPONSE_SIZE + 1], "too large"),
        (b"HTTP/1.1 \xff\r\n\r\n".to_vec(), "UTF-8"),
        (b"invalid\r\n\r\n".to_vec(), "invalid"),
        (
            b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n".to_vec(),
            "status 407",
        ),
    ] {
        let (proxy, _) = test_proxy(response).await;
        let error = connect_tunnel(&proxy, "wss://example.test/socket")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "unexpected error: {error:#}"
        );
    }
}

#[tokio::test]
async fn tunnel_has_a_bounded_handshake_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap().to_string().parse().unwrap();
    let hold = std::sync::Arc::new(Notify::new());
    let server_hold = std::sync::Arc::clone(&hold);
    tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        server_hold.notified().await;
    });

    let error = connect_tunnel_with_timeout(
        &proxy,
        "wss://example.test/socket",
        std::time::Duration::from_millis(30),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("timed out"));
}
