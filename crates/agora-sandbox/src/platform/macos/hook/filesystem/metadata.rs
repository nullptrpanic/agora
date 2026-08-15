use super::*;

type StatFn = unsafe extern "C" fn(*const libc::c_char, *mut libc::stat) -> libc::c_int;
type FstatFn = unsafe extern "C" fn(libc::c_int, *mut libc::stat) -> libc::c_int;
type FstatAtFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::c_char,
    *mut libc::stat,
    libc::c_int,
) -> libc::c_int;
type AccessFn = unsafe extern "C" fn(*const libc::c_char, libc::c_int) -> libc::c_int;
type FaccessAtFn =
    unsafe extern "C" fn(libc::c_int, *const libc::c_char, libc::c_int, libc::c_int) -> libc::c_int;
type ReadlinkFn =
    unsafe extern "C" fn(*const libc::c_char, *mut libc::c_char, libc::size_t) -> libc::ssize_t;
type ReadlinkAtFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::c_char,
    *mut libc::c_char,
    libc::size_t,
) -> libc::ssize_t;
type ChmodFn = unsafe extern "C" fn(*const libc::c_char, libc::mode_t) -> libc::c_int;
type FchmodFn = unsafe extern "C" fn(libc::c_int, libc::mode_t) -> libc::c_int;
type FchmodAtFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::c_char,
    libc::mode_t,
    libc::c_int,
) -> libc::c_int;

unsafe fn sandbox_chmod(path: *const libc::c_char, mode: libc::mode_t) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_chmod() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(path, mode) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(path, mode) };
        };
        match unsafe { runtime.native_passthrough_c_path(path, libc::AT_FDCWD) } {
            Ok(Some(native)) => return unsafe { original(native.as_ptr(), mode) },
            Ok(None) => {}
            Err(error) => return unsafe { fail(&error, -1) },
        }
        match runtime.chmod(path, libc::AT_FDCWD, mode, true) {
            Ok(()) => 0,
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_chmod(
    path: *const libc::c_char,
    mode: libc::mode_t,
) -> libc::c_int {
    unsafe { sandbox_chmod(path, mode) }
}

unsafe fn sandbox_fchmod(descriptor: libc::c_int, mode: libc::mode_t) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_fchmod() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, mode) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(descriptor, mode) };
        };
        if runtime.native_passthrough_descriptor(descriptor) {
            return unsafe { original(descriptor, mode) };
        }
        let Some(open) = runtime.tracked_open(descriptor) else {
            unsafe { set_errno(libc::EPERM) };
            return -1;
        };
        if open.manages_metadata() {
            unsafe { set_errno(libc::ENOTSUP) };
            return -1;
        }
        match runtime.filesystem.chmod_authorized(
            &open.logical(),
            mode.into(),
            false,
            &Credentials::effective(),
        ) {
            Ok(()) => 0,
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fchmod(
    descriptor: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    unsafe { sandbox_fchmod(descriptor, mode) }
}

unsafe fn sandbox_fchmodat(
    directory: libc::c_int,
    path: *const libc::c_char,
    mode: libc::mode_t,
    flags: libc::c_int,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_fchmodat() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(directory, path, mode, flags) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(directory, path, mode, flags) };
        };
        match unsafe { runtime.native_passthrough_c_path(path, directory) } {
            Ok(Some(native)) => {
                return unsafe { original(libc::AT_FDCWD, native.as_ptr(), mode, flags) };
            }
            Ok(None) => {}
            Err(error) => return unsafe { fail(&error, -1) },
        }
        if flags & !libc::AT_SYMLINK_NOFOLLOW != 0 {
            unsafe { set_errno(libc::EINVAL) };
            return -1;
        }
        match runtime.chmod(
            path,
            directory,
            mode,
            flags & libc::AT_SYMLINK_NOFOLLOW == 0,
        ) {
            Ok(()) => 0,
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fchmodat(
    directory: libc::c_int,
    path: *const libc::c_char,
    mode: libc::mode_t,
    flags: libc::c_int,
) -> libc::c_int {
    unsafe { sandbox_fchmodat(directory, path, mode, flags) }
}

unsafe fn mapped_stat(
    path: *const libc::c_char,
    status: *mut libc::stat,
    original: StatFn,
    follow_final: bool,
) -> libc::c_int {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return unsafe { original(path, status) };
    };
    let Some(runtime) = FilesystemHookRuntime::global() else {
        return unsafe { original(path, status) };
    };
    let caller_errno = unsafe { *libc::__error() };
    match runtime.map_metadata(
        path,
        libc::AT_FDCWD,
        follow_final,
        &Credentials::effective(),
    ) {
        Ok((mapped, plaintext_size, attributes, _anchor)) => {
            let result = unsafe { original(mapped.as_ptr(), status) };
            if result == 0 && !status.is_null() {
                unsafe { patch_stat(&mut *status, plaintext_size, attributes.as_ref()) };
            }
            if result == 0 {
                unsafe { set_errno(caller_errno) };
            }
            result
        }
        Err(error) => unsafe { fail(&error, -1) },
    }
}

pub(super) unsafe fn patch_stat(
    status: &mut libc::stat,
    plaintext_size: Option<libc::off_t>,
    attributes: Option<&FileAttributes>,
) {
    if let Some(size) = plaintext_size {
        status.st_size = size;
    }
    if let Some(attributes) = attributes {
        status.st_mode = attributes.mode as _;
        status.st_uid = attributes.uid;
        status.st_gid = attributes.gid;
        status.st_atime = attributes.atime;
        status.st_atime_nsec = attributes.atime_nsec;
        status.st_mtime = attributes.mtime;
        status.st_mtime_nsec = attributes.mtime_nsec;
    }
}

unsafe fn sandbox_stat(path: *const libc::c_char, status: *mut libc::stat) -> libc::c_int {
    catch_filesystem_panic(-1, || match original_stat() {
        Some(original) => unsafe { mapped_stat(path, status, original, true) },
        None => {
            unsafe { set_errno(libc::ENOSYS) };
            -1
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_stat(
    path: *const libc::c_char,
    status: *mut libc::stat,
) -> libc::c_int {
    unsafe { sandbox_stat(path, status) }
}

unsafe fn sandbox_lstat(path: *const libc::c_char, status: *mut libc::stat) -> libc::c_int {
    catch_filesystem_panic(-1, || match original_lstat() {
        Some(original) => unsafe { mapped_stat(path, status, original, false) },
        None => {
            unsafe { set_errno(libc::ENOSYS) };
            -1
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_lstat(
    path: *const libc::c_char,
    status: *mut libc::stat,
) -> libc::c_int {
    unsafe { sandbox_lstat(path, status) }
}

unsafe fn sandbox_fstatat(
    directory: libc::c_int,
    path: *const libc::c_char,
    status: *mut libc::stat,
    flags: libc::c_int,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_fstatat() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(directory, path, status, flags) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(directory, path, status, flags) };
        };
        let caller_errno = unsafe { *libc::__error() };
        if let Some(result) = unsafe {
            native_directory_fstatat(
                runtime,
                directory,
                path,
                status,
                flags,
                original,
                caller_errno,
            )
        } {
            return result;
        }
        let follow_final = flags & libc::AT_SYMLINK_NOFOLLOW == 0;
        match runtime.map_metadata(path, directory, follow_final, &Credentials::effective()) {
            Ok((mapped, plaintext_size, attributes, _anchor)) => {
                let result = unsafe { original(libc::AT_FDCWD, mapped.as_ptr(), status, flags) };
                if result == 0 && !status.is_null() {
                    unsafe { patch_stat(&mut *status, plaintext_size, attributes.as_ref()) };
                }
                if result == 0 {
                    unsafe { set_errno(caller_errno) };
                }
                result
            }
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

unsafe fn native_directory_fstatat(
    runtime: &FilesystemHookRuntime,
    directory: libc::c_int,
    path: *const libc::c_char,
    status: *mut libc::stat,
    flags: libc::c_int,
    original: FstatAtFn,
    caller_errno: libc::c_int,
) -> Option<libc::c_int> {
    if directory == libc::AT_FDCWD
        || path.is_null()
        || status.is_null()
        || flags & !libc::AT_SYMLINK_NOFOLLOW != 0
        || !runtime.native_directory_snapshot_is_current(directory)
    {
        return None;
    }
    let path = unsafe { CStr::from_ptr(path) };
    let name = path.to_bytes();
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
        return None;
    }
    if flags & libc::AT_SYMLINK_NOFOLLOW != 0 {
        let result = unsafe { original(directory, path.as_ptr(), status, flags) };
        if result == 0 {
            unsafe { set_errno(caller_errno) };
        }
        return Some(result);
    }
    let mut probe = unsafe { std::mem::zeroed::<libc::stat>() };
    let result = unsafe {
        original(
            directory,
            path.as_ptr(),
            &mut probe,
            flags | libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Some(result);
    }
    if probe.st_mode & libc::S_IFMT == libc::S_IFLNK {
        unsafe { set_errno(caller_errno) };
        return None;
    }
    unsafe {
        *status = probe;
        set_errno(caller_errno);
    }
    Some(0)
}

unsafe fn sandbox_fstat(descriptor: libc::c_int, status: *mut libc::stat) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_fstat() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor, status) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(descriptor, status) };
        };
        let caller_errno = unsafe { *libc::__error() };
        let result = unsafe { original(descriptor, status) };
        if result == 0
            && !status.is_null()
            && let Some(open) = runtime.tracked_open(descriptor)
        {
            let attributes = match open.managed_attributes(runtime) {
                Ok(Some(attributes)) => Some(attributes),
                Ok(None) => match runtime.filesystem.attributes(&open.logical()) {
                    Ok(attributes) => attributes,
                    Err(error) => return unsafe { fail(&error, -1) },
                },
                Err(error) => return unsafe { fail(&error, -1) },
            };
            unsafe { patch_stat(&mut *status, None, attributes.as_ref()) };
            if let Some(local) = open.local_inheritance() {
                unsafe {
                    (*status).st_dev = local.identity.device as _;
                    (*status).st_ino = local.identity.inode;
                    (*status).st_nlink = local.identity.links as _;
                }
            } else if let Some(identity) = open.identity {
                unsafe {
                    (*status).st_dev = identity.device as _;
                    (*status).st_ino = identity.inode;
                }
            }
        }
        if result == 0 {
            unsafe { set_errno(caller_errno) };
        }
        result
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fstat(
    descriptor: libc::c_int,
    status: *mut libc::stat,
) -> libc::c_int {
    unsafe { sandbox_fstat(descriptor, status) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fstatat(
    directory: libc::c_int,
    path: *const libc::c_char,
    status: *mut libc::stat,
    flags: libc::c_int,
) -> libc::c_int {
    unsafe { sandbox_fstatat(directory, path, status, flags) }
}

unsafe fn sandbox_access(path: *const libc::c_char, mode: libc::c_int) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_access() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(path, mode) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(path, mode) };
        };
        let request = match AccessRequest::from_access_mode(mode) {
            Ok(request) => request,
            Err(error) => return unsafe { fail(&error.into(), -1) },
        };
        let credentials = Credentials::real();
        execute_access_plan(
            unsafe { runtime.prepare_access(path, libc::AT_FDCWD, true, request, &credentials) },
            |mapped| unsafe { original(mapped, mode) },
        )
    })
}

fn execute_access_plan(
    plan: Result<AccessPlan>,
    native: impl FnOnce(*const libc::c_char) -> libc::c_int,
) -> libc::c_int {
    match plan {
        Ok(AccessPlan::Allowed) => 0,
        Ok(AccessPlan::Native(mapped)) => {
            let mapped = match CString::new(mapped.as_os_str().as_bytes()) {
                Ok(mapped) => mapped,
                Err(error) => return unsafe { fail(&error.into(), -1) },
            };
            native(mapped.as_ptr())
        }
        Err(error) => unsafe { fail(&error, -1) },
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_access(
    path: *const libc::c_char,
    mode: libc::c_int,
) -> libc::c_int {
    unsafe { sandbox_access(path, mode) }
}

unsafe fn sandbox_faccessat(
    directory: libc::c_int,
    path: *const libc::c_char,
    mode: libc::c_int,
    flags: libc::c_int,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_faccessat() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(directory, path, mode, flags) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(directory, path, mode, flags) };
        };
        if flags & !(libc::AT_EACCESS | libc::AT_SYMLINK_NOFOLLOW) != 0 {
            unsafe { set_errno(libc::EINVAL) };
            return -1;
        }
        let request = match AccessRequest::from_access_mode(mode) {
            Ok(request) => request,
            Err(error) => return unsafe { fail(&error.into(), -1) },
        };
        let credentials = if flags & libc::AT_EACCESS != 0 {
            Credentials::effective()
        } else {
            Credentials::real()
        };
        execute_access_plan(
            unsafe {
                runtime.prepare_access(
                    path,
                    directory,
                    flags & libc::AT_SYMLINK_NOFOLLOW == 0,
                    request,
                    &credentials,
                )
            },
            |mapped| unsafe { original(libc::AT_FDCWD, mapped, mode, flags) },
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_faccessat(
    directory: libc::c_int,
    path: *const libc::c_char,
    mode: libc::c_int,
    flags: libc::c_int,
) -> libc::c_int {
    unsafe { sandbox_faccessat(directory, path, mode, flags) }
}

unsafe fn sandbox_readlink(
    path: *const libc::c_char,
    buffer: *mut libc::c_char,
    size: libc::size_t,
) -> libc::ssize_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_readlink() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(path, buffer, size) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(path, buffer, size) };
        };
        match runtime.map_metadata(path, libc::AT_FDCWD, false, &Credentials::effective()) {
            Ok((mapped, _, _, _anchor)) => unsafe { original(mapped.as_ptr(), buffer, size) },
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_readlink(
    path: *const libc::c_char,
    buffer: *mut libc::c_char,
    size: libc::size_t,
) -> libc::ssize_t {
    unsafe { sandbox_readlink(path, buffer, size) }
}

unsafe fn sandbox_readlinkat(
    directory: libc::c_int,
    path: *const libc::c_char,
    buffer: *mut libc::c_char,
    size: libc::size_t,
) -> libc::ssize_t {
    catch_filesystem_panic(-1, || {
        let Some(original_at) = original_readlinkat() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original_at(directory, path, buffer, size) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original_at(directory, path, buffer, size) };
        };
        let Some(original) = original_readlink() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        match runtime.map_metadata(path, directory, false, &Credentials::effective()) {
            Ok((mapped, _, _, _anchor)) => unsafe { original(mapped.as_ptr(), buffer, size) },
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_readlinkat(
    directory: libc::c_int,
    path: *const libc::c_char,
    buffer: *mut libc::c_char,
    size: libc::size_t,
) -> libc::ssize_t {
    unsafe { sandbox_readlinkat(directory, path, buffer, size) }
}

fn original_chmod() -> Option<ChmodFn> {
    function_from_interpose(&INTERPOSE_CHMOD)
}

fn original_fchmod() -> Option<FchmodFn> {
    function_from_interpose(&INTERPOSE_FCHMOD)
}

fn original_fchmodat() -> Option<FchmodAtFn> {
    function_from_interpose(&INTERPOSE_FCHMODAT)
}

fn original_stat() -> Option<StatFn> {
    function_from_interpose(&INTERPOSE_STAT)
}

pub(super) fn original_lstat() -> Option<StatFn> {
    function_from_interpose(&INTERPOSE_LSTAT)
}

fn original_fstatat() -> Option<FstatAtFn> {
    function_from_interpose(&INTERPOSE_FSTATAT)
}

fn original_fstat() -> Option<FstatFn> {
    function_from_interpose(&INTERPOSE_FSTAT)
}

fn original_access() -> Option<AccessFn> {
    function_from_interpose(&INTERPOSE_ACCESS)
}

fn original_faccessat() -> Option<FaccessAtFn> {
    function_from_interpose(&INTERPOSE_FACCESSAT)
}

fn original_readlink() -> Option<ReadlinkFn> {
    function_from_interpose(&INTERPOSE_READLINK)
}

fn original_readlinkat() -> Option<ReadlinkAtFn> {
    function_from_interpose(&INTERPOSE_READLINKAT)
}

dyld_interpose!(INTERPOSE_CHMOD, agora_sandbox_chmod, libc::chmod);

dyld_interpose!(INTERPOSE_FCHMOD, agora_sandbox_fchmod, libc::fchmod);

dyld_interpose!(INTERPOSE_FCHMODAT, agora_sandbox_fchmodat, libc::fchmodat);

dyld_interpose!(INTERPOSE_STAT, agora_sandbox_stat, libc::stat);

dyld_interpose!(INTERPOSE_LSTAT, agora_sandbox_lstat, libc::lstat);

dyld_interpose!(INTERPOSE_FSTATAT, agora_sandbox_fstatat, libc::fstatat);

dyld_interpose!(INTERPOSE_FSTAT, agora_sandbox_fstat, libc::fstat);

dyld_interpose!(INTERPOSE_ACCESS, agora_sandbox_access, libc::access);

dyld_interpose!(
    INTERPOSE_FACCESSAT,
    agora_sandbox_faccessat,
    libc::faccessat
);

dyld_interpose!(INTERPOSE_READLINK, agora_sandbox_readlink, libc::readlink);

dyld_interpose!(
    INTERPOSE_READLINKAT,
    agora_sandbox_readlinkat,
    libc::readlinkat
);
