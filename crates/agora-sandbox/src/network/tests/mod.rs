mod http_proxy;
mod proxy;
mod relay;

use super::inspection::DomainObservation;
use super::{NetworkConfig, NetworkController, NetworkRunContext, NetworkState, TlsMode};
use crate::callback::{Decision, DomainSource, HttpProxy, NoopCallback, Proxy};
use crate::protocol::{HookOperation, ProcessIdentity, RouteRegistration};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

fn registration() -> RouteRegistration {
    RouteRegistration {
        connection_id: "connection-1".to_string(),
        trace_id: "trace-test".to_string(),
        destination: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 443),
        process: ProcessIdentity {
            pid: 1,
            ppid: 0,
            executable: "/tmp/client".to_string(),
        },
        operation: HookOperation::Connect,
    }
}

#[test]
fn network_config_requires_a_positive_connection_limit() {
    let mut config = NetworkConfig::default();
    assert!(config.max_connections > 0);

    config.max_connections = 0;
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("max_connections")
    );
}

#[test]
fn network_config_requires_positive_inspection_and_callback_timeouts() {
    let config = NetworkConfig {
        upstream_connect_timeout: std::time::Duration::ZERO,
        ..NetworkConfig::default()
    };
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("upstream_connect_timeout")
    );

    let config = NetworkConfig {
        domain_inspection_timeout: std::time::Duration::ZERO,
        ..NetworkConfig::default()
    };
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("domain_inspection_timeout")
    );

    let config = NetworkConfig {
        callback_timeout: std::time::Duration::ZERO,
        ..NetworkConfig::default()
    };
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("callback_timeout")
    );
}

#[tokio::test]
async fn controller_reports_an_unexpected_listener_exit() {
    let mut controller = NetworkController::start(
        NetworkConfig::default(),
        NetworkRunContext::new("sandbox", "run"),
        NoopCallback,
    )
    .await
    .unwrap();
    controller.abort_listener_for_test();

    let error = controller.wait_failure().await;

    assert!(error.to_string().contains("proxy listener"));
}

#[tokio::test]
async fn controller_reports_empty_successful_and_panicked_listener_sets() {
    let context = || NetworkRunContext::new("sandbox", "run");

    let mut empty = NetworkController::start(NetworkConfig::default(), context(), NoopCallback)
        .await
        .unwrap();
    empty.tasks.shutdown().await;
    assert!(
        empty
            .wait_failure()
            .await
            .to_string()
            .contains("no active listener")
    );

    let mut stopped = NetworkController::start(NetworkConfig::default(), context(), NoopCallback)
        .await
        .unwrap();
    stopped.tasks.shutdown().await;
    stopped.tasks.spawn(async { Ok(()) });
    assert!(
        stopped
            .wait_failure()
            .await
            .to_string()
            .contains("stopped unexpectedly")
    );

    let mut panicked = NetworkController::start(NetworkConfig::default(), context(), NoopCallback)
        .await
        .unwrap();
    panicked.tasks.shutdown().await;
    panicked.tasks.spawn(async {
        panic!("injected proxy task panic");
        #[allow(unreachable_code)]
        Ok(())
    });
    assert!(
        panicked
            .wait_failure()
            .await
            .to_string()
            .contains("listener task failed")
    );

    let mut shutdown = NetworkController::start(NetworkConfig::default(), context(), NoopCallback)
        .await
        .unwrap();
    shutdown.tasks.shutdown().await;
    shutdown.tasks.spawn(async {
        panic!("injected proxy shutdown panic");
        #[allow(unreachable_code)]
        Ok(())
    });
    assert!(shutdown.shutdown().await.is_err());
}

#[tokio::test]
async fn controller_and_upstream_fail_closed_without_required_runtime_support() {
    let tls_config = NetworkConfig {
        tls: TlsMode::Auto,
        ..NetworkConfig::default()
    };
    assert!(
        NetworkController::start(
            tls_config,
            NetworkRunContext::new("sandbox", "run"),
            NoopCallback,
        )
        .await
        .err()
        .unwrap()
        .to_string()
        .contains("requires a configured CA")
    );

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let proxy = listener.local_addr().unwrap();
    let stalled_proxy = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    });
    let mut state = NetworkState {
        config: NetworkConfig {
            upstream_connect_timeout: std::time::Duration::from_millis(10),
            ..NetworkConfig::default()
        },
        context: NetworkRunContext::new("sandbox", "run"),
        token: "token".to_string(),
        callback: NoopCallback,
        tls: None,
        connections: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
    };
    let timeout = state
        .open_upstream(
            &registration(),
            &Decision::Proxy {
                proxy: Proxy::Http(HttpProxy {
                    address: proxy.to_string(),
                    basic_auth: None,
                }),
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(timeout.kind(), std::io::ErrorKind::TimedOut);
    stalled_proxy.abort();

    state.config.upstream_connect_timeout = std::time::Duration::from_secs(1);
    let denied = state
        .open_upstream(
            &registration(),
            &Decision::Deny {
                reason: Some("test denial".to_string()),
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
    state
        .publish_denied(&registration(), None, &Decision::Allow)
        .await;
}

#[tokio::test]
async fn controller_starts_tls_interception_from_a_fixed_ca() {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec!["Agora Sandbox Test CA".to_string()]).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let certificate = params.self_signed(&key).unwrap();
    let config = NetworkConfig {
        tls: TlsMode::Auto,
        ..NetworkConfig::default()
    };

    let controller = NetworkController::start_with_tls_ca(
        config,
        NetworkRunContext::new("sandbox", "run"),
        NoopCallback,
        certificate.pem().as_bytes(),
        key.serialize_pem().as_bytes(),
    )
    .await
    .unwrap();

    assert_eq!(
        controller.runtime().tls_trust_anchor_der(),
        Some(certificate.der().as_ref())
    );
    controller.shutdown().await.unwrap();
}

#[test]
fn tls_sni_populates_only_the_tls_domain_fields() {
    let registration = registration();
    let observation = DomainObservation {
        domain: "secure.example.com".to_string(),
        source: DomainSource::TlsSni,
        target_port: None,
    };

    let context = NetworkState::<NoopCallback>::network_context(&registration, Some(&observation));

    assert_eq!(context.http_host, None);
    assert_eq!(context.tls_sni.as_deref(), Some("secure.example.com"));
    assert_eq!(context.domain.as_deref(), Some("secure.example.com"));
    assert_eq!(context.domain_source, Some(DomainSource::TlsSni));
    assert_eq!(context.target, None);
}

#[test]
fn http_connect_target_is_separate_from_the_tcp_destination() {
    let mut registration = registration();
    registration.destination = "127.0.0.1:1087".parse().unwrap();
    let observation = DomainObservation {
        domain: "chatgpt.com".to_string(),
        source: DomainSource::HttpHost,
        target_port: Some(443),
    };

    let context = NetworkState::<NoopCallback>::network_context(&registration, Some(&observation));

    assert_eq!(
        context.destination_ip,
        "127.0.0.1".parse::<IpAddr>().unwrap()
    );
    assert_eq!(context.destination_port, 1087);
    let target = context.target.as_deref().unwrap();
    assert_eq!(target.host, "chatgpt.com");
    assert_eq!(target.port, 443);
}
