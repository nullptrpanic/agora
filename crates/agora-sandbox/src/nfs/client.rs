#[cfg(target_os = "macos")]
use crate::ipc::InheritedControlStream;
use crate::nfs::protocol::{
    PROTOCOL_VERSION, REMOTE_CLIENT_TIMEOUT, Request, RequestEnvelope, RequestId, Response,
    ResponseEnvelope,
};
use crate::nfs::transport;
use serde::de::DeserializeOwned;
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read};
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

const REMOTE_REQUEST_ATTEMPTS: usize = 2;

#[derive(Clone, Debug)]
pub(crate) struct RemoteClient {
    socket: PathBuf,
    token: String,
    timeout: Duration,
    #[cfg(target_os = "macos")]
    shared: Option<Arc<InheritedControlStream<UnixStream>>>,
    #[cfg(target_os = "macos")]
    prefer_shared: Arc<AtomicBool>,
    #[cfg(target_os = "macos")]
    observed_pid: Arc<AtomicU32>,
}

impl RemoteClient {
    pub(crate) fn new(socket: impl Into<PathBuf>, token: impl Into<String>) -> Self {
        Self::new_with_timeout(socket, token, REMOTE_CLIENT_TIMEOUT)
    }

    fn new_with_timeout(
        socket: impl Into<PathBuf>,
        token: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            socket: socket.into(),
            token: token.into(),
            timeout,
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
        socket: impl Into<PathBuf>,
        token: impl Into<String>,
        shared: Arc<InheritedControlStream<UnixStream>>,
    ) -> Self {
        let mut client = Self::new(socket, token);
        client.shared = Some(shared);
        client
    }

    #[cfg(all(target_os = "macos", any(agora_sandbox_hook_build, test, coverage)))]
    pub(crate) fn ping_shared(&self) -> Result<(), RemoteClientError> {
        let request_id = RequestId::new(uuid::Uuid::new_v4().simple().to_string())
            .expect("UUID is a valid remote request ID");
        let reply = self.request_shared(request_id, Request::Ping, None)?;
        if reply.response == Response::Success && reply.descriptor.is_none() {
            Ok(())
        } else {
            Err(RemoteClientError::new(
                libc::EPROTO,
                "remote filesystem ping returned an unexpected response",
            ))
        }
    }

    pub(crate) fn request(&self, request: Request) -> Result<RemoteReply, RemoteClientError> {
        self.request_with_optional_descriptor(request, None)
    }

    pub(crate) fn request_with_descriptor(
        &self,
        request: Request,
        descriptor: RawFd,
    ) -> Result<RemoteReply, RemoteClientError> {
        self.request_with_optional_descriptor(request, Some(descriptor))
    }

    fn request_with_optional_descriptor(
        &self,
        request: Request,
        descriptor: Option<RawFd>,
    ) -> Result<RemoteReply, RemoteClientError> {
        let request_id = RequestId::new(uuid::Uuid::new_v4().simple().to_string())
            .expect("UUID is a valid remote request ID");
        let reply = self.request_with_id(request_id.clone(), request, descriptor)?;
        let resource = match &reply.response {
            Response::Open { handle, .. } => Some((Some(handle.clone()), None)),
            Response::Stat { anchor, .. } | Response::List { anchor } => {
                Some((None, Some(anchor.clone())))
            }
            Response::Read { payload, .. } => Some((None, Some(payload.clone()))),
            _ => None,
        };
        if let Some((handle, anchor)) = resource
            && let Err(error) = self.request_with_id(
                RequestId::new(uuid::Uuid::new_v4().simple().to_string())
                    .expect("UUID is a valid remote request ID"),
                Request::Claim { request_id },
                None,
            )
        {
            if let Some(handle) = handle {
                let _ = self.request(Request::Abort { handle });
            }
            if let Some(anchor) = anchor {
                remove_anchor(self.socket.parent(), &anchor);
            }
            return Err(error);
        }
        Ok(reply)
    }

    fn request_with_id(
        &self,
        request_id: RequestId,
        request: Request,
        descriptor: Option<RawFd>,
    ) -> Result<RemoteReply, RemoteClientError> {
        let mut last_error = None;
        for _ in 0..REMOTE_REQUEST_ATTEMPTS {
            match self.request_once(request_id.clone(), request.clone(), descriptor) {
                Ok(reply) => return Ok(reply),
                Err(error) if error.retryable => last_error = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last_error.expect("remote request attempts is non-zero"))
    }

    fn request_once(
        &self,
        request_id: RequestId,
        request: Request,
        descriptor: Option<RawFd>,
    ) -> Result<RemoteReply, RemoteClientError> {
        #[cfg(target_os = "macos")]
        let current_pid = std::process::id();
        #[cfg(target_os = "macos")]
        if self.observed_pid.swap(current_pid, Ordering::AcqRel) != current_pid
            && self.shared.is_some()
        {
            self.prefer_shared.store(true, Ordering::Release);
        }
        #[cfg(target_os = "macos")]
        if self.prefer_shared.load(Ordering::Acquire) && self.shared.is_some() {
            let result = self.request_shared(request_id, request, descriptor);
            if result.is_err() {
                self.prefer_shared.store(false, Ordering::Release);
            }
            return result;
        }
        let mut stream = match UnixStream::connect(&self.socket) {
            Ok(stream) => stream,
            Err(error) => {
                #[cfg(target_os = "macos")]
                if self.shared.is_some() {
                    let result = self.request_shared(request_id, request, descriptor);
                    self.prefer_shared.store(result.is_ok(), Ordering::Release);
                    return result;
                }
                return Err(RemoteClientError::io(
                    "failed to connect to remote broker",
                    error,
                    true,
                ));
            }
        };
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|error| {
                RemoteClientError::io("failed to configure remote broker", error, false)
            })?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(|error| {
                RemoteClientError::io("failed to configure remote broker", error, false)
            })?;
        Self::exchange(&mut stream, &self.token, request_id, request, descriptor)
    }

    #[cfg(target_os = "macos")]
    fn request_shared(
        &self,
        request_id: RequestId,
        request: Request,
        descriptor: Option<RawFd>,
    ) -> Result<RemoteReply, RemoteClientError> {
        let shared = self.shared.as_ref().ok_or_else(|| {
            RemoteClientError::new(
                libc::ENOTCONN,
                "shared remote filesystem control stream is unavailable",
            )
        })?;
        shared
            .transact(|stream| Self::exchange(stream, &self.token, request_id, request, descriptor))
            .map_err(|error| {
                RemoteClientError::io(
                    "failed to serialize inherited remote filesystem request",
                    error,
                    true,
                )
            })?
    }

    fn exchange(
        stream: &mut UnixStream,
        token: &str,
        request_id: RequestId,
        request: Request,
        descriptor: Option<RawFd>,
    ) -> Result<RemoteReply, RemoteClientError> {
        let request = RequestEnvelope {
            version: PROTOCOL_VERSION,
            token: token.to_string(),
            request_id: request_id.clone(),
            request,
        };
        transport::send(stream, &request, descriptor)
            .map_err(|error| RemoteClientError::io("failed to send remote request", error, true))?;
        let (response, descriptor) =
            transport::receive::<ResponseEnvelope>(stream).map_err(|error| {
                RemoteClientError::io("failed to receive remote response", error, true)
            })?;
        if response.version != PROTOCOL_VERSION {
            return Err(RemoteClientError::new(
                libc::EPROTO,
                "unsupported remote broker protocol version",
            ));
        }
        if response.request_id != request_id {
            return Err(RemoteClientError::new(
                libc::EPROTO,
                "remote broker response request ID did not match",
            ));
        }
        response_result(response.response, descriptor)
    }
}

fn remove_anchor(runtime: Option<&std::path::Path>, anchor: &str) {
    let Some(runtime) = runtime else {
        return;
    };
    let path = runtime.join(anchor);
    let _ = std::fs::remove_file(&path).or_else(|_| std::fs::remove_dir(&path));
}

#[derive(Debug)]
pub(crate) struct RemoteReply {
    pub(crate) response: Response,
    pub(crate) descriptor: Option<OwnedFd>,
}

pub(crate) fn decode_json_descriptor<T>(descriptor: OwnedFd) -> serde_json::Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_reader(BufReader::new(DescriptorReader {
        file: descriptor.into(),
        offset: 0,
    }))
}

struct DescriptorReader {
    file: File,
    offset: u64,
}

impl Read for DescriptorReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.file.read_at(buffer, self.offset)?;
        self.offset = self
            .offset
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::other("remote descriptor offset overflowed"))?;
        Ok(read)
    }
}

#[derive(Debug)]
pub(crate) struct RemoteClientError {
    errno: libc::c_int,
    message: String,
    retryable: bool,
}

impl RemoteClientError {
    fn new(errno: libc::c_int, message: impl Into<String>) -> Self {
        Self {
            errno,
            message: message.into(),
            retryable: false,
        }
    }

    fn io(context: &str, error: std::io::Error, retryable: bool) -> Self {
        let errno = match error.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => libc::ETIMEDOUT,
            _ => error.raw_os_error().unwrap_or(libc::EIO),
        };
        Self {
            errno,
            message: format!("{context}: {error}"),
            retryable,
        }
    }

    pub(crate) fn errno(&self) -> libc::c_int {
        self.errno
    }
}

impl fmt::Display for RemoteClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RemoteClientError {}

fn response_result(
    response: Response,
    descriptor: Option<OwnedFd>,
) -> Result<RemoteReply, RemoteClientError> {
    if let Response::Error { errno, message } = response {
        return Err(RemoteClientError::new(errno, message));
    }
    let expects_descriptor = matches!(
        &response,
        Response::Open { .. } | Response::List { .. } | Response::Read { .. }
    );
    if expects_descriptor != descriptor.is_some() {
        return Err(RemoteClientError::new(
            libc::EPROTO,
            "remote broker descriptor response did not match operation",
        ));
    }
    Ok(RemoteReply {
        response,
        descriptor,
    })
}

#[cfg(test)]
mod tests;
