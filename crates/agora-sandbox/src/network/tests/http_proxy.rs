use super::super::http_proxy::HttpProxyConnector;
use crate::callback::{BasicAuth, HttpProxy};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn proxy(address: String) -> HttpProxy {
    HttpProxy {
        address,
        basic_auth: None,
    }
}

async fn responding_proxy(response: Vec<u8>) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0_u8; 4096];
        let read = stream.read(&mut request).await.unwrap();
        request.truncate(read);
        if !response.is_empty() {
            stream.write_all(&response).await.unwrap();
        }
        request
    });
    (address, server)
}

#[tokio::test]
async fn http_proxy_rejects_invalid_configuration_before_connecting() {
    let destination = SocketAddr::from((Ipv4Addr::LOCALHOST, 443));
    let Err(error) = HttpProxyConnector::connect(&proxy("  ".to_string()), destination).await
    else {
        panic!("an empty proxy address must be rejected");
    };
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    let proxy = HttpProxy {
        address: "127.0.0.1:1".to_string(),
        basic_auth: Some(BasicAuth {
            username: "invalid:user".to_string(),
            password: "secret".to_string(),
        }),
    };
    let Err(error) = HttpProxyConnector::connect(&proxy, destination).await else {
        panic!("an invalid Basic Auth username must be rejected");
    };
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

#[tokio::test]
async fn http_proxy_reports_closed_malformed_and_oversized_responses() {
    let destination = SocketAddr::from((Ipv4Addr::LOCALHOST, 443));
    let mut oversized = b"HTTP/1.1 200 OK\r\nX-Test: ".to_vec();
    oversized.resize(16 * 1024, b'x');
    for (response, expected) in [
        (Vec::new(), io::ErrorKind::UnexpectedEof),
        (b"not http\r\n\r\n".to_vec(), io::ErrorKind::InvalidData),
        (b"HTTP/1.1 \r\n\r\n".to_vec(), io::ErrorKind::InvalidData),
        (oversized, io::ErrorKind::InvalidData),
    ] {
        let (address, server) = responding_proxy(response).await;
        let Err(error) = HttpProxyConnector::connect(&proxy(address), destination).await else {
            panic!("an invalid proxy response must be rejected");
        };
        assert_eq!(error.kind(), expected);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn http_proxy_formats_ipv6_authority_and_preserves_tunnel_bytes() {
    let (address, server) =
        responding_proxy(b"HTTP/1.1 200 Connection Established\r\n\r\nhello".to_vec()).await;
    let destination = SocketAddr::from((Ipv6Addr::LOCALHOST, 8443));

    let mut connection = HttpProxyConnector::connect(&proxy(address), destination)
        .await
        .unwrap();
    let request = server.await.unwrap();

    assert!(request.starts_with(b"CONNECT [::1]:8443 HTTP/1.1\r\n"));
    assert_eq!(connection.initial_data, b"hello");
    connection.stream.shutdown().await.unwrap();
}
