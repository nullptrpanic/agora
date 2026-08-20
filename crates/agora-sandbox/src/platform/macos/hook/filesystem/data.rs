use super::super::abi::{darwin_mach_task_self, darwin_mach_vm_read_overwrite};
use super::content::{ContentIoOffset, managed_read_io, managed_seek_io, managed_write_io};
#[cfg(test)]
use super::content::{READ_AHEAD_MAX_BYTES, read_materialization_length};
use super::*;

const MAX_VECTOR_COUNT: usize = 1024;

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_data_descriptor_requires_hook(
    descriptor: libc::c_int,
) -> libc::c_int {
    let _signals = super::super::SignalMaskGuard::block_or_abort();
    let Some(runtime) = FilesystemHookRuntime::global() else {
        return 0;
    };
    libc::c_int::from(
        runtime
            .memory_index
            .data_descriptor_state(descriptor)
            .unwrap_or(true),
    )
}

type ReadFn = unsafe extern "C" fn(libc::c_int, *mut libc::c_void, usize) -> libc::ssize_t;
type PreadFn =
    unsafe extern "C" fn(libc::c_int, *mut libc::c_void, usize, libc::off_t) -> libc::ssize_t;
type ReadvFn = unsafe extern "C" fn(libc::c_int, *const libc::iovec, libc::c_int) -> libc::ssize_t;
type PreadvFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::iovec,
    libc::c_int,
    libc::off_t,
) -> libc::ssize_t;
type WriteFn = unsafe extern "C" fn(libc::c_int, *const libc::c_void, usize) -> libc::ssize_t;
type PwriteFn =
    unsafe extern "C" fn(libc::c_int, *const libc::c_void, usize, libc::off_t) -> libc::ssize_t;
type WritevFn = unsafe extern "C" fn(libc::c_int, *const libc::iovec, libc::c_int) -> libc::ssize_t;
type PwritevFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::iovec,
    libc::c_int,
    libc::off_t,
) -> libc::ssize_t;
type LseekFn = unsafe extern "C" fn(libc::c_int, libc::off_t, libc::c_int) -> libc::off_t;
type SendfileFn = unsafe extern "C" fn(
    libc::c_int,
    libc::c_int,
    libc::off_t,
    *mut libc::off_t,
    *mut libc::sf_hdtr,
    libc::c_int,
) -> libc::c_int;
type FcopyfileFn = unsafe extern "C" fn(
    libc::c_int,
    libc::c_int,
    libc::copyfile_state_t,
    libc::copyfile_flags_t,
) -> libc::c_int;
type AioFn = unsafe extern "C" fn(*mut libc::aiocb) -> libc::c_int;
type LioListioFn = unsafe extern "C" fn(
    libc::c_int,
    *const *mut libc::aiocb,
    libc::c_int,
    *mut libc::sigevent,
) -> libc::c_int;
type GuardedWriteFn =
    unsafe extern "C" fn(libc::c_int, *const GuardId, *const libc::c_void, usize) -> libc::ssize_t;
type GuardedPwriteFn = unsafe extern "C" fn(
    libc::c_int,
    *const GuardId,
    *const libc::c_void,
    usize,
    libc::off_t,
) -> libc::ssize_t;
type GuardedWritevFn = unsafe extern "C" fn(
    libc::c_int,
    *const GuardId,
    *const libc::iovec,
    libc::c_int,
) -> libc::ssize_t;

unsafe fn sandbox_read_with(
    descriptor: libc::c_int,
    buffer: *mut libc::c_void,
    length: usize,
    original: Option<ReadFn>,
    positioned: Option<PreadFn>,
) -> libc::ssize_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, buffer, length) };
        };
        match unsafe {
            managed_read_io(
                descriptor,
                ContentIoOffset::Sequential,
                || Ok(length),
                |payload, available| {
                    positioned
                        .map(|pread| pread(payload, buffer, available, 0))
                        .unwrap_or_else(|| {
                            set_errno(libc::ENOSYS);
                            -1
                        })
                },
                |offset| {
                    positioned
                        .map(|pread| pread(descriptor, buffer, length, offset))
                        .unwrap_or_else(|| {
                            set_errno(libc::ENOSYS);
                            -1
                        })
                },
                || original(descriptor, buffer, length),
            )
        } {
            Some(result) => result,
            None => unsafe { original(descriptor, buffer, length) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_read(
    descriptor: libc::c_int,
    buffer: *mut libc::c_void,
    length: usize,
) -> libc::ssize_t {
    unsafe {
        sandbox_read_with(
            descriptor,
            buffer,
            length,
            original_read(),
            original_pread(),
        )
    }
}

unsafe fn sandbox_pread_with(
    descriptor: libc::c_int,
    buffer: *mut libc::c_void,
    length: usize,
    offset: libc::off_t,
    original: Option<PreadFn>,
) -> libc::ssize_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, buffer, length, offset) };
        };
        match unsafe {
            managed_read_io(
                descriptor,
                ContentIoOffset::Positioned(offset),
                || Ok(length),
                |payload, available| original(payload, buffer, available, 0),
                |offset| original(descriptor, buffer, length, offset),
                || original(descriptor, buffer, length, offset),
            )
        } {
            Some(result) => result,
            None => unsafe { original(descriptor, buffer, length, offset) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_pread(
    descriptor: libc::c_int,
    buffer: *mut libc::c_void,
    length: usize,
    offset: libc::off_t,
) -> libc::ssize_t {
    unsafe { sandbox_pread_with(descriptor, buffer, length, offset, original_pread()) }
}

unsafe fn sandbox_readv_with(
    descriptor: libc::c_int,
    vectors: *const libc::iovec,
    count: libc::c_int,
    original: Option<ReadvFn>,
    positioned: Option<PreadvFn>,
) -> libc::ssize_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, vectors, count) };
        };
        match unsafe {
            managed_read_io(
                descriptor,
                ContentIoOffset::Sequential,
                || vector_read_length(vectors, count),
                |payload, _available| {
                    positioned
                        .map(|preadv| preadv(payload, vectors, count, 0))
                        .unwrap_or_else(|| {
                            set_errno(libc::ENOSYS);
                            -1
                        })
                },
                |offset| {
                    positioned
                        .map(|preadv| preadv(descriptor, vectors, count, offset))
                        .unwrap_or_else(|| {
                            set_errno(libc::ENOSYS);
                            -1
                        })
                },
                || original(descriptor, vectors, count),
            )
        } {
            Some(result) => result,
            None => unsafe { original(descriptor, vectors, count) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_readv(
    descriptor: libc::c_int,
    vectors: *const libc::iovec,
    count: libc::c_int,
) -> libc::ssize_t {
    unsafe {
        sandbox_readv_with(
            descriptor,
            vectors,
            count,
            original_readv(),
            original_preadv(),
        )
    }
}

unsafe fn sandbox_preadv_with(
    descriptor: libc::c_int,
    vectors: *const libc::iovec,
    count: libc::c_int,
    offset: libc::off_t,
    original: Option<PreadvFn>,
) -> libc::ssize_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, vectors, count, offset) };
        };
        match unsafe {
            managed_read_io(
                descriptor,
                ContentIoOffset::Positioned(offset),
                || vector_read_length(vectors, count),
                |payload, _available| original(payload, vectors, count, 0),
                |offset| original(descriptor, vectors, count, offset),
                || original(descriptor, vectors, count, offset),
            )
        } {
            Some(result) => result,
            None => unsafe { original(descriptor, vectors, count, offset) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_preadv(
    descriptor: libc::c_int,
    vectors: *const libc::iovec,
    count: libc::c_int,
    offset: libc::off_t,
) -> libc::ssize_t {
    unsafe { sandbox_preadv_with(descriptor, vectors, count, offset, original_preadv()) }
}

unsafe fn sandbox_write_with(
    descriptor: libc::c_int,
    buffer: *const libc::c_void,
    length: usize,
    original: Option<WriteFn>,
    positioned: Option<PwriteFn>,
) -> libc::ssize_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, buffer, length) };
        };
        match unsafe {
            managed_write_io(
                descriptor,
                ContentIoOffset::Sequential,
                Some(length),
                || Ok(length),
                |payload, bounded| {
                    positioned
                        .map(|pwrite| pwrite(payload, buffer, bounded, 0))
                        .unwrap_or_else(|| {
                            set_errno(libc::ENOSYS);
                            -1
                        })
                },
                |offset| {
                    positioned
                        .map(|pwrite| pwrite(descriptor, buffer, length, offset))
                        .unwrap_or_else(|| {
                            set_errno(libc::ENOSYS);
                            -1
                        })
                },
                || original(descriptor, buffer, length),
            )
        } {
            Some(result) => result,
            None => unsafe { original(descriptor, buffer, length) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_write(
    descriptor: libc::c_int,
    buffer: *const libc::c_void,
    length: usize,
) -> libc::ssize_t {
    unsafe {
        sandbox_write_with(
            descriptor,
            buffer,
            length,
            original_write(),
            original_pwrite(),
        )
    }
}

unsafe fn sandbox_pwrite_with(
    descriptor: libc::c_int,
    buffer: *const libc::c_void,
    length: usize,
    offset: libc::off_t,
    original: Option<PwriteFn>,
) -> libc::ssize_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, buffer, length, offset) };
        };
        match unsafe {
            managed_write_io(
                descriptor,
                ContentIoOffset::Positioned(offset),
                Some(length),
                || Ok(length),
                |payload, bounded| original(payload, buffer, bounded, 0),
                |offset| original(descriptor, buffer, length, offset),
                || original(descriptor, buffer, length, offset),
            )
        } {
            Some(result) => result,
            None => unsafe { original(descriptor, buffer, length, offset) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_pwrite(
    descriptor: libc::c_int,
    buffer: *const libc::c_void,
    length: usize,
    offset: libc::off_t,
) -> libc::ssize_t {
    unsafe { sandbox_pwrite_with(descriptor, buffer, length, offset, original_pwrite()) }
}

unsafe fn sandbox_writev_with(
    descriptor: libc::c_int,
    vectors: *const libc::iovec,
    count: libc::c_int,
    original: Option<WritevFn>,
    positioned: Option<PwritevFn>,
) -> libc::ssize_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, vectors, count) };
        };
        match unsafe {
            managed_write_io(
                descriptor,
                ContentIoOffset::Sequential,
                (count == 0).then_some(0),
                || vector_read_length(vectors, count),
                |payload, bounded| {
                    let copied = match bounded_process_vectors(vectors, count, bounded) {
                        Ok(copied) => copied,
                        Err(errno) => {
                            set_errno(errno);
                            return -1;
                        }
                    };
                    positioned
                        .map(|pwritev| {
                            pwritev(payload, copied.as_ptr(), copied.len() as libc::c_int, 0)
                        })
                        .unwrap_or_else(|| {
                            set_errno(libc::ENOSYS);
                            -1
                        })
                },
                |offset| {
                    positioned
                        .map(|pwritev| pwritev(descriptor, vectors, count, offset))
                        .unwrap_or_else(|| {
                            set_errno(libc::ENOSYS);
                            -1
                        })
                },
                || original(descriptor, vectors, count),
            )
        } {
            Some(result) => result,
            None => unsafe { original(descriptor, vectors, count) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_writev(
    descriptor: libc::c_int,
    vectors: *const libc::iovec,
    count: libc::c_int,
) -> libc::ssize_t {
    unsafe {
        sandbox_writev_with(
            descriptor,
            vectors,
            count,
            original_writev(),
            original_pwritev(),
        )
    }
}

unsafe fn sandbox_pwritev_with(
    descriptor: libc::c_int,
    vectors: *const libc::iovec,
    count: libc::c_int,
    offset: libc::off_t,
    original: Option<PwritevFn>,
) -> libc::ssize_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, vectors, count, offset) };
        };
        match unsafe {
            managed_write_io(
                descriptor,
                ContentIoOffset::Positioned(offset),
                Some(vector_write_length(count)),
                || vector_read_length(vectors, count),
                |payload, bounded| {
                    let copied = match bounded_process_vectors(vectors, count, bounded) {
                        Ok(copied) => copied,
                        Err(errno) => {
                            set_errno(errno);
                            return -1;
                        }
                    };
                    original(payload, copied.as_ptr(), copied.len() as libc::c_int, 0)
                },
                |offset| original(descriptor, vectors, count, offset),
                || original(descriptor, vectors, count, offset),
            )
        } {
            Some(result) => result,
            None => unsafe { original(descriptor, vectors, count, offset) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_pwritev(
    descriptor: libc::c_int,
    vectors: *const libc::iovec,
    count: libc::c_int,
    offset: libc::off_t,
) -> libc::ssize_t {
    unsafe { sandbox_pwritev_with(descriptor, vectors, count, offset, original_pwritev()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_guarded_write(
    descriptor: libc::c_int,
    guard: *const GuardId,
    buffer: *const libc::c_void,
    length: usize,
) -> libc::ssize_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_guarded_write() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_hook_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, guard, buffer, length) };
        };
        if let Some(runtime) = FilesystemHookRuntime::global()
            && let Err(error) = prepare_native_snapshot_descriptor(runtime, descriptor)
        {
            return unsafe { fail(&error, -1) };
        }
        match unsafe {
            managed_write_io(
                descriptor,
                ContentIoOffset::Sequential,
                Some(length),
                || Ok(length),
                |_, _| unreachable!("remote guarded writes use a materialized snapshot"),
                |offset| {
                    original_guarded_pwrite()
                        .map(|pwrite| pwrite(descriptor, guard, buffer, length, offset))
                        .unwrap_or_else(|| {
                            set_errno(libc::ENOSYS);
                            -1
                        })
                },
                || original(descriptor, guard, buffer, length),
            )
        } {
            Some(result) => result,
            None => unsafe { original(descriptor, guard, buffer, length) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_guarded_pwrite(
    descriptor: libc::c_int,
    guard: *const GuardId,
    buffer: *const libc::c_void,
    length: usize,
    offset: libc::off_t,
) -> libc::ssize_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_guarded_pwrite() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_hook_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, guard, buffer, length, offset) };
        };
        if let Some(runtime) = FilesystemHookRuntime::global()
            && let Err(error) = prepare_native_snapshot_descriptor(runtime, descriptor)
        {
            return unsafe { fail(&error, -1) };
        }
        match unsafe {
            managed_write_io(
                descriptor,
                ContentIoOffset::Positioned(offset),
                Some(length),
                || Ok(length),
                |_, _| unreachable!("remote guarded writes use a materialized snapshot"),
                |_| original(descriptor, guard, buffer, length, offset),
                || original(descriptor, guard, buffer, length, offset),
            )
        } {
            Some(result) => result,
            None => unsafe { original(descriptor, guard, buffer, length, offset) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_guarded_writev(
    descriptor: libc::c_int,
    guard: *const GuardId,
    vectors: *const libc::iovec,
    count: libc::c_int,
) -> libc::ssize_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_guarded_writev() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_hook_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, guard, vectors, count) };
        };
        if let Some(runtime) = FilesystemHookRuntime::global()
            && let Err(error) = prepare_native_snapshot_descriptor(runtime, descriptor)
        {
            return unsafe { fail(&error, -1) };
        }
        if count < 0 {
            unsafe { set_errno(libc::EINVAL) };
            return -1;
        }
        match unsafe {
            managed_write_io(
                descriptor,
                ContentIoOffset::Sequential,
                (count == 0).then_some(0),
                || vector_read_length(vectors, count),
                |_, _| unreachable!("remote guarded writes use a materialized snapshot"),
                |offset| guarded_writev_at(descriptor, guard, vectors, count, offset),
                || original(descriptor, guard, vectors, count),
            )
        } {
            Some(result) => result,
            None => unsafe { original(descriptor, guard, vectors, count) },
        }
    })
}

unsafe fn sandbox_lseek(
    descriptor: libc::c_int,
    offset: libc::off_t,
    whence: libc::c_int,
) -> libc::off_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_lseek() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, offset, whence) };
        };
        match unsafe {
            managed_seek_io(descriptor, offset, whence, || {
                original(descriptor, offset, whence)
            })
        } {
            Some(result) => result,
            None => unsafe { original(descriptor, offset, whence) },
        }
    })
}

unsafe fn vector_read_length(
    vectors: *const libc::iovec,
    count: libc::c_int,
) -> std::result::Result<usize, libc::c_int> {
    let count = usize::try_from(count).map_err(|_| libc::EINVAL)?;
    if count > MAX_VECTOR_COUNT {
        return Err(libc::EINVAL);
    }
    if count == 0 {
        return Ok(0);
    }
    let copied_vectors = unsafe { copy_process_slice(vectors, count) }?;
    copied_vectors
        .into_iter()
        .try_fold(0_usize, |total, vector| {
            total
                .checked_add(vector.iov_len)
                .filter(|total| *total <= libc::ssize_t::MAX as usize)
                .ok_or(libc::EINVAL)
        })
}

unsafe fn bounded_process_vectors(
    vectors: *const libc::iovec,
    count: libc::c_int,
    maximum: usize,
) -> std::result::Result<Vec<libc::iovec>, libc::c_int> {
    let count = usize::try_from(count).map_err(|_| libc::EINVAL)?;
    if count > MAX_VECTOR_COUNT {
        return Err(libc::EINVAL);
    }
    let mut vectors = unsafe { copy_process_slice(vectors, count) }?;
    let mut remaining = maximum;
    let mut retained = 0;
    for vector in &mut vectors {
        if remaining == 0 {
            break;
        }
        vector.iov_len = vector.iov_len.min(remaining);
        remaining -= vector.iov_len;
        retained += 1;
    }
    vectors.truncate(retained);
    Ok(vectors)
}

unsafe fn copy_process_value<T>(pointer: *const T) -> std::result::Result<T, libc::c_int> {
    let bytes = unsafe { copy_process_bytes(pointer.cast(), std::mem::size_of::<T>()) }?;
    Ok(unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) })
}

unsafe fn copy_process_slice<T>(
    pointer: *const T,
    count: usize,
) -> std::result::Result<Vec<T>, libc::c_int> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let item_size = std::mem::size_of::<T>();
    let size = count.checked_mul(item_size).ok_or(libc::EINVAL)?;
    let bytes = unsafe { copy_process_bytes(pointer.cast(), size) }?;
    Ok((0..count)
        .map(|index| unsafe {
            std::ptr::read_unaligned(bytes.as_ptr().add(index * item_size).cast())
        })
        .collect())
}

unsafe fn copy_process_bytes(
    pointer: *const libc::c_void,
    size: usize,
) -> std::result::Result<Vec<u8>, libc::c_int> {
    if pointer.is_null() {
        return Err(libc::EFAULT);
    }
    let mut bytes = vec![0_u8; size];
    let mut copied = 0_u64;
    let status = unsafe {
        darwin_mach_vm_read_overwrite(
            darwin_mach_task_self,
            pointer as u64,
            size as u64,
            bytes.as_mut_ptr() as u64,
            &mut copied,
        )
    };
    if status != 0 || copied != size as u64 {
        return Err(libc::EFAULT);
    }
    Ok(bytes)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_lseek(
    descriptor: libc::c_int,
    offset: libc::off_t,
    whence: libc::c_int,
) -> libc::off_t {
    unsafe { sandbox_lseek(descriptor, offset, whence) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_sendfile(
    descriptor: libc::c_int,
    socket: libc::c_int,
    offset: libc::off_t,
    length: *mut libc::off_t,
    headers: *mut libc::sf_hdtr,
    flags: libc::c_int,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_sendfile() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, socket, offset, length, headers, flags) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            drop(guard);
            return unsafe { original(descriptor, socket, offset, length, headers, flags) };
        };
        let Some(open) = runtime.tracked_open(descriptor) else {
            drop(guard);
            return unsafe { original(descriptor, socket, offset, length, headers, flags) };
        };
        if offset < 0 {
            drop(guard);
            return unsafe { original(descriptor, socket, offset, length, headers, flags) };
        }
        let range = match unsafe { sendfile_materialization_range(offset, length, headers) } {
            Ok(range) => range,
            Err(errno) => {
                unsafe { set_errno(errno) };
                return -1;
            }
        };
        if let Err(error) = open.managed().materialize(runtime, range) {
            return unsafe { fail(&error, -1) };
        }
        drop(guard);
        unsafe { original(descriptor, socket, offset, length, headers, flags) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fcopyfile(
    source: libc::c_int,
    destination: libc::c_int,
    state: libc::copyfile_state_t,
    flags: libc::copyfile_flags_t,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_fcopyfile() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(source, destination, state, flags) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            drop(guard);
            return unsafe { original(source, destination, state, flags) };
        };
        if let Err(error) = prepare_native_snapshot_descriptor(runtime, destination) {
            return unsafe { fail(&error, -1) };
        }
        if runtime
            .tracked_open(destination)
            .map(|open| open.managed().accepts_opaque_copy())
            == Some(false)
        {
            unsafe { set_errno(libc::ENOTSUP) };
            return -1;
        }
        if let Some(open) = runtime.tracked_open(source)
            && let Err(error) = open.managed().materialize(runtime, None)
        {
            return unsafe { fail(&error, -1) };
        }
        // Record a conservative full-file candidate before the native call.
        // A signal handler may leave the call through `siglongjmp`, so there is
        // no reliable post-call point at which to observe the resulting size.
        record_snapshot_write_descriptor(
            runtime,
            destination,
            LocalByteRange::new(0, u64::MAX).ok(),
        );
        drop(guard);
        unsafe { original(source, destination, state, flags) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_aio_read(control: *mut libc::aiocb) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_aio_read() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(control) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            drop(guard);
            return unsafe { original(control) };
        };
        let copied = match unsafe { copy_process_value(control.cast_const()) } {
            Ok(copied) => copied,
            Err(errno) => {
                unsafe { set_errno(errno) };
                return -1;
            }
        };
        if let Some(range) = aio_read_range(&copied)
            && let Err(error) = materialize_descriptor(runtime, copied.aio_fildes, Some(range))
        {
            return unsafe { fail(&error, -1) };
        }
        drop(guard);
        unsafe { original(control) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_aio_write(control: *mut libc::aiocb) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_aio_write() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(control) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            drop(guard);
            return unsafe { original(control) };
        };
        let copied = match unsafe { copy_process_value(control.cast_const()) } {
            Ok(copied) => copied,
            Err(errno) => {
                unsafe { set_errno(errno) };
                return -1;
            }
        };
        if let Err(error) = prepare_async_write_descriptor(runtime, copied.aio_fildes) {
            return unsafe { fail(&error, -1) };
        }
        record_snapshot_write_descriptor(runtime, copied.aio_fildes, aio_read_range(&copied));
        drop(guard);
        unsafe { original(control) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_lio_listio(
    mode: libc::c_int,
    controls: *const *mut libc::aiocb,
    count: libc::c_int,
    event: *mut libc::sigevent,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_lio_listio() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(mode, controls, count, event) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            drop(guard);
            return unsafe { original(mode, controls, count, event) };
        };
        let copied = match usize::try_from(count) {
            Ok(count) if count <= MAX_VECTOR_COUNT => unsafe {
                copy_process_slice(controls, count)
            },
            _ => Err(libc::EINVAL),
        };
        let copied = match copied {
            Ok(copied) => copied,
            Err(errno) => {
                unsafe { set_errno(errno) };
                return -1;
            }
        };
        for control in copied.into_iter().filter(|control| !control.is_null()) {
            let control = match unsafe { copy_process_value(control.cast_const()) } {
                Ok(control) => control,
                Err(errno) => {
                    unsafe { set_errno(errno) };
                    return -1;
                }
            };
            let materialized = match control.aio_lio_opcode {
                libc::LIO_READ => aio_read_range(&control)
                    .map(|range| materialize_descriptor(runtime, control.aio_fildes, Some(range)))
                    .transpose(),
                libc::LIO_WRITE => {
                    let materialized = prepare_async_write_descriptor(runtime, control.aio_fildes);
                    if materialized.is_ok() {
                        record_snapshot_write_descriptor(
                            runtime,
                            control.aio_fildes,
                            aio_read_range(&control),
                        );
                    }
                    Some(materialized).transpose()
                }
                _ => Ok(None),
            };
            if let Err(error) = materialized {
                return unsafe { fail(&error, -1) };
            }
        }
        drop(guard);
        unsafe { original(mode, controls, count, event) }
    })
}

fn materialize_descriptor(
    runtime: &FilesystemHookRuntime,
    descriptor: libc::c_int,
    range: Option<LocalByteRange>,
) -> Result<()> {
    let Some(open) = runtime.tracked_open(descriptor) else {
        return Ok(());
    };
    open.managed().materialize(runtime, range)
}

fn prepare_native_snapshot_descriptor(
    runtime: &FilesystemHookRuntime,
    descriptor: libc::c_int,
) -> Result<()> {
    let Some(open) = runtime.tracked_open(descriptor) else {
        return Ok(());
    };
    open.managed().prepare_native_snapshot_if_needed(runtime)
}

fn prepare_async_write_descriptor(
    runtime: &FilesystemHookRuntime,
    descriptor: libc::c_int,
) -> Result<()> {
    let Some(open) = runtime.tracked_open(descriptor) else {
        return Ok(());
    };
    if !open.managed().supports_async_write() {
        return Err(std::io::Error::from_raw_os_error(libc::ENOTSUP).into());
    }
    open.managed().prepare_native_snapshot_if_needed(runtime)
}

fn record_snapshot_write_descriptor(
    runtime: &FilesystemHookRuntime,
    descriptor: libc::c_int,
    range: Option<LocalByteRange>,
) {
    let Some(open) = runtime.tracked_open(descriptor) else {
        return;
    };
    open.managed().record_snapshot_write(range);
}

fn aio_read_range(control: &libc::aiocb) -> Option<LocalByteRange> {
    let start = u64::try_from(control.aio_offset).ok()?;
    let length = u64::try_from(control.aio_nbytes).unwrap_or(u64::MAX);
    LocalByteRange::new(start, start.saturating_add(length)).ok()
}

unsafe fn sendfile_materialization_range(
    offset: libc::off_t,
    length: *const libc::off_t,
    headers: *const libc::sf_hdtr,
) -> std::result::Result<Option<LocalByteRange>, libc::c_int> {
    if !headers.is_null() {
        let headers = unsafe { copy_process_value(headers) }?;
        if headers.hdr_cnt != 0 || headers.trl_cnt != 0 {
            return Ok(None);
        }
    }
    let requested = unsafe { copy_process_value(length) }?;
    let start = u64::try_from(offset).map_err(|_| libc::EINVAL)?;
    let end = if requested == 0 {
        u64::MAX
    } else {
        let requested = u64::try_from(requested).map_err(|_| libc::EINVAL)?;
        start.checked_add(requested).ok_or(libc::EOVERFLOW)?
    };
    LocalByteRange::new(start, end)
        .map(Some)
        .map_err(|_| libc::EINVAL)
}

unsafe fn guarded_writev_at(
    descriptor: libc::c_int,
    guard: *const GuardId,
    vectors: *const libc::iovec,
    count: libc::c_int,
    offset: libc::off_t,
) -> libc::ssize_t {
    let Some(writev) = original_guarded_writev() else {
        unsafe { set_errno(libc::ENOSYS) };
        return -1;
    };
    if count == 0 {
        return unsafe { writev(descriptor, guard, vectors, count) };
    }
    let Some(lseek) = original_lseek() else {
        unsafe { set_errno(libc::ENOSYS) };
        return -1;
    };
    if unsafe { lseek(descriptor, offset, libc::SEEK_SET) } < 0 {
        return -1;
    }
    unsafe { writev(descriptor, guard, vectors, count) }
}

fn vector_write_length(count: libc::c_int) -> usize {
    if count > 0 { usize::MAX } else { 0 }
}

fn original_read() -> Option<ReadFn> {
    function_from_interpose(&INTERPOSE_READ)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_read() -> *const libc::c_void {
    INTERPOSE_READ.replacee
}

fn original_pread() -> Option<PreadFn> {
    function_from_interpose(&INTERPOSE_PREAD)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_pread() -> *const libc::c_void {
    INTERPOSE_PREAD.replacee
}

fn original_readv() -> Option<ReadvFn> {
    function_from_interpose(&INTERPOSE_READV)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_readv() -> *const libc::c_void {
    INTERPOSE_READV.replacee
}

fn original_preadv() -> Option<PreadvFn> {
    function_from_interpose(&INTERPOSE_PREADV)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_preadv() -> *const libc::c_void {
    INTERPOSE_PREADV.replacee
}

fn original_write() -> Option<WriteFn> {
    function_from_interpose(&INTERPOSE_WRITE)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_write() -> *const libc::c_void {
    INTERPOSE_WRITE.replacee
}

fn original_pwrite() -> Option<PwriteFn> {
    function_from_interpose(&INTERPOSE_PWRITE)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_pwrite() -> *const libc::c_void {
    INTERPOSE_PWRITE.replacee
}

fn original_writev() -> Option<WritevFn> {
    function_from_interpose(&INTERPOSE_WRITEV)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_writev() -> *const libc::c_void {
    INTERPOSE_WRITEV.replacee
}

fn original_pwritev() -> Option<PwritevFn> {
    function_from_interpose(&INTERPOSE_PWRITEV)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_pwritev() -> *const libc::c_void {
    INTERPOSE_PWRITEV.replacee
}

fn original_lseek() -> Option<LseekFn> {
    function_from_interpose(&INTERPOSE_LSEEK)
}

fn original_sendfile() -> Option<SendfileFn> {
    function_from_interpose(&INTERPOSE_SENDFILE)
}

fn original_fcopyfile() -> Option<FcopyfileFn> {
    function_from_interpose(&INTERPOSE_FCOPYFILE)
}

fn original_aio_read() -> Option<AioFn> {
    function_from_interpose(&INTERPOSE_AIO_READ)
}

fn original_aio_write() -> Option<AioFn> {
    function_from_interpose(&INTERPOSE_AIO_WRITE)
}

fn original_lio_listio() -> Option<LioListioFn> {
    function_from_interpose(&INTERPOSE_LIO_LISTIO)
}

fn original_read_nocancel() -> Option<ReadFn> {
    function_from_interpose(&INTERPOSE_READ_NOCANCEL)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_read_nocancel() -> *const libc::c_void {
    INTERPOSE_READ_NOCANCEL.replacee
}

fn original_pread_nocancel() -> Option<PreadFn> {
    function_from_interpose(&INTERPOSE_PREAD_NOCANCEL)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_pread_nocancel() -> *const libc::c_void {
    INTERPOSE_PREAD_NOCANCEL.replacee
}

fn original_readv_nocancel() -> Option<ReadvFn> {
    function_from_interpose(&INTERPOSE_READV_NOCANCEL)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_readv_nocancel() -> *const libc::c_void {
    INTERPOSE_READV_NOCANCEL.replacee
}

fn original_preadv_nocancel() -> Option<PreadvFn> {
    function_from_interpose(&INTERPOSE_PREADV_NOCANCEL)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_preadv_nocancel() -> *const libc::c_void {
    INTERPOSE_PREADV_NOCANCEL.replacee
}

fn original_write_nocancel() -> Option<WriteFn> {
    function_from_interpose(&INTERPOSE_WRITE_NOCANCEL)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_write_nocancel() -> *const libc::c_void {
    INTERPOSE_WRITE_NOCANCEL.replacee
}

fn original_pwrite_nocancel() -> Option<PwriteFn> {
    function_from_interpose(&INTERPOSE_PWRITE_NOCANCEL)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_pwrite_nocancel() -> *const libc::c_void {
    INTERPOSE_PWRITE_NOCANCEL.replacee
}

fn original_writev_nocancel() -> Option<WritevFn> {
    function_from_interpose(&INTERPOSE_WRITEV_NOCANCEL)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_writev_nocancel() -> *const libc::c_void {
    INTERPOSE_WRITEV_NOCANCEL.replacee
}

fn original_pwritev_nocancel() -> Option<PwritevFn> {
    function_from_interpose(&INTERPOSE_PWRITEV_NOCANCEL)
}

#[unsafe(no_mangle)]
pub extern "C" fn agora_sandbox_original_pwritev_nocancel() -> *const libc::c_void {
    INTERPOSE_PWRITEV_NOCANCEL.replacee
}

fn original_guarded_write() -> Option<GuardedWriteFn> {
    function_from_interpose(&INTERPOSE_GUARDED_WRITE)
}

fn original_guarded_pwrite() -> Option<GuardedPwriteFn> {
    function_from_interpose(&INTERPOSE_GUARDED_PWRITE)
}

fn original_guarded_writev() -> Option<GuardedWritevFn> {
    function_from_interpose(&INTERPOSE_GUARDED_WRITEV)
}

unsafe extern "C" {
    fn agora_sandbox_read_shim(
        descriptor: libc::c_int,
        buffer: *mut libc::c_void,
        length: usize,
    ) -> libc::ssize_t;

    fn agora_sandbox_readv_shim(
        descriptor: libc::c_int,
        vectors: *const libc::iovec,
        count: libc::c_int,
    ) -> libc::ssize_t;

    fn agora_sandbox_pread_shim(
        descriptor: libc::c_int,
        buffer: *mut libc::c_void,
        length: usize,
        offset: libc::off_t,
    ) -> libc::ssize_t;

    fn agora_sandbox_preadv_shim(
        descriptor: libc::c_int,
        vectors: *const libc::iovec,
        count: libc::c_int,
        offset: libc::off_t,
    ) -> libc::ssize_t;

    fn agora_sandbox_write_shim(
        descriptor: libc::c_int,
        buffer: *const libc::c_void,
        length: usize,
    ) -> libc::ssize_t;

    fn agora_sandbox_pwrite_shim(
        descriptor: libc::c_int,
        buffer: *const libc::c_void,
        length: usize,
        offset: libc::off_t,
    ) -> libc::ssize_t;

    fn agora_sandbox_writev_shim(
        descriptor: libc::c_int,
        vectors: *const libc::iovec,
        count: libc::c_int,
    ) -> libc::ssize_t;

    fn agora_sandbox_pwritev_shim(
        descriptor: libc::c_int,
        vectors: *const libc::iovec,
        count: libc::c_int,
        offset: libc::off_t,
    ) -> libc::ssize_t;

    fn agora_sandbox_read_nocancel_shim(
        descriptor: libc::c_int,
        buffer: *mut libc::c_void,
        length: usize,
    ) -> libc::ssize_t;

    fn agora_sandbox_readv_nocancel_shim(
        descriptor: libc::c_int,
        vectors: *const libc::iovec,
        count: libc::c_int,
    ) -> libc::ssize_t;

    fn agora_sandbox_pread_nocancel_shim(
        descriptor: libc::c_int,
        buffer: *mut libc::c_void,
        length: usize,
        offset: libc::off_t,
    ) -> libc::ssize_t;

    fn agora_sandbox_preadv_nocancel_shim(
        descriptor: libc::c_int,
        vectors: *const libc::iovec,
        count: libc::c_int,
        offset: libc::off_t,
    ) -> libc::ssize_t;

    fn agora_sandbox_write_nocancel_shim(
        descriptor: libc::c_int,
        buffer: *const libc::c_void,
        length: usize,
    ) -> libc::ssize_t;

    fn agora_sandbox_pwrite_nocancel_shim(
        descriptor: libc::c_int,
        buffer: *const libc::c_void,
        length: usize,
        offset: libc::off_t,
    ) -> libc::ssize_t;

    fn agora_sandbox_writev_nocancel_shim(
        descriptor: libc::c_int,
        vectors: *const libc::iovec,
        count: libc::c_int,
    ) -> libc::ssize_t;

    fn agora_sandbox_pwritev_nocancel_shim(
        descriptor: libc::c_int,
        vectors: *const libc::iovec,
        count: libc::c_int,
        offset: libc::off_t,
    ) -> libc::ssize_t;
}

dyld_interpose!(INTERPOSE_READ, agora_sandbox_read_shim, libc::read);
dyld_interpose!(INTERPOSE_PREAD, agora_sandbox_pread_shim, libc::pread);
dyld_interpose!(INTERPOSE_READV, agora_sandbox_readv_shim, libc::readv);
dyld_interpose!(INTERPOSE_PREADV, agora_sandbox_preadv_shim, libc::preadv);
dyld_interpose!(INTERPOSE_WRITE, agora_sandbox_write_shim, libc::write);
dyld_interpose!(INTERPOSE_PWRITE, agora_sandbox_pwrite_shim, libc::pwrite);
dyld_interpose!(INTERPOSE_WRITEV, agora_sandbox_writev_shim, libc::writev);
dyld_interpose!(INTERPOSE_PWRITEV, agora_sandbox_pwritev_shim, libc::pwritev);
dyld_interpose!(INTERPOSE_LSEEK, agora_sandbox_lseek, libc::lseek);
dyld_interpose!(INTERPOSE_SENDFILE, agora_sandbox_sendfile, libc::sendfile);
dyld_interpose!(
    INTERPOSE_FCOPYFILE,
    agora_sandbox_fcopyfile,
    libc::fcopyfile
);
dyld_interpose!(INTERPOSE_AIO_READ, agora_sandbox_aio_read, libc::aio_read);
dyld_interpose!(
    INTERPOSE_AIO_WRITE,
    agora_sandbox_aio_write,
    libc::aio_write
);
dyld_interpose!(
    INTERPOSE_LIO_LISTIO,
    agora_sandbox_lio_listio,
    libc::lio_listio
);

unsafe extern "C" {
    #[link_name = "guarded_write_np"]
    fn system_guarded_write_np(
        descriptor: libc::c_int,
        guard: *const GuardId,
        buffer: *const libc::c_void,
        length: usize,
    ) -> libc::ssize_t;

    #[link_name = "guarded_pwrite_np"]
    fn system_guarded_pwrite_np(
        descriptor: libc::c_int,
        guard: *const GuardId,
        buffer: *const libc::c_void,
        length: usize,
        offset: libc::off_t,
    ) -> libc::ssize_t;

    #[link_name = "guarded_writev_np"]
    fn system_guarded_writev_np(
        descriptor: libc::c_int,
        guard: *const GuardId,
        vectors: *const libc::iovec,
        count: libc::c_int,
    ) -> libc::ssize_t;
}

dyld_interpose!(
    INTERPOSE_GUARDED_WRITE,
    agora_sandbox_guarded_write,
    system_guarded_write_np
);

dyld_interpose!(
    INTERPOSE_GUARDED_PWRITE,
    agora_sandbox_guarded_pwrite,
    system_guarded_pwrite_np
);

dyld_interpose!(
    INTERPOSE_GUARDED_WRITEV,
    agora_sandbox_guarded_writev,
    system_guarded_writev_np
);

unsafe extern "C" {
    #[link_name = "read$NOCANCEL"]
    fn read_nocancel(
        descriptor: libc::c_int,
        buffer: *mut libc::c_void,
        length: usize,
    ) -> libc::ssize_t;
    #[link_name = "pread$NOCANCEL"]
    fn pread_nocancel(
        descriptor: libc::c_int,
        buffer: *mut libc::c_void,
        length: usize,
        offset: libc::off_t,
    ) -> libc::ssize_t;
    #[link_name = "readv$NOCANCEL"]
    fn readv_nocancel(
        descriptor: libc::c_int,
        vectors: *const libc::iovec,
        count: libc::c_int,
    ) -> libc::ssize_t;
    #[link_name = "preadv$NOCANCEL"]
    fn preadv_nocancel(
        descriptor: libc::c_int,
        vectors: *const libc::iovec,
        count: libc::c_int,
        offset: libc::off_t,
    ) -> libc::ssize_t;
    #[link_name = "write$NOCANCEL"]
    fn write_nocancel(
        descriptor: libc::c_int,
        buffer: *const libc::c_void,
        length: usize,
    ) -> libc::ssize_t;
    #[link_name = "pwrite$NOCANCEL"]
    fn pwrite_nocancel(
        descriptor: libc::c_int,
        buffer: *const libc::c_void,
        length: usize,
        offset: libc::off_t,
    ) -> libc::ssize_t;
    #[link_name = "writev$NOCANCEL"]
    fn writev_nocancel(
        descriptor: libc::c_int,
        vectors: *const libc::iovec,
        count: libc::c_int,
    ) -> libc::ssize_t;
    #[link_name = "pwritev$NOCANCEL"]
    fn pwritev_nocancel(
        descriptor: libc::c_int,
        vectors: *const libc::iovec,
        count: libc::c_int,
        offset: libc::off_t,
    ) -> libc::ssize_t;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_read_nocancel(
    descriptor: libc::c_int,
    buffer: *mut libc::c_void,
    length: usize,
) -> libc::ssize_t {
    unsafe {
        sandbox_read_with(
            descriptor,
            buffer,
            length,
            original_read_nocancel(),
            original_pread_nocancel(),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_pread_nocancel(
    descriptor: libc::c_int,
    buffer: *mut libc::c_void,
    length: usize,
    offset: libc::off_t,
) -> libc::ssize_t {
    unsafe {
        sandbox_pread_with(
            descriptor,
            buffer,
            length,
            offset,
            original_pread_nocancel(),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_readv_nocancel(
    descriptor: libc::c_int,
    vectors: *const libc::iovec,
    count: libc::c_int,
) -> libc::ssize_t {
    unsafe {
        sandbox_readv_with(
            descriptor,
            vectors,
            count,
            original_readv_nocancel(),
            original_preadv_nocancel(),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_preadv_nocancel(
    descriptor: libc::c_int,
    vectors: *const libc::iovec,
    count: libc::c_int,
    offset: libc::off_t,
) -> libc::ssize_t {
    unsafe {
        sandbox_preadv_with(
            descriptor,
            vectors,
            count,
            offset,
            original_preadv_nocancel(),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_write_nocancel(
    descriptor: libc::c_int,
    buffer: *const libc::c_void,
    length: usize,
) -> libc::ssize_t {
    unsafe {
        sandbox_write_with(
            descriptor,
            buffer,
            length,
            original_write_nocancel(),
            original_pwrite_nocancel(),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_pwrite_nocancel(
    descriptor: libc::c_int,
    buffer: *const libc::c_void,
    length: usize,
    offset: libc::off_t,
) -> libc::ssize_t {
    unsafe {
        sandbox_pwrite_with(
            descriptor,
            buffer,
            length,
            offset,
            original_pwrite_nocancel(),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_writev_nocancel(
    descriptor: libc::c_int,
    vectors: *const libc::iovec,
    count: libc::c_int,
) -> libc::ssize_t {
    unsafe {
        sandbox_writev_with(
            descriptor,
            vectors,
            count,
            original_writev_nocancel(),
            original_pwritev_nocancel(),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_pwritev_nocancel(
    descriptor: libc::c_int,
    vectors: *const libc::iovec,
    count: libc::c_int,
    offset: libc::off_t,
) -> libc::ssize_t {
    unsafe {
        sandbox_pwritev_with(
            descriptor,
            vectors,
            count,
            offset,
            original_pwritev_nocancel(),
        )
    }
}

dyld_interpose!(
    INTERPOSE_READ_NOCANCEL,
    agora_sandbox_read_nocancel_shim,
    read_nocancel
);
dyld_interpose!(
    INTERPOSE_PREAD_NOCANCEL,
    agora_sandbox_pread_nocancel_shim,
    pread_nocancel
);
dyld_interpose!(
    INTERPOSE_READV_NOCANCEL,
    agora_sandbox_readv_nocancel_shim,
    readv_nocancel
);
dyld_interpose!(
    INTERPOSE_PREADV_NOCANCEL,
    agora_sandbox_preadv_nocancel_shim,
    preadv_nocancel
);

dyld_interpose!(
    INTERPOSE_WRITE_NOCANCEL,
    agora_sandbox_write_nocancel_shim,
    write_nocancel
);
dyld_interpose!(
    INTERPOSE_PWRITE_NOCANCEL,
    agora_sandbox_pwrite_nocancel_shim,
    pwrite_nocancel
);
dyld_interpose!(
    INTERPOSE_WRITEV_NOCANCEL,
    agora_sandbox_writev_nocancel_shim,
    writev_nocancel
);
dyld_interpose!(
    INTERPOSE_PWRITEV_NOCANCEL,
    agora_sandbox_pwritev_nocancel_shim,
    pwritev_nocancel
);

#[cfg(test)]
mod tests;
