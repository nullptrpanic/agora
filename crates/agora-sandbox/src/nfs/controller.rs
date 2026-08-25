use crate::nfs::backend::RemoteStorage;
use crate::nfs::broker::Broker;
use crate::nfs::protocol::{
    PROTOCOL_VERSION, Request, RequestEnvelope, Response, ResponseEnvelope,
};
use crate::nfs::transport;
use anyhow::{Context, Result};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use uuid::Uuid;

const REMOTE_MAX_CONNECTIONS: usize = 128;
const REMOTE_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const REMOTE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RemoteConnectionStatus {
    Connected { root: u32 },
    Unavailable { root: u32, errno: libc::c_int },
}

impl RemoteConnectionStatus {
    pub(crate) fn root(&self) -> u32 {
        match self {
            Self::Connected { root } | Self::Unavailable { root, .. } => *root,
        }
    }
}

pub(crate) enum RemoteControllerEvent {
    Connection(RemoteConnectionStatus),
    Failure(anyhow::Error),
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteRuntime {
    socket: PathBuf,
    token: String,
}

impl RemoteRuntime {
    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

pub(crate) struct RemoteController {
    runtime: RemoteRuntime,
    shutdown: watch::Sender<bool>,
    tasks: JoinSet<Result<()>>,
    connection_probes: JoinSet<RemoteConnectionStatus>,
}

impl RemoteController {
    pub(crate) async fn start_with_storage<S>(
        storage: Arc<S>,
        runtime_directory: &Path,
    ) -> Result<Self>
    where
        S: RemoteStorage,
    {
        std::fs::create_dir_all(runtime_directory).with_context(|| {
            format!(
                "failed to create remote filesystem runtime directory {}",
                runtime_directory.display()
            )
        })?;
        let socket = runtime_directory.join("nfs.sock");
        let listener = UnixListener::bind(&socket).with_context(|| {
            format!(
                "failed to bind remote filesystem broker {}",
                socket.display()
            )
        })?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .context("failed to secure remote filesystem broker socket")?;
        let broker = Arc::new(Broker::new(storage, runtime_directory)?);
        let token = Uuid::new_v4().simple().to_string();
        let state = Arc::new(RemoteState {
            token: token.clone(),
            broker,
        });
        let (shutdown, receiver) = watch::channel(false);
        let mut tasks = JoinSet::new();
        tasks.spawn(RemoteServer::new(listener, state).run(receiver));
        Ok(Self {
            runtime: RemoteRuntime { socket, token },
            shutdown,
            tasks,
            connection_probes: JoinSet::new(),
        })
    }

    pub(crate) async fn start_with_storage_and_connection_probes<S>(
        storage: Arc<S>,
        runtime_directory: &Path,
        preflight_errors: &[Option<libc::c_int>],
    ) -> Result<Self>
    where
        S: RemoteStorage,
    {
        let mut controller =
            Self::start_with_storage(Arc::clone(&storage), runtime_directory).await?;
        for (root, preflight_error) in preflight_errors.iter().copied().enumerate() {
            let root = u32::try_from(root).context("too many remote filesystem roots")?;
            let storage = Arc::clone(&storage);
            controller.connection_probes.spawn(async move {
                if let Some(errno) = preflight_error {
                    return RemoteConnectionStatus::Unavailable { root, errno };
                }
                match storage.connect(root).await {
                    Ok(()) => RemoteConnectionStatus::Connected { root },
                    Err(error) => RemoteConnectionStatus::Unavailable {
                        root,
                        errno: error.errno(),
                    },
                }
            });
        }
        Ok(controller)
    }

    pub(crate) fn runtime(&self) -> &RemoteRuntime {
        &self.runtime
    }

    pub(crate) async fn wait_event(&mut self) -> RemoteControllerEvent {
        tokio::select! {
            result = self.tasks.join_next() => RemoteControllerEvent::Failure(match result {
                Some(Ok(Ok(()))) => {
                    anyhow::anyhow!("remote filesystem broker stopped unexpectedly")
                }
                Some(Ok(Err(error))) => error.context("remote filesystem broker failed"),
                Some(Err(error)) => {
                    anyhow::Error::from(error).context("remote filesystem broker task failed")
                }
                None => anyhow::anyhow!("remote filesystem broker has no active task"),
            }),
            result = self.connection_probes.join_next(), if !self.connection_probes.is_empty() => {
                match result {
                    Some(Ok(status)) => RemoteControllerEvent::Connection(status),
                    Some(Err(error)) => RemoteControllerEvent::Failure(
                        anyhow::Error::from(error)
                            .context("remote filesystem connection probe task failed"),
                    ),
                    None => unreachable!("non-empty connection probe set returned no task"),
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn wait_failure(&mut self) -> anyhow::Error {
        loop {
            if let RemoteControllerEvent::Failure(error) = self.wait_event().await {
                return error;
            }
        }
    }

    pub(crate) async fn shutdown(mut self) -> Result<()> {
        let _ = self.shutdown.send(true);
        self.connection_probes.abort_all();
        while self.connection_probes.join_next().await.is_some() {}
        let mut first_error = None;
        while let Some(task) = self.tasks.join_next().await {
            match task {
                Ok(Ok(())) => {}
                Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
                Err(error) if first_error.is_none() => first_error = Some(error.into()),
                _ => {}
            }
        }
        let remove = std::fs::remove_file(&self.runtime.socket);
        if let Err(error) = remove
            && error.kind() != std::io::ErrorKind::NotFound
            && first_error.is_none()
        {
            first_error = Some(error.into());
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn abort_server_for_test(&mut self) {
        self.tasks.spawn(async {
            anyhow::bail!("injected remote filesystem failure");
        });
    }
}

impl Drop for RemoteController {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.tasks.abort_all();
        self.connection_probes.abort_all();
        let _ = std::fs::remove_file(&self.runtime.socket);
    }
}

struct RemoteState<S>
where
    S: RemoteStorage,
{
    token: String,
    broker: Arc<Broker<S>>,
}

struct RemoteServer<S>
where
    S: RemoteStorage,
{
    listener: UnixListener,
    state: Arc<RemoteState<S>>,
    connections: Arc<Semaphore>,
}

struct RemoteConnectionControl {
    stream: std::os::unix::net::UnixStream,
}

impl RemoteConnectionControl {
    fn close(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

impl<S> RemoteServer<S>
where
    S: RemoteStorage,
{
    fn new(listener: UnixListener, state: Arc<RemoteState<S>>) -> Self {
        Self {
            listener,
            state,
            connections: Arc::new(Semaphore::new(REMOTE_MAX_CONNECTIONS)),
        }
    }

    async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut connections = JoinSet::new();
        let mut controls = Vec::new();
        let mut expiry = tokio::time::interval(Duration::from_secs(30));
        expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {
                    controls.retain(|control: &Weak<RemoteConnectionControl>| {
                        control.strong_count() != 0
                    });
                }
                _ = expiry.tick() => self.state.broker.expire_requests().await,
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted.context("remote filesystem accept failed")?;
                    let Ok(permit) = Arc::clone(&self.connections).try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    let stream = stream.into_std()?;
                    configure_server_stream(
                        &stream,
                        REMOTE_REQUEST_TIMEOUT,
                        REMOTE_RESPONSE_TIMEOUT,
                    )?;
                    let control = Arc::new(RemoteConnectionControl {
                        stream: stream.try_clone()?,
                    });
                    controls.push(Arc::downgrade(&control));
                    let state = Arc::clone(&self.state);
                    connections.spawn(async move {
                        let _permit = permit;
                        let _control = control;
                        let _ = Self::handle(stream, state).await;
                    });
                }
            }
        }
        controls
            .iter()
            .filter_map(Weak::upgrade)
            .for_each(|control| control.close());
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        Ok(())
    }

    async fn handle(
        mut stream: std::os::unix::net::UnixStream,
        state: Arc<RemoteState<S>>,
    ) -> Result<()> {
        let mut persistent = false;
        loop {
            let (returned, received) = tokio::task::spawn_blocking(move || {
                let mut stream = stream;
                let result = transport::receive::<RequestEnvelope>(&mut stream);
                (stream, result)
            })
            .await
            .context("remote filesystem receive task failed")?;
            stream = returned;
            let (request, descriptor) = received?;
            let request_id = request.request_id.clone();
            let authenticated = request.version == PROTOCOL_VERSION
                && crate::ipc::constant_time_equal(
                    request.token.as_bytes(),
                    state.token.as_bytes(),
                );
            let expects_descriptor = matches!(&request.request, Request::Write { .. });
            let descriptor_valid = expects_descriptor == descriptor.is_some();
            let valid = authenticated && descriptor_valid;
            let ping = matches!(&request.request, Request::Ping);
            let reply = if !descriptor_valid {
                crate::nfs::broker::BrokerReply {
                    response: Response::Error {
                        errno: libc::EPROTO,
                        message: "remote request descriptor did not match operation".to_string(),
                    },
                    descriptor: None,
                }
            } else if request.version != PROTOCOL_VERSION {
                crate::nfs::broker::BrokerReply {
                    response: Response::Error {
                        errno: libc::EPROTO,
                        message: "unsupported remote filesystem protocol version".to_string(),
                    },
                    descriptor: None,
                }
            } else if !authenticated {
                crate::nfs::broker::BrokerReply {
                    response: Response::Error {
                        errno: libc::EACCES,
                        message: "invalid remote filesystem token".to_string(),
                    },
                    descriptor: None,
                }
            } else {
                state
                    .broker
                    .handle_request_with_descriptor(request_id.clone(), request.request, descriptor)
                    .await
            };
            let response = ResponseEnvelope {
                version: PROTOCOL_VERSION,
                request_id,
                response: reply.response,
            };
            let (returned, sent) = tokio::task::spawn_blocking(move || {
                let result = transport::send(
                    &mut stream,
                    &response,
                    reply.descriptor.as_ref().map(AsRawFd::as_raw_fd),
                );
                (stream, result)
            })
            .await
            .context("remote filesystem send task failed")?;
            stream = returned;
            sent?;
            if !persistent && ping && valid {
                stream.set_read_timeout(None)?;
                persistent = true;
            } else if !persistent || !valid {
                return Ok(());
            }
        }
    }
}

fn configure_server_stream(
    stream: &std::os::unix::net::UnixStream,
    read_timeout: Duration,
    write_timeout: Duration,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(write_timeout))
}

#[cfg(test)]
mod tests;
