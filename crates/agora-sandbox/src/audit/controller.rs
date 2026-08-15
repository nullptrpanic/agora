use super::protocol::{
    AuditEventRequest, AuditResponse, FileOperation, decode_request, encode_response, frame_length,
};
use crate::callback::{
    Callback, EVENT_SCHEMA_VERSION, Event, EventResult, EventStatus, EventType, FileEvent,
    ProcessEvent, Subsystem,
};
use crate::trace::TraceContext;
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use ring::digest::{SHA256, digest};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Semaphore, oneshot, watch};
use tokio::task::JoinSet;
use uuid::Uuid;

const AUDIT_MAX_CONNECTIONS: usize = 1024;
const AUDIT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);
const AUDIT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const AUDIT_REQUEST_TTL: Duration = Duration::from_secs(120);
const AUDIT_REQUEST_CAPACITY: usize = 4096;

#[derive(Clone, Debug)]
pub(crate) struct AuditRuntime {
    token: String,
    control: SocketAddr,
}

impl AuditRuntime {
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn control(&self) -> SocketAddr {
        self.control
    }
}

pub(crate) struct AuditController {
    runtime: AuditRuntime,
    shutdown: watch::Sender<bool>,
    tasks: JoinSet<Result<()>>,
}

impl AuditController {
    pub(crate) async fn start<C>(
        sandbox_id: String,
        run_id: String,
        callback: C,
        callback_timeout: Duration,
    ) -> Result<Self>
    where
        C: Callback,
    {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("failed to bind sandbox audit controller")?;
        let control = listener.local_addr()?;
        let token = Uuid::new_v4().simple().to_string();
        let (shutdown, receiver) = watch::channel(false);
        let state = Arc::new(AuditState {
            token: token.clone(),
            sandbox_id,
            run_id,
            callback,
            callback_timeout,
            requests: Mutex::new(AuditRequestCache::default()),
        });
        let mut tasks = JoinSet::new();
        tasks.spawn(AuditServer::new(listener, state).run(receiver));
        Ok(Self {
            runtime: AuditRuntime { token, control },
            shutdown,
            tasks,
        })
    }

    pub(crate) fn runtime(&self) -> &AuditRuntime {
        &self.runtime
    }

    pub(crate) async fn wait_failure(&mut self) -> anyhow::Error {
        match self.tasks.join_next().await {
            Some(Ok(Ok(()))) => anyhow::anyhow!("sandbox audit controller stopped unexpectedly"),
            Some(Ok(Err(error))) => error.context("sandbox audit controller failed"),
            Some(Err(error)) => anyhow::Error::from(error).context("sandbox audit task failed"),
            None => anyhow::anyhow!("sandbox audit controller has no active task"),
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
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn abort_server_for_test(&mut self) {
        self.tasks.spawn(async {
            anyhow::bail!("injected audit controller failure");
        });
    }
}

impl Drop for AuditController {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.tasks.abort_all();
    }
}

struct AuditState<C>
where
    C: Callback,
{
    token: String,
    sandbox_id: String,
    run_id: String,
    callback: C,
    callback_timeout: Duration,
    requests: Mutex<AuditRequestCache>,
}

#[derive(Default)]
struct AuditRequestCache {
    entries: HashMap<String, CachedAuditRequest>,
}

enum CachedAuditRequest {
    Pending {
        fingerprint: [u8; 32],
        waiters: Vec<oneshot::Sender<AuditResponse>>,
    },
    Completed {
        fingerprint: [u8; 32],
        response: AuditResponse,
        completed_at: Instant,
    },
}

enum AuditCacheDecision {
    Execute,
    Wait(oneshot::Receiver<AuditResponse>),
    Replay(AuditResponse),
    Reject,
}

impl<C> AuditState<C>
where
    C: Callback,
{
    async fn publish(&self, request: AuditEventRequest) -> Result<()> {
        let event = match request {
            AuditEventRequest::Ping => anyhow::bail!("audit ping cannot be published as an event"),
            AuditEventRequest::Process {
                trace_id,
                process,
                command,
            } => Event::Process(ProcessEvent {
                schema_version: EVENT_SCHEMA_VERSION,
                event_id: Uuid::new_v4().to_string(),
                occurred_at: now(),
                subsystem: Subsystem::Process,
                event_type: EventType::ProcessExecAttempt,
                sandbox_id: self.sandbox_id.clone(),
                run_id: self.run_id.clone(),
                trace_id: validate_trace(trace_id)?,
                process,
                command,
                result: started(),
            }),
            AuditEventRequest::File {
                trace_id,
                process,
                operation,
                file,
            } => Event::File(FileEvent {
                schema_version: EVENT_SCHEMA_VERSION,
                event_id: Uuid::new_v4().to_string(),
                occurred_at: now(),
                subsystem: Subsystem::Filesystem,
                event_type: match operation {
                    FileOperation::Open => EventType::FilesystemOpen,
                    FileOperation::Close => EventType::FilesystemClose,
                },
                sandbox_id: self.sandbox_id.clone(),
                run_id: self.run_id.clone(),
                trace_id: validate_trace(trace_id)?,
                process,
                file,
                result: started(),
            }),
        };
        let _ = tokio::time::timeout(self.callback_timeout, self.callback.on_event(event)).await;
        Ok(())
    }

    async fn publish_once(&self, request_id: String, event: AuditEventRequest) -> AuditResponse {
        let fingerprint = match audit_event_fingerprint(&event) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                return AuditResponse::Error {
                    errno: libc::EPROTO,
                    message: format!("failed to fingerprint audit request: {error}"),
                };
            }
        };
        let decision = self
            .requests
            .lock()
            .await
            .begin(request_id.clone(), fingerprint);
        match decision {
            AuditCacheDecision::Execute => {
                let response = match self.publish(event).await {
                    Ok(()) => AuditResponse::Accepted,
                    Err(error) => AuditResponse::Error {
                        errno: libc::EINVAL,
                        message: format!("{error:#}"),
                    },
                };
                self.requests
                    .lock()
                    .await
                    .complete(request_id, response.clone(), Instant::now());
                response
            }
            AuditCacheDecision::Wait(receiver) => receiver.await.unwrap_or(AuditResponse::Error {
                errno: libc::EIO,
                message: "audit request was cancelled".to_string(),
            }),
            AuditCacheDecision::Replay(response) => response,
            AuditCacheDecision::Reject => AuditResponse::Error {
                errno: libc::EPROTO,
                message: "audit request ID was reused for a different event".to_string(),
            },
        }
    }
}

impl AuditRequestCache {
    fn begin(&mut self, request_id: String, fingerprint: [u8; 32]) -> AuditCacheDecision {
        self.prune(Instant::now());
        match self.entries.get_mut(&request_id) {
            Some(CachedAuditRequest::Pending {
                fingerprint: cached,
                waiters,
            }) if cached == &fingerprint => {
                let (sender, receiver) = oneshot::channel();
                waiters.push(sender);
                AuditCacheDecision::Wait(receiver)
            }
            Some(CachedAuditRequest::Completed {
                fingerprint: cached,
                response,
                ..
            }) if cached == &fingerprint => AuditCacheDecision::Replay(response.clone()),
            Some(_) => AuditCacheDecision::Reject,
            None => {
                self.entries.insert(
                    request_id,
                    CachedAuditRequest::Pending {
                        fingerprint,
                        waiters: Vec::new(),
                    },
                );
                AuditCacheDecision::Execute
            }
        }
    }

    fn complete(&mut self, request_id: String, response: AuditResponse, now: Instant) {
        let Some(CachedAuditRequest::Pending {
            fingerprint,
            waiters,
        }) = self.entries.remove(&request_id)
        else {
            return;
        };
        for waiter in waiters {
            let _ = waiter.send(response.clone());
        }
        self.entries.insert(
            request_id,
            CachedAuditRequest::Completed {
                fingerprint,
                response,
                completed_at: now,
            },
        );
        self.prune(now);
    }

    fn prune(&mut self, now: Instant) {
        self.entries.retain(|_, entry| match entry {
            CachedAuditRequest::Pending { .. } => true,
            CachedAuditRequest::Completed { completed_at, .. } => {
                now.saturating_duration_since(*completed_at) < AUDIT_REQUEST_TTL
            }
        });
        let completed = self
            .entries
            .values()
            .filter(|entry| matches!(entry, CachedAuditRequest::Completed { .. }))
            .count();
        if completed <= AUDIT_REQUEST_CAPACITY {
            return;
        }
        let mut oldest = self
            .entries
            .iter()
            .filter_map(|(request_id, entry)| match entry {
                CachedAuditRequest::Completed { completed_at, .. } => {
                    Some((request_id.clone(), *completed_at))
                }
                CachedAuditRequest::Pending { .. } => None,
            })
            .collect::<Vec<_>>();
        oldest.sort_unstable_by_key(|(_, completed_at)| *completed_at);
        for (request_id, _) in oldest.into_iter().take(completed - AUDIT_REQUEST_CAPACITY) {
            self.entries.remove(&request_id);
        }
    }
}

fn audit_event_fingerprint(event: &AuditEventRequest) -> Result<[u8; 32], serde_json::Error> {
    let encoded = serde_json::to_vec(event)?;
    let digest = digest(&SHA256, &encoded);
    let mut fingerprint = [0_u8; 32];
    fingerprint.copy_from_slice(digest.as_ref());
    Ok(fingerprint)
}

struct AuditServer<C>
where
    C: Callback,
{
    listener: TcpListener,
    state: Arc<AuditState<C>>,
    connections: Arc<Semaphore>,
}

impl<C> AuditServer<C>
where
    C: Callback,
{
    fn new(listener: TcpListener, state: Arc<AuditState<C>>) -> Self {
        Self {
            listener,
            state,
            connections: Arc::new(Semaphore::new(AUDIT_MAX_CONNECTIONS)),
        }
    }

    async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted.context("sandbox audit accept failed")?;
                    let Ok(permit) = Arc::clone(&self.connections).try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    let state = Arc::clone(&self.state);
                    connections.spawn(async move {
                        let _permit = permit;
                        Self::handle(stream, state).await
                    });
                }
            }
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        Ok(())
    }

    async fn handle(stream: TcpStream, state: Arc<AuditState<C>>) -> Result<()> {
        Self::handle_with_timeouts(stream, state, AUDIT_HANDSHAKE_TIMEOUT, AUDIT_IDLE_TIMEOUT).await
    }

    async fn handle_with_timeouts(
        mut stream: TcpStream,
        state: Arc<AuditState<C>>,
        handshake_timeout: Duration,
        idle_timeout: Duration,
    ) -> Result<()> {
        let first = tokio::time::timeout(handshake_timeout, read_frame(&mut stream))
            .await
            .context("sandbox audit handshake timed out")??;
        let mut persistent = Self::publish_frame(&mut stream, &state, first).await?;
        loop {
            let frame = if persistent {
                match read_frame(&mut stream).await {
                    Ok(frame) => frame,
                    Err(error) if disconnected(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
            } else {
                match tokio::time::timeout(idle_timeout, read_frame(&mut stream)).await {
                    Ok(Ok(frame)) => frame,
                    Err(_) => anyhow::bail!("sandbox audit connection timed out"),
                    Ok(Err(error)) if disconnected(&error) => return Ok(()),
                    Ok(Err(error)) => return Err(error),
                }
            };
            persistent |= Self::publish_frame(&mut stream, &state, frame).await?;
        }
    }

    async fn publish_frame(
        stream: &mut TcpStream,
        state: &AuditState<C>,
        frame: Vec<u8>,
    ) -> Result<bool> {
        let request = decode_request(&frame)?;
        let authenticated = request.token == state.token;
        let ping = matches!(&request.event, AuditEventRequest::Ping);
        let response = if !authenticated {
            AuditResponse::Error {
                errno: libc::EACCES,
                message: "invalid audit token".to_string(),
            }
        } else if ping {
            AuditResponse::Accepted
        } else {
            state.publish_once(request.request_id, request.event).await
        };
        stream.write_all(&encode_response(&response)?).await?;
        Ok(authenticated && ping)
    }
}

fn disconnected(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
        )
    })
}

async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut prefix = [0_u8; 4];
    stream.read_exact(&mut prefix).await?;
    let mut frame = vec![0_u8; frame_length(prefix)?];
    stream.read_exact(&mut frame).await?;
    Ok(frame)
}

fn validate_trace(trace_id: String) -> Result<String> {
    TraceContext::parse(&trace_id)
        .map(|trace| trace.encode())
        .map_err(|error| anyhow::anyhow!("invalid audit trace id: {error}"))
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn started() -> EventResult {
    EventResult {
        status: EventStatus::Started,
        error_code: None,
        error_message: None,
    }
}

#[cfg(test)]
mod tests;
