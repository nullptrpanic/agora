//! Mirror public Darwin spawn actions so preparation can use their final FD
//! mapping without mutating a caller-owned, reusable opaque action list.

use super::super::filesystem::{self, FilesystemHookGuard};
use super::*;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone)]
pub(in crate::platform::hook) enum Action {
    Close(i32),
    Dup2(i32, i32),
    Open(i32, CString, i32, libc::mode_t),
    Inherit(i32),
    Chdir(CString),
    Fchdir(i32),
}

impl Action {
    pub(in crate::platform::hook) fn descriptors(&self) -> [i32; 2] {
        match self {
            Self::Close(fd) | Self::Open(fd, ..) | Self::Inherit(fd) | Self::Fchdir(fd) => {
                [*fd, *fd]
            }
            Self::Dup2(source, destination) => [*source, *destination],
            Self::Chdir(_) => [-1, -1],
        }
    }

    unsafe fn replay(&self, actions: *mut libc::posix_spawn_file_actions_t) -> i32 {
        unsafe {
            match self {
                Self::Close(fd) => libc::posix_spawn_file_actions_addclose(actions, *fd),
                Self::Dup2(source, destination) => {
                    libc::posix_spawn_file_actions_adddup2(actions, *source, *destination)
                }
                Self::Open(fd, path, flags, mode) => libc::posix_spawn_file_actions_addopen(
                    actions,
                    *fd,
                    path.as_ptr(),
                    *flags,
                    *mode,
                ),
                Self::Inherit(fd) => posix_spawn_file_actions_addinherit_np(actions, *fd),
                Self::Chdir(path) => posix_spawn_file_actions_addchdir_np(actions, path.as_ptr()),
                Self::Fchdir(fd) => posix_spawn_file_actions_addfchdir_np(actions, *fd),
            }
        }
    }
}

type Actions = libc::posix_spawn_file_actions_t;
static ACTIONS: Mutex<Option<HashMap<usize, Vec<Action>>>> = Mutex::new(None);

// All access is inside the filesystem fork barrier. Never hold this mutex
// while preparing local descriptors, contacting the Broker, or spawning.
fn with_actions<T>(apply: impl FnOnce(&mut HashMap<usize, Vec<Action>>) -> T) -> T {
    apply(
        ACTIONS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_or_insert_with(HashMap::new),
    )
}

pub(in crate::platform::hook) unsafe fn record(
    actions: *mut Actions,
    action: impl FnOnce() -> Action,
    original: impl FnOnce() -> i32,
) -> i32 {
    with_actions(|lists| {
        let previous = unsafe { actions.as_ref() }
            .copied()
            .unwrap_or(std::ptr::null_mut()) as usize;
        let result = original();
        // Darwin reallocates the opaque list as actions are appended. Move the
        // mirror with it under the same mutex, before that address can be reused.
        if result == 0
            && let Some(mut list) = lists.remove(&previous)
        {
            list.push(action());
            lists.insert(unsafe { *actions } as usize, list);
        }
        result
    })
}

unsafe fn append(
    actions: *mut Actions,
    action: impl FnOnce() -> Action,
    original: impl FnOnce() -> i32,
) -> i32 {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return original();
    };
    unsafe { record(actions, action, original) }
}

pub(super) struct SpawnFileActions {
    borrowed: *const Actions,
    owned: Option<Actions>,
    pub(super) local: filesystem::SpawnLocalDescriptors,
    pub(super) replaces_process: bool,
}

impl SpawnFileActions {
    pub(super) unsafe fn prepare(
        actions: *const Actions,
        attributes: *const libc::posix_spawnattr_t,
    ) -> Result<Self, i32> {
        let guard = FilesystemHookGuard::enter();
        let recorded = if actions.is_null() {
            Some(Vec::new())
        } else if guard.is_some() {
            with_actions(|lists| lists.get(&(unsafe { *actions } as usize)).cloned())
        } else {
            None
        };
        drop(guard);
        let mut flags = 0;
        if !attributes.is_null() {
            let result = unsafe { libc::posix_spawnattr_getflags(attributes, &mut flags) };
            if result != 0 {
                return Err(result);
            }
        }
        let replaces_process = i32::from(flags) & libc::POSIX_SPAWN_SETEXEC != 0;
        let local = filesystem::prepare_local_spawn(
            recorded.as_deref(),
            i32::from(flags) & libc::POSIX_SPAWN_CLOEXEC_DEFAULT != 0,
            replaces_process,
        )
        .map_err(|error| PrepareError::from_anyhow(error, libc::EIO).errno)?;
        let mut prepared = Self {
            borrowed: actions,
            owned: None,
            local,
            replaces_process,
        };
        let Some(recorded) = recorded else {
            // Lists created before hook initialization cannot be inspected.
            // Native-only descriptors need no translation; managed ones were
            // rejected above rather than silently restoring the wrong state.
            return Ok(prepared);
        };
        let mut owned = std::ptr::null_mut();
        let result = unsafe { libc::posix_spawn_file_actions_init(&mut owned) };
        if result != 0 {
            return Err(result);
        }
        prepared.owned = Some(owned);
        let native = prepared.owned.as_mut().unwrap();
        for fd in super::super::control::inheritable_descriptors()
            .into_iter()
            .chain(prepared.local.descriptors())
        {
            let result = unsafe { posix_spawn_file_actions_addinherit_np(native, fd) };
            if result != 0 {
                return Err(result);
            }
        }
        for action in recorded {
            let result = unsafe { action.replay(native) };
            if result != 0 {
                return Err(result);
            }
        }
        for fd in &prepared.local.close {
            let result = unsafe { libc::posix_spawn_file_actions_addclose(native, *fd) };
            if result != 0 {
                return Err(result);
            }
        }
        Ok(prepared)
    }

    pub(super) fn as_ptr(&self) -> *const Actions {
        self.owned
            .as_ref()
            .map_or(self.borrowed, |owned| owned as *const _)
    }
}

impl Drop for SpawnFileActions {
    fn drop(&mut self) {
        if let Some(actions) = self.owned.as_mut() {
            unsafe { libc::posix_spawn_file_actions_destroy(actions) };
        }
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn agora_sandbox_posix_spawn_file_actions_init(actions: *mut Actions) -> i32 {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return unsafe { libc::posix_spawn_file_actions_init(actions) };
    };
    with_actions(|lists| {
        let result = unsafe { libc::posix_spawn_file_actions_init(actions) };
        if result == 0 {
            lists.insert(unsafe { *actions } as usize, Vec::new());
        }
        result
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn agora_sandbox_posix_spawn_file_actions_destroy(actions: *mut Actions) -> i32 {
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return unsafe { libc::posix_spawn_file_actions_destroy(actions) };
    };
    with_actions(|lists| {
        let key = unsafe { actions.as_ref() }
            .copied()
            .unwrap_or(std::ptr::null_mut()) as usize;
        let result = unsafe { libc::posix_spawn_file_actions_destroy(actions) };
        if result == 0 {
            lists.remove(&key);
        }
        result
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn agora_sandbox_posix_spawn_file_actions_addclose(
    actions: *mut Actions,
    fd: i32,
) -> i32 {
    unsafe {
        append(
            actions,
            || Action::Close(fd),
            || libc::posix_spawn_file_actions_addclose(actions, fd),
        )
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn agora_sandbox_posix_spawn_file_actions_adddup2(
    actions: *mut Actions,
    source: i32,
    destination: i32,
) -> i32 {
    unsafe {
        append(
            actions,
            || Action::Dup2(source, destination),
            || libc::posix_spawn_file_actions_adddup2(actions, source, destination),
        )
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn agora_sandbox_posix_spawn_file_actions_addinherit_np(
    actions: *mut Actions,
    fd: i32,
) -> i32 {
    unsafe {
        append(
            actions,
            || Action::Inherit(fd),
            || posix_spawn_file_actions_addinherit_np(actions, fd),
        )
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn agora_sandbox_posix_spawn_file_actions_addchdir_np(
    actions: *mut Actions,
    path: *const libc::c_char,
) -> i32 {
    unsafe {
        append(
            actions,
            || Action::Chdir(CStr::from_ptr(path).to_owned()),
            || posix_spawn_file_actions_addchdir_np(actions, path),
        )
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn agora_sandbox_posix_spawn_file_actions_addfchdir_np(
    actions: *mut Actions,
    fd: i32,
) -> i32 {
    unsafe {
        append(
            actions,
            || Action::Fchdir(fd),
            || posix_spawn_file_actions_addfchdir_np(actions, fd),
        )
    }
}

// macOS 26 added distinct standard spellings. Weak references in the C shim
// keep older deployment targets loadable while recording both entry points.
unsafe extern "C" {
    fn posix_spawn_file_actions_addchdir_np(
        actions: *mut Actions,
        path: *const libc::c_char,
    ) -> i32;
    fn posix_spawn_file_actions_addfchdir_np(actions: *mut Actions, fd: i32) -> i32;
    static AGORA_INTERPOSE_SPAWN_ADDCHDIR: super::super::dyld::DyldInterpose;
    static AGORA_INTERPOSE_SPAWN_ADDFCHDIR: super::super::dyld::DyldInterpose;
}

#[unsafe(no_mangle)]
unsafe extern "C" fn agora_sandbox_posix_spawn_file_actions_addchdir(
    actions: *mut Actions,
    path: *const libc::c_char,
) -> i32 {
    let Some(original) = function_from_interpose::<
        unsafe extern "C" fn(*mut Actions, *const libc::c_char) -> i32,
    >(unsafe { &AGORA_INTERPOSE_SPAWN_ADDCHDIR }) else {
        return libc::ENOSYS;
    };
    unsafe {
        append(
            actions,
            || Action::Chdir(CStr::from_ptr(path).to_owned()),
            || original(actions, path),
        )
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn agora_sandbox_posix_spawn_file_actions_addfchdir(
    actions: *mut Actions,
    fd: i32,
) -> i32 {
    let Some(original) =
        function_from_interpose::<unsafe extern "C" fn(*mut Actions, i32) -> i32>(unsafe {
            &AGORA_INTERPOSE_SPAWN_ADDFCHDIR
        })
    else {
        return libc::ENOSYS;
    };
    unsafe { append(actions, || Action::Fchdir(fd), || original(actions, fd)) }
}

dyld_interpose!(
    INTERPOSE_INIT,
    agora_sandbox_posix_spawn_file_actions_init,
    libc::posix_spawn_file_actions_init
);
dyld_interpose!(
    INTERPOSE_DESTROY,
    agora_sandbox_posix_spawn_file_actions_destroy,
    libc::posix_spawn_file_actions_destroy
);
dyld_interpose!(
    INTERPOSE_CLOSE,
    agora_sandbox_posix_spawn_file_actions_addclose,
    libc::posix_spawn_file_actions_addclose
);
dyld_interpose!(
    INTERPOSE_DUP2,
    agora_sandbox_posix_spawn_file_actions_adddup2,
    libc::posix_spawn_file_actions_adddup2
);
dyld_interpose!(
    INTERPOSE_INHERIT,
    agora_sandbox_posix_spawn_file_actions_addinherit_np,
    posix_spawn_file_actions_addinherit_np
);
dyld_interpose!(
    INTERPOSE_CHDIR,
    agora_sandbox_posix_spawn_file_actions_addchdir_np,
    posix_spawn_file_actions_addchdir_np
);
dyld_interpose!(
    INTERPOSE_FCHDIR,
    agora_sandbox_posix_spawn_file_actions_addfchdir_np,
    posix_spawn_file_actions_addfchdir_np
);
