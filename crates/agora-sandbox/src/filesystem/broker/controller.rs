use super::protocol::{
    PROTOCOL_VERSION, Request, RequestEnvelope, Response, ResponseEnvelope, valid_request_id,
};
use super::service::{LocalBroker, MAPPED_WRITEBACK_INTERVAL, WRITEBACK_DELAY};
use crate::filesystem::FileCipher;
use crate::ipc;
use anyhow::{Context, Result};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::net::UnixListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use uuid::Uuid;

const MAX_CONNECTIONS: usize = 128;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECTION_INITIAL: u8 = 0;
const CONNECTION_IDLE: u8 = 1;
const CONNECTION_BUSY: u8 = 2;
const CONNECTION_CLOSING: u8 = 3;

#[derive(Clone, Debug)]
pub(crate) struct LocalRuntime {
    socket: PathBuf,
    token: String,
}

impl LocalRuntime {
    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

pub(crate) struct LocalController {
    runtime: LocalRuntime,
    broker: Arc<LocalBroker>,
    shutdown: watch::Sender<bool>,
    tasks: JoinSet<Result<()>>,
}

impl LocalController {
    pub(crate) async fn start(
        root: &Path,
        cipher: FileCipher,
        runtime_directory: &Path,
    ) -> Result<Self> {
        std::fs::create_dir_all(runtime_directory)?;
        let socket = runtime_directory.join("local-filesystem.sock");
        let listener = UnixListener::bind(&socket).with_context(|| {
            format!(
                "failed to bind local filesystem broker {}",
                socket.display()
            )
        })?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let broker = Arc::new(LocalBroker::new_in(root, cipher, runtime_directory)?);
        let runtime = LocalRuntime {
            socket,
            token: Uuid::new_v4().simple().to_string(),
        };
        let state = Arc::new(ServerState {
            token: runtime.token.clone(),
            broker: Arc::clone(&broker),
        });
        let (shutdown, receiver) = watch::channel(false);
        let mut tasks = JoinSet::new();
        tasks.spawn(Server::new(listener, state).run(receiver));
        Ok(Self {
            runtime,
            broker,
            shutdown,
            tasks,
        })
    }

    pub(crate) fn runtime(&self) -> &LocalRuntime {
        &self.runtime
    }

    pub(crate) async fn wait_failure(&mut self) -> anyhow::Error {
        match self.tasks.join_next().await {
            Some(Ok(Ok(()))) => anyhow::anyhow!("local filesystem broker stopped unexpectedly"),
            Some(Ok(Err(error))) => error.context("local filesystem broker failed"),
            Some(Err(error)) => {
                anyhow::Error::from(error).context("local filesystem broker task failed")
            }
            None => anyhow::anyhow!("local filesystem broker has no active task"),
        }
    }

    pub(crate) async fn shutdown(mut self) -> Result<()> {
        let _ = self.shutdown.send(true);
        let mut first = None;
        while let Some(result) = self.tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) if first.is_none() => first = Some(error),
                Err(error) if first.is_none() => first = Some(error.into()),
                _ => {}
            }
        }
        let broker = Arc::clone(&self.broker);
        tokio::task::spawn_blocking(move || broker.flush_all())
            .await
            .context("local filesystem final flush task failed")??;
        let _ = std::fs::remove_file(&self.runtime.socket);
        match first {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for LocalController {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.tasks.abort_all();
        let _ = std::fs::remove_file(&self.runtime.socket);
    }
}

struct ServerState {
    token: String,
    broker: Arc<LocalBroker>,
}

struct Server {
    listener: UnixListener,
    state: Arc<ServerState>,
    connections: Arc<Semaphore>,
    attached_tx: mpsc::Sender<AttachedConnection>,
    attached_rx: mpsc::Receiver<AttachedConnection>,
}

struct AttachedConnection {
    stream: std::os::unix::net::UnixStream,
    permit: OwnedSemaphorePermit,
}

struct ConnectionControl {
    stream: std::os::unix::net::UnixStream,
    state: AtomicU8,
}

enum ConnectionMode {
    Initial,
    Shared,
    WriteLease(WriteLease),
}

#[derive(Clone)]
struct WriteLease {
    handle: String,
    write_id: String,
}

impl ConnectionControl {
    fn close_if_idle(&self) {
        if self
            .state
            .compare_exchange(
                CONNECTION_IDLE,
                CONNECTION_CLOSING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            let _ = self.stream.shutdown(Shutdown::Both);
        }
    }

    fn begin_request(&self) -> bool {
        self.state
            .compare_exchange(
                CONNECTION_IDLE,
                CONNECTION_BUSY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn finish_request(&self, shutdown_requested: &AtomicBool) -> bool {
        self.state.store(CONNECTION_IDLE, Ordering::Release);
        shutdown_requested.load(Ordering::Acquire)
    }
}

impl Server {
    fn new(listener: UnixListener, state: Arc<ServerState>) -> Self {
        let (attached_tx, attached_rx) = mpsc::channel(MAX_CONNECTIONS);
        Self {
            listener,
            state,
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            attached_tx,
            attached_rx,
        }
    }

    async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut tasks = JoinSet::new();
        let mut controls = Vec::new();
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let mut writebacks = JoinSet::new();
        let mut expiry = tokio::time::interval(Duration::from_secs(30));
        expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut writeback = tokio::time::interval(WRITEBACK_DELAY);
        writeback.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut mapped_writeback = tokio::time::interval_at(
            tokio::time::Instant::now() + MAPPED_WRITEBACK_INTERVAL,
            MAPPED_WRITEBACK_INTERVAL,
        );
        mapped_writeback.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        shutdown_requested.store(true, Ordering::Release);
                        break;
                    }
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {
                    controls.retain(|control: &Weak<ConnectionControl>| control.strong_count() != 0);
                }
                Some(result) = writebacks.join_next(), if !writebacks.is_empty() => {
                    result
                        .context("local filesystem writeback task failed")??;
                }
                _ = expiry.tick() => {
                    self.state.broker.expire_closed();
                    self.state.broker.expire_requests();
                },
                _ = writeback.tick(), if writebacks.is_empty() => {
                    if self.state.broker.writeback_pending() {
                        let broker = Arc::clone(&self.state.broker);
                        writebacks.spawn_blocking(move || broker.flush_due(Instant::now()));
                    }
                },
                _ = mapped_writeback.tick(), if writebacks.is_empty() => {
                    let broker = Arc::clone(&self.state.broker);
                    writebacks.spawn_blocking(move || broker.flush_mapped_changed());
                },
                Some(attached_connection) = self.attached_rx.recv() => {
                    let AttachedConnection { stream, permit } = attached_connection;
                    self.spawn_connection(
                        &mut tasks,
                        &mut controls,
                        stream,
                        permit,
                        Arc::clone(&shutdown_requested),
                    )?;
                }
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted?;
                    let Ok(permit) = Arc::clone(&self.connections).try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    let stream = stream.into_std()?;
                    stream.set_nonblocking(false)?;
                    self.spawn_connection(
                        &mut tasks,
                        &mut controls,
                        stream,
                        permit,
                        Arc::clone(&shutdown_requested),
                    )?;
                }
            }
        }
        self.attached_rx.close();
        while self.attached_rx.try_recv().is_ok() {}
        controls
            .iter()
            .filter_map(Weak::upgrade)
            .for_each(|control| control.close_if_idle());
        while tasks.join_next().await.is_some() {}
        while let Some(result) = writebacks.join_next().await {
            result.context("local filesystem writeback task failed")??;
        }
        Ok(())
    }

    fn spawn_connection(
        &self,
        tasks: &mut JoinSet<()>,
        controls: &mut Vec<Weak<ConnectionControl>>,
        stream: std::os::unix::net::UnixStream,
        permit: OwnedSemaphorePermit,
        shutdown_requested: Arc<AtomicBool>,
    ) -> Result<()> {
        let control = Arc::new(ConnectionControl {
            stream: stream.try_clone()?,
            state: AtomicU8::new(CONNECTION_INITIAL),
        });
        controls.push(Arc::downgrade(&control));
        let state = Arc::clone(&self.state);
        let connections = Arc::clone(&self.connections);
        let attached = self.attached_tx.clone();
        tasks.spawn(async move {
            let _permit = permit;
            let _ = Self::handle(
                stream,
                state,
                control,
                shutdown_requested,
                connections,
                attached,
            )
            .await;
        });
        Ok(())
    }

    async fn handle(
        stream: std::os::unix::net::UnixStream,
        state: Arc<ServerState>,
        control: Arc<ConnectionControl>,
        shutdown_requested: Arc<AtomicBool>,
        connections: Arc<Semaphore>,
        attached: mpsc::Sender<AttachedConnection>,
    ) -> Result<()> {
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(RESPONSE_TIMEOUT))?;
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut stream = stream;
            let mut mode = ConnectionMode::Initial;
            let mut active_lease = None;
            let result = (|| {
                loop {
                    let (request, descriptor) = ipc::receive::<RequestEnvelope>(&mut stream)?;
                    if !matches!(&mode, ConnectionMode::Initial) && !control.begin_request() {
                        return Ok(());
                    }
                    let authenticated = request.version == PROTOCOL_VERSION
                        && ipc::constant_time_equal(
                            request.token.as_bytes(),
                            state.token.as_bytes(),
                        );
                    let valid_id = valid_request_id(&request.request_id)
                        && !matches!(
                            &request.request,
                            Request::Claim { request_id } if !valid_request_id(request_id)
                        )
                        && !matches!(
                            &request.request,
                            Request::BeginWrite { write_id, .. }
                                | Request::BeginAppend { write_id, .. }
                                | Request::FinishWrite { write_id, .. }
                                | Request::CancelWrite { write_id, .. }
                                if !valid_request_id(write_id)
                        );
                    let descriptor_free = descriptor.is_none();
                    let ping = matches!(&request.request, Request::Ping) && descriptor_free;
                    let requested_lease = match &request.request {
                        Request::BeginWrite {
                            handle, write_id, ..
                        }
                        | Request::BeginAppend { handle, write_id } => Some(WriteLease {
                            handle: handle.clone(),
                            write_id: write_id.clone(),
                        }),
                        _ => None,
                    };
                    let begin_write = matches!(&request.request, Request::BeginWrite { .. });
                    let begin_append = matches!(&request.request, Request::BeginAppend { .. });
                    let attach_write_lease = matches!(&request.request, Request::AttachWriteLease);
                    let valid_for_connection = match &mode {
                        ConnectionMode::Initial => !matches!(
                            &request.request,
                            Request::AttachWriteLease
                                | Request::FinishWrite { .. }
                                | Request::CancelWrite { .. }
                        ),
                        ConnectionMode::Shared => !matches!(
                            &request.request,
                            Request::BeginWrite { .. }
                                | Request::BeginAppend { .. }
                                | Request::FinishWrite { .. }
                                | Request::CancelWrite { .. }
                        ),
                        ConnectionMode::WriteLease(lease) => matches!(
                            &request.request,
                            Request::FinishWrite {
                                handle,
                                write_id,
                                ..
                            } | Request::CancelWrite { handle, write_id }
                                if handle == &lease.handle && write_id == &lease.write_id
                        ),
                    };
                    let reply = if request.version != PROTOCOL_VERSION {
                        super::service::BrokerReply::error(
                            libc::EPROTO,
                            "unsupported local filesystem protocol version",
                        )
                    } else if !authenticated {
                        super::service::BrokerReply::error(
                            libc::EACCES,
                            "invalid local filesystem token",
                        )
                    } else if !valid_id {
                        super::service::BrokerReply::error(
                            libc::EPROTO,
                            "invalid local filesystem request ID",
                        )
                    } else if !valid_for_connection {
                        super::service::BrokerReply::error(
                            libc::EPROTO,
                            "local filesystem write lifecycle used the wrong connection",
                        )
                    } else if attach_write_lease {
                        Self::attach_write_connection(
                            descriptor,
                            Arc::clone(&connections),
                            &attached,
                        )
                    } else if begin_write || begin_append {
                        state.broker.handle_uncached(request.request, descriptor)
                    } else {
                        state.broker.handle_request(
                            request.request_id.clone(),
                            request.request,
                            descriptor,
                        )
                    };
                    let promote_shared = matches!(&mode, ConnectionMode::Initial)
                        && ping
                        && authenticated
                        && valid_id
                        && matches!(&reply.response, Response::Success);
                    let promote_lease = if matches!(&mode, ConnectionMode::Initial)
                        && authenticated
                        && valid_id
                        && descriptor_free
                        && ((begin_write && matches!(&reply.response, Response::Success))
                            || (begin_append && matches!(&reply.response, Response::Offset { .. })))
                    {
                        requested_lease
                    } else {
                        None
                    };
                    if let Some(lease) = &promote_lease {
                        active_lease = Some(lease.clone());
                    }
                    let descriptors = reply
                        .descriptors
                        .iter()
                        .map(AsRawFd::as_raw_fd)
                        .collect::<Vec<_>>();
                    ipc::send_with_descriptors(
                        &mut stream,
                        &ResponseEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: request.request_id,
                            response: reply.response,
                        },
                        &descriptors,
                    )?;
                    match &mode {
                        ConnectionMode::Initial if promote_shared => {
                            stream.set_read_timeout(None)?;
                            mode = ConnectionMode::Shared;
                        }
                        ConnectionMode::Initial if promote_lease.is_some() => {
                            stream.set_read_timeout(None)?;
                            mode = ConnectionMode::WriteLease(
                                promote_lease.expect("write lease promotion was checked"),
                            );
                        }
                        ConnectionMode::Initial => return Ok(()),
                        ConnectionMode::Shared if !authenticated || !valid_id => return Ok(()),
                        ConnectionMode::Shared => {}
                        ConnectionMode::WriteLease(_) => return Ok(()),
                    }
                    if control.finish_request(&shutdown_requested) {
                        return Ok(());
                    }
                }
            })();
            if let Some(lease) = active_lease {
                state.broker.abandon_write(&lease.handle, &lease.write_id);
            }
            result
        })
        .await
        .context("local filesystem request task failed")??;
        Ok(())
    }

    fn attach_write_connection(
        descriptor: Option<OwnedFd>,
        connections: Arc<Semaphore>,
        attached: &mpsc::Sender<AttachedConnection>,
    ) -> super::service::BrokerReply {
        let result = (|| {
            let descriptor = descriptor.ok_or((
                libc::EPROTO,
                "write lease attachment did not include a Unix stream".to_string(),
            ))?;
            let stream = std::os::unix::net::UnixStream::from(descriptor);
            stream.peer_addr().map_err(|error| {
                (
                    libc::EPROTO,
                    format!("write lease attachment is not a Unix stream: {error}"),
                )
            })?;
            stream.set_nonblocking(false).map_err(|error| {
                (
                    error.raw_os_error().unwrap_or(libc::EIO),
                    format!("failed to configure attached write lease: {error}"),
                )
            })?;
            let permit = connections.try_acquire_owned().map_err(|_| {
                (
                    libc::EAGAIN,
                    "local filesystem connection limit is busy".to_string(),
                )
            })?;
            attached
                .blocking_send(AttachedConnection { stream, permit })
                .map_err(|_| {
                    (
                        libc::EPIPE,
                        "local filesystem broker stopped accepting write leases".to_string(),
                    )
                })
        })();
        match result {
            Ok(()) => super::service::BrokerReply::response(Response::Success),
            Err((errno, message)) => super::service::BrokerReply::error(errno, message),
        }
    }
}

#[cfg(test)]
#[path = "controller/tests.rs"]
mod tests;
