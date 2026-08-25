use std::io;
use std::marker::PhantomData;
use std::rc::Rc;

unsafe extern "C" {
    fn pthread_setcancelstate(state: libc::c_int, old_state: *mut libc::c_int) -> libc::c_int;
}

/// Defers catchable asynchronous signals and pthread cancellation while
/// Rust-owned hook state is live.
///
/// Owners must release their state in their own `Drop` implementation. Rust
/// drops this field afterwards, so restoring a pending signal cannot bypass
/// cleanup through an application `siglongjmp` handler.
#[must_use]
pub(crate) struct SignalMaskGuard {
    previous: libc::sigset_t,
    previous_cancel_state: libc::c_int,
    _thread_bound: PhantomData<Rc<()>>,
}

impl SignalMaskGuard {
    pub(crate) fn block() -> io::Result<Self> {
        let mut blocked = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        let mut previous = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        unsafe {
            if libc::sigfillset(blocked.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            for signal in [
                libc::SIGKILL,
                libc::SIGSTOP,
                libc::SIGILL,
                libc::SIGTRAP,
                libc::SIGABRT,
                libc::SIGEMT,
                libc::SIGFPE,
                libc::SIGBUS,
                libc::SIGSEGV,
                libc::SIGSYS,
            ] {
                libc::sigdelset(blocked.as_mut_ptr(), signal);
            }
            let mut previous_cancel_state = 0;
            let result = pthread_setcancelstate(
                libc::PTHREAD_CANCEL_DISABLE,
                &raw mut previous_cancel_state,
            );
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result));
            }
            let result =
                libc::pthread_sigmask(libc::SIG_BLOCK, blocked.as_ptr(), previous.as_mut_ptr());
            if result != 0 {
                if pthread_setcancelstate(previous_cancel_state, std::ptr::null_mut()) != 0 {
                    libc::abort();
                }
                return Err(io::Error::from_raw_os_error(result));
            }
            Ok(Self {
                previous: previous.assume_init(),
                previous_cancel_state,
                _thread_bound: PhantomData,
            })
        }
    }

    pub(crate) fn block_or_abort() -> Self {
        Self::block().unwrap_or_else(|_| unsafe { libc::abort() })
    }
}

impl Drop for SignalMaskGuard {
    fn drop(&mut self) {
        let result =
            unsafe { pthread_setcancelstate(self.previous_cancel_state, std::ptr::null_mut()) };
        if result != 0 {
            unsafe { libc::abort() };
        }
        let result = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut())
        };
        if result != 0 {
            unsafe { libc::abort() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SignalMaskGuard, pthread_setcancelstate};

    #[test]
    fn guard_disables_and_restores_pthread_cancellation() {
        let mut initial = 0;
        assert_eq!(
            unsafe { pthread_setcancelstate(libc::PTHREAD_CANCEL_DISABLE, &mut initial) },
            0
        );
        assert_eq!(
            unsafe { pthread_setcancelstate(initial, std::ptr::null_mut()) },
            0
        );
        assert_eq!(initial, libc::PTHREAD_CANCEL_ENABLE);

        let guard = SignalMaskGuard::block().unwrap();
        let mut during = 0;
        assert_eq!(
            unsafe { pthread_setcancelstate(libc::PTHREAD_CANCEL_DISABLE, &mut during) },
            0
        );
        assert_eq!(during, libc::PTHREAD_CANCEL_DISABLE);
        drop(guard);

        let mut restored = 0;
        assert_eq!(
            unsafe { pthread_setcancelstate(libc::PTHREAD_CANCEL_DISABLE, &mut restored) },
            0
        );
        assert_eq!(restored, initial);
        assert_eq!(
            unsafe { pthread_setcancelstate(restored, std::ptr::null_mut()) },
            0
        );
    }
}
