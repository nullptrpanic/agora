use super::protocol::{
    BackingPath, ByteRange, PROTOCOL_VERSION, Request, RequestEnvelope, Response, ResponseEnvelope,
};
use crate::ipc;
#[cfg(target_os = "macos")]
use crate::ipc::InheritedControlStream;
use std::fmt;
use std::fs::File;
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(1);
const MAX_BUSY_RETRY_DELAY: Duration = Duration::from_millis(25);
const IDEMPOTENT_ATTEMPTS: usize = 2;

#[derive(Clone, Debug)]
pub(crate) struct LocalClient {
    socket: PathBuf,
    token: String,
    #[cfg(target_os = "macos")]
    shared: Option<Arc<InheritedControlStream<UnixStream>>>,
    #[cfg(target_os = "macos")]
    prefer_shared: Arc<AtomicBool>,
    #[cfg(target_os = "macos")]
    observed_pid: Arc<AtomicU32>,
}

pub(crate) struct LocalOpen {
    pub(crate) handle: String,
    pub(crate) descriptor: File,
    pub(crate) state: super::LocalOpenState,
    pub(crate) lock: File,
    pub(crate) identity: LocalFileIdentity,
    pub(crate) lazy: bool,
}

pub(crate) struct LocalFileIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) links: u64,
}

struct LocalReply {
    response: Response,
    descriptors: Vec<OwnedFd>,
}

pub(crate) struct LocalWrite {
    id: String,
}

#[derive(Debug)]
pub(crate) struct LocalClientError {
    errno: libc::c_int,
    message: String,
    retryable: bool,
}

impl LocalClient {
    pub(crate) fn new(socket: impl Into<PathBuf>, token: impl Into<String>) -> Self {
        Self {
            socket: socket.into(),
            token: token.into(),
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
    pub(crate) fn ping_shared(&self) -> Result<(), LocalClientError> {
        let request_id = uuid::Uuid::new_v4().simple().to_string();
        let reply = self.request_shared(request_id, Request::Ping, None)?;
        if !reply.descriptors.is_empty() {
            return Err(LocalClientError::protocol(
                "local filesystem ping unexpectedly returned descriptors",
            ));
        }
        match reply.response {
            Response::Success => Ok(()),
            _ => Err(LocalClientError::protocol(
                "local filesystem ping returned an unexpected response",
            )),
        }
    }

    pub(crate) fn open(
        &self,
        path: &Path,
        flags: libc::c_int,
    ) -> Result<LocalOpen, LocalClientError> {
        let (
            request_id,
            LocalReply {
                response,
                mut descriptors,
            },
        ) = self.request_until_ready(
            Request::Open {
                path: BackingPath::from_path(path),
                flags,
            },
            None,
            IDEMPOTENT_ATTEMPTS,
        )?;
        let Response::Open {
            handle,
            device,
            inode,
            links,
            lazy,
        } = response
        else {
            return Err(LocalClientError::protocol(
                "local filesystem open returned an unexpected response",
            ));
        };
        if descriptors.len() != 3 {
            let _ = self.success(
                Request::Abort {
                    handle: handle.clone(),
                },
                IDEMPOTENT_ATTEMPTS,
            );
            return Err(LocalClientError::protocol(
                "local filesystem open did not return content, state, and lock descriptors",
            ));
        }
        let lock = File::from(descriptors.pop().unwrap());
        let state = match super::LocalOpenState::from_descriptor(descriptors.pop().unwrap()) {
            Ok(state) => state,
            Err(error) => {
                let _ = self.success(
                    Request::Abort {
                        handle: handle.clone(),
                    },
                    IDEMPOTENT_ATTEMPTS,
                );
                return Err(LocalClientError::io(
                    "invalid local open state",
                    error,
                    false,
                ));
            }
        };
        let descriptor = File::from(descriptors.pop().unwrap());
        if let Err(error) = self.success(Request::Claim { request_id }, IDEMPOTENT_ATTEMPTS) {
            let _ = self.success(
                Request::Abort {
                    handle: handle.clone(),
                },
                IDEMPOTENT_ATTEMPTS,
            );
            return Err(error);
        }
        Ok(LocalOpen {
            handle,
            descriptor,
            state,
            lock,
            identity: LocalFileIdentity {
                device,
                inode,
                links,
            },
            lazy,
        })
    }

    pub(crate) fn materialize(
        &self,
        handle: &str,
        range: Option<ByteRange>,
    ) -> Result<(), LocalClientError> {
        self.success_until_ready(
            Request::Materialize {
                handle: handle.to_string(),
                range,
            },
            IDEMPOTENT_ATTEMPTS,
        )
    }

    pub(crate) fn sync(
        &self,
        handle: &str,
        ranges: Vec<ByteRange>,
        durable: bool,
    ) -> Result<(), LocalClientError> {
        self.success_until_ready(
            Request::Sync {
                handle: handle.to_string(),
                ranges,
                durable,
            },
            IDEMPOTENT_ATTEMPTS,
        )
    }

    pub(crate) fn potentially_dirty(
        &self,
        handle: &str,
        range: ByteRange,
    ) -> Result<(), LocalClientError> {
        self.success(
            Request::PotentiallyDirty {
                handle: handle.to_string(),
                range,
            },
            IDEMPOTENT_ATTEMPTS,
        )
    }

    pub(crate) fn begin_write(
        &self,
        handle: &str,
        range: ByteRange,
    ) -> Result<LocalWrite, LocalClientError> {
        let write_id = uuid::Uuid::new_v4().simple().to_string();
        self.success_until_ready(
            Request::BeginWrite {
                handle: handle.to_string(),
                write_id: write_id.clone(),
                range,
            },
            IDEMPOTENT_ATTEMPTS,
        )?;
        Ok(LocalWrite { id: write_id })
    }

    pub(crate) fn begin_append(&self, handle: &str) -> Result<(LocalWrite, u64), LocalClientError> {
        let write_id = uuid::Uuid::new_v4().simple().to_string();
        let (_, reply) = self.request_until_ready(
            Request::BeginAppend {
                handle: handle.to_string(),
                write_id: write_id.clone(),
            },
            None,
            IDEMPOTENT_ATTEMPTS,
        )?;
        if !reply.descriptors.is_empty() {
            return Err(LocalClientError::protocol(
                "local filesystem append unexpectedly returned descriptors",
            ));
        }
        let Response::Offset { offset } = reply.response else {
            return Err(LocalClientError::protocol(
                "local filesystem append returned an unexpected response",
            ));
        };
        Ok((LocalWrite { id: write_id }, offset))
    }

    pub(crate) fn finish_write(
        &self,
        handle: &str,
        write: &LocalWrite,
        range: ByteRange,
    ) -> Result<(), LocalClientError> {
        self.success(
            Request::FinishWrite {
                handle: handle.to_string(),
                write_id: write.id.clone(),
                range,
            },
            IDEMPOTENT_ATTEMPTS,
        )
    }

    pub(crate) fn cancel_write(
        &self,
        handle: &str,
        write: &LocalWrite,
    ) -> Result<(), LocalClientError> {
        self.success(
            Request::CancelWrite {
                handle: handle.to_string(),
                write_id: write.id.clone(),
            },
            IDEMPOTENT_ATTEMPTS,
        )
    }

    pub(crate) fn close(
        &self,
        handle: &str,
        ranges: Vec<ByteRange>,
    ) -> Result<(), LocalClientError> {
        self.success_until_ready(
            Request::Close {
                handle: handle.to_string(),
                ranges,
            },
            IDEMPOTENT_ATTEMPTS,
        )
    }

    pub(crate) fn retain(&self, handles: Vec<String>) -> Result<(), LocalClientError> {
        if handles.is_empty() {
            return Ok(());
        }
        self.success(Request::Retain { handles }, IDEMPOTENT_ATTEMPTS)
    }

    pub(crate) fn release_retained(&self, handles: Vec<String>) -> Result<(), LocalClientError> {
        if handles.is_empty() {
            return Ok(());
        }
        self.success(Request::ReleaseRetain { handles }, IDEMPOTENT_ATTEMPTS)
    }

    fn success(&self, request: Request, attempts: usize) -> Result<(), LocalClientError> {
        let reply = self.request(request, None, attempts)?;
        Self::expect_success(reply)
    }

    fn success_until_ready(
        &self,
        request: Request,
        attempts: usize,
    ) -> Result<(), LocalClientError> {
        let (_, reply) = self.request_until_ready(request, None, attempts)?;
        Self::expect_success(reply)
    }

    fn expect_success(reply: LocalReply) -> Result<(), LocalClientError> {
        if !reply.descriptors.is_empty() {
            return Err(LocalClientError::protocol(
                "local filesystem response unexpectedly included descriptors",
            ));
        }
        match reply.response {
            Response::Success => Ok(()),
            _ => Err(LocalClientError::protocol(
                "local filesystem broker returned an unexpected response",
            )),
        }
    }

    fn request_until_ready(
        &self,
        request: Request,
        descriptor: Option<RawFd>,
        attempts: usize,
    ) -> Result<(String, LocalReply), LocalClientError> {
        let deadline = Instant::now() + CLIENT_TIMEOUT;
        let mut retry_delay = BUSY_RETRY_DELAY;
        loop {
            let request_id = uuid::Uuid::new_v4().simple().to_string();
            match self.request_with_id(request_id.clone(), request.clone(), descriptor, attempts) {
                Ok(reply) => return Ok((request_id, reply)),
                Err(error) if error.errno == libc::EAGAIN && Instant::now() < deadline => {
                    std::thread::sleep(retry_delay);
                    retry_delay = (retry_delay * 2).min(MAX_BUSY_RETRY_DELAY);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn request(
        &self,
        request: Request,
        descriptor: Option<RawFd>,
        attempts: usize,
    ) -> Result<LocalReply, LocalClientError> {
        let request_id = uuid::Uuid::new_v4().simple().to_string();
        self.request_with_id(request_id, request, descriptor, attempts)
    }

    fn request_with_id(
        &self,
        request_id: String,
        request: Request,
        descriptor: Option<RawFd>,
        attempts: usize,
    ) -> Result<LocalReply, LocalClientError> {
        let mut last = None;
        for _ in 0..attempts {
            match self.request_once(request_id.clone(), request.clone(), descriptor) {
                Ok(response) => return Ok(response),
                Err(error) if error.retryable => last = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last.expect("local request attempts is non-zero"))
    }

    fn request_once(
        &self,
        request_id: String,
        request: Request,
        descriptor: Option<RawFd>,
    ) -> Result<LocalReply, LocalClientError> {
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
                return Err(LocalClientError::io(
                    "failed to connect to local filesystem broker",
                    error,
                    true,
                ));
            }
        };
        stream
            .set_read_timeout(Some(CLIENT_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(CLIENT_TIMEOUT)))
            .map_err(|error| {
                LocalClientError::io("failed to configure local filesystem broker", error, false)
            })?;
        Self::exchange(&mut stream, &self.token, request_id, request, descriptor)
    }

    #[cfg(target_os = "macos")]
    fn request_shared(
        &self,
        request_id: String,
        request: Request,
        descriptor: Option<RawFd>,
    ) -> Result<LocalReply, LocalClientError> {
        let shared = self.shared.as_ref().ok_or_else(|| {
            LocalClientError::protocol("shared local filesystem control stream is unavailable")
        })?;
        shared
            .transact(|stream| Self::exchange(stream, &self.token, request_id, request, descriptor))
            .map_err(|error| {
                LocalClientError::io(
                    "failed to serialize inherited local filesystem request",
                    error,
                    true,
                )
            })?
    }

    fn exchange(
        stream: &mut UnixStream,
        token: &str,
        request_id: String,
        request: Request,
        descriptor: Option<RawFd>,
    ) -> Result<LocalReply, LocalClientError> {
        ipc::send(
            stream,
            &RequestEnvelope {
                version: PROTOCOL_VERSION,
                token: token.to_string(),
                request_id: request_id.clone(),
                request,
            },
            descriptor,
        )
        .map_err(|error| {
            LocalClientError::io("failed to send local filesystem request", error, true)
        })?;
        let (response, descriptors) = ipc::receive_with_descriptors::<ResponseEnvelope>(stream)
            .map_err(|error| {
                LocalClientError::io("failed to receive local filesystem response", error, true)
            })?;
        if response.version != PROTOCOL_VERSION || response.request_id != request_id {
            return Err(LocalClientError::protocol(
                "local filesystem response did not match the request",
            ));
        }
        match response.response {
            Response::Error { errno, message } => Err(LocalClientError {
                errno,
                message,
                retryable: false,
            }),
            response => Ok(LocalReply {
                response,
                descriptors,
            }),
        }
    }
}

impl LocalClientError {
    fn protocol(message: impl Into<String>) -> Self {
        Self {
            errno: libc::EPROTO,
            message: message.into(),
            retryable: false,
        }
    }

    fn io(context: &str, error: std::io::Error, retryable: bool) -> Self {
        Self {
            errno: error.raw_os_error().unwrap_or(libc::EIO),
            message: format!("{context}: {error}"),
            retryable,
        }
    }

    pub(crate) fn errno(&self) -> libc::c_int {
        self.errno
    }
}

impl fmt::Display for LocalClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LocalClientError {}

#[cfg(test)]
#[path = "client/tests.rs"]
mod tests;
