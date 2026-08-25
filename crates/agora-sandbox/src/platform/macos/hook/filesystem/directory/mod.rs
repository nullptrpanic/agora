mod fts;

#[cfg(test)]
mod tests;

pub(in crate::platform::hook::filesystem) use fts::{
    active_fts_logical_path, register_active_fts_mapping,
};

#[cfg(test)]
pub(super) use fts::{
    FtsStreamState, fts_bulk_entry_names_for_test, fts_directory_descent_path_for_test,
    fts_getattrlistbulk_for_test, fts_read_returns_virtual_entry_for_test,
    fts_stream_may_change_current_directory, fts_streams,
};

use super::*;
use crate::platform::hook::abi::darwin_readdir_r;

type FchdirFn = unsafe extern "C" fn(libc::c_int) -> libc::c_int;
type ChdirFn = unsafe extern "C" fn(*const libc::c_char) -> libc::c_int;
type GetcwdFn = unsafe extern "C" fn(*mut libc::c_char, libc::size_t) -> *mut libc::c_char;
type RealpathFn = unsafe extern "C" fn(*const libc::c_char, *mut libc::c_char) -> *mut libc::c_char;
type OpendirFn = unsafe extern "C" fn(*const libc::c_char) -> *mut libc::DIR;
type FdopendirFn = unsafe extern "C" fn(libc::c_int) -> *mut libc::DIR;
type ReaddirFn = unsafe extern "C" fn(*mut libc::DIR) -> *mut libc::dirent;
type RewinddirFn = unsafe extern "C" fn(*mut libc::DIR);
type ClosedirFn = unsafe extern "C" fn(*mut libc::DIR) -> libc::c_int;

pub(super) struct DirectoryCursor {
    sources: Vec<DirectorySource>,
    source_index: usize,
    hidden: HashSet<Vec<u8>>,
    aliases: HashMap<Vec<u8>, Vec<u8>>,
    seen: HashSet<Vec<u8>>,
    remote_names: HashSet<Vec<u8>>,
    remote: Option<RemoteDirectoryCursor>,
}

struct DirectorySource {
    directory: usize,
    lower: bool,
    owned: bool,
}

struct RemoteDirectoryCursor {
    entries: Vec<(Vec<u8>, u8)>,
    index: usize,
    current: Box<libc::dirent>,
}

impl DirectoryCursor {
    pub(super) fn new(
        primary: *mut libc::DIR,
        auxiliary: Option<*mut libc::DIR>,
        primary_layer: FileLayer,
        view: &DirectoryView,
        remote_roots: Vec<Vec<u8>>,
    ) -> Self {
        let primary = DirectorySource {
            directory: primary as usize,
            lower: primary_layer == FileLayer::Lower,
            owned: false,
        };
        let auxiliary = auxiliary.map(|directory| DirectorySource {
            directory: directory as usize,
            lower: primary_layer == FileLayer::Upper,
            owned: true,
        });
        let sources = match (primary_layer, auxiliary) {
            (FileLayer::Upper, auxiliary) => std::iter::once(primary).chain(auxiliary).collect(),
            (FileLayer::Lower, Some(auxiliary)) => vec![auxiliary, primary],
            (FileLayer::Lower, None) => vec![primary],
        };
        Self::from_sources(sources, view).with_remote_roots(remote_roots)
    }

    fn from_sources(sources: Vec<DirectorySource>, view: &DirectoryView) -> Self {
        Self {
            sources,
            source_index: 0,
            hidden: view
                .hidden()
                .iter()
                .map(|name| name.as_bytes().to_vec())
                .collect(),
            aliases: view
                .aliases()
                .iter()
                .map(|(physical, logical)| {
                    (physical.as_bytes().to_vec(), logical.as_bytes().to_vec())
                })
                .collect(),
            seen: HashSet::new(),
            remote_names: HashSet::new(),
            remote: None,
        }
    }

    pub(super) fn filter(view: &DirectoryView) -> Self {
        Self::from_sources(Vec::new(), view)
    }

    pub(super) fn layered_filter(
        view: &DirectoryView,
        entries: &[crate::nfs::protocol::RemoteEntry],
    ) -> Self {
        let mut cursor = Self::filter(view);
        cursor.remote_names = entries
            .iter()
            .map(|entry| entry.name.as_bytes().to_vec())
            .collect();
        cursor
    }

    fn remote(entries: Vec<crate::nfs::protocol::RemoteEntry>) -> Self {
        Self::from_remote_and_sources(entries, Vec::new(), None)
    }

    fn layered_remote(
        entries: Vec<crate::nfs::protocol::RemoteEntry>,
        sources: Vec<DirectorySource>,
        view: &DirectoryView,
    ) -> Self {
        Self::from_remote_and_sources(entries, sources, Some(view))
    }

    fn from_remote_and_sources(
        entries: Vec<crate::nfs::protocol::RemoteEntry>,
        sources: Vec<DirectorySource>,
        view: Option<&DirectoryView>,
    ) -> Self {
        let mut visible = Vec::with_capacity(entries.len() + 2);
        visible.push((b".".to_vec(), libc::DT_DIR));
        visible.push((b"..".to_vec(), libc::DT_DIR));
        visible.extend(entries.into_iter().map(|entry| {
            let file_type = match entry.metadata.file_type {
                crate::nfs::protocol::RemoteFileType::File => libc::DT_REG,
                crate::nfs::protocol::RemoteFileType::Directory => libc::DT_DIR,
            };
            (entry.name.into_bytes(), file_type)
        }));
        let remote_names = visible.iter().map(|(name, _)| name.clone()).collect();
        let mut cursor = if let Some(view) = view {
            Self::from_sources(sources, view)
        } else {
            Self {
                sources,
                source_index: 0,
                hidden: HashSet::new(),
                aliases: HashMap::new(),
                seen: HashSet::new(),
                remote_names: HashSet::new(),
                remote: None,
            }
        };
        cursor.remote_names = remote_names;
        cursor.remote = Some(RemoteDirectoryCursor {
            entries: visible,
            index: 0,
            current: Box::new(unsafe { std::mem::zeroed() }),
        });
        cursor
    }

    fn with_remote_roots(mut self, names: Vec<Vec<u8>>) -> Self {
        if names.is_empty() {
            return self;
        }
        self.remote_names.extend(names.iter().cloned());
        self.remote = Some(RemoteDirectoryCursor {
            entries: names.into_iter().map(|name| (name, libc::DT_DIR)).collect(),
            index: 0,
            current: Box::new(unsafe { std::mem::zeroed() }),
        });
        self
    }

    fn source(&self) -> Option<&DirectorySource> {
        self.sources.get(self.source_index)
    }

    pub(super) fn include(&mut self, name: &[u8], lower: bool) -> Option<Vec<u8>> {
        let visible = self
            .aliases
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(name)
            .to_vec();
        if self.hidden.contains(name)
            || self.hidden.contains(&visible)
            || self.remote_names.contains(&visible)
            || lower && self.seen.contains(&visible)
        {
            return None;
        }
        self.seen.insert(visible.clone());
        Some(visible)
    }

    fn reset(&mut self) {
        if let Some(remote) = &mut self.remote {
            remote.index = 0;
        }
        self.source_index = 0;
        self.seen.clear();
    }

    fn owned_sources(&self) -> impl Iterator<Item = *mut libc::DIR> + '_ {
        self.sources
            .iter()
            .filter(|source| source.owned)
            .map(|source| source.directory as *mut libc::DIR)
    }

    fn next_remote(&mut self) -> Result<Option<*mut libc::dirent>> {
        let Some(remote) = &mut self.remote else {
            return Ok(None);
        };
        let Some((name, file_type)) = remote.entries.get(remote.index) else {
            return Ok(None);
        };
        if name.len() >= remote.current.d_name.len() {
            return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG).into());
        }
        remote.index += 1;
        *remote.current = unsafe { std::mem::zeroed() };
        remote.current.d_seekoff = remote.index as _;
        remote.current.d_reclen = std::mem::size_of::<libc::dirent>() as _;
        remote.current.d_namlen = name.len() as _;
        remote.current.d_type = *file_type;
        unsafe {
            std::ptr::copy_nonoverlapping(
                name.as_ptr().cast::<libc::c_char>(),
                remote.current.d_name.as_mut_ptr(),
                name.len(),
            );
        }
        Ok(Some(remote.current.as_mut()))
    }
}

fn directory_cursors() -> &'static Mutex<HashMap<usize, DirectoryCursor>> {
    static DIRECTORIES: OnceLock<Mutex<HashMap<usize, DirectoryCursor>>> = OnceLock::new();
    DIRECTORIES.get_or_init(|| Mutex::new(HashMap::new()))
}

unsafe fn sandbox_chdir(path: *const libc::c_char) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_chdir() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(path) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(path) };
        };
        let caller_errno = unsafe { *libc::__error() };
        match runtime.prepare_change_directory(path) {
            Ok((mapped, logical, remote, anchor)) => {
                let result = unsafe { original(mapped.as_ptr()) };
                if result == 0 {
                    if remote || runtime.synchronize_current_directory().is_err() {
                        runtime.set_current_directory_state(logical, remote, anchor);
                    }
                    unsafe { set_errno(caller_errno) };
                }
                result
            }
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_chdir(path: *const libc::c_char) -> libc::c_int {
    unsafe { sandbox_chdir(path) }
}

unsafe fn sandbox_fchdir(descriptor: libc::c_int) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_fchdir() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(descriptor) };
        };
        let caller_errno = unsafe { *libc::__error() };
        if runtime.native_passthrough_descriptor(descriptor) {
            let result = unsafe { original(descriptor) };
            if result == 0 {
                let _ = runtime.synchronize_current_directory();
                unsafe { set_errno(caller_errno) };
            }
            return result;
        }
        let logical = match runtime.resolve_descriptor_logical_path(descriptor) {
            Ok(logical) => logical,
            Err(error) => return unsafe { fail(&error, -1) },
        };
        let tracked_open = runtime.tracked_open(descriptor);
        let directory_registration = lock(&runtime.directory_descriptors)
            .get(&descriptor)
            .cloned();
        let managed_descriptor = tracked_open.is_some() || directory_registration.is_some();
        let remote_descriptor = tracked_open.is_some_and(|open| open.manages_metadata())
            || directory_registration.is_some_and(|registration| registration.remote);
        if remote_descriptor {
            let is_directory = lock(&runtime.directory_descriptors).contains_key(&descriptor)
                || runtime
                    .tracked_open(descriptor)
                    .is_some_and(|open| open.managed_is_directory());
            if !is_directory {
                unsafe { set_errno(libc::ENOTDIR) };
                return -1;
            }
        } else if let Err(error) = runtime.filesystem.require_descriptor_access(
            &logical,
            AccessRequest::EXECUTE,
            &Credentials::effective(),
        ) {
            return unsafe { fail(&error, -1) };
        }
        let result = unsafe { original(descriptor) };
        if result == 0 {
            if managed_descriptor || runtime.synchronize_current_directory().is_err() {
                runtime.set_current_directory_state(logical, remote_descriptor, None);
            }
            unsafe { set_errno(caller_errno) };
        }
        result
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fchdir(descriptor: libc::c_int) -> libc::c_int {
    unsafe { sandbox_fchdir(descriptor) }
}

unsafe fn sandbox_getcwd(buffer: *mut libc::c_char, size: libc::size_t) -> *mut libc::c_char {
    catch_filesystem_panic(std::ptr::null_mut(), || {
        let Some(original) = original_getcwd() else {
            unsafe { set_errno(libc::ENOSYS) };
            return std::ptr::null_mut();
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(buffer, size) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(buffer, size) };
        };
        let logical = match runtime.logical_current_directory() {
            Ok(logical) => logical,
            Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
        };
        let required = logical.as_bytes_with_nul().len();
        let (target, capacity) = if buffer.is_null() {
            let capacity = if size == 0 { required } else { size };
            let target = unsafe { libc::malloc(capacity) }.cast::<libc::c_char>();
            if target.is_null() {
                unsafe { set_errno(libc::ENOMEM) };
                return std::ptr::null_mut();
            }
            (target, capacity)
        } else {
            (buffer, size)
        };
        if capacity < required {
            if buffer.is_null() {
                unsafe { libc::free(target.cast()) };
            }
            unsafe { set_errno(libc::ERANGE) };
            return std::ptr::null_mut();
        }
        unsafe {
            std::ptr::copy_nonoverlapping(logical.as_ptr(), target, required);
        }
        target
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_getcwd(
    buffer: *mut libc::c_char,
    size: libc::size_t,
) -> *mut libc::c_char {
    unsafe { sandbox_getcwd(buffer, size) }
}

unsafe fn sandbox_realpath(
    path: *const libc::c_char,
    resolved: *mut libc::c_char,
) -> *mut libc::c_char {
    catch_filesystem_panic(std::ptr::null_mut(), || {
        let Some(original) = original_realpath() else {
            unsafe { set_errno(libc::ENOSYS) };
            return std::ptr::null_mut();
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(path, resolved) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(path, resolved) };
        };
        match unsafe { runtime.native_passthrough_c_path(path, libc::AT_FDCWD) } {
            Ok(Some(native)) => return unsafe { original(native.as_ptr(), resolved) },
            Ok(None) => {}
            Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
        }
        let canonical = match unsafe { runtime.canonical_path(path) } {
            Ok(canonical) => canonical,
            Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
        };
        let required = canonical.as_bytes_with_nul().len();
        if required > libc::PATH_MAX as usize {
            unsafe { set_errno(libc::ENAMETOOLONG) };
            return std::ptr::null_mut();
        }
        let target = if resolved.is_null() {
            let allocated = unsafe { libc::malloc(required) }.cast::<libc::c_char>();
            if allocated.is_null() {
                unsafe { set_errno(libc::ENOMEM) };
                return std::ptr::null_mut();
            }
            allocated
        } else {
            resolved
        };
        unsafe {
            std::ptr::copy_nonoverlapping(canonical.as_ptr(), target, required);
        }
        target
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_realpath(
    path: *const libc::c_char,
    resolved: *mut libc::c_char,
) -> *mut libc::c_char {
    unsafe { sandbox_realpath(path, resolved) }
}

unsafe fn sandbox_opendir(path: *const libc::c_char) -> *mut libc::DIR {
    catch_filesystem_panic(std::ptr::null_mut(), || {
        let Some(original) = original_opendir() else {
            unsafe { set_errno(libc::ENOSYS) };
            return std::ptr::null_mut();
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(path) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(path) };
        };
        match runtime.remote_directory_view(path) {
            Ok(Some(view)) => {
                let directory = unsafe { original(view.anchor().as_ptr()) };
                if directory.is_null() {
                    return directory;
                }
                let prepared = match unsafe { prepare_remote_directory_cursor(runtime, view) } {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        unsafe { original_closedir().map(|close| close(directory)) };
                        return unsafe { fail(&error, std::ptr::null_mut()) };
                    }
                };
                unsafe { register_remote_directory_cursor(runtime, directory, prepared) };
                return directory;
            }
            Ok(None) => {}
            Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
        }
        match runtime.directory_view(path) {
            Ok(view) => {
                let remote_roots = match runtime.remote_route_root_names(view.logical()) {
                    Ok(remote_roots) => remote_roots,
                    Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
                };
                let primary = match CString::new(view.primary().as_os_str().as_bytes()) {
                    Ok(path) => path,
                    Err(error) => return unsafe { fail(&error.into(), std::ptr::null_mut()) },
                };
                let directory = unsafe { original(primary.as_ptr()) };
                if directory.is_null() {
                    return directory;
                }
                if view.is_passthrough() && remote_roots.is_empty() {
                    if let Some(snapshot) = view.native_snapshot().cloned() {
                        runtime.register_directory(
                            unsafe { libc::dirfd(directory) },
                            view.logical().into(),
                            false,
                            Some(snapshot),
                        );
                    }
                    return directory;
                }
                let layer = if runtime.filesystem.is_internal(view.primary()) {
                    FileLayer::Upper
                } else {
                    FileLayer::Lower
                };
                let auxiliary = match unsafe { open_auxiliary_directory(&view, layer) } {
                    Ok(auxiliary) => auxiliary,
                    Err(error) => {
                        unsafe { original_closedir().map(|close| close(directory)) };
                        return unsafe { fail(&error, std::ptr::null_mut()) };
                    }
                };
                unsafe {
                    register_directory_cursor(
                        runtime,
                        directory,
                        auxiliary,
                        layer,
                        &view,
                        remote_roots,
                    )
                };
                directory
            }
            Err(error) => unsafe { fail(&error, std::ptr::null_mut()) },
        }
    })
}

struct PreparedRemoteDirectoryCursor {
    logical: PathBuf,
    physical: PathBuf,
    cursor: DirectoryCursor,
}

unsafe fn prepare_remote_directory_cursor(
    runtime: &FilesystemHookRuntime,
    view: RemoteDirectoryView,
) -> Result<PreparedRemoteDirectoryCursor> {
    let logical = view.logical().to_path_buf();
    let physical = PathBuf::from(OsStr::from_bytes(view.anchor().to_bytes()));
    let entries = view.into_entries();
    let cursor = match runtime.local_directory_view_for_remote(&logical)? {
        Some(local) => DirectoryCursor::layered_remote(
            entries,
            unsafe { open_owned_directory_sources(runtime, &local)? },
            &local,
        ),
        None => DirectoryCursor::remote(entries),
    };
    Ok(PreparedRemoteDirectoryCursor {
        logical,
        physical,
        cursor,
    })
}

unsafe fn register_remote_directory_cursor(
    runtime: &FilesystemHookRuntime,
    directory: *mut libc::DIR,
    prepared: PreparedRemoteDirectoryCursor,
) {
    register_active_fts_mapping(&prepared.physical, &prepared.logical);
    lock(directory_cursors()).insert(directory as usize, prepared.cursor);
    runtime.register_directory(
        unsafe { libc::dirfd(directory) },
        prepared.logical,
        true,
        None,
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_opendir(path: *const libc::c_char) -> *mut libc::DIR {
    unsafe { sandbox_opendir(path) }
}

unsafe fn open_auxiliary_directory(
    view: &DirectoryView,
    primary_layer: FileLayer,
) -> Result<Option<*mut libc::DIR>> {
    let path = match primary_layer {
        FileLayer::Upper => view.lower(),
        FileLayer::Lower if view.lower().is_some() => Some(view.primary()),
        FileLayer::Lower => None,
    };
    let Some(path) = path else {
        return Ok(None);
    };
    let path = CString::new(path.as_os_str().as_bytes())
        .context("auxiliary directory path contains NUL")?;
    let original = original_opendir().context("opendir is unavailable")?;
    let directory = unsafe { original(path.as_ptr()) };
    if directory.is_null() {
        return Err(io::Error::last_os_error().into());
    }
    Ok(Some(directory))
}

unsafe fn open_owned_directory_sources(
    runtime: &FilesystemHookRuntime,
    view: &DirectoryView,
) -> Result<Vec<DirectorySource>> {
    let primary_is_upper = runtime.filesystem.is_internal(view.primary());
    let paths = std::iter::once((view.primary(), !primary_is_upper))
        .chain(view.lower().map(|path| (path, true)));
    let original = original_opendir().context("opendir is unavailable")?;
    let close = original_closedir().context("closedir is unavailable")?;
    let mut sources: Vec<DirectorySource> = Vec::new();
    for (path, lower) in paths {
        let path =
            CString::new(path.as_os_str().as_bytes()).context("directory path contains NUL")?;
        let directory = unsafe { original(path.as_ptr()) };
        if directory.is_null() {
            let error = io::Error::last_os_error();
            for source in sources {
                unsafe { close(source.directory as *mut libc::DIR) };
            }
            return Err(error.into());
        }
        sources.push(DirectorySource {
            directory: directory as usize,
            lower,
            owned: true,
        });
    }
    Ok(sources)
}

unsafe fn register_directory_cursor(
    runtime: &FilesystemHookRuntime,
    directory: *mut libc::DIR,
    auxiliary: Option<*mut libc::DIR>,
    primary_layer: FileLayer,
    view: &DirectoryView,
    remote_roots: Vec<Vec<u8>>,
) {
    lock(directory_cursors()).insert(
        directory as usize,
        DirectoryCursor::new(directory, auxiliary, primary_layer, view, remote_roots),
    );
    runtime.register_directory(
        unsafe { libc::dirfd(directory) },
        view.logical().into(),
        false,
        None,
    );
}

unsafe fn sandbox_fdopendir(descriptor: libc::c_int) -> *mut libc::DIR {
    catch_filesystem_panic(std::ptr::null_mut(), || {
        let Some(original) = original_fdopendir() else {
            unsafe { set_errno(libc::ENOSYS) };
            return std::ptr::null_mut();
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(descriptor) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(descriptor) };
        };
        match runtime.descriptor_remote_directory_view(descriptor) {
            Ok(Some(view)) => {
                let prepared = match unsafe { prepare_remote_directory_cursor(runtime, view) } {
                    Ok(prepared) => prepared,
                    Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
                };
                let directory = unsafe { original(descriptor) };
                if directory.is_null() {
                    if let Some(close) = original_closedir() {
                        for source in prepared.cursor.owned_sources() {
                            unsafe { close(source) };
                        }
                    }
                    return directory;
                }
                unsafe { register_remote_directory_cursor(runtime, directory, prepared) };
                return directory;
            }
            Ok(None) => {}
            Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
        }
        let (view, layer) = match runtime.descriptor_directory_view(descriptor) {
            Ok(view) => view,
            Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
        };
        let remote_roots = match runtime.remote_route_root_names(view.logical()) {
            Ok(remote_roots) => remote_roots,
            Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
        };
        if view.is_passthrough() && layer == FileLayer::Lower && remote_roots.is_empty() {
            let directory = unsafe { original(descriptor) };
            if !directory.is_null()
                && let Some(snapshot) = view.native_snapshot().cloned()
            {
                runtime.register_directory(
                    unsafe { libc::dirfd(directory) },
                    view.logical().into(),
                    false,
                    Some(snapshot),
                );
            }
            return directory;
        }
        let auxiliary = match unsafe { open_auxiliary_directory(&view, layer) } {
            Ok(auxiliary) => auxiliary,
            Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
        };
        let directory = unsafe { original(descriptor) };
        if directory.is_null() {
            let error = unsafe { *libc::__error() };
            if let Some(auxiliary) = auxiliary {
                unsafe { original_closedir().map(|close| close(auxiliary)) };
            }
            unsafe { set_errno(error) };
            return directory;
        }
        unsafe {
            register_directory_cursor(runtime, directory, auxiliary, layer, &view, remote_roots)
        };
        directory
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fdopendir(descriptor: libc::c_int) -> *mut libc::DIR {
    unsafe { sandbox_fdopendir(descriptor) }
}

unsafe fn sandbox_readdir(directory: *mut libc::DIR) -> *mut libc::dirent {
    let Some(original) = original_readdir() else {
        unsafe { set_errno(libc::ENOSYS) };
        return std::ptr::null_mut();
    };
    unsafe { sandbox_readdir_with(directory, original) }
}

unsafe fn sandbox_readdir_with(
    directory: *mut libc::DIR,
    original: ReaddirFn,
) -> *mut libc::dirent {
    catch_filesystem_panic(std::ptr::null_mut(), || {
        let Some(guard) = FilesystemHookGuard::enter() else {
            unsafe { set_errno(0) };
            return unsafe { original(directory) };
        };
        let mut cursors = lock(directory_cursors());
        let Some(cursor) = cursors.get_mut(&(directory as usize)) else {
            drop(cursors);
            drop(guard);
            unsafe { set_errno(0) };
            return unsafe { original(directory) };
        };
        if cursor.remote.is_some() {
            unsafe { set_errno(0) };
            match cursor.next_remote() {
                Ok(Some(entry)) => return entry,
                Ok(None) => {}
                Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
            }
        }
        loop {
            let Some(source) = cursor.source() else {
                return std::ptr::null_mut();
            };
            let source_directory = source.directory as *mut libc::DIR;
            let lower = source.lower;
            unsafe { set_errno(0) };
            let entry = unsafe { original(source_directory) };
            if entry.is_null() {
                if unsafe { *libc::__error() } == 0 {
                    cursor.source_index += 1;
                    continue;
                }
                return std::ptr::null_mut();
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if let Some(visible) = cursor.include(name.to_bytes(), lower) {
                if visible != name.to_bytes() {
                    if visible.len() >= unsafe { (*entry).d_name.len() } {
                        unsafe { set_errno(libc::ENAMETOOLONG) };
                        return std::ptr::null_mut();
                    }
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            visible.as_ptr().cast::<libc::c_char>(),
                            (*entry).d_name.as_mut_ptr(),
                            visible.len(),
                        );
                        (*entry).d_name[visible.len()] = 0;
                        (*entry).d_namlen = visible.len() as u16;
                    }
                }
                return entry;
            }
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_readdir(directory: *mut libc::DIR) -> *mut libc::dirent {
    unsafe { sandbox_readdir(directory) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_readdir_r(
    directory: *mut libc::DIR,
    entry: *mut libc::dirent,
    result: *mut *mut libc::dirent,
) -> libc::c_int {
    catch_filesystem_panic(libc::EIO, || {
        if directory.is_null() || entry.is_null() || result.is_null() {
            return libc::EINVAL;
        }
        unsafe { *result = std::ptr::null_mut() };
        unsafe { set_errno(0) };
        let source = unsafe { sandbox_readdir(directory) };
        if source.is_null() {
            let error = unsafe { *libc::__error() };
            return error;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(source, entry, 1);
            *result = entry;
        }
        0
    })
}

unsafe fn sandbox_rewinddir(directory: *mut libc::DIR) {
    let Some(original) = original_rewinddir() else {
        unsafe { set_errno(libc::ENOSYS) };
        return;
    };
    unsafe { sandbox_rewinddir_with(directory, original) }
}

unsafe fn sandbox_rewinddir_with(directory: *mut libc::DIR, original: RewinddirFn) {
    catch_filesystem_panic((), || {
        let Some(guard) = FilesystemHookGuard::enter() else {
            unsafe { original(directory) };
            return;
        };
        let mut cursors = lock(directory_cursors());
        let Some(cursor) = cursors.get_mut(&(directory as usize)) else {
            drop(cursors);
            drop(guard);
            unsafe { original(directory) };
            return;
        };
        unsafe { original(directory) };
        for source in cursor.owned_sources() {
            unsafe { original(source) };
        }
        cursor.reset();
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_rewinddir(directory: *mut libc::DIR) {
    unsafe { sandbox_rewinddir(directory) }
}

unsafe fn sandbox_closedir(directory: *mut libc::DIR) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_closedir() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(directory) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(directory) };
        };
        let cursor = lock(directory_cursors()).remove(&(directory as usize));
        let descriptor = unsafe { libc::dirfd(directory) };
        let _operation = runtime.operations.acquire(
            mapping::OperationRequest::new()
                .descriptor_registry_shared()
                .descriptor_exclusive(descriptor),
        );
        let transition = runtime.begin_descriptor_transition_under_lease(descriptor);
        let tracked = runtime.take_descriptor_during_transition_under_lease(descriptor);
        if let Some((open, true)) = &tracked
            && let Err(error) = runtime.finish_open_file(descriptor, open)
        {
            runtime.restore_descriptor_under_lease(descriptor, Arc::clone(open));
            lock(directory_cursors()).extend(cursor.map(|cursor| (directory as usize, cursor)));
            return unsafe { fail(&error, -1) };
        }
        let result = unsafe { original(directory) };
        if let Some(cursor) = cursor {
            for source in cursor.owned_sources() {
                unsafe { original(source) };
            }
        }
        if result == 0 {
            transition.clear();
            runtime.unregister_directory(descriptor);
        } else if result != 0
            && let Some((open, _)) = tracked
        {
            runtime.restore_descriptor_under_lease(descriptor, open);
        }
        result
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_closedir(directory: *mut libc::DIR) -> libc::c_int {
    unsafe { sandbox_closedir(directory) }
}

fn original_chdir() -> Option<ChdirFn> {
    function_from_interpose(&INTERPOSE_CHDIR)
}

pub(super) fn change_directory_native(path: &CStr) -> Result<()> {
    let original = original_chdir().context("native chdir is unavailable")?;
    native_operation_result(unsafe { original(path.as_ptr()) })
}

fn original_fchdir() -> Option<FchdirFn> {
    function_from_interpose(&INTERPOSE_FCHDIR)
}

fn original_getcwd() -> Option<GetcwdFn> {
    function_from_interpose(&INTERPOSE_GETCWD)
}

fn original_realpath() -> Option<RealpathFn> {
    function_from_interpose(&INTERPOSE_REALPATH)
}

fn original_opendir() -> Option<OpendirFn> {
    function_from_interpose(&INTERPOSE_OPENDIR)
}

fn original_fdopendir() -> Option<FdopendirFn> {
    function_from_interpose(&INTERPOSE_FDOPENDIR)
}

fn original_readdir() -> Option<ReaddirFn> {
    function_from_interpose(&INTERPOSE_READDIR)
}

fn original_rewinddir() -> Option<RewinddirFn> {
    function_from_interpose(&INTERPOSE_REWINDDIR)
}

fn original_closedir() -> Option<ClosedirFn> {
    function_from_interpose(&INTERPOSE_CLOSEDIR)
}

dyld_interpose!(INTERPOSE_CHDIR, agora_sandbox_chdir, libc::chdir);

dyld_interpose!(INTERPOSE_FCHDIR, agora_sandbox_fchdir, libc::fchdir);

dyld_interpose!(INTERPOSE_GETCWD, agora_sandbox_getcwd, libc::getcwd);

dyld_interpose!(INTERPOSE_REALPATH, agora_sandbox_realpath, libc::realpath);

dyld_interpose!(INTERPOSE_OPENDIR, agora_sandbox_opendir, libc::opendir);

dyld_interpose!(
    INTERPOSE_FDOPENDIR,
    agora_sandbox_fdopendir,
    libc::fdopendir
);

dyld_interpose!(INTERPOSE_READDIR, agora_sandbox_readdir, libc::readdir);

dyld_interpose!(
    INTERPOSE_READDIR_R,
    agora_sandbox_readdir_r,
    darwin_readdir_r
);

dyld_interpose!(
    INTERPOSE_REWINDDIR,
    agora_sandbox_rewinddir,
    libc::rewinddir
);

dyld_interpose!(INTERPOSE_CLOSEDIR, agora_sandbox_closedir, libc::closedir);
