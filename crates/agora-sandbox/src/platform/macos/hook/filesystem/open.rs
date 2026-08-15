use super::descriptor::{
    agora_sandbox_guarded_close, original_close, original_fclose, release_local_close_locks,
};
use super::*;

type OpenFn = unsafe extern "C" fn(*const libc::c_char, libc::c_int, libc::mode_t) -> libc::c_int;
type OpenAtFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::c_char,
    libc::c_int,
    libc::mode_t,
) -> libc::c_int;
type FopenFn = unsafe extern "C" fn(*const libc::c_char, *const libc::c_char) -> *mut libc::FILE;
type FreopenFn = unsafe extern "C" fn(
    *const libc::c_char,
    *const libc::c_char,
    *mut libc::FILE,
) -> *mut libc::FILE;
type PosixSpawnAddOpenFn = unsafe extern "C" fn(
    *mut libc::posix_spawn_file_actions_t,
    libc::c_int,
    *const libc::c_char,
    libc::c_int,
    libc::mode_t,
) -> libc::c_int;

#[derive(Clone, Copy)]
enum GuardedOpenKind {
    Regular,
    DataProtected {
        class: libc::c_int,
        flags: libc::c_int,
    },
}

#[derive(Clone, Copy)]
enum OpenOperation {
    Regular(OpenFn),
    Guarded {
        kind: GuardedOpenKind,
        guard: *const GuardId,
        guard_flags: libc::c_uint,
    },
}

impl OpenOperation {
    unsafe fn call(
        self,
        path: *const libc::c_char,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> libc::c_int {
        match self {
            Self::Regular(original) => unsafe { original(path, flags, mode) },
            Self::Guarded {
                kind,
                guard,
                guard_flags,
            } => unsafe { call_original_guarded_open(kind, path, guard, guard_flags, flags, mode) },
        }
    }

    unsafe fn close(self, descriptor: libc::c_int) {
        match self {
            Self::Regular(_) => {
                if let Some(close) = original_close() {
                    unsafe { close(descriptor) };
                }
            }
            Self::Guarded { guard, .. } => {
                unsafe { agora_sandbox_guarded_close(descriptor, guard) };
            }
        }
    }

    unsafe fn configure_anonymous(self, descriptor: libc::c_int) -> std::io::Result<()> {
        let Self::Guarded {
            guard, guard_flags, ..
        } = self
        else {
            return Ok(());
        };
        let mut descriptor_flags = 0;
        if unsafe {
            system_change_fdguard_np(
                descriptor,
                std::ptr::null(),
                0,
                guard,
                guard_flags,
                &mut descriptor_flags,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

unsafe fn sandbox_open_with_mode(
    path: *const libc::c_char,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    let Some(original) = original_open() else {
        unsafe { set_errno(libc::ENOSYS) };
        return -1;
    };
    unsafe { sandbox_open_with_operation(path, flags, mode, OpenOperation::Regular(original)) }
}

unsafe fn sandbox_open_with_operation(
    path: *const libc::c_char,
    flags: libc::c_int,
    mode: libc::mode_t,
    operation: OpenOperation,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { operation.call(path, flags, mode) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { operation.call(path, flags, mode) };
        };
        match runtime.prepare_open(path, libc::AT_FDCWD, flags, mode) {
            Ok(request) => {
                match request.native_path() {
                    Ok(Some(native)) => {
                        return unsafe { operation.call(native.as_ptr(), flags, mode) };
                    }
                    Ok(None) => {}
                    Err(error) => return unsafe { fail(&error, -1) },
                }
                if let Err(error) = runtime.publish(FileOperation::Open, request.file.clone()) {
                    return unsafe { fail_audit(&error, -1) };
                }
                let mut prepared = request.into_prepared();
                let target_is_path = matches!(prepared.prepared.target(), OpenTarget::Path(_));
                let path_descriptor = match prepared.prepared.target() {
                    OpenTarget::Path(mapped) => {
                        let mapped = match CString::new(mapped.as_os_str().as_bytes()) {
                            Ok(mapped) => mapped,
                            Err(error) => return unsafe { fail(&error.into(), -1) },
                        };
                        Some(unsafe { operation.call(mapped.as_ptr(), flags, mode) })
                    }
                    OpenTarget::Descriptor(_) => None,
                };
                if path_descriptor.is_some_and(|descriptor| descriptor < 0) {
                    return path_descriptor.unwrap();
                }
                if let Err(error) = runtime.commit_open(&mut prepared) {
                    if let Some(descriptor) = path_descriptor {
                        unsafe { operation.close(descriptor) };
                    }
                    return unsafe { fail(&error, -1) };
                }
                if !target_is_path {
                    let descriptor = match prepared.prepared.target() {
                        OpenTarget::Descriptor(file) => file.as_raw_fd(),
                        OpenTarget::Path(_) => unreachable!("open target kind changed"),
                    };
                    let local = prepared.has_encrypted_broker();
                    if let Err(error) =
                        configure_descriptor(descriptor, flags, local).and_then(|()| {
                            unsafe { operation.configure_anonymous(descriptor) }.map_err(Into::into)
                        })
                    {
                        let (target, open) = prepared.into_parts();
                        let _ = runtime.finish_open_file(descriptor, &open);
                        drop(target);
                        return unsafe { fail(&error, -1) };
                    }
                }
                let (target, open) = prepared.into_parts();
                let descriptor = match target {
                    OpenTarget::Path(_) => path_descriptor.expect("path target was opened"),
                    OpenTarget::Descriptor(file) => file.into_raw_fd(),
                };
                runtime.register(descriptor, open);
                descriptor
            }
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_open_with_mode(
    path: *const libc::c_char,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    unsafe { sandbox_open_with_mode(path, flags, mode) }
}

unsafe fn sandbox_guarded_open_with_mode(
    path: *const libc::c_char,
    guard: *const GuardId,
    guardflags: libc::c_uint,
    flags: libc::c_int,
    mode: libc::mode_t,
    kind: GuardedOpenKind,
) -> libc::c_int {
    if !guarded_open_available(kind) {
        unsafe { set_errno(libc::ENOSYS) };
        return -1;
    }
    unsafe {
        sandbox_open_with_operation(
            path,
            flags,
            mode,
            OpenOperation::Guarded {
                kind,
                guard,
                guard_flags: guardflags,
            },
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_guarded_open_with_mode(
    path: *const libc::c_char,
    guard: *const GuardId,
    guardflags: libc::c_uint,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    unsafe {
        sandbox_guarded_open_with_mode(
            path,
            guard,
            guardflags,
            flags,
            mode,
            GuardedOpenKind::Regular,
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_guarded_open_dprotected_with_mode(
    path: *const libc::c_char,
    guard: *const GuardId,
    guardflags: libc::c_uint,
    flags: libc::c_int,
    class: libc::c_int,
    protection_flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    unsafe {
        sandbox_guarded_open_with_mode(
            path,
            guard,
            guardflags,
            flags,
            mode,
            GuardedOpenKind::DataProtected {
                class,
                flags: protection_flags,
            },
        )
    }
}

unsafe fn sandbox_openat_with_mode(
    directory: libc::c_int,
    path: *const libc::c_char,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_openat() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(directory, path, flags, mode) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(directory, path, flags, mode) };
        };
        match runtime.prepare_open(path, directory, flags, mode) {
            Ok(request) => {
                match request.native_path() {
                    Ok(Some(native)) => {
                        return unsafe { original(libc::AT_FDCWD, native.as_ptr(), flags, mode) };
                    }
                    Ok(None) => {}
                    Err(error) => return unsafe { fail(&error, -1) },
                }
                if let Err(error) = runtime.publish(FileOperation::Open, request.file.clone()) {
                    return unsafe { fail_audit(&error, -1) };
                }
                let mut prepared = request.into_prepared();
                let target_is_path = matches!(prepared.prepared.target(), OpenTarget::Path(_));
                let path_descriptor = match prepared.prepared.target() {
                    OpenTarget::Path(mapped) => {
                        let mapped = match CString::new(mapped.as_os_str().as_bytes()) {
                            Ok(mapped) => mapped,
                            Err(error) => return unsafe { fail(&error.into(), -1) },
                        };
                        Some(unsafe { original(libc::AT_FDCWD, mapped.as_ptr(), flags, mode) })
                    }
                    OpenTarget::Descriptor(_) => None,
                };
                if path_descriptor.is_some_and(|descriptor| descriptor < 0) {
                    return path_descriptor.unwrap();
                }
                if let Err(error) = runtime.commit_open(&mut prepared) {
                    if let Some(descriptor) = path_descriptor
                        && let Some(close) = original_close()
                    {
                        unsafe { close(descriptor) };
                    }
                    return unsafe { fail(&error, -1) };
                }
                if !target_is_path {
                    let descriptor = match prepared.prepared.target() {
                        OpenTarget::Descriptor(file) => file.as_raw_fd(),
                        OpenTarget::Path(_) => unreachable!("open target kind changed"),
                    };
                    if let Err(error) =
                        configure_descriptor(descriptor, flags, prepared.has_encrypted_broker())
                    {
                        let (target, open) = prepared.into_parts();
                        let _ = runtime.finish_open_file(descriptor, &open);
                        drop(target);
                        return unsafe { fail(&error, -1) };
                    }
                }
                let (target, open) = prepared.into_parts();
                let descriptor = match target {
                    OpenTarget::Path(_) => path_descriptor.expect("path target was opened"),
                    OpenTarget::Descriptor(file) => file.into_raw_fd(),
                };
                runtime.register(descriptor, open);
                descriptor
            }
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_openat_with_mode(
    directory: libc::c_int,
    path: *const libc::c_char,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    unsafe { sandbox_openat_with_mode(directory, path, flags, mode) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_creat(
    path: *const libc::c_char,
    mode: libc::mode_t,
) -> libc::c_int {
    unsafe { sandbox_open_with_mode(path, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, mode) }
}

unsafe fn sandbox_fopen(path: *const libc::c_char, mode: *const libc::c_char) -> *mut libc::FILE {
    catch_filesystem_panic(std::ptr::null_mut(), || {
        let Some(original) = original_fopen() else {
            unsafe { set_errno(libc::ENOSYS) };
            return std::ptr::null_mut();
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(path, mode) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(path, mode) };
        };
        match runtime.prepare_fopen(path, mode) {
            Ok(request) => {
                match request.native_path() {
                    Ok(Some(native)) => return unsafe { original(native.as_ptr(), mode) },
                    Ok(None) => {}
                    Err(error) => {
                        return unsafe { fail(&error, std::ptr::null_mut()) };
                    }
                }
                let flags = request.intent.flags();
                if let Err(error) = runtime.publish(FileOperation::Open, request.file.clone()) {
                    return unsafe { fail_audit(&error, std::ptr::null_mut()) };
                }
                let mut prepared = request.into_prepared();
                let path_stream = match prepared.prepared.target() {
                    OpenTarget::Path(mapped) => {
                        let mapped = match CString::new(mapped.as_os_str().as_bytes()) {
                            Ok(mapped) => mapped,
                            Err(error) => {
                                return unsafe { fail(&error.into(), std::ptr::null_mut()) };
                            }
                        };
                        Some(unsafe { original(mapped.as_ptr(), mode) })
                    }
                    OpenTarget::Descriptor(_) => None,
                };
                if path_stream.is_some_and(|stream| stream.is_null()) {
                    return std::ptr::null_mut();
                }
                if let Err(error) = runtime.commit_open(&mut prepared) {
                    if let Some(stream) = path_stream
                        && let Some(close) = original_fclose()
                    {
                        unsafe { close(stream) };
                    }
                    return unsafe { fail(&error, std::ptr::null_mut()) };
                }
                let (target, open) = prepared.into_parts();
                let stream = match target {
                    OpenTarget::Path(_) => path_stream.expect("path target was opened"),
                    OpenTarget::Descriptor(file) => {
                        let descriptor = file.as_raw_fd();
                        if let Err(error) = configure_descriptor(
                            descriptor,
                            flags,
                            open.supports_exec_inheritance(),
                        ) {
                            let _ = runtime.finish_open_file(descriptor, &open);
                            return unsafe { fail(&error, std::ptr::null_mut()) };
                        }
                        let descriptor = file.into_raw_fd();
                        let stream = unsafe { libc::fdopen(descriptor, mode) };
                        if stream.is_null()
                            && let Some(close) = original_close()
                        {
                            unsafe { close(descriptor) };
                            let _ = runtime.finish_open_file(-1, &open);
                        }
                        stream
                    }
                };
                if stream.is_null() {
                    return stream;
                }
                let descriptor = unsafe { libc::fileno(stream) };
                if descriptor >= 0 {
                    runtime.register(descriptor, open);
                }
                stream
            }
            Err(error) => unsafe { fail(&error, std::ptr::null_mut()) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fopen(
    path: *const libc::c_char,
    mode: *const libc::c_char,
) -> *mut libc::FILE {
    unsafe { sandbox_fopen(path, mode) }
}

unsafe fn sandbox_freopen(
    path: *const libc::c_char,
    mode: *const libc::c_char,
    stream: *mut libc::FILE,
) -> *mut libc::FILE {
    catch_filesystem_panic(std::ptr::null_mut(), || {
        let Some(original) = original_freopen() else {
            unsafe { set_errno(libc::ENOSYS) };
            return std::ptr::null_mut();
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(path, mode, stream) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(path, mode, stream) };
        };
        match unsafe { runtime.native_passthrough_c_path(path, libc::AT_FDCWD) } {
            Ok(Some(native)) => {
                let descriptor = if stream.is_null() {
                    -1
                } else {
                    unsafe { libc::fileno(stream) }
                };
                if descriptor < 0 {
                    return unsafe { original(native.as_ptr(), mode, stream) };
                }
                let _operation = match runtime.acquire_descriptor_replacement(None, descriptor) {
                    Ok(operation) => operation,
                    Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
                };
                let transition = runtime.begin_descriptor_transition_under_lease(descriptor);
                let result = unsafe { original(native.as_ptr(), mode, stream) };
                if let Some((open, last_alias)) =
                    runtime.take_descriptor_during_transition_under_lease(descriptor)
                {
                    release_local_close_locks(&open, last_alias);
                    if last_alias && !runtime.has_mapping(&open) {
                        let _ = runtime.finish_open_file(-1, &open);
                    }
                }
                transition.clear();
                runtime.unregister_directory(descriptor);
                return result;
            }
            Ok(None) => {}
            Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
        }
        unsafe { set_errno(libc::ENOTSUP) };
        std::ptr::null_mut()
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_freopen(
    path: *const libc::c_char,
    mode: *const libc::c_char,
    stream: *mut libc::FILE,
) -> *mut libc::FILE {
    unsafe { sandbox_freopen(path, mode, stream) }
}

unsafe fn sandbox_posix_spawn_file_actions_addopen(
    actions: *mut libc::posix_spawn_file_actions_t,
    descriptor: libc::c_int,
    path: *const libc::c_char,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    catch_filesystem_panic(libc::EIO, || {
        let Some(original) = original_posix_spawn_file_actions_addopen() else {
            return libc::ENOSYS;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(actions, descriptor, path, flags, mode) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(actions, descriptor, path, flags, mode) };
        };
        let request = match runtime.prepare_open(path, libc::AT_FDCWD, flags, mode) {
            Ok(request) => request,
            Err(error) => return error_errno(&error),
        };
        match request.native_path() {
            Ok(Some(native)) => {
                return unsafe { original(actions, descriptor, native.as_ptr(), flags, mode) };
            }
            Ok(None) => {}
            Err(error) => return error_errno(&error),
        }
        if let Err(error) = runtime.publish(FileOperation::Open, request.file.clone()) {
            return error.errno();
        }
        let write_intent = flags & libc::O_ACCMODE != libc::O_RDONLY
            || flags & (libc::O_CREAT | libc::O_TRUNC | libc::O_APPEND) != 0;
        if write_intent {
            return libc::ENOTSUP;
        }
        let prepared = request.into_prepared();
        let mapped = match prepared.prepared.target() {
            OpenTarget::Path(mapped) => match CString::new(mapped.as_os_str().as_bytes()) {
                Ok(mapped) => mapped,
                Err(error) => return error_errno(&error.into()),
            },
            OpenTarget::Descriptor(_) => return libc::ENOTSUP,
        };
        unsafe { original(actions, descriptor, mapped.as_ptr(), flags, mode) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_posix_spawn_file_actions_addopen(
    actions: *mut libc::posix_spawn_file_actions_t,
    descriptor: libc::c_int,
    path: *const libc::c_char,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    unsafe { sandbox_posix_spawn_file_actions_addopen(actions, descriptor, path, flags, mode) }
}

fn original_open() -> Option<OpenFn> {
    (!INTERPOSE_OPEN.replacee.is_null()).then_some(call_original_open)
}

fn original_openat() -> Option<OpenAtFn> {
    (!INTERPOSE_OPENAT.replacee.is_null()).then_some(call_original_openat)
}

fn guarded_open_available(kind: GuardedOpenKind) -> bool {
    match kind {
        GuardedOpenKind::Regular => !INTERPOSE_GUARDED_OPEN.replacee.is_null(),
        GuardedOpenKind::DataProtected { .. } => {
            !INTERPOSE_GUARDED_OPEN_DPROTECTED.replacee.is_null()
        }
    }
}

unsafe fn call_original_guarded_open(
    kind: GuardedOpenKind,
    path: *const libc::c_char,
    guard: *const GuardId,
    guardflags: libc::c_uint,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    match kind {
        GuardedOpenKind::Regular => unsafe {
            agora_sandbox_call_guarded_open(
                INTERPOSE_GUARDED_OPEN.replacee,
                path,
                guard,
                guardflags,
                flags,
                mode,
            )
        },
        GuardedOpenKind::DataProtected {
            class,
            flags: protection_flags,
        } => unsafe {
            agora_sandbox_call_guarded_open_dprotected(
                INTERPOSE_GUARDED_OPEN_DPROTECTED.replacee,
                path,
                guard,
                guardflags,
                flags,
                class,
                protection_flags,
                mode,
            )
        },
    }
}

unsafe extern "C" fn call_original_open(
    path: *const libc::c_char,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    unsafe { agora_sandbox_call_open(INTERPOSE_OPEN.replacee, path, flags, mode) }
}

unsafe extern "C" fn call_original_openat(
    directory: libc::c_int,
    path: *const libc::c_char,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    unsafe { agora_sandbox_call_openat(INTERPOSE_OPENAT.replacee, directory, path, flags, mode) }
}

fn original_fopen() -> Option<FopenFn> {
    function_from_interpose(&INTERPOSE_FOPEN)
}

fn original_freopen() -> Option<FreopenFn> {
    function_from_interpose(&INTERPOSE_FREOPEN)
}

fn original_posix_spawn_file_actions_addopen() -> Option<PosixSpawnAddOpenFn> {
    function_from_interpose(&INTERPOSE_POSIX_SPAWN_FILE_ACTIONS_ADDOPEN)
}

unsafe extern "C" {
    fn agora_sandbox_call_open(
        function: *const libc::c_void,
        path: *const libc::c_char,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> libc::c_int;

    fn agora_sandbox_call_openat(
        function: *const libc::c_void,
        directory: libc::c_int,
        path: *const libc::c_char,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> libc::c_int;

    fn agora_sandbox_call_guarded_open(
        function: *const libc::c_void,
        path: *const libc::c_char,
        guard: *const GuardId,
        guardflags: libc::c_uint,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> libc::c_int;

    fn agora_sandbox_call_guarded_open_dprotected(
        function: *const libc::c_void,
        path: *const libc::c_char,
        guard: *const GuardId,
        guardflags: libc::c_uint,
        flags: libc::c_int,
        class: libc::c_int,
        protection_flags: libc::c_int,
        mode: libc::mode_t,
    ) -> libc::c_int;

    fn agora_sandbox_open_shim(path: *const libc::c_char, flags: libc::c_int, ...) -> libc::c_int;

    fn agora_sandbox_openat_shim(
        directory: libc::c_int,
        path: *const libc::c_char,
        flags: libc::c_int,
        ...
    ) -> libc::c_int;

    fn agora_sandbox_guarded_open_shim(
        path: *const libc::c_char,
        guard: *const GuardId,
        guardflags: libc::c_uint,
        flags: libc::c_int,
        ...
    ) -> libc::c_int;

    fn agora_sandbox_guarded_open_dprotected_shim(
        path: *const libc::c_char,
        guard: *const GuardId,
        guardflags: libc::c_uint,
        flags: libc::c_int,
        class: libc::c_int,
        protection_flags: libc::c_int,
        ...
    ) -> libc::c_int;

    #[link_name = "guarded_open_np"]
    fn system_guarded_open_np(
        path: *const libc::c_char,
        guard: *const GuardId,
        guardflags: libc::c_uint,
        flags: libc::c_int,
        ...
    ) -> libc::c_int;

    #[link_name = "guarded_open_dprotected_np"]
    fn system_guarded_open_dprotected_np(
        path: *const libc::c_char,
        guard: *const GuardId,
        guardflags: libc::c_uint,
        flags: libc::c_int,
        class: libc::c_int,
        protection_flags: libc::c_int,
        ...
    ) -> libc::c_int;

    #[link_name = "change_fdguard_np"]
    fn system_change_fdguard_np(
        descriptor: libc::c_int,
        guard: *const GuardId,
        guardflags: libc::c_uint,
        new_guard: *const GuardId,
        new_guardflags: libc::c_uint,
        descriptor_flags: *mut libc::c_int,
    ) -> libc::c_int;
}

dyld_interpose!(INTERPOSE_OPEN, agora_sandbox_open_shim, libc::open);

dyld_interpose!(INTERPOSE_OPENAT, agora_sandbox_openat_shim, libc::openat);

dyld_interpose!(
    INTERPOSE_GUARDED_OPEN,
    agora_sandbox_guarded_open_shim,
    system_guarded_open_np
);

dyld_interpose!(
    INTERPOSE_GUARDED_OPEN_DPROTECTED,
    agora_sandbox_guarded_open_dprotected_shim,
    system_guarded_open_dprotected_np
);

dyld_interpose!(INTERPOSE_CREAT, agora_sandbox_creat, libc::creat);

dyld_interpose!(INTERPOSE_FOPEN, agora_sandbox_fopen, libc::fopen);

dyld_interpose!(INTERPOSE_FREOPEN, agora_sandbox_freopen, libc::freopen);

dyld_interpose!(
    INTERPOSE_POSIX_SPAWN_FILE_ACTIONS_ADDOPEN,
    agora_sandbox_posix_spawn_file_actions_addopen,
    libc::posix_spawn_file_actions_addopen
);
