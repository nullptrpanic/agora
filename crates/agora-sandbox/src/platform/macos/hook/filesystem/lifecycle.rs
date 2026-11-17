use super::*;

type ForkFn = unsafe extern "C" fn() -> libc::pid_t;

#[derive(Default)]
struct ForkRetain {
    handles: Vec<String>,
    error: i32,
}

thread_local! {
    static FORK_RETAIN: Cell<*mut ForkRetain> = const { Cell::new(std::ptr::null_mut()) };
}

// Called with the write barrier held, after later-registered application
// prepare handlers and all other filesystem hooks have finished.
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
pub(super) fn retain_at_fork_boundary() {
    FORK_RETAIN.with(|slot| {
        let Some(retained) = (unsafe { slot.get().as_mut() }) else {
            return;
        };
        if let Some(runtime) = FilesystemHookRuntime::global() {
            match runtime.retain_local_files_before_fork() {
                Ok(handles) => retained.handles = handles,
                Err(error) => retained.error = error_errno(&error),
            }
        }
    });
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
pub(super) fn fork_retain_failed() -> bool {
    FORK_RETAIN.with(|slot| unsafe { slot.get().as_ref() }.is_some_and(|state| state.error != 0))
}

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
    {
        let Some(_guard) = FilesystemHookGuard::enter() else {
            let result = unsafe { original() };
            drop(signals);
            return result;
        };
    }
    let mut retained = ForkRetain::default();
    let previous = FORK_RETAIN.with(|slot| slot.replace(&mut retained));
    let result = unsafe { original() };
    let errno = unsafe { *libc::__error() };
    FORK_RETAIN.with(|slot| slot.set(previous));
    if retained.error != 0 {
        // pthread_atfork cannot veto native fork. The child handler exits before
        // application callbacks; reap that child before exposing failure.
        if result > 0 {
            while unsafe { libc::waitpid(result, std::ptr::null_mut(), 0) } < 0
                && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
            {}
        }
        let error = retained.error;
        drop(retained);
        drop(signals);
        unsafe { set_errno(error) };
        return -1;
    }
    if result < 0
        && !retained.handles.is_empty()
        && let Some(_guard) = FilesystemHookGuard::enter()
        && let Some(runtime) = FilesystemHookRuntime::global()
    {
        let _ =
            runtime.release_local_files_after_failed_fork(std::mem::take(&mut retained.handles));
    }
    drop(retained);
    drop(signals);
    unsafe { set_errno(errno) };
    result
}

fn original_fork() -> Option<ForkFn> {
    function_from_interpose(&INTERPOSE_FORK)
}

dyld_interpose!(INTERPOSE_FORK, agora_sandbox_fork, libc::fork);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::hook::tests::SignalMaskProbe;

    thread_local! {
        static FAILED_CHILD: Cell<i32> = const { Cell::new(-1) };
    }

    unsafe extern "C" fn fork_with_failed_retain() -> i32 {
        FORK_RETAIN.with(|slot| unsafe { (*slot.get()).error = libc::EIO });
        let child = unsafe { libc::fork() };
        if child == 0 {
            unsafe { reset_filesystem_after_fork() };
            unsafe { libc::_exit(99) };
        }
        FAILED_CHILD.with(|slot| slot.set(child));
        child
    }

    #[test]
    fn failed_prepare_retain_reaps_child_and_restores_thread_state() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FilesystemHookRuntime::new(directory.path()).unwrap();
        with_test_runtime(&runtime, || {
            let signal = SignalMaskProbe::unblocked(libc::SIGUSR2);
            assert_eq!(unsafe { sandbox_fork_with(fork_with_failed_retain) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EIO));
            assert!(!signal.is_blocked());
            assert!(FORK_RETAIN.with(|slot| slot.get().is_null()));
            let child = FAILED_CHILD.with(Cell::get);
            assert!(child > 0);
            assert_eq!(
                unsafe { libc::waitpid(child, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        });
    }
}
