#[cfg(all(target_os = "macos", any(agora_sandbox_hook_build, test, coverage)))]
use super::protocol::encode_ping_request;
use super::protocol::{
    AuditEventRequest, AuditResponse, decode_response, encode_request, frame_length,
};
#[cfg(target_os = "macos")]
use crate::ipc::InheritedControlStream;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::IntoRawFd;
#[cfg(target_os = "macos")]
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

const AUDIT_CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
const AUDIT_CONNECTION_MAX_IDLE: Duration = Duration::from_secs(25);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct AuditEndpoint {
    control: SocketAddr,
    token: String,
}

struct AuditConnection {
    stream: TcpStream,
    last_used: Instant,
}

struct AuditConnections {
    pid: u32,
    entries: HashMap<AuditEndpoint, AuditConnection>,
}

impl AuditConnections {
    fn new() -> Self {
        Self {
            pid: std::process::id(),
            entries: HashMap::new(),
        }
    }

    fn enter_process(&mut self, pid: u32) {
        if self.pid == pid {
            return;
        }

        // A fork child may reach an exec hook after the launcher has already
        // closed descriptors that belonged to the parent. Dropping the
        // inherited TcpStream values would then close a reused descriptor and
        // triggers Rust's owned-FD safety check in debug-enabled runtimes.
        // Abandon only descriptor ownership in this child; the parent still
        // owns and eventually closes the actual connections.
        for (_, connection) in std::mem::take(&mut self.entries) {
            let _ = connection.stream.into_raw_fd();
        }
        self.pid = pid;
    }
}

thread_local! {
    static CONNECTIONS: RefCell<AuditConnections> = RefCell::new(AuditConnections::new());
}

#[derive(Clone, Debug)]
pub(crate) struct AuditClient {
    endpoint: AuditEndpoint,
    #[cfg(target_os = "macos")]
    shared: Option<Arc<InheritedControlStream<TcpStream>>>,
    #[cfg(target_os = "macos")]
    prefer_shared: Arc<AtomicBool>,
    #[cfg(target_os = "macos")]
    observed_pid: Arc<AtomicU32>,
}

impl AuditClient {
    pub(crate) fn new(control: SocketAddr, token: impl Into<String>) -> Self {
        Self {
            endpoint: AuditEndpoint {
                control,
                token: token.into(),
            },
            #[cfg(target_os = "macos")]
            shared: None,
            #[cfg(target_os = "macos")]
            prefer_shared: Arc::new(AtomicBool::new(false)),
            #[cfg(target_os = "macos")]
            observed_pid: Arc::new(AtomicU32::new(std::process::id())),
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn with_shared(
        control: SocketAddr,
        token: impl Into<String>,
        shared: Arc<InheritedControlStream<TcpStream>>,
    ) -> Self {
        let mut client = Self::new(control, token);
        client.shared = Some(shared);
        client.prefer_shared.store(true, Ordering::Release);
        client
    }

    pub(crate) fn publish(&self, event: AuditEventRequest) -> Result<(), AuditError> {
        let request = encode_request(&self.endpoint.token, event).map_err(AuditError::from_io)?;
        #[cfg(target_os = "macos")]
        let current_pid = std::process::id();
        #[cfg(target_os = "macos")]
        if self.observed_pid.swap(current_pid, Ordering::AcqRel) != current_pid
            && self.shared.is_some()
        {
            self.prefer_shared.store(true, Ordering::Release);
        }
        self.enter_process();
        #[cfg(target_os = "macos")]
        if self.prefer_shared.load(Ordering::Acquire) && self.shared.is_some() {
            let result = self.publish_shared(&request);
            if !result.as_ref().is_err_and(AuditError::disconnects) {
                return result;
            }
            self.prefer_shared.store(false, Ordering::Release);
        }
        let result = self.publish_regular(&request);
        #[cfg(target_os = "macos")]
        if result.as_ref().is_err_and(AuditError::disconnects) && self.shared.is_some() {
            let result = self.publish_shared(&request);
            self.prefer_shared.store(
                !result.as_ref().is_err_and(AuditError::disconnects),
                Ordering::Release,
            );
            return result;
        }
        result
    }

    #[cfg(all(target_os = "macos", any(agora_sandbox_hook_build, test, coverage)))]
    pub(crate) fn ping_shared(&self) -> Result<(), AuditError> {
        let request = encode_ping_request(&self.endpoint.token).map_err(AuditError::from_io)?;
        let shared = self.shared_stream()?;
        shared
            .transact(|stream| Self::publish_on(stream, &request))
            .map_err(AuditError::from_io)?
    }

    fn publish_regular(&self, request: &[u8]) -> Result<(), AuditError> {
        #[cfg(target_os = "macos")]
        let _signals =
            crate::platform::hook::SignalMaskGuard::block().map_err(AuditError::from_io)?;
        CONNECTIONS
            .try_with(|connections| {
                let mut connections = connections.borrow_mut();
                connections.enter_process(std::process::id());
                if connections
                    .entries
                    .get(&self.endpoint)
                    .is_some_and(|connection| {
                        connection.last_used.elapsed() >= AUDIT_CONNECTION_MAX_IDLE
                    })
                {
                    connections.entries.remove(&self.endpoint);
                }
                if let Some(connection) = connections.entries.get_mut(&self.endpoint) {
                    let result = Self::send_on(&mut connection.stream, request);
                    if result.is_ok() {
                        connection.last_used = Instant::now();
                    }
                    if !result.as_ref().is_err_and(AuditError::disconnects) {
                        return result;
                    }
                    connections.entries.remove(&self.endpoint);
                }

                let mut stream = Self::connect(self.endpoint.control)?;
                let result = Self::publish_on(&mut stream, request);
                if result.is_ok() {
                    connections.entries.insert(
                        self.endpoint.clone(),
                        AuditConnection {
                            stream,
                            last_used: Instant::now(),
                        },
                    );
                }
                result
            })
            .unwrap_or_else(|_| {
                let mut stream = Self::connect(self.endpoint.control)?;
                Self::publish_on(&mut stream, request)
            })
    }

    fn enter_process(&self) {
        let _ = CONNECTIONS.try_with(|connections| {
            connections.borrow_mut().enter_process(std::process::id());
        });
    }

    #[cfg(target_os = "macos")]
    fn publish_shared(&self, request: &[u8]) -> Result<(), AuditError> {
        let shared = self.shared_stream()?;
        shared
            .transact(|stream| Self::send_on(stream, request))
            .map_err(AuditError::from_io)?
    }

    #[cfg(target_os = "macos")]
    fn shared_stream(&self) -> Result<&InheritedControlStream<TcpStream>, AuditError> {
        self.shared.as_deref().ok_or_else(|| AuditError {
            errno: libc::ENOTCONN,
            message: "shared audit control stream is unavailable".to_string(),
            disconnect: true,
        })
    }

    fn connect(control: SocketAddr) -> Result<TcpStream, AuditError> {
        let stream = TcpStream::connect(control).map_err(AuditError::from_io)?;
        stream
            .set_read_timeout(Some(AUDIT_CLIENT_TIMEOUT))
            .map_err(AuditError::from_io)?;
        stream
            .set_write_timeout(Some(AUDIT_CLIENT_TIMEOUT))
            .map_err(AuditError::from_io)?;
        Ok(stream)
    }

    fn publish_on(stream: &mut TcpStream, request: &[u8]) -> Result<(), AuditError> {
        Self::send_on(stream, request)?;
        let mut prefix = [0_u8; 4];
        stream
            .read_exact(&mut prefix)
            .map_err(AuditError::from_io)?;
        let mut response = vec![0_u8; frame_length(prefix).map_err(AuditError::from_io)?];
        stream
            .read_exact(&mut response)
            .map_err(AuditError::from_io)?;
        match decode_response(&response).map_err(AuditError::from_io)? {
            AuditResponse::Accepted => Ok(()),
            AuditResponse::Error { errno, message } => Err(AuditError {
                errno,
                message,
                disconnect: false,
            }),
        }
    }

    fn send_on(stream: &mut TcpStream, request: &[u8]) -> Result<(), AuditError> {
        stream.write_all(request).map_err(AuditError::from_io)
    }
}

#[derive(Debug)]
pub(crate) struct AuditError {
    errno: libc::c_int,
    message: String,
    disconnect: bool,
}

impl AuditError {
    pub(crate) fn errno(&self) -> libc::c_int {
        self.errno
    }

    fn disconnects(&self) -> bool {
        self.disconnect
    }

    fn from_io(error: io::Error) -> Self {
        Self {
            errno: error.raw_os_error().unwrap_or(match error.kind() {
                io::ErrorKind::PermissionDenied => libc::EACCES,
                io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => libc::EINVAL,
                io::ErrorKind::TimedOut => libc::ETIMEDOUT,
                _ => libc::EIO,
            }),
            message: error.to_string(),
            disconnect: true,
        }
    }
}

impl std::fmt::Display for AuditError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AuditError {}

#[cfg(test)]
mod tests;
