use std::io;
use std::marker::PhantomData;
use std::rc::Rc;

/// Defers catchable asynchronous signals while Rust-owned hook state is live.
///
/// Owners must release their state in their own `Drop` implementation. Rust
/// drops this field afterwards, so restoring a pending signal cannot bypass
/// cleanup through an application `siglongjmp` handler.
#[must_use]
pub(crate) struct SignalMaskGuard {
    previous: libc::sigset_t,
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
            let result =
                libc::pthread_sigmask(libc::SIG_BLOCK, blocked.as_ptr(), previous.as_mut_ptr());
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result));
            }
            Ok(Self {
                previous: previous.assume_init(),
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
        let result = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut())
        };
        if result != 0 {
            unsafe { libc::abort() };
        }
    }
}
