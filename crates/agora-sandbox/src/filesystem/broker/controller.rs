use super::protocol::{
    PROTOCOL_VERSION, Request, RequestEnvelope, Response, ResponseEnvelope, valid_request_id,
};
use super::service::{LocalBroker, WRITEBACK_DELAY};
use crate::filesystem::FileCipher;
use crate::ipc;
use anyhow::{Context, Result};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::net::UnixListener;
use tokio::sync::{Semaphore, watch};
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
}

struct ConnectionControl {
    stream: std::os::unix::net::UnixStream,
    state: AtomicU8,
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
        Self {
            listener,
            state,
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        }
    }

    async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut tasks = JoinSet::new();
        let mut controls = Vec::new();
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let mut writebacks = JoinSet::new();
        let mut expiry = tokio::time::interval(Duration::from_secs(30));
        expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut writeback = tokio::time::interval(WRITEBACK_DELAY);
        writeback.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted?;
                    let Ok(permit) = Arc::clone(&self.connections).try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    let stream = stream.into_std()?;
                    stream.set_nonblocking(false)?;
                    let control = Arc::new(ConnectionControl {
                        stream: stream.try_clone()?,
                        state: AtomicU8::new(CONNECTION_INITIAL),
                    });
                    controls.push(Arc::downgrade(&control));
                    let state = Arc::clone(&self.state);
                    let shutdown_requested = Arc::clone(&shutdown_requested);
                    tasks.spawn(async move {
                        let _permit = permit;
                        let _ = Self::handle(stream, state, control, shutdown_requested).await;
                    });
                }
            }
        }
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

    async fn handle(
        stream: std::os::unix::net::UnixStream,
        state: Arc<ServerState>,
        control: Arc<ConnectionControl>,
        shutdown_requested: Arc<AtomicBool>,
    ) -> Result<()> {
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(RESPONSE_TIMEOUT))?;
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut stream = stream;
            let mut persistent = false;
            loop {
                let (request, descriptor) = ipc::receive::<RequestEnvelope>(&mut stream)?;
                if persistent && !control.begin_request() {
                    return Ok(());
                }
                let authenticated = request.version == PROTOCOL_VERSION
                    && constant_time_equal(request.token.as_bytes(), state.token.as_bytes());
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
                let ping = matches!(&request.request, Request::Ping) && descriptor.is_none();
                let reply = if request.version != PROTOCOL_VERSION {
                    super::service::BrokerReply {
                        response: Response::Error {
                            errno: libc::EPROTO,
                            message: "unsupported local filesystem protocol version".to_string(),
                        },
                        descriptors: Vec::new(),
                    }
                } else if !authenticated {
                    super::service::BrokerReply {
                        response: Response::Error {
                            errno: libc::EACCES,
                            message: "invalid local filesystem token".to_string(),
                        },
                        descriptors: Vec::new(),
                    }
                } else if !valid_id {
                    super::service::BrokerReply {
                        response: Response::Error {
                            errno: libc::EPROTO,
                            message: "invalid local filesystem request ID".to_string(),
                        },
                        descriptors: Vec::new(),
                    }
                } else {
                    state.broker.handle_request(
                        request.request_id.clone(),
                        request.request,
                        descriptor,
                    )
                };
                let descriptors = reply
                    .descriptors
                    .iter()
                    .map(AsRawFd::as_raw_fd)
                    .collect::<Vec<_>>();
                let promote = !persistent && ping && authenticated && valid_id;
                ipc::send_with_descriptors(
                    &mut stream,
                    &ResponseEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: request.request_id,
                        response: reply.response,
                    },
                    &descriptors,
                )?;
                if promote {
                    stream.set_read_timeout(None)?;
                    persistent = true;
                } else if !persistent || !authenticated || !valid_id {
                    return Ok(());
                }
                if control.finish_request(&shutdown_requested) {
                    return Ok(());
                }
            }
        })
        .await
        .context("local filesystem request task failed")??;
        Ok(())
    }
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
#[path = "controller/tests.rs"]
mod tests;
