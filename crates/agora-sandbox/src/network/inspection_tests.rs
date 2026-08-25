use super::inspection::{
    DomainObservation, InspectionObservation, InspectionState, MAX_INSPECTION_BYTES,
    ProtocolInspector, TlsClientHello,
};
use crate::callback::DomainSource;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore};
use std::sync::Arc;

#[test]
fn http_host_is_detected_and_normalized() {
    let mut inspector = ProtocolInspector::new();

    assert_eq!(
        inspector.inspect(b"GET / HTTP/1.1\r\nHost: Example.COM:8080\r\n"),
        InspectionState::Pending
    );
    assert_eq!(
        inspector.inspect(b"Connection: close\r\n\r\n"),
        InspectionState::Complete(InspectionObservation {
            domain: Some(DomainObservation {
                domain: "example.com".to_string(),
                source: DomainSource::HttpHost,
                target_port: None,
            }),
            tls: None,
        })
    );
}

#[test]
fn http_connect_target_preserves_its_explicit_port() {
    let mut inspector = ProtocolInspector::new();

    assert_eq!(
        inspector.inspect(b"CONNECT chatgpt.com:443 HTTP/1.1\r\nHost: chatgpt.com:443\r\n\r\n"),
        InspectionState::Complete(InspectionObservation {
            domain: Some(DomainObservation {
                domain: "chatgpt.com".to_string(),
                source: DomainSource::HttpHost,
                target_port: Some(443),
            }),
            tls: None,
        })
    );
}

#[test]
fn fragmented_tls_client_hello_sni_is_detected() {
    let hello = tls_client_hello("secure.example.com", &[b"h2", b"http/1.1"]);
    let split = hello.len() / 2;
    let mut inspector = ProtocolInspector::new();

    assert_eq!(inspector.inspect(&hello[..split]), InspectionState::Pending);
    assert_eq!(
        inspector.inspect(&hello[split..]),
        InspectionState::Complete(InspectionObservation {
            domain: Some(DomainObservation {
                domain: "secure.example.com".to_string(),
                source: DomainSource::TlsSni,
                target_port: None,
            }),
            tls: Some(TlsClientHello {
                server_name: Some("secure.example.com".to_string()),
                alpn: vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            }),
        })
    );
}

#[test]
fn non_http_and_non_tls_payload_has_no_domain() {
    let mut inspector = ProtocolInspector::new();

    assert_eq!(
        inspector.inspect(b"SSH-2.0-OpenSSH_9.9\r\n"),
        InspectionState::Complete(InspectionObservation::default())
    );
    assert_eq!(
        inspector.inspect(b"Host: misleading.example\r\n\r\n"),
        InspectionState::Complete(InspectionObservation::default())
    );
}

#[test]
fn inspection_bounds_and_malformed_protocols_finish_without_a_domain() {
    let mut empty = ProtocolInspector::new();
    assert_eq!(empty.inspect(b""), InspectionState::Pending);

    let mut oversized = ProtocolInspector::new();
    assert_eq!(
        oversized.inspect(&vec![b'A'; MAX_INSPECTION_BYTES + 1]),
        InspectionState::Complete(InspectionObservation::default())
    );

    let mut binary = ProtocolInspector::new();
    assert_eq!(
        binary.inspect(&[0]),
        InspectionState::Complete(InspectionObservation::default())
    );

    let mut malformed_tls = ProtocolInspector::new();
    assert_eq!(
        malformed_tls.inspect(b"\x16\x03\x03\x00\x01\xff"),
        InspectionState::Pending
    );

    let mut invalid_client_hello = ProtocolInspector::new();
    assert_eq!(
        invalid_client_hello.inspect(b"\x16\x03\x03\x00\x04\x01\x00\x00\x00"),
        InspectionState::Complete(InspectionObservation::default())
    );
}

#[test]
fn http_host_normalization_handles_brackets_and_non_port_colons() {
    let mut bracketed = ProtocolInspector::new();
    assert_eq!(
        bracketed.inspect(b"GET / HTTP/1.1\r\nHost: [Example.COM]:443\r\n\r\n"),
        InspectionState::Complete(InspectionObservation {
            domain: Some(DomainObservation {
                domain: "example.com".to_string(),
                source: DomainSource::HttpHost,
                target_port: None,
            }),
            tls: None,
        })
    );

    let mut non_port = ProtocolInspector::new();
    assert_eq!(
        non_port.inspect(b"GET / HTTP/1.1\r\nHost: Example.COM:not-a-port\r\n\r\n"),
        InspectionState::Complete(InspectionObservation {
            domain: Some(DomainObservation {
                domain: "example.com:not-a-port".to_string(),
                source: DomainSource::HttpHost,
                target_port: None,
            }),
            tls: None,
        })
    );
}

fn tls_client_hello(server_name: &str, alpn: &[&[u8]]) -> Vec<u8> {
    let mut config = ClientConfig::builder()
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
    let server_name = ServerName::try_from(server_name.to_string()).expect("valid server name");
    let mut connection =
        ClientConnection::new(Arc::new(config), server_name).expect("client connection");
    let mut hello = Vec::new();
    connection.write_tls(&mut hello).expect("client hello");
    hello
}
