use super::{sandbox_dlerror_with, sandbox_dlopen_preflight_with, sandbox_dlopen_with};
use crate::platform::hook::filesystem::{
    FilesystemHookGuard, FilesystemHookRuntime, with_test_runtime,
};
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

thread_local! {
    static LAST_PATH: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    static LAST_MODE: Cell<libc::c_int> = const { Cell::new(0) };
    static ORIGINAL_ENTERED_FILESYSTEM: Cell<bool> = const { Cell::new(false) };
}

struct Fixture {
    directory: PathBuf,
    lower: PathBuf,
    runtime: FilesystemHookRuntime,
}

impl Fixture {
    fn plain() -> Self {
        Self::new(false)
    }

    fn encrypted() -> Self {
        Self::new(true)
    }

    fn new(encrypted: bool) -> Self {
        let directory =
            std::env::temp_dir().join(format!("agora-filesystem-loader-{}", uuid::Uuid::new_v4()));
        let lower = directory.join("lower");
        std::fs::create_dir_all(&lower).unwrap();
        let root = directory.join("workdir/fs");
        let runtime = if encrypted {
            FilesystemHookRuntime::new_encrypted(root, b"loader-test-key", b"0123456789abcdef")
                .unwrap()
        } else {
            FilesystemHookRuntime::new(root).unwrap()
        };
        Self {
            directory,
            lower,
            runtime,
        }
    }

    fn backing(&self, logical: &Path) -> CString {
        CString::new(
            self.runtime
                .filesystem
                .root()
                .join(logical.strip_prefix(Path::new("/")).unwrap())
                .as_os_str()
                .as_encoded_bytes(),
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}

unsafe extern "C" fn record_dlopen(path: *const libc::c_char, mode: libc::c_int) -> *mut c_void {
    LAST_PATH.with(|slot| {
        slot.replace((!path.is_null()).then(|| unsafe { CStr::from_ptr(path) }.to_bytes().to_vec()))
    });
    LAST_MODE.with(|slot| slot.set(mode));
    std::ptr::NonNull::<u8>::dangling().as_ptr().cast()
}

unsafe extern "C" fn record_preflight(path: *const libc::c_char) -> bool {
    LAST_PATH.with(|slot| {
        slot.replace((!path.is_null()).then(|| unsafe { CStr::from_ptr(path) }.to_bytes().to_vec()))
    });
    true
}

unsafe extern "C" fn record_dlopen_with_fresh_filesystem_guard(
    path: *const libc::c_char,
    mode: libc::c_int,
) -> *mut c_void {
    let guard = FilesystemHookGuard::enter();
    ORIGINAL_ENTERED_FILESYSTEM.with(|entered| entered.set(guard.is_some()));
    drop(guard);
    unsafe { record_dlopen(path, mode) }
}

unsafe extern "C" fn no_native_loader_error() -> *mut libc::c_char {
    std::ptr::null_mut()
}

unsafe extern "C" fn native_loader_error() -> *mut libc::c_char {
    c"native loader error".as_ptr().cast_mut()
}

fn recorded_path() -> Option<PathBuf> {
    LAST_PATH.with(|slot| {
        slot.borrow()
            .as_deref()
            .map(|bytes| PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    })
}

fn clear_recording() {
    LAST_PATH.with(|slot| slot.borrow_mut().take());
    LAST_MODE.with(|slot| slot.set(0));
    unsafe {
        sandbox_dlerror_with(no_native_loader_error);
        sandbox_dlerror_with(no_native_loader_error);
    }
}

#[test]
fn null_and_ordinary_loader_paths_delegate_unchanged() {
    let fixture = Fixture::plain();
    clear_recording();

    with_test_runtime(&fixture.runtime, || unsafe {
        let handle = sandbox_dlopen_with(std::ptr::null(), libc::RTLD_NOW, record_dlopen);
        assert!(!handle.is_null());
        assert_eq!(recorded_path(), None);

        let ordinary = c"/usr/lib/libSystem.B.dylib";
        let handle = sandbox_dlopen_with(ordinary.as_ptr(), libc::RTLD_LAZY, record_dlopen);
        assert!(!handle.is_null());
        assert_eq!(
            recorded_path().as_deref(),
            Some(Path::new(ordinary.to_str().unwrap()))
        );
        assert_eq!(LAST_MODE.with(Cell::get), libc::RTLD_LAZY);
    });
}

#[test]
fn loader_translates_lower_and_plain_upper_backing_aliases() {
    let fixture = Fixture::plain();
    let lower = fixture
        .lower
        .join("Relocated.app/Contents/PlugIns/lower.dylib");
    std::fs::create_dir_all(lower.parent().unwrap()).unwrap();
    std::fs::write(&lower, b"lower").unwrap();
    let lower_alias = fixture.backing(&lower);

    let upper = fixture
        .lower
        .join("Relocated.app/Contents/PlugIns/upper.dylib");
    let upper_backing = fixture
        .runtime
        .filesystem
        .prepare_write(&upper, true)
        .unwrap();
    std::fs::write(&upper_backing, b"upper").unwrap();
    let upper_alias = fixture.backing(&upper);

    with_test_runtime(&fixture.runtime, || unsafe {
        assert!(
            !sandbox_dlopen_with(lower_alias.as_ptr(), libc::RTLD_NOW, record_dlopen).is_null()
        );
        assert_eq!(recorded_path().as_deref(), Some(lower.as_path()));

        assert!(sandbox_dlopen_preflight_with(
            upper_alias.as_ptr(),
            record_preflight
        ));
        assert_eq!(recorded_path().as_deref(), Some(upper_backing.as_path()));
    });
}

#[test]
fn translated_loader_calls_do_not_bypass_hooks_in_library_initializers() {
    let fixture = Fixture::plain();
    let lower = fixture
        .lower
        .join("Relocated.app/Contents/PlugIns/libfixture.dylib");
    std::fs::create_dir_all(lower.parent().unwrap()).unwrap();
    std::fs::write(&lower, b"lower").unwrap();
    let alias = fixture.backing(&lower);

    with_test_runtime(&fixture.runtime, || unsafe {
        assert!(
            !sandbox_dlopen_with(
                alias.as_ptr(),
                libc::RTLD_NOW,
                record_dlopen_with_fresh_filesystem_guard,
            )
            .is_null()
        );
    });

    assert!(ORIGINAL_ENTERED_FILESYSTEM.with(Cell::get));
}

#[test]
fn loader_reports_a_missing_backing_alias_once_through_dlerror() {
    let fixture = Fixture::plain();
    let missing = fixture.backing(&fixture.lower.join("missing.dylib"));

    with_test_runtime(&fixture.runtime, || unsafe {
        clear_recording();
        assert!(sandbox_dlopen_with(missing.as_ptr(), libc::RTLD_NOW, record_dlopen).is_null());
        assert_eq!(recorded_path(), None);
        let error = sandbox_dlerror_with(no_native_loader_error);
        assert!(!error.is_null());
        assert!(
            CStr::from_ptr(error)
                .to_bytes()
                .starts_with(b"agora sandbox loader:")
        );
        assert!(sandbox_dlerror_with(no_native_loader_error).is_null());
    });
}

#[test]
fn loader_denies_direct_and_decoded_private_workdir_paths() {
    let fixture = Fixture::plain();
    let private_path = fixture.directory.join("workdir/runtime/private.dylib");
    let private = CString::new(private_path.as_os_str().as_encoded_bytes()).unwrap();
    let private_alias = fixture.backing(&private_path);

    with_test_runtime(&fixture.runtime, || unsafe {
        assert!(sandbox_dlopen_with(private.as_ptr(), libc::RTLD_NOW, record_dlopen).is_null());
        assert_eq!(*libc::__error(), libc::EACCES);

        assert!(
            sandbox_dlopen_with(private_alias.as_ptr(), libc::RTLD_NOW, record_dlopen).is_null()
        );
        assert_eq!(*libc::__error(), libc::EACCES);
    });
}

#[test]
fn loader_rejects_an_encrypted_upper_without_a_native_plaintext_path() {
    let encrypted = Fixture::encrypted();
    let logical = encrypted.lower.join("encrypted.dylib");
    let backing = encrypted
        .runtime
        .filesystem
        .prepare_write(&logical, true)
        .unwrap();
    std::fs::write(&backing, b"ciphertext").unwrap();
    let alias = encrypted.backing(&logical);

    with_test_runtime(&encrypted.runtime, || unsafe {
        assert!(sandbox_dlopen_with(alias.as_ptr(), libc::RTLD_NOW, record_dlopen).is_null());
        assert_eq!(*libc::__error(), libc::ENOTSUP);
    });
}

#[test]
fn loader_does_not_fall_back_to_lower_after_a_whiteout() {
    let fixture = Fixture::plain();
    let logical = fixture.lower.join("whiteout.dylib");
    std::fs::write(&logical, b"lower").unwrap();
    fixture.runtime.filesystem.remove(&logical, false).unwrap();
    let alias = fixture.backing(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        assert!(sandbox_dlopen_with(alias.as_ptr(), libc::RTLD_NOW, record_dlopen).is_null());
        assert_eq!(*libc::__error(), libc::ENOENT);
    });
}

#[test]
fn successful_native_loader_call_discards_an_older_custom_error() {
    let fixture = Fixture::plain();
    let missing = fixture.backing(&fixture.lower.join("missing.dylib"));
    let ordinary = c"/usr/lib/libSystem.B.dylib";

    with_test_runtime(&fixture.runtime, || unsafe {
        assert!(sandbox_dlopen_with(missing.as_ptr(), libc::RTLD_NOW, record_dlopen).is_null());
        assert!(!sandbox_dlopen_with(ordinary.as_ptr(), libc::RTLD_NOW, record_dlopen).is_null());
        let error = sandbox_dlerror_with(native_loader_error);
        assert_eq!(CStr::from_ptr(error), c"native loader error");
    });
}
