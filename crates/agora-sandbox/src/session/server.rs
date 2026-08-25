use super::protocol::{
    ClientMessage, PROTOCOL_VERSION, ServerMessage, WirePreparedLaunch, read_frame, write_frame,
};
use super::startup::{DaemonStartup, SessionPaths, build_identity};
use crate::callback::Callback;
use crate::runner::{SandboxConfig, SandboxRuntime};
use anyhow::{Context, Result, anyhow, bail};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;

const MAX_CONNECTIONS: usize = 128;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const LAUNCH_STATE_TIMEOUT: Duration = Duration::from_secs(30);
const FIRST_CLIENT_TIMEOUT: Duration = Duration::from_secs(60);

struct PrepareRequest {
    executable: PathBuf,
    response: oneshot::Sender<std::result::Result<WirePreparedLaunch, String>>,
}

#[derive(Clone)]
struct ConnectionContext {
    build_identity: Arc<str>,
    config_identity: Arc<str>,
    sandbox_id: Arc<str>,
    run_id: Arc<str>,
    prepare: mpsc::Sender<PrepareRequest>,
    events: mpsc::Sender<SessionEvent>,
    failure: watch::Receiver<Option<String>>,
}

enum SessionEvent {
    Joined,
    BuildMismatch {
        response: oneshot::Sender<bool>,
    },
    Releasing {
        response: Option<oneshot::Sender<std::result::Result<(), String>>>,
    },
}

pub async fn serve<C>(
    config: SandboxConfig,
    callback: C,
    config_identity: String,
    mut startup: DaemonStartup,
) -> Result<()>
where
    C: Callback,
{
    let (paths, build_identity) = match SessionPaths::resolve(config.workdir())
        .and_then(|paths| Ok((paths, build_identity()?)))
    {
        Ok(values) => values,
        Err(error) => {
            let _ = startup.failed(&error).await;
            return Err(error);
        }
    };
    let mut runtime = match SandboxRuntime::start(config, callback).await {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = startup.failed(&error).await;
            return Err(error);
        }
    };
    let listener = match UnixListener::bind(paths.socket()) {
        Ok(listener) => listener,
        Err(error) => {
            let error = anyhow::Error::from(error).context(format!(
                "failed to bind sandbox session {}",
                paths.socket().display()
            ));
            let _ = startup.failed(&error).await;
            let _ = runtime.shutdown().await;
            return Err(error);
        }
    };
    let socket = SocketGuard(paths.socket().to_path_buf());
    if let Err(error) =
        std::fs::set_permissions(paths.socket(), std::fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "failed to secure session socket {}",
                    paths.socket().display()
                )
            })
    {
        let _ = startup.failed(&error).await;
        let _ = runtime.shutdown().await;
        return Err(error);
    }
    if let Err(error) = startup.ready().await {
        let _ = runtime.shutdown().await;
        return Err(error);
    }

    let sandbox_id: Arc<str> = Arc::from(runtime.sandbox_id());
    let run_id: Arc<str> = Arc::from(runtime.run_id());
    let config_identity: Arc<str> = Arc::from(config_identity);
    let build_identity: Arc<str> = Arc::from(build_identity);
    let (prepare_sender, mut prepare_receiver) = mpsc::channel::<PrepareRequest>(MAX_CONNECTIONS);
    let (event_sender, mut event_receiver) = mpsc::channel::<SessionEvent>(MAX_CONNECTIONS);
    let (failure_sender, failure_receiver) = watch::channel::<Option<String>>(None);
    let connection_context = ConnectionContext {
        build_identity,
        config_identity,
        sandbox_id,
        run_id,
        prepare: prepare_sender.clone(),
        events: event_sender.clone(),
        failure: failure_receiver,
    };
    let mut connections = JoinSet::new();
    let first_client = tokio::time::sleep(FIRST_CLIENT_TIMEOUT);
    tokio::pin!(first_client);
    let mut saw_client = false;
    let mut active_leases = 0_usize;
    let mut runtime_failure = None;
    let mut final_release = None;

    loop {
        enum Completion {
            Accepted(std::io::Result<(UnixStream, tokio::net::unix::SocketAddr)>),
            Connection(Option<std::result::Result<Result<()>, tokio::task::JoinError>>),
            Prepare(Option<PrepareRequest>),
            Event(Option<SessionEvent>),
            Runtime(anyhow::Error),
            FirstClientTimeout,
        }
        let completion = tokio::select! {
            request = prepare_receiver.recv() => Completion::Prepare(request),
            event = event_receiver.recv() => Completion::Event(event),
            result = connections.join_next(), if !connections.is_empty() => {
                Completion::Connection(result)
            }
            error = runtime.wait_failure() => Completion::Runtime(error),
            accepted = listener.accept() => Completion::Accepted(accepted),
            _ = &mut first_client, if !saw_client => Completion::FirstClientTimeout,
        };
        match completion {
            Completion::Accepted(Ok((stream, _))) => {
                if connections.len() >= MAX_CONNECTIONS
                    || peer_effective_uid(&stream)? != current_uid()
                {
                    drop(stream);
                    continue;
                }
                connections.spawn(handle_connection(stream, connection_context.clone()));
            }
            Completion::Accepted(Err(error)) => {
                runtime_failure =
                    Some(anyhow::Error::from(error).context("sandbox session accept failed"));
                break;
            }
            Completion::Prepare(Some(request)) => {
                let result = runtime
                    .prepare(request.executable)
                    .await
                    .map(|launch| WirePreparedLaunch::from(&launch))
                    .map_err(|error| format!("{error:#}"));
                let _ = request.response.send(result);
            }
            Completion::Prepare(None) => {
                runtime_failure = Some(anyhow!("sandbox session prepare channel closed"));
                break;
            }
            Completion::Event(Some(SessionEvent::Joined)) => {
                saw_client = true;
                active_leases = active_leases.saturating_add(1);
            }
            Completion::Event(Some(SessionEvent::BuildMismatch { response })) => {
                let retiring = !saw_client && active_leases == 0;
                let _ = response.send(retiring);
                if retiring {
                    break;
                }
            }
            Completion::Event(Some(SessionEvent::Releasing { response })) => {
                if active_leases == 0 {
                    runtime_failure = Some(anyhow!("sandbox session released an inactive lease"));
                    break;
                }
                active_leases -= 1;
                if active_leases == 0 {
                    final_release = response;
                    break;
                }
                if let Some(response) = response {
                    let _ = response.send(Ok(()));
                }
            }
            Completion::Event(None) => {
                runtime_failure = Some(anyhow!("sandbox session event channel closed"));
                break;
            }
            Completion::Connection(Some(Ok(Ok(())))) => {
                if saw_client && connections.is_empty() {
                    break;
                }
            }
            Completion::Connection(Some(Ok(Err(error)))) => {
                agora_core::logger::error!("sandbox session client failed: {error:#}");
                if saw_client && connections.is_empty() {
                    break;
                }
            }
            Completion::Connection(Some(Err(error))) => {
                agora_core::logger::error!("sandbox session client task failed: {error}");
                if saw_client && connections.is_empty() {
                    break;
                }
            }
            Completion::Connection(None) => unreachable!("non-empty session task set was empty"),
            Completion::Runtime(error) => {
                let message = format!("{error:#}");
                let _ = failure_sender.send(Some(message));
                runtime_failure = Some(error);
                break;
            }
            Completion::FirstClientTimeout => {
                runtime_failure = Some(anyhow!("sandbox session received no initial client"));
                break;
            }
        }
    }

    drop(listener);
    drop(socket);
    drop(prepare_sender);
    drop(event_sender);
    if runtime_failure.is_some() {
        let deadline = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => {
                    connections.abort_all();
                    break;
                }
                result = connections.join_next(), if !connections.is_empty() => {
                    if result.is_none() || connections.is_empty() {
                        break;
                    }
                }
                else => break,
            }
        }
    }
    let shutdown = runtime.shutdown().await;
    if let Some(response) = final_release {
        let result = match &shutdown {
            Ok(()) => Ok(()),
            Err(error) => Err(format!("{error:#}")),
        };
        let _ = response.send(result);
    }
    if runtime_failure.is_none() {
        while connections.join_next().await.is_some() {}
    } else {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    match runtime_failure {
        Some(error) => {
            let _ = shutdown;
            Err(error)
        }
        None => shutdown,
    }
}

async fn handle_connection(mut stream: UnixStream, context: ConnectionContext) -> Result<()> {
    let ConnectionContext {
        build_identity,
        config_identity,
        sandbox_id,
        run_id,
        prepare,
        events,
        mut failure,
    } = context;
    let join = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        read_frame::<_, ClientMessage>(&mut stream),
    )
    .await
    .context("sandbox session join timed out")??;
    let ClientMessage::Join {
        protocol,
        build,
        config,
    } = join
    else {
        return reject(&mut stream, "sandbox session expected join").await;
    };
    if protocol != PROTOCOL_VERSION {
        return reject(&mut stream, "sandbox session protocol mismatch").await;
    }
    if build != build_identity.as_ref() {
        let (response, retiring) = oneshot::channel();
        events
            .send(SessionEvent::BuildMismatch { response })
            .await
            .context("sandbox session owner stopped before build mismatch decision")?;
        if retiring
            .await
            .context("sandbox session build mismatch decision was cancelled")?
        {
            return retire(&mut stream, "sandbox session build mismatch").await;
        }
        return reject(&mut stream, "sandbox session build mismatch").await;
    }
    if config != config_identity.as_ref() {
        return reject(&mut stream, "sandbox session configuration mismatch").await;
    }
    events
        .send(SessionEvent::Joined)
        .await
        .context("sandbox session owner stopped")?;
    write_frame(
        &mut stream,
        &ServerMessage::Joined {
            sandbox_id: sandbox_id.to_string(),
            run_id: run_id.to_string(),
        },
    )
    .await?;

    let result = handle_joined_connection(&mut stream, &prepare, &events, &mut failure).await;
    match result {
        Ok(ConnectionCompletion::Released | ConnectionCompletion::RuntimeFailed) => Ok(()),
        Ok(ConnectionCompletion::Abandoned) => {
            abandon(&events).await?;
            Ok(())
        }
        Err(error) => {
            let _ = abandon(&events).await;
            Err(error)
        }
    }
}

enum ConnectionCompletion {
    Released,
    Abandoned,
    RuntimeFailed,
}

async fn handle_joined_connection(
    stream: &mut UnixStream,
    prepare: &mpsc::Sender<PrepareRequest>,
    events: &mpsc::Sender<SessionEvent>,
    failure: &mut watch::Receiver<Option<String>>,
) -> Result<ConnectionCompletion> {
    let message = read_launch_state(stream, "prepare").await?;
    let ClientMessage::Prepare { executable } = message else {
        reject(stream, "sandbox session expected prepare").await?;
        return Ok(ConnectionCompletion::Abandoned);
    };
    let executable = PathBuf::from(executable.into_os_string());
    if !executable.is_absolute() {
        reject(stream, "sandbox session executable is not absolute").await?;
        return Ok(ConnectionCompletion::Abandoned);
    }
    let (response, receiver) = oneshot::channel();
    prepare
        .send(PrepareRequest {
            executable,
            response,
        })
        .await
        .context("sandbox session owner stopped")?;
    let prepared = receiver
        .await
        .context("sandbox session prepare response was dropped")?;
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            reject(stream, &message).await?;
            return Ok(ConnectionCompletion::Abandoned);
        }
    };
    let launch_id = prepared.launch_id().to_owned();
    write_frame(stream, &ServerMessage::Prepared { launch: prepared }).await?;

    loop {
        tokio::select! {
            changed = failure.changed() => {
                if changed.is_err() {
                    bail!("sandbox session runtime monitor stopped");
                }
                let message = failure.borrow().clone();
                if let Some(message) = message {
                    write_frame(stream, &ServerMessage::RuntimeFailed { message }).await?;
                    return Ok(ConnectionCompletion::RuntimeFailed);
                }
            }
            message = read_frame::<_, ClientMessage>(stream) => {
                match message {
                    Ok(ClientMessage::Finished { launch_id: finished, .. })
                        if finished == launch_id =>
                    {
                        release(events, stream).await?;
                        return Ok(ConnectionCompletion::Released);
                    }
                    Ok(ClientMessage::Cancel { launch_id: cancelled })
                        if cancelled == launch_id =>
                    {
                        release(events, stream).await?;
                        return Ok(ConnectionCompletion::Released);
                    }
                    Ok(_) => {
                        reject(stream, "invalid sandbox session finish state").await?;
                        return Ok(ConnectionCompletion::Abandoned);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                        return Ok(ConnectionCompletion::Abandoned);
                    }
                    Err(error) => return Err(error).context("failed to read sandbox session state"),
                }
            }
        }
    }
}

async fn read_launch_state(stream: &mut UnixStream, state: &str) -> Result<ClientMessage> {
    tokio::time::timeout(LAUNCH_STATE_TIMEOUT, read_frame::<_, ClientMessage>(stream))
        .await
        .with_context(|| format!("sandbox session {state} timed out"))?
        .with_context(|| format!("failed to read sandbox session {state}"))
}

async fn release(events: &mpsc::Sender<SessionEvent>, stream: &mut UnixStream) -> Result<()> {
    let (response, released) = oneshot::channel();
    events
        .send(SessionEvent::Releasing {
            response: Some(response),
        })
        .await
        .context("sandbox session owner stopped before release")?;
    let released = released
        .await
        .context("sandbox session release was cancelled")?;
    let message = match released {
        Ok(()) => ServerMessage::Released,
        Err(message) => ServerMessage::RuntimeFailed { message },
    };
    write_frame(stream, &message).await?;
    Ok(())
}

async fn abandon(events: &mpsc::Sender<SessionEvent>) -> Result<()> {
    events
        .send(SessionEvent::Releasing { response: None })
        .await
        .context("sandbox session owner stopped before abandoned release")
}

async fn reject(stream: &mut UnixStream, message: &str) -> Result<()> {
    write_frame(
        stream,
        &ServerMessage::Rejected {
            message: message.to_owned(),
        },
    )
    .await?;
    Ok(())
}

async fn retire(stream: &mut UnixStream, message: &str) -> Result<()> {
    write_frame(
        stream,
        &ServerMessage::Retiring {
            message: message.to_owned(),
        },
    )
    .await?;
    Ok(())
}

fn peer_effective_uid(stream: &UnixStream) -> Result<libc::uid_t> {
    let mut uid = 0;
    let mut gid = 0;
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to authenticate sandbox session peer");
    }
    Ok(uid)
}

fn current_uid() -> libc::uid_t {
    unsafe { libc::geteuid() }
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[tokio::test]
    async fn incompatible_build_is_retryable_when_the_idle_owner_retires() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (prepare, _prepare_receiver) = mpsc::channel(MAX_CONNECTIONS);
        let (events, mut event_receiver) = mpsc::channel(MAX_CONNECTIONS);
        let (_failure_sender, failure) = watch::channel(None);
        let context = ConnectionContext {
            build_identity: Arc::from("current-build"),
            config_identity: Arc::from("config-a"),
            sandbox_id: Arc::from("sandbox-a"),
            run_id: Arc::from("run-a"),
            prepare,
            events,
            failure,
        };
        let handler = tokio::spawn(handle_connection(server, context));
        write_frame(
            &mut client,
            &ClientMessage::Join {
                protocol: PROTOCOL_VERSION,
                build: "old-build".to_string(),
                config: "config-a".to_string(),
            },
        )
        .await
        .unwrap();
        match event_receiver.recv().await {
            Some(SessionEvent::BuildMismatch { response }) => response.send(true).unwrap(),
            _ => panic!("missing build mismatch decision request"),
        }

        let response: Value = read_frame(&mut client).await.unwrap();

        assert_eq!(response["type"], "retiring");
        handler.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn incompatible_build_is_rejected_while_a_compatible_lease_is_active() {
        let (prepare, _prepare_receiver) = mpsc::channel(MAX_CONNECTIONS);
        let (events, mut event_receiver) = mpsc::channel(MAX_CONNECTIONS);
        let (_failure_sender, failure) = watch::channel(None);
        let context = ConnectionContext {
            build_identity: Arc::from("current-build"),
            config_identity: Arc::from("config-a"),
            sandbox_id: Arc::from("sandbox-a"),
            run_id: Arc::from("run-a"),
            prepare,
            events,
            failure,
        };
        let (mut compatible, compatible_server) = UnixStream::pair().unwrap();
        let compatible_handler =
            tokio::spawn(handle_connection(compatible_server, context.clone()));
        write_frame(
            &mut compatible,
            &ClientMessage::Join {
                protocol: PROTOCOL_VERSION,
                build: "current-build".to_string(),
                config: "config-a".to_string(),
            },
        )
        .await
        .unwrap();
        let joined: Value = read_frame(&mut compatible).await.unwrap();
        assert_eq!(joined["type"], "joined");
        assert!(matches!(
            event_receiver.recv().await,
            Some(SessionEvent::Joined)
        ));

        let (mut incompatible, incompatible_server) = UnixStream::pair().unwrap();
        let incompatible_handler = tokio::spawn(handle_connection(incompatible_server, context));
        write_frame(
            &mut incompatible,
            &ClientMessage::Join {
                protocol: PROTOCOL_VERSION,
                build: "old-build".to_string(),
                config: "config-a".to_string(),
            },
        )
        .await
        .unwrap();
        match event_receiver.recv().await {
            Some(SessionEvent::BuildMismatch { response }) => response.send(false).unwrap(),
            _ => panic!("missing build mismatch decision request"),
        }

        let response: Value = read_frame(&mut incompatible).await.unwrap();

        assert_eq!(response["type"], "rejected");
        compatible_handler.abort();
        incompatible_handler.await.unwrap().unwrap();
    }
}
