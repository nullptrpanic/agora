use super::super::abi::{darwin_close, darwin_close_nocancel};
use super::*;

type CloseFn = unsafe extern "C" fn(libc::c_int) -> libc::c_int;
type GuardedCloseFn = unsafe extern "C" fn(libc::c_int, *const GuardId) -> libc::c_int;
type FcloseFn = unsafe extern "C" fn(*mut libc::FILE) -> libc::c_int;
type DescriptorFn = unsafe extern "C" fn(libc::c_int) -> libc::c_int;
type Dup2Fn = unsafe extern "C" fn(libc::c_int, libc::c_int) -> libc::c_int;
type TruncateFn = unsafe extern "C" fn(*const libc::c_char, libc::off_t) -> libc::c_int;
type FtruncateFn = unsafe extern "C" fn(libc::c_int, libc::off_t) -> libc::c_int;
pub(super) type FlockFn = unsafe extern "C" fn(libc::c_int, libc::c_int) -> libc::c_int;

unsafe fn sandbox_truncate(path: *const libc::c_char, length: libc::off_t) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_truncate() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(path, length) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(path, length) };
        };
        if length < 0 {
            unsafe { set_errno(libc::EINVAL) };
            return -1;
        }
        let request =
            match runtime.prepare_materialized_open(path, libc::AT_FDCWD, libc::O_WRONLY, 0) {
                Ok(request) => request,
                Err(error) => return unsafe { fail(&error, -1) },
            };
        match request.native_path() {
            Ok(Some(native)) => return unsafe { original(native.as_ptr(), length) },
            Ok(None) => {}
            Err(error) => return unsafe { fail(&error, -1) },
        }
        if let Err(error) = runtime.publish(FileOperation::Open, request.file.clone()) {
            return unsafe { fail_audit(&error, -1) };
        }
        let mut prepared = request.into_prepared();
        if let PreparedOpenFile::Remote(remote) = &mut prepared.prepared {
            let result = match u64::try_from(length) {
                Ok(length) => remote.set_length(length).map(|()| 0),
                Err(_) => Err(io::Error::from_raw_os_error(libc::EINVAL).into()),
            };
            return match result {
                Ok(result) => result,
                Err(error) => unsafe { fail(&error, -1) },
            };
        }
        let result = match prepared.prepared.target_mut() {
            OpenTarget::Path(mapped) => {
                let mapped = match CString::new(mapped.as_os_str().as_bytes()) {
                    Ok(mapped) => mapped,
                    Err(error) => return unsafe { fail(&error.into(), -1) },
                };
                unsafe { original(mapped.as_ptr(), length) }
            }
            OpenTarget::Descriptor(file) => match u64::try_from(length) {
                Ok(length) => file.set_len(length).map(|()| 0).unwrap_or_else(|error| {
                    unsafe { set_errno(error.raw_os_error().unwrap_or(libc::EIO)) };
                    -1
                }),
                Err(_) => {
                    unsafe { set_errno(libc::EINVAL) };
                    -1
                }
            },
        };
        if result != 0 {
            return result;
        }
        match runtime.commit_open(&mut prepared) {
            Ok(()) => 0,
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_truncate(
    path: *const libc::c_char,
    length: libc::off_t,
) -> libc::c_int {
    unsafe { sandbox_truncate(path, length) }
}

#[cfg(test)]
pub(super) unsafe fn sandbox_descriptor_mutation(
    descriptor: libc::c_int,
    operation: impl FnMut(libc::c_int) -> libc::c_int,
) -> libc::c_int {
    unsafe { sandbox_descriptor_mutation_with_truncate(descriptor, None, operation) }
}

unsafe fn sandbox_descriptor_mutation_with_truncate(
    descriptor: libc::c_int,
    truncate: Option<libc::off_t>,
    mut operation: impl FnMut(libc::c_int) -> libc::c_int,
) -> libc::c_int {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return operation(descriptor);
    };
    let Some(runtime) = FilesystemHookRuntime::global() else {
        return operation(descriptor);
    };
    if runtime.native_passthrough_descriptor(descriptor) {
        return operation(descriptor);
    }
    let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(descriptor, &mut status) } != 0 {
        return -1;
    }
    if status.st_mode & libc::S_IFMT != libc::S_IFREG {
        unsafe { set_errno(libc::ENOTSUP) };
        return -1;
    }
    if let Some(open) = runtime.tracked_open(descriptor) {
        if open.layer == FileLayer::Lower {
            unsafe { set_errno(libc::ENOTSUP) };
            return -1;
        }
        if let Some(length) = truncate {
            let content = open.managed();
            let Ok(requested_length) = u64::try_from(length) else {
                unsafe { set_errno(libc::EINVAL) };
                return -1;
            };
            let reservation = u64::try_from(status.st_size)
                .ok()
                .and_then(|current| truncate_reservation(current, length));
            return match unsafe {
                content.truncate(
                    runtime,
                    descriptor,
                    &open,
                    requested_length,
                    reservation,
                    || operation(descriptor),
                )
            } {
                Ok(result) => result,
                Err(error) => unsafe { fail(&error, -1) },
            };
        }
        let result = operation(descriptor);
        if result == 0 {
            let logical = open.logical();
            if let Err(error) =
                runtime.refresh_attributes(descriptor, logical.to_string_lossy().as_ref())
            {
                return unsafe { fail(&error, -1) };
            }
            if let Err(error) = runtime.writeback(descriptor) {
                return unsafe { fail(&error, -1) };
            }
        }
        return result;
    }
    let staged = match runtime.prepare_descriptor_mutation(descriptor) {
        Ok(staged) => staged,
        Err(error) => return unsafe { fail(&error, -1) },
    };
    let result = operation(descriptor);
    if result == 0
        && let Err(error) = runtime.filesystem.commit_write(staged)
    {
        return unsafe { fail(&error, -1) };
    }
    result
}

pub(super) fn truncate_reservation(
    current_length: u64,
    requested_length: libc::off_t,
) -> Option<LocalByteRange> {
    let requested_length = u64::try_from(requested_length).ok()?;
    let start = current_length.min(requested_length);
    let end = current_length.max(requested_length);
    LocalByteRange::new(start, end).ok()
}

unsafe fn sandbox_ftruncate(descriptor: libc::c_int, length: libc::off_t) -> libc::c_int {
    catch_filesystem_panic(-1, || match original_ftruncate() {
        Some(original) => unsafe {
            sandbox_descriptor_mutation_with_truncate(descriptor, Some(length), |descriptor| {
                original(descriptor, length)
            })
        },
        None => unsafe {
            set_errno(libc::ENOSYS);
            -1
        },
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_ftruncate(
    descriptor: libc::c_int,
    length: libc::off_t,
) -> libc::c_int {
    unsafe { sandbox_ftruncate(descriptor, length) }
}

unsafe fn sandbox_close(
    descriptor: libc::c_int,
    operation: impl FnOnce(libc::c_int) -> libc::c_int,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return operation(descriptor);
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return operation(descriptor);
        };
        if let Some(file) = runtime.tracked(descriptor)
            && let Err(error) = runtime.publish(FileOperation::Close, file)
        {
            return unsafe { fail_audit(&error, -1) };
        }
        let tracked = runtime.take_descriptor(descriptor);
        if let Some((open, true)) = &tracked {
            let result = if runtime.has_mapping(open) {
                runtime.commit_open_file(descriptor, open, true)
            } else {
                runtime.finish_open_file(descriptor, open)
            };
            if let Err(error) = result {
                runtime.restore_descriptor(descriptor, Arc::clone(open));
                return unsafe { fail(&error, -1) };
            }
        }
        let result = operation(descriptor);
        if result == 0
            && let Some((open, last_alias)) = &tracked
        {
            release_local_close_locks(open, *last_alias);
        }
        if result != 0
            && let Some((open, _)) = tracked
        {
            runtime.restore_descriptor(descriptor, open);
        } else if result == 0 {
            runtime.unregister_directory(descriptor);
        }
        result
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_close(descriptor: libc::c_int) -> libc::c_int {
    let Some(original) = original_close() else {
        unsafe { set_errno(libc::ENOSYS) };
        return -1;
    };
    unsafe { sandbox_close(descriptor, |descriptor| original(descriptor)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_close_nocancel(descriptor: libc::c_int) -> libc::c_int {
    let Some(original) = original_close_nocancel() else {
        unsafe { set_errno(libc::ENOSYS) };
        return -1;
    };
    unsafe { sandbox_close(descriptor, |descriptor| original(descriptor)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_guarded_close(
    descriptor: libc::c_int,
    guard: *const GuardId,
) -> libc::c_int {
    let Some(original) = original_guarded_close() else {
        unsafe { set_errno(libc::ENOSYS) };
        return -1;
    };
    unsafe { sandbox_close(descriptor, |descriptor| original(descriptor, guard)) }
}

unsafe fn sandbox_fclose(stream: *mut libc::FILE) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_fclose() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(stream) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(stream) };
        };
        let descriptor = if stream.is_null() {
            -1
        } else {
            unsafe { libc::fileno(stream) }
        };
        if let Some(file) = runtime.tracked(descriptor)
            && let Err(error) = runtime.publish(FileOperation::Close, file)
        {
            return unsafe { fail_audit(&error, -1) };
        }
        let flush_result = if descriptor >= 0 {
            unsafe { libc::fflush(stream) }
        } else {
            0
        };
        let flush_errno = (flush_result != 0).then(|| unsafe { *libc::__error() });
        let tracked = runtime.take_descriptor(descriptor);
        if let Some((open, true)) = &tracked {
            let result = if runtime.has_mapping(open) {
                runtime.commit_open_file(descriptor, open, true)
            } else {
                runtime.finish_open_file(descriptor, open)
            };
            if let Err(error) = result {
                runtime.restore_descriptor(descriptor, Arc::clone(open));
                return unsafe { fail(&error, -1) };
            }
        }
        let result = unsafe { original(stream) };
        if let Some((open, last_alias)) = &tracked {
            release_local_close_locks(open, *last_alias);
        }
        if result != 0 {
            return result;
        }
        if let Some(errno) = flush_errno {
            unsafe { set_errno(errno) };
            return -1;
        }
        0
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fclose(stream: *mut libc::FILE) -> libc::c_int {
    unsafe { sandbox_fclose(stream) }
}

unsafe fn sandbox_sync_descriptor(descriptor: libc::c_int, original: DescriptorFn) -> libc::c_int {
    let result = unsafe { original(descriptor) };
    if result != 0 {
        return result;
    }
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return result;
    };
    let Some(runtime) = FilesystemHookRuntime::global() else {
        return result;
    };
    match runtime.writeback(descriptor) {
        Ok(()) => result,
        Err(error) => unsafe { fail(&error, -1) },
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fsync(descriptor: libc::c_int) -> libc::c_int {
    catch_filesystem_panic(-1, || match original_fsync() {
        Some(original) => unsafe { sandbox_sync_descriptor(descriptor, original) },
        None => {
            unsafe { set_errno(libc::ENOSYS) };
            -1
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_commit_synced_descriptor(
    descriptor: libc::c_int,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return 0;
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return 0;
        };
        match runtime.writeback(descriptor) {
            Ok(()) => 0,
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_dup(descriptor: libc::c_int) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_dup() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let result = unsafe { original(descriptor) };
        if result < 0 {
            return result;
        }
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return result;
        };
        if let Some(runtime) = FilesystemHookRuntime::global() {
            runtime.duplicate_descriptor(descriptor, result);
        }
        result
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_dup2(
    source: libc::c_int,
    destination: libc::c_int,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_dup2() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(source, destination) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(source, destination) };
        };
        if source == destination {
            return unsafe { original(source, destination) };
        }
        if let Err(error) = runtime.writeback(destination) {
            return unsafe { fail(&error, -1) };
        }
        let result = unsafe { original(source, destination) };
        if result >= 0 {
            let replaced = runtime.take_descriptor(destination);
            runtime.duplicate_descriptor(source, destination);
            if let Some((open, last_alias)) = &replaced {
                release_local_close_locks(open, *last_alias);
            }
            if let Some((open, true)) = replaced
                && open.managed().is_broker_managed()
                && !runtime.has_mapping(&open)
            {
                let _ = runtime.finish_open_file(-1, &open);
            }
        }
        result
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_track_fcntl_duplicate(
    source: libc::c_int,
    destination: libc::c_int,
) {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return;
    };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(runtime) = FilesystemHookRuntime::global() {
            runtime.duplicate_descriptor(source, destination);
        }
    }));
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_fcntl_setfd_argument(
    descriptor: libc::c_int,
    flags: libc::c_int,
) -> libc::c_int {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return flags;
    };
    catch_unwind(AssertUnwindSafe(|| {
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return flags;
        };
        if runtime
            .tracked_open(descriptor)
            .is_some_and(|open| open.close_on_exec && !open.managed().supports_exec_inheritance())
        {
            flags | libc::FD_CLOEXEC
        } else {
            flags
        }
    }))
    .unwrap_or(flags | libc::FD_CLOEXEC)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_fcntl_commit_setfd(descriptor: libc::c_int) {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return;
    };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        refresh_descriptor_inheritance(descriptor);
    }));
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_fcntl_getfl(
    descriptor: libc::c_int,
    native_flags: libc::c_int,
) -> libc::c_int {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return native_flags;
    };
    match catch_unwind(AssertUnwindSafe(|| -> Result<libc::c_int> {
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return Ok(native_flags);
        };
        runtime
            .tracked_open(descriptor)
            .map(|open| open.managed().merge_status_flags(native_flags))
            .unwrap_or(Ok(native_flags))
    })) {
        Ok(Ok(flags)) => flags,
        Ok(Err(error)) => unsafe { fail(&error, -1) },
        Err(_) => {
            unsafe { set_errno(libc::EIO) };
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_fcntl_setfl_argument(
    descriptor: libc::c_int,
    flags: libc::c_int,
) -> libc::c_int {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return flags;
    };
    catch_unwind(AssertUnwindSafe(|| {
        FilesystemHookRuntime::global()
            .and_then(|runtime| runtime.tracked_open(descriptor))
            .map(|open| open.managed().native_status_flags(flags))
            .unwrap_or(flags)
    }))
    .unwrap_or(flags)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_fcntl_commit_setfl(
    descriptor: libc::c_int,
    flags: libc::c_int,
) -> libc::c_int {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return 0;
    };
    match catch_unwind(AssertUnwindSafe(|| -> Result<()> {
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return Ok(());
        };
        runtime
            .tracked_open(descriptor)
            .map(|open| open.managed().commit_status_flags(flags))
            .unwrap_or(Ok(()))
    })) {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => unsafe { fail(&error, -1) },
        Err(_) => {
            unsafe { set_errno(libc::EIO) };
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_validate_content_fcntl(descriptor: libc::c_int) -> libc::c_int {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return 0;
    };
    catch_unwind(AssertUnwindSafe(|| {
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return 0;
        };
        if runtime
            .tracked_open(descriptor)
            .map(|open| open.managed().is_broker_managed())
            .unwrap_or(false)
        {
            unsafe { set_errno(libc::ENOTSUP) };
            -1
        } else {
            0
        }
    }))
    .unwrap_or_else(|_| {
        unsafe { set_errno(libc::EIO) };
        -1
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_lock_descriptor(descriptor: libc::c_int) -> libc::c_int {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return descriptor;
    };
    catch_unwind(AssertUnwindSafe(|| {
        FilesystemHookRuntime::global()
            .and_then(|runtime| runtime.tracked_open(descriptor))
            .map(|open| open.managed().lock_descriptor(descriptor))
            .unwrap_or(descriptor)
    }))
    .unwrap_or(descriptor)
}

pub(super) unsafe fn sandbox_flock_with(
    descriptor: libc::c_int,
    operation: libc::c_int,
    original: FlockFn,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, operation) };
        };
        let descriptor = FilesystemHookRuntime::global()
            .and_then(|runtime| runtime.tracked_open(descriptor))
            .map(|open| open.managed().lock_descriptor(descriptor))
            .unwrap_or(descriptor);
        drop(guard);
        unsafe { original(descriptor, operation) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_flock(
    descriptor: libc::c_int,
    operation: libc::c_int,
) -> libc::c_int {
    let Some(original) = original_flock() else {
        unsafe { set_errno(libc::ENOSYS) };
        return -1;
    };
    unsafe { sandbox_flock_with(descriptor, operation, original) }
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_fcntl() -> *const libc::c_void {
    INTERPOSE_FCNTL.replacee
}

fn original_truncate() -> Option<TruncateFn> {
    function_from_interpose(&INTERPOSE_TRUNCATE)
}

fn original_ftruncate() -> Option<FtruncateFn> {
    function_from_interpose(&INTERPOSE_FTRUNCATE)
}

fn original_flock() -> Option<FlockFn> {
    function_from_interpose(&INTERPOSE_FLOCK)
}

pub(super) fn release_local_close_locks(open: &OpenFile, last_alias: bool) {
    open.managed().release_close_locks(last_alias);
}

pub(super) fn original_close() -> Option<CloseFn> {
    function_from_interpose(&INTERPOSE_CLOSE)
}

fn original_close_nocancel() -> Option<CloseFn> {
    function_from_interpose(&INTERPOSE_CLOSE_NOCANCEL)
}

fn original_guarded_close() -> Option<GuardedCloseFn> {
    function_from_interpose(&INTERPOSE_GUARDED_CLOSE)
}

pub(super) fn original_fclose() -> Option<FcloseFn> {
    function_from_interpose(&INTERPOSE_FCLOSE)
}

fn original_fsync() -> Option<DescriptorFn> {
    function_from_interpose(&INTERPOSE_FSYNC)
}

fn original_dup() -> Option<DescriptorFn> {
    function_from_interpose(&INTERPOSE_DUP)
}

fn original_dup2() -> Option<Dup2Fn> {
    function_from_interpose(&INTERPOSE_DUP2)
}

unsafe extern "C" {
    pub(super) fn agora_sandbox_fcntl_shim(
        descriptor: libc::c_int,
        command: libc::c_int,
        ...
    ) -> libc::c_int;

    #[link_name = "guarded_close_np"]
    fn system_guarded_close_np(descriptor: libc::c_int, guard: *const GuardId) -> libc::c_int;
}

dyld_interpose!(INTERPOSE_TRUNCATE, agora_sandbox_truncate, libc::truncate);

dyld_interpose!(
    INTERPOSE_FTRUNCATE,
    agora_sandbox_ftruncate,
    libc::ftruncate
);

dyld_interpose!(INTERPOSE_CLOSE, agora_sandbox_close, darwin_close);

dyld_interpose!(
    INTERPOSE_GUARDED_CLOSE,
    agora_sandbox_guarded_close,
    system_guarded_close_np
);

dyld_interpose!(
    INTERPOSE_CLOSE_NOCANCEL,
    agora_sandbox_close_nocancel,
    darwin_close_nocancel
);

dyld_interpose!(INTERPOSE_FCLOSE, agora_sandbox_fclose, libc::fclose);

dyld_interpose!(INTERPOSE_FSYNC, agora_sandbox_fsync, libc::fsync);

dyld_interpose!(INTERPOSE_DUP, agora_sandbox_dup, libc::dup);

dyld_interpose!(INTERPOSE_DUP2, agora_sandbox_dup2, libc::dup2);

dyld_interpose!(INTERPOSE_FCNTL, agora_sandbox_fcntl_shim, libc::fcntl);

dyld_interpose!(INTERPOSE_FLOCK, agora_sandbox_flock, libc::flock);
