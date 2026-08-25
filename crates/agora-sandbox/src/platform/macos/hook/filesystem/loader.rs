use super::super::abi::darwin_dlopen_preflight;
use super::*;
use std::cell::RefCell;
use std::ffi::c_void;

type DlopenFn = unsafe extern "C" fn(*const libc::c_char, libc::c_int) -> *mut c_void;
type DlopenPreflightFn = unsafe extern "C" fn(*const libc::c_char) -> bool;
type DlerrorFn = unsafe extern "C" fn() -> *mut libc::c_char;

#[derive(Default)]
struct LoaderErrorState {
    pending: Option<CString>,
    returned: Option<CString>,
}

thread_local! {
    static LOADER_ERROR: RefCell<LoaderErrorState> = RefCell::new(LoaderErrorState::default());
}

fn clear_custom_error() {
    LOADER_ERROR.with(|state| state.borrow_mut().pending = None);
}

fn set_custom_error(error: &anyhow::Error) {
    if let Some(original) = original_dlerror() {
        unsafe { original() };
    }
    let message = CString::new(format!("agora sandbox loader: {error:#}"))
        .unwrap_or_else(|_| c"agora sandbox loader error".to_owned());
    LOADER_ERROR.with(|state| {
        let mut state = state.borrow_mut();
        state.pending = Some(message);
        state.returned = None;
    });
    unsafe { set_errno(error_errno(error)) };
}

fn set_panic_error() {
    let error = io::Error::from_raw_os_error(libc::EIO);
    set_custom_error(&error.into());
}

fn take_custom_error() -> Option<*mut libc::c_char> {
    LOADER_ERROR.with(|state| {
        let mut state = state.borrow_mut();
        let pending = state.pending.take()?;
        state.returned = Some(pending);
        state
            .returned
            .as_ref()
            .map(|message| message.as_ptr().cast_mut())
    })
}

unsafe fn sandbox_dlopen_with(
    path: *const libc::c_char,
    mode: libc::c_int,
    original: DlopenFn,
) -> *mut c_void {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(path, mode) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            drop(guard);
            return unsafe { original(path, mode) };
        };
        if path.is_null() {
            clear_custom_error();
            drop(guard);
            return unsafe { original(path, mode) };
        }
        let requested = Path::new(OsStr::from_bytes(
            unsafe { CStr::from_ptr(path) }.to_bytes(),
        ));
        match runtime.prepare_loader_path(requested) {
            Ok(Some(mapped)) => {
                clear_custom_error();
                drop(guard);
                unsafe { original(mapped.as_ptr(), mode) }
            }
            Ok(None) => {
                clear_custom_error();
                drop(guard);
                unsafe { original(path, mode) }
            }
            Err(error) => {
                set_custom_error(&error);
                std::ptr::null_mut()
            }
        }
    }))
    .unwrap_or_else(|_| {
        set_panic_error();
        std::ptr::null_mut()
    })
}

unsafe fn sandbox_dlopen_preflight_with(
    path: *const libc::c_char,
    original: DlopenPreflightFn,
) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(path) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            drop(guard);
            return unsafe { original(path) };
        };
        if path.is_null() {
            clear_custom_error();
            drop(guard);
            return unsafe { original(path) };
        }
        let requested = Path::new(OsStr::from_bytes(
            unsafe { CStr::from_ptr(path) }.to_bytes(),
        ));
        match runtime.prepare_loader_path(requested) {
            Ok(Some(mapped)) => {
                clear_custom_error();
                drop(guard);
                unsafe { original(mapped.as_ptr()) }
            }
            Ok(None) => {
                clear_custom_error();
                drop(guard);
                unsafe { original(path) }
            }
            Err(error) => {
                set_custom_error(&error);
                false
            }
        }
    }))
    .unwrap_or_else(|_| {
        set_panic_error();
        false
    })
}

unsafe fn sandbox_dlerror_with(original: DlerrorFn) -> *mut libc::c_char {
    let Some(guard) = FilesystemHookGuard::enter() else {
        return unsafe { original() };
    };
    if let Some(error) = take_custom_error() {
        return error;
    }
    drop(guard);
    unsafe { original() }
}

fn original_dlopen() -> Option<DlopenFn> {
    function_from_interpose(&INTERPOSE_DLOPEN)
}

fn original_dlopen_preflight() -> Option<DlopenPreflightFn> {
    function_from_interpose(&INTERPOSE_DLOPEN_PREFLIGHT)
}

fn original_dlerror() -> Option<DlerrorFn> {
    function_from_interpose(&INTERPOSE_DLERROR)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_dlopen(
    path: *const libc::c_char,
    mode: libc::c_int,
) -> *mut c_void {
    let Some(original) = original_dlopen() else {
        unsafe { set_errno(libc::ENOSYS) };
        return std::ptr::null_mut();
    };
    unsafe { sandbox_dlopen_with(path, mode, original) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_dlopen_preflight(path: *const libc::c_char) -> bool {
    let Some(original) = original_dlopen_preflight() else {
        unsafe { set_errno(libc::ENOSYS) };
        return false;
    };
    unsafe { sandbox_dlopen_preflight_with(path, original) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_dlerror() -> *mut libc::c_char {
    let Some(original) = original_dlerror() else {
        unsafe { set_errno(libc::ENOSYS) };
        return std::ptr::null_mut();
    };
    unsafe { sandbox_dlerror_with(original) }
}

dyld_interpose!(INTERPOSE_DLOPEN, agora_sandbox_dlopen, libc::dlopen);
dyld_interpose!(
    INTERPOSE_DLOPEN_PREFLIGHT,
    agora_sandbox_dlopen_preflight,
    darwin_dlopen_preflight
);
dyld_interpose!(INTERPOSE_DLERROR, agora_sandbox_dlerror, libc::dlerror);

#[cfg(test)]
mod tests;
