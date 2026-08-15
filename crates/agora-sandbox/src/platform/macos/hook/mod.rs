mod abi;
mod config;
mod control;
mod dyld;
mod filesystem;
mod network;
mod process;
mod signal;

pub(crate) use signal::SignalMaskGuard;

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

static HOOK_INITIALIZED: AtomicBool = AtomicBool::new(false);
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
static EXIT_FLUSH_REGISTERED: Once = Once::new();

fn initialized() -> bool {
    HOOK_INITIALIZED.load(Ordering::Acquire)
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
extern "C" fn flush_filesystem_at_exit() {
    filesystem::flush_at_exit();
}

#[cfg(all(coverage, not(agora_sandbox_hook_build), not(test)))]
fn is_coverage_hook_image() -> bool {
    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
    let found = unsafe {
        libc::dladdr(
            initialize_hook as *const () as *const libc::c_void,
            info.as_mut_ptr(),
        )
    };
    if found == 0 {
        return false;
    }
    let info = unsafe { info.assume_init() };
    if info.dli_fname.is_null() {
        return false;
    }
    unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }
        .to_bytes()
        .ends_with(b".dylib")
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
extern "C" fn initialize_hook() {
    #[cfg(all(coverage, not(agora_sandbox_hook_build), not(test)))]
    if !is_coverage_hook_image() {
        return;
    }
    let initialized = config::initialize()
        .map_err(anyhow::Error::msg)
        .and_then(|()| control::initialize())
        .and_then(|()| filesystem::initialize_process());
    if let Err(error) = initialized {
        let message = format!("agora-sandbox: {error:#}\n");
        unsafe {
            libc::write(libc::STDERR_FILENO, message.as_ptr().cast(), message.len());
            libc::_exit(126);
        }
    }
    EXIT_FLUSH_REGISTERED.call_once(|| unsafe {
        libc::atexit(flush_filesystem_at_exit);
    });
    HOOK_INITIALIZED.store(true, Ordering::Release);
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
#[used]
#[unsafe(link_section = "__DATA,__mod_init_func")]
static HOOK_INITIALIZER: extern "C" fn() = initialize_hook;

#[cfg(target_os = "macos")]
unsafe fn set_errno(value: libc::c_int) {
    unsafe { *libc::__error() = value };
}

#[cfg(test)]
pub(crate) mod tests;
