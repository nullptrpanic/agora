mod abi;
mod config;
mod control;
mod dyld;
mod filesystem;
mod network;
mod process;
mod signal;

pub(crate) use signal::SignalMaskGuard;

use std::path::{Path, PathBuf};
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
use std::sync::Once;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static HOOK_INITIALIZED: AtomicBool = AtomicBool::new(false);
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
static EXIT_FLUSH_REGISTERED: Once = Once::new();

fn initialized() -> bool {
    HOOK_INITIALIZED.load(Ordering::Acquire)
}

fn logical_process_executable_path(filesystem_root: &Path, executable: &Path) -> PathBuf {
    let executable =
        crate::filesystem::normalize_path(executable).unwrap_or_else(|_| executable.to_path_buf());
    let filesystem_root = crate::filesystem::normalize_path(filesystem_root)
        .unwrap_or_else(|_| filesystem_root.to_path_buf());
    executable
        .strip_prefix(&filesystem_root)
        .map(|relative| Path::new("/").join(relative))
        .unwrap_or(executable)
}

fn try_current_process_executable() -> std::io::Result<String> {
    static EXECUTABLE: OnceLock<String> = OnceLock::new();
    if let Some(executable) = EXECUTABLE.get() {
        return Ok(executable.clone());
    }
    let executable = std::env::current_exe()?;
    let executable = config::global().map_or(executable.clone(), |config| {
        logical_process_executable_path(Path::new(config.filesystem_root()), &executable)
    });
    let executable = executable.to_string_lossy().into_owned();
    let _ = EXECUTABLE.set(executable.clone());
    Ok(EXECUTABLE.get().cloned().unwrap_or(executable))
}

fn current_process_executable() -> String {
    try_current_process_executable().unwrap_or_default()
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
extern "C" fn flush_filesystem_at_exit() {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(filesystem::flush_at_exit)).is_err() {
        const MESSAGE: &[u8] = b"agora-sandbox: filesystem exit flush panicked\n";
        unsafe {
            libc::write(libc::STDERR_FILENO, MESSAGE.as_ptr().cast(), MESSAGE.len());
            libc::abort();
        }
    }
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
        .and_then(|()| process::publish_pending_event().map_err(anyhow::Error::msg))
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
