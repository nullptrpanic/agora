use super::inspection::{
    InspectionObservation, InspectionState, MAX_INSPECTION_BYTES, ProtocolInspector,
};
use super::relay::relay_bidirectional;
use super::tls::PrefixedIo;
use super::{NetworkState, TlsMode, UpstreamConnection};
use crate::callback::{Callback, Decision, TlsContext, TlsOutcome, TlsPolicy};
use crate::protocol::{
    ConnectRequest, HANDSHAKE_TIMEOUT, MAX_FRAME_SIZE, ProtocolError, RouteRegistration,
    parse_connect_request_prefix,
};
use anyhow::{Context, Result};
use std::io;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

pub(super) struct ProxyServer<C>
where
    C: Callback,
{
    listener: TcpListener,
    state: Arc<NetworkState<C>>,
}

struct RelayContext {
    registration: RouteRegistration,
    observation: InspectionObservation,
    decision: Decision,
    tls: Option<TlsContext>,
}

impl<C> ProxyServer<C>
where
    C: Callback,
{
    pub(super) fn new(listener: TcpListener, state: Arc<NetworkState<C>>) -> Self {
        Self { listener, state }
    }

    pub(super) async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                accepted = self.listener.accept() => {
                    let (client, _) = accepted.context("sandbox proxy accept failed")?;
                    let Ok(permit) = Arc::clone(&self.state.connections).try_acquire_owned() else {
                        drop(client);
                        continue;
                    };
                    let state = Arc::clone(&self.state);
                    connections.spawn(async move {
                        let _permit = permit;
                        Self::handle_connection(state, client).await;
                    });
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        let drained = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            connections.shutdown().await;
        }
        Ok(())
    }

    async fn handle_connection(state: Arc<NetworkState<C>>, mut client: TcpStream) {
        let (request, initial_data) =
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, Self::read_request(&mut client)).await {
                Ok(Ok(request)) => request,
                Ok(Err(_)) | Err(_) => return,
            };

        if state.validate_request(&request).is_err() {
            return;
        }

        let registration = request.into_registration();
        let (initial_data, observation) = match Self::inspect_domain(
            &mut client,
            initial_data,
            state.config.domain_inspection_timeout,
        )
        .await
        {
            Ok(result) => result,
            Err(_) => return,
        };
        let decision = state
            .authorize(&registration, observation.domain.as_ref())
            .await;
        if matches!(decision, Decision::Deny { .. }) {
            drop(client);
            state
                .publish_denied(&registration, observation.domain.as_ref(), &decision)
                .await;
            return;
        }
        let upstream = match state.open_upstream(&registration, &decision).await {
            Ok(upstream) => upstream,
            Err(error) => {
                let tls = observation
                    .tls
                    .as_ref()
                    .map(|_| Self::tls_context(state.config.tls, TlsOutcome::Failed, None));
                state
                    .publish_failed(
                        &registration,
                        observation.domain.as_ref(),
                        &decision,
                        &error,
                        tls,
                    )
                    .await;
                return;
            }
        };

        if state.config.tls != TlsMode::Off
            && let Some(hello) = observation.tls.as_ref()
        {
            let identity = hello
                .server_name
                .clone()
                .unwrap_or_else(|| registration.destination.ip().to_string());
            let bridge = state
                .tls
                .as_ref()
                .expect("validated TLS mode must have a bridge");
            match bridge
                .establish(
                    client,
                    upstream,
                    initial_data,
                    hello,
                    identity,
                    state.config.upstream_connect_timeout,
                )
                .await
            {
                Ok(connection) => {
                    let tls = Self::tls_context(
                        state.config.tls,
                        TlsOutcome::Terminated,
                        connection.alpn().map(str::to_string),
                    );
                    state
                        .publish_established(
                            &registration,
                            observation.domain.as_ref(),
                            &decision,
                            Some(tls.clone()),
                        )
                        .await;
                    Self::relay_tls(
                        state,
                        connection,
                        RelayContext {
                            registration,
                            observation,
                            decision,
                            tls: Some(tls),
                        },
                    )
                    .await;
                }
                Err(error) => {
                    state
                        .publish_failed(
                            &registration,
                            observation.domain.as_ref(),
                            &decision,
                            &error,
                            Some(Self::tls_context(
                                state.config.tls,
                                TlsOutcome::Failed,
                                None,
                            )),
                        )
                        .await;
                }
            }
            return;
        }

        let tls = observation
            .tls
            .as_ref()
            .map(|_| Self::tls_context(state.config.tls, TlsOutcome::Passthrough, None));
        state
            .publish_established(
                &registration,
                observation.domain.as_ref(),
                &decision,
                tls.clone(),
            )
            .await;
        Self::relay(
            state,
            client,
            upstream,
            initial_data,
            RelayContext {
                registration,
                observation,
                decision,
                tls,
            },
        )
        .await;
    }

    async fn inspect_domain(
        client: &mut TcpStream,
        mut initial_data: Vec<u8>,
        timeout: std::time::Duration,
    ) -> io::Result<(Vec<u8>, InspectionObservation)> {
        let mut inspector = ProtocolInspector::new();
        if !initial_data.is_empty() {
            match inspector.inspect(&initial_data) {
                InspectionState::Pending => {}
                InspectionState::Complete(observation) => {
                    return Ok((initial_data, observation));
                }
            }
        }

        let deadline = tokio::time::Instant::now() + timeout;
        let mut buffer = [0_u8; 4096];
        while initial_data.len() < MAX_INSPECTION_BYTES {
            let available = (MAX_INSPECTION_BYTES - initial_data.len()).min(buffer.len());
            let read = match tokio::time::timeout_at(
                deadline,
                client.read(&mut buffer[..available]),
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => break,
            };
            if read == 0 {
                break;
            }
            initial_data.extend_from_slice(&buffer[..read]);
            match inspector.inspect(&buffer[..read]) {
                InspectionState::Pending => {}
                InspectionState::Complete(observation) => {
                    return Ok((initial_data, observation));
                }
            }
        }
        Ok((initial_data, InspectionObservation::default()))
    }

    async fn read_request(
        client: &mut TcpStream,
    ) -> Result<(ConnectRequest, Vec<u8>), ProtocolError> {
        let mut bytes = Vec::with_capacity(4096);
        let mut buffer = [0_u8; 4096];
        while bytes.len() < MAX_FRAME_SIZE {
            let available = (MAX_FRAME_SIZE - bytes.len()).min(buffer.len());
            let read = client
                .read(&mut buffer[..available])
                .await
                .map_err(|error| {
                    ProtocolError::bad_request(format!("failed to read HTTP request: {error}"))
                })?;
            if read == 0 {
                return Err(ProtocolError::bad_request(
                    "proxy connection closed before the HTTP request was complete",
                ));
            }
            bytes.extend_from_slice(&buffer[..read]);
            if let Some((request, consumed)) = parse_connect_request_prefix(&bytes)? {
                let initial_data = bytes.split_off(consumed);
                return Ok((request, initial_data));
            }
        }
        Err(ProtocolError::bad_request(format!(
            "HTTP request head exceeds {MAX_FRAME_SIZE} bytes",
        )))
    }

    async fn relay(
        state: Arc<NetworkState<C>>,
        client: TcpStream,
        upstream: UpstreamConnection,
        initial_client_data: Vec<u8>,
        context: RelayContext,
    ) {
        let started = Instant::now();
        let result = relay_bidirectional(
            PrefixedIo::new(initial_client_data, client),
            PrefixedIo::new(upstream.initial_data, upstream.stream),
        )
        .await;
        let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        state
            .publish_closed(
                &context.registration,
                result,
                duration_ms,
                context.observation.domain.as_ref(),
                &context.decision,
                context.tls,
            )
            .await;
    }

    async fn relay_tls(
        state: Arc<NetworkState<C>>,
        connection: super::tls::TlsConnection,
        context: RelayContext,
    ) {
        let started = Instant::now();
        let result = connection.relay().await;
        let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        state
            .publish_closed(
                &context.registration,
                result,
                duration_ms,
                context.observation.domain.as_ref(),
                &context.decision,
                context.tls,
            )
            .await;
    }

    fn tls_context(mode: TlsMode, outcome: TlsOutcome, alpn: Option<String>) -> TlsContext {
        let policy = match mode {
            TlsMode::Off => TlsPolicy::Off,
            TlsMode::Auto => TlsPolicy::Auto,
        };
        TlsContext {
            policy,
            outcome,
            alpn,
        }
    }
}
