use super::*;

type ForkFn = unsafe extern "C" fn() -> libc::pid_t;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fork() -> libc::pid_t {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_fork() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        unsafe { sandbox_fork_with(original) }
    })
}

pub(super) unsafe fn sandbox_fork_with(original: ForkFn) -> libc::pid_t {
    // The registered atfork handlers hold the filesystem barrier across the
    // native fork. Defer application signal handlers until the parent or child
    // handler has released/reset that barrier. Both processes restore the
    // caller's signal mask as soon as `fork` returns.
    let signals = super::super::SignalMaskGuard::block_or_abort();
    let retained = {
        let Some(_guard) = FilesystemHookGuard::enter() else {
            let result = unsafe { original() };
            drop(signals);
            return result;
        };
        match FilesystemHookRuntime::global() {
            Some(runtime) => match runtime.retain_local_files_before_fork() {
                Ok(handles) => handles,
                Err(error) => return unsafe { fail(&error, -1) },
            },
            None => Vec::new(),
        }
    };
    let result = unsafe { original() };
    drop(signals);
    if result < 0 && !retained.is_empty() {
        let errno = unsafe { *libc::__error() };
        if let Some(_guard) = FilesystemHookGuard::enter()
            && let Some(runtime) = FilesystemHookRuntime::global()
        {
            let _ = runtime.release_local_files_after_failed_fork(retained);
        }
        unsafe { set_errno(errno) };
    }
    result
}

fn original_fork() -> Option<ForkFn> {
    function_from_interpose(&INTERPOSE_FORK)
}

dyld_interpose!(INTERPOSE_FORK, agora_sandbox_fork, libc::fork);
