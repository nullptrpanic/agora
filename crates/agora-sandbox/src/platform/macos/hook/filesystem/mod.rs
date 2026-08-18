#![cfg(target_os = "macos")]

use super::config;
use super::dyld::{dyld_interpose, function_from_interpose};
use super::set_errno;
use crate::audit::{AuditClient, AuditError, AuditEventRequest, FileOperation};
use crate::callback::{FileAccessMode, FileContext, FileOpenMode, ProcessContext};
#[cfg(test)]
use crate::filesystem::ByteRangeSet;
use crate::filesystem::broker::{LocalClient, LocalClientError, LocalFileIdentity, LocalOpenState};
use crate::filesystem::{
    AccessPlan, AccessRequest, ByteRange as LocalByteRange, Credentials, DirectoryView,
    FileAttributes, FileLayer, MetadataPlan, NativeDirectorySnapshot, OpenIntent, OpenTarget,
    PreparedFile, StagedWrite, VirtualFilesystem, normalize_path,
};
use crate::trace::TraceContext;
use anyhow::{Context, Result};
use mapping::{MemoryStateIndex, OperationCoordinator};
use nfs::{RemoteAnchor, RemoteDirectoryView, RemoteFilesystem, RemoteOpen};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use content::{EncryptedContent, LocalContentInheritance, ManagedContent, NfsContent};

type GuardId = u64;
const INHERITED_LOCAL_DESCRIPTOR_VERSION: u8 = 6;
const MAX_INHERITED_LOCAL_DESCRIPTORS: usize = 256;

thread_local! {
    static INSIDE_FILESYSTEM_HOOK: Cell<bool> = const { Cell::new(false) };
    static FORK_IN_PROGRESS: Cell<bool> = const { Cell::new(false) };
    static FORK_SUSPENDED_FILESYSTEM_GUARD: Cell<bool> = const { Cell::new(false) };
    #[cfg(test)]
    static TEST_FILESYSTEM_RUNTIME: Cell<*const FilesystemHookRuntime> = const { Cell::new(std::ptr::null()) };
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
static FILESYSTEM_FORK_BARRIER_REGISTRATION: Once = Once::new();
static FILESYSTEM_BARRIER_READ_KEY: OnceLock<libc::pthread_key_t> = OnceLock::new();
static FILESYSTEM_BARRIER_READ_MARKER: u8 = 0;
static mut FILESYSTEM_FORK_BARRIER: libc::pthread_rwlock_t = libc::PTHREAD_RWLOCK_INITIALIZER;

fn require_fork_barrier(result: libc::c_int) {
    if result != 0 {
        unsafe { libc::abort() };
    }
}

unsafe extern "C" fn release_filesystem_barrier_read(value: *mut libc::c_void) {
    if !value.is_null() {
        require_fork_barrier(unsafe {
            libc::pthread_rwlock_unlock(&raw mut FILESYSTEM_FORK_BARRIER)
        });
    }
}

fn filesystem_barrier_read_key() -> libc::pthread_key_t {
    *FILESYSTEM_BARRIER_READ_KEY.get_or_init(|| {
        let mut key = 0;
        require_fork_barrier(unsafe {
            libc::pthread_key_create(&mut key, Some(release_filesystem_barrier_read))
        });
        key
    })
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
fn filesystem_barrier_read_held() -> bool {
    !unsafe { libc::pthread_getspecific(filesystem_barrier_read_key()) }.is_null()
}

fn set_filesystem_barrier_read_held(held: bool) {
    let value = if held {
        (&raw const FILESYSTEM_BARRIER_READ_MARKER)
            .cast_mut()
            .cast::<libc::c_void>()
    } else {
        std::ptr::null_mut()
    };
    require_fork_barrier(unsafe {
        libc::pthread_setspecific(filesystem_barrier_read_key(), value)
    });
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
pub(super) fn initialize_process() -> Result<()> {
    filesystem_barrier_read_key();
    FILESYSTEM_FORK_BARRIER_REGISTRATION.call_once(|| unsafe {
        libc::pthread_atfork(
            Some(lock_filesystem_before_fork),
            Some(unlock_filesystem_after_fork),
            Some(reset_filesystem_after_fork),
        );
    });
    if config::global().is_some() && FilesystemHookRuntime::initialize_global().is_none() {
        anyhow::bail!("failed to initialize the filesystem hook runtime");
    }
    Ok(())
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
unsafe extern "C" fn lock_filesystem_before_fork() {
    FORK_IN_PROGRESS.with(|forking| forking.set(true));
    let suspended = filesystem_barrier_read_held();
    set_filesystem_barrier_read_held(false);
    FORK_SUSPENDED_FILESYSTEM_GUARD.with(|guard| guard.set(suspended));
    if suspended {
        require_fork_barrier(unsafe {
            libc::pthread_rwlock_unlock(&raw mut FILESYSTEM_FORK_BARRIER)
        });
    }
    require_fork_barrier(unsafe { libc::pthread_rwlock_wrlock(&raw mut FILESYSTEM_FORK_BARRIER) });
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
unsafe extern "C" fn unlock_filesystem_after_fork() {
    require_fork_barrier(unsafe { libc::pthread_rwlock_unlock(&raw mut FILESYSTEM_FORK_BARRIER) });
    unsafe { restore_filesystem_guard_after_fork() };
    FORK_IN_PROGRESS.with(|forking| forking.set(false));
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
unsafe extern "C" fn reset_filesystem_after_fork() {
    unsafe {
        std::ptr::write(
            &raw mut FILESYSTEM_FORK_BARRIER,
            libc::PTHREAD_RWLOCK_INITIALIZER,
        );
    }
    unsafe { super::control::reset_after_fork() };
    unsafe { restore_filesystem_guard_after_fork() };
    FORK_IN_PROGRESS.with(|forking| forking.set(false));
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
unsafe fn restore_filesystem_guard_after_fork() {
    let suspended = FORK_SUSPENDED_FILESYSTEM_GUARD.with(|guard| guard.replace(false));
    if !suspended {
        return;
    }
    require_fork_barrier(unsafe { libc::pthread_rwlock_rdlock(&raw mut FILESYSTEM_FORK_BARRIER) });
    set_filesystem_barrier_read_held(true);
}

struct FilesystemHookGuard {
    _signals: super::SignalMaskGuard,
}

impl FilesystemHookGuard {
    fn enter() -> Option<Self> {
        if !super::initialized() && !test_runtime_is_set() {
            return None;
        }

        Self::enter_initialized()
    }

    // Keep Darwin TLV access out of the pre-initialization fast path. On x86_64,
    // optimized code may otherwise hoist the access above the initialized check
    // while libSystem is still bootstrapping thread-local storage.
    #[inline(never)]
    fn enter_initialized() -> Option<Self> {
        let signals = super::SignalMaskGuard::block_or_abort();
        if FORK_IN_PROGRESS.with(|forking| forking.get()) {
            return None;
        }
        let entered = INSIDE_FILESYSTEM_HOOK.with(|inside| !inside.replace(true));
        if !entered {
            return None;
        }
        if unsafe { libc::pthread_rwlock_rdlock(&raw mut FILESYSTEM_FORK_BARRIER) } != 0 {
            INSIDE_FILESYSTEM_HOOK.with(|inside| inside.set(false));
            return None;
        }
        set_filesystem_barrier_read_held(true);
        Some(Self { _signals: signals })
    }
}

#[cfg(test)]
fn test_runtime_is_set() -> bool {
    TEST_FILESYSTEM_RUNTIME.with(|runtime| !runtime.get().is_null())
}

#[cfg(not(test))]
fn test_runtime_is_set() -> bool {
    false
}

impl Drop for FilesystemHookGuard {
    fn drop(&mut self) {
        set_filesystem_barrier_read_held(false);
        require_fork_barrier(unsafe {
            libc::pthread_rwlock_unlock(&raw mut FILESYSTEM_FORK_BARRIER)
        });
        INSIDE_FILESYSTEM_HOOK.with(|inside| inside.set(false));
    }
}

struct FilesystemHookRuntime {
    filesystem: VirtualFilesystem,
    local: Option<LocalClient>,
    remote: Option<RemoteFilesystem>,
    audit: Option<AuditClient>,
    trace: TraceContext,
    native_passthrough_roots: Vec<PathBuf>,
    current_directory: Mutex<CurrentDirectory>,
    open_files: Mutex<HashMap<libc::c_int, Arc<OpenFile>>>,
    mappings: Mutex<Vec<MemoryMapping>>,
    memory_index: MemoryStateIndex,
    operations: OperationCoordinator,
    directory_descriptors: Mutex<HashMap<libc::c_int, DirectoryDescriptor>>,
}

struct DescriptorTransition<'a> {
    runtime: &'a FilesystemHookRuntime,
    descriptor: libc::c_int,
    previous: Option<(bool, bool)>,
    committed: bool,
}

impl DescriptorTransition<'_> {
    fn clear(mut self) {
        self.runtime
            .memory_index
            .set_descriptor(self.descriptor, false, false);
        self.committed = true;
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for DescriptorTransition<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some((data_tracked, mapping_managed)) = self.previous {
            self.runtime.memory_index.set_descriptor(
                self.descriptor,
                data_tracked,
                mapping_managed,
            );
        }
    }
}

#[derive(Clone)]
struct DirectoryDescriptor {
    logical: PathBuf,
    remote: bool,
    native_snapshot: Option<NativeDirectorySnapshot>,
}

struct CurrentDirectory {
    logical: PathBuf,
    remote: bool,
    _anchor: Option<RemoteAnchor>,
}

struct PreparedOpen {
    prepared: PreparedOpenFile,
    file: FileContext,
    logical: PathBuf,
    content: Option<ManagedContent>,
    identity: Option<LogicalFileIdentity>,
}

struct OpenFile {
    file: FileContext,
    logical: Mutex<PathBuf>,
    content: ManagedContent,
    identity: Option<LogicalFileIdentity>,
    layer: FileLayer,
    close_on_exec: bool,
    finished: AtomicBool,
}

impl OpenFile {
    fn managed(&self) -> &ManagedContent {
        &self.content
    }

    fn local_inheritance(&self) -> Option<LocalContentInheritance<'_>> {
        self.managed().local_inheritance()
    }

    fn supports_exec_inheritance(&self) -> bool {
        self.managed().supports_exec_inheritance()
    }

    fn manages_metadata(&self) -> bool {
        self.managed().manages_metadata()
    }

    fn managed_attributes(
        &self,
        runtime: &FilesystemHookRuntime,
    ) -> Result<Option<FileAttributes>> {
        self.managed().file_attributes(runtime)
    }

    fn managed_is_directory(&self) -> bool {
        self.managed().is_directory()
    }

    #[cfg(test)]
    fn managed_handle(&self) -> Option<&str> {
        self.managed().handle()
    }

    fn publishes_writes(&self) -> bool {
        self.managed().publishes_writes()
    }

    fn manages_mappings(&self) -> bool {
        self.managed().is_broker_managed() || self.publishes_writes()
    }
}

#[derive(Serialize, Deserialize)]
struct InheritedLocalDescriptors {
    version: u8,
    descriptors: Vec<InheritedLocalDescriptor>,
}

pub(super) struct InheritedLocalEnvironment {
    pub(super) encoded: String,
    pub(super) handles: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct InheritedLocalDescriptor {
    descriptor: libc::c_int,
    state_descriptor: libc::c_int,
    state_device: u64,
    state_inode: u64,
    lock_descriptor: libc::c_int,
    lock_device: u64,
    lock_inode: u64,
    device: u64,
    inode: u64,
    logical_device: u64,
    logical_inode: u64,
    logical_links: u64,
    file: FileContext,
    logical: Vec<u8>,
    handle: String,
    writable: bool,
    lazy: bool,
}

#[derive(Clone)]
struct MemoryMapping {
    start: usize,
    end: usize,
    file_offset: u64,
    writable: bool,
    open: Arc<OpenFile>,
}

enum PreparedOpenFile {
    Local(PreparedFile),
    Remote(RemoteOpen),
}

#[derive(Clone, Copy)]
struct LogicalFileIdentity {
    device: u64,
    inode: u64,
}

type MetadataMapping = (
    CString,
    Option<libc::off_t>,
    Option<FileAttributes>,
    Option<RemoteAnchor>,
);

static FILESYSTEM_RUNTIME: OnceLock<Option<FilesystemHookRuntime>> = OnceLock::new();

impl PreparedOpen {
    fn has_encrypted_broker(&self) -> bool {
        self.content
            .as_ref()
            .is_some_and(ManagedContent::supports_exec_inheritance)
    }

    fn into_parts(self) -> (OpenTarget, OpenFile) {
        let (target, prepared_content, layer) = self.prepared.into_parts();
        // Broker-managed encrypted opens may still carry the eager fallback
        // produced by the overlay plan. The Broker has always been the
        // authoritative writeback owner in that case.
        let writable = self.file.mode.access != FileAccessMode::Read;
        let content = self
            .content
            .or(prepared_content)
            .unwrap_or_else(|| ManagedContent::plain(writable));
        let close_on_exec = matches!(target, OpenTarget::Descriptor(_));
        (
            target,
            OpenFile {
                file: self.file,
                logical: Mutex::new(self.logical),
                content,
                identity: self.identity,
                layer,
                close_on_exec,
                finished: AtomicBool::new(false),
            },
        )
    }
}

impl PreparedOpenFile {
    fn target(&self) -> &OpenTarget {
        match self {
            Self::Local(prepared) => prepared.target(),
            Self::Remote(prepared) => prepared.target(),
        }
    }

    fn target_mut(&mut self) -> &mut OpenTarget {
        match self {
            Self::Local(prepared) => prepared.target_mut(),
            Self::Remote(prepared) => prepared.target_mut(),
        }
    }

    fn into_parts(self) -> (OpenTarget, Option<ManagedContent>, FileLayer) {
        match self {
            Self::Local(prepared) => {
                let (target, writeback, layer) = prepared.into_parts();
                (
                    target,
                    writeback.map(ManagedContent::eager_encrypted),
                    layer,
                )
            }
            Self::Remote(prepared) => {
                let (target, handle, metadata, writable) = prepared.into_parts();
                (
                    target,
                    Some(ManagedContent::nfs(
                        NfsContent {
                            handle,
                            metadata: Mutex::new(metadata),
                            snapshot: AtomicBool::new(false),
                        },
                        writable,
                    )),
                    FileLayer::Upper,
                )
            }
        }
    }
}

struct OpenRequest {
    logical: PathBuf,
    intent: OpenIntent,
    prepared: PreparedOpenFile,
    file: FileContext,
    allowlisted_passthrough: bool,
}

impl OpenRequest {
    fn native_path(&self) -> Result<Option<CString>> {
        self.allowlisted_passthrough
            .then(|| {
                CString::new(self.logical.as_os_str().as_bytes())
                    .context("native passthrough path contains NUL")
            })
            .transpose()
    }

    fn into_prepared(self) -> PreparedOpen {
        PreparedOpen {
            prepared: self.prepared,
            file: self.file,
            logical: self.logical,
            content: None,
            identity: None,
        }
    }
}

fn intent_from_fopen_mode(mode: &[u8]) -> Result<OpenIntent> {
    let mut flags = match mode.first() {
        Some(b'w') => libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
        Some(b'a') => libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
        _ => libc::O_RDONLY,
    };
    if mode.contains(&b'+') {
        flags = (flags & !libc::O_ACCMODE) | libc::O_RDWR;
    }
    if mode.contains(&b'x') {
        flags |= libc::O_EXCL;
    }
    OpenIntent::new(flags, 0o666)
}

impl OpenFile {
    fn logical(&self) -> PathBuf {
        lock(&self.logical).clone()
    }

    fn retarget(&self, from: &Path, to: &Path) {
        let mut logical = lock(&self.logical);
        if let Ok(suffix) = logical.strip_prefix(from) {
            *logical = to.join(suffix);
        }
    }
}

impl FilesystemHookRuntime {
    fn global() -> Option<&'static Self> {
        Self::global_when_ready(super::initialized() || test_runtime_is_set())
    }

    fn global_when_ready(ready: bool) -> Option<&'static Self> {
        if !ready {
            return None;
        }
        Self::global_initialized()
    }

    // Keep every runtime lookup, including calls from the variadic C shim, out
    // of Darwin TLV and OnceLock initialization until the dylib initializer has
    // completed. The initializer uses `initialize_global` directly.
    #[inline(never)]
    fn global_initialized() -> Option<&'static Self> {
        #[cfg(test)]
        {
            let runtime = TEST_FILESYSTEM_RUNTIME.with(Cell::get);
            if !runtime.is_null() {
                return Some(unsafe { &*runtime });
            }
        }
        Self::initialize_global()
    }

    fn initialize_global() -> Option<&'static Self> {
        FILESYSTEM_RUNTIME
            .get_or_init(|| {
                config::global().and_then(|config| {
                    let filesystem = match config.filesystem_cipher() {
                        Some(cipher) => {
                            VirtualFilesystem::encrypted(config.filesystem_root(), cipher)
                        }
                        None => VirtualFilesystem::plain(config.filesystem_root()),
                    };
                    filesystem.ok().and_then(|filesystem| {
                        let remote = config
                            .remote_filesystem()
                            .map(|(control, token, routes)| {
                                RemoteFilesystem::from_json_with_shared(
                                    control,
                                    token,
                                    routes,
                                    super::control::remote(),
                                )
                            })
                            .transpose()
                            .ok()?;
                        let current_directory = Self::initial_current_directory(
                            &filesystem,
                            remote.as_ref(),
                            config.remote_current_directory(),
                        )
                        .ok()?;
                        let runtime = Self {
                            filesystem,
                            local: config.local_filesystem().map(|(control, token)| {
                                super::control::local().map_or_else(
                                    || LocalClient::new(control, token),
                                    |shared| LocalClient::with_shared(control, token, shared),
                                )
                            }),
                            remote,
                            audit: Some(super::control::audit().map_or_else(
                                || AuditClient::new(config.audit_control(), config.audit_token()),
                                |shared| {
                                    AuditClient::with_shared(
                                        config.audit_control(),
                                        config.audit_token(),
                                        shared,
                                    )
                                },
                            )),
                            trace: config.trace().clone(),
                            native_passthrough_roots: config.native_passthrough_roots().to_vec(),
                            current_directory: Mutex::new(current_directory),
                            open_files: Mutex::new(HashMap::new()),
                            mappings: Mutex::new(Vec::new()),
                            memory_index: MemoryStateIndex::new(),
                            operations: OperationCoordinator::new(),
                            directory_descriptors: Mutex::new(HashMap::new()),
                        };
                        runtime
                            .restore_inherited_local_descriptors(
                                config.inherited_local_descriptors(),
                            )
                            .ok()?;
                        Some(runtime)
                    })
                })
            })
            .as_ref()
    }

    #[cfg(test)]
    fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let filesystem = VirtualFilesystem::plain(root)?;
        let current_directory = Self::native_current_directory(&filesystem)?;
        Ok(Self {
            filesystem,
            local: None,
            remote: None,
            audit: None,
            trace: TraceContext::parse("test-trace").map_err(anyhow::Error::msg)?,
            native_passthrough_roots: vec![PathBuf::from("/dev")],
            current_directory: Mutex::new(CurrentDirectory {
                logical: current_directory,
                remote: false,
                _anchor: None,
            }),
            open_files: Mutex::new(HashMap::new()),
            mappings: Mutex::new(Vec::new()),
            memory_index: MemoryStateIndex::new(),
            operations: OperationCoordinator::new(),
            directory_descriptors: Mutex::new(HashMap::new()),
        })
    }

    #[cfg(test)]
    fn new_encrypted(root: impl Into<PathBuf>, key: &[u8], salt: &[u8]) -> Result<Self> {
        let cipher = crate::filesystem::FileCipher::derive(key, salt)?;
        let filesystem = VirtualFilesystem::encrypted(root, cipher)?;
        let current_directory = Self::native_current_directory(&filesystem)?;
        Ok(Self {
            filesystem,
            local: None,
            remote: None,
            audit: None,
            trace: TraceContext::parse("test-trace").map_err(anyhow::Error::msg)?,
            native_passthrough_roots: vec![PathBuf::from("/dev")],
            current_directory: Mutex::new(CurrentDirectory {
                logical: current_directory,
                remote: false,
                _anchor: None,
            }),
            open_files: Mutex::new(HashMap::new()),
            mappings: Mutex::new(Vec::new()),
            memory_index: MemoryStateIndex::new(),
            operations: OperationCoordinator::new(),
            directory_descriptors: Mutex::new(HashMap::new()),
        })
    }

    fn native_current_directory(filesystem: &VirtualFilesystem) -> Result<PathBuf> {
        let directory = std::env::current_dir().context("failed to resolve current directory")?;
        if filesystem.is_internal(&directory) {
            filesystem.logical_path(&directory)
        } else {
            Ok(directory)
        }
    }

    fn initial_current_directory(
        filesystem: &VirtualFilesystem,
        remote: Option<&RemoteFilesystem>,
        inherited_remote: Option<&Path>,
    ) -> Result<CurrentDirectory> {
        let native = Self::native_current_directory(filesystem)?;
        Self::current_directory_from_native(native, remote, inherited_remote)
    }

    fn current_directory_from_native(
        native: PathBuf,
        remote: Option<&RemoteFilesystem>,
        inherited_remote: Option<&Path>,
    ) -> Result<CurrentDirectory> {
        if let (Some(remote), Some(logical)) = (remote, inherited_remote)
            && let Some(logical) = remote.restore_current_directory(&native, logical)?
        {
            return Ok(CurrentDirectory {
                logical,
                remote: true,
                _anchor: RemoteAnchor::adopt(&native).ok(),
            });
        }
        Ok(CurrentDirectory {
            logical: native,
            remote: false,
            _anchor: None,
        })
    }

    fn native_passthrough_path(&self, path: &Path) -> Result<Option<PathBuf>> {
        let normalized = normalize_path(path)?;
        Ok(self
            .native_passthrough_roots
            .iter()
            .any(|root| normalized.starts_with(root))
            .then_some(normalized))
    }

    unsafe fn native_passthrough_c_path(
        &self,
        path: *const libc::c_char,
        directory: libc::c_int,
    ) -> Result<Option<CString>> {
        let logical = unsafe { self.logical_path(path, directory) }?;
        self.native_passthrough_path(&logical)?
            .map(|native| {
                CString::new(native.as_os_str().as_bytes())
                    .context("native passthrough path contains NUL")
            })
            .transpose()
    }

    unsafe fn native_passthrough_pair(
        &self,
        first: *const libc::c_char,
        first_directory: libc::c_int,
        second: *const libc::c_char,
        second_directory: libc::c_int,
    ) -> Result<Option<(CString, CString)>> {
        let first = unsafe { self.native_passthrough_c_path(first, first_directory) }?;
        let second = unsafe { self.native_passthrough_c_path(second, second_directory) }?;
        Ok(first.zip(second))
    }

    fn native_passthrough_descriptor(&self, descriptor: libc::c_int) -> bool {
        if self.tracked_open(descriptor).is_some()
            || lock(&self.directory_descriptors).contains_key(&descriptor)
        {
            return false;
        }
        Self::descriptor_path(descriptor)
            .ok()
            .and_then(|path| self.native_passthrough_path(&path).ok().flatten())
            .is_some()
    }

    fn prepare_loader_path(&self, requested: &Path) -> Result<Option<CString>> {
        if !requested.is_absolute() {
            return Ok(None);
        }
        if !self.filesystem.is_internal(requested) {
            if self.filesystem.is_private(requested)? {
                return Err(io::Error::from_raw_os_error(libc::EACCES).into());
            }
            return Ok(None);
        }

        let logical = self.logical_or_host(requested)?;
        if let Some(native) = self.native_passthrough_path(&logical)? {
            return CString::new(native.as_os_str().as_bytes())
                .context("native loader path contains NUL")
                .map(Some);
        }
        if let Some(remote) = &self.remote
            && remote.route_result(&logical)?.is_some()
        {
            return Err(io::Error::from_raw_os_error(libc::ENOTSUP).into());
        }
        self.publish_open_writers(&logical)?;
        let intent = OpenIntent::new(libc::O_RDONLY, 0)?;
        let plan = self.filesystem.prepare_authorized_broker_open(
            &logical,
            intent,
            &Credentials::effective(),
        )?;
        let (resolved, prepared) = plan.into_parts();
        self.logical_or_host(&resolved)?;
        let OpenTarget::Path(mapped) = prepared.target() else {
            return Err(io::Error::from_raw_os_error(libc::ENOTSUP).into());
        };
        mapped.metadata()?;
        CString::new(mapped.as_os_str().as_bytes())
            .context("resolved loader path contains NUL")
            .map(Some)
    }

    #[cfg(test)]
    fn map(&self, path: *const libc::c_char, directory: libc::c_int) -> Result<CString> {
        let logical = unsafe { self.logical_path(path, directory) }?;
        let mapped = self.filesystem.prepare_read(&logical)?;
        CString::new(mapped.as_os_str().as_bytes()).context("mapped filesystem path contains NUL")
    }

    fn map_metadata(
        &self,
        path: *const libc::c_char,
        directory: libc::c_int,
        follow_final: bool,
        credentials: &Credentials,
    ) -> Result<MetadataMapping> {
        let logical = unsafe { self.logical_path(path, directory) }?;
        if let Some(native) = self.native_passthrough_path(&logical)? {
            let mapped = CString::new(native.as_os_str().as_bytes())
                .context("native passthrough path contains NUL")?;
            return Ok((mapped, None, None, None));
        }
        if let Some(remote) = &self.remote
            && let Some(routed) = remote.route_result(&logical)?
        {
            self.publish_open_writers(routed.logical())?;
            match remote.stat_plan(&routed) {
                Ok((anchor, plaintext_size, attributes, _)) => {
                    let mapped = anchor.path().to_owned();
                    return Ok((mapped, plaintext_size, Some(attributes), Some(anchor)));
                }
                Err(error) if error_errno(&error) == libc::ENOENT => {}
                Err(error) => return Err(error),
            }
        }
        let plan: MetadataPlan =
            self.filesystem
                .prepare_authorized_metadata(&logical, follow_final, credentials)?;
        let (resolved, mapped, plaintext_size, attributes) = plan.into_parts();
        self.logical_or_host(&resolved)?;
        let mapped = CString::new(mapped.as_os_str().as_bytes())
            .context("mapped filesystem path contains NUL")?;
        let plaintext_size = plaintext_size
            .map(libc::off_t::try_from)
            .transpose()
            .context("plaintext filesystem file is too large")?;
        Ok((mapped, plaintext_size, attributes, None))
    }

    unsafe fn prepare_access(
        &self,
        path: *const libc::c_char,
        directory: libc::c_int,
        follow_final: bool,
        request: AccessRequest,
        credentials: &Credentials,
    ) -> Result<AccessPlan> {
        let logical = unsafe { self.logical_path(path, directory) }?;
        if let Some(native) = self.native_passthrough_path(&logical)? {
            return Ok(AccessPlan::Native(native));
        }
        if let Some(remote) = &self.remote
            && let Some(routed) = remote.route_result(&logical)?
        {
            let mode = ((request.read as libc::c_int) * libc::R_OK)
                | ((request.write as libc::c_int) * libc::W_OK)
                | ((request.execute as libc::c_int) * libc::X_OK);
            match remote.access(&routed, mode) {
                Ok(()) => return Ok(AccessPlan::Allowed),
                Err(error) if error_errno(&error) == libc::ENOENT => {}
                Err(error) => return Err(error),
            }
        }
        self.filesystem
            .check_access(&logical, follow_final, request, credentials)
    }

    fn chmod(
        &self,
        path: *const libc::c_char,
        directory: libc::c_int,
        mode: libc::mode_t,
        follow_final: bool,
    ) -> Result<()> {
        let requested = unsafe { self.logical_path(path, directory) }?;
        if self.remote_entry_exists(&requested)? {
            return Err(io::Error::from_raw_os_error(libc::ENOTSUP).into());
        }
        let credentials = Credentials::effective();
        self.filesystem
            .chmod_authorized(&requested, mode.into(), follow_final, &credentials)
    }

    unsafe fn logical_path(
        &self,
        path: *const libc::c_char,
        directory: libc::c_int,
    ) -> Result<PathBuf> {
        if path.is_null() {
            return Err(io::Error::from_raw_os_error(libc::EFAULT).into());
        }
        let requested = Path::new(OsStr::from_bytes(
            unsafe { CStr::from_ptr(path) }.to_bytes(),
        ));
        if requested.is_absolute() {
            let requested = directory::active_fts_logical_path(requested)
                .unwrap_or_else(|| requested.to_path_buf());
            return self.logical_or_host(&requested);
        }
        let base = if directory == libc::AT_FDCWD {
            lock(&self.current_directory).logical.clone()
        } else {
            self.descriptor_logical_path(directory)
                .map(Ok)
                .unwrap_or_else(|| self.resolve_descriptor_logical_path(directory))?
        };
        let candidate = self.logical_or_host(&base)?.join(requested);
        let candidate = directory::active_fts_logical_path(&candidate).unwrap_or(candidate);
        self.logical_or_host(&candidate)
    }

    fn logical_or_host(&self, path: &Path) -> Result<PathBuf> {
        if self.filesystem.is_internal(path) {
            let path = normalize_path(path)?;
            if !self.filesystem.is_internal(&path) {
                return Err(io::Error::from_raw_os_error(libc::EACCES).into());
            }
            let logical = self.filesystem.logical_path(&path)?;
            if self.filesystem.is_private(&logical)? {
                return Err(io::Error::from_raw_os_error(libc::EACCES).into());
            }
            return Ok(logical);
        }
        if self.filesystem.is_private(path)? {
            return Err(io::Error::from_raw_os_error(libc::EACCES).into());
        }
        Ok(path.to_path_buf())
    }

    fn descriptor_path(descriptor: libc::c_int) -> Result<PathBuf> {
        let mut buffer = vec![0_u8; libc::PATH_MAX as usize];
        if unsafe { libc::fcntl(descriptor, libc::F_GETPATH, buffer.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error())
                .context("failed to resolve directory descriptor");
        }
        let path = CStr::from_bytes_until_nul(&buffer)
            .context("directory descriptor path is not NUL terminated")?;
        Ok(PathBuf::from(OsStr::from_bytes(path.to_bytes())))
    }

    fn prepare_open(
        &self,
        path: *const libc::c_char,
        directory: libc::c_int,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> Result<OpenRequest> {
        let requested = unsafe { self.logical_path(path, directory) }?;
        self.prepare_open_request(requested, OpenIntent::new(flags, mode.into())?)
    }

    fn prepare_materialized_open(
        &self,
        path: *const libc::c_char,
        directory: libc::c_int,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> Result<OpenRequest> {
        let requested = unsafe { self.logical_path(path, directory) }?;
        self.prepare_open_request_with_broker(
            requested,
            OpenIntent::new(flags, mode.into())?,
            false,
        )
    }

    fn prepare_open_request(&self, requested: PathBuf, intent: OpenIntent) -> Result<OpenRequest> {
        self.prepare_open_request_with_broker(requested, intent, true)
    }

    fn prepare_open_request_with_broker(
        &self,
        requested: PathBuf,
        intent: OpenIntent,
        broker_managed: bool,
    ) -> Result<OpenRequest> {
        let flags = intent.flags();
        let allowlisted = self.native_passthrough_path(&requested)?;
        let allowlisted_passthrough = allowlisted.is_some();
        let (logical, prepared) = match allowlisted {
            Some(path) => {
                let prepared = self.filesystem.prepare_native_open(&path);
                (path, PreparedOpenFile::Local(prepared))
            }
            None => {
                if let Some(remote) = &self.remote
                    && let Some(routed) = remote.route_result(&requested)?
                {
                    let logical = routed.logical().to_path_buf();
                    self.publish_open_writers(&logical)?;
                    let prepared = if flags & libc::O_CREAT == 0 {
                        match remote.open(&routed, flags, intent.mode()) {
                            Ok(prepared) => Some(prepared),
                            Err(error) if error_errno(&error) == libc::ENOENT => None,
                            Err(error) => return Err(error),
                        }
                    } else {
                        let remote_exists = match remote.stat(&routed) {
                            Ok(_) => true,
                            Err(error) if error_errno(&error) == libc::ENOENT => false,
                            Err(error) => return Err(error),
                        };
                        let local_exists = !remote_exists && self.filesystem.exists(&requested)?;
                        if remote_exists || !local_exists {
                            Some(remote.open(&routed, flags, intent.mode())?)
                        } else {
                            None
                        }
                    };
                    if let Some(prepared) = prepared {
                        return Ok(Self::open_request(
                            requested,
                            logical,
                            intent,
                            PreparedOpenFile::Remote(prepared),
                            false,
                        ));
                    }
                }
                self.publish_open_writers(&requested)?;
                let credentials = Credentials::effective();
                let mut plan_path = requested.clone();
                let mut synchronized = None;
                loop {
                    let plan = if self.local.is_some() && broker_managed {
                        self.filesystem.prepare_authorized_broker_open(
                            &plan_path,
                            intent,
                            &credentials,
                        )?
                    } else {
                        self.filesystem
                            .prepare_authorized_open(&plan_path, intent, &credentials)?
                    };
                    let logical = self.logical_or_host(plan.logical())?;
                    if logical != plan.logical() {
                        drop(plan);
                        self.publish_open_writers(&logical)?;
                        plan_path = logical;
                        continue;
                    }
                    if logical != requested
                        && synchronized.as_deref() != Some(logical.as_path())
                        && self.has_open_writer(&logical)
                    {
                        drop(plan);
                        self.publish_open_writers(&logical)?;
                        synchronized = Some(logical);
                        continue;
                    }
                    let (logical, prepared) = plan.into_parts();
                    break (logical, PreparedOpenFile::Local(prepared));
                }
            }
        };
        Ok(Self::open_request(
            requested,
            logical,
            intent,
            prepared,
            allowlisted_passthrough,
        ))
    }

    fn open_request(
        requested: PathBuf,
        logical: PathBuf,
        intent: OpenIntent,
        prepared: PreparedOpenFile,
        allowlisted_passthrough: bool,
    ) -> OpenRequest {
        let flags = intent.flags();
        let access = intent.access();
        OpenRequest {
            logical,
            intent,
            prepared,
            file: FileContext {
                path: requested.to_string_lossy().into_owned(),
                mode: FileOpenMode {
                    access: match (access.read, access.write) {
                        (true, true) => FileAccessMode::ReadWrite,
                        (false, true) => FileAccessMode::Write,
                        _ => FileAccessMode::Read,
                    },
                    create: flags & libc::O_CREAT != 0,
                    truncate: flags & libc::O_TRUNC != 0,
                    append: flags & libc::O_APPEND != 0,
                    exclusive: flags & libc::O_EXCL != 0,
                },
            },
            allowlisted_passthrough,
        }
    }

    fn prepare_fopen(
        &self,
        path: *const libc::c_char,
        mode: *const libc::c_char,
    ) -> Result<OpenRequest> {
        if mode.is_null() {
            return Err(io::Error::from_raw_os_error(libc::EFAULT).into());
        }
        let mode = unsafe { CStr::from_ptr(mode) }.to_bytes();
        let requested = unsafe { self.logical_path(path, libc::AT_FDCWD) }?;
        self.prepare_open_request(requested, intent_from_fopen_mode(mode)?)
    }

    fn commit_open(&self, prepared: &mut PreparedOpen) -> Result<()> {
        match &mut prepared.prepared {
            PreparedOpenFile::Local(local) => {
                self.filesystem.commit_open(local)?;
                prepared.identity = local
                    .encrypted_backing_identity()?
                    .map(|(device, inode)| LogicalFileIdentity { device, inode });
                if let Some(client) = &self.local
                    && let Some((path, flags)) = local.local_broker_request()
                {
                    let opened = client.open(&path, flags)?;
                    let writable = flags & libc::O_ACCMODE != libc::O_RDONLY;
                    *local.target_mut() = OpenTarget::Descriptor(opened.descriptor);
                    prepared.content = Some(ManagedContent::encrypted(
                        EncryptedContent {
                            handle: opened.handle,
                            lazy: opened.lazy,
                            state: opened.state,
                            lock: opened.lock,
                            identity: opened.identity,
                        },
                        writable,
                    ));
                }
                Ok(())
            }
            PreparedOpenFile::Remote(prepared) => prepared.commit(),
        }
    }

    fn has_open_writer(&self, logical: &Path) -> bool {
        lock(&self.open_files)
            .values()
            .any(|open| open.publishes_writes() && open.logical() == logical)
    }

    fn publish_open_writers(&self, logical: &Path) -> Result<()> {
        self.flush_logical_mappings(logical, false)?;
        let mut seen = HashSet::new();
        let writers = lock(&self.open_files)
            .values()
            .filter(|open| {
                open.publishes_writes()
                    && open.logical() == logical
                    && seen.insert(Arc::as_ptr(open))
            })
            .cloned()
            .collect::<Vec<_>>();
        for open in writers {
            loop {
                let descriptor =
                    lock(&self.open_files)
                        .iter()
                        .find_map(|(&descriptor, candidate)| {
                            Arc::ptr_eq(candidate, &open).then_some(descriptor)
                        });
                let Some(descriptor) = descriptor else {
                    break;
                };
                let _operation = self.operations.acquire(
                    mapping::OperationRequest::new()
                        .descriptor_registry_shared()
                        .descriptor_shared(descriptor),
                );
                if !self
                    .tracked_open(descriptor)
                    .is_some_and(|current| Arc::ptr_eq(&current, &open))
                {
                    continue;
                }
                self.commit_open_file(descriptor, &open, false)?;
                break;
            }
        }
        Ok(())
    }

    fn prepare_descriptor_mutation(&self, descriptor: libc::c_int) -> Result<StagedWrite> {
        if self.tracked(descriptor).is_some() {
            return Err(io::Error::from_raw_os_error(libc::EALREADY).into());
        }
        let path = Self::descriptor_path(descriptor)?;
        if !self.filesystem.is_internal(&path) {
            return Err(io::Error::from_raw_os_error(libc::EPERM).into());
        }
        if !path.symlink_metadata()?.is_file() {
            return Err(io::Error::from_raw_os_error(libc::ENOTSUP).into());
        }
        let logical = self.filesystem.logical_path(&path)?;
        self.filesystem.stage_write(&logical, false)
    }

    fn publish(&self, operation: FileOperation, file: FileContext) -> Result<(), AuditError> {
        let Some(audit) = &self.audit else {
            return Ok(());
        };
        let executable = super::current_process_executable();
        audit.publish(AuditEventRequest::File {
            trace_id: self.trace.encode(),
            process: ProcessContext {
                pid: std::process::id(),
                ppid: unsafe { libc::getppid() as u32 },
                executable,
            },
            operation,
            file,
        })
    }

    fn register(&self, descriptor: libc::c_int, open: OpenFile) {
        let _operation = self.operations.acquire(
            mapping::OperationRequest::new()
                .descriptor_registry_shared()
                .descriptor_exclusive(descriptor),
        );
        let mut files = lock(&self.open_files);
        let manages_mappings = open.manages_mappings();
        files.insert(descriptor, Arc::new(open));
        self.memory_index
            .set_descriptor(descriptor, true, manages_mappings);
    }

    fn tracked(&self, descriptor: libc::c_int) -> Option<FileContext> {
        lock(&self.open_files)
            .get(&descriptor)
            .map(|open| open.file.clone())
    }

    fn tracked_open(&self, descriptor: libc::c_int) -> Option<Arc<OpenFile>> {
        lock(&self.open_files).get(&descriptor).cloned()
    }

    pub(super) fn retain_local_files_before_fork(&self) -> Result<Vec<String>> {
        let Some(local) = &self.local else {
            return Ok(Vec::new());
        };
        let mut handles = lock(&self.open_files)
            .values()
            .filter_map(|open| {
                open.local_inheritance()
                    .map(|local| local.handle.to_owned())
            })
            .collect::<Vec<_>>();
        handles.extend(lock(&self.mappings).iter().filter_map(|mapping| {
            mapping
                .open
                .local_inheritance()
                .map(|local| local.handle.to_owned())
        }));
        handles.sort_unstable();
        handles.dedup();
        local.retain(handles.clone())?;
        Ok(handles)
    }

    pub(super) fn release_local_files_after_failed_fork(&self, handles: Vec<String>) -> Result<()> {
        let Some(local) = &self.local else {
            return Ok(());
        };
        local.release_retained(handles)?;
        Ok(())
    }

    #[cfg(test)]
    fn duplicate_descriptor(&self, source: libc::c_int, destination: libc::c_int) {
        let _operation = self.operations.acquire(
            mapping::OperationRequest::new()
                .descriptor_registry_shared()
                .descriptor_shared(source)
                .descriptor_exclusive(destination),
        );
        self.duplicate_descriptor_under_lease(source, destination);
    }

    fn duplicate_descriptor_under_lease(&self, source: libc::c_int, destination: libc::c_int) {
        let mut files = lock(&self.open_files);
        let close_on_exec = files
            .get(&source)
            .is_some_and(|open| open.close_on_exec && !open.supports_exec_inheritance());
        let duplicated = match files.get(&source).cloned() {
            Some(open) => {
                files.insert(destination, Arc::clone(&open));
                Some(open)
            }
            None => {
                files.remove(&destination);
                None
            }
        };
        self.memory_index.set_descriptor(
            destination,
            duplicated.is_some(),
            duplicated
                .as_ref()
                .is_some_and(|open| open.manages_mappings()),
        );
        drop(files);

        if let Some(open) = duplicated {
            self.refresh_local_state_inheritance(&open);
        }

        if close_on_exec {
            let flags = unsafe { libc::fcntl(destination, libc::F_GETFD) };
            if flags >= 0 {
                unsafe { libc::fcntl(destination, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
            }
        }

        let mut directories = lock(&self.directory_descriptors);
        match directories.get(&source).cloned() {
            Some(registration) => {
                directories.insert(destination, registration);
            }
            None => {
                directories.remove(&destination);
            }
        }
    }

    fn encode_inherited_local_descriptors(&self) -> Option<InheritedLocalEnvironment> {
        let files = lock(&self.open_files);
        let mut descriptors = Vec::new();
        for (&descriptor, open) in files.iter() {
            let Some(local) = open.local_inheritance() else {
                continue;
            };
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            if flags < 0 || flags & libc::FD_CLOEXEC != 0 {
                continue;
            }
            let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
            if unsafe { libc::fstat(descriptor, &mut status) } != 0 {
                continue;
            }
            let mut lock_status = unsafe { std::mem::zeroed::<libc::stat>() };
            if unsafe { libc::fstat(local.lock.as_raw_fd(), &mut lock_status) } != 0 {
                continue;
            }
            let mut state_status = unsafe { std::mem::zeroed::<libc::stat>() };
            if unsafe { libc::fstat(local.state.as_raw_fd(), &mut state_status) } != 0 {
                continue;
            }
            descriptors.push(InheritedLocalDescriptor {
                descriptor,
                state_descriptor: local.state.as_raw_fd(),
                state_device: state_status.st_dev as u64,
                state_inode: state_status.st_ino,
                lock_descriptor: local.lock.as_raw_fd(),
                lock_device: lock_status.st_dev as u64,
                lock_inode: lock_status.st_ino,
                device: status.st_dev as u64,
                inode: status.st_ino,
                logical_device: local.identity.device,
                logical_inode: local.identity.inode,
                logical_links: local.identity.links,
                file: open.file.clone(),
                logical: open.logical().into_os_string().into_vec(),
                handle: local.handle.to_owned(),
                writable: open.managed().writable(),
                lazy: local.lazy,
            });
        }
        if descriptors.is_empty() {
            return None;
        }
        let mut handles = descriptors
            .iter()
            .map(|descriptor| descriptor.handle.clone())
            .collect::<Vec<_>>();
        handles.sort_unstable();
        handles.dedup();
        let encoded = serde_json::to_string(&InheritedLocalDescriptors {
            version: INHERITED_LOCAL_DESCRIPTOR_VERSION,
            descriptors,
        })
        .ok()?;
        Some(InheritedLocalEnvironment { encoded, handles })
    }

    fn inherited_descriptor_matches(descriptor: libc::c_int, device: u64, inode: u64) -> bool {
        Self::inherited_descriptor_identity(descriptor) == Some((device, inode))
    }

    fn inherited_descriptor_identity(descriptor: libc::c_int) -> Option<(u64, u64)> {
        if descriptor < 0 {
            return None;
        }
        let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
        ((unsafe { libc::fstat(descriptor, &mut status) }) == 0)
            .then_some((status.st_dev as u64, status.st_ino))
    }

    fn duplicate_inherited_state(inherited: &InheritedLocalDescriptor) -> Option<LocalOpenState> {
        if !Self::inherited_descriptor_matches(
            inherited.state_descriptor,
            inherited.state_device,
            inherited.state_inode,
        ) {
            return None;
        }
        let duplicate =
            unsafe { libc::fcntl(inherited.state_descriptor, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return None;
        }
        unsafe { LocalOpenState::from_descriptor(OwnedFd::from_raw_fd(duplicate)) }.ok()
    }

    fn duplicate_inherited_lock(inherited: &InheritedLocalDescriptor) -> Option<File> {
        if !Self::inherited_descriptor_matches(
            inherited.lock_descriptor,
            inherited.lock_device,
            inherited.lock_inode,
        ) {
            return None;
        }
        let duplicate = unsafe { libc::fcntl(inherited.lock_descriptor, libc::F_DUPFD_CLOEXEC, 0) };
        (duplicate >= 0).then(|| unsafe { File::from(OwnedFd::from_raw_fd(duplicate)) })
    }

    fn inherited_auxiliary_roles_are_valid(
        inherited: &InheritedLocalDescriptor,
        content_descriptors: &HashSet<libc::c_int>,
    ) -> bool {
        inherited.state_descriptor >= 0
            && inherited.lock_descriptor >= 0
            && inherited.state_descriptor != inherited.lock_descriptor
            && !content_descriptors.contains(&inherited.state_descriptor)
            && !content_descriptors.contains(&inherited.lock_descriptor)
    }

    fn restore_inherited_local_descriptors(&self, encoded: Option<&str>) -> Result<()> {
        let Some(encoded) = encoded else {
            return Ok(());
        };
        let Ok(inherited) = serde_json::from_str::<InheritedLocalDescriptors>(encoded) else {
            return Ok(());
        };
        if inherited.version != INHERITED_LOCAL_DESCRIPTOR_VERSION
            || inherited.descriptors.len() > MAX_INHERITED_LOCAL_DESCRIPTORS
        {
            return Ok(());
        }
        let Some(local) = &self.local else {
            return Ok(());
        };
        let content_descriptors = inherited
            .descriptors
            .iter()
            .map(|descriptor| descriptor.descriptor)
            .collect::<HashSet<_>>();
        let internal_identities = inherited
            .descriptors
            .iter()
            .flat_map(|descriptor| {
                [
                    (descriptor.device, descriptor.inode),
                    (descriptor.state_device, descriptor.state_inode),
                    (descriptor.lock_device, descriptor.lock_inode),
                ]
            })
            .collect::<HashSet<_>>();
        let mut advertised_handles = inherited
            .descriptors
            .iter()
            .map(|descriptor| descriptor.handle.clone())
            .collect::<Vec<_>>();
        advertised_handles.sort_unstable();
        advertised_handles.dedup();
        let mut handles = HashMap::<String, (libc::c_int, libc::c_int, Arc<OpenFile>)>::new();
        let mut state_owners = HashMap::<libc::c_int, String>::new();
        let mut lock_owners = HashMap::<libc::c_int, String>::new();
        let mut restored_descriptors = HashSet::new();
        let mut files = lock(&self.open_files);
        for inherited in &inherited.descriptors {
            if inherited.descriptor < 0 {
                continue;
            }
            let flags = unsafe { libc::fcntl(inherited.descriptor, libc::F_GETFD) };
            if flags < 0 || flags & libc::FD_CLOEXEC != 0 {
                continue;
            }
            let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
            if unsafe { libc::fstat(inherited.descriptor, &mut status) } != 0
                || status.st_dev as u64 != inherited.device
                || status.st_ino != inherited.inode
            {
                continue;
            }
            let open = if let Some((state_descriptor, lock_descriptor, open)) =
                handles.get(&inherited.handle)
            {
                if *state_descriptor != inherited.state_descriptor
                    || *lock_descriptor != inherited.lock_descriptor
                {
                    continue;
                }
                Arc::clone(open)
            } else {
                if !Self::inherited_auxiliary_roles_are_valid(inherited, &content_descriptors)
                    || state_owners
                        .get(&inherited.state_descriptor)
                        .is_some_and(|owner| owner != &inherited.handle)
                    || state_owners
                        .get(&inherited.lock_descriptor)
                        .is_some_and(|owner| owner != &inherited.handle)
                    || lock_owners
                        .get(&inherited.lock_descriptor)
                        .is_some_and(|owner| owner != &inherited.handle)
                    || lock_owners
                        .get(&inherited.state_descriptor)
                        .is_some_and(|owner| owner != &inherited.handle)
                {
                    continue;
                }
                let Some(state) = Self::duplicate_inherited_state(inherited) else {
                    continue;
                };
                let Some(lock_descriptor) = Self::duplicate_inherited_lock(inherited) else {
                    continue;
                };
                let handle = inherited.handle.clone();
                let open = Arc::new(OpenFile {
                    file: inherited.file.clone(),
                    logical: Mutex::new(PathBuf::from(OsString::from_vec(
                        inherited.logical.clone(),
                    ))),
                    content: ManagedContent::encrypted(
                        EncryptedContent {
                            handle: inherited.handle.clone(),
                            lazy: inherited.lazy,
                            state,
                            lock: lock_descriptor,
                            identity: LocalFileIdentity {
                                device: inherited.logical_device,
                                inode: inherited.logical_inode,
                                links: inherited.logical_links,
                            },
                        },
                        inherited.writable,
                    ),
                    identity: Some(LogicalFileIdentity {
                        device: inherited.logical_device,
                        inode: inherited.logical_inode,
                    }),
                    layer: FileLayer::Upper,
                    close_on_exec: true,
                    finished: AtomicBool::new(false),
                });
                state_owners.insert(inherited.state_descriptor, handle.clone());
                lock_owners.insert(inherited.lock_descriptor, handle.clone());
                handles.insert(
                    handle,
                    (
                        inherited.state_descriptor,
                        inherited.lock_descriptor,
                        Arc::clone(&open),
                    ),
                );
                open
            };
            let manages_mappings = open.manages_mappings();
            files.insert(inherited.descriptor, open);
            restored_descriptors.insert(inherited.descriptor);
            self.memory_index
                .set_descriptor(inherited.descriptor, true, manages_mappings);
        }
        let restored_handles = handles.keys().cloned().collect::<HashSet<_>>();
        drop(files);

        let mut descriptors_to_close = HashSet::new();
        for inherited in &inherited.descriptors {
            for descriptor in [
                inherited.descriptor,
                inherited.state_descriptor,
                inherited.lock_descriptor,
            ] {
                if !restored_descriptors.contains(&descriptor)
                    && Self::inherited_descriptor_identity(descriptor)
                        .is_some_and(|identity| internal_identities.contains(&identity))
                {
                    descriptors_to_close.insert(descriptor);
                }
            }
        }

        let unmatched_handles = advertised_handles
            .into_iter()
            .filter(|handle| !restored_handles.contains(handle))
            .collect::<Vec<_>>();
        let mut cleanup_error = local
            .release_retained(unmatched_handles)
            .err()
            .map(anyhow::Error::from);
        for descriptor in descriptors_to_close {
            if unsafe { libc::close(descriptor) } != 0 && cleanup_error.is_none() {
                cleanup_error = Some(anyhow::anyhow!(
                    "failed to close inherited local descriptor {descriptor}: {}",
                    io::Error::last_os_error()
                ));
            }
        }
        match cleanup_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    fn take_descriptor(&self, descriptor: libc::c_int) -> Option<(Arc<OpenFile>, bool)> {
        let _operation = self.operations.acquire(
            mapping::OperationRequest::new()
                .descriptor_registry_shared()
                .descriptor_exclusive(descriptor),
        );
        self.take_descriptor_under_lease(descriptor)
    }

    #[cfg(test)]
    fn take_descriptor_under_lease(
        &self,
        descriptor: libc::c_int,
    ) -> Option<(Arc<OpenFile>, bool)> {
        let tracked = self.remove_descriptor_under_lease(descriptor);
        self.memory_index.set_descriptor(descriptor, false, false);
        tracked
    }

    fn take_descriptor_during_transition_under_lease(
        &self,
        descriptor: libc::c_int,
    ) -> Option<(Arc<OpenFile>, bool)> {
        self.remove_descriptor_under_lease(descriptor)
    }

    fn remove_descriptor_under_lease(
        &self,
        descriptor: libc::c_int,
    ) -> Option<(Arc<OpenFile>, bool)> {
        let mut files = lock(&self.open_files);
        let open = files.remove(&descriptor)?;
        let last_alias = !files
            .values()
            .any(|candidate| Arc::ptr_eq(candidate, &open));
        drop(files);
        self.refresh_local_state_inheritance(&open);
        Some((open, last_alias))
    }

    fn begin_descriptor_transition_under_lease(
        &self,
        descriptor: libc::c_int,
    ) -> DescriptorTransition<'_> {
        let previous = self.memory_index.descriptor_routing_state(descriptor);
        self.memory_index.set_descriptor(descriptor, true, true);
        DescriptorTransition {
            runtime: self,
            descriptor,
            previous,
            committed: false,
        }
    }

    #[cfg(test)]
    fn restore_descriptor(&self, descriptor: libc::c_int, open: Arc<OpenFile>) {
        let _operation = self.operations.acquire(
            mapping::OperationRequest::new()
                .descriptor_registry_shared()
                .descriptor_exclusive(descriptor),
        );
        self.restore_descriptor_under_lease(descriptor, open);
    }

    fn restore_descriptor_under_lease(&self, descriptor: libc::c_int, open: Arc<OpenFile>) {
        let mut files = lock(&self.open_files);
        let manages_mappings = open.manages_mappings();
        files.insert(descriptor, Arc::clone(&open));
        self.memory_index
            .set_descriptor(descriptor, true, manages_mappings);
        drop(files);
        self.refresh_local_state_inheritance(&open);
    }

    fn refresh_local_state_inheritance(&self, open: &Arc<OpenFile>) {
        let Some(local) = open.local_inheritance() else {
            return;
        };
        let inheritable = lock(&self.open_files)
            .iter()
            .any(|(&descriptor, candidate)| {
                if !Arc::ptr_eq(candidate, open) {
                    return false;
                }
                let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
                flags >= 0 && flags & libc::FD_CLOEXEC == 0
            });
        let _ = local.state.set_close_on_exec(!inheritable);
        let _ = set_descriptor_close_on_exec(local.lock.as_raw_fd(), !inheritable);
    }

    fn inheritable_local_descriptors(&self) -> Vec<libc::c_int> {
        let files = lock(&self.open_files);
        let mut descriptors = Vec::new();
        let mut states = HashSet::new();
        for (&descriptor, open) in files.iter() {
            let Some(local) = open.local_inheritance() else {
                continue;
            };
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            if flags < 0 || flags & libc::FD_CLOEXEC != 0 {
                continue;
            }
            descriptors.push(descriptor);
            if states.insert(local.state.as_raw_fd()) {
                descriptors.push(local.state.as_raw_fd());
            }
            if states.insert(local.lock.as_raw_fd()) {
                descriptors.push(local.lock.as_raw_fd());
            }
        }
        descriptors
    }

    fn writeback(&self, descriptor: libc::c_int) -> Result<()> {
        self.writeback_descriptor(descriptor).map(drop)
    }

    fn commit_open_file(
        &self,
        descriptor: libc::c_int,
        open: &OpenFile,
        durable: bool,
    ) -> Result<()> {
        open.managed().sync(self, descriptor, open, durable)
    }

    fn finish_open_file(&self, descriptor: libc::c_int, open: &OpenFile) -> Result<()> {
        if open.finished.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.finish_claimed_open_file(descriptor, open)
    }

    fn finish_claimed_open_file(&self, descriptor: libc::c_int, open: &OpenFile) -> Result<()> {
        let result = open.managed().finish(self, descriptor, open);
        if result.is_err() {
            open.finished.store(false, Ordering::Release);
        }
        result
    }

    fn commit_all_open_files(&self) -> Result<()> {
        let (stale, open_files) = {
            let _barrier = self
                .operations
                .acquire(mapping::OperationRequest::new().descriptor_registry_exclusive());
            let mut files = lock(&self.open_files);
            let mut stale = Vec::new();
            files.retain(|&descriptor, open| {
                if unsafe { libc::fcntl(descriptor, libc::F_GETFD) } >= 0 {
                    true
                } else {
                    stale.push(Arc::clone(open));
                    self.memory_index.set_descriptor(descriptor, false, false);
                    false
                }
            });
            let mut seen = HashSet::new();
            let open_files = files
                .values()
                .filter(|open| seen.insert(Arc::as_ptr(open)))
                .cloned()
                .collect::<Vec<_>>();
            (stale, open_files)
        };
        self.finish_unreferenced(stale)?;
        for open in open_files {
            loop {
                let descriptor =
                    lock(&self.open_files)
                        .iter()
                        .find_map(|(&descriptor, candidate)| {
                            Arc::ptr_eq(candidate, &open).then_some(descriptor)
                        });
                let Some(descriptor) = descriptor else {
                    break;
                };
                let _operation = self.operations.acquire(
                    mapping::OperationRequest::new()
                        .descriptor_registry_shared()
                        .descriptor_shared(descriptor),
                );
                if !self
                    .tracked_open(descriptor)
                    .is_some_and(|current| Arc::ptr_eq(&current, &open))
                {
                    continue;
                }
                self.commit_open_file(descriptor, &open, true)
                    .with_context(|| {
                        format!(
                            "failed to synchronize descriptor {descriptor} for {}",
                            open.logical().display()
                        )
                    })?;
                break;
            }
        }
        Ok(())
    }

    #[cfg(any(agora_sandbox_hook_build, test, coverage))]
    fn finish_all_open_files(&self) -> Result<()> {
        // Make mapped pages visible in the shared plaintext vnode first. The
        // subsequent Close is the sole encrypted and durable writeback owner;
        // sending a separate Broker Sync here would encrypt every writable
        // mapping twice during normal process exit.
        self.flush_native_memory_mappings()?;
        let open_files = {
            let _barrier = self.operations.acquire(
                mapping::OperationRequest::new()
                    .descriptor_registry_exclusive()
                    .mapping_registry_exclusive(),
            );
            let files = lock(&self.open_files);
            let mappings = lock(&self.mappings);
            let mut seen = HashSet::new();
            let mut open_files = Vec::new();
            for (&descriptor, open) in files.iter() {
                if seen.insert(Arc::as_ptr(open)) {
                    open_files.push((descriptor, Arc::clone(open)));
                }
            }
            for mapping in mappings.iter() {
                if seen.insert(Arc::as_ptr(&mapping.open)) {
                    open_files.push((-1, Arc::clone(&mapping.open)));
                }
            }
            open_files
        };
        let mut first = None;
        for (descriptor, open) in open_files {
            if let Err(error) = self.finish_open_file(descriptor, &open)
                && first.is_none()
            {
                first = Some(error);
            }
        }
        match first {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn refresh_attributes(&self, descriptor: libc::c_int, path: &str) -> Result<()> {
        if self
            .tracked_open(descriptor)
            .is_some_and(|open| open.manages_metadata())
        {
            return Ok(());
        }
        let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::fstat(descriptor, &mut status) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        self.filesystem
            .set_attributes(Path::new(path), FileAttributes::from_stat(&status))
    }

    fn refresh_open_attributes(&self, descriptor: libc::c_int, open: &OpenFile) -> Result<()> {
        let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::fstat(descriptor, &mut status) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let logical = open.logical();
        if let Some(identity) = open.identity {
            match self.filesystem.visible_identity(&logical) {
                Ok((device, inode)) if device == identity.device && inode == identity.inode => {}
                Ok(_) => return Ok(()),
                Err(error) if error_errno(&error) == libc::ENOENT => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        self.filesystem.refresh_timestamps(&logical, &status)
    }

    fn create_directory(
        &self,
        directory: libc::c_int,
        path: *const libc::c_char,
        mode: libc::mode_t,
    ) -> Result<()> {
        let logical = unsafe { self.logical_path(path, directory) }?;
        if let Some(native) = self.native_passthrough_path(&logical)? {
            let native = CString::new(native.as_os_str().as_bytes())
                .context("native passthrough path contains NUL")?;
            let original =
                original_mkdir().ok_or_else(|| io::Error::from_raw_os_error(libc::ENOSYS))?;
            return native_operation_result(unsafe { original(native.as_ptr(), mode) });
        }
        if let Some(remote) = &self.remote
            && let Some(routed) = remote.route_result(&logical)?
        {
            match remote.stat(&routed) {
                Ok(_) => return remote.create_directory(&routed, mode),
                Err(error) if error_errno(&error) == libc::ENOENT => {
                    if !self.filesystem.exists(&logical)? {
                        return remote.create_directory(&routed, mode);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        self.filesystem
            .create_directory_authorized(&logical, u32::from(mode), &Credentials::effective())
            .map(|_| ())
    }

    fn create_symlink(
        &self,
        target: *const libc::c_char,
        directory: libc::c_int,
        link: *const libc::c_char,
    ) -> Result<()> {
        if target.is_null() {
            return Err(io::Error::from_raw_os_error(libc::EFAULT).into());
        }
        let link = unsafe { self.logical_path(link, directory) }?;
        if let Some(native) = self.native_passthrough_path(&link)? {
            let native = CString::new(native.as_os_str().as_bytes())
                .context("native passthrough path contains NUL")?;
            let original =
                original_symlink().ok_or_else(|| io::Error::from_raw_os_error(libc::ENOSYS))?;
            return native_operation_result(unsafe { original(target, native.as_ptr()) });
        }
        if let Some(remote) = &self.remote
            && let Some(routed) = remote.route_result(&link)?
        {
            self.publish_open_writers(routed.logical())?;
            match remote.stat(&routed) {
                Ok(_) => return Err(io::Error::from_raw_os_error(libc::ENOTSUP).into()),
                Err(error) if error_errno(&error) == libc::ENOENT => {
                    if !self.filesystem.exists(&link)? {
                        return Err(io::Error::from_raw_os_error(libc::ENOTSUP).into());
                    }
                }
                Err(error) => return Err(error),
            }
        }
        let requested = Path::new(OsStr::from_bytes(unsafe {
            CStr::from_ptr(target).to_bytes()
        }));
        let target = if requested.is_absolute() {
            self.logical_or_host(requested)?
        } else {
            requested.to_path_buf()
        };
        self.filesystem
            .create_symlink_authorized(&link, &target, &Credentials::effective())
            .map(|_| ())
    }

    fn remove(
        &self,
        directory: libc::c_int,
        path: *const libc::c_char,
        remove_directory: bool,
    ) -> Result<()> {
        let logical = unsafe { self.logical_path(path, directory) }?;
        if let Some(native) = self.native_passthrough_path(&logical)? {
            let native = CString::new(native.as_os_str().as_bytes())
                .context("native passthrough path contains NUL")?;
            let original = if remove_directory {
                original_rmdir()
            } else {
                original_unlink()
            }
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOSYS))?;
            return native_operation_result(unsafe { original(native.as_ptr()) });
        }
        if let Some(remote) = &self.remote
            && let Some(routed) = remote.route_result(&logical)?
        {
            match remote.remove(&routed, remove_directory) {
                Ok(()) => return Ok(()),
                Err(error) if error_errno(&error) == libc::ENOENT => {}
                Err(error) => return Err(error),
            }
        }
        self.filesystem
            .remove_authorized(&logical, remove_directory, &Credentials::effective())
    }

    fn rename(
        &self,
        from_directory: libc::c_int,
        from: *const libc::c_char,
        to_directory: libc::c_int,
        to: *const libc::c_char,
    ) -> Result<()> {
        let from = unsafe { self.logical_path(from, from_directory) }?;
        let to = unsafe { self.logical_path(to, to_directory) }?;
        let native_from = self.native_passthrough_path(&from)?;
        let native_to = self.native_passthrough_path(&to)?;
        match (native_from, native_to) {
            (Some(from), Some(to)) => {
                let from = CString::new(from.as_os_str().as_bytes())
                    .context("native passthrough path contains NUL")?;
                let to = CString::new(to.as_os_str().as_bytes())
                    .context("native passthrough path contains NUL")?;
                let original =
                    original_rename().ok_or_else(|| io::Error::from_raw_os_error(libc::ENOSYS))?;
                return native_operation_result(unsafe { original(from.as_ptr(), to.as_ptr()) });
            }
            (None, None) => {}
            _ => return Err(io::Error::from_raw_os_error(libc::EXDEV).into()),
        }
        if let Some(remote) = &self.remote {
            let remote_from = remote.route_result(&from)?;
            let remote_to = remote.route_result(&to)?;
            match (remote_from, remote_to) {
                (Some(remote_from), Some(remote_to)) => {
                    self.publish_open_writers(remote_from.logical())?;
                    match remote.stat(&remote_from) {
                        Ok(_) => {
                            remote.rename(&remote_from, &remote_to)?;
                            self.retarget_open_files(&from, &to);
                            return Ok(());
                        }
                        Err(error) if error_errno(&error) == libc::ENOENT => {
                            if self.filesystem.exists(&from)? && self.remote_entry_exists(&to)? {
                                return Err(io::Error::from_raw_os_error(libc::EXDEV).into());
                            }
                        }
                        Err(error) => return Err(error),
                    }
                }
                (None, None) => {}
                _ => return Err(io::Error::from_raw_os_error(libc::EXDEV).into()),
            }
        }
        let credentials = Credentials::effective();
        self.filesystem
            .rename_authorized(&from, &to, &credentials)?;
        self.retarget_open_files(&from, &to);
        Ok(())
    }

    fn retarget_open_files(&self, from: &Path, to: &Path) {
        let open_files = lock(&self.open_files).values().cloned().collect::<Vec<_>>();
        for open in open_files {
            open.retarget(from, to);
        }
    }

    fn prepare_change_directory(
        &self,
        path: *const libc::c_char,
    ) -> Result<(CString, PathBuf, bool, Option<RemoteAnchor>)> {
        let requested = unsafe { self.logical_path(path, libc::AT_FDCWD) }?;
        if let Some(native) = self.native_passthrough_path(&requested)? {
            let mapped = CString::new(native.as_os_str().as_bytes())
                .context("native passthrough path contains NUL")?;
            return Ok((mapped, native, false, None));
        }
        if let Some(remote) = &self.remote
            && let Some(routed) = remote.route_result(&requested)?
        {
            match remote.stat_plan(&routed) {
                Ok((anchor, _, _, metadata)) => {
                    if metadata.file_type != crate::nfs::protocol::RemoteFileType::Directory {
                        return Err(io::Error::from_raw_os_error(libc::ENOTDIR).into());
                    }
                    let mapped = anchor.path().to_owned();
                    return Ok((mapped, requested, true, Some(anchor)));
                }
                Err(error) if error_errno(&error) == libc::ENOENT => {}
                Err(error) => return Err(error),
            }
        }
        let credentials = Credentials::effective();
        let (mapped, logical) = self
            .filesystem
            .prepare_change_directory(&requested, &credentials)?;
        self.logical_or_host(&logical)?;
        let mapped = CString::new(mapped.as_os_str().as_bytes())
            .context("mapped filesystem path contains NUL")?;
        Ok((mapped, logical, false, None))
    }

    fn remote_directory_view(
        &self,
        path: *const libc::c_char,
    ) -> Result<Option<RemoteDirectoryView>> {
        let logical = unsafe { self.logical_path(path, libc::AT_FDCWD) }?;
        self.remote_directory_view_for_logical(&logical)
    }

    fn remote_directory_view_for_logical(
        &self,
        logical: &Path,
    ) -> Result<Option<RemoteDirectoryView>> {
        let Some(remote) = &self.remote else {
            return Ok(None);
        };
        let Some(routed) = remote.route_result(logical)? else {
            return Ok(None);
        };
        match remote.directory_view(&routed) {
            Ok(view) => Ok(Some(view)),
            Err(error) if error_errno(&error) == libc::ENOENT => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn remote_route_root_names(&self, parent: &Path) -> Result<Vec<Vec<u8>>> {
        self.remote
            .as_ref()
            .map_or_else(|| Ok(Vec::new()), |remote| remote.route_root_names(parent))
    }

    fn descriptor_remote_directory_view(
        &self,
        descriptor: libc::c_int,
    ) -> Result<Option<RemoteDirectoryView>> {
        let Some(open) = self.tracked_open(descriptor) else {
            return Ok(None);
        };
        if !open.manages_metadata() {
            return Ok(None);
        }
        if !open.managed_is_directory() {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR).into());
        }
        let remote = self
            .remote
            .as_ref()
            .context("remote filesystem runtime is unavailable")?;
        let routed = remote
            .route_result(&open.logical())?
            .context("remote descriptor is outside configured routes")?;
        remote.directory_view(&routed).map(Some)
    }

    #[cfg(test)]
    fn is_nfs_route(&self, path: &Path) -> bool {
        self.remote
            .as_ref()
            .is_some_and(|remote| remote.route(path).is_some())
    }

    fn path_exists(&self, path: &Path) -> Result<bool> {
        if self.remote_entry_exists(path)? {
            return Ok(true);
        }
        self.filesystem.exists(path)
    }

    #[cfg(test)]
    fn set_current_directory(&self, directory: PathBuf) {
        let remote = self.is_nfs_route(&directory);
        self.set_current_directory_state(directory, remote, None);
    }

    fn set_current_directory_state(
        &self,
        directory: PathBuf,
        remote: bool,
        anchor: Option<RemoteAnchor>,
    ) {
        *lock(&self.current_directory) = CurrentDirectory {
            logical: directory,
            remote,
            _anchor: anchor,
        };
    }

    fn synchronize_current_directory(&self) -> Result<()> {
        let directory = Self::native_current_directory(&self.filesystem)?;
        self.set_current_directory_state(directory, false, None);
        Ok(())
    }

    fn synchronize_current_directory_for_fts(&self) -> Result<()> {
        if lock(&self.current_directory).remote {
            return Ok(());
        }
        self.synchronize_current_directory()
    }

    fn prepare_child_current_directory(&self) -> Result<Option<PathBuf>> {
        let current = {
            let current = lock(&self.current_directory);
            (current.logical.clone(), current.remote)
        };
        if Self::native_current_directory(&self.filesystem).is_err() {
            let requested = CString::new(current.0.as_os_str().as_bytes())
                .context("logical current directory contains NUL")?;
            let (mapped, logical, remote, anchor) =
                self.prepare_change_directory(requested.as_ptr())?;
            directory::change_directory_native(mapped.as_c_str())?;
            self.set_current_directory_state(logical, remote, anchor);
        }
        let current = lock(&self.current_directory);
        Ok(current.remote.then(|| current.logical.clone()))
    }

    fn descriptor_logical_path(&self, descriptor: libc::c_int) -> Option<PathBuf> {
        self.tracked_open(descriptor)
            .map(|open| open.logical())
            .or_else(|| {
                lock(&self.directory_descriptors)
                    .get(&descriptor)
                    .map(|registration| registration.logical.clone())
            })
    }

    fn resolve_descriptor_logical_path(&self, descriptor: libc::c_int) -> Result<PathBuf> {
        if let Some(logical) = self.descriptor_logical_path(descriptor) {
            return Ok(logical);
        }
        let path = Self::descriptor_path(descriptor)?;
        if self.filesystem.is_internal(&path) {
            self.filesystem.logical_path(&path)
        } else {
            self.logical_or_host(&path)
        }
    }

    fn register_directory(
        &self,
        descriptor: libc::c_int,
        logical: PathBuf,
        remote: bool,
        native_snapshot: Option<NativeDirectorySnapshot>,
    ) {
        lock(&self.directory_descriptors).insert(
            descriptor,
            DirectoryDescriptor {
                logical,
                remote,
                native_snapshot,
            },
        );
    }

    fn native_directory_snapshot_is_current(&self, descriptor: libc::c_int) -> bool {
        let snapshot = lock(&self.directory_descriptors)
            .get(&descriptor)
            .and_then(|registration| registration.native_snapshot.clone());
        snapshot.is_some_and(|snapshot| {
            self.filesystem
                .native_directory_snapshot_is_current(&snapshot)
                .unwrap_or(false)
        })
    }

    fn unregister_directory(&self, descriptor: libc::c_int) {
        lock(&self.directory_descriptors).remove(&descriptor);
    }

    fn logical_current_directory(&self) -> Result<CString> {
        let current = lock(&self.current_directory);
        CString::new(current.logical.as_os_str().as_bytes())
            .context("current directory contains NUL")
    }

    unsafe fn canonical_path(&self, path: *const libc::c_char) -> Result<CString> {
        let logical = unsafe { self.logical_path(path, libc::AT_FDCWD) }?;
        if let Some(remote) = &self.remote
            && let Some(routed) = remote.route_result(&logical)?
        {
            match remote.stat(&routed) {
                Ok(_) => {
                    return CString::new(logical.as_os_str().as_bytes())
                        .context("canonical remote filesystem path contains NUL");
                }
                Err(error) if error_errno(&error) == libc::ENOENT => {}
                Err(error) => return Err(error),
            }
        }
        let canonical = self
            .filesystem
            .canonicalize_authorized(&logical, &Credentials::effective())?;
        let canonical = self.logical_or_host(&canonical)?;
        CString::new(canonical.as_os_str().as_bytes())
            .context("canonical filesystem path contains NUL")
    }

    fn directory_view(&self, path: *const libc::c_char) -> Result<DirectoryView> {
        let logical = unsafe { self.logical_path(path, libc::AT_FDCWD) }?;
        if let Some(native) = self.native_passthrough_path(&logical)? {
            return Ok(DirectoryView::passthrough(native));
        }
        let credentials = Credentials::effective();
        self.filesystem
            .directory_view_authorized(&logical, &credentials)
    }

    fn local_directory_view_for_remote(&self, logical: &Path) -> Result<Option<DirectoryView>> {
        match self.filesystem.directory_view(logical) {
            Ok(view) => Ok(Some(view)),
            Err(error) if matches!(error_errno(&error), libc::ENOENT | libc::ENOTDIR) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn descriptor_directory_view(
        &self,
        descriptor: libc::c_int,
    ) -> Result<(DirectoryView, FileLayer)> {
        let (logical, layer) = if let Some(open) = self.tracked_open(descriptor) {
            (open.logical(), open.layer)
        } else {
            let path = Self::descriptor_path(descriptor)?;
            let layer = if self.filesystem.is_internal(&path) {
                FileLayer::Upper
            } else {
                FileLayer::Lower
            };
            let logical = if layer == FileLayer::Upper {
                self.filesystem.logical_path(&path)?
            } else {
                self.logical_or_host(&path)?
            };
            (logical, layer)
        };
        if let Some(native) = self.native_passthrough_path(&logical)? {
            return Ok((DirectoryView::passthrough(native), FileLayer::Lower));
        }
        Ok((self.filesystem.directory_view(&logical)?, layer))
    }

    fn remote_entry_exists(&self, logical: &Path) -> Result<bool> {
        let Some(remote) = &self.remote else {
            return Ok(false);
        };
        let Some(routed) = remote.route_result(logical)? else {
            return Ok(false);
        };
        self.publish_open_writers(routed.logical())?;
        match remote.stat(&routed) {
            Ok(_) => Ok(true),
            Err(error) if error_errno(&error) == libc::ENOENT => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn native_operation_result(result: libc::c_int) -> Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error().into())
    }
}

fn set_descriptor_close_on_exec(descriptor: libc::c_int, close: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = if close {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn tracked_current_directory() -> Option<PathBuf> {
    let _guard = FilesystemHookGuard::enter()?;
    FilesystemHookRuntime::global().map(|runtime| lock(&runtime.current_directory).logical.clone())
}

pub(super) fn prepare_child_current_directory() -> Result<Option<PathBuf>> {
    if config::global().is_none() {
        return Ok(None);
    }
    let _guard = FilesystemHookGuard::enter()
        .context("filesystem hook is unavailable while preparing a child process")?;
    let runtime = FilesystemHookRuntime::global()
        .context("filesystem hook runtime is unavailable while preparing a child process")?;
    runtime.prepare_child_current_directory()
}

pub(super) fn inherited_local_descriptors() -> Option<InheritedLocalEnvironment> {
    let _guard = FilesystemHookGuard::enter()?;
    FILESYSTEM_RUNTIME
        .get()
        .and_then(Option::as_ref)
        .and_then(FilesystemHookRuntime::encode_inherited_local_descriptors)
}

pub(super) struct SpawnLocalRetain {
    release: Option<(LocalClient, Vec<String>)>,
}

impl SpawnLocalRetain {
    pub(super) fn commit(mut self) {
        self.release = None;
    }

    pub(super) fn rollback(mut self) -> Result<()> {
        self.release()
    }

    fn release(&mut self) -> Result<()> {
        if self.release.is_none() {
            return Ok(());
        }
        let _guard = FilesystemHookGuard::enter()
            .context("filesystem hook is unavailable while releasing child files")?;
        let (local, handles) = self.release.take().expect("release was checked above");
        local.release_retained(handles)?;
        Ok(())
    }
}

impl Drop for SpawnLocalRetain {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

pub(super) fn retain_local_files_for_spawn(handles: Vec<String>) -> Result<SpawnLocalRetain> {
    if handles.is_empty() {
        return Ok(SpawnLocalRetain { release: None });
    }
    let _guard = FilesystemHookGuard::enter()
        .context("filesystem hook is unavailable while retaining child files")?;
    let runtime = FilesystemHookRuntime::global()
        .context("filesystem hook runtime is unavailable while retaining child files")?;
    let local = runtime
        .local
        .clone()
        .context("local filesystem broker is unavailable while retaining child files")?;
    local.retain(handles.clone())?;
    Ok(SpawnLocalRetain {
        release: Some((local, handles)),
    })
}

pub(super) fn inheritable_internal_descriptors() -> Vec<libc::c_int> {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return Vec::new();
    };
    FILESYSTEM_RUNTIME
        .get()
        .and_then(Option::as_ref)
        .map(FilesystemHookRuntime::inheritable_local_descriptors)
        .unwrap_or_default()
}

fn refresh_descriptor_inheritance(descriptor: libc::c_int) {
    let Some(runtime) = FilesystemHookRuntime::global() else {
        return;
    };
    if let Some(open) = runtime.tracked_open(descriptor) {
        runtime.refresh_local_state_inheritance(&open);
    }
}

pub(super) fn flush_before_exec() -> Result<()> {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return Ok(());
    };
    let Some(runtime) = FILESYSTEM_RUNTIME.get().and_then(Option::as_ref) else {
        return Ok(());
    };
    runtime.flush_memory_mappings()?;
    runtime.commit_all_open_files()
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
pub(super) fn flush_at_exit() {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return;
    };
    unsafe {
        libc::fflush(std::ptr::null_mut());
    }
    if let Some(runtime) = FilesystemHookRuntime::global() {
        let _ = runtime.finish_all_open_files();
    }
}

#[cfg(test)]
fn with_test_runtime<T>(runtime: &FilesystemHookRuntime, operation: impl FnOnce() -> T) -> T {
    struct ResetTestRuntime(*const FilesystemHookRuntime);

    impl Drop for ResetTestRuntime {
        fn drop(&mut self) {
            TEST_FILESYSTEM_RUNTIME.with(|runtime| runtime.set(self.0));
        }
    }

    let previous = TEST_FILESYSTEM_RUNTIME.with(|slot| slot.replace(runtime));
    let _reset = ResetTestRuntime(previous);
    operation()
}

pub(super) fn error_errno(error: &anyhow::Error) -> libc::c_int {
    if let Some(error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<LocalClientError>())
    {
        return error.errno();
    }
    error
        .chain()
        .find_map(|cause| {
            let error = cause.downcast_ref::<io::Error>()?;
            Some(error.raw_os_error().unwrap_or(match error.kind() {
                io::ErrorKind::NotFound => libc::ENOENT,
                io::ErrorKind::PermissionDenied => libc::EACCES,
                io::ErrorKind::AlreadyExists => libc::EEXIST,
                io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => libc::EINVAL,
                io::ErrorKind::Interrupted => libc::EINTR,
                io::ErrorKind::Unsupported => libc::ENOTSUP,
                io::ErrorKind::OutOfMemory => libc::ENOMEM,
                io::ErrorKind::NotADirectory => libc::ENOTDIR,
                io::ErrorKind::IsADirectory => libc::EISDIR,
                io::ErrorKind::DirectoryNotEmpty => libc::ENOTEMPTY,
                _ => libc::EIO,
            }))
        })
        .unwrap_or(libc::EIO)
}

unsafe fn fail<T>(error: &anyhow::Error, value: T) -> T {
    unsafe { set_errno(error_errno(error)) };
    value
}

unsafe fn fail_audit<T>(error: &AuditError, value: T) -> T {
    unsafe { set_errno(error.errno()) };
    value
}

fn catch_filesystem_panic<T: Copy>(failure: T, operation: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(operation)).unwrap_or_else(|_| {
        unsafe { set_errno(libc::EIO) };
        failure
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn configure_descriptor(
    descriptor: libc::c_int,
    flags: libc::c_int,
    virtual_status: bool,
) -> Result<()> {
    let descriptor_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if descriptor_flags < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let descriptor_flags = descriptor_flags | libc::FD_CLOEXEC;
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, descriptor_flags) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let status_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if status_flags < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let requested = if virtual_status {
        0
    } else {
        flags & (libc::O_APPEND | libc::O_NONBLOCK)
    };
    let status_flags = (status_flags & !(libc::O_APPEND | libc::O_NONBLOCK)) | requested;
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, status_flags) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

mod content;
mod data;
mod descriptor;
mod directory;
mod lifecycle;
mod loader;
mod mapping;
mod metadata;
mod namespace;
mod nfs;
mod open;
pub(super) mod socket;
mod unsupported;

#[cfg(not(test))]
use namespace::{
    original_mkdir, original_rename, original_rmdir, original_symlink, original_unlink,
};

#[cfg(test)]
pub(super) use descriptor::*;
#[cfg(test)]
pub(super) use directory::*;
#[cfg(test)]
pub(super) use metadata::*;
#[cfg(test)]
pub(super) use namespace::*;
#[cfg(test)]
pub(super) use open::*;
#[cfg(test)]
pub(super) use unsupported::*;

#[cfg(test)]
mod tests;
