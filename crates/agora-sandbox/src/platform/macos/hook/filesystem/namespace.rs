use super::*;

type UnlinkFn = unsafe extern "C" fn(*const libc::c_char) -> libc::c_int;
type UnlinkAtFn =
    unsafe extern "C" fn(libc::c_int, *const libc::c_char, libc::c_int) -> libc::c_int;
type RenameFn = unsafe extern "C" fn(*const libc::c_char, *const libc::c_char) -> libc::c_int;
type RenameAtFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::c_char,
    libc::c_int,
    *const libc::c_char,
) -> libc::c_int;
type RenameXFn =
    unsafe extern "C" fn(*const libc::c_char, *const libc::c_char, libc::c_uint) -> libc::c_int;
type RenameAtXFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::c_char,
    libc::c_int,
    *const libc::c_char,
    libc::c_uint,
) -> libc::c_int;
type MkdirFn = unsafe extern "C" fn(*const libc::c_char, libc::mode_t) -> libc::c_int;
type MkdirAtFn =
    unsafe extern "C" fn(libc::c_int, *const libc::c_char, libc::mode_t) -> libc::c_int;
type SymlinkFn = unsafe extern "C" fn(*const libc::c_char, *const libc::c_char) -> libc::c_int;
type SymlinkAtFn =
    unsafe extern "C" fn(*const libc::c_char, libc::c_int, *const libc::c_char) -> libc::c_int;

unsafe fn sandbox_symlink(target: *const libc::c_char, link: *const libc::c_char) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_symlink() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(target, link) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(target, link) };
        };
        match runtime.create_symlink(target, libc::AT_FDCWD, link) {
            Ok(()) => 0,
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_symlink(
    target: *const libc::c_char,
    link: *const libc::c_char,
) -> libc::c_int {
    unsafe { sandbox_symlink(target, link) }
}

unsafe fn sandbox_symlinkat(
    target: *const libc::c_char,
    directory: libc::c_int,
    link: *const libc::c_char,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_symlinkat() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(target, directory, link) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(target, directory, link) };
        };
        match runtime.create_symlink(target, directory, link) {
            Ok(()) => 0,
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_symlinkat(
    target: *const libc::c_char,
    directory: libc::c_int,
    link: *const libc::c_char,
) -> libc::c_int {
    unsafe { sandbox_symlinkat(target, directory, link) }
}

macro_rules! overlay_mutation_hook {
    (
        $sandbox:ident, $export:ident, $original:ident,
        ($($argument:ident: $argument_type:ty),* $(,)?),
        ($($call_argument:expr),* $(,)?),
        |$runtime:ident| $operation:expr
    ) => {
        unsafe fn $sandbox($($argument: $argument_type),*) -> libc::c_int {
            catch_filesystem_panic(-1, || {
                let Some(original) = $original() else {
                    unsafe { set_errno(libc::ENOSYS) };
                    return -1;
                };
                let Some(_guard) = FilesystemHookGuard::enter() else {
                    return unsafe { original($($call_argument),*) };
                };
                let Some($runtime) = FilesystemHookRuntime::global() else {
                    return unsafe { original($($call_argument),*) };
                };
                match $operation {
                    Ok(()) => 0,
                    Err(error) => unsafe { fail(&error, -1) },
                }
            })
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $export($($argument: $argument_type),*) -> libc::c_int {
            unsafe { $sandbox($($argument),*) }
        }
    }
}

overlay_mutation_hook!(
    sandbox_unlink,
    agora_sandbox_unlink,
    original_unlink,
    (path: *const libc::c_char),
    (path),
    |runtime| runtime.remove(libc::AT_FDCWD, path, false)
);

unsafe fn sandbox_unlinkat(
    directory: libc::c_int,
    path: *const libc::c_char,
    flags: libc::c_int,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_unlinkat() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(directory, path, flags) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(directory, path, flags) };
        };
        match unsafe { runtime.native_passthrough_c_path(path, directory) } {
            Ok(Some(native)) => {
                return unsafe { original(libc::AT_FDCWD, native.as_ptr(), flags) };
            }
            Ok(None) => {}
            Err(error) => return unsafe { fail(&error, -1) },
        }
        let supported = flags == 0 || flags == libc::AT_REMOVEDIR;
        if !supported {
            unsafe { set_errno(libc::EINVAL) };
            return -1;
        }
        match runtime.remove(directory, path, flags == libc::AT_REMOVEDIR) {
            Ok(()) => 0,
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_unlinkat(
    directory: libc::c_int,
    path: *const libc::c_char,
    flags: libc::c_int,
) -> libc::c_int {
    unsafe { sandbox_unlinkat(directory, path, flags) }
}

overlay_mutation_hook!(
    sandbox_rmdir,
    agora_sandbox_rmdir,
    original_rmdir,
    (path: *const libc::c_char),
    (path),
    |runtime| runtime.remove(libc::AT_FDCWD, path, true)
);

overlay_mutation_hook!(
    sandbox_rename,
    agora_sandbox_rename,
    original_rename,
    (from: *const libc::c_char, to: *const libc::c_char),
    (from, to),
    |runtime| runtime.rename(libc::AT_FDCWD, from, libc::AT_FDCWD, to)
);

overlay_mutation_hook!(
    sandbox_renameat,
    agora_sandbox_renameat,
    original_renameat,
    (
        from_directory: libc::c_int,
        from: *const libc::c_char,
        to_directory: libc::c_int,
        to: *const libc::c_char,
    ),
    (from_directory, from, to_directory, to),
    |runtime| runtime.rename(from_directory, from, to_directory, to)
);

unsafe fn sandbox_renamex_np(
    from: *const libc::c_char,
    to: *const libc::c_char,
    flags: libc::c_uint,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_renamex_np() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(from, to, flags) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(from, to, flags) };
        };
        match unsafe { runtime.native_passthrough_pair(from, libc::AT_FDCWD, to, libc::AT_FDCWD) } {
            Ok(Some((from, to))) => {
                return unsafe { original(from.as_ptr(), to.as_ptr(), flags) };
            }
            Ok(None) => {}
            Err(error) => return unsafe { fail(&error, -1) },
        }
        if flags != 0 {
            unsafe { set_errno(libc::ENOTSUP) };
            return -1;
        }
        match runtime.rename(libc::AT_FDCWD, from, libc::AT_FDCWD, to) {
            Ok(()) => 0,
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_renamex_np(
    from: *const libc::c_char,
    to: *const libc::c_char,
    flags: libc::c_uint,
) -> libc::c_int {
    unsafe { sandbox_renamex_np(from, to, flags) }
}

unsafe fn sandbox_renameatx_np(
    from_directory: libc::c_int,
    from: *const libc::c_char,
    to_directory: libc::c_int,
    to: *const libc::c_char,
    flags: libc::c_uint,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_renameatx_np() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(from_directory, from, to_directory, to, flags) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(from_directory, from, to_directory, to, flags) };
        };
        match unsafe { runtime.native_passthrough_pair(from, from_directory, to, to_directory) } {
            Ok(Some((from, to))) => {
                return unsafe {
                    original(
                        libc::AT_FDCWD,
                        from.as_ptr(),
                        libc::AT_FDCWD,
                        to.as_ptr(),
                        flags,
                    )
                };
            }
            Ok(None) => {}
            Err(error) => return unsafe { fail(&error, -1) },
        }
        if flags != 0 {
            unsafe { set_errno(libc::ENOTSUP) };
            return -1;
        }
        match runtime.rename(from_directory, from, to_directory, to) {
            Ok(()) => 0,
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_renameatx_np(
    from_directory: libc::c_int,
    from: *const libc::c_char,
    to_directory: libc::c_int,
    to: *const libc::c_char,
    flags: libc::c_uint,
) -> libc::c_int {
    unsafe { sandbox_renameatx_np(from_directory, from, to_directory, to, flags) }
}

overlay_mutation_hook!(
    sandbox_mkdir,
    agora_sandbox_mkdir,
    original_mkdir,
    (path: *const libc::c_char, mode: libc::mode_t),
    (path, mode),
    |runtime| runtime.create_directory(libc::AT_FDCWD, path, mode)
);

overlay_mutation_hook!(
    sandbox_mkdirat,
    agora_sandbox_mkdirat,
    original_mkdirat,
    (
        directory: libc::c_int,
        path: *const libc::c_char,
        mode: libc::mode_t,
    ),
    (directory, path, mode),
    |runtime| runtime.create_directory(directory, path, mode)
);

pub(super) fn original_symlink() -> Option<SymlinkFn> {
    function_from_interpose(&INTERPOSE_SYMLINK)
}

fn original_symlinkat() -> Option<SymlinkAtFn> {
    function_from_interpose(&INTERPOSE_SYMLINKAT)
}

pub(super) fn original_unlink() -> Option<UnlinkFn> {
    function_from_interpose(&INTERPOSE_UNLINK)
}

fn original_unlinkat() -> Option<UnlinkAtFn> {
    function_from_interpose(&INTERPOSE_UNLINKAT)
}

pub(super) fn original_rmdir() -> Option<UnlinkFn> {
    function_from_interpose(&INTERPOSE_RMDIR)
}

pub(super) fn original_rename() -> Option<RenameFn> {
    function_from_interpose(&INTERPOSE_RENAME)
}

fn original_renameat() -> Option<RenameAtFn> {
    function_from_interpose(&INTERPOSE_RENAMEAT)
}

fn original_renamex_np() -> Option<RenameXFn> {
    function_from_interpose(&INTERPOSE_RENAMEX_NP)
}

fn original_renameatx_np() -> Option<RenameAtXFn> {
    function_from_interpose(&INTERPOSE_RENAMEATX_NP)
}

pub(super) fn original_mkdir() -> Option<MkdirFn> {
    function_from_interpose(&INTERPOSE_MKDIR)
}

fn original_mkdirat() -> Option<MkdirAtFn> {
    function_from_interpose(&INTERPOSE_MKDIRAT)
}

dyld_interpose!(INTERPOSE_SYMLINK, agora_sandbox_symlink, libc::symlink);

dyld_interpose!(
    INTERPOSE_SYMLINKAT,
    agora_sandbox_symlinkat,
    libc::symlinkat
);

dyld_interpose!(INTERPOSE_UNLINK, agora_sandbox_unlink, libc::unlink);

dyld_interpose!(INTERPOSE_UNLINKAT, agora_sandbox_unlinkat, libc::unlinkat);

dyld_interpose!(INTERPOSE_RMDIR, agora_sandbox_rmdir, libc::rmdir);

dyld_interpose!(INTERPOSE_RENAME, agora_sandbox_rename, libc::rename);

dyld_interpose!(INTERPOSE_RENAMEAT, agora_sandbox_renameat, libc::renameat);

dyld_interpose!(
    INTERPOSE_RENAMEX_NP,
    agora_sandbox_renamex_np,
    libc::renamex_np
);

dyld_interpose!(
    INTERPOSE_RENAMEATX_NP,
    agora_sandbox_renameatx_np,
    libc::renameatx_np
);

dyld_interpose!(INTERPOSE_MKDIR, agora_sandbox_mkdir, libc::mkdir);

dyld_interpose!(INTERPOSE_MKDIRAT, agora_sandbox_mkdirat, libc::mkdirat);
