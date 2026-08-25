use super::config::{
    AUDIT_CONTROL_DESCRIPTOR, CONTROL_LOCK_DESCRIPTOR, EXECUTION_CONTROL_DESCRIPTOR,
    LOCAL_CONTROL_DESCRIPTOR, REMOTE_CONTROL_DESCRIPTOR,
};
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
use super::config::{HookConfig, InheritedControlDescriptors};
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
use crate::audit::AuditClient;
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
use crate::execution::encode_ping_request;
use crate::execution::{PrepareResponse, decode_prepare_response, frame_length};
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
use crate::filesystem::broker::LocalClient;
use crate::ipc::{InheritedControlLock, InheritedControlStream};
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
use crate::nfs::client::RemoteClient;
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
use anyhow::{Context, Result, ensure};
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::RawFd;
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
use std::path::Path;
use std::sync::{Arc, OnceLock};
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
use std::time::Duration;

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
const EXECUTION_SLOT: i64 = 0;
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
const AUDIT_SLOT: i64 = 1;
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
const LOCAL_SLOT: i64 = 2;
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
const REMOTE_SLOT: i64 = 3;
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
const AUDIT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
const REMOTE_TIMEOUT: Duration = Duration::from_secs(15 * 60 + 5);

static CONTROL: OnceLock<std::result::Result<Option<ControlRuntime>, String>> = OnceLock::new();

struct ControlRuntime {
    lock: Arc<InheritedControlLock>,
    execution: Arc<InheritedControlStream<TcpStream>>,
    audit: Arc<InheritedControlStream<TcpStream>>,
    local: Option<Arc<InheritedControlStream<UnixStream>>>,
    remote: Option<Arc<InheritedControlStream<UnixStream>>>,
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
#[derive(Default)]
struct OwnedControlDescriptors {
    lock: Option<OwnedFd>,
    execution: Option<OwnedFd>,
    audit: Option<OwnedFd>,
    local: Option<OwnedFd>,
    remote: Option<OwnedFd>,
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
pub(super) fn initialize() -> Result<()> {
    CONTROL
        .get_or_init(|| {
            super::config::global()
                .map(ControlRuntime::new)
                .transpose()
                .map_err(|error| format!("{error:#}"))
        })
        .as_ref()
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

pub(super) fn execution() -> Option<Arc<InheritedControlStream<TcpStream>>> {
    runtime().map(|runtime| Arc::clone(&runtime.execution))
}

pub(super) fn audit() -> Option<Arc<InheritedControlStream<TcpStream>>> {
    runtime().map(|runtime| Arc::clone(&runtime.audit))
}

pub(super) fn local() -> Option<Arc<InheritedControlStream<UnixStream>>> {
    runtime()?.local.as_ref().map(Arc::clone)
}

pub(super) fn remote() -> Option<Arc<InheritedControlStream<UnixStream>>> {
    runtime()?.remote.as_ref().map(Arc::clone)
}

pub(super) fn child_environment() -> Vec<(&'static str, String)> {
    let Some(runtime) = runtime() else {
        return Vec::new();
    };
    let mut environment = vec![
        (
            CONTROL_LOCK_DESCRIPTOR,
            runtime.lock.descriptor().to_string(),
        ),
        (
            EXECUTION_CONTROL_DESCRIPTOR,
            runtime.execution.descriptor().to_string(),
        ),
        (
            AUDIT_CONTROL_DESCRIPTOR,
            runtime.audit.descriptor().to_string(),
        ),
    ];
    if let Some(local) = &runtime.local {
        environment.push((LOCAL_CONTROL_DESCRIPTOR, local.descriptor().to_string()));
    }
    if let Some(remote) = &runtime.remote {
        environment.push((REMOTE_CONTROL_DESCRIPTOR, remote.descriptor().to_string()));
    }
    environment
}

pub(super) fn inheritable_descriptors() -> Vec<RawFd> {
    let Some(runtime) = runtime() else {
        return Vec::new();
    };
    let mut descriptors = vec![
        runtime.lock.descriptor(),
        runtime.execution.descriptor(),
        runtime.audit.descriptor(),
    ];
    if let Some(local) = &runtime.local {
        descriptors.push(local.descriptor());
    }
    if let Some(remote) = &runtime.remote {
        descriptors.push(remote.descriptor());
    }
    descriptors
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
pub(super) unsafe fn reset_after_fork() {
    let Some(runtime) = runtime() else {
        return;
    };
    unsafe {
        runtime.execution.reset_after_fork();
        runtime.audit.reset_after_fork();
    }
    if let Some(local) = &runtime.local {
        unsafe { local.reset_after_fork() };
    }
    if let Some(remote) = &runtime.remote {
        unsafe { remote.reset_after_fork() };
    }
}

fn runtime() -> Option<&'static ControlRuntime> {
    CONTROL.get()?.as_ref().ok()?.as_ref()
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
impl ControlRuntime {
    fn new(config: &HookConfig) -> Result<Self> {
        let descriptors = Self::validate_descriptors(config)?;
        if descriptors.lock.is_some() {
            match OwnedControlDescriptors::duplicate(descriptors)
                .and_then(|owned| Self::from_descriptors(config, owned))
            {
                Ok(runtime) => {
                    Self::close_inherited(descriptors);
                    return Ok(runtime);
                }
                Err(inherited_error) => {
                    return Self::connect(config)
                        .with_context(|| {
                            format!(
                                "failed to reconnect sandbox control streams after inherited descriptors became unavailable: {inherited_error:#}"
                            )
                        });
                }
            }
        }
        Self::connect(config)
    }

    fn connect(config: &HookConfig) -> Result<Self> {
        let runtime = Self::from_descriptors(config, OwnedControlDescriptors::default())?;
        runtime.authenticate(config)?;
        Ok(runtime)
    }

    fn from_descriptors(config: &HookConfig, descriptors: OwnedControlDescriptors) -> Result<Self> {
        let lock = match descriptors.lock {
            Some(descriptor) => InheritedControlLock::from_owned_descriptor(descriptor)
                .context("failed to adopt the inherited sandbox control lock")?,
            None => InheritedControlLock::anonymous()
                .context("failed to create the sandbox control lock")?,
        };
        let execution = Self::tcp_stream(
            descriptors.execution,
            config.execution_control(),
            Arc::clone(&lock),
            EXECUTION_SLOT,
            EXECUTION_TIMEOUT,
        )
        .context("failed to initialize the execution control stream")?;
        let audit = Self::tcp_stream(
            descriptors.audit,
            config.audit_control(),
            Arc::clone(&lock),
            AUDIT_SLOT,
            AUDIT_TIMEOUT,
        )
        .context("failed to initialize the audit control stream")?;
        let local = config
            .local_filesystem()
            .map(|(socket, _)| {
                Self::unix_stream(
                    descriptors.local,
                    socket,
                    Arc::clone(&lock),
                    LOCAL_SLOT,
                    LOCAL_TIMEOUT,
                )
            })
            .transpose()
            .context("failed to initialize the local filesystem control stream")?;
        let remote = config
            .remote_filesystem()
            .map(|(socket, _, _)| {
                Self::unix_stream(
                    descriptors.remote,
                    socket,
                    Arc::clone(&lock),
                    REMOTE_SLOT,
                    REMOTE_TIMEOUT,
                )
            })
            .transpose()
            .context("failed to initialize the remote filesystem control stream")?;
        Ok(Self {
            lock,
            execution,
            audit,
            local,
            remote,
        })
    }

    fn validate_descriptors(config: &HookConfig) -> Result<InheritedControlDescriptors> {
        let descriptors = config.inherited_control_descriptors();
        let inherited = descriptors.lock.is_some();
        ensure!(
            descriptors.execution.is_some() == inherited
                && descriptors.audit.is_some() == inherited
                && descriptors.local.is_some()
                    == (inherited && config.local_filesystem().is_some())
                && descriptors.remote.is_some()
                    == (inherited && config.remote_filesystem().is_some()),
            "inherited sandbox control descriptors are incomplete"
        );
        let mut unique = HashSet::new();
        ensure!(
            [
                descriptors.lock,
                descriptors.execution,
                descriptors.audit,
                descriptors.local,
                descriptors.remote,
            ]
            .into_iter()
            .flatten()
            .all(|descriptor| unique.insert(descriptor)),
            "inherited sandbox control descriptors are not unique"
        );
        Ok(descriptors)
    }

    fn close_inherited(descriptors: InheritedControlDescriptors) {
        for descriptor in [
            descriptors.lock,
            descriptors.execution,
            descriptors.audit,
            descriptors.local,
            descriptors.remote,
        ]
        .into_iter()
        .flatten()
        {
            unsafe { libc::close(descriptor) };
        }
    }

    fn tcp_stream(
        descriptor: Option<OwnedFd>,
        control: std::net::SocketAddr,
        lock: Arc<InheritedControlLock>,
        slot: i64,
        timeout: Duration,
    ) -> Result<Arc<InheritedControlStream<TcpStream>>> {
        let stream = match descriptor {
            Some(descriptor) => TcpStream::from(descriptor),
            None => TcpStream::connect(control)?,
        };
        ensure!(
            stream.peer_addr()? == control,
            "inherited TCP control descriptor has an unexpected peer"
        );
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(InheritedControlStream::new(stream, lock, slot)?)
    }

    fn unix_stream(
        descriptor: Option<OwnedFd>,
        socket: &str,
        lock: Arc<InheritedControlLock>,
        slot: i64,
        timeout: Duration,
    ) -> Result<Arc<InheritedControlStream<UnixStream>>> {
        let stream = match descriptor {
            Some(descriptor) => UnixStream::from(descriptor),
            None => UnixStream::connect(socket)?,
        };
        ensure!(
            stream.peer_addr()?.as_pathname() == Some(Path::new(socket)),
            "inherited Unix control descriptor has an unexpected peer"
        );
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(InheritedControlStream::new(stream, lock, slot)?)
    }

    fn authenticate(&self, config: &HookConfig) -> Result<()> {
        self.ping_execution(config.execution_token())?;
        AuditClient::with_shared(
            config.audit_control(),
            config.audit_token(),
            Arc::clone(&self.audit),
        )
        .ping_shared()
        .context("audit control handshake failed")?;
        if let (Some((socket, token)), Some(stream)) = (config.local_filesystem(), &self.local) {
            LocalClient::with_shared(socket, token, Arc::clone(stream))
                .ping_shared()
                .context("local filesystem control handshake failed")?;
        }
        if let (Some((socket, token, _)), Some(stream)) = (config.remote_filesystem(), &self.remote)
        {
            RemoteClient::with_shared(socket, token, Arc::clone(stream))
                .ping_shared()
                .context("remote filesystem control handshake failed")?;
        }
        Ok(())
    }

    fn ping_execution(&self, token: &str) -> Result<()> {
        let request = encode_ping_request(token)?;
        let response = self
            .execution
            .transact(|stream| execution_request(stream, &request))
            .context("failed to serialize the execution control handshake")??;
        ensure!(
            response == PrepareResponse::Accepted,
            "execution control handshake returned an unexpected response"
        );
        Ok(())
    }
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
impl OwnedControlDescriptors {
    fn duplicate(descriptors: InheritedControlDescriptors) -> Result<Self> {
        Ok(Self {
            lock: Self::duplicate_one(descriptors.lock)?,
            execution: Self::duplicate_one(descriptors.execution)?,
            audit: Self::duplicate_one(descriptors.audit)?,
            local: Self::duplicate_one(descriptors.local)?,
            remote: Self::duplicate_one(descriptors.remote)?,
        })
    }

    fn duplicate_one(descriptor: Option<RawFd>) -> Result<Option<OwnedFd>> {
        descriptor
            .map(|descriptor| {
                let descriptor = unsafe { BorrowedFd::borrow_raw(descriptor) };
                descriptor.try_clone_to_owned().with_context(|| {
                    format!(
                        "failed to duplicate inherited sandbox control descriptor {}",
                        descriptor.as_raw_fd()
                    )
                })
            })
            .transpose()
    }
}

pub(super) fn execution_request(
    stream: &mut TcpStream,
    request: &[u8],
) -> std::io::Result<PrepareResponse> {
    stream.write_all(request)?;
    let mut prefix = [0_u8; 4];
    stream.read_exact(&mut prefix)?;
    let mut response = vec![0_u8; frame_length(prefix)?];
    stream.read_exact(&mut response)?;
    decode_prepare_response(&response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::EXECUTION_PROTOCOL_VERSION;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn base_value(key: &str) -> Option<&'static str> {
        match key {
            "AGORA_SANDBOX_TOKEN" => Some("token"),
            "AGORA_SANDBOX_PROXY_IPV4" => Some("127.0.0.1:41000"),
            "AGORA_SANDBOX_PROXY_IPV6" => Some("[::1]:41001"),
            "AGORA_SANDBOX_EXECUTION_CONTROL" => Some("127.0.0.1:41002"),
            "AGORA_SANDBOX_EXECUTION_TOKEN" => Some("execution-token"),
            "AGORA_SANDBOX_AUDIT_CONTROL" => Some("127.0.0.1:41003"),
            "AGORA_SANDBOX_AUDIT_TOKEN" => Some("audit-token"),
            "AGORA_SANDBOX_HOOK_LIBRARIES" => Some("/tmp/hook.dylib"),
            "AGORA_SANDBOX_FILESYSTEM_ROOT" => Some("/tmp/agora-fs"),
            "AGORA_SANDBOX_FILESYSTEM_MODE" => Some("plain"),
            "AGORA_SANDBOX_TRACE_ID" => Some("trace-root"),
            _ => None,
        }
    }

    fn config(overrides: &[(&str, &str)]) -> HookConfig {
        HookConfig::from_getter(|key| {
            overrides
                .iter()
                .find_map(|(name, value)| (*name == key).then(|| (*value).to_string()))
                .or_else(|| base_value(key).map(ToString::to_string))
        })
        .unwrap()
    }

    fn framed(body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    fn count_requests(mut stream: TcpStream, response: Vec<u8>) -> thread::JoinHandle<usize> {
        thread::spawn(move || {
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut requests = 0;
            loop {
                let mut prefix = [0_u8; 4];
                match stream.read_exact(&mut prefix) {
                    Ok(()) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::UnexpectedEof
                                | std::io::ErrorKind::ConnectionReset
                        ) =>
                    {
                        return requests;
                    }
                    Err(error) => panic!("failed to read control request: {error}"),
                }
                let mut request = vec![0_u8; u32::from_be_bytes(prefix) as usize];
                stream.read_exact(&mut request).unwrap();
                stream.write_all(&response).unwrap();
                requests += 1;
            }
        })
    }

    fn execution_accepted() -> Vec<u8> {
        let mut body = Vec::with_capacity(7);
        body.extend_from_slice(&EXECUTION_PROTOCOL_VERSION.to_be_bytes());
        body.push(0);
        body.extend_from_slice(&0_u32.to_be_bytes());
        framed(&body)
    }

    fn audit_accepted() -> Vec<u8> {
        framed(br#""Accepted""#)
    }

    #[test]
    fn inherited_control_streams_do_not_repeat_authentication() {
        let execution_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let execution_address = execution_listener.local_addr().unwrap();
        let execution_client = TcpStream::connect(execution_address).unwrap();
        let (execution_server, _) = execution_listener.accept().unwrap();

        let audit_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let audit_address = audit_listener.local_addr().unwrap();
        let audit_client = TcpStream::connect(audit_address).unwrap();
        let (audit_server, _) = audit_listener.accept().unwrap();

        let execution_requests = count_requests(execution_server, execution_accepted());
        let audit_requests = count_requests(audit_server, audit_accepted());
        let lock = tempfile::tempfile().unwrap().into_raw_fd();
        let execution = execution_client.into_raw_fd();
        let audit = audit_client.into_raw_fd();
        let execution_address = execution_address.to_string();
        let audit_address = audit_address.to_string();
        let lock = lock.to_string();
        let execution = execution.to_string();
        let audit = audit.to_string();
        let config = config(&[
            ("AGORA_SANDBOX_EXECUTION_CONTROL", &execution_address),
            ("AGORA_SANDBOX_AUDIT_CONTROL", &audit_address),
            (CONTROL_LOCK_DESCRIPTOR, &lock),
            (EXECUTION_CONTROL_DESCRIPTOR, &execution),
            (AUDIT_CONTROL_DESCRIPTOR, &audit),
        ]);

        let runtime = ControlRuntime::new(&config).unwrap();
        drop(runtime);

        assert_eq!(execution_requests.join().unwrap(), 0);
        assert_eq!(audit_requests.join().unwrap(), 0);
    }

    #[test]
    fn inherited_control_descriptors_are_complete_and_unique() {
        ControlRuntime::validate_descriptors(&config(&[])).unwrap();
        ControlRuntime::validate_descriptors(&config(&[
            (CONTROL_LOCK_DESCRIPTOR, "10"),
            (EXECUTION_CONTROL_DESCRIPTOR, "11"),
            (AUDIT_CONTROL_DESCRIPTOR, "12"),
        ]))
        .unwrap();

        assert!(
            ControlRuntime::validate_descriptors(&config(&[(CONTROL_LOCK_DESCRIPTOR, "10")]))
                .unwrap_err()
                .to_string()
                .contains("incomplete")
        );
        assert!(
            ControlRuntime::validate_descriptors(&config(&[
                (CONTROL_LOCK_DESCRIPTOR, "10"),
                (EXECUTION_CONTROL_DESCRIPTOR, "10"),
                (AUDIT_CONTROL_DESCRIPTOR, "12"),
            ]))
            .unwrap_err()
            .to_string()
            .contains("not unique")
        );
    }

    #[test]
    fn inherited_control_descriptor_values_are_nonnegative_integers() {
        for value in ["-1", "invalid"] {
            let error = HookConfig::from_getter(|key| {
                if key == CONTROL_LOCK_DESCRIPTOR {
                    Some(value.to_string())
                } else {
                    base_value(key).map(ToString::to_string)
                }
            })
            .unwrap_err();
            assert!(error.contains(CONTROL_LOCK_DESCRIPTOR));
        }
    }

    #[test]
    fn inherited_control_streams_reject_reused_descriptors_before_authentication() {
        let lock = InheritedControlLock::anonymous().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let descriptor: OwnedFd = stream.into();
        let error = ControlRuntime::tcp_stream(
            Some(descriptor),
            other.local_addr().unwrap(),
            Arc::clone(&lock),
            EXECUTION_SLOT,
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("unexpected peer"));

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let stream = UnixStream::connect(&socket).unwrap();
        let descriptor: OwnedFd = stream.into();
        let error = ControlRuntime::unix_stream(
            Some(descriptor),
            directory.path().join("other.sock").to_str().unwrap(),
            lock,
            LOCAL_SLOT,
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("unexpected peer"));

        drop(listener);
    }
}
