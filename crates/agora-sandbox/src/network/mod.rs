mod config;
mod http_proxy;
mod inspection;
mod proxy;
mod relay;
mod tls;

pub use config::{NetworkConfig, NetworkEnforcement, TlsMode};

use std::path::Path;

pub fn generate_tls_ca(certificate: impl AsRef<Path>, private_key: impl AsRef<Path>) -> Result<()> {
    tls::certificate::generate_ca(certificate.as_ref(), private_key.as_ref())
}

pub(crate) fn validate_tls_ca(certificate_pem: &[u8], private_key_pem: &[u8]) -> Result<()> {
    TlsAuthority::from_pem(certificate_pem, private_key_pem, 1).map(|_| ())
}

use crate::callback::{
    Callback, Decision, DomainSource, EVENT_SCHEMA_VERSION, Event, EventMetrics, EventResult,
    EventStatus, EventType, NetworkContext, NetworkEvent, NetworkProtocol, NetworkTarget,
    ProcessContext, Proxy, Subsystem, TlsContext,
};
use crate::protocol::{ConnectRequest, PROTOCOL_VERSION, ProtocolError, RouteRegistration};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use http_proxy::HttpProxyConnector;
use inspection::DomainObservation;
use relay::RelayOutcome;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
#[cfg(target_os = "macos")]
pub(crate) use tls::native_root_certificates;
use tls::{TlsAuthority, TlsBridge};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NetworkRunContext {
    sandbox_id: String,
    run_id: String,
}

impl NetworkRunContext {
    pub(crate) fn new(sandbox_id: impl Into<String>, run_id: impl Into<String>) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            run_id: run_id.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NetworkRuntime {
    token: String,
    proxy_ipv4: SocketAddr,
    proxy_ipv6: SocketAddr,
    tls_trust_anchor_der: Option<Vec<u8>>,
}

impl NetworkRuntime {
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn proxy_ipv4(&self) -> SocketAddr {
        self.proxy_ipv4
    }

    pub(crate) fn proxy_ipv6(&self) -> SocketAddr {
        self.proxy_ipv6
    }

    pub(crate) fn tls_trust_anchor_der(&self) -> Option<&[u8]> {
        self.tls_trust_anchor_der.as_deref()
    }
}

pub(crate) struct NetworkController {
    runtime: NetworkRuntime,
    shutdown: watch::Sender<bool>,
    tasks: JoinSet<Result<()>>,
}

impl NetworkController {
    pub(crate) async fn start<C>(
        config: NetworkConfig,
        context: NetworkRunContext,
        callback: C,
    ) -> Result<Self>
    where
        C: Callback,
    {
        Self::start_inner(config, context, callback, None).await
    }

    pub(crate) async fn start_with_tls_ca<C>(
        config: NetworkConfig,
        context: NetworkRunContext,
        callback: C,
        certificate_pem: &[u8],
        private_key_pem: &[u8],
    ) -> Result<Self>
    where
        C: Callback,
    {
        const CERTIFICATE_CACHE_CAPACITY: usize = 2048;

        let authority =
            TlsAuthority::from_pem(certificate_pem, private_key_pem, CERTIFICATE_CACHE_CAPACITY)?;
        let tls = TlsBridge::new(authority)?;
        Self::start_inner(config, context, callback, Some(Arc::new(tls))).await
    }

    #[cfg(test)]
    pub(crate) async fn start_with_tls_ca_and_roots<C>(
        config: NetworkConfig,
        context: NetworkRunContext,
        callback: C,
        certificate_pem: &[u8],
        private_key_pem: &[u8],
        upstream_roots: Vec<rustls::pki_types::CertificateDer<'static>>,
    ) -> Result<Self>
    where
        C: Callback,
    {
        const CERTIFICATE_CACHE_CAPACITY: usize = 2048;

        let authority =
            TlsAuthority::from_pem(certificate_pem, private_key_pem, CERTIFICATE_CACHE_CAPACITY)?;
        let tls = TlsBridge::with_root_certificates(authority, upstream_roots)?;
        Self::start_inner(config, context, callback, Some(Arc::new(tls))).await
    }

    #[cfg(test)]
    pub(in crate::network) async fn start_with_tls_for_test<C>(
        config: NetworkConfig,
        context: NetworkRunContext,
        callback: C,
        tls: TlsBridge,
    ) -> Result<Self>
    where
        C: Callback,
    {
        Self::start_inner(config, context, callback, Some(Arc::new(tls))).await
    }

    async fn start_inner<C>(
        config: NetworkConfig,
        context: NetworkRunContext,
        callback: C,
        tls: Option<Arc<TlsBridge>>,
    ) -> Result<Self>
    where
        C: Callback,
    {
        config.validate()?;
        if config.tls != TlsMode::Off && tls.is_none() {
            anyhow::bail!("TLS interception requires a configured CA certificate and private key");
        }

        let tls_trust_anchor_der = tls.as_deref().map(TlsBridge::trust_anchor_der);
        let token = Uuid::new_v4().simple().to_string();
        let ipv4_listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .context("failed to bind IPv4 sandbox proxy")?;
        let ipv6_listener = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0))
            .await
            .context("failed to bind IPv6 sandbox proxy")?;
        let proxy_ipv4 = ipv4_listener.local_addr()?;
        let proxy_ipv6 = ipv6_listener.local_addr()?;
        let max_connections = config.max_connections;
        let state = Arc::new(NetworkState {
            config,
            context,
            token: token.clone(),
            callback,
            tls,
            connections: Arc::new(Semaphore::new(max_connections)),
        });
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let ipv4 = proxy::ProxyServer::new(ipv4_listener, Arc::clone(&state));
        let ipv6 = proxy::ProxyServer::new(ipv6_listener, state);
        let mut tasks = JoinSet::new();
        tasks.spawn(ipv4.run(shutdown_receiver.clone()));
        tasks.spawn(ipv6.run(shutdown_receiver));

        Ok(Self {
            runtime: NetworkRuntime {
                token,
                proxy_ipv4,
                proxy_ipv6,
                tls_trust_anchor_der,
            },
            shutdown,
            tasks,
        })
    }

    pub(crate) fn runtime(&self) -> &NetworkRuntime {
        &self.runtime
    }

    pub(crate) async fn wait_failure(&mut self) -> anyhow::Error {
        match self.tasks.join_next().await {
            Some(Ok(Ok(()))) => anyhow::anyhow!("sandbox proxy listener stopped unexpectedly"),
            Some(Ok(Err(error))) => error.context("sandbox proxy listener failed"),
            Some(Err(error)) => {
                anyhow::Error::from(error).context("sandbox proxy listener task failed")
            }
            None => anyhow::anyhow!("sandbox proxy has no active listener tasks"),
        }
    }

    pub(crate) async fn shutdown(mut self) -> Result<()> {
        let _ = self.shutdown.send(true);
        let mut first_error = None;
        while let Some(task) = self.tasks.join_next().await {
            match task {
                Ok(Ok(())) => {}
                Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
                Err(error) if first_error.is_none() => first_error = Some(error.into()),
                _ => {}
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn abort_listener_for_test(&mut self) {
        self.tasks.spawn(async {
            anyhow::bail!("injected proxy listener failure");
        });
    }
}

impl Drop for NetworkController {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.tasks.abort_all();
    }
}

struct NetworkState<C>
where
    C: Callback,
{
    config: NetworkConfig,
    context: NetworkRunContext,
    token: String,
    callback: C,
    tls: Option<Arc<TlsBridge>>,
    connections: Arc<Semaphore>,
}

struct EventPublication {
    event_type: EventType,
    sequence: u64,
    decision: Option<Decision>,
    result: EventResult,
    metrics: Option<EventMetrics>,
    tls: Option<TlsContext>,
}

struct UpstreamConnection {
    stream: TcpStream,
    initial_data: Vec<u8>,
}

impl<C> NetworkState<C>
where
    C: Callback,
{
    pub(super) fn validate_request(&self, request: &ConnectRequest) -> Result<(), ProtocolError> {
        if request.protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::version_not_supported(format!(
                "unsupported proxy protocol version {}",
                request.protocol_version,
            )));
        }
        if request.token != self.token {
            return Err(ProtocolError::unauthorized("invalid proxy bearer token"));
        }
        Ok(())
    }

    pub(super) async fn authorize(
        &self,
        registration: &RouteRegistration,
        observation: Option<&DomainObservation>,
    ) -> Decision {
        self.dispatch(self.network_event(
            registration,
            observation,
            EventPublication {
                event_type: EventType::NetworkConnectAttempt,
                sequence: 0,
                decision: None,
                result: EventResult {
                    status: EventStatus::Started,
                    error_code: None,
                    error_message: None,
                },
                metrics: None,
                tls: None,
            },
        ))
        .await
    }

    pub(super) async fn publish_denied(
        &self,
        registration: &RouteRegistration,
        observation: Option<&DomainObservation>,
        decision: &Decision,
    ) {
        let reason = match decision {
            Decision::Deny { reason } => reason.clone(),
            Decision::Allow | Decision::Proxy { .. } => None,
        };
        let event = self.network_event(
            registration,
            observation,
            EventPublication {
                event_type: EventType::NetworkConnectDenied,
                sequence: 1,
                decision: Some(decision.clone()),
                result: EventResult {
                    status: EventStatus::Denied,
                    error_code: Some("policy_denied".to_string()),
                    error_message: reason,
                },
                metrics: None,
                tls: None,
            },
        );
        let _ = self.dispatch(event).await;
    }

    pub(super) async fn open_upstream(
        &self,
        registration: &RouteRegistration,
        decision: &Decision,
    ) -> io::Result<UpstreamConnection> {
        let upstream = tokio::time::timeout(
            self.config.upstream_connect_timeout,
            Self::dial_upstream(registration.destination, decision),
        )
        .await;
        match upstream {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "upstream connection timed out",
            )),
        }
    }

    async fn dial_upstream(
        destination: SocketAddr,
        decision: &Decision,
    ) -> io::Result<UpstreamConnection> {
        match decision {
            Decision::Allow => Ok(UpstreamConnection {
                stream: TcpStream::connect(destination).await?,
                initial_data: Vec::new(),
            }),
            Decision::Deny { .. } => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "network connection was denied by policy",
            )),
            Decision::Proxy {
                proxy: Proxy::Http(proxy),
            } => {
                let connection = HttpProxyConnector::connect(proxy, destination).await?;
                Ok(UpstreamConnection {
                    stream: connection.stream,
                    initial_data: connection.initial_data,
                })
            }
        }
    }

    pub(super) async fn publish_established(
        &self,
        registration: &RouteRegistration,
        observation: Option<&DomainObservation>,
        decision: &Decision,
        tls: Option<TlsContext>,
    ) {
        self.publish_route_event(
            registration,
            observation,
            EventPublication {
                event_type: EventType::NetworkConnectEstablished,
                sequence: 1,
                decision: Some(decision.clone()),
                result: EventResult {
                    status: EventStatus::Succeeded,
                    error_code: None,
                    error_message: None,
                },
                metrics: None,
                tls,
            },
        )
        .await;
    }

    pub(super) async fn publish_failed(
        &self,
        registration: &RouteRegistration,
        observation: Option<&DomainObservation>,
        decision: &Decision,
        error: &io::Error,
        tls: Option<TlsContext>,
    ) {
        self.publish_route_event(
            registration,
            observation,
            EventPublication {
                event_type: EventType::NetworkConnectFailed,
                sequence: 1,
                decision: Some(decision.clone()),
                result: EventResult {
                    status: EventStatus::Failed,
                    error_code: error.raw_os_error().map(|value| value.to_string()),
                    error_message: Some(error.to_string()),
                },
                metrics: None,
                tls,
            },
        )
        .await;
    }

    async fn publish_route_event(
        &self,
        registration: &RouteRegistration,
        observation: Option<&DomainObservation>,
        publication: EventPublication,
    ) {
        let event = self.network_event(registration, observation, publication);
        let _ = self.dispatch(event).await;
    }

    pub(super) async fn publish_closed(
        &self,
        registration: &RouteRegistration,
        outcome: RelayOutcome,
        duration_ms: u64,
        observation: Option<&DomainObservation>,
        decision: &Decision,
        tls: Option<TlsContext>,
    ) {
        let metrics = EventMetrics {
            bytes_sent: outcome.bytes_sent,
            bytes_received: outcome.bytes_received,
            duration_ms,
        };
        let (status, error_code, error_message) = match outcome.error {
            None => (EventStatus::Succeeded, None, None),
            Some(error) => (
                EventStatus::Failed,
                error.raw_os_error().map(|value| value.to_string()),
                Some(error.to_string()),
            ),
        };
        let event = self.network_event(
            registration,
            observation,
            EventPublication {
                event_type: EventType::NetworkConnectionClosed,
                sequence: 2,
                decision: Some(decision.clone()),
                result: EventResult {
                    status,
                    error_code,
                    error_message,
                },
                metrics: Some(metrics),
                tls,
            },
        );
        let _ = self.dispatch(event).await;
    }

    async fn dispatch(&self, event: NetworkEvent) -> Decision {
        match tokio::time::timeout(
            self.config.callback_timeout,
            self.callback.on_event(Event::Network(event)),
        )
        .await
        {
            Ok(decision) => decision,
            Err(_) => Decision::Deny {
                reason: Some("sandbox callback timed out".to_string()),
            },
        }
    }

    fn network_event(
        &self,
        registration: &RouteRegistration,
        observation: Option<&DomainObservation>,
        publication: EventPublication,
    ) -> NetworkEvent {
        NetworkEvent {
            schema_version: EVENT_SCHEMA_VERSION,
            event_id: Uuid::new_v4().to_string(),
            occurred_at: Self::now(),
            subsystem: Subsystem::Network,
            event_type: publication.event_type,
            sandbox_id: self.context.sandbox_id.clone(),
            run_id: self.context.run_id.clone(),
            trace_id: registration.trace_id.clone(),
            connection_id: Some(registration.connection_id.clone()),
            sequence: Some(publication.sequence),
            process: Self::process_context(&registration.process),
            network: Some(Self::network_context(registration, observation)),
            tls: publication.tls,
            decision: publication.decision,
            result: publication.result,
            metrics: publication.metrics,
        }
    }

    fn process_context(process: &crate::protocol::ProcessIdentity) -> ProcessContext {
        ProcessContext {
            pid: process.pid,
            ppid: process.ppid,
            executable: process.executable.clone(),
        }
    }

    fn network_context(
        registration: &RouteRegistration,
        observation: Option<&DomainObservation>,
    ) -> NetworkContext {
        NetworkContext {
            protocol: NetworkProtocol::Tcp,
            destination_ip: registration.destination.ip(),
            destination_port: registration.destination.port(),
            target: observation.and_then(|value| {
                value.target_port.map(|port| {
                    Box::new(NetworkTarget {
                        host: value.domain.clone(),
                        port,
                    })
                })
            }),
            http_host: observation
                .filter(|value| value.source == DomainSource::HttpHost)
                .map(|value| value.domain.clone()),
            tls_sni: observation
                .filter(|value| value.source == DomainSource::TlsSni)
                .map(|value| value.domain.clone()),
            domain: observation.map(|value| value.domain.clone()),
            domain_source: observation.map(|value| value.source),
        }
    }

    fn now() -> String {
        Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
    }
}

#[cfg(test)]
mod inspection_tests;
#[cfg(test)]
mod tests;
