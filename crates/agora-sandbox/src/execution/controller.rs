use super::protocol::{
    ExecutionRequest, PrepareResponse, decode_request, encode_prepare_response, frame_length,
};
use super::store::ExecutableStore;
use crate::filesystem::FileCipher;
use anyhow::{Context, Result};
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use uuid::Uuid;

const EXECUTION_MAX_CONNECTIONS: usize = 64;
const EXECUTION_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub(crate) struct ExecutionRuntime {
    token: String,
    control: SocketAddr,
}

impl ExecutionRuntime {
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn control(&self) -> SocketAddr {
        self.control
    }
}

pub(crate) struct ExecutionController {
    runtime: ExecutionRuntime,
    store: Arc<Mutex<ExecutableStore>>,
    shutdown: watch::Sender<bool>,
    tasks: JoinSet<Result<()>>,
}

impl ExecutionController {
    pub(crate) async fn start(directory: PathBuf) -> Result<Self> {
        Self::start_with_cipher(directory, None).await
    }

    pub(crate) async fn start_encrypted(directory: PathBuf, cipher: FileCipher) -> Result<Self> {
        Self::start_with_cipher(directory, Some(cipher)).await
    }

    async fn start_with_cipher(directory: PathBuf, cipher: Option<FileCipher>) -> Result<Self> {
        let store = Arc::new(Mutex::new(match cipher {
            Some(cipher) => ExecutableStore::encrypted(directory, cipher)?,
            None => ExecutableStore::new(directory)?,
        }));
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .context("failed to bind sandbox execution controller")?;
        let control = listener.local_addr()?;
        let token = Uuid::new_v4().simple().to_string();
        let (shutdown, receiver) = watch::channel(false);
        let state = Arc::new(ExecutionState {
            token: token.clone(),
            store: Arc::clone(&store),
        });
        let mut tasks = JoinSet::new();
        tasks.spawn(ExecutionServer::new(listener, state).run(receiver));
        Ok(Self {
            runtime: ExecutionRuntime { token, control },
            store,
            shutdown,
            tasks,
        })
    }

    pub(crate) fn runtime(&self) -> &ExecutionRuntime {
        &self.runtime
    }

    pub(crate) async fn prepare(&self, executable: PathBuf) -> Result<PathBuf> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || lock(&store).prepare(&executable))
            .await
            .context("sandbox executable preparation task failed")?
    }

    pub(crate) async fn wait_failure(&mut self) -> anyhow::Error {
        match self.tasks.join_next().await {
            Some(Ok(Ok(()))) => {
                anyhow::anyhow!("sandbox execution controller stopped unexpectedly")
            }
            Some(Ok(Err(error))) => error.context("sandbox execution controller failed"),
            Some(Err(error)) => anyhow::Error::from(error).context("sandbox execution task failed"),
            None => anyhow::anyhow!("sandbox execution controller has no active task"),
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
    pub(crate) fn abort_server_for_test(&mut self) {
        self.tasks.spawn(async {
            anyhow::bail!("injected execution controller failure");
        });
    }

    #[cfg(test)]
    pub(crate) fn stop_server_for_test(&self) {
        let _ = self.shutdown.send(true);
    }

    #[cfg(test)]
    pub(crate) fn abort_tasks_for_test(&mut self) {
        self.tasks.abort_all();
    }
}

impl Drop for ExecutionController {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.tasks.abort_all();
    }
}

struct ExecutionState {
    token: String,
    store: Arc<Mutex<ExecutableStore>>,
}

struct ExecutionServer {
    listener: TcpListener,
    state: Arc<ExecutionState>,
    connections: Arc<Semaphore>,
}

impl ExecutionServer {
    fn new(listener: TcpListener, state: Arc<ExecutionState>) -> Self {
        Self {
            listener,
            state,
            connections: Arc::new(Semaphore::new(EXECUTION_MAX_CONNECTIONS)),
        }
    }

    async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted.context("sandbox execution accept failed")?;
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

    async fn handle(mut stream: TcpStream, state: Arc<ExecutionState>) -> Result<()> {
        let frame =
            tokio::time::timeout(EXECUTION_HANDSHAKE_TIMEOUT, Self::read_frame(&mut stream))
                .await
                .context("sandbox execution handshake timed out")??;
        let (ping, authenticated) = Self::respond(&mut stream, &state, frame).await?;
        if !ping || !authenticated {
            stream.shutdown().await?;
            return Ok(());
        }
        loop {
            let frame = match Self::read_frame(&mut stream).await {
                Ok(frame) => frame,
                Err(error) if disconnected(&error) => return Ok(()),
                Err(error) => return Err(error),
            };
            let (_, authenticated) = Self::respond(&mut stream, &state, frame).await?;
            if !authenticated {
                stream.shutdown().await?;
                return Ok(());
            }
        }
    }

    async fn respond(
        stream: &mut TcpStream,
        state: &ExecutionState,
        frame: Vec<u8>,
    ) -> Result<(bool, bool)> {
        let request = decode_request(&frame)?;
        let ping = matches!(&request, ExecutionRequest::Ping { .. });
        let token = match &request {
            ExecutionRequest::Ping { token } => token,
            ExecutionRequest::Prepare(request) => &request.token,
        };
        let authenticated = token == &state.token;
        let response = if !authenticated {
            PrepareResponse::Error {
                errno: libc::EACCES,
                message: "invalid execution token".to_string(),
            }
        } else {
            match request {
                ExecutionRequest::Ping { .. } => PrepareResponse::Accepted,
                ExecutionRequest::Prepare(request) => {
                    let store = Arc::clone(&state.store);
                    let executable = request.executable;
                    match tokio::task::spawn_blocking(move || lock(&store).prepare(&executable))
                        .await
                    {
                        Ok(Ok(path)) => PrepareResponse::Ready(path),
                        Ok(Err(error)) => PrepareResponse::Error {
                            errno: preparation_errno(&error),
                            message: format!("{error:#}"),
                        },
                        Err(error) => PrepareResponse::Error {
                            errno: libc::EIO,
                            message: format!("sandbox executable preparation task failed: {error}"),
                        },
                    }
                }
            }
        };
        stream
            .write_all(&encode_prepare_response(&response)?)
            .await?;
        Ok((ping, authenticated))
    }

    async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
        let mut prefix = [0_u8; 4];
        stream.read_exact(&mut prefix).await?;
        let mut frame = vec![0_u8; frame_length(prefix)?];
        stream.read_exact(&mut frame).await?;
        Ok(frame)
    }
}

fn disconnected(error: &anyhow::Error) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::UnexpectedEof
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::BrokenPipe
        )
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn preparation_errno(error: &anyhow::Error) -> i32 {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<io::Error>()?.raw_os_error())
        .unwrap_or(libc::EIO)
}
