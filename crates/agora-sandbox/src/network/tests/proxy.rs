use super::super::tls::{TlsAuthority, TlsBridge};
use super::super::{NetworkConfig, NetworkController, NetworkRunContext, NetworkRuntime, TlsMode};
use crate::callback::{
    BasicAuth, Callback, Decision, DomainSource, Event, EventType, HttpProxy, NetworkEvent, Proxy,
    TlsOutcome, TlsPolicy,
};
use crate::protocol::{
    ConnectRequest, HookOperation, MAX_FRAME_SIZE, PROTOCOL_VERSION, ProcessIdentity,
    encode_connect_request,
};
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, pem::PemObject};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

#[derive(Clone, Default)]
struct EventLog(Arc<Mutex<Vec<NetworkEvent>>>);

impl Callback for EventLog {
    fn on_event(&self, event: Event) -> impl Future<Output = Decision> + Send {
        if let Some(event) = event.into_network() {
            self.0.lock().unwrap().push(event);
        }
        std::future::ready(Decision::Allow)
    }
}

impl EventLog {
    fn snapshot(&self) -> Vec<NetworkEvent> {
        self.0.lock().unwrap().clone()
    }

    async fn wait_for_len(&self, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if self.0.lock().unwrap().len() >= expected {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

struct ProxyFixture {
    controller: NetworkController,
    events: EventLog,
}

impl ProxyFixture {
    async fn start() -> Self {
        Self::start_with_config(NetworkConfig::default()).await
    }

    async fn start_with_config(config: NetworkConfig) -> Self {
        let events = EventLog::default();
        let controller = NetworkController::start(
            config,
            NetworkRunContext::new("sandbox-1", "run-1"),
            events.clone(),
        )
        .await
        .unwrap();
        Self { controller, events }
    }

    async fn start_with_tls(config: NetworkConfig, tls: TlsBridge) -> Self {
        let events = EventLog::default();
        let controller = NetworkController::start_with_tls_for_test(
            config,
            NetworkRunContext::new("sandbox-1", "run-1"),
            events.clone(),
            tls,
        )
        .await
        .unwrap();
        Self { controller, events }
    }
}

fn connect_request(
    runtime: &NetworkRuntime,
    destination: SocketAddr,
    connection_id: &str,
) -> ConnectRequest {
    ConnectRequest {
        protocol_version: PROTOCOL_VERSION,
        token: runtime.token().to_string(),
        connection_id: connection_id.to_string(),
        destination,
        process: ProcessIdentity {
            pid: std::process::id(),
            ppid: 1,
            executable: "/tmp/test-client".to_string(),
        },
        trace_id: "trace-test".to_string(),
        operation: HookOperation::Connect,
    }
}

async fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut bytes = [0_u8; 64];
                loop {
                    let read = stream.read(&mut bytes).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    stream.write_all(&bytes[..read]).await.unwrap();
                }
            });
        }
    });
    address
}

async fn tls_echo_server(identity: &str) -> (SocketAddr, CertificateDer<'static>) {
    let (issuer, root) = test_ca();
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec![identity.to_string()]).unwrap();
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let certificate = params.signed_by(&key, &issuer).unwrap();
    let private_key = PrivatePkcs8KeyDer::from(key.serialize_der());
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.der().clone(), root.clone()],
            private_key.into(),
        )
        .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let mut stream = acceptor.accept(stream).await.unwrap();
                let mut buffer = [0_u8; 64];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    stream.write_all(&buffer[..read]).await.unwrap();
                }
            });
        }
    });
    (address, root)
}

async fn open_tunnel(
    runtime: &NetworkRuntime,
    request: &ConnectRequest,
    initial_data: &[u8],
) -> TcpStream {
    let mut stream = TcpStream::connect(runtime.proxy_ipv4()).await.unwrap();
    let mut bytes = encode_connect_request(request).unwrap();
    bytes.extend_from_slice(initial_data);
    stream.write_all(&bytes).await.unwrap();
    stream
}

async fn assert_rejected(runtime: &NetworkRuntime, request: &ConnectRequest) {
    let mut client = open_tunnel(runtime, request, &[]).await;
    assert_stream_rejected(&mut client).await;
}

async fn assert_stream_rejected(client: &mut TcpStream) {
    let mut byte = [0_u8; 1];
    let read = tokio::time::timeout(std::time::Duration::from_secs(1), client.read(&mut byte))
        .await
        .unwrap();
    match read {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("rejected tunnel remained usable: {other:?}"),
    }
}

async fn read_http_head(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 256];
    loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0, "HTTP connection closed before the head completed");
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(bytes).unwrap();
        }
    }
}

#[tokio::test]
async fn connect_request_relays_bytes_and_emits_ordered_audit_events() {
    let destination = echo_server().await;
    let fixture = ProxyFixture::start().await;
    let runtime = fixture.controller.runtime().clone();
    let request = connect_request(&runtime, destination, "connection-1");
    let mut client = open_tunnel(&runtime, &request, b"hello").await;

    let mut echoed = [0_u8; 5];
    client.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"hello");
    client.shutdown().await.unwrap();

    fixture.events.wait_for_len(3).await;
    let events = fixture.events.snapshot();
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        vec![
            EventType::NetworkConnectAttempt,
            EventType::NetworkConnectEstablished,
            EventType::NetworkConnectionClosed,
        ]
    );
    let network = events[0].network.as_ref().unwrap();
    assert_eq!(network.destination_ip, destination.ip());
    assert_eq!(network.destination_port, destination.port());
    assert!(events.iter().all(|event| event.trace_id == "trace-test"));

    fixture.controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn connect_preface_is_not_visible_to_the_application() {
    let destination = echo_server().await;
    let fixture = ProxyFixture::start().await;
    let runtime = fixture.controller.runtime().clone();
    let request = connect_request(&runtime, destination, "connection-transparent");
    let mut client = open_tunnel(&runtime, &request, b"hello").await;

    let mut echoed = [0_u8; 5];
    client.read_exact(&mut echoed).await.unwrap();

    assert_eq!(&echoed, b"hello");
    drop(client);
    fixture.controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn initial_payload_is_relayed_and_audited() {
    let destination = echo_server().await;
    let fixture = ProxyFixture::start().await;
    let runtime = fixture.controller.runtime().clone();
    let connect = connect_request(&runtime, destination, "connection-initial-http");
    let request = b"GET / HTTP/1.1\r\nHost: Initial.Example\r\n\r\n";
    let mut client = open_tunnel(&runtime, &connect, request).await;

    let mut echoed = vec![0_u8; request.len()];
    client.read_exact(&mut echoed).await.unwrap();
    assert_eq!(echoed, request);
    client.shutdown().await.unwrap();

    fixture.events.wait_for_len(3).await;
    let events = fixture.events.snapshot();
    let observed = events
        .iter()
        .find(|event| event.event_type == EventType::NetworkConnectAttempt)
        .unwrap();
    assert_eq!(
        observed.network.as_ref().unwrap().domain.as_deref(),
        Some("initial.example")
    );

    fixture.controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn partial_protocol_before_eof_is_relayed_unchanged() {
    let destination = echo_server().await;
    let config = NetworkConfig {
        domain_inspection_timeout: std::time::Duration::from_secs(2),
        ..NetworkConfig::default()
    };
    let fixture = ProxyFixture::start_with_config(config).await;
    let runtime = fixture.controller.runtime().clone();
    let request = connect_request(&runtime, destination, "connection-partial-protocol");
    let mut client = open_tunnel(&runtime, &request, &[]).await;
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;

    client.write_all(b"G").await.unwrap();
    client.shutdown().await.unwrap();

    let mut echoed = [0_u8; 1];
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        client.read_exact(&mut echoed),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(&echoed, b"G");

    fixture.controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn server_first_bytes_are_relayed_without_a_proxy_response() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let destination = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream.write_all(b"banner").await.unwrap();
    });
    let fixture = ProxyFixture::start().await;
    let runtime = fixture.controller.runtime().clone();
    let request = connect_request(&runtime, destination, "connection-banner");
    let mut client = open_tunnel(&runtime, &request, &[]).await;

    let mut banner = [0_u8; 6];
    client.read_exact(&mut banner).await.unwrap();

    assert_eq!(&banner, b"banner");
    drop(client);
    fixture.controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn http_host_is_audited_from_relayed_payload() {
    let destination = echo_server().await;
    let fixture = ProxyFixture::start().await;
    let runtime = fixture.controller.runtime().clone();
    let connect = connect_request(&runtime, destination, "connection-http");
    let request = b"GET / HTTP/1.1\r\nHost: Audit.Example:8080\r\n\r\n";
    let mut client = open_tunnel(&runtime, &connect, request).await;

    let mut echoed = vec![0_u8; request.len()];
    client.read_exact(&mut echoed).await.unwrap();
    assert_eq!(echoed, request);
    client.shutdown().await.unwrap();

    fixture.events.wait_for_len(3).await;
    let events = fixture.events.snapshot();
    let event = events
        .iter()
        .find(|event| event.event_type == EventType::NetworkConnectAttempt)
        .unwrap();
    let network = event.network.as_ref().unwrap();
    assert_eq!(network.http_host.as_deref(), Some("audit.example"));
    assert_eq!(network.tls_sni, None);
    assert_eq!(network.domain.as_deref(), Some("audit.example"));
    assert_eq!(network.domain_source, Some(DomainSource::HttpHost));
    let closed = events
        .iter()
        .find(|event| event.event_type == EventType::NetworkConnectionClosed)
        .unwrap();
    assert_eq!(closed.sequence, Some(2));
    assert_eq!(closed.network.as_ref().unwrap(), network);

    fixture.controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_credentials_and_versions_are_rejected_without_audit_events() {
    let destination = echo_server().await;
    let fixture = ProxyFixture::start().await;
    let runtime = fixture.controller.runtime().clone();

    let mut request = connect_request(&runtime, destination, "connection-auth");
    request.token = "wrong-token".to_string();
    assert_rejected(&runtime, &request).await;

    request.token = runtime.token().to_string();
    request.protocol_version += 1;
    assert_rejected(&runtime, &request).await;
    assert!(fixture.events.snapshot().is_empty());

    request.protocol_version = PROTOCOL_VERSION;
    request.connection_id = "connection-after-rejection".to_string();
    let mut client = open_tunnel(&runtime, &request, b"still-alive").await;
    let mut echoed = [0_u8; 11];
    client.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"still-alive");

    fixture.controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn incomplete_and_oversized_proxy_request_heads_are_rejected() {
    let fixture = ProxyFixture::start().await;
    let runtime = fixture.controller.runtime().clone();

    let mut incomplete = TcpStream::connect(runtime.proxy_ipv4()).await.unwrap();
    incomplete.write_all(b"POST /incomplete").await.unwrap();
    incomplete.shutdown().await.unwrap();
    assert_stream_rejected(&mut incomplete).await;

    let mut oversized = TcpStream::connect(runtime.proxy_ipv4()).await.unwrap();
    oversized
        .write_all(&vec![b'x'; MAX_FRAME_SIZE])
        .await
        .unwrap();
    assert_stream_rejected(&mut oversized).await;

    assert!(fixture.events.snapshot().is_empty());
    fixture.controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn upstream_failure_closes_the_tunnel_and_is_audited() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let unavailable = listener.local_addr().unwrap();
    drop(listener);
    let fixture = ProxyFixture::start().await;
    let runtime = fixture.controller.runtime().clone();
    let request = connect_request(&runtime, unavailable, "connection-failed");

    assert_rejected(&runtime, &request).await;

    assert_eq!(
        fixture
            .events
            .snapshot()
            .iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        vec![
            EventType::NetworkConnectAttempt,
            EventType::NetworkConnectFailed,
        ]
    );

    fixture.controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn denied_http_domain_never_connects_to_upstream() {
    let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let destination = upstream.local_addr().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let callback = {
        let events = Arc::clone(&events);
        move |event: Event| {
            let event = event.into_network().unwrap();
            let denied = event.event_type == EventType::NetworkConnectAttempt
                && event
                    .network
                    .as_ref()
                    .and_then(|network| network.domain.as_deref())
                    == Some("blocked.example");
            events.lock().unwrap().push(event);
            std::future::ready(if denied {
                Decision::Deny {
                    reason: Some("domain is blocked".to_string()),
                }
            } else {
                Decision::Allow
            })
        }
    };
    let controller = NetworkController::start(
        NetworkConfig::default(),
        NetworkRunContext::new("sandbox-1", "run-1"),
        callback,
    )
    .await
    .unwrap();
    let runtime = controller.runtime().clone();
    let request = connect_request(&runtime, destination, "connection-denied-domain");
    let mut client = open_tunnel(
        &runtime,
        &request,
        b"GET / HTTP/1.1\r\nHost: blocked.example\r\n\r\n",
    )
    .await;

    assert_stream_rejected(&mut client).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), upstream.accept())
            .await
            .is_err()
    );
    {
        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type)
                .collect::<Vec<_>>(),
            vec![
                EventType::NetworkConnectAttempt,
                EventType::NetworkConnectDenied,
            ]
        );
        assert_eq!(
            events[1].decision,
            Some(Decision::Deny {
                reason: Some("domain is blocked".to_string()),
            })
        );
    }

    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn denied_tls_sni_never_connects_to_upstream() {
    let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let destination = upstream.local_addr().unwrap();
    let callback = |event: Event| async move {
        let event = event.into_network().unwrap();
        let denied = event.event_type == EventType::NetworkConnectAttempt
            && event
                .network
                .as_ref()
                .and_then(|network| network.tls_sni.as_deref())
                == Some("blocked.example");
        if denied {
            Decision::Deny {
                reason: Some("TLS SNI is blocked".to_string()),
            }
        } else {
            Decision::Allow
        }
    };
    let controller = NetworkController::start(
        NetworkConfig::default(),
        NetworkRunContext::new("sandbox-1", "run-1"),
        callback,
    )
    .await
    .unwrap();
    let runtime = controller.runtime().clone();
    let request = connect_request(&runtime, destination, "connection-denied-sni");
    let hello = tls_client_hello("blocked.example");
    let mut client = open_tunnel(&runtime, &request, &hello).await;

    assert_stream_rejected(&mut client).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), upstream.accept())
            .await
            .is_err()
    );

    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn callback_timeout_denies_before_connecting_to_upstream() {
    let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let destination = upstream.local_addr().unwrap();
    let callback = |event: Event| async move {
        let event = event.into_network().unwrap();
        if event.event_type == EventType::NetworkConnectAttempt {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        Decision::Allow
    };
    let config = NetworkConfig {
        callback_timeout: std::time::Duration::from_millis(20),
        ..NetworkConfig::default()
    };
    let controller = NetworkController::start(
        config,
        NetworkRunContext::new("sandbox-1", "run-1"),
        callback,
    )
    .await
    .unwrap();
    let runtime = controller.runtime().clone();
    let request = connect_request(&runtime, destination, "connection-callback-timeout");
    let mut client = open_tunnel(
        &runtime,
        &request,
        b"GET / HTTP/1.1\r\nHost: allowed.example\r\n\r\n",
    )
    .await;

    assert_stream_rejected(&mut client).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), upstream.accept())
            .await
            .is_err()
    );

    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn proxy_decision_uses_http_connect_with_basic_auth() {
    let destination = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let destination_address = destination.local_addr().unwrap();
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let proxy_address = proxy.local_addr().unwrap();
    let request_bytes = b"GET / HTTP/1.1\r\nHost: proxied.example\r\n\r\n";
    let proxy_task = tokio::spawn(async move {
        let (mut stream, _) = proxy.accept().await.unwrap();
        let head = read_http_head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\nbanner")
            .await
            .unwrap();
        let mut request = vec![0_u8; request_bytes.len()];
        stream.read_exact(&mut request).await.unwrap();
        stream.write_all(b"proxied").await.unwrap();
        (head, request)
    });
    let decision = Decision::Proxy {
        proxy: Proxy::Http(HttpProxy {
            address: proxy_address.to_string(),
            basic_auth: Some(BasicAuth {
                username: "alice".to_string(),
                password: "secret".to_string(),
            }),
        }),
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let callback = {
        let decision = decision.clone();
        let events = Arc::clone(&events);
        move |event: Event| {
            let event = event.into_network().unwrap();
            let result = if event.event_type == EventType::NetworkConnectAttempt {
                decision.clone()
            } else {
                Decision::Allow
            };
            events.lock().unwrap().push(event);
            std::future::ready(result)
        }
    };
    let controller = NetworkController::start(
        NetworkConfig::default(),
        NetworkRunContext::new("sandbox-1", "run-1"),
        callback,
    )
    .await
    .unwrap();
    let runtime = controller.runtime().clone();
    let request = connect_request(&runtime, destination_address, "connection-http-proxy");
    let mut client = open_tunnel(&runtime, &request, request_bytes).await;

    let mut response = [0_u8; 13];
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        client.read_exact(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(&response, b"bannerproxied");
    client.shutdown().await.unwrap();
    let (head, relayed_request) = proxy_task.await.unwrap();
    assert!(head.starts_with(&format!("CONNECT {destination_address} HTTP/1.1\r\n")));
    assert!(head.contains(&format!("Host: {destination_address}\r\n")));
    assert!(head.contains("Proxy-Authorization: Basic YWxpY2U6c2VjcmV0\r\n"));
    assert_eq!(relayed_request, request_bytes);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), destination.accept())
            .await
            .is_err()
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if events.lock().unwrap().len() >= 3 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(events.lock().unwrap()[1].decision, Some(decision));

    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn http_proxy_rejection_is_fail_closed_without_direct_fallback() {
    let destination = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let destination_address = destination.local_addr().unwrap();
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let proxy_address = proxy.local_addr().unwrap();
    let proxy_task = tokio::spawn(async move {
        let (mut stream, _) = proxy.accept().await.unwrap();
        let _ = read_http_head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
            .await
            .unwrap();
    });
    let callback = move |event: Event| {
        let event = event.into_network().unwrap();
        std::future::ready(if event.event_type == EventType::NetworkConnectAttempt {
            Decision::Proxy {
                proxy: Proxy::Http(HttpProxy {
                    address: proxy_address.to_string(),
                    basic_auth: None,
                }),
            }
        } else {
            Decision::Allow
        })
    };
    let controller = NetworkController::start(
        NetworkConfig::default(),
        NetworkRunContext::new("sandbox-1", "run-1"),
        callback,
    )
    .await
    .unwrap();
    let runtime = controller.runtime().clone();
    let request = connect_request(
        &runtime,
        destination_address,
        "connection-http-proxy-rejected",
    );
    let mut client = open_tunnel(
        &runtime,
        &request,
        b"GET / HTTP/1.1\r\nHost: rejected.example\r\n\r\n",
    )
    .await;

    assert_stream_rejected(&mut client).await;
    proxy_task.await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), destination.accept())
            .await
            .is_err()
    );

    controller.shutdown().await.unwrap();
}

fn tls_client_hello(server_name: &str) -> Vec<u8> {
    let config = ClientConfig::builder()
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth();
    let server_name = ServerName::try_from(server_name.to_string()).unwrap();
    let mut connection = ClientConnection::new(Arc::new(config), server_name).unwrap();
    let mut hello = Vec::new();
    connection.write_tls(&mut hello).unwrap();
    hello
}

#[tokio::test]
async fn connection_limit_rejects_excess_tunnels() {
    let destination = echo_server().await;
    let config = NetworkConfig {
        max_connections: 1,
        ..NetworkConfig::default()
    };
    let fixture = ProxyFixture::start_with_config(config).await;
    let runtime = fixture.controller.runtime().clone();
    let first_request = connect_request(&runtime, destination, "connection-first");
    let mut first = open_tunnel(&runtime, &first_request, b"one").await;
    let mut echoed = [0_u8; 3];
    first.read_exact(&mut echoed).await.unwrap();
    fixture.events.wait_for_len(2).await;

    let second_request = connect_request(&runtime, destination, "connection-second");
    assert_rejected(&runtime, &second_request).await;

    assert_eq!(&echoed, b"one");
    drop(first);
    fixture.controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn auto_tls_intercepts_and_relays_with_the_upstream_alpn() {
    let identity = "origin.example.test";
    let (destination, origin_root) = tls_echo_server(identity).await;
    let (authority, interception_root) = interception_authority();
    let tls = TlsBridge::with_root_certificates(authority, vec![origin_root]).unwrap();
    let config = NetworkConfig {
        tls: TlsMode::Auto,
        ..NetworkConfig::default()
    };
    let fixture = ProxyFixture::start_with_tls(config, tls).await;
    let runtime = fixture.controller.runtime().clone();
    let request = connect_request(&runtime, destination, "connection-tls-intercepted");
    let tunnel = open_tunnel(&runtime, &request, &[]).await;
    let mut roots = RootCertStore::empty();
    roots.add(interception_root).unwrap();
    let mut client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let connector = TlsConnector::from(Arc::new(client_config));
    let mut client = connector
        .connect(ServerName::try_from(identity.to_string()).unwrap(), tunnel)
        .await
        .unwrap();

    client.write_all(b"hello").await.unwrap();
    let mut echoed = [0_u8; 5];
    client.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"hello");
    assert_eq!(client.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
    client.shutdown().await.unwrap();

    fixture.events.wait_for_len(3).await;
    let events = fixture.events.snapshot();
    let established = events
        .iter()
        .find(|event| event.event_type == EventType::NetworkConnectEstablished)
        .unwrap();
    let tls = established.tls.as_ref().unwrap();
    assert_eq!(tls.policy, TlsPolicy::Auto);
    assert_eq!(tls.outcome, TlsOutcome::Terminated);
    assert_eq!(tls.alpn.as_deref(), Some("h2"));

    fixture.controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn tls_interception_rejects_an_untrusted_upstream_certificate() {
    let identity = "untrusted.example.test";
    let (destination, _) = tls_echo_server(identity).await;
    let (authority, interception_root) = interception_authority();
    let (_, wrong_root) = test_ca();
    let tls = TlsBridge::with_root_certificates(authority, vec![wrong_root]).unwrap();
    let config = NetworkConfig {
        tls: TlsMode::Auto,
        ..NetworkConfig::default()
    };
    let fixture = ProxyFixture::start_with_tls(config, tls).await;
    let runtime = fixture.controller.runtime().clone();
    let request = connect_request(&runtime, destination, "connection-tls-untrusted");
    let tunnel = open_tunnel(&runtime, &request, &[]).await;
    let mut roots = RootCertStore::empty();
    roots.add(interception_root).unwrap();
    let client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));

    assert!(
        connector
            .connect(ServerName::try_from(identity.to_string()).unwrap(), tunnel)
            .await
            .is_err()
    );
    fixture.events.wait_for_len(2).await;
    let events = fixture.events.snapshot();
    let failed = events
        .iter()
        .find(|event| event.event_type == EventType::NetworkConnectFailed)
        .unwrap();
    assert_eq!(failed.tls.as_ref().unwrap().outcome, TlsOutcome::Failed);

    fixture.controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn auto_tls_passes_plaintext_through_unchanged() {
    let destination = echo_server().await;
    let (authority, _) = interception_authority();
    let (_, origin_root) = test_ca();
    let tls = TlsBridge::with_root_certificates(authority, vec![origin_root]).unwrap();
    let config = NetworkConfig {
        tls: TlsMode::Auto,
        ..NetworkConfig::default()
    };
    let fixture = ProxyFixture::start_with_tls(config, tls).await;
    let runtime = fixture.controller.runtime().clone();
    let request = connect_request(&runtime, destination, "connection-plaintext-passthrough");
    let mut client = open_tunnel(&runtime, &request, b"plaintext").await;
    let mut echoed = [0_u8; 9];

    client.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"plaintext");
    fixture.events.wait_for_len(2).await;
    let events = fixture.events.snapshot();
    let established = events
        .iter()
        .find(|event| event.event_type == EventType::NetworkConnectEstablished)
        .unwrap();
    assert!(established.tls.is_none());

    drop(client);
    fixture.controller.shutdown().await.unwrap();
}

fn interception_authority() -> (TlsAuthority, CertificateDer<'static>) {
    let (certificate, key) = test_ca_pem();
    let root = CertificateDer::pem_slice_iter(certificate.as_bytes())
        .next()
        .unwrap()
        .unwrap();
    let authority = TlsAuthority::from_pem(certificate.as_bytes(), key.as_bytes(), 16).unwrap();
    (authority, root)
}

fn test_ca() -> (CertifiedIssuer<'static, KeyPair>, CertificateDer<'static>) {
    let key = KeyPair::generate().unwrap();
    let params = ca_params();
    let issuer = CertifiedIssuer::self_signed(params, key).unwrap();
    let root = issuer.der().clone();
    (issuer, root)
}

fn test_ca_pem() -> (String, String) {
    let key = KeyPair::generate().unwrap();
    let certificate = ca_params().self_signed(&key).unwrap();
    (certificate.pem(), key.serialize_pem())
}

fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::new(vec!["Agora Test CA".to_string()]).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params
}
