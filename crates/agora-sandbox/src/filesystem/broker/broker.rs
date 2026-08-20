use super::protocol::{ByteRange, Request, Response};
use super::{ByteRangeSet, LOCAL_STATUS_FLAGS_MASK, LocalOpenState};
use crate::filesystem::crypto::PLAINTEXT_BLOCK_SIZE;
use crate::filesystem::{EncryptedFile, FileCipher};
use ring::digest::{SHA256, digest};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};
use uuid::Uuid;

const COPY_BUFFER_SIZE: usize = 64 * 1024;
const LAZY_PLAINTEXT_THRESHOLD: u64 = 1024 * 1024;
pub(super) const WRITEBACK_DELAY: Duration = Duration::from_millis(10);
const CLOSED_HANDLE_TTL: Duration = Duration::from_secs(120);
const CLOSED_HANDLE_CAPACITY: usize = 128;
const REQUEST_CACHE_TTL: Duration = Duration::from_secs(120);
const REQUEST_CACHE_CAPACITY: usize = 256;

pub(crate) struct BrokerReply {
    pub(crate) response: Response,
    pub(crate) descriptors: Vec<File>,
}

impl BrokerReply {
    pub(super) fn response(response: Response) -> Self {
        Self {
            response,
            descriptors: Vec::new(),
        }
    }

    pub(super) fn error(errno: libc::c_int, message: impl Into<String>) -> Self {
        Self::response(Response::Error {
            errno,
            message: message.into(),
        })
    }
}

pub(crate) struct LocalBroker {
    root: PathBuf,
    cipher: FileCipher,
    lock_directory: tempfile::TempDir,
    handles: Mutex<HashMap<String, Arc<Mutex<LocalHandle>>>>,
    files: Mutex<HashMap<FileIdentity, Weak<SharedPlaintext>>>,
    open_locks: Mutex<HashMap<FileIdentity, Weak<Mutex<()>>>>,
    requests: Mutex<RequestCache>,
    writeback_pending: AtomicBool,
}

#[derive(Default)]
struct RequestCache {
    entries: HashMap<String, CachedRequest>,
}

enum CachedRequest {
    Pending {
        fingerprint: [u8; 32],
        completion: Arc<RequestCompletion>,
    },
    Completed {
        fingerprint: [u8; 32],
        response: Response,
        completed_at: Instant,
        claimed: bool,
    },
}

#[derive(Default)]
struct RequestCompletion {
    response: Mutex<Option<Response>>,
    ready: Condvar,
}

enum CacheDecision {
    Execute,
    Wait(Arc<RequestCompletion>),
    Replay(Response),
    Reject,
}

struct LocalHandle {
    writable: bool,
    potentially_dirty: ByteRangeSet,
    active_writes: HashMap<String, ActiveWrite>,
    references: usize,
    closed_at: Option<Instant>,
    shared: Arc<SharedPlaintext>,
    state: LocalOpenState,
}

struct SharedPlaintext {
    inner: Mutex<SharedFile>,
    lock_anchor: tempfile::NamedTempFile,
    mutations: Mutex<SharedMutations>,
    identity: FileIdentity,
    poisoned: AtomicBool,
}

#[derive(Clone, Copy)]
struct ActiveWrite {
    range: ByteRange,
    recoverable: bool,
}

struct SharedFile {
    plaintext: File,
    encrypted: EncryptedFile,
    resident: ByteRangeSet,
    baseline: PlaintextIdentity,
    pending_writes: ByteRangeSet,
    pending_since: Option<Instant>,
    needs_durable_sync: bool,
}

#[derive(Default)]
struct SharedMutations {
    ordinary: HashSet<WriteKey>,
    append: Option<WriteKey>,
    exclusive: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WriteKey {
    handle: String,
    write_id: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PlaintextIdentity {
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

struct BrokerError {
    errno: libc::c_int,
    message: String,
}

struct SharedExclusiveGuard<'a> {
    shared: &'a SharedPlaintext,
}

fn open_status_flags(flags: libc::c_int) -> libc::c_int {
    flags & LOCAL_STATUS_FLAGS_MASK
}

impl SharedPlaintext {
    fn is_healthy(&self) -> bool {
        !self.poisoned.load(Ordering::Acquire)
    }

    fn ensure_healthy(&self) -> Result<(), BrokerError> {
        if self.is_healthy() {
            Ok(())
        } else {
            Err(BrokerError::poisoned())
        }
    }

    fn try_begin_write(&self, key: WriteKey) -> bool {
        let mut mutations = lock(&self.mutations);
        if mutations.ordinary.contains(&key) {
            return false;
        }
        if mutations.exclusive || mutations.append.is_some() {
            return false;
        }
        mutations.ordinary.insert(key);
        true
    }

    fn try_begin_append(&self, key: WriteKey) -> bool {
        let mut mutations = lock(&self.mutations);
        if mutations.append.as_ref() == Some(&key) {
            return false;
        }
        if mutations.exclusive || mutations.append.is_some() || !mutations.ordinary.is_empty() {
            return false;
        }
        mutations.append = Some(key);
        true
    }

    fn finish_write(&self, key: &WriteKey) {
        let mut mutations = lock(&self.mutations);
        if !mutations.ordinary.remove(key) && mutations.append.as_ref() == Some(key) {
            mutations.append = None;
        }
    }

    fn try_begin_exclusive(&self) -> Option<SharedExclusiveGuard<'_>> {
        let mut mutations = lock(&self.mutations);
        if mutations.exclusive || mutations.append.is_some() || !mutations.ordinary.is_empty() {
            return None;
        }
        mutations.exclusive = true;
        Some(SharedExclusiveGuard { shared: self })
    }
}

impl Drop for SharedExclusiveGuard<'_> {
    fn drop(&mut self) {
        let mut mutations = lock(&self.shared.mutations);
        mutations.exclusive = false;
    }
}

impl LocalBroker {
    #[cfg(test)]
    pub(crate) fn new(root: &Path, cipher: FileCipher) -> std::io::Result<Self> {
        Self::with_lock_directory(root, cipher, tempfile::tempdir()?)
    }

    pub(crate) fn new_in(
        root: &Path,
        cipher: FileCipher,
        runtime_directory: &Path,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(runtime_directory)?;
        let lock_directory = tempfile::Builder::new()
            .prefix("local-locks-")
            .tempdir_in(runtime_directory)?;
        Self::with_lock_directory(root, cipher, lock_directory)
    }

    fn with_lock_directory(
        root: &Path,
        cipher: FileCipher,
        lock_directory: tempfile::TempDir,
    ) -> std::io::Result<Self> {
        Ok(Self {
            root: root.canonicalize()?,
            cipher,
            lock_directory,
            handles: Mutex::new(HashMap::new()),
            files: Mutex::new(HashMap::new()),
            open_locks: Mutex::new(HashMap::new()),
            requests: Mutex::new(RequestCache::default()),
            writeback_pending: AtomicBool::new(false),
        })
    }

    pub(crate) fn handle_request(
        &self,
        request_id: String,
        request: Request,
        descriptor: Option<OwnedFd>,
    ) -> BrokerReply {
        let fingerprint = match request_fingerprint(&request) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                drop(descriptor);
                return BrokerReply::error(
                    libc::EPROTO,
                    format!("failed to fingerprint local filesystem request: {error}"),
                );
            }
        };
        let decision = lock(&self.requests).begin(request_id.clone(), fingerprint);
        match decision {
            CacheDecision::Execute => {
                let response = self.response(request, descriptor);
                let abandoned =
                    lock(&self.requests).complete(request_id, response.clone(), Instant::now());
                self.abort_handles(abandoned);
                self.reply(response)
            }
            CacheDecision::Wait(completion) => {
                drop(descriptor);
                self.reply(completion.wait())
            }
            CacheDecision::Replay(response) => {
                drop(descriptor);
                self.reply(response)
            }
            CacheDecision::Reject => {
                drop(descriptor);
                BrokerReply::error(
                    libc::EPROTO,
                    "local request ID was reused for a different operation",
                )
            }
        }
    }

    pub(super) fn handle_uncached(
        &self,
        request: Request,
        descriptor: Option<OwnedFd>,
    ) -> BrokerReply {
        let response = self.response(request, descriptor);
        self.reply(response)
    }

    #[cfg(test)]
    pub(crate) fn handle(&self, request: Request, descriptor: Option<OwnedFd>) -> BrokerReply {
        self.handle_uncached(request, descriptor)
    }

    fn response(&self, request: Request, descriptor: Option<OwnedFd>) -> Response {
        match self.dispatch(request, descriptor) {
            Ok(response) => response,
            Err(error) => Response::Error {
                errno: error.errno,
                message: error.message,
            },
        }
    }

    fn reply(&self, response: Response) -> BrokerReply {
        let descriptors = if let Response::Open { handle, .. } = &response {
            match self.open_descriptors(handle) {
                Ok(descriptors) => descriptors,
                Err(error) => {
                    return BrokerReply::error(error.errno, error.message);
                }
            }
        } else {
            Vec::new()
        };
        BrokerReply {
            response,
            descriptors,
        }
    }

    pub(crate) fn flush_all(&self) -> std::io::Result<()> {
        let active = self.active_handles();
        for (id, _) in &active {
            self.abandon_active_writes(id)
                .map_err(BrokerError::into_io)?;
        }
        let active = active
            .into_iter()
            .filter(|(_, shared)| shared.is_healthy())
            .collect::<Vec<_>>();
        for (id, _) in &active {
            self.sync_handle(id, Vec::new(), false, true, false)
                .map_err(BrokerError::into_io)?;
        }
        for (id, _) in Self::unique_files(active) {
            self.sync_handle(&id, Vec::new(), true, false, false)
                .map_err(BrokerError::into_io)?;
        }
        Ok(())
    }

    pub(crate) fn flush_due(&self, now: Instant) -> std::io::Result<()> {
        self.writeback_pending.store(false, Ordering::Release);
        let active = self.active_handles();
        let due = active
            .iter()
            .filter(|(_, shared)| {
                shared.is_healthy()
                    && lock(&shared.inner).pending_since.is_some_and(|pending| {
                        now.saturating_duration_since(pending) >= WRITEBACK_DELAY
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        for (id, _) in Self::unique_files(due) {
            self.sync_handle(&id, Vec::new(), false, false, false)
                .map_err(BrokerError::into_io)?;
        }
        if active
            .iter()
            .any(|(_, shared)| shared.is_healthy() && lock(&shared.inner).pending_since.is_some())
        {
            self.writeback_pending.store(true, Ordering::Release);
        }
        Ok(())
    }

    fn active_handles(&self) -> Vec<(String, Arc<SharedPlaintext>)> {
        lock(&self.handles)
            .iter()
            .filter_map(|(id, handle)| {
                let handle = lock(handle);
                (handle.references != 0).then(|| (id.clone(), Arc::clone(&handle.shared)))
            })
            .collect()
    }

    fn unique_files(
        handles: Vec<(String, Arc<SharedPlaintext>)>,
    ) -> Vec<(String, Arc<SharedPlaintext>)> {
        let mut seen = HashSet::new();
        handles
            .into_iter()
            .filter(|(_, shared)| seen.insert(Arc::as_ptr(shared)))
            .collect()
    }

    pub(super) fn writeback_pending(&self) -> bool {
        self.writeback_pending.load(Ordering::Acquire)
    }

    pub(crate) fn expire_closed(&self) {
        self.prune_closed(Instant::now());
    }

    pub(crate) fn expire_requests(&self) {
        let abandoned = lock(&self.requests).prune(Instant::now());
        self.abort_handles(abandoned);
    }

    fn dispatch(
        &self,
        request: Request,
        descriptor: Option<OwnedFd>,
    ) -> Result<Response, BrokerError> {
        match request {
            Request::Ping => {
                Self::reject_descriptor(descriptor)?;
                Ok(Response::Success)
            }
            Request::AttachWriteLease => Err(BrokerError::protocol(
                "write lease attachment is available only on shared control streams",
            )),
            Request::Open { path, flags } => {
                Self::reject_descriptor(descriptor)?;
                self.open(&path.to_path().map_err(BrokerError::protocol_error)?, flags)
            }
            Request::Materialize { handle, range } => {
                Self::reject_descriptor(descriptor)?;
                if let Some(range) = range {
                    Self::validate_range(range)?;
                }
                self.activate(&handle)?;
                self.materialize(&handle, range)?;
                Ok(Response::Success)
            }
            Request::Sync {
                handle,
                ranges,
                durable,
            } => {
                Self::reject_descriptor(descriptor)?;
                Self::validate_ranges(&ranges)?;
                self.activate(&handle)?;
                self.sync_handle(&handle, ranges, durable, true, false)?;
                Ok(Response::Success)
            }
            Request::PotentiallyDirty { handle, range } => {
                Self::reject_descriptor(descriptor)?;
                Self::validate_range(range)?;
                self.activate(&handle)?;
                let local = self.lookup_handle(&handle)?;
                let shared = Arc::clone(&lock(&local).shared);
                shared.ensure_healthy()?;
                let _file_guard = lock(&shared.inner);
                shared.ensure_healthy()?;
                let mut handle = lock(&local);
                if !handle.writable {
                    return Err(BrokerError::new(
                        libc::EBADF,
                        "local filesystem handle is not writable",
                    ));
                }
                handle.potentially_dirty.insert(range);
                Ok(Response::Success)
            }
            Request::BeginWrite {
                handle,
                write_id,
                range,
            } => {
                Self::reject_descriptor(descriptor)?;
                Self::validate_range(range)?;
                self.activate(&handle)?;
                let local = self.lookup_handle(&handle)?;
                let shared = Arc::clone(&lock(&local).shared);
                shared.ensure_healthy()?;
                if !lock(&local).writable {
                    return Err(BrokerError::new(
                        libc::EBADF,
                        "local filesystem handle is not writable",
                    ));
                }
                if lock(&local).active_writes.contains_key(&write_id) {
                    return Err(BrokerError::protocol("local write ID was reused"));
                }
                let recoverable = {
                    let shared_file = lock(&shared.inner);
                    shared.ensure_healthy()?;
                    let length = shared_file
                        .plaintext
                        .metadata()
                        .map_err(|error| {
                            BrokerError::io("failed to inspect local plaintext file", error)
                        })?
                        .len();
                    let existing_end = range.end.min(length);
                    range.start >= existing_end
                        || shared_file.resident.covers(ByteRange {
                            start: range.start,
                            end: existing_end,
                        })
                };
                if !shared.try_begin_write(WriteKey {
                    handle: handle.clone(),
                    write_id: write_id.clone(),
                }) {
                    return Err(BrokerError::busy());
                }
                if let Err(error) = shared.ensure_healthy() {
                    shared.finish_write(&WriteKey { handle, write_id });
                    return Err(error);
                }
                lock(&local)
                    .active_writes
                    .insert(write_id, ActiveWrite { range, recoverable });
                Ok(Response::Success)
            }
            Request::BeginAppend { handle, write_id } => {
                Self::reject_descriptor(descriptor)?;
                self.activate(&handle)?;
                let local = self.lookup_handle(&handle)?;
                let shared = Arc::clone(&lock(&local).shared);
                shared.ensure_healthy()?;
                if !lock(&local).writable {
                    return Err(BrokerError::new(
                        libc::EBADF,
                        "local filesystem handle is not writable",
                    ));
                }
                if lock(&local).active_writes.contains_key(&write_id) {
                    return Err(BrokerError::protocol("local append write ID was reused"));
                }
                let key = WriteKey {
                    handle: handle.clone(),
                    write_id: write_id.clone(),
                };
                if !shared.try_begin_append(key.clone()) {
                    return Err(BrokerError::busy());
                }
                let offset = match lock(&shared.inner).plaintext.metadata() {
                    Ok(metadata) => metadata.len(),
                    Err(error) => {
                        shared.finish_write(&key);
                        return Err(BrokerError::io(
                            "failed to inspect local plaintext file",
                            error,
                        ));
                    }
                };
                let Some(range) = (offset < u64::MAX).then_some(ByteRange {
                    start: offset,
                    end: u64::MAX,
                }) else {
                    shared.finish_write(&key);
                    return Err(BrokerError::new(
                        libc::EFBIG,
                        "local plaintext file cannot be extended",
                    ));
                };
                if let Err(error) = shared.ensure_healthy() {
                    shared.finish_write(&key);
                    return Err(error);
                }
                lock(&local).active_writes.insert(
                    write_id,
                    ActiveWrite {
                        range,
                        recoverable: true,
                    },
                );
                Ok(Response::Offset { offset })
            }
            Request::FinishWrite {
                handle,
                write_id,
                range,
            } => {
                Self::reject_descriptor(descriptor)?;
                let local = self.lookup_handle(&handle)?;
                let shared = Arc::clone(&lock(&local).shared);
                let reserved = {
                    let mut local = lock(&local);
                    if !local.writable {
                        return Err(BrokerError::new(
                            libc::EBADF,
                            "local filesystem handle is not writable",
                        ));
                    }
                    local
                        .active_writes
                        .remove(&write_id)
                        .ok_or_else(|| BrokerError::protocol("unknown local write ID"))?
                };
                let valid = range.start < range.end
                    && range.start >= reserved.range.start
                    && range.end <= reserved.range.end;
                if valid && shared.ensure_healthy().is_ok() {
                    let mut shared_file = lock(&shared.inner);
                    if shared.ensure_healthy().is_ok() {
                        shared_file.pending_writes.insert(range);
                        shared_file.resident.insert(range);
                        shared_file.pending_since.get_or_insert_with(Instant::now);
                        self.writeback_pending.store(true, Ordering::Release);
                    }
                }
                shared.finish_write(&WriteKey { handle, write_id });
                if !valid {
                    Err(BrokerError::protocol(
                        "completed local write is invalid or exceeds its reserved range",
                    ))
                } else if !shared.is_healthy() {
                    Err(BrokerError::poisoned())
                } else {
                    Ok(Response::Success)
                }
            }
            Request::CancelWrite { handle, write_id } => {
                Self::reject_descriptor(descriptor)?;
                let local = self.lookup_handle(&handle)?;
                let shared = Arc::clone(&lock(&local).shared);
                lock(&local).active_writes.remove(&write_id);
                shared.finish_write(&WriteKey { handle, write_id });
                Ok(Response::Success)
            }
            Request::Claim { request_id } => {
                Self::reject_descriptor(descriptor)?;
                lock(&self.requests).claim(&request_id).ok_or_else(|| {
                    BrokerError::protocol("local resource request is not available to claim")
                })?;
                Ok(Response::Success)
            }
            Request::Abort { handle } => {
                Self::reject_descriptor(descriptor)?;
                self.abort_handle(&handle);
                Ok(Response::Success)
            }
            Request::Retain {
                handles: mut retained,
            } => {
                Self::reject_descriptor(descriptor)?;
                retained.sort_unstable();
                retained.dedup();
                let handles = lock(&self.handles);
                let retained_handles = retained
                    .iter()
                    .map(|id| {
                        handles
                            .get(id)
                            .cloned()
                            .ok_or_else(BrokerError::bad_descriptor)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                for handle in &retained_handles {
                    let handle = lock(handle);
                    handle.references.checked_add(1).ok_or_else(|| {
                        BrokerError::new(libc::EOVERFLOW, "local filesystem reference overflow")
                    })?;
                }
                for handle in retained_handles {
                    let mut handle = lock(&handle);
                    handle.references += 1;
                    handle.closed_at = None;
                }
                drop(handles);
                Ok(Response::Success)
            }
            Request::ReleaseRetain {
                handles: mut retained,
            } => {
                Self::reject_descriptor(descriptor)?;
                retained.sort_unstable();
                retained.dedup();
                let handles = lock(&self.handles);
                let retained_handles = retained
                    .iter()
                    .map(|id| {
                        handles
                            .get(id)
                            .cloned()
                            .ok_or_else(BrokerError::bad_descriptor)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if retained_handles
                    .iter()
                    .any(|handle| lock(handle).references == 0)
                {
                    return Err(BrokerError::bad_descriptor());
                }
                for handle in retained_handles {
                    let mut handle = lock(&handle);
                    handle.references -= 1;
                    if handle.references == 0 {
                        handle.closed_at = Some(Instant::now());
                    }
                }
                drop(handles);
                self.prune_closed(Instant::now());
                Ok(Response::Success)
            }
            Request::Close { handle, ranges } => {
                Self::reject_descriptor(descriptor)?;
                Self::validate_ranges(&ranges)?;
                let Some(local) = lock(&self.handles).get(&handle).cloned() else {
                    return Ok(Response::Success);
                };
                let final_reference = lock(&local).references <= 1;
                self.sync_handle(&handle, ranges, true, true, final_reference)?;
                let mut local = lock(&local);
                if local.references > 0 {
                    local.references -= 1;
                }
                if local.references == 0 {
                    local.closed_at = Some(Instant::now());
                }
                drop(local);
                self.prune_closed(Instant::now());
                Ok(Response::Success)
            }
        }
    }

    fn open(&self, path: &Path, flags: libc::c_int) -> Result<Response, BrokerError> {
        let canonical = path
            .canonicalize()
            .map_err(|error| BrokerError::io("failed to resolve encrypted backing file", error))?;
        if !canonical.starts_with(&self.root) {
            return Err(BrokerError::new(
                libc::EACCES,
                "encrypted backing file is outside the workspace",
            ));
        }
        let access = flags & libc::O_ACCMODE;
        if access != libc::O_RDONLY && access != libc::O_WRONLY && access != libc::O_RDWR {
            return Err(BrokerError::new(
                libc::EINVAL,
                "invalid local filesystem open access mode",
            ));
        }
        let encrypted = self
            .cipher
            .open_file(&canonical)
            .map_err(|error| BrokerError::anyhow("failed to open encrypted backing file", error))?;
        let metadata = encrypted
            .backing_file()
            .metadata()
            .map_err(|error| BrokerError::io("failed to inspect encrypted backing file", error))?;
        let identity = FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let open_lock = {
            let mut open_locks = lock(&self.open_locks);
            open_locks.retain(|_, open_lock| open_lock.strong_count() != 0);
            if let Some(open_lock) = open_locks.get(&identity).and_then(Weak::upgrade) {
                open_lock
            } else {
                let open_lock = Arc::new(Mutex::new(()));
                open_locks.insert(identity, Arc::downgrade(&open_lock));
                open_lock
            }
        };
        let _open = lock(&open_lock);
        let mut encrypted = Some(encrypted);
        let shared = {
            let mut files = lock(&self.files);
            files.retain(|_, file| file.strong_count() != 0);
            let reusable = files
                .get(&identity)
                .and_then(Weak::upgrade)
                .filter(|file| file.is_healthy());
            if let Some(file) = reusable {
                drop(files);
                if flags & libc::O_TRUNC != 0 {
                    file.ensure_healthy()?;
                    let _exclusive = file.try_begin_exclusive().ok_or_else(BrokerError::busy)?;
                    let mut shared = lock(&file.inner);
                    file.ensure_healthy()?;
                    shared.plaintext.set_len(0).map_err(|error| {
                        BrokerError::io("failed to truncate shared local plaintext file", error)
                    })?;
                    let mut replacement = encrypted.take().expect("encrypted file is available");
                    replacement.set_len(0).map_err(|error| {
                        BrokerError::anyhow("failed to truncate encrypted local file", error)
                    })?;
                    let metadata = shared.plaintext.metadata().map_err(|error| {
                        BrokerError::io("failed to inspect shared local plaintext file", error)
                    })?;
                    shared.encrypted = replacement;
                    shared.resident = ByteRangeSet::default();
                    shared.baseline = PlaintextIdentity::from_metadata(&metadata);
                    shared.needs_durable_sync = true;
                }
                file
            } else {
                files.remove(&identity);
                drop(files);
                let mut encrypted = encrypted.take().expect("encrypted file is available");
                let truncated = flags & libc::O_TRUNC != 0;
                if truncated {
                    encrypted.set_len(0).map_err(|error| {
                        BrokerError::anyhow("failed to truncate encrypted local file", error)
                    })?;
                }
                let length = encrypted.len();
                let lazy = !truncated && length > LAZY_PLAINTEXT_THRESHOLD;
                let plaintext = tempfile::tempfile().map_err(|error| {
                    BrokerError::io("failed to create shared local plaintext file", error)
                })?;
                plaintext.set_len(length).map_err(|error| {
                    BrokerError::io("failed to size shared local plaintext file", error)
                })?;
                let plaintext_metadata = plaintext.metadata().map_err(|error| {
                    BrokerError::io("failed to inspect local plaintext file", error)
                })?;
                let mut shared_file = SharedFile {
                    plaintext,
                    encrypted,
                    resident: ByteRangeSet::default(),
                    baseline: PlaintextIdentity::from_metadata(&plaintext_metadata),
                    pending_writes: ByteRangeSet::default(),
                    pending_since: None,
                    needs_durable_sync: truncated,
                };
                if !lazy {
                    Self::materialize_locked(&mut shared_file, None)?;
                }
                let file = Arc::new(SharedPlaintext {
                    inner: Mutex::new(shared_file),
                    lock_anchor: tempfile::NamedTempFile::new_in(self.lock_directory.path())
                        .map_err(|error| {
                            BrokerError::io("failed to create local lock anchor", error)
                        })?,
                    mutations: Mutex::new(SharedMutations::default()),
                    identity,
                    poisoned: AtomicBool::new(false),
                });
                lock(&self.files).insert(identity, Arc::downgrade(&file));
                file
            }
        };
        let lazy = {
            let shared = lock(&shared.inner);
            !shared.fully_resident()
        };
        let state = LocalOpenState::create(open_status_flags(flags))
            .map_err(|error| BrokerError::io("failed to create local open state", error))?;
        let id = Uuid::new_v4().simple().to_string();
        lock(&self.handles).insert(
            id.clone(),
            Arc::new(Mutex::new(LocalHandle {
                writable: access != libc::O_RDONLY,
                potentially_dirty: ByteRangeSet::default(),
                active_writes: HashMap::new(),
                references: 1,
                closed_at: None,
                shared,
                state,
            })),
        );
        Ok(Response::Open {
            handle: id,
            device: identity.device,
            inode: identity.inode,
            links: metadata.nlink(),
            lazy,
        })
    }

    fn materialize(&self, id: &str, range: Option<ByteRange>) -> Result<(), BrokerError> {
        let handle = self.lookup_handle(id)?;
        let shared = Arc::clone(&lock(&handle).shared);
        shared.ensure_healthy()?;
        let _exclusive = shared.try_begin_exclusive().ok_or_else(BrokerError::busy)?;
        let mut shared_file = lock(&shared.inner);
        shared.ensure_healthy()?;
        Self::materialize_locked(&mut shared_file, range)
    }

    fn materialize_locked(
        shared: &mut SharedFile,
        range: Option<ByteRange>,
    ) -> Result<(), BrokerError> {
        let length = shared.encrypted.len();
        if length == 0 {
            return Ok(());
        }
        let requested = range.unwrap_or(ByteRange {
            start: 0,
            end: length,
        });
        let start = requested.start.min(length);
        let end = requested.end.min(length);
        if start >= end {
            return Ok(());
        }
        let block = PLAINTEXT_BLOCK_SIZE as u64;
        let aligned = ByteRange {
            start: start - start % block,
            end: end.div_ceil(block).saturating_mul(block).min(length),
        };
        let missing = shared.resident.missing(aligned);
        if missing.is_empty() {
            return Ok(());
        }
        let before = shared
            .plaintext
            .metadata()
            .map_err(|error| BrokerError::io("failed to inspect local plaintext file", error))?;
        let baseline_clean = PlaintextIdentity::from_metadata(&before) == shared.baseline;
        let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
        let result = (|| {
            for range in missing {
                let mut offset = range.start;
                while offset < range.end {
                    let count = usize::try_from((range.end - offset).min(buffer.len() as u64))
                        .expect("materialization chunk length fits usize");
                    let read = shared
                        .encrypted
                        .read_at(&mut buffer[..count], offset)
                        .map_err(|error| {
                            BrokerError::anyhow("failed to decrypt local plaintext range", error)
                        })?;
                    if read != count {
                        return Err(BrokerError::new(
                            libc::EIO,
                            "encrypted local file ended before its logical length",
                        ));
                    }
                    write_all_at(&shared.plaintext, &buffer[..read], offset).map_err(|error| {
                        BrokerError::io("failed to materialize local plaintext range", error)
                    })?;
                    offset += read as u64;
                }
                shared.resident.insert(range);
            }
            Ok(())
        })();
        let refreshed = if baseline_clean {
            shared
                .plaintext
                .metadata()
                .map(|metadata| shared.baseline = PlaintextIdentity::from_metadata(&metadata))
                .map_err(|error| BrokerError::io("failed to inspect local plaintext file", error))
        } else {
            Ok(())
        };
        result.and(refreshed)
    }

    fn open_descriptors(&self, id: &str) -> Result<Vec<File>, BrokerError> {
        let handle = self.lookup_handle(id)?;
        let (shared, state) = {
            let handle = lock(&handle);
            let state = handle.state.try_clone_file().map_err(|error| {
                BrokerError::io("failed to clone local open state descriptor", error)
            })?;
            (Arc::clone(&handle.shared), state)
        };
        let lock_descriptor = shared
            .lock_anchor
            .reopen()
            .map_err(|error| BrokerError::io("failed to open local lock anchor", error))?;
        let shared_file = lock(&shared.inner);
        Ok(vec![
            shared_file.plaintext.try_clone().map_err(|error| {
                BrokerError::io("failed to clone local plaintext descriptor", error)
            })?,
            state,
            lock_descriptor,
        ])
    }

    fn sync_handle(
        &self,
        id: &str,
        ranges: Vec<ByteRange>,
        durable: bool,
        include_potential: bool,
        include_active: bool,
    ) -> Result<(), BrokerError> {
        if include_active {
            self.abandon_active_writes(id)?;
        }
        let handle = self.lookup_handle(id)?;
        let shared = Arc::clone(&lock(&handle).shared);
        shared.ensure_healthy()?;
        let mut shared_file = lock(&shared.inner);
        shared.ensure_healthy()?;
        let handle = lock(&handle);
        let metadata = shared_file
            .plaintext
            .metadata()
            .map_err(|error| BrokerError::io("failed to inspect local plaintext file", error))?;
        let current = PlaintextIdentity::from_metadata(&metadata);
        if include_potential
            && !handle.writable
            && shared_file.pending_writes.is_empty()
            && current != shared_file.baseline
        {
            return Err(BrokerError::new(
                libc::EBADF,
                "local filesystem handle is not writable",
            ));
        }
        if !ranges.is_empty() && !handle.writable {
            return Err(BrokerError::new(
                libc::EBADF,
                "local filesystem handle is not writable",
            ));
        }
        for range in ranges {
            shared_file.pending_writes.insert(range);
            shared_file.resident.insert(range);
        }
        if !shared_file.pending_writes.is_empty() {
            shared_file.pending_since.get_or_insert_with(Instant::now);
        }
        let mut candidates = shared_file.pending_writes.clone();
        if include_potential && current != shared_file.baseline {
            for range in handle.potentially_dirty.iter() {
                candidates.insert(*range);
            }
        }
        if include_potential
            && handle.writable
            && candidates.is_empty()
            && current != shared_file.baseline
            && current.length > 0
        {
            candidates.insert(ByteRange {
                start: 0,
                end: current.length,
            });
        }
        let ranges = candidates.to_vec();
        let length = current.length;
        if include_potential && !handle.potentially_dirty.is_empty() && !handle.writable {
            return Err(BrokerError::new(
                libc::EBADF,
                "local filesystem handle is not writable",
            ));
        }
        let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
        let mut mutated = false;
        for range in ranges {
            let start = range.start.min(length);
            let end = range.end.min(length);
            if start >= end {
                continue;
            }
            let mut offset = start;
            while offset < end {
                let count = usize::try_from((end - offset).min(buffer.len() as u64))
                    .expect("copy chunk length fits usize");
                read_exact_at(&shared_file.plaintext, &mut buffer[..count], offset).map_err(
                    |error| BrokerError::io("failed to read local plaintext range", error),
                )?;
                shared_file
                    .encrypted
                    .write_at(&buffer[..count], offset)
                    .map_err(|error| {
                        BrokerError::anyhow("failed to encrypt local file range", error)
                    })?;
                mutated = true;
                offset += count as u64;
            }
        }
        if shared_file.encrypted.len() != length {
            shared_file.encrypted.set_len(length).map_err(|error| {
                BrokerError::anyhow("failed to resize encrypted local file", error)
            })?;
            mutated = true;
        }
        shared_file.needs_durable_sync |= mutated;
        if durable && shared_file.needs_durable_sync {
            shared_file.encrypted.sync_all().map_err(|error| {
                BrokerError::anyhow("failed to sync encrypted local file", error)
            })?;
            shared_file.needs_durable_sync = false;
        }
        shared_file.pending_writes.clear();
        shared_file.pending_since = None;
        shared_file.baseline = current;
        Ok(())
    }

    fn activate(&self, id: &str) -> Result<(), BrokerError> {
        let handles = lock(&self.handles);
        let handle = handles.get(id).ok_or_else(BrokerError::bad_descriptor)?;
        let mut handle = lock(handle);
        if handle.references == 0 {
            handle.references = 1;
            handle.closed_at = None;
        }
        Ok(())
    }

    fn lookup_handle(&self, id: &str) -> Result<Arc<Mutex<LocalHandle>>, BrokerError> {
        lock(&self.handles)
            .get(id)
            .cloned()
            .ok_or_else(BrokerError::bad_descriptor)
    }

    fn reject_descriptor(descriptor: Option<OwnedFd>) -> Result<(), BrokerError> {
        if descriptor.is_some() {
            Err(BrokerError::protocol(
                "local filesystem request unexpectedly included a descriptor",
            ))
        } else {
            Ok(())
        }
    }

    fn validate_range(range: ByteRange) -> Result<(), BrokerError> {
        if range.start < range.end {
            Ok(())
        } else {
            Err(BrokerError::protocol("invalid local filesystem byte range"))
        }
    }

    fn validate_ranges(ranges: &[ByteRange]) -> Result<(), BrokerError> {
        for &range in ranges {
            Self::validate_range(range)?;
        }
        Ok(())
    }

    pub(super) fn abandon_write(&self, id: &str, write_id: &str) {
        let Some(handle) = lock(&self.handles).get(id).cloned() else {
            return;
        };
        let (shared, write) = {
            let mut local = lock(&handle);
            (
                Arc::clone(&local.shared),
                local.active_writes.remove(write_id),
            )
        };
        if let Some(write) = write {
            self.recover_abandoned_write(
                &shared,
                WriteKey {
                    handle: id.to_string(),
                    write_id: write_id.to_string(),
                },
                write,
            );
        }
    }

    fn abandon_active_writes(&self, id: &str) -> Result<(), BrokerError> {
        let handle = self.lookup_handle(id)?;
        let (shared, writes) = {
            let mut local = lock(&handle);
            let shared = Arc::clone(&local.shared);
            let writes = std::mem::take(&mut local.active_writes);
            (shared, writes)
        };
        for (write_id, write) in writes {
            self.recover_abandoned_write(
                &shared,
                WriteKey {
                    handle: id.to_string(),
                    write_id,
                },
                write,
            );
        }
        Ok(())
    }

    fn recover_abandoned_write(
        &self,
        shared: &Arc<SharedPlaintext>,
        key: WriteKey,
        write: ActiveWrite,
    ) {
        if write.recoverable && shared.is_healthy() {
            let mut shared_file = lock(&shared.inner);
            if shared.is_healthy() {
                let length = match shared_file.plaintext.metadata() {
                    Ok(metadata) => metadata.len(),
                    Err(_) => {
                        drop(shared_file);
                        self.poison_shared(shared);
                        shared.finish_write(&key);
                        return;
                    }
                };
                let range = ByteRange {
                    start: write.range.start.min(length),
                    end: write.range.end.min(length),
                };
                shared_file.pending_writes.insert(range);
                shared_file.resident.insert(range);
                shared_file.pending_since.get_or_insert_with(Instant::now);
                self.writeback_pending.store(true, Ordering::Release);
            }
        } else {
            self.poison_shared(shared);
        }
        shared.finish_write(&key);
    }

    fn poison_shared(&self, shared: &Arc<SharedPlaintext>) {
        if !shared.is_healthy() {
            return;
        }
        let mut shared_file = lock(&shared.inner);
        if shared.poisoned.swap(true, Ordering::AcqRel) {
            return;
        }
        shared_file.pending_writes.clear();
        shared_file.pending_since = None;
        let mut files = lock(&self.files);
        let attached = files
            .get(&shared.identity)
            .and_then(Weak::upgrade)
            .is_some_and(|current| Arc::ptr_eq(&current, shared));
        if attached {
            files.remove(&shared.identity);
        }
    }

    fn abort_handle(&self, handle: &str) {
        if let Some(local) = lock(&self.handles).remove(handle) {
            Self::release_active_writes(handle, &local);
        }
    }

    fn release_active_writes(handle: &str, local: &Mutex<LocalHandle>) {
        let (shared, write_ids) = {
            let mut local = lock(local);
            (
                Arc::clone(&local.shared),
                std::mem::take(&mut local.active_writes).into_keys(),
            )
        };
        for write_id in write_ids {
            shared.finish_write(&WriteKey {
                handle: handle.to_string(),
                write_id,
            });
        }
    }

    fn abort_handles(&self, handles: Vec<String>) {
        for handle in handles {
            self.abort_handle(&handle);
        }
    }

    fn prune_closed(&self, now: Instant) {
        let mut handles = lock(&self.handles);
        let mut expired = Vec::new();
        let mut retained = Vec::new();
        for (id, handle) in handles.iter() {
            let (references, closed_at) = {
                let local = lock(handle);
                (local.references, local.closed_at)
            };
            if references != 0 {
                continue;
            }
            if closed_at.is_none_or(|closed| now.duration_since(closed) >= CLOSED_HANDLE_TTL) {
                expired.push(id.clone());
            } else {
                retained.push((id.clone(), closed_at));
            }
        }
        retained.sort_unstable_by_key(|(_, closed_at)| *closed_at);
        let excess = retained.len().saturating_sub(CLOSED_HANDLE_CAPACITY);
        expired.extend(retained.into_iter().take(excess).map(|(id, _)| id));
        let removed = expired
            .into_iter()
            .filter_map(|id| handles.remove(&id).map(|handle| (id, handle)))
            .collect::<Vec<_>>();
        drop(handles);
        for (id, handle) in removed {
            Self::release_active_writes(&id, &handle);
        }
    }
}

impl RequestCompletion {
    fn complete(&self, response: Response) {
        *lock(&self.response) = Some(response);
        self.ready.notify_all();
    }

    fn wait(&self) -> Response {
        let mut response = lock(&self.response);
        loop {
            if let Some(response) = response.clone() {
                return response;
            }
            response = self
                .ready
                .wait(response)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

impl RequestCache {
    fn begin(&mut self, request_id: String, fingerprint: [u8; 32]) -> CacheDecision {
        match self.entries.get(&request_id) {
            Some(CachedRequest::Pending {
                fingerprint: cached,
                completion,
            }) if cached == &fingerprint => CacheDecision::Wait(Arc::clone(completion)),
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
                        completion: Arc::new(RequestCompletion::default()),
                    },
                );
                CacheDecision::Execute
            }
        }
    }

    fn complete(&mut self, request_id: String, response: Response, now: Instant) -> Vec<String> {
        let Some(CachedRequest::Pending {
            fingerprint,
            completion,
        }) = self.entries.remove(&request_id)
        else {
            return Vec::new();
        };
        completion.complete(response.clone());
        self.entries.insert(
            request_id,
            CachedRequest::Completed {
                fingerprint,
                claimed: !matches!(response, Response::Open { .. }),
                response,
                completed_at: now,
            },
        );
        self.prune(now)
    }

    fn claim(&mut self, request_id: &str) -> Option<()> {
        let CachedRequest::Completed {
            response, claimed, ..
        } = self.entries.get_mut(request_id)?
        else {
            return None;
        };
        if !matches!(response, Response::Open { .. }) {
            return None;
        }
        *claimed = true;
        Some(())
    }

    fn prune(&mut self, now: Instant) -> Vec<String> {
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
            .values()
            .filter(|entry| matches!(entry, CachedRequest::Completed { .. }))
            .count()
            .saturating_sub(remove.len());
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
            oldest.sort_unstable_by_key(|(_, completed_at)| *completed_at);
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
                    response: Response::Open { handle, .. },
                    claimed: false,
                    ..
                }) => Some(handle),
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

impl PlaintextIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

impl SharedFile {
    fn fully_resident(&self) -> bool {
        let length = self.encrypted.len();
        length == 0
            || self.resident.covers(ByteRange {
                start: 0,
                end: length,
            })
    }
}

impl BrokerError {
    fn new(errno: libc::c_int, message: impl Into<String>) -> Self {
        Self {
            errno,
            message: message.into(),
        }
    }

    fn bad_descriptor() -> Self {
        Self::new(libc::EBADF, "unknown local filesystem handle")
    }

    fn busy() -> Self {
        Self::new(libc::EAGAIN, "local plaintext file is busy")
    }

    fn poisoned() -> Self {
        Self::new(
            libc::EIO,
            "local plaintext file was detached after an abandoned lazy write",
        )
    }

    fn protocol(message: impl Into<String>) -> Self {
        Self::new(libc::EPROTO, message)
    }

    fn protocol_error(error: anyhow::Error) -> Self {
        Self::protocol(error.to_string())
    }

    fn io(context: &str, error: std::io::Error) -> Self {
        Self::new(
            error.raw_os_error().unwrap_or(libc::EIO),
            format!("{context}: {error}"),
        )
    }

    fn anyhow(context: &str, error: anyhow::Error) -> Self {
        let errno = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .and_then(std::io::Error::raw_os_error)
            .unwrap_or(libc::EIO);
        Self::new(errno, format!("{context}: {error:#}"))
    }

    fn into_io(self) -> std::io::Error {
        std::io::Error::other(self.message)
    }
}

fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buffer.is_empty() {
        let read = file.read_at(buffer, offset)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "local plaintext range is incomplete",
            ));
        }
        offset += read as u64;
        buffer = &mut buffer[read..];
    }
    Ok(())
}

fn write_all_at(file: &File, mut buffer: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !buffer.is_empty() {
        let written = file.write_at(buffer, offset)?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to update peer plaintext file",
            ));
        }
        offset += written as u64;
        buffer = &buffer[written..];
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
#[path = "broker/tests.rs"]
mod tests;
