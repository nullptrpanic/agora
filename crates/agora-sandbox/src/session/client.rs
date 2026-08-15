use super::protocol::{ClientMessage, PROTOCOL_VERSION, ServerMessage, read_frame, write_frame};
use super::startup::{SessionPaths, build_identity, connect_or_start};
use crate::runner::{PreparedLaunch, RunningSandboxCommand, SandboxCommand, SandboxOutcome};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::time::Instant;

const JOIN_RETRIES: usize = 3;
#[cfg(not(test))]
const SESSION_CONTROL_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(test)]
const SESSION_CONTROL_TIMEOUT: Duration = Duration::from_millis(200);

pub async fn run(
    config_path: &Path,
    workdir: &Path,
    config_identity: &str,
    command: SandboxCommand,
) -> Result<SandboxOutcome> {
    let executable = command.resolved_program()?;
    let paths = SessionPaths::resolve(workdir)?;
    let build_identity = build_identity()?;
    let prepared = prepare_with_retry(
        config_path,
        &paths,
        &build_identity,
        config_identity,
        executable,
    )
    .await?;
    run_prepared(command, prepared).await
}

struct PreparedConnection {
    stream: UnixStream,
    launch: PreparedLaunch,
    sandbox_id: String,
    run_id: String,
}

async fn prepare_with_retry(
    config_path: &Path,
    paths: &SessionPaths,
    build_identity: &str,
    config_identity: &str,
    executable: std::path::PathBuf,
) -> Result<PreparedConnection> {
    let deadline = Instant::now() + SESSION_CONTROL_TIMEOUT;
    let mut last = None;
    for attempt in 0..JOIN_RETRIES {
        match prepare_once(
            config_path,
            paths,
            build_identity,
            config_identity,
            executable.clone(),
            deadline,
        )
        .await
        {
            Ok(prepared) => return Ok(prepared),
            Err(JoinFailure::Fatal(error)) => return Err(error),
            Err(JoinFailure::Retry(error)) => {
                last = Some(error);
                if Instant::now() >= deadline {
                    break;
                }
                if attempt + 1 < JOIN_RETRIES {
                    tokio::time::sleep_until(
                        (Instant::now() + Duration::from_millis(25)).min(deadline),
                    )
                    .await;
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("sandbox session join failed")))
}

enum JoinFailure {
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

async fn prepare_once(
    config_path: &Path,
    paths: &SessionPaths,
    build_identity: &str,
    config_identity: &str,
    executable: std::path::PathBuf,
    deadline: Instant,
) -> std::result::Result<PreparedConnection, JoinFailure> {
    let mut stream = tokio::time::timeout_at(deadline, connect_or_start(config_path, paths))
        .await
        .context("sandbox session connection timed out")
        .and_then(|result| result)
        .map_err(JoinFailure::Retry)?;
    write_control_frame(
        &mut stream,
        &ClientMessage::Join {
            protocol: PROTOCOL_VERSION,
            build: build_identity.to_owned(),
            config: config_identity.to_owned(),
        },
        deadline,
        "join request",
    )
    .await
    .map_err(JoinFailure::Retry)?;
    let (sandbox_id, run_id) = match read_control_frame::<_, ServerMessage>(
        &mut stream,
        deadline,
        "join response",
    )
    .await
    {
        Ok(ServerMessage::Joined { sandbox_id, run_id }) => (sandbox_id, run_id),
        Ok(ServerMessage::Rejected { message }) => {
            return Err(JoinFailure::Fatal(anyhow!(message)));
        }
        Ok(ServerMessage::RuntimeFailed { message }) => {
            return Err(JoinFailure::Retry(anyhow!(message)));
        }
        Ok(ServerMessage::Retiring { message }) => {
            return Err(JoinFailure::Retry(anyhow!(message)));
        }
        Ok(_) => {
            return Err(JoinFailure::Fatal(anyhow!(
                "invalid sandbox session join response"
            )));
        }
        Err(error) => return Err(JoinFailure::Retry(error)),
    };
    write_control_frame(
        &mut stream,
        &ClientMessage::Prepare {
            executable: executable.as_os_str().into(),
        },
        deadline,
        "prepare request",
    )
    .await
    .map_err(JoinFailure::Retry)?;
    let launch =
        match read_control_frame::<_, ServerMessage>(&mut stream, deadline, "prepare response")
            .await
        {
            Ok(ServerMessage::Prepared { launch }) => {
                launch.into_prepared().map_err(JoinFailure::Fatal)?
            }
            Ok(ServerMessage::Rejected { message }) => {
                return Err(JoinFailure::Fatal(anyhow!(message)));
            }
            Ok(ServerMessage::RuntimeFailed { message }) => {
                return Err(JoinFailure::Retry(anyhow!(message)));
            }
            Ok(ServerMessage::Retiring { message }) => {
                return Err(JoinFailure::Retry(anyhow!(message)));
            }
            Ok(_) => {
                return Err(JoinFailure::Fatal(anyhow!(
                    "invalid sandbox session prepare response"
                )));
            }
            Err(error) => return Err(JoinFailure::Retry(error)),
        };
    Ok(PreparedConnection {
        stream,
        launch,
        sandbox_id,
        run_id,
    })
}

async fn run_prepared(
    command: SandboxCommand,
    mut prepared: PreparedConnection,
) -> Result<SandboxOutcome> {
    let mut child = match RunningSandboxCommand::spawn(command, &prepared.launch) {
        Ok(child) => child,
        Err(error) => {
            let _ = cancel_launch(&mut prepared).await;
            return Err(error);
        }
    };
    let (mut reader, mut writer) = prepared.stream.into_split();
    let mut notification = tokio::spawn(async move {
        read_frame::<_, ServerMessage>(&mut reader)
            .await
            .context("failed to read sandbox session notification")
    });
    let status = match child
        .wait_or_failure(async {
            match (&mut notification).await {
                Ok(Ok(ServerMessage::RuntimeFailed { message })) => anyhow!(message),
                Ok(Ok(ServerMessage::Retiring { message })) => anyhow!(message),
                Ok(Ok(ServerMessage::Rejected { message })) => anyhow!(message),
                Ok(Ok(message)) => anyhow!("unexpected sandbox session message: {message:?}"),
                Ok(Err(error)) => error,
                Err(error) => {
                    anyhow::Error::from(error).context("sandbox session notification task failed")
                }
            }
        })
        .await
    {
        Ok(status) => status,
        Err(error) => {
            notification.abort();
            return Err(error);
        }
    };
    let deadline = Instant::now() + SESSION_CONTROL_TIMEOUT;
    if let Err(error) = write_control_frame(
        &mut writer,
        &ClientMessage::Finished {
            launch_id: prepared.launch.launch_id().to_owned(),
        },
        deadline,
        "finish request",
    )
    .await
    .context("failed to release sandbox session launch")
    {
        notification.abort();
        return Err(error);
    }
    let released = match tokio::time::timeout_at(deadline, &mut notification).await {
        Ok(result) => result
            .context("sandbox session notification task failed")?
            .context("failed to read sandbox session release response")?,
        Err(error) => {
            notification.abort();
            return Err(error).context("sandbox session release response timed out");
        }
    };
    match released {
        ServerMessage::Released => Ok(SandboxOutcome::new(
            status,
            prepared.sandbox_id,
            prepared.run_id,
        )),
        ServerMessage::RuntimeFailed { message } | ServerMessage::Rejected { message } => {
            bail!(message)
        }
        ServerMessage::Retiring { message } => bail!(message),
        _ => bail!("invalid sandbox session release response"),
    }
}

async fn cancel_launch(prepared: &mut PreparedConnection) -> Result<()> {
    let deadline = Instant::now() + SESSION_CONTROL_TIMEOUT;
    write_control_frame(
        &mut prepared.stream,
        &ClientMessage::Cancel {
            launch_id: prepared.launch.launch_id().to_owned(),
        },
        deadline,
        "cancel request",
    )
    .await?;
    match read_control_frame::<_, ServerMessage>(&mut prepared.stream, deadline, "cancel response")
        .await?
    {
        ServerMessage::Released => Ok(()),
        ServerMessage::RuntimeFailed { message }
        | ServerMessage::Retiring { message }
        | ServerMessage::Rejected { message } => {
            bail!(message)
        }
        _ => bail!("invalid sandbox session cancellation response"),
    }
}

async fn write_control_frame<W, T>(
    writer: &mut W,
    value: &T,
    deadline: Instant,
    state: &str,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    tokio::time::timeout_at(deadline, write_frame(writer, value))
        .await
        .with_context(|| format!("sandbox session {state} timed out"))?
        .with_context(|| format!("failed to write sandbox session {state}"))
}

async fn read_control_frame<R, T>(reader: &mut R, deadline: Instant, state: &str) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    tokio::time::timeout_at(deadline, read_frame(reader))
        .await
        .with_context(|| format!("sandbox session {state} timed out"))?
        .with_context(|| format!("failed to read sandbox session {state}"))
}
