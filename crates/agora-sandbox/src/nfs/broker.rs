use crate::filesystem::{ByteRange, ByteRangeSet};
use crate::nfs::backend::{RemoteStorage, StorageError, StorageResult};
use crate::nfs::protocol::{
    MAX_REMOTE_DIRECTORY_ENTRIES, MAX_REMOTE_DIRECTORY_PAYLOAD_BYTES, MAX_REMOTE_FILE_BYTES,
    REMOTE_OPERATION_TIMEOUT, REMOTE_RESET_TIMEOUT, RemoteFileType, RemoteMetadata, RemotePath,
    Request, RequestId, Response,
};
use md5::{Digest, Md5};
use ring::digest::{SHA256, digest};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use uuid::Uuid;

const REQUEST_CACHE_CAPACITY: usize = 4_096;
const REQUEST_CACHE_TTL: Duration = Duration::from_secs(120);
const CLOSED_HANDLE_CAPACITY: usize = 4_096;

pub(crate) struct BrokerReply {
    pub(crate) response: Response,
    pub(crate) descriptor: Option<OwnedFd>,
}

pub(crate) struct Broker<S>
where
    S: RemoteStorage,
{
    storage: Arc<S>,
    staging: PathBuf,
    handles: Mutex<HashMap<String, SharedRemoteHandle<S::FileHandle>>>,
    requests: Mutex<RequestCache>,
    closed_handles: Mutex<HandleTombstones>,
    list_payloads: Mutex<HashMap<String, File>>,
    read_payloads: Mutex<HashMap<String, File>>,
    root_mutations: Mutex<HashMap<u32, Arc<Mutex<()>>>>,
    limits: RemoteLimits,
}

#[derive(Clone, Copy)]
struct RemoteLimits {
    max_file_bytes: u64,
    max_directory_entries: usize,
    max_directory_payload_bytes: u64,
    operation_timeout: Duration,
    reset_timeout: Duration,
}

impl Default for RemoteLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: MAX_REMOTE_FILE_BYTES,
            max_directory_entries: MAX_REMOTE_DIRECTORY_ENTRIES,
            max_directory_payload_bytes: MAX_REMOTE_DIRECTORY_PAYLOAD_BYTES,
            operation_timeout: REMOTE_OPERATION_TIMEOUT,
            reset_timeout: REMOTE_RESET_TIMEOUT,
        }
    }
}

struct RemoteHandle<H> {
    path: RemotePath,
    backend: Option<H>,
    file: Option<File>,
    application: File,
    created: bool,
    pending_truncate: bool,
    backend_dirty: bool,
    writable: bool,
    snapshot: bool,
    unlinked: bool,
    checksum: Option<[u8; 16]>,
    materialized: ByteRangeSet,
    potentially_dirty: ByteRangeSet,
    mapping_fingerprints: Vec<RangeFingerprint>,
    baseline: Option<RemoteMetadata>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RangeFingerprint {
    range: ByteRange,
    checksum: [u8; 16],
}

type SharedRemoteHandle<H> = Arc<Mutex<RemoteHandle<H>>>;

#[derive(Default)]
struct RequestCache {
    entries: HashMap<RequestId, CachedRequest>,
}

enum CachedRequest {
    Pending {
        fingerprint: [u8; 32],
        waiters: Vec<oneshot::Sender<Response>>,
    },
    Completed {
        fingerprint: [u8; 32],
        response: Response,
        completed_at: Instant,
        claimed: bool,
    },
}

enum CacheDecision {
    Execute,
    Wait(oneshot::Receiver<Response>),
    Replay(Response),
    Reject,
}

enum AbandonedResource {
    Handle(String),
    Anchor(String),
    Payload(String),
}

#[derive(Default)]
struct HandleTombstones {
    entries: HashSet<String>,
    order: VecDeque<String>,
}

impl<S> Broker<S>
where
    S: RemoteStorage,
{
    pub(crate) fn new(storage: Arc<S>, staging: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::new_with_limits(storage, staging, RemoteLimits::default())
    }

    fn new_with_limits(
        storage: Arc<S>,
        staging: impl AsRef<Path>,
        limits: RemoteLimits,
    ) -> std::io::Result<Self> {
        let staging = staging.as_ref().to_path_buf();
        std::fs::create_dir_all(&staging)?;
        Ok(Self {
            storage,
            staging,
            handles: Mutex::new(HashMap::new()),
            requests: Mutex::new(RequestCache::default()),
            closed_handles: Mutex::new(HandleTombstones::default()),
            list_payloads: Mutex::new(HashMap::new()),
            read_payloads: Mutex::new(HashMap::new()),
            root_mutations: Mutex::new(HashMap::new()),
            limits,
        })
    }

    #[cfg(test)]
    pub(crate) async fn handle_request(
        &self,
        request_id: RequestId,
        request: Request,
    ) -> BrokerReply {
        self.handle_request_with_descriptor(request_id, request, None)
            .await
    }

    pub(crate) async fn handle_request_with_descriptor(
        &self,
        request_id: RequestId,
        request: Request,
        descriptor: Option<OwnedFd>,
    ) -> BrokerReply {
        let fingerprint = match request_fingerprint(&request) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                return protocol_reply(&format!(
                    "failed to fingerprint remote filesystem request: {error}"
                ));
            }
        };
        let decision = self
            .requests
            .lock()
            .await
            .begin(request_id.clone(), fingerprint);
        match decision {
            CacheDecision::Execute => {
                let reply = self.handle_with_descriptor(request, descriptor).await;
                let abandoned = self
                    .requests
                    .lock()
                    .await
                    .complete(request_id, reply.response.clone());
                self.discard_abandoned_resources(abandoned).await;
                reply
            }
            CacheDecision::Wait(receiver) => match receiver.await {
                Ok(response) => self.reply_for_response(response).await,
                Err(_) => protocol_reply("remote request was cancelled"),
            },
            CacheDecision::Replay(response) => self.reply_for_response(response).await,
            CacheDecision::Reject => {
                protocol_reply("remote request ID was reused for a different operation")
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn handle(&self, request: Request) -> BrokerReply {
        self.handle_with_descriptor(request, None).await
    }

    async fn handle_with_descriptor(
        &self,
        request: Request,
        descriptor: Option<OwnedFd>,
    ) -> BrokerReply {
        let started = Instant::now();
        let deadline = started + self.limits.operation_timeout;
        let root =
            match tokio::time::timeout(self.limits.operation_timeout, self.request_root(&request))
                .await
            {
                Ok(root) => root,
                Err(_) => return error_reply(operation_timed_out()),
            };
        let remaining = self
            .limits
            .operation_timeout
            .saturating_sub(started.elapsed());
        let result =
            tokio::time::timeout(remaining, self.dispatch(request, descriptor, deadline)).await;
        let result = match result {
            Ok(result) => result,
            Err(_) => {
                if let Some(root) = root {
                    let _ =
                        tokio::time::timeout(self.limits.reset_timeout, self.storage.reset(root))
                            .await;
                }
                Err(operation_timed_out())
            }
        };
        result.unwrap_or_else(error_reply)
    }

    async fn request_root(&self, request: &Request) -> Option<u32> {
        match request {
            Request::Ping => None,
            Request::Open { path, .. }
            | Request::Stat { path, .. }
            | Request::List { path, .. }
            | Request::Access { path, .. }
            | Request::CreateDirectory { path, .. }
            | Request::Remove { path, .. } => Some(path.root()),
            Request::Rename { from, .. } => Some(from.root()),
            Request::Metadata { handle }
            | Request::Read { handle, .. }
            | Request::Write { handle, .. }
            | Request::SetLength { handle, .. }
            | Request::Materialize { handle, .. }
            | Request::PotentiallyDirty { handle, .. }
            | Request::Sync { handle, .. }
            | Request::Close { handle, .. } => {
                let handle = self.handles.lock().await.get(handle).cloned()?;
                Some(handle.lock().await.path.root())
            }
            Request::Abort { .. } | Request::Claim { .. } => None,
        }
    }

    async fn dispatch(
        &self,
        request: Request,
        descriptor: Option<OwnedFd>,
        deadline: Instant,
    ) -> StorageResult<BrokerReply> {
        let expects_descriptor = matches!(&request, Request::Write { .. });
        if expects_descriptor != descriptor.is_some() {
            return Err(StorageError::new(
                libc::EPROTO,
                "remote request descriptor did not match operation",
            ));
        }
        match request {
            Request::Ping => Ok(success()),
            Request::Open { path, flags, mode } => self.open(path, flags, mode, deadline).await,
            Request::Stat {
                path,
                name_capacity,
            } => self.stat(path, name_capacity).await,
            Request::List {
                path,
                name_capacity,
            } => self.list(path, name_capacity).await,
            Request::Access { path, mode } => {
                if mode & !(libc::R_OK | libc::W_OK | libc::X_OK) != 0 {
                    return Err(StorageError::new(libc::EINVAL, "invalid access mode"));
                }
                let metadata = self.storage.stat(&path).await?;
                if mode & libc::X_OK != 0 && metadata.file_type == RemoteFileType::File {
                    return Err(StorageError::new(
                        libc::EACCES,
                        "remote regular files are not executable",
                    ));
                }
                Ok(success())
            }
            Request::Metadata { handle } => {
                let metadata = self.metadata(&handle).await?;
                Ok(BrokerReply {
                    response: Response::Metadata { metadata },
                    descriptor: None,
                })
            }
            Request::Read {
                handle,
                offset,
                length,
            } => self.read(&handle, offset, length).await,
            Request::Write {
                handle,
                offset,
                length,
                checksum,
            } => {
                self.write(
                    &handle,
                    offset,
                    length,
                    checksum,
                    descriptor.expect("write descriptor was validated"),
                    deadline,
                )
                .await
            }
            Request::SetLength { handle, length } => self.set_length(&handle, length).await,
            Request::Materialize { handle, range } => {
                if let Some(range) = range {
                    validate_byte_range(range)?;
                }
                self.materialize(&handle, range, deadline).await
            }
            Request::PotentiallyDirty { handle, range } => {
                validate_byte_range(range)?;
                self.register_potentially_dirty(&handle, range, deadline)
                    .await?;
                Ok(success())
            }
            Request::Sync { handle, ranges } => {
                validate_byte_ranges(&ranges)?;
                let metadata = self.sync(&handle, ranges, deadline).await?;
                Ok(BrokerReply {
                    response: Response::Synced { metadata },
                    descriptor: None,
                })
            }
            Request::Close { handle, ranges } => {
                validate_byte_ranges(&ranges)?;
                self.close(&handle, ranges, deadline).await?;
                Ok(success())
            }
            Request::Abort { handle } => {
                self.abort(&handle).await?;
                Ok(success())
            }
            Request::Claim { request_id } => {
                self.claim_request(&request_id).await?;
                Ok(success())
            }
            Request::CreateDirectory { path, mode: _ } => {
                if path.path().is_empty() {
                    return Err(StorageError::new(
                        libc::EEXIST,
                        "remote filesystem root already exists",
                    ));
                }
                let _mutation = self.lock_root(path.root()).await;
                self.storage.create_directory(&path).await?;
                Ok(success())
            }
            Request::Remove { path, directory } => {
                if path.path().is_empty() {
                    return Err(StorageError::new(
                        libc::EACCES,
                        "cannot remove the remote filesystem root",
                    ));
                }
                let _mutation = self.lock_root(path.root()).await;
                self.storage.remove(&path, directory).await?;
                self.discard_handles(&path).await;
                Ok(success())
            }
            Request::Rename { from, to } => {
                if from.path().is_empty() || to.path().is_empty() {
                    return Err(StorageError::new(
                        libc::EBUSY,
                        "cannot rename the remote filesystem root",
                    ));
                }
                if from.root() != to.root() {
                    return Err(StorageError::new(
                        libc::EXDEV,
                        "cannot rename across remote roots",
                    ));
                }
                let _mutation = self.lock_root(from.root()).await;
                self.storage.rename(&from, &to).await?;
                self.retarget_handles(&from, &to).await;
                Ok(success())
            }
        }
    }

    pub(crate) async fn expire_requests(&self) {
        let abandoned = self.requests.lock().await.expire();
        self.discard_abandoned_resources(abandoned).await;
    }

    async fn claim_request(&self, request_id: &RequestId) -> StorageResult<()> {
        let resource = self
            .requests
            .lock()
            .await
            .claim(request_id)
            .ok_or_else(|| {
                StorageError::new(
                    libc::EPROTO,
                    "remote resource request is not available to claim",
                )
            })?;
        if let Some(resource) = resource {
            self.list_payloads.lock().await.remove(&resource);
            self.read_payloads.lock().await.remove(&resource);
        }
        Ok(())
    }

    async fn reply_for_response(&self, response: Response) -> BrokerReply {
        let descriptor = match &response {
            Response::Open { handle, .. } => match self.application_descriptor(handle).await {
                Ok(descriptor) => Some(descriptor),
                Err(error) => {
                    return BrokerReply {
                        response: Response::Error {
                            errno: error.errno,
                            message: error.message,
                        },
                        descriptor: None,
                    };
                }
            },
            Response::List { anchor } => match self.list_descriptor(anchor).await {
                Ok(descriptor) => Some(descriptor),
                Err(error) => {
                    return BrokerReply {
                        response: Response::Error {
                            errno: error.errno,
                            message: error.message,
                        },
                        descriptor: None,
                    };
                }
            },
            Response::Read { payload, .. } => match self.read_descriptor(payload).await {
                Ok(descriptor) => Some(descriptor),
                Err(error) => {
                    return BrokerReply {
                        response: Response::Error {
                            errno: error.errno,
                            message: error.message,
                        },
                        descriptor: None,
                    };
                }
            },
            _ => None,
        };
        BrokerReply {
            response,
            descriptor,
        }
    }

    async fn application_descriptor(&self, id: &str) -> StorageResult<OwnedFd> {
        let handle = self
            .handles
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| StorageError::new(libc::EBADF, "unknown remote handle"))?;
        let descriptor = handle
            .lock()
            .await
            .application
            .try_clone()
            .map_err(|error| storage_io("failed to duplicate remote descriptor", error))?;
        Ok(descriptor.into())
    }

    async fn list_descriptor(&self, anchor: &str) -> StorageResult<OwnedFd> {
        self.list_payloads
            .lock()
            .await
            .get(anchor)
            .ok_or_else(|| StorageError::new(libc::EPROTO, "remote list payload is unavailable"))?
            .try_clone()
            .map(Into::into)
            .map_err(|error| storage_io("failed to duplicate remote list payload", error))
    }

    async fn read_descriptor(&self, payload: &str) -> StorageResult<OwnedFd> {
        self.read_payloads
            .lock()
            .await
            .get(payload)
            .ok_or_else(|| StorageError::new(libc::EPROTO, "remote read payload is unavailable"))?
            .try_clone()
            .map(Into::into)
            .map_err(|error| storage_io("failed to duplicate remote read payload", error))
    }

    async fn discard_abandoned_resources(&self, resources: Vec<AbandonedResource>) {
        let mut abandoned_handles = Vec::new();
        for resource in resources {
            match resource {
                AbandonedResource::Handle(handle) => {
                    if let Some(open) = self.handles.lock().await.remove(&handle) {
                        self.closed_handles.lock().await.insert(handle);
                        abandoned_handles.push(open);
                    }
                }
                AbandonedResource::Anchor(anchor) => {
                    self.list_payloads.lock().await.remove(&anchor);
                    let path = self.staging.join(anchor);
                    let removed = std::fs::remove_file(&path)
                        .or_else(|file_error| std::fs::remove_dir(&path).map_err(|_| file_error));
                    if let Err(error) = removed
                        && error.kind() != std::io::ErrorKind::NotFound
                    {
                        // Cleanup is best effort; the staging directory is removed
                        // when the controller exits.
                    }
                }
                AbandonedResource::Payload(payload) => {
                    self.read_payloads.lock().await.remove(&payload);
                }
            }
        }
        for handle in abandoned_handles {
            let _ = self.discard_handle(&handle).await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn handle_count_for_test(&self) -> usize {
        self.handles.lock().await.len()
    }

    async fn open(
        &self,
        path: RemotePath,
        flags: libc::c_int,
        mode: u32,
        _deadline: Instant,
    ) -> StorageResult<BrokerReply> {
        let access = flags & libc::O_ACCMODE;
        let (readable, writable) = match access {
            libc::O_RDONLY => (true, false),
            libc::O_WRONLY => (false, true),
            libc::O_RDWR => (true, true),
            _ => return Err(StorageError::new(libc::EINVAL, "invalid open access mode")),
        };
        if flags & libc::O_TRUNC != 0 && !writable {
            return Err(StorageError::new(
                libc::EINVAL,
                "O_TRUNC requires write access",
            ));
        }
        let _mutation = self.lock_root(path.root()).await;
        let existing = match self.storage.stat(&path).await {
            Ok(metadata) => Some(metadata),
            Err(error) if error.errno() == libc::ENOENT => None,
            Err(error) => return Err(error),
        };
        if existing.is_some()
            && flags & (libc::O_CREAT | libc::O_EXCL) == libc::O_CREAT | libc::O_EXCL
        {
            return Err(StorageError::new(
                libc::EEXIST,
                "remote path already exists",
            ));
        }
        if existing.is_none() && flags & libc::O_CREAT == 0 {
            return Err(StorageError::not_found());
        }
        if let Some(metadata) = existing.as_ref()
            && metadata.file_type == RemoteFileType::Directory
        {
            if writable || flags & (libc::O_CREAT | libc::O_TRUNC) != 0 {
                return Err(StorageError::new(
                    libc::EISDIR,
                    "remote path is a directory",
                ));
            }
            return self.open_directory(path, metadata.clone()).await;
        }
        if flags & libc::O_DIRECTORY != 0 {
            return Err(StorageError::new(
                if existing.is_some() {
                    libc::ENOTDIR
                } else {
                    libc::ENOENT
                },
                "remote path is not a directory",
            ));
        }
        let backend_flags = flags & !libc::O_TRUNC;
        let (backend, metadata, created) =
            self.storage.open_file(&path, backend_flags, mode).await?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.staging)
            .map_err(|error| storage_io("failed to create anonymous remote file", error))?;
        temporary
            .as_file_mut()
            .set_len(metadata.size)
            .map_err(|error| storage_io("failed to size remote placeholder", error))?;
        temporary
            .flush()
            .map_err(|error| storage_io("failed to flush anonymous remote file", error))?;
        let retained = temporary
            .reopen()
            .map_err(|error| storage_io("failed to retain anonymous remote file", error))?;
        let mut options = OpenOptions::new();
        options.read(readable).write(writable).custom_flags(
            libc::O_CLOEXEC
                | (flags & (libc::O_APPEND | libc::O_NONBLOCK | libc::O_SYNC | libc::O_DSYNC)),
        );
        let application = options
            .open(temporary.path())
            .map_err(|error| storage_io("failed to open anonymous remote file", error))?;
        std::fs::remove_file(temporary.path())
            .map_err(|error| storage_io("failed to unlink anonymous remote file", error))?;
        drop(temporary);
        set_close_on_exec(&application)?;
        let replay = application
            .try_clone()
            .map_err(|error| storage_io("failed to retain remote descriptor", error))?;
        let handle = Uuid::new_v4().simple().to_string();
        self.handles.lock().await.insert(
            handle.clone(),
            Arc::new(Mutex::new(RemoteHandle {
                path,
                backend: Some(backend),
                file: Some(retained),
                application: replay,
                created,
                pending_truncate: flags & libc::O_TRUNC != 0 && !created,
                backend_dirty: false,
                writable,
                snapshot: false,
                unlinked: false,
                checksum: None,
                materialized: ByteRangeSet::default(),
                potentially_dirty: ByteRangeSet::default(),
                mapping_fingerprints: Vec::new(),
                baseline: Some(metadata.clone()),
            })),
        );
        Ok(BrokerReply {
            response: Response::Open { handle, metadata },
            descriptor: Some(application.into()),
        })
    }

    async fn open_directory(
        &self,
        path: RemotePath,
        metadata: RemoteMetadata,
    ) -> StorageResult<BrokerReply> {
        let anchor = self.anchor(&path, RemoteFileType::Directory, 0).await?;
        let physical = self.staging.join(&anchor);
        let application = File::open(&physical)
            .map_err(|error| storage_io("failed to open remote directory anchor", error))?;
        std::fs::remove_dir(&physical)
            .map_err(|error| storage_io("failed to unlink remote directory anchor", error))?;
        set_close_on_exec(&application)?;
        let replay = application
            .try_clone()
            .map_err(|error| storage_io("failed to retain remote directory descriptor", error))?;
        let handle = Uuid::new_v4().simple().to_string();
        self.handles.lock().await.insert(
            handle.clone(),
            Arc::new(Mutex::new(RemoteHandle {
                path,
                backend: None,
                file: None,
                application: replay,
                created: false,
                pending_truncate: false,
                backend_dirty: false,
                writable: false,
                snapshot: false,
                unlinked: false,
                checksum: None,
                materialized: ByteRangeSet::default(),
                potentially_dirty: ByteRangeSet::default(),
                mapping_fingerprints: Vec::new(),
                baseline: Some(metadata.clone()),
            })),
        );
        Ok(BrokerReply {
            response: Response::Open { handle, metadata },
            descriptor: Some(application.into()),
        })
    }

    async fn stat(&self, path: RemotePath, name_capacity: u16) -> StorageResult<BrokerReply> {
        let metadata = self.storage.stat(&path).await?;
        let anchor = self
            .anchor(&path, metadata.file_type, name_capacity)
            .await?;
        Ok(BrokerReply {
            response: Response::Stat { metadata, anchor },
            descriptor: None,
        })
    }

    async fn list(&self, path: RemotePath, name_capacity: u16) -> StorageResult<BrokerReply> {
        let mut payload = tempfile::tempfile_in(&self.staging)
            .map_err(|error| storage_io("failed to create remote list payload", error))?;
        {
            let mut limited =
                LimitedWriter::new(&mut payload, self.limits.max_directory_payload_bytes);
            limited
                .write_all(b"[")
                .map_err(|error| list_payload_error(error, limited.exceeded()))?;
            let mut count = 0_usize;
            {
                let mut emit = |entry: crate::nfs::protocol::RemoteEntry| {
                    if count == self.limits.max_directory_entries {
                        return Err(directory_too_large());
                    }
                    if count != 0 {
                        limited
                            .write_all(b",")
                            .map_err(|error| list_payload_error(error, limited.exceeded()))?;
                    }
                    serde_json::to_writer(&mut limited, &entry)
                        .map_err(|error| list_payload_error(error.into(), limited.exceeded()))?;
                    count += 1;
                    Ok(())
                };
                self.storage.list(&path, &mut emit).await?;
            }
            limited
                .write_all(b"]")
                .map_err(|error| list_payload_error(error, limited.exceeded()))?;
        }
        payload
            .flush()
            .and_then(|()| payload.seek(SeekFrom::Start(0)).map(|_| ()))
            .map_err(|error| storage_io("failed to prepare remote list payload", error))?;
        let anchor = self
            .anchor(&path, RemoteFileType::Directory, name_capacity)
            .await?;
        let descriptor = payload
            .try_clone()
            .map_err(|error| storage_io("failed to duplicate remote list payload", error))?;
        self.list_payloads
            .lock()
            .await
            .insert(anchor.clone(), payload);
        Ok(BrokerReply {
            response: Response::List { anchor },
            descriptor: Some(descriptor.into()),
        })
    }

    async fn anchor(
        &self,
        path: &RemotePath,
        file_type: RemoteFileType,
        name_capacity: u16,
    ) -> StorageResult<String> {
        let _ = path;
        let mut anchor = format!("anchor-{}", Uuid::new_v4().simple());
        anchor.extend(std::iter::repeat_n(
            'x',
            usize::from(name_capacity).saturating_sub(anchor.len()),
        ));
        let physical = self.staging.join(&anchor);
        match file_type {
            RemoteFileType::File => {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&physical)
                    .map_err(|error| storage_io("failed to create remote file anchor", error))?;
            }
            RemoteFileType::Directory => {
                std::fs::create_dir(&physical).map_err(|error| {
                    storage_io("failed to create remote directory anchor", error)
                })?;
            }
        }
        Ok(anchor)
    }

    async fn read(&self, id: &str, offset: u64, length: u32) -> StorageResult<BrokerReply> {
        if length > crate::nfs::protocol::MAX_REMOTE_IO_BYTES {
            return Err(StorageError::new(
                libc::EINVAL,
                "remote read exceeds the per-operation limit",
            ));
        }
        let handle = self.open_handle(id).await?;
        let mut handle = handle.lock().await;
        if handle.snapshot {
            return Err(StorageError::new(
                libc::EBUSY,
                "remote handle is using a local mmap snapshot",
            ));
        }
        let backend = handle
            .backend
            .as_mut()
            .ok_or_else(|| StorageError::new(libc::EISDIR, "remote handle is a directory"))?;
        let mut payload = tempfile::tempfile_in(&self.staging)
            .map_err(|error| storage_io("failed to create remote read payload", error))?;
        let length = self
            .storage
            .read_at(backend, offset, length, &mut payload)
            .await?;
        payload
            .flush()
            .and_then(|()| payload.seek(SeekFrom::Start(0)).map(|_| ()))
            .map_err(|error| storage_io("failed to prepare remote read payload", error))?;
        let descriptor = payload
            .try_clone()
            .map_err(|error| storage_io("failed to duplicate remote read payload", error))?;
        let payload_id = Uuid::new_v4().simple().to_string();
        self.read_payloads
            .lock()
            .await
            .insert(payload_id.clone(), payload);
        Ok(BrokerReply {
            response: Response::Read {
                payload: payload_id,
                length,
            },
            descriptor: Some(descriptor.into()),
        })
    }

    async fn metadata(&self, id: &str) -> StorageResult<RemoteMetadata> {
        let handle = self.open_handle(id).await?;
        let mut handle = handle.lock().await;
        if handle.snapshot || handle.backend.is_none() {
            return handle
                .baseline
                .clone()
                .ok_or_else(|| StorageError::new(libc::EBADF, "remote metadata is unavailable"));
        }
        let backend = handle
            .backend
            .as_mut()
            .ok_or_else(|| StorageError::new(libc::EISDIR, "remote handle is a directory"))?;
        let metadata = self.storage.file_metadata(backend).await?;
        handle.baseline = Some(metadata.clone());
        Ok(metadata)
    }

    async fn write(
        &self,
        id: &str,
        offset: Option<u64>,
        length: u32,
        checksum: [u8; 16],
        descriptor: OwnedFd,
        deadline: Instant,
    ) -> StorageResult<BrokerReply> {
        if length > crate::nfs::protocol::MAX_REMOTE_IO_BYTES {
            return Err(StorageError::new(
                libc::EINVAL,
                "remote write exceeds the per-operation limit",
            ));
        }
        let mut source = File::from(descriptor);
        let source_length = source
            .metadata()
            .map_err(|error| storage_io("failed to inspect remote write payload", error))?
            .len();
        if source_length != u64::from(length) {
            return Err(StorageError::new(
                libc::EPROTO,
                "remote write payload length did not match its request",
            ));
        }
        if checksum_file(
            source
                .try_clone()
                .map_err(|error| storage_io("failed to clone remote write payload", error))?,
            deadline,
        )
        .await?
            != checksum
        {
            return Err(StorageError::new(
                libc::EPROTO,
                "remote write payload checksum did not match its request",
            ));
        }
        let handle = self.open_handle(id).await?;
        let root = handle.lock().await.path.root();
        let _append_mutation = if offset.is_none() {
            Some(self.lock_root(root).await)
        } else {
            None
        };
        let mut handle = handle.lock().await;
        if handle.snapshot {
            return Err(StorageError::new(
                libc::EBUSY,
                "remote handle is using a local mmap snapshot",
            ));
        }
        let offset = if let Some(offset) = offset {
            offset
        } else {
            let backend = handle
                .backend
                .as_mut()
                .ok_or_else(|| StorageError::new(libc::EISDIR, "remote handle is a directory"))?;
            let metadata = self.storage.file_metadata(backend).await?;
            let offset = metadata.size;
            handle.baseline = Some(metadata);
            offset
        };
        let backend = handle
            .backend
            .as_mut()
            .ok_or_else(|| StorageError::new(libc::EISDIR, "remote handle is a directory"))?;
        let (length, size) = self
            .storage
            .write_at(backend, offset, &mut source, length)
            .await?;
        handle.backend_dirty = true;
        if let Some(metadata) = handle.baseline.as_mut() {
            metadata.size = size;
        }
        Ok(BrokerReply {
            response: Response::Written {
                offset,
                length,
                size,
            },
            descriptor: None,
        })
    }

    async fn set_length(&self, id: &str, length: u64) -> StorageResult<BrokerReply> {
        let handle = self.open_handle(id).await?;
        let mut handle = handle.lock().await;
        if handle.snapshot {
            return Err(StorageError::new(
                libc::EBUSY,
                "remote handle is using a local mmap snapshot",
            ));
        }
        let backend = handle
            .backend
            .as_mut()
            .ok_or_else(|| StorageError::new(libc::EISDIR, "remote handle is a directory"))?;
        let size = self.storage.set_length(backend, length).await?;
        handle.backend_dirty = true;
        if let Some(metadata) = handle.baseline.as_mut() {
            metadata.size = size;
        }
        Ok(BrokerReply {
            response: Response::Resized { size },
            descriptor: None,
        })
    }

    async fn materialize(
        &self,
        id: &str,
        range: Option<ByteRange>,
        deadline: Instant,
    ) -> StorageResult<BrokerReply> {
        let handle = self.open_handle(id).await?;
        let root = handle.lock().await.path.root();
        let _mutation = self.lock_root(root).await;
        let mut handle = handle.lock().await;
        if !handle.snapshot && handle.backend_dirty {
            let metadata = {
                let backend = handle.backend.as_mut().ok_or_else(|| {
                    StorageError::new(libc::EBADF, "remote file handle is closed")
                })?;
                self.storage.flush_file(backend).await?
            };
            handle.baseline = Some(metadata);
            handle.backend_dirty = false;
        }
        let baseline = handle
            .baseline
            .clone()
            .ok_or_else(|| StorageError::new(libc::EBADF, "remote metadata is unavailable"))?;
        if range.is_none() && !handle.snapshot && handle.materialized.is_empty() {
            let (metadata, checksum) = if baseline.size == 0 {
                let current = self.snapshot_backend_metadata(&mut handle).await?;
                ensure_same_snapshot(&baseline, &current)?;
                let file = handle.file.as_ref().ok_or_else(|| {
                    StorageError::new(libc::EBADF, "remote placeholder is unavailable")
                })?;
                let checksum = checksum_file(
                    file.try_clone().map_err(|error| {
                        storage_io("failed to clone empty remote snapshot", error)
                    })?,
                    deadline,
                )
                .await?;
                (current, checksum)
            } else {
                let RemoteHandle { backend, file, .. } = &mut *handle;
                let backend = backend.as_mut().ok_or_else(|| {
                    StorageError::new(libc::EISDIR, "remote handle is a directory")
                })?;
                let file = file.as_mut().ok_or_else(|| {
                    StorageError::new(libc::EBADF, "remote placeholder is unavailable")
                })?;
                let metadata = self
                    .storage
                    .read_into(backend, file, self.limits.max_file_bytes)
                    .await?;
                file.flush()
                    .map_err(|error| storage_io("failed to flush remote mmap snapshot", error))?;
                let checksum = checksum_file(
                    file.try_clone().map_err(|error| {
                        storage_io("failed to clone remote mmap snapshot", error)
                    })?,
                    deadline,
                )
                .await?;
                (metadata, checksum)
            };
            self.close_snapshot_backend(&mut handle).await?;
            if metadata.size != 0 {
                handle.materialized.insert(ByteRange {
                    start: 0,
                    end: metadata.size,
                });
            }
            handle.snapshot = true;
            handle.checksum = Some(checksum);
            handle.baseline = Some(metadata.clone());
            return Ok(BrokerReply {
                response: Response::Materialized { metadata },
                descriptor: None,
            });
        }

        let end = range.map_or(baseline.size, |range| range.end.min(baseline.size));
        let start = range.map_or(0, |range| range.start.min(end));
        if end - start > self.limits.max_file_bytes {
            return Err(file_too_large());
        }
        if start < end {
            self.materialize_snapshot_range(&mut handle, ByteRange { start, end })
                .await?;
        }
        handle.snapshot = true;
        if range.is_none() {
            self.close_snapshot_backend(&mut handle).await?;
        }
        Ok(BrokerReply {
            response: Response::Materialized { metadata: baseline },
            descriptor: None,
        })
    }

    async fn materialize_snapshot_range(
        &self,
        handle: &mut RemoteHandle<S::FileHandle>,
        requested: ByteRange,
    ) -> StorageResult<()> {
        let missing = handle.materialized.missing(requested);
        if missing.is_empty() {
            return Ok(());
        }
        let baseline = handle
            .baseline
            .clone()
            .ok_or_else(|| StorageError::new(libc::EBADF, "remote metadata is unavailable"))?;
        let current = self.snapshot_backend_metadata(handle).await?;
        ensure_same_snapshot(&baseline, &current)?;

        let mut payload = tempfile::tempfile_in(&self.staging)
            .map_err(|error| storage_io("failed to create remote snapshot payload", error))?;
        for range in &missing {
            let mut offset = range.start;
            while offset < range.end {
                let length = u32::try_from(
                    (range.end - offset).min(u64::from(crate::nfs::protocol::MAX_REMOTE_IO_BYTES)),
                )
                .expect("remote snapshot chunks are protocol bounded");
                let actual = {
                    let backend = handle.backend.as_mut().ok_or_else(|| {
                        StorageError::new(libc::EBADF, "remote file handle is closed")
                    })?;
                    self.storage
                        .read_at(backend, offset, length, &mut payload)
                        .await?
                };
                if actual != length {
                    return Err(StorageError::new(
                        libc::ESTALE,
                        "remote file changed while creating a snapshot",
                    ));
                }
                let file = handle.file.as_ref().ok_or_else(|| {
                    StorageError::new(libc::EBADF, "remote placeholder is unavailable")
                })?;
                copy_file_range_at(&payload, file, offset, actual)?;
                let end = offset + u64::from(actual);
                offset = end;
            }
        }

        let current = self.snapshot_backend_metadata(handle).await?;
        ensure_same_snapshot(&baseline, &current)?;
        for range in missing {
            handle.materialized.insert(range);
        }
        Ok(())
    }

    async fn snapshot_backend_metadata(
        &self,
        handle: &mut RemoteHandle<S::FileHandle>,
    ) -> StorageResult<RemoteMetadata> {
        let backend = handle
            .backend
            .as_mut()
            .ok_or_else(|| StorageError::new(libc::EBADF, "remote file handle is closed"))?;
        self.storage.file_metadata(backend).await
    }

    async fn close_snapshot_backend(
        &self,
        handle: &mut RemoteHandle<S::FileHandle>,
    ) -> StorageResult<()> {
        if let Some(mut backend) = handle.backend.take()
            && let Err(error) = self.storage.close_file(&mut backend).await
        {
            handle.backend = Some(backend);
            return Err(error);
        }
        Ok(())
    }

    async fn materialize_whole_snapshot_baseline(
        &self,
        handle: &mut RemoteHandle<S::FileHandle>,
        length: u64,
    ) -> StorageResult<()> {
        let baseline_size = handle.baseline.as_ref().map_or(0, |metadata| metadata.size);
        let end = length.min(baseline_size);
        if end != 0 {
            self.materialize_snapshot_range(handle, ByteRange { start: 0, end })
                .await?;
        }
        self.close_snapshot_backend(handle).await
    }

    async fn register_potentially_dirty(
        &self,
        id: &str,
        range: ByteRange,
        deadline: Instant,
    ) -> StorageResult<()> {
        let handle = self.open_handle(id).await?;
        let mut handle = handle.lock().await;
        if !handle.writable {
            return Err(StorageError::new(
                libc::EBADF,
                "remote filesystem handle is not writable",
            ));
        }
        if !handle.snapshot {
            return Err(StorageError::new(
                libc::EBUSY,
                "remote handle is not using a local mmap snapshot",
            ));
        }
        let file = handle
            .file
            .as_ref()
            .ok_or_else(|| StorageError::new(libc::EBADF, "remote file handle is closed"))?;
        let length = file
            .metadata()
            .map_err(|error| storage_io("failed to inspect anonymous remote file", error))?
            .len();
        let missing = handle.potentially_dirty.missing(range);
        if missing.is_empty() {
            return Ok(());
        }
        let fingerprints = fingerprint_file_ranges(
            file.try_clone()
                .map_err(|error| storage_io("failed to clone anonymous remote file", error))?,
            missing.clone(),
            length,
            deadline,
        )
        .await?;
        for range in missing {
            handle.materialized.insert(range);
            handle.potentially_dirty.insert(range);
        }
        handle.mapping_fingerprints.extend(fingerprints);
        handle
            .mapping_fingerprints
            .sort_unstable_by_key(|fingerprint| fingerprint.range.start);
        Ok(())
    }

    async fn open_handle(&self, id: &str) -> StorageResult<SharedRemoteHandle<S::FileHandle>> {
        self.handles
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| StorageError::new(libc::EBADF, "unknown remote handle"))
    }

    async fn sync(
        &self,
        id: &str,
        ranges: Vec<ByteRange>,
        deadline: Instant,
    ) -> StorageResult<Option<RemoteMetadata>> {
        let handle = self
            .handles
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| StorageError::new(libc::EBADF, "unknown remote handle"))?;
        self.sync_handle(&handle, ranges, deadline).await
    }

    async fn sync_handle(
        &self,
        handle: &SharedRemoteHandle<S::FileHandle>,
        ranges: Vec<ByteRange>,
        deadline: Instant,
    ) -> StorageResult<Option<RemoteMetadata>> {
        let root = handle.lock().await.path.root();
        let _mutation = self.lock_root(root).await;
        let mut handle = handle.lock().await;
        let explicitly_dirty = !ranges.is_empty();
        for range in ranges {
            handle.materialized.insert(range);
        }
        self.sync_locked(&mut handle, explicitly_dirty, deadline)
            .await
    }

    async fn sync_locked(
        &self,
        handle: &mut RemoteHandle<S::FileHandle>,
        explicitly_dirty: bool,
        deadline: Instant,
    ) -> StorageResult<Option<RemoteMetadata>> {
        if !handle.snapshot {
            if handle.pending_truncate {
                let backend = handle.backend.as_mut().ok_or_else(|| {
                    StorageError::new(libc::EBADF, "remote file handle is closed")
                })?;
                self.storage.set_length(backend, 0).await?;
                handle
                    .file
                    .as_ref()
                    .ok_or_else(|| {
                        StorageError::new(libc::EBADF, "remote placeholder is unavailable")
                    })?
                    .set_len(0)
                    .map_err(|error| storage_io("failed to truncate remote placeholder", error))?;
                if let Some(metadata) = handle.baseline.as_mut() {
                    metadata.size = 0;
                }
                handle.pending_truncate = false;
                handle.backend_dirty = false;
            }
            if !handle.writable {
                return Ok((!handle.unlinked)
                    .then(|| handle.baseline.clone())
                    .flatten());
            }
            let backend = handle
                .backend
                .as_mut()
                .ok_or_else(|| StorageError::new(libc::EBADF, "remote file handle is closed"))?;
            let metadata = self.storage.flush_file(backend).await?;
            handle.baseline = Some(metadata.clone());
            handle.backend_dirty = false;
            return Ok((!handle.unlinked).then_some(metadata));
        }
        if handle.unlinked {
            return Ok(None);
        }
        if !handle.writable {
            return Ok(handle.baseline.clone());
        }
        let file = handle
            .file
            .as_ref()
            .ok_or_else(|| StorageError::new(libc::EBADF, "remote file handle is closed"))?;
        let length = file
            .metadata()
            .map_err(|error| storage_io("failed to inspect anonymous remote file", error))?
            .len();
        if length > self.limits.max_file_bytes {
            return Err(file_too_large());
        }
        let baseline_length = handle.baseline.as_ref().map_or(0, |metadata| metadata.size);
        let mut changed = explicitly_dirty || length != baseline_length;
        let current_mapping_fingerprints = if handle.mapping_fingerprints.is_empty() {
            None
        } else {
            let ranges = handle
                .mapping_fingerprints
                .iter()
                .map(|fingerprint| fingerprint.range)
                .collect();
            Some(
                fingerprint_file_ranges(
                    file.try_clone().map_err(|error| {
                        storage_io("failed to clone remote mmap snapshot", error)
                    })?,
                    ranges,
                    length,
                    deadline,
                )
                .await?,
            )
        };
        if !changed
            && current_mapping_fingerprints
                .as_ref()
                .is_some_and(|current| current != &handle.mapping_fingerprints)
        {
            changed = true;
        }
        if !changed && let Some(expected) = handle.checksum {
            let current = checksum_file(
                file.try_clone().map_err(|error| {
                    storage_io("failed to clone remote snapshot for checksum", error)
                })?,
                deadline,
            )
            .await?;
            changed = current != expected;
        }
        if !changed {
            return Ok(handle.baseline.clone());
        }

        self.materialize_whole_snapshot_baseline(handle, length)
            .await?;
        let file = handle
            .file
            .as_ref()
            .ok_or_else(|| StorageError::new(libc::EBADF, "remote file handle is closed"))?;
        let mut snapshot = file
            .try_clone()
            .map_err(|error| storage_io("failed to clone anonymous remote file", error))?;
        snapshot
            .seek(SeekFrom::Start(0))
            .map_err(|error| storage_io("failed to rewind anonymous remote file", error))?;
        let checksum = checksum_file(
            snapshot.try_clone().map_err(|error| {
                storage_io("failed to clone remote snapshot for checksum", error)
            })?,
            deadline,
        )
        .await?;
        if handle.checksum == Some(checksum) {
            if let Some(fingerprints) = current_mapping_fingerprints {
                handle.mapping_fingerprints = fingerprints;
            }
            return Ok(handle.baseline.clone());
        }
        let metadata = self
            .storage
            .write_from_if_unchanged(
                &handle.path,
                handle.baseline.as_ref(),
                &mut snapshot,
                length,
            )
            .await?;
        handle.baseline = Some(metadata.clone());
        handle.checksum = Some(checksum);
        if let Some(fingerprints) = current_mapping_fingerprints {
            handle.mapping_fingerprints = fingerprints;
        }
        handle.materialized.clear();
        if length != 0 {
            handle.materialized.insert(ByteRange {
                start: 0,
                end: length,
            });
        }
        Ok(Some(metadata))
    }

    async fn close(
        &self,
        id: &str,
        ranges: Vec<ByteRange>,
        deadline: Instant,
    ) -> StorageResult<()> {
        let handle = match self.handles.lock().await.get(id).cloned() {
            Some(handle) => handle,
            None if self.closed_handles.lock().await.contains(id) => return Ok(()),
            None => return Err(StorageError::new(libc::EBADF, "unknown remote handle")),
        };
        self.sync_handle(&handle, ranges, deadline).await?;
        {
            let mut handle = handle.lock().await;
            self.close_snapshot_backend(&mut handle).await?;
        }
        let mut handles = self.handles.lock().await;
        if handles
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, &handle))
        {
            handles.remove(id);
            self.closed_handles.lock().await.insert(id.to_string());
        }
        Ok(())
    }

    async fn abort(&self, id: &str) -> StorageResult<()> {
        let handle = { self.handles.lock().await.get(id).cloned() };
        if let Some(handle) = handle {
            self.discard_handle(&handle).await?;
            self.handles.lock().await.remove(id);
            self.closed_handles.lock().await.insert(id.to_string());
            return Ok(());
        }
        if self.closed_handles.lock().await.contains(id) {
            Ok(())
        } else {
            Err(StorageError::new(libc::EBADF, "unknown remote handle"))
        }
    }

    async fn discard_handle(
        &self,
        handle: &SharedRemoteHandle<S::FileHandle>,
    ) -> StorageResult<()> {
        let (path, created, baseline) = {
            let mut handle = handle.lock().await;
            if let Some(mut backend) = handle.backend.take()
                && let Err(error) = self.storage.close_file(&mut backend).await
            {
                handle.backend = Some(backend);
                return Err(error);
            }
            (handle.path.clone(), handle.created, handle.baseline.clone())
        };
        if !created {
            return Ok(());
        }
        let _mutation = self.lock_root(path.root()).await;
        match self.storage.stat(&path).await {
            Ok(current)
                if baseline
                    .as_ref()
                    .is_some_and(|baseline| baseline.identity == current.identity) =>
            {
                self.storage.remove(&path, false).await
            }
            Ok(_) => Ok(()),
            Err(error) if error.errno() == libc::ENOENT => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn retarget_handles(&self, from: &RemotePath, to: &RemotePath) {
        let handles = self
            .handles
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for handle in handles {
            let mut handle = handle.lock().await;
            if path_is_at_or_below(&handle.path, to) {
                handle.unlinked = true;
            } else if let Some(retargeted) = retarget_path(&handle.path, from, to) {
                handle.path = retargeted;
            }
        }
    }

    async fn discard_handles(&self, path: &RemotePath) {
        let handles = self
            .handles
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for handle in handles {
            let mut handle = handle.lock().await;
            if handle.path == *path {
                handle.unlinked = true;
            }
        }
    }

    async fn lock_root(&self, root: u32) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .root_mutations
            .lock()
            .await
            .entry(root)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        lock.lock_owned().await
    }
}

fn list_payload_error(error: std::io::Error, exceeded: bool) -> StorageError {
    if exceeded {
        directory_too_large()
    } else {
        storage_io("failed to serialize remote list", error)
    }
}

async fn fingerprint_file_ranges(
    file: File,
    ranges: Vec<ByteRange>,
    length: u64,
    deadline: Instant,
) -> StorageResult<Vec<RangeFingerprint>> {
    let maximum_duration = remaining_until(deadline)?;
    let worker = tokio::task::spawn_blocking(move || {
        let mut buffer = [0_u8; 64 * 1024];
        let mut fingerprints = Vec::with_capacity(ranges.len());
        for range in ranges {
            let mut digest = Md5::new();
            let mut offset = range.start.min(length);
            let end = range.end.min(length);
            while offset < end {
                if Instant::now() >= deadline {
                    return Err(operation_timed_out());
                }
                let requested = usize::try_from((end - offset).min(buffer.len() as u64))
                    .expect("remote fingerprint chunks fit usize");
                let read = file
                    .read_at(&mut buffer[..requested], offset)
                    .map_err(|error| {
                        storage_io("failed to fingerprint remote mmap snapshot", error)
                    })?;
                if read == 0 {
                    return Err(StorageError::new(
                        libc::EIO,
                        "remote mmap snapshot ended while fingerprinting a mapped range",
                    ));
                }
                digest.update(&buffer[..read]);
                offset += read as u64;
            }
            fingerprints.push(RangeFingerprint {
                range,
                checksum: digest.finalize().into(),
            });
        }
        Ok(fingerprints)
    });
    match tokio::time::timeout(maximum_duration, worker).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(StorageError::new(
            libc::EIO,
            format!("remote fingerprint worker failed: {error}"),
        )),
        Err(_) => Err(operation_timed_out()),
    }
}

async fn checksum_file(file: File, deadline: Instant) -> StorageResult<[u8; 16]> {
    let maximum_duration = remaining_until(deadline)?;
    let worker = tokio::task::spawn_blocking(move || {
        let mut file = file;
        checksum_file_blocking(&mut file, deadline)
    });
    match tokio::time::timeout(maximum_duration, worker).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(StorageError::new(
            libc::EIO,
            format!("remote checksum worker failed: {error}"),
        )),
        Err(_) => Err(operation_timed_out()),
    }
}

fn checksum_file_blocking(file: &mut File, deadline: Instant) -> StorageResult<[u8; 16]> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| storage_io("failed to rewind anonymous remote file", error))?;
    let mut digest = Md5::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if Instant::now() >= deadline {
            return Err(operation_timed_out());
        }
        let read = file
            .read(&mut buffer)
            .map_err(|error| storage_io("failed to read anonymous remote file", error))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| storage_io("failed to rewind anonymous remote file", error))?;
    Ok(digest.finalize().into())
}

fn retarget_path(path: &RemotePath, from: &RemotePath, to: &RemotePath) -> Option<RemotePath> {
    if path.root() != from.root() || from.root() != to.root() {
        return None;
    }
    if path == from {
        return Some(to.clone());
    }
    let suffix = path.path().strip_prefix(from.path())?;
    if !suffix.starts_with('/') {
        return None;
    }
    let target = format!("{}{}", to.path(), suffix);
    RemotePath::new(to.root(), target).ok()
}

fn path_is_at_or_below(path: &RemotePath, parent: &RemotePath) -> bool {
    if path.root() != parent.root() {
        return false;
    }
    path == parent
        || path
            .path()
            .strip_prefix(parent.path())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

impl RequestCache {
    fn begin(&mut self, request_id: RequestId, fingerprint: [u8; 32]) -> CacheDecision {
        match self.entries.get_mut(&request_id) {
            Some(CachedRequest::Pending {
                fingerprint: cached,
                waiters,
            }) if cached == &fingerprint => {
                let (sender, receiver) = oneshot::channel();
                waiters.push(sender);
                CacheDecision::Wait(receiver)
            }
            Some(CachedRequest::Completed {
                fingerprint: cached,
                response,
                ..
            }) if cached == &fingerprint => CacheDecision::Replay(response.clone()),
            Some(_) => CacheDecision::Reject,
            None => {
                self.entries.insert(
                    request_id,
                    CachedRequest::Pending {
                        fingerprint,
                        waiters: Vec::new(),
                    },
                );
                CacheDecision::Execute
            }
        }
    }

    fn complete(&mut self, request_id: RequestId, response: Response) -> Vec<AbandonedResource> {
        let Some(CachedRequest::Pending {
            fingerprint,
            waiters,
        }) = self.entries.remove(&request_id)
        else {
            return Vec::new();
        };
        for waiter in waiters {
            let _ = waiter.send(response.clone());
        }
        self.entries.insert(
            request_id,
            CachedRequest::Completed {
                fingerprint,
                claimed: !response_has_resource(&response),
                response,
                completed_at: Instant::now(),
            },
        );
        self.prune(Instant::now())
    }

    fn claim(&mut self, request_id: &RequestId) -> Option<Option<String>> {
        let Some(CachedRequest::Completed {
            response, claimed, ..
        }) = self.entries.get_mut(request_id)
        else {
            return None;
        };
        if !response_has_resource(response) {
            return None;
        }
        *claimed = true;
        Some(match response {
            Response::List { anchor } => Some(anchor.clone()),
            Response::Read { payload, .. } => Some(payload.clone()),
            _ => None,
        })
    }

    fn expire(&mut self) -> Vec<AbandonedResource> {
        self.prune(Instant::now())
    }

    fn prune(&mut self, now: Instant) -> Vec<AbandonedResource> {
        let mut remove = self
            .entries
            .iter()
            .filter_map(|(request_id, entry)| match entry {
                CachedRequest::Completed { completed_at, .. }
                    if now.saturating_duration_since(*completed_at) >= REQUEST_CACHE_TTL =>
                {
                    Some(request_id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let completed = self
            .entries
            .len()
            .saturating_sub(remove.len())
            .saturating_sub(
                self.entries
                    .values()
                    .filter(|entry| matches!(entry, CachedRequest::Pending { .. }))
                    .count(),
            );
        if completed > REQUEST_CACHE_CAPACITY {
            let mut oldest = self
                .entries
                .iter()
                .filter_map(|(request_id, entry)| match entry {
                    CachedRequest::Completed { completed_at, .. }
                        if !remove.contains(request_id) =>
                    {
                        Some((request_id.clone(), *completed_at))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            oldest.sort_by_key(|(_, completed_at)| *completed_at);
            remove.extend(
                oldest
                    .into_iter()
                    .take(completed - REQUEST_CACHE_CAPACITY)
                    .map(|(request_id, _)| request_id),
            );
        }
        remove
            .into_iter()
            .filter_map(|request_id| match self.entries.remove(&request_id) {
                Some(CachedRequest::Completed {
                    response,
                    claimed: false,
                    ..
                }) => response_resource(&response),
                _ => None,
            })
            .collect()
    }
}

fn request_fingerprint(request: &Request) -> Result<[u8; 32], serde_json::Error> {
    let encoded = serde_json::to_vec(request)?;
    let digest = digest(&SHA256, &encoded);
    let mut fingerprint = [0_u8; 32];
    fingerprint.copy_from_slice(digest.as_ref());
    Ok(fingerprint)
}

fn response_has_resource(response: &Response) -> bool {
    response_resource(response).is_some()
}

fn response_resource(response: &Response) -> Option<AbandonedResource> {
    match response {
        Response::Open { handle, .. } => Some(AbandonedResource::Handle(handle.clone())),
        Response::Stat { anchor, .. } | Response::List { anchor } => {
            Some(AbandonedResource::Anchor(anchor.clone()))
        }
        Response::Read { payload, .. } => Some(AbandonedResource::Payload(payload.clone())),
        _ => None,
    }
}

impl HandleTombstones {
    fn contains(&self, handle: &str) -> bool {
        self.entries.contains(handle)
    }

    fn insert(&mut self, handle: String) {
        if !self.entries.insert(handle.clone()) {
            return;
        }
        self.order.push_back(handle);
        while self.order.len() > CLOSED_HANDLE_CAPACITY {
            if let Some(expired) = self.order.pop_front() {
                self.entries.remove(&expired);
            }
        }
    }
}

fn success() -> BrokerReply {
    BrokerReply {
        response: Response::Success,
        descriptor: None,
    }
}

fn protocol_reply(message: &str) -> BrokerReply {
    BrokerReply {
        response: Response::Error {
            errno: libc::EPROTO,
            message: message.to_string(),
        },
        descriptor: None,
    }
}

fn error_reply(error: StorageError) -> BrokerReply {
    BrokerReply {
        response: Response::Error {
            errno: error.errno,
            message: error.message,
        },
        descriptor: None,
    }
}

#[cfg(test)]
fn empty_file_metadata() -> RemoteMetadata {
    RemoteMetadata {
        file_type: RemoteFileType::File,
        size: 0,
        modified_seconds: 0,
        modified_nanoseconds: 0,
        identity: String::new(),
    }
}

fn file_too_large() -> StorageError {
    StorageError::new(
        libc::EFBIG,
        "remote file exceeds the sandbox snapshot limit",
    )
}

fn directory_too_large() -> StorageError {
    StorageError::new(
        libc::EOVERFLOW,
        "remote directory exceeds the sandbox listing limit",
    )
}

fn operation_timed_out() -> StorageError {
    StorageError::new(libc::ETIMEDOUT, "remote filesystem operation timed out")
}

fn ensure_same_snapshot(expected: &RemoteMetadata, current: &RemoteMetadata) -> StorageResult<()> {
    if expected.identity != current.identity
        || expected.size != current.size
        || expected.file_type != current.file_type
    {
        return Err(StorageError::new(
            libc::ESTALE,
            "remote file changed while creating a snapshot",
        ));
    }
    Ok(())
}

fn validate_byte_range(range: ByteRange) -> StorageResult<()> {
    if range.start >= range.end {
        return Err(StorageError::new(
            libc::EINVAL,
            "invalid remote filesystem byte range",
        ));
    }
    Ok(())
}

fn validate_byte_ranges(ranges: &[ByteRange]) -> StorageResult<()> {
    for range in ranges {
        validate_byte_range(*range)?;
    }
    Ok(())
}

fn copy_file_range_at(
    source: &File,
    destination: &File,
    offset: u64,
    length: u32,
) -> StorageResult<()> {
    let length = u64::from(length);
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    while copied < length {
        let requested = usize::try_from((length - copied).min(buffer.len() as u64))
            .expect("snapshot copy chunks fit usize");
        let read = FileExt::read_at(source, &mut buffer[..requested], copied)
            .map_err(|error| storage_io("failed to read remote snapshot payload", error))?;
        if read == 0 {
            return Err(StorageError::new(
                libc::EIO,
                "remote snapshot payload ended before its declared length",
            ));
        }
        let mut written = 0;
        while written < read {
            let actual = FileExt::write_at(
                destination,
                &buffer[written..read],
                offset + copied + written as u64,
            )
            .map_err(|error| storage_io("failed to write remote snapshot range", error))?;
            if actual == 0 {
                return Err(StorageError::new(
                    libc::EIO,
                    "remote snapshot range write made no progress",
                ));
            }
            written += actual;
        }
        copied += read as u64;
    }
    Ok(())
}

fn remaining_until(deadline: Instant) -> StorageResult<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(operation_timed_out)
}

struct LimitedWriter<W> {
    inner: W,
    remaining: u64,
    exceeded: bool,
}

impl<W> LimitedWriter<W> {
    fn new(inner: W, maximum: u64) -> Self {
        Self {
            inner,
            remaining: maximum,
            exceeded: false,
        }
    }

    fn exceeded(&self) -> bool {
        self.exceeded
    }
}

impl<W> Write for LimitedWriter<W>
where
    W: Write,
{
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer.len() as u64 > self.remaining {
            self.exceeded = true;
            return Err(std::io::Error::from_raw_os_error(libc::EOVERFLOW));
        }
        let written = self.inner.write(buffer)?;
        self.remaining = self.remaining.saturating_sub(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn set_close_on_exec(file: &File) -> StorageResult<()> {
    let descriptor = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(storage_io(
            "failed to protect anonymous remote descriptor",
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn storage_io(context: &str, error: std::io::Error) -> StorageError {
    StorageError::new(
        error.raw_os_error().unwrap_or(libc::EIO),
        format!("{context}: {error}"),
    )
}

#[cfg(test)]
mod tests;
