use super::{
    FixedResolver, TlsAuthority, TlsBridge, load_pem_root_certificates, other_error, timeout_error,
};
use crate::network::UpstreamConnection;
use crate::network::inspection::TlsClientHello;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use std::io::ErrorKind;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

#[test]
fn fallback_root_bundle_accepts_certificates_and_rejects_invalid_inputs() {
    let directory =
        std::env::temp_dir().join(format!("agora-fallback-roots-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();

    let key = KeyPair::generate().unwrap();
    let certificate = CertificateParams::new(Vec::new())
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let valid = directory.join("valid.pem");
    std::fs::write(&valid, certificate.pem()).unwrap();
    assert_eq!(
        load_pem_root_certificates(valid.to_str().unwrap())
            .unwrap()
            .len(),
        1
    );

    let empty = directory.join("empty.pem");
    std::fs::write(&empty, b"").unwrap();
    assert!(
        load_pem_root_certificates(empty.to_str().unwrap())
            .unwrap_err()
            .to_string()
            .contains("contains no certificates")
    );

    let malformed = directory.join("malformed.pem");
    std::fs::write(
        &malformed,
        b"-----BEGIN CERTIFICATE-----\n%%%%\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    assert!(load_pem_root_certificates(malformed.to_str().unwrap()).is_err());
    assert!(
        load_pem_root_certificates(directory.join("missing.pem").to_str().unwrap())
            .unwrap_err()
            .to_string()
            .contains("failed to read fallback TLS roots")
    );

    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn tls_error_helpers_preserve_their_intended_error_kinds() {
    let other = other_error("TLS bridge failed");
    assert_eq!(other.kind(), ErrorKind::Other);
    assert_eq!(other.to_string(), "TLS bridge failed");

    let timeout = timeout_error("TLS handshake timed out");
    assert_eq!(timeout.kind(), ErrorKind::TimedOut);
    assert_eq!(timeout.to_string(), "TLS handshake timed out");
}

#[test]
fn tls_bridge_rejects_empty_roots_and_resolver_debug_is_stable() {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let certificate = params.self_signed(&key).unwrap();
    let authority = TlsAuthority::from_pem(
        certificate.pem().as_bytes(),
        key.serialize_pem().as_bytes(),
        2,
    )
    .unwrap();

    let Err(error) = TlsBridge::with_root_certificates(authority, Vec::new()) else {
        panic!("empty root certificates must be rejected");
    };
    assert!(error.to_string().contains("contains no valid certificates"));

    let authority = TlsAuthority::from_pem(
        certificate.pem().as_bytes(),
        key.serialize_pem().as_bytes(),
        2,
    )
    .unwrap();
    let certificate = authority.issue("example.com").unwrap();
    let resolver = FixedResolver(certificate.certified_key());
    assert_eq!(format!("{resolver:?}"), "FixedResolver");
}

#[tokio::test]
async fn tls_bridge_rejects_a_wildcard_as_an_upstream_server_name() {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let certificate = params.self_signed(&key).unwrap();
    let root = certificate.der().clone();
    let authority = TlsAuthority::from_pem(
        certificate.pem().as_bytes(),
        key.serialize_pem().as_bytes(),
        2,
    )
    .unwrap();
    let bridge = TlsBridge::with_root_certificates(authority, vec![root]).unwrap();
    let (client, _client_peer) = tcp_pair().await;
    let (upstream, _upstream_peer) = tcp_pair().await;

    let result = bridge
        .establish(
            client,
            UpstreamConnection {
                stream: upstream,
                initial_data: Vec::new(),
            },
            Vec::new(),
            &TlsClientHello {
                server_name: Some("*.example.com".to_string()),
                alpn: Vec::new(),
            },
            "*.example.com".to_string(),
            Duration::from_millis(25),
        )
        .await;
    let Err(error) = result else {
        panic!("wildcard TLS server identity unexpectedly succeeded");
    };

    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(error.to_string().contains("invalid TLS server identity"));
}

async fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let connected = TcpStream::connect(address);
    let accepted = listener.accept();
    let (connected, accepted) = tokio::join!(connected, accepted);
    (connected.unwrap(), accepted.unwrap().0)
}
