#![cfg(target_os = "macos")]

use super::config::{
    self, CHILD_RUNTIME_ENVIRONMENT, HookConfig, INHERITED_LOCAL_DESCRIPTORS,
    REMOTE_CURRENT_DIRECTORY,
};
use super::dyld::{dyld_interpose, function_from_interpose};
use super::set_errno;
use crate::audit::{AuditClient, AuditEventRequest};
use crate::callback::{CommandContext, ProcessContext, ProcessOperation};
use crate::execution::{
    DEFAULT_EXECUTABLE_PATH, PrepareResponse, encode_prepare_request, resolve_shebang,
};
use crate::ipc::InheritedControlStream;
use crate::trace::TraceContext;
use std::cell::Cell;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::net::TcpStream;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const MAX_RECORDED_ARGUMENTS: usize = 256;
const MAX_RECORDED_ARGUMENT_BYTES: usize = 32 * 1024;
const TRUNCATED_ARGUMENTS: &str = "[truncated]";

type PosixSpawnFn = unsafe extern "C" fn(
    *mut libc::pid_t,
    *const libc::c_char,
    *const libc::posix_spawn_file_actions_t,
    *const libc::posix_spawnattr_t,
    *const *mut libc::c_char,
    *const *mut libc::c_char,
) -> libc::c_int;

type ExecveFn = unsafe extern "C" fn(
    *const libc::c_char,
    *const *const libc::c_char,
    *const *const libc::c_char,
) -> libc::c_int;

unsafe extern "C" {
    fn posix_spawn_file_actions_addinherit_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        descriptor: libc::c_int,
    ) -> libc::c_int;
}

thread_local! {
    static INSIDE_PROCESS_HOOK: Cell<bool> = const { Cell::new(false) };
    #[cfg(test)]
    static TEST_PROCESS_RUNTIME: Cell<*const ProcessHookRuntime> = const { Cell::new(std::ptr::null()) };
}

struct ProcessHookGuard {
    _signals: super::SignalMaskGuard,
}

struct SpawnFileActions {
    borrowed: *const libc::posix_spawn_file_actions_t,
    owned: Option<libc::posix_spawn_file_actions_t>,
}

impl SpawnFileActions {
    unsafe fn prepare(
        file_actions: *const libc::posix_spawn_file_actions_t,
        attributes: *const libc::posix_spawnattr_t,
    ) -> Result<Self, libc::c_int> {
        if attributes.is_null() {
            return Ok(Self {
                borrowed: file_actions,
                owned: None,
            });
        }
        let mut flags = 0;
        let result = unsafe { libc::posix_spawnattr_getflags(attributes, &mut flags) };
        if result != 0 {
            return Err(result);
        }
        if i32::from(flags) & libc::POSIX_SPAWN_CLOEXEC_DEFAULT == 0 {
            return Ok(Self {
                borrowed: file_actions,
                owned: None,
            });
        }

        let mut descriptors = super::control::inheritable_descriptors();
        descriptors.extend(super::filesystem::inheritable_internal_descriptors());
        descriptors.sort_unstable();
        descriptors.dedup();
        if descriptors.is_empty() {
            return Ok(Self {
                borrowed: file_actions,
                owned: None,
            });
        }

        let mut prepared = if file_actions.is_null() {
            let mut owned = std::ptr::null_mut();
            let result = unsafe { libc::posix_spawn_file_actions_init(&mut owned) };
            if result != 0 {
                return Err(result);
            }
            Self {
                borrowed: std::ptr::null(),
                owned: Some(owned),
            }
        } else {
            Self {
                borrowed: file_actions,
                owned: None,
            }
        };
        for descriptor in descriptors {
            let result = unsafe {
                posix_spawn_file_actions_addinherit_np(prepared.as_mut_ptr(), descriptor)
            };
            if result != 0 {
                return Err(result);
            }
        }
        Ok(prepared)
    }

    fn as_ptr(&self) -> *const libc::posix_spawn_file_actions_t {
        self.owned
            .as_ref()
            .map_or(self.borrowed, |owned| owned as *const _)
    }

    fn as_mut_ptr(&mut self) -> *mut libc::posix_spawn_file_actions_t {
        self.owned
            .as_mut()
            .map_or(self.borrowed.cast_mut(), |owned| owned as *mut _)
    }
}

impl Drop for SpawnFileActions {
    fn drop(&mut self) {
        if let Some(actions) = self.owned.as_mut() {
            unsafe {
                libc::posix_spawn_file_actions_destroy(actions);
            }
        }
    }
}

impl ProcessHookGuard {
    fn enter() -> Option<Self> {
        Self::enter_when_ready(super::initialized() || test_process_runtime_is_set())
    }

    fn enter_when_ready(ready: bool) -> Option<Self> {
        if !ready {
            return None;
        }
        Self::enter_initialized()
    }

    // Keep Darwin TLV access out of the pre-initialization fast path. This
    // mirrors the filesystem guard because x86_64 may call process symbols
    // while libSystem is still bootstrapping thread-local storage.
    #[inline(never)]
    fn enter_initialized() -> Option<Self> {
        let signals = super::SignalMaskGuard::block_or_abort();
        INSIDE_PROCESS_HOOK.with(|inside| {
            if inside.replace(true) {
                None
            } else {
                Some(Self { _signals: signals })
            }
        })
    }
}

#[cfg(test)]
fn test_process_runtime_is_set() -> bool {
    TEST_PROCESS_RUNTIME.with(|runtime| !runtime.get().is_null())
}

#[cfg(not(test))]
const fn test_process_runtime_is_set() -> bool {
    false
}

impl Drop for ProcessHookGuard {
    fn drop(&mut self) {
        INSIDE_PROCESS_HOOK.with(|inside| inside.set(false));
    }
}

struct ProcessHookRuntime {
    config: HookConfig,
    audit: Option<AuditClient>,
    execution: Option<Arc<InheritedControlStream<TcpStream>>>,
    prefer_shared: AtomicBool,
    observed_pid: AtomicU32,
}

#[derive(Debug)]
struct PreparedExecutable {
    program: CString,
    arguments: Vec<CString>,
}

#[derive(Debug)]
struct PrepareError {
    errno: libc::c_int,
    message: String,
    transport: bool,
}

impl PrepareError {
    fn new(errno: libc::c_int, message: impl Into<String>) -> Self {
        Self {
            errno,
            message: message.into(),
            transport: false,
        }
    }

    fn from_anyhow(error: anyhow::Error, fallback_errno: libc::c_int) -> Self {
        let errno = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<io::Error>())
            .map(io_errno)
            .unwrap_or(fallback_errno);
        Self::new(errno, format!("{error:#}"))
    }
}

impl From<io::Error> for PrepareError {
    fn from(error: io::Error) -> Self {
        Self {
            errno: io_errno(&error),
            message: error.to_string(),
            transport: true,
        }
    }
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PrepareError {}

fn io_errno(error: &io::Error) -> libc::c_int {
    error.raw_os_error().unwrap_or(match error.kind() {
        io::ErrorKind::NotFound => libc::ENOENT,
        io::ErrorKind::PermissionDenied => libc::EACCES,
        io::ErrorKind::InvalidInput => libc::EINVAL,
        io::ErrorKind::InvalidData => libc::EPROTO,
        io::ErrorKind::TimedOut => libc::ETIMEDOUT,
        io::ErrorKind::Unsupported => libc::ENOTSUP,
        _ => libc::EIO,
    })
}

struct ChildArguments {
    values: Vec<CString>,
    pointers: Vec<*mut libc::c_char>,
}

impl ChildArguments {
    unsafe fn new(
        arguments: *const *const libc::c_char,
        prepared: &PreparedExecutable,
    ) -> Option<Self> {
        let mut values = Vec::new();
        let mut current = arguments;
        if !prepared.arguments.is_empty() {
            values.push(prepared.program.clone());
            values.extend(prepared.arguments.iter().cloned());
            if !current.is_null() && !(unsafe { *current }).is_null() {
                current = unsafe { current.add(1) };
            }
        }
        if !current.is_null() {
            while !(unsafe { *current }).is_null() {
                values.push(CString::new(unsafe { CStr::from_ptr(*current) }.to_bytes()).ok()?);
                current = unsafe { current.add(1) };
            }
        }
        let mut pointers = values
            .iter()
            .map(|value| value.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        pointers.push(std::ptr::null_mut());
        Some(Self { values, pointers })
    }

    unsafe fn shell_fallback(arguments: *const *const libc::c_char, script: &CStr) -> Option<Self> {
        let mut values = vec![
            CString::new("sh").ok()?,
            CString::new(script.to_bytes()).ok()?,
        ];
        let mut current = arguments;
        if !current.is_null() && !(unsafe { *current }).is_null() {
            current = unsafe { current.add(1) };
        }
        if !current.is_null() {
            while !(unsafe { *current }).is_null() {
                values.push(CString::new(unsafe { CStr::from_ptr(*current) }.to_bytes()).ok()?);
                current = unsafe { current.add(1) };
            }
        }
        let mut pointers = values
            .iter()
            .map(|value| value.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        pointers.push(std::ptr::null_mut());
        Some(Self { values, pointers })
    }

    fn as_posix_ptr(&self) -> *const *mut libc::c_char {
        debug_assert_eq!(self.values.len() + 1, self.pointers.len());
        self.pointers.as_ptr()
    }

    fn as_exec_ptr(&self) -> *const *const libc::c_char {
        self.as_posix_ptr().cast()
    }
}

struct ChildEnvironment {
    values: Vec<CString>,
    pointers: Vec<*mut libc::c_char>,
}

impl ChildEnvironment {
    unsafe fn new(
        environment: *const *const libc::c_char,
        config: &HookConfig,
        trace: &TraceContext,
        remote_current_directory: Option<&Path>,
    ) -> Option<Self> {
        let mut values = Vec::new();
        if !environment.is_null() {
            let mut current = environment;
            while !(unsafe { *current }).is_null() {
                let value = unsafe { CStr::from_ptr(*current) }.to_bytes();
                if !CHILD_RUNTIME_ENVIRONMENT
                    .iter()
                    .any(|key| Self::has_key(value, key))
                    && !Self::has_key(value, "DYLD_INSERT_LIBRARIES")
                {
                    values.push(CString::new(value).ok()?);
                }
                current = unsafe { current.add(1) };
            }
        }
        for (key, value) in config.child_environment_for(trace) {
            let mut entry = Vec::with_capacity(key.len() + 1 + value.len());
            entry.extend_from_slice(key.as_bytes());
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            values.push(CString::new(entry).ok()?);
        }
        for (key, value) in super::control::child_environment() {
            values.push(CString::new(format!("{key}={value}")).ok()?);
        }
        if let Some(descriptors) = super::filesystem::inherited_local_descriptors() {
            values.push(CString::new(format!("{INHERITED_LOCAL_DESCRIPTORS}={descriptors}")).ok()?);
        }
        if let Some(directory) = remote_current_directory {
            let mut entry = Vec::with_capacity(
                REMOTE_CURRENT_DIRECTORY.len() + 1 + directory.as_os_str().as_bytes().len(),
            );
            entry.extend_from_slice(REMOTE_CURRENT_DIRECTORY.as_bytes());
            entry.push(b'=');
            entry.extend_from_slice(directory.as_os_str().as_bytes());
            values.push(CString::new(entry).ok()?);
        }
        values
            .push(CString::new(format!("DYLD_INSERT_LIBRARIES={}", config.hook_libraries())).ok()?);
        let mut pointers = values
            .iter()
            .map(|value| value.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        pointers.push(std::ptr::null_mut());
        Some(Self { values, pointers })
    }

    fn as_posix_ptr(&self) -> *const *mut libc::c_char {
        debug_assert_eq!(self.values.len() + 1, self.pointers.len());
        self.pointers.as_ptr()
    }

    fn as_exec_ptr(&self) -> *const *const libc::c_char {
        self.as_posix_ptr().cast()
    }

    fn has_key(value: &[u8], key: &str) -> bool {
        value
            .strip_prefix(key.as_bytes())
            .is_some_and(|suffix| suffix.starts_with(b"="))
    }
}

impl ProcessHookRuntime {
    fn global() -> Option<&'static Self> {
        #[cfg(test)]
        {
            let runtime = TEST_PROCESS_RUNTIME.with(Cell::get);
            if !runtime.is_null() {
                return Some(unsafe { &*runtime });
            }
        }
        static RUNTIME: OnceLock<Option<ProcessHookRuntime>> = OnceLock::new();
        RUNTIME
            .get_or_init(|| {
                config::global().cloned().map(|config| {
                    let execution = super::control::execution();
                    Self {
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
                        prefer_shared: AtomicBool::new(execution.is_some()),
                        observed_pid: AtomicU32::new(std::process::id()),
                        execution,
                        config,
                    }
                })
            })
            .as_ref()
    }

    fn prepare(&self, executable: &Path) -> Result<CString, PrepareError> {
        let request = encode_prepare_request(self.config.execution_token(), executable)
            .map_err(|error| PrepareError::new(io_errno(&error), error.to_string()))?;
        let current_pid = std::process::id();
        if self.observed_pid.swap(current_pid, Ordering::AcqRel) != current_pid
            && self.execution.is_some()
        {
            self.prefer_shared.store(true, Ordering::Release);
        }
        let response = if self.prefer_shared.load(Ordering::Acquire) {
            match self.prepare_shared(&request) {
                Some(Ok(response)) => response,
                Some(Err(error)) if error.transport => {
                    self.prefer_shared.store(false, Ordering::Release);
                    self.prepare_fresh(&request)?
                }
                Some(Err(error)) => return Err(error),
                None => {
                    self.prefer_shared.store(false, Ordering::Release);
                    self.prepare_fresh(&request)?
                }
            }
        } else {
            match self.prepare_fresh(&request) {
                Err(error) if error.transport => {
                    let Some(response) = self.prepare_shared(&request) else {
                        return Err(error);
                    };
                    let response = response?;
                    self.prefer_shared.store(true, Ordering::Release);
                    response
                }
                result => result?,
            }
        };
        match response {
            PrepareResponse::Accepted => Err(PrepareError::new(
                libc::EPROTO,
                "execution preparation returned a handshake response",
            )),
            PrepareResponse::Ready(path) => {
                CString::new(path.as_os_str().as_bytes()).map_err(|_| {
                    PrepareError::new(libc::EINVAL, "prepared executable path contains NUL")
                })
            }
            PrepareResponse::Error { errno, message } => Err(PrepareError::new(errno, message)),
        }
    }

    fn prepare_fresh(&self, request: &[u8]) -> Result<PrepareResponse, PrepareError> {
        let mut stream = TcpStream::connect(self.config.execution_control())?;
        let timeout = Some(Duration::from_secs(30));
        stream.set_read_timeout(timeout)?;
        stream.set_write_timeout(timeout)?;
        super::control::execution_request(&mut stream, request).map_err(PrepareError::from)
    }

    fn prepare_shared(&self, request: &[u8]) -> Option<Result<PrepareResponse, PrepareError>> {
        let shared = self.execution.as_ref()?;
        Some(
            shared
                .transact(|stream| super::control::execution_request(stream, request))
                .map_err(PrepareError::from)
                .and_then(|response| response.map_err(PrepareError::from)),
        )
    }

    fn prepare_executable(&self, executable: &Path) -> Result<PreparedExecutable, PrepareError> {
        let program = self.prepare(executable)?;
        let script = Path::new(OsStr::from_bytes(program.to_bytes()));
        let Some(shebang) = resolve_shebang(script)
            .map_err(|error| PrepareError::from_anyhow(error, libc::ENOEXEC))?
        else {
            return Ok(PreparedExecutable {
                program,
                arguments: Vec::new(),
            });
        };
        let interpreter = self.prepare(&shebang.interpreter)?;
        let mut arguments = Vec::with_capacity(2);
        if let Some(argument) = shebang.argument {
            arguments.push(
                CString::new(argument.as_bytes()).map_err(|_| {
                    PrepareError::new(libc::EINVAL, "shebang argument contains NUL")
                })?,
            );
        }
        arguments.push(
            CString::new(program.to_bytes())
                .map_err(|_| PrepareError::new(libc::EINVAL, "script path contains NUL"))?,
        );
        Ok(PreparedExecutable {
            program: interpreter,
            arguments,
        })
    }

    fn publish(&self, event: AuditEventRequest) -> Result<(), PrepareError> {
        let Some(audit) = &self.audit else {
            return Ok(());
        };
        audit
            .publish(event)
            .map_err(|error| PrepareError::new(error.errno(), error.to_string()))
    }
}

#[cfg(test)]
fn with_test_runtime<T>(runtime: &ProcessHookRuntime, operation: impl FnOnce() -> T) -> T {
    struct Reset(*const ProcessHookRuntime);

    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_PROCESS_RUNTIME.with(|current| current.set(self.0));
        }
    }

    let previous = TEST_PROCESS_RUNTIME.with(|current| current.replace(runtime));
    let _reset = Reset(previous);
    operation()
}

unsafe fn requested_executable(
    path: *const libc::c_char,
    search_path: bool,
) -> Result<PathBuf, PrepareError> {
    if path.is_null() {
        return Err(PrepareError::new(
            libc::EFAULT,
            "requested executable could not be resolved",
        ));
    }
    let path = OsStr::from_bytes(unsafe { CStr::from_ptr(path) }.to_bytes());
    if !search_path || path.as_bytes().contains(&b'/') {
        let path = Path::new(path);
        return Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(PrepareError::from)?
                .join(path)
        });
    }
    let search = std::env::var_os("PATH").unwrap_or_else(|| DEFAULT_EXECUTABLE_PATH.into());
    let current = std::env::current_dir()
        .or_else(|error| std::env::var_os("PWD").map(PathBuf::from).ok_or(error))
        .map_err(PrepareError::from)?;
    search_path_executable(path, &search, &current)
}

fn search_path_executable(
    path: &OsStr,
    search: &OsStr,
    current: &Path,
) -> Result<PathBuf, PrepareError> {
    let mut denied = false;
    for directory in std::env::split_paths(search) {
        let directory = if directory.as_os_str().is_empty() {
            current.to_path_buf()
        } else if directory.is_absolute() {
            directory
        } else {
            current.join(directory)
        };
        let candidate = directory.join(path);
        let metadata = match candidate.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => {
                denied = true;
                continue;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                denied = true;
                continue;
            }
            Err(_) => continue,
        };
        if metadata.permissions().mode() & 0o111 == 0 {
            denied = true;
            continue;
        }
        let candidate_c = match CString::new(candidate.as_os_str().as_bytes()) {
            Ok(candidate) => candidate,
            Err(_) => continue,
        };
        if unsafe { libc::access(candidate_c.as_ptr(), libc::X_OK) } == 0 {
            return Ok(candidate);
        }
        if unsafe { *libc::__error() } == libc::EACCES {
            denied = true;
        }
    }
    Err(PrepareError::new(
        if denied { libc::EACCES } else { libc::ENOENT },
        "requested executable could not be resolved through PATH",
    ))
}

unsafe fn prepared_executable(
    path: *const libc::c_char,
    search_path: bool,
    arguments: *const *const libc::c_char,
    operation: ProcessOperation,
) -> Result<(PreparedExecutable, TraceContext), PrepareError> {
    let runtime = ProcessHookRuntime::global()
        .ok_or_else(|| PrepareError::new(libc::EACCES, "sandbox process runtime is unavailable"))?;
    let executable = unsafe { requested_executable(path, search_path) }?;
    let trace = runtime.config.trace().child();
    let event = unsafe { process_event_request(&executable, arguments, operation, &trace) }?;
    runtime.publish(event)?;
    let prepared = runtime.prepare_executable(&executable)?;
    Ok((prepared, trace))
}

unsafe fn process_event_request(
    executable: &Path,
    arguments: *const *const libc::c_char,
    operation: ProcessOperation,
    trace: &TraceContext,
) -> Result<AuditEventRequest, PrepareError> {
    let mut values = Vec::new();
    let mut recorded_bytes = 0_usize;
    if !arguments.is_null() {
        let mut current = arguments;
        while values.len() < MAX_RECORDED_ARGUMENTS && !(unsafe { *current }).is_null() {
            let value = unsafe { CStr::from_ptr(*current) }.to_string_lossy();
            if recorded_bytes.saturating_add(value.len()) > MAX_RECORDED_ARGUMENT_BYTES {
                values.push(TRUNCATED_ARGUMENTS.to_string());
                break;
            }
            recorded_bytes += value.len();
            values.push(value.into_owned());
            current = unsafe { current.add(1) };
        }
        if values.len() == MAX_RECORDED_ARGUMENTS && !(unsafe { *current }).is_null() {
            values.push(TRUNCATED_ARGUMENTS.to_string());
        }
    }
    let process_executable = super::try_current_process_executable().map_err(|error| {
        PrepareError::new(
            io_errno(&error),
            format!("failed to resolve current executable: {error}"),
        )
    })?;
    let current_dir = resolve_current_directory(
        super::filesystem::tracked_current_directory(),
        std::env::current_dir,
        std::env::var_os("PWD"),
    );
    let current_dir = current_dir.map_err(|error| {
        PrepareError::new(
            io_errno(&error),
            format!("failed to resolve current directory: {error}"),
        )
    })?;
    Ok(AuditEventRequest::Process {
        trace_id: trace.encode(),
        process: ProcessContext {
            pid: std::process::id(),
            ppid: unsafe { libc::getppid() as u32 },
            executable: process_executable,
        },
        command: CommandContext {
            executable: executable.to_string_lossy().into_owned(),
            arguments: values,
            current_dir: current_dir.to_string_lossy().into_owned(),
            operation,
        },
    })
}

fn resolve_current_directory(
    tracked: Option<PathBuf>,
    native: impl FnOnce() -> io::Result<PathBuf>,
    pwd: Option<OsString>,
) -> io::Result<PathBuf> {
    tracked
        .map(Ok)
        .unwrap_or_else(native)
        .or_else(|error| pwd.map(PathBuf::from).ok_or(error))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_posix_spawn(
    pid: *mut libc::pid_t,
    path: *const libc::c_char,
    file_actions: *const libc::posix_spawn_file_actions_t,
    attributes: *const libc::posix_spawnattr_t,
    arguments: *const *mut libc::c_char,
    environment: *const *mut libc::c_char,
) -> libc::c_int {
    let Some(original) = original_posix_spawn() else {
        return libc::ENOSYS;
    };
    let Some(guard) = ProcessHookGuard::enter() else {
        return libc::EACCES;
    };
    let remote_current_directory = match super::filesystem::prepare_child_current_directory() {
        Ok(directory) => directory,
        Err(error) => return PrepareError::from_anyhow(error, libc::EIO).errno,
    };
    let (prepared, trace) = match unsafe {
        prepared_executable(
            path,
            false,
            arguments.cast::<*const libc::c_char>(),
            ProcessOperation::PosixSpawn,
        )
    } {
        Ok(prepared) => prepared,
        Err(error) => return error.errno,
    };
    let Some(runtime) = ProcessHookRuntime::global() else {
        return libc::EACCES;
    };
    let Some(environment) = (unsafe {
        ChildEnvironment::new(
            environment.cast::<*const libc::c_char>(),
            &runtime.config,
            &trace,
            remote_current_directory.as_deref(),
        )
    }) else {
        return libc::EACCES;
    };
    let Some(arguments) =
        (unsafe { ChildArguments::new(arguments.cast::<*const libc::c_char>(), &prepared) })
    else {
        return libc::EACCES;
    };
    let file_actions = match unsafe { SpawnFileActions::prepare(file_actions, attributes) } {
        Ok(file_actions) => file_actions,
        Err(error) => return error,
    };
    drop(guard);
    unsafe {
        original(
            pid,
            prepared.program.as_ptr(),
            file_actions.as_ptr(),
            attributes,
            arguments.as_posix_ptr(),
            environment.as_posix_ptr(),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_posix_spawnp(
    pid: *mut libc::pid_t,
    file: *const libc::c_char,
    file_actions: *const libc::posix_spawn_file_actions_t,
    attributes: *const libc::posix_spawnattr_t,
    arguments: *const *mut libc::c_char,
    environment: *const *mut libc::c_char,
) -> libc::c_int {
    let Some(original) = original_posix_spawn() else {
        return libc::ENOSYS;
    };
    let Some(guard) = ProcessHookGuard::enter() else {
        return libc::EACCES;
    };
    let remote_current_directory = match super::filesystem::prepare_child_current_directory() {
        Ok(directory) => directory,
        Err(error) => return PrepareError::from_anyhow(error, libc::EIO).errno,
    };
    let (prepared, trace) = match unsafe {
        prepared_executable(
            file,
            true,
            arguments.cast::<*const libc::c_char>(),
            ProcessOperation::PosixSpawnp,
        )
    } {
        Ok(prepared) => prepared,
        Err(error) => return error.errno,
    };
    let Some(runtime) = ProcessHookRuntime::global() else {
        return libc::EACCES;
    };
    let Some(environment) = (unsafe {
        ChildEnvironment::new(
            environment.cast::<*const libc::c_char>(),
            &runtime.config,
            &trace,
            remote_current_directory.as_deref(),
        )
    }) else {
        return libc::EACCES;
    };
    let Some(arguments) =
        (unsafe { ChildArguments::new(arguments.cast::<*const libc::c_char>(), &prepared) })
    else {
        return libc::EACCES;
    };
    let file_actions = match unsafe { SpawnFileActions::prepare(file_actions, attributes) } {
        Ok(file_actions) => file_actions,
        Err(error) => return error,
    };
    drop(guard);
    unsafe {
        original(
            pid,
            prepared.program.as_ptr(),
            file_actions.as_ptr(),
            attributes,
            arguments.as_posix_ptr(),
            environment.as_posix_ptr(),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_execve(
    path: *const libc::c_char,
    arguments: *const *const libc::c_char,
    environment: *const *const libc::c_char,
) -> libc::c_int {
    unsafe {
        execute(
            path,
            false,
            arguments,
            environment,
            ProcessOperation::Execve,
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_execv(
    path: *const libc::c_char,
    arguments: *const *const libc::c_char,
) -> libc::c_int {
    unsafe {
        execute(
            path,
            false,
            arguments,
            current_environment(),
            ProcessOperation::Execv,
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_execvp(
    file: *const libc::c_char,
    arguments: *const *const libc::c_char,
) -> libc::c_int {
    unsafe {
        execute(
            file,
            true,
            arguments,
            current_environment(),
            ProcessOperation::Execvp,
        )
    }
}

unsafe fn execute(
    path: *const libc::c_char,
    search_path: bool,
    arguments: *const *const libc::c_char,
    environment: *const *const libc::c_char,
    operation: ProcessOperation,
) -> libc::c_int {
    let Some(original) = original_execve() else {
        unsafe { set_errno(libc::ENOSYS) };
        return -1;
    };
    let Some(guard) = ProcessHookGuard::enter() else {
        unsafe { set_errno(libc::EACCES) };
        return -1;
    };
    let remote_current_directory = match super::filesystem::prepare_child_current_directory() {
        Ok(directory) => directory,
        Err(error) => {
            let error = PrepareError::from_anyhow(error, libc::EIO);
            unsafe { set_errno(error.errno) };
            return -1;
        }
    };
    let (prepared, trace) =
        match unsafe { prepared_executable(path, search_path, arguments, operation) } {
            Ok(prepared) => prepared,
            Err(error) => {
                unsafe { set_errno(error.errno) };
                return -1;
            }
        };
    let Some(runtime) = ProcessHookRuntime::global() else {
        unsafe { set_errno(libc::EACCES) };
        return -1;
    };
    let Some(environment) = (unsafe {
        ChildEnvironment::new(
            environment,
            &runtime.config,
            &trace,
            remote_current_directory.as_deref(),
        )
    }) else {
        unsafe { set_errno(libc::EACCES) };
        return -1;
    };
    let Some(child_arguments) = (unsafe { ChildArguments::new(arguments, &prepared) }) else {
        unsafe { set_errno(libc::EACCES) };
        return -1;
    };
    if let Err(error) = super::filesystem::flush_before_exec() {
        let error = PrepareError::from_anyhow(error, libc::EIO);
        unsafe { set_errno(error.errno) };
        return -1;
    }
    drop(guard);
    let result = unsafe {
        original(
            prepared.program.as_ptr(),
            child_arguments.as_exec_ptr(),
            environment.as_exec_ptr(),
        )
    };
    if !search_path || result != -1 || unsafe { *libc::__error() } != libc::ENOEXEC {
        return result;
    }
    let Some(fallback_guard) = ProcessHookGuard::enter() else {
        unsafe { set_errno(libc::EACCES) };
        return -1;
    };
    let shell = match runtime.prepare_executable(Path::new("/bin/sh")) {
        Ok(shell) => shell,
        Err(error) => {
            unsafe { set_errno(error.errno) };
            return -1;
        }
    };
    let Some(arguments) =
        (unsafe { ChildArguments::shell_fallback(arguments, prepared.program.as_c_str()) })
    else {
        unsafe { set_errno(libc::EACCES) };
        return -1;
    };
    drop(fallback_guard);
    unsafe {
        original(
            shell.program.as_ptr(),
            arguments.as_exec_ptr(),
            environment.as_exec_ptr(),
        )
    }
}

unsafe fn current_environment() -> *const *const libc::c_char {
    let environment = unsafe { libc::_NSGetEnviron() };
    if environment.is_null() {
        std::ptr::null()
    } else {
        unsafe { *environment }.cast()
    }
}

fn original_posix_spawn() -> Option<PosixSpawnFn> {
    function_from_interpose(&INTERPOSE_POSIX_SPAWN)
}

fn original_execve() -> Option<ExecveFn> {
    function_from_interpose(&INTERPOSE_EXECVE)
}

dyld_interpose!(
    INTERPOSE_POSIX_SPAWN,
    agora_sandbox_posix_spawn,
    libc::posix_spawn
);
dyld_interpose!(
    INTERPOSE_POSIX_SPAWNP,
    agora_sandbox_posix_spawnp,
    libc::posix_spawnp
);
dyld_interpose!(INTERPOSE_EXECVE, agora_sandbox_execve, libc::execve);
dyld_interpose!(INTERPOSE_EXECV, agora_sandbox_execv, libc::execv);
dyld_interpose!(INTERPOSE_EXECVP, agora_sandbox_execvp, libc::execvp);

#[cfg(test)]
mod tests;
