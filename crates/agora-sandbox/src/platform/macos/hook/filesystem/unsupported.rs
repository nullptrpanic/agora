use super::super::abi::{darwin_removefile, darwin_removefileat};
use super::*;

type UtimesFn = unsafe extern "C" fn(*const libc::c_char, *const libc::timeval) -> libc::c_int;
type FutimesFn = unsafe extern "C" fn(libc::c_int, *const libc::timeval) -> libc::c_int;
type UtimensAtFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::c_char,
    *const libc::timespec,
    libc::c_int,
) -> libc::c_int;
type FutimensFn = unsafe extern "C" fn(libc::c_int, *const libc::timespec) -> libc::c_int;
type ChflagsFn = unsafe extern "C" fn(*const libc::c_char, libc::c_uint) -> libc::c_int;
type FchflagsFn = unsafe extern "C" fn(libc::c_int, libc::c_uint) -> libc::c_int;
type SetxattrFn = unsafe extern "C" fn(
    *const libc::c_char,
    *const libc::c_char,
    *const libc::c_void,
    libc::size_t,
    u32,
    libc::c_int,
) -> libc::c_int;
type FsetxattrFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::c_char,
    *const libc::c_void,
    libc::size_t,
    u32,
    libc::c_int,
) -> libc::c_int;
type RemovexattrFn =
    unsafe extern "C" fn(*const libc::c_char, *const libc::c_char, libc::c_int) -> libc::c_int;
type FremovexattrFn =
    unsafe extern "C" fn(libc::c_int, *const libc::c_char, libc::c_int) -> libc::c_int;
type ChownFn = unsafe extern "C" fn(*const libc::c_char, libc::uid_t, libc::gid_t) -> libc::c_int;
type FchownFn = unsafe extern "C" fn(libc::c_int, libc::uid_t, libc::gid_t) -> libc::c_int;
type FchownAtFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::c_char,
    libc::uid_t,
    libc::gid_t,
    libc::c_int,
) -> libc::c_int;
type LinkFn = unsafe extern "C" fn(*const libc::c_char, *const libc::c_char) -> libc::c_int;
type LinkAtFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::c_char,
    libc::c_int,
    *const libc::c_char,
    libc::c_int,
) -> libc::c_int;
type ClonefileFn =
    unsafe extern "C" fn(*const libc::c_char, *const libc::c_char, u32) -> libc::c_int;
type ClonefileAtFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::c_char,
    libc::c_int,
    *const libc::c_char,
    u32,
) -> libc::c_int;
type CopyfileFn = unsafe extern "C" fn(
    *const libc::c_char,
    *const libc::c_char,
    libc::copyfile_state_t,
    libc::copyfile_flags_t,
) -> libc::c_int;
type RemovefileFn =
    unsafe extern "C" fn(*const libc::c_char, *mut libc::c_void, libc::c_uint) -> libc::c_int;
type RemovefileAtFn = unsafe extern "C" fn(
    libc::c_int,
    *const libc::c_char,
    *mut libc::c_void,
    libc::c_uint,
) -> libc::c_int;

pub(super) unsafe fn sandbox_unsupported_path_mutation(
    path: *const libc::c_char,
    directory: libc::c_int,
    operation: impl FnOnce(libc::c_int, *const libc::c_char) -> libc::c_int,
) -> libc::c_int {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return operation(directory, path);
    };
    let Some(runtime) = FilesystemHookRuntime::global() else {
        return operation(directory, path);
    };
    match unsafe { runtime.native_passthrough_c_path(path, directory) } {
        Ok(Some(native)) => operation(libc::AT_FDCWD, native.as_ptr()),
        Ok(None) => {
            unsafe { set_errno(libc::ENOTSUP) };
            -1
        }
        Err(error) => unsafe { fail(&error, -1) },
    }
}

unsafe fn sandbox_unsupported_descriptor_mutation(
    descriptor: libc::c_int,
    operation: impl FnOnce(libc::c_int) -> libc::c_int,
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
    unsafe { set_errno(libc::ENOTSUP) };
    -1
}

unsafe fn sandbox_unsupported_pair_mutation(
    first: *const libc::c_char,
    first_directory: libc::c_int,
    second: *const libc::c_char,
    second_directory: libc::c_int,
    operation: impl FnOnce(
        libc::c_int,
        *const libc::c_char,
        libc::c_int,
        *const libc::c_char,
    ) -> libc::c_int,
) -> libc::c_int {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return operation(first_directory, first, second_directory, second);
    };
    let Some(runtime) = FilesystemHookRuntime::global() else {
        return operation(first_directory, first, second_directory, second);
    };
    match unsafe {
        runtime.native_passthrough_pair(first, first_directory, second, second_directory)
    } {
        Ok(Some((first, second))) => operation(
            libc::AT_FDCWD,
            first.as_ptr(),
            libc::AT_FDCWD,
            second.as_ptr(),
        ),
        Ok(None) => {
            unsafe { set_errno(libc::ENOTSUP) };
            -1
        }
        Err(error) => unsafe { fail(&error, -1) },
    }
}

macro_rules! unsupported_path_filesystem_hook {
    (
        $sandbox:ident, $export:ident, $original:ident,
        ($($argument:ident: $argument_type:ty),* $(,)?),
        $path:ident, $directory:expr,
        |$original_fn:ident, $native_directory:ident, $native_path:ident| $native_call:expr
    ) => {
        unsafe fn $sandbox($($argument: $argument_type),*) -> libc::c_int {
            catch_filesystem_panic(-1, || match $original() {
                Some($original_fn) => unsafe {
                    sandbox_unsupported_path_mutation(
                        $path,
                        $directory,
                        |$native_directory, $native_path| $native_call,
                    )
                },
                None => unsafe {
                    set_errno(libc::ENOSYS);
                    -1
                },
            })
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $export($($argument: $argument_type),*) -> libc::c_int {
            unsafe { $sandbox($($argument),*) }
        }
    }
}

macro_rules! unsupported_descriptor_filesystem_hook {
    (
        $sandbox:ident, $export:ident, $original:ident,
        ($($argument:ident: $argument_type:ty),* $(,)?),
        $descriptor:ident,
        |$original_fn:ident, $native_descriptor:ident| $native_call:expr
    ) => {
        unsafe fn $sandbox($($argument: $argument_type),*) -> libc::c_int {
            catch_filesystem_panic(-1, || match $original() {
                Some($original_fn) => unsafe {
                    sandbox_unsupported_descriptor_mutation(
                        $descriptor,
                        |$native_descriptor| $native_call,
                    )
                },
                None => unsafe {
                    set_errno(libc::ENOSYS);
                    -1
                },
            })
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $export($($argument: $argument_type),*) -> libc::c_int {
            unsafe { $sandbox($($argument),*) }
        }
    }
}

macro_rules! unsupported_pair_filesystem_hook {
    (
        $sandbox:ident, $export:ident, $original:ident,
        ($($argument:ident: $argument_type:ty),* $(,)?),
        ($first:ident, $first_directory:expr),
        ($second:ident, $second_directory:expr),
        |$original_fn:ident,
         $native_first_directory:ident, $native_first:ident,
         $native_second_directory:ident, $native_second:ident| $native_call:expr
    ) => {
        unsafe fn $sandbox($($argument: $argument_type),*) -> libc::c_int {
            catch_filesystem_panic(-1, || match $original() {
                Some($original_fn) => unsafe {
                    sandbox_unsupported_pair_mutation(
                        $first,
                        $first_directory,
                        $second,
                        $second_directory,
                        |$native_first_directory,
                         $native_first,
                         $native_second_directory,
                         $native_second| $native_call,
                    )
                },
                None => unsafe {
                    set_errno(libc::ENOSYS);
                    -1
                },
            })
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $export($($argument: $argument_type),*) -> libc::c_int {
            unsafe { $sandbox($($argument),*) }
        }
    }
}

unsupported_path_filesystem_hook!(
    sandbox_removefile,
    agora_sandbox_removefile,
    original_removefile,
    (
        path: *const libc::c_char,
        state: *mut libc::c_void,
        flags: libc::c_uint,
    ),
    path,
    libc::AT_FDCWD,
    |original, _native_directory, native| original(native, state, flags)
);

unsupported_path_filesystem_hook!(
    sandbox_removefileat,
    agora_sandbox_removefileat,
    original_removefileat,
    (
        directory: libc::c_int,
        path: *const libc::c_char,
        state: *mut libc::c_void,
        flags: libc::c_uint,
    ),
    path,
    directory,
    |original, native_directory, native| original(native_directory, native, state, flags)
);

unsupported_path_filesystem_hook!(
    sandbox_utimes,
    agora_sandbox_utimes,
    original_utimes,
    (
        path: *const libc::c_char,
        times: *const libc::timeval,
    ),
    path,
    libc::AT_FDCWD,
    |original, _native_directory, native| original(native, times)
);

unsupported_path_filesystem_hook!(
    sandbox_lutimes,
    agora_sandbox_lutimes,
    original_lutimes,
    (
        path: *const libc::c_char,
        times: *const libc::timeval,
    ),
    path,
    libc::AT_FDCWD,
    |original, _native_directory, native| original(native, times)
);

unsupported_descriptor_filesystem_hook!(
    sandbox_futimes,
    agora_sandbox_futimes,
    original_futimes,
    (
        descriptor: libc::c_int,
        times: *const libc::timeval,
    ),
    descriptor,
    |original, native| original(native, times)
);

unsupported_descriptor_filesystem_hook!(
    sandbox_futimens,
    agora_sandbox_futimens,
    original_futimens,
    (
        descriptor: libc::c_int,
        times: *const libc::timespec,
    ),
    descriptor,
    |original, native| original(native, times)
);

unsupported_path_filesystem_hook!(
    sandbox_utimensat,
    agora_sandbox_utimensat,
    original_utimensat,
    (
        directory: libc::c_int,
        path: *const libc::c_char,
        times: *const libc::timespec,
        flags: libc::c_int,
    ),
    path,
    directory,
    |original, native_directory, native| original(native_directory, native, times, flags)
);

unsupported_path_filesystem_hook!(
    sandbox_chflags,
    agora_sandbox_chflags,
    original_chflags,
    (
        path: *const libc::c_char,
        flags: libc::c_uint,
    ),
    path,
    libc::AT_FDCWD,
    |original, _native_directory, native| original(native, flags)
);

unsupported_descriptor_filesystem_hook!(
    sandbox_fchflags,
    agora_sandbox_fchflags,
    original_fchflags,
    (
        descriptor: libc::c_int,
        flags: libc::c_uint,
    ),
    descriptor,
    |original, native| original(native, flags)
);

unsupported_path_filesystem_hook!(
    sandbox_setxattr,
    agora_sandbox_setxattr,
    original_setxattr,
    (
        path: *const libc::c_char,
        name: *const libc::c_char,
        value: *const libc::c_void,
        size: libc::size_t,
        position: u32,
        flags: libc::c_int,
    ),
    path,
    libc::AT_FDCWD,
    |original, _native_directory, native| original(native, name, value, size, position, flags)
);

unsupported_descriptor_filesystem_hook!(
    sandbox_fsetxattr,
    agora_sandbox_fsetxattr,
    original_fsetxattr,
    (
        descriptor: libc::c_int,
        name: *const libc::c_char,
        value: *const libc::c_void,
        size: libc::size_t,
        position: u32,
        flags: libc::c_int,
    ),
    descriptor,
    |original, native| original(native, name, value, size, position, flags)
);

unsupported_path_filesystem_hook!(
    sandbox_removexattr,
    agora_sandbox_removexattr,
    original_removexattr,
    (
        path: *const libc::c_char,
        name: *const libc::c_char,
        flags: libc::c_int,
    ),
    path,
    libc::AT_FDCWD,
    |original, _native_directory, native| original(native, name, flags)
);

unsupported_descriptor_filesystem_hook!(
    sandbox_fremovexattr,
    agora_sandbox_fremovexattr,
    original_fremovexattr,
    (
        descriptor: libc::c_int,
        name: *const libc::c_char,
        flags: libc::c_int,
    ),
    descriptor,
    |original, native| original(native, name, flags)
);

unsupported_path_filesystem_hook!(
    sandbox_chown,
    agora_sandbox_chown,
    original_chown,
    (
        path: *const libc::c_char,
        owner: libc::uid_t,
        group: libc::gid_t,
    ),
    path,
    libc::AT_FDCWD,
    |original, _native_directory, native| original(native, owner, group)
);

unsupported_descriptor_filesystem_hook!(
    sandbox_fchown,
    agora_sandbox_fchown,
    original_fchown,
    (
        descriptor: libc::c_int,
        owner: libc::uid_t,
        group: libc::gid_t,
    ),
    descriptor,
    |original, native| original(native, owner, group)
);

unsupported_path_filesystem_hook!(
    sandbox_lchown,
    agora_sandbox_lchown,
    original_lchown,
    (
        path: *const libc::c_char,
        owner: libc::uid_t,
        group: libc::gid_t,
    ),
    path,
    libc::AT_FDCWD,
    |original, _native_directory, native| original(native, owner, group)
);

unsupported_path_filesystem_hook!(
    sandbox_fchownat,
    agora_sandbox_fchownat,
    original_fchownat,
    (
        directory: libc::c_int,
        path: *const libc::c_char,
        owner: libc::uid_t,
        group: libc::gid_t,
        flags: libc::c_int,
    ),
    path,
    directory,
    |original, native_directory, native| original(native_directory, native, owner, group, flags)
);

unsupported_pair_filesystem_hook!(
    sandbox_link,
    agora_sandbox_link,
    original_link,
    (
        source: *const libc::c_char,
        destination: *const libc::c_char,
    ),
    (source, libc::AT_FDCWD),
    (destination, libc::AT_FDCWD),
    |original,
     _native_source_directory,
     native_source,
     _native_destination_directory,
     native_destination| original(native_source, native_destination)
);

unsupported_pair_filesystem_hook!(
    sandbox_linkat,
    agora_sandbox_linkat,
    original_linkat,
    (
        source_directory: libc::c_int,
        source: *const libc::c_char,
        destination_directory: libc::c_int,
        destination: *const libc::c_char,
        flags: libc::c_int,
    ),
    (source, source_directory),
    (destination, destination_directory),
    |original,
     native_source_directory,
     native_source,
     native_destination_directory,
     native_destination| original(
        native_source_directory,
        native_source,
        native_destination_directory,
        native_destination,
        flags,
    )
);

unsupported_pair_filesystem_hook!(
    sandbox_clonefile,
    agora_sandbox_clonefile,
    original_clonefile,
    (
        source: *const libc::c_char,
        destination: *const libc::c_char,
        flags: u32,
    ),
    (source, libc::AT_FDCWD),
    (destination, libc::AT_FDCWD),
    |original,
     _native_source_directory,
     native_source,
     _native_destination_directory,
     native_destination| original(
        native_source,
        native_destination,
        flags,
    )
);

unsupported_pair_filesystem_hook!(
    sandbox_clonefileat,
    agora_sandbox_clonefileat,
    original_clonefileat,
    (
        source_directory: libc::c_int,
        source: *const libc::c_char,
        destination_directory: libc::c_int,
        destination: *const libc::c_char,
        flags: u32,
    ),
    (source, source_directory),
    (destination, destination_directory),
    |original,
     native_source_directory,
     native_source,
     native_destination_directory,
     native_destination| original(
        native_source_directory,
        native_source,
        native_destination_directory,
        native_destination,
        flags,
    )
);

unsupported_pair_filesystem_hook!(
    sandbox_copyfile,
    agora_sandbox_copyfile,
    original_copyfile,
    (
        source: *const libc::c_char,
        destination: *const libc::c_char,
        state: libc::copyfile_state_t,
        flags: libc::copyfile_flags_t,
    ),
    (source, libc::AT_FDCWD),
    (destination, libc::AT_FDCWD),
    |original,
     _native_source_directory,
     native_source,
     _native_destination_directory,
     native_destination| original(
        native_source,
        native_destination,
        state,
        flags,
    )
);

fn original_utimes() -> Option<UtimesFn> {
    function_from_interpose(&INTERPOSE_UTIMES)
}

fn original_lutimes() -> Option<UtimesFn> {
    function_from_interpose(&INTERPOSE_LUTIMES)
}

fn original_futimes() -> Option<FutimesFn> {
    function_from_interpose(&INTERPOSE_FUTIMES)
}

fn original_futimens() -> Option<FutimensFn> {
    function_from_interpose(&INTERPOSE_FUTIMENS)
}

fn original_utimensat() -> Option<UtimensAtFn> {
    function_from_interpose(&INTERPOSE_UTIMENSAT)
}

fn original_chflags() -> Option<ChflagsFn> {
    function_from_interpose(&INTERPOSE_CHFLAGS)
}

fn original_fchflags() -> Option<FchflagsFn> {
    function_from_interpose(&INTERPOSE_FCHFLAGS)
}

fn original_setxattr() -> Option<SetxattrFn> {
    function_from_interpose(&INTERPOSE_SETXATTR)
}

fn original_fsetxattr() -> Option<FsetxattrFn> {
    function_from_interpose(&INTERPOSE_FSETXATTR)
}

fn original_removexattr() -> Option<RemovexattrFn> {
    function_from_interpose(&INTERPOSE_REMOVEXATTR)
}

fn original_fremovexattr() -> Option<FremovexattrFn> {
    function_from_interpose(&INTERPOSE_FREMOVEXATTR)
}

fn original_chown() -> Option<ChownFn> {
    function_from_interpose(&INTERPOSE_CHOWN)
}

fn original_fchown() -> Option<FchownFn> {
    function_from_interpose(&INTERPOSE_FCHOWN)
}

fn original_lchown() -> Option<ChownFn> {
    function_from_interpose(&INTERPOSE_LCHOWN)
}

fn original_fchownat() -> Option<FchownAtFn> {
    function_from_interpose(&INTERPOSE_FCHOWNAT)
}

fn original_link() -> Option<LinkFn> {
    function_from_interpose(&INTERPOSE_LINK)
}

fn original_linkat() -> Option<LinkAtFn> {
    function_from_interpose(&INTERPOSE_LINKAT)
}

fn original_clonefile() -> Option<ClonefileFn> {
    function_from_interpose(&INTERPOSE_CLONEFILE)
}

fn original_clonefileat() -> Option<ClonefileAtFn> {
    function_from_interpose(&INTERPOSE_CLONEFILEAT)
}

fn original_copyfile() -> Option<CopyfileFn> {
    function_from_interpose(&INTERPOSE_COPYFILE)
}

fn original_removefile() -> Option<RemovefileFn> {
    function_from_interpose(&INTERPOSE_REMOVEFILE)
}

fn original_removefileat() -> Option<RemovefileAtFn> {
    function_from_interpose(&INTERPOSE_REMOVEFILEAT)
}

dyld_interpose!(
    INTERPOSE_REMOVEFILE,
    agora_sandbox_removefile,
    darwin_removefile
);

dyld_interpose!(
    INTERPOSE_REMOVEFILEAT,
    agora_sandbox_removefileat,
    darwin_removefileat
);

dyld_interpose!(INTERPOSE_UTIMES, agora_sandbox_utimes, libc::utimes);

dyld_interpose!(INTERPOSE_LUTIMES, agora_sandbox_lutimes, libc::lutimes);

dyld_interpose!(INTERPOSE_FUTIMES, agora_sandbox_futimes, libc::futimes);

dyld_interpose!(INTERPOSE_FUTIMENS, agora_sandbox_futimens, libc::futimens);

dyld_interpose!(
    INTERPOSE_UTIMENSAT,
    agora_sandbox_utimensat,
    libc::utimensat
);

dyld_interpose!(INTERPOSE_CHFLAGS, agora_sandbox_chflags, libc::chflags);

dyld_interpose!(INTERPOSE_FCHFLAGS, agora_sandbox_fchflags, libc::fchflags);

dyld_interpose!(INTERPOSE_SETXATTR, agora_sandbox_setxattr, libc::setxattr);

dyld_interpose!(
    INTERPOSE_FSETXATTR,
    agora_sandbox_fsetxattr,
    libc::fsetxattr
);

dyld_interpose!(
    INTERPOSE_REMOVEXATTR,
    agora_sandbox_removexattr,
    libc::removexattr
);

dyld_interpose!(
    INTERPOSE_FREMOVEXATTR,
    agora_sandbox_fremovexattr,
    libc::fremovexattr
);

dyld_interpose!(INTERPOSE_CHOWN, agora_sandbox_chown, libc::chown);

dyld_interpose!(INTERPOSE_FCHOWN, agora_sandbox_fchown, libc::fchown);

dyld_interpose!(INTERPOSE_LCHOWN, agora_sandbox_lchown, libc::lchown);

dyld_interpose!(INTERPOSE_FCHOWNAT, agora_sandbox_fchownat, libc::fchownat);

dyld_interpose!(INTERPOSE_LINK, agora_sandbox_link, libc::link);

dyld_interpose!(INTERPOSE_LINKAT, agora_sandbox_linkat, libc::linkat);

dyld_interpose!(
    INTERPOSE_CLONEFILE,
    agora_sandbox_clonefile,
    libc::clonefile
);

dyld_interpose!(
    INTERPOSE_CLONEFILEAT,
    agora_sandbox_clonefileat,
    libc::clonefileat
);

dyld_interpose!(INTERPOSE_COPYFILE, agora_sandbox_copyfile, libc::copyfile);

#[cfg(test)]
mod tests;
