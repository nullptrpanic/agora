use super::super::metadata::{original_lstat, patch_stat};
use super::super::*;
use super::DirectoryCursor;
use crate::platform::hook::abi::{
    DarwinFtsEntry, FtsCompareFn, darwin_fts_children, darwin_fts_close, darwin_fts_open,
    darwin_fts_read, darwin_fts_set, darwin_getattrlistbulk,
};
use std::cell::{Cell, RefCell};
use std::os::unix::fs::FileTypeExt;

type FtsOpenFn = unsafe extern "C" fn(
    *const *mut libc::c_char,
    libc::c_int,
    Option<FtsCompareFn>,
) -> *mut libc::c_void;
type FtsChildrenFn = unsafe extern "C" fn(*mut libc::c_void, libc::c_int) -> *mut DarwinFtsEntry;
type FtsCloseFn = unsafe extern "C" fn(*mut libc::c_void) -> libc::c_int;
type FtsReadFn = unsafe extern "C" fn(*mut libc::c_void) -> *mut DarwinFtsEntry;
type GetattrlistbulkFn = unsafe extern "C" fn(
    libc::c_int,
    *mut libc::c_void,
    *mut libc::c_void,
    libc::size_t,
    u64,
) -> libc::c_int;

const FTS_D: libc::c_ushort = 1;
const FTS_DNR: libc::c_ushort = 4;
const FTS_DOT: libc::c_ushort = 5;
const FTS_ERR: libc::c_ushort = 7;
const FTS_F: libc::c_ushort = 8;
const FTS_NS: libc::c_ushort = 10;
const FTS_NSOK: libc::c_ushort = 11;
const FTS_SL: libc::c_ushort = 12;
const FTS_DEFAULT: libc::c_ushort = 3;
const FTS_SKIP: libc::c_int = 4;
const FTS_NOCHDIR: libc::c_int = 0x004;
const DARWIN_VNODE_TYPE_DIRECTORY: u32 = 2;

thread_local! {
    static ACTIVE_FTS_STREAM: Cell<usize> = const { Cell::new(0) };
    static FTS_BULK_CURSORS: RefCell<HashMap<libc::c_int, FtsBulkCursor>> = RefCell::new(HashMap::new());
    static FTS_COMPARE_CONTEXTS: RefCell<Vec<FtsCompareContext>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
thread_local! {
    static TEST_FTS_READ_ENTRY: Cell<usize> = const { Cell::new(usize::MAX) };
}

struct FtsVirtualBulk {
    previous: usize,
}

impl FtsVirtualBulk {
    fn enter(stream: *mut libc::c_void) -> Self {
        let previous = ACTIVE_FTS_STREAM.with(|active| active.replace(stream as usize));
        Self { previous }
    }

    fn is_active() -> bool {
        Self::active_stream().is_some()
    }

    fn active_stream() -> Option<usize> {
        let stream = ACTIVE_FTS_STREAM.with(Cell::get);
        (stream != 0).then_some(stream)
    }
}

impl Drop for FtsVirtualBulk {
    fn drop(&mut self) {
        ACTIVE_FTS_STREAM.with(|active| active.set(self.previous));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FtsDescriptorIdentity {
    device: libc::dev_t,
    inode: libc::ino_t,
}

struct FtsBulkEntry {
    name: Vec<u8>,
    object_type: u32,
}

struct FtsBulkCursor {
    identity: FtsDescriptorIdentity,
    entries: Vec<FtsBulkEntry>,
    next: usize,
}

#[derive(Clone)]
pub(in crate::platform::hook::filesystem) struct FtsRootMapping {
    physical: Vec<u8>,
    logical: Vec<u8>,
    resolved: Vec<u8>,
}

pub(in crate::platform::hook::filesystem) struct PresentedFtsEntry {
    entry: usize,
    original_path: usize,
    original_access_path: usize,
    original_path_length: libc::c_ushort,
    original_name_length: libc::c_ushort,
    original_name: Vec<libc::c_char>,
    logical_path: CString,
    logical_access_path: CString,
}

pub(in crate::platform::hook::filesystem) struct FtsStreamState {
    pub(in crate::platform::hook::filesystem) compare: Option<FtsCompareFn>,
    pub(in crate::platform::hook::filesystem) mappings: Vec<FtsRootMapping>,
    pub(in crate::platform::hook::filesystem) presented: Vec<PresentedFtsEntry>,
    pub(in crate::platform::hook::filesystem) traversal_paths: Vec<CString>,
    pub(in crate::platform::hook::filesystem) anchors: Vec<RemoteAnchor>,
}

impl FtsStreamState {
    fn restore(&mut self) {
        for presented in self.presented.drain(..) {
            let entry = presented.entry as *mut DarwinFtsEntry;
            unsafe {
                (*entry).fts_path = presented.original_path as *mut libc::c_char;
                (*entry).fts_accpath = presented.original_access_path as *mut libc::c_char;
                (*entry).fts_pathlen = presented.original_path_length;
                (*entry).fts_namelen = presented.original_name_length;
                std::ptr::copy_nonoverlapping(
                    presented.original_name.as_ptr(),
                    (*entry).fts_name.as_mut_ptr(),
                    presented.original_name.len(),
                );
            }
        }
    }

    fn present(&mut self, entry: *mut DarwinFtsEntry) -> Result<()> {
        if entry.is_null() {
            return Ok(());
        }
        let path = unsafe { CStr::from_ptr((*entry).fts_path) }.to_bytes();
        let access_path = unsafe { CStr::from_ptr((*entry).fts_accpath) }.to_bytes();
        let Some(logical_path) = self.translate(path).or_else(|| self.translate(access_path))
        else {
            return Ok(());
        };
        let logical_access_path = self
            .translate(access_path)
            .unwrap_or_else(|| logical_path.clone());
        let original_name_length = unsafe { (*entry).fts_namelen };
        let original_name = unsafe {
            std::slice::from_raw_parts(
                (*entry).fts_name.as_ptr(),
                usize::from(original_name_length) + 1,
            )
            .to_vec()
        };
        let original_name_bytes = unsafe {
            std::slice::from_raw_parts(
                original_name.as_ptr().cast::<u8>(),
                usize::from(original_name_length),
            )
        };
        let logical_name = if original_name_bytes == path
            || original_name_bytes == logical_basename(path)
            || original_name_bytes == access_path
            || original_name_bytes == logical_basename(access_path)
        {
            logical_basename(&logical_path).to_vec()
        } else {
            original_name_bytes.to_vec()
        };
        if logical_name.len() > usize::from(original_name_length)
            || logical_name.len() > usize::from(u16::MAX)
        {
            return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG).into());
        }
        let logical_path = CString::new(logical_path).context("logical FTS path contains NUL")?;
        let logical_access_path =
            CString::new(logical_access_path).context("logical FTS access path contains NUL")?;
        let presented = PresentedFtsEntry {
            entry: entry as usize,
            original_path: unsafe { (*entry).fts_path } as usize,
            original_access_path: unsafe { (*entry).fts_accpath } as usize,
            original_path_length: unsafe { (*entry).fts_pathlen },
            original_name_length,
            original_name,
            logical_path,
            logical_access_path,
        };
        unsafe {
            (*entry).fts_path = presented.logical_path.as_ptr().cast_mut();
            (*entry).fts_accpath = presented.logical_access_path.as_ptr().cast_mut();
            (*entry).fts_pathlen = u16::try_from(presented.logical_path.as_bytes().len())?;
            (*entry).fts_namelen = u16::try_from(logical_name.len())?;
            std::ptr::copy_nonoverlapping(
                logical_name.as_ptr().cast::<libc::c_char>(),
                (*entry).fts_name.as_mut_ptr(),
                logical_name.len(),
            );
            *(*entry).fts_name.as_mut_ptr().add(logical_name.len()) = 0;
        }
        self.presented.push(presented);
        Ok(())
    }

    fn present_list(&mut self, mut entry: *mut DarwinFtsEntry) -> Result<()> {
        while !entry.is_null() {
            let next = unsafe { (*entry).fts_link };
            self.present(entry)?;
            entry = next;
        }
        Ok(())
    }

    fn retarget_directory(
        &mut self,
        entry: *mut DarwinFtsEntry,
        mapped: CString,
        resolved: &Path,
    ) -> Result<()> {
        let path = unsafe { CStr::from_ptr((*entry).fts_path) }.to_bytes();
        let access_path = unsafe { CStr::from_ptr((*entry).fts_accpath) }.to_bytes();
        if access_path == mapped.as_bytes() {
            return Ok(());
        }
        let logical = self
            .translate(path)
            .or_else(|| self.translate(access_path))
            .unwrap_or_else(|| resolved.as_os_str().as_bytes().to_vec());
        let physical = mapped.as_bytes().to_vec();
        if !self
            .mappings
            .iter()
            .any(|mapping| mapping.physical == physical)
        {
            self.mappings.push(FtsRootMapping {
                physical,
                logical,
                resolved: resolved.as_os_str().as_bytes().to_vec(),
            });
            self.mappings
                .sort_by_key(|mapping| std::cmp::Reverse(mapping.physical.len()));
        }
        let pointer = if let Some(path) = self
            .traversal_paths
            .iter()
            .find(|path| path.as_bytes() == mapped.as_bytes())
        {
            path.as_ptr()
        } else {
            self.traversal_paths.push(mapped);
            self.traversal_paths
                .last()
                .context("missing FTS traversal path")?
                .as_ptr()
        };
        unsafe { (*entry).fts_accpath = pointer.cast_mut() };
        Ok(())
    }

    fn translate(&self, path: &[u8]) -> Option<Vec<u8>> {
        self.translate_with(path, |mapping| &mapping.logical)
    }

    fn resolve(&self, path: &[u8]) -> Option<Vec<u8>> {
        self.translate_with(path, |mapping| &mapping.resolved)
    }

    fn translate_with<'a>(
        &'a self,
        path: &[u8],
        destination: impl Fn(&'a FtsRootMapping) -> &'a Vec<u8>,
    ) -> Option<Vec<u8>> {
        self.mappings.iter().find_map(|mapping| {
            let suffix = path.strip_prefix(mapping.physical.as_slice())?;
            if !suffix.is_empty() && !suffix.starts_with(b"/") {
                return None;
            }
            let mut logical = destination(mapping).clone();
            logical.extend_from_slice(suffix);
            Some(logical)
        })
    }
}

#[derive(Clone)]
struct FtsCompareContext {
    compare: FtsCompareFn,
    mappings: Vec<FtsRootMapping>,
}

struct FtsCompareGuard;

impl FtsCompareGuard {
    fn enter(compare: Option<FtsCompareFn>, mappings: &[FtsRootMapping]) -> Option<Self> {
        let compare = compare?;
        if mappings.is_empty() {
            return None;
        }
        FTS_COMPARE_CONTEXTS.with(|contexts| {
            contexts.borrow_mut().push(FtsCompareContext {
                compare,
                mappings: mappings.to_vec(),
            });
        });
        Some(Self)
    }

    fn for_stream(stream: *mut libc::c_void) -> Option<Self> {
        let streams = lock(fts_streams());
        let state = streams.get(&(stream as usize))?;
        Self::enter(state.compare, &state.mappings)
    }
}

impl Drop for FtsCompareGuard {
    fn drop(&mut self) {
        FTS_COMPARE_CONTEXTS.with(|contexts| {
            contexts.borrow_mut().pop();
        });
    }
}

struct FtsEntryShadow {
    storage: Box<[u64]>,
    _path: CString,
    _access_path: CString,
}

impl FtsEntryShadow {
    unsafe fn new(
        entry: *const DarwinFtsEntry,
        mappings: &[FtsRootMapping],
    ) -> Result<Option<Self>> {
        let path = unsafe { CStr::from_ptr((*entry).fts_path) }.to_bytes();
        let access_path = unsafe { CStr::from_ptr((*entry).fts_accpath) }.to_bytes();
        let Some(logical_path) = translate_fts_path(mappings, path)
            .or_else(|| translate_fts_path(mappings, access_path))
        else {
            return Ok(None);
        };
        let logical_access_path =
            translate_fts_path(mappings, access_path).unwrap_or_else(|| logical_path.clone());
        let original_name = unsafe {
            std::slice::from_raw_parts(
                (*entry).fts_name.as_ptr().cast::<u8>(),
                usize::from((*entry).fts_namelen),
            )
        };
        let logical_name = if original_name == path
            || original_name == logical_basename(path)
            || original_name == access_path
            || original_name == logical_basename(access_path)
        {
            logical_basename(&logical_path).to_vec()
        } else {
            original_name.to_vec()
        };
        let path =
            CString::new(logical_path).context("logical FTS comparator path contains NUL")?;
        let access_path = CString::new(logical_access_path)
            .context("logical FTS comparator access path contains NUL")?;
        let bytes = std::mem::offset_of!(DarwinFtsEntry, fts_name)
            .checked_add(logical_name.len())
            .and_then(|size| size.checked_add(1))
            .context("logical FTS comparator entry is too large")?;
        let words = bytes.div_ceil(std::mem::size_of::<u64>());
        let mut storage = vec![0_u64; words].into_boxed_slice();
        let shadow = storage.as_mut_ptr().cast::<DarwinFtsEntry>();
        unsafe {
            std::ptr::copy_nonoverlapping(
                entry.cast::<u8>(),
                shadow.cast::<u8>(),
                std::mem::offset_of!(DarwinFtsEntry, fts_name),
            );
            (*shadow).fts_path = path.as_ptr().cast_mut();
            (*shadow).fts_accpath = access_path.as_ptr().cast_mut();
            (*shadow).fts_pathlen = u16::try_from(path.as_bytes().len())?;
            (*shadow).fts_namelen = u16::try_from(logical_name.len())?;
            std::ptr::copy_nonoverlapping(
                logical_name.as_ptr().cast::<libc::c_char>(),
                (*shadow).fts_name.as_mut_ptr(),
                logical_name.len(),
            );
            *(*shadow).fts_name.as_mut_ptr().add(logical_name.len()) = 0;
        }
        Ok(Some(Self {
            storage,
            _path: path,
            _access_path: access_path,
        }))
    }

    fn pointer(&self) -> *const DarwinFtsEntry {
        self.storage.as_ptr().cast()
    }
}

unsafe extern "C" fn logical_fts_compare(
    left: *const *const DarwinFtsEntry,
    right: *const *const DarwinFtsEntry,
) -> libc::c_int {
    let Some(context) = FTS_COMPARE_CONTEXTS.with(|contexts| contexts.borrow().last().cloned())
    else {
        return 0;
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let left_entry = *left;
        let right_entry = *right;
        if left_entry.is_null() || right_entry.is_null() {
            return (context.compare)(left, right);
        }
        let left_shadow = FtsEntryShadow::new(left_entry, &context.mappings)
            .ok()
            .flatten();
        let right_shadow = FtsEntryShadow::new(right_entry, &context.mappings)
            .ok()
            .flatten();
        let left_logical = left_shadow
            .as_ref()
            .map_or(left_entry, FtsEntryShadow::pointer);
        let right_logical = right_shadow
            .as_ref()
            .map_or(right_entry, FtsEntryShadow::pointer);
        (context.compare)(&left_logical, &right_logical)
    }))
    .unwrap_or_else(|_| unsafe { (context.compare)(left, right) })
}

fn translate_fts_path(mappings: &[FtsRootMapping], path: &[u8]) -> Option<Vec<u8>> {
    mappings.iter().find_map(|mapping| {
        let suffix = path.strip_prefix(mapping.physical.as_slice())?;
        if !suffix.is_empty() && !suffix.starts_with(b"/") {
            return None;
        }
        let mut logical = mapping.logical.clone();
        logical.extend_from_slice(suffix);
        Some(logical)
    })
}

pub(in crate::platform::hook::filesystem) fn active_fts_logical_path(
    path: &Path,
) -> Option<PathBuf> {
    let stream = FtsVirtualBulk::active_stream()?;
    let path = path.as_os_str().as_bytes();
    lock(fts_streams())
        .get(&stream)?
        .resolve(path)
        .map(|path| PathBuf::from(OsStr::from_bytes(&path)))
}

pub(in crate::platform::hook::filesystem) fn register_active_fts_mapping(
    physical: &Path,
    logical: &Path,
) {
    let Some(stream) = FtsVirtualBulk::active_stream() else {
        return;
    };
    let physical = physical.as_os_str().as_bytes().to_vec();
    let logical = logical.as_os_str().as_bytes().to_vec();
    let mut streams = lock(fts_streams());
    let Some(state) = streams.get_mut(&stream) else {
        return;
    };
    if state
        .mappings
        .iter()
        .any(|mapping| mapping.physical == physical)
    {
        return;
    }
    state.mappings.push(FtsRootMapping {
        physical,
        logical: logical.clone(),
        resolved: logical,
    });
    state
        .mappings
        .sort_by_key(|mapping| std::cmp::Reverse(mapping.physical.len()));
}

fn logical_basename(path: &[u8]) -> &[u8] {
    let path = path.strip_suffix(b"/").unwrap_or(path);
    path.rsplit(|byte| *byte == b'/').next().unwrap_or(path)
}

pub(in crate::platform::hook::filesystem) fn fts_streams()
-> &'static Mutex<HashMap<usize, FtsStreamState>> {
    static STREAMS: OnceLock<Mutex<HashMap<usize, FtsStreamState>>> = OnceLock::new();
    STREAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(in crate::platform::hook::filesystem) fn fts_stream_may_change_current_directory(
    stream: *mut libc::c_void,
) -> bool {
    !lock(fts_streams()).contains_key(&(stream as usize))
}

fn restore_fts_stream(stream: *mut libc::c_void) {
    if let Some(state) = lock(fts_streams()).get_mut(&(stream as usize)) {
        state.restore();
    }
}

fn present_fts_entry(stream: *mut libc::c_void, entry: *mut DarwinFtsEntry) -> Result<()> {
    if let Some(state) = lock(fts_streams()).get_mut(&(stream as usize)) {
        state.present(entry)?;
    }
    Ok(())
}

fn present_fts_list(stream: *mut libc::c_void, entry: *mut DarwinFtsEntry) -> Result<()> {
    if let Some(state) = lock(fts_streams()).get_mut(&(stream as usize)) {
        state.present_list(entry)?;
    }
    Ok(())
}

impl FtsBulkCursor {
    fn new(
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        identity: FtsDescriptorIdentity,
    ) -> Result<Option<Self>> {
        if let Some(view) = runtime.descriptor_remote_directory_view(descriptor)? {
            return Self::layered_remote(runtime, identity, view).map(Some);
        }
        let descriptor_path = FilesystemHookRuntime::descriptor_path(descriptor)?;
        if let Some(logical) = active_fts_logical_path(&descriptor_path)
            && let Some(view) = runtime.remote_directory_view_for_logical(&logical)?
        {
            return Self::layered_remote(runtime, identity, view).map(Some);
        }
        let (view, _) = runtime.descriptor_directory_view(descriptor)?;
        let remote_roots = runtime.remote_route_root_names(view.logical())?;
        if view.is_passthrough() && remote_roots.is_empty() {
            return Ok(None);
        }
        let mut filter = DirectoryCursor::filter(&view);
        filter.remote_names.extend(remote_roots.iter().cloned());
        let mut entries = remote_roots
            .into_iter()
            .map(|name| FtsBulkEntry {
                name,
                object_type: DARWIN_VNODE_TYPE_DIRECTORY,
            })
            .collect();
        let primary_is_upper = runtime.filesystem.is_internal(view.primary());
        Self::extend_entries(&mut entries, &mut filter, view.primary(), !primary_is_upper)?;
        if let Some(lower) = view.lower() {
            Self::extend_entries(&mut entries, &mut filter, lower, true)?;
        }
        Ok(Some(Self {
            identity,
            entries,
            next: 0,
        }))
    }

    fn layered_remote(
        runtime: &FilesystemHookRuntime,
        identity: FtsDescriptorIdentity,
        view: RemoteDirectoryView,
    ) -> Result<Self> {
        let logical = view.logical().to_path_buf();
        let remote_entries = view.into_entries();
        let mut entries = remote_entries
            .iter()
            .map(|entry| FtsBulkEntry {
                name: entry.name.as_bytes().to_vec(),
                object_type: match entry.metadata.file_type {
                    crate::nfs::protocol::RemoteFileType::File => 1,
                    crate::nfs::protocol::RemoteFileType::Directory => DARWIN_VNODE_TYPE_DIRECTORY,
                },
            })
            .collect::<Vec<_>>();
        if let Some(local) = runtime.local_directory_view_for_remote(&logical)? {
            let mut filter = DirectoryCursor::layered_filter(&local, &remote_entries);
            let primary_is_upper = runtime.filesystem.is_internal(local.primary());
            Self::extend_entries(
                &mut entries,
                &mut filter,
                local.primary(),
                !primary_is_upper,
            )?;
            if let Some(lower) = local.lower() {
                Self::extend_entries(&mut entries, &mut filter, lower, true)?;
            }
        }
        Ok(Self {
            identity,
            entries,
            next: 0,
        })
    }

    fn extend_entries(
        entries: &mut Vec<FtsBulkEntry>,
        filter: &mut DirectoryCursor,
        directory: &Path,
        lower: bool,
    ) -> Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = filter.include(name.as_bytes(), lower) else {
                continue;
            };
            entries.push(FtsBulkEntry {
                name,
                object_type: darwin_object_type(&entry.file_type()?),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
pub(in crate::platform::hook::filesystem) fn fts_bulk_entry_names_for_test(
    runtime: &FilesystemHookRuntime,
    descriptor: libc::c_int,
) -> Result<Vec<Vec<u8>>> {
    let identity = fts_descriptor_identity(descriptor)?;
    Ok(FtsBulkCursor::new(runtime, descriptor, identity)?
        .map(|cursor| cursor.entries.into_iter().map(|entry| entry.name).collect())
        .unwrap_or_default())
}

#[cfg(test)]
pub(in crate::platform::hook::filesystem) fn fts_read_returns_virtual_entry_for_test(
    logical: &Path,
) -> Result<bool> {
    Ok(fts_read_virtual_entry_for_test(logical, FTS_NSOK)?.is_some())
}

#[cfg(test)]
pub(in crate::platform::hook::filesystem) fn fts_directory_descent_path_for_test(
    logical: &Path,
) -> Result<Vec<u8>> {
    fts_read_virtual_entry_for_test(logical, FTS_D)?
        .context("virtual FTS test directory was filtered")
}

#[cfg(test)]
fn fts_read_virtual_entry_for_test(
    logical: &Path,
    info: libc::c_ushort,
) -> Result<Option<Vec<u8>>> {
    let logical_root = logical
        .parent()
        .context("virtual FTS test entry has no parent")?;
    let physical_root = std::env::temp_dir().join(format!("agora-fts-{}", uuid::Uuid::new_v4()));
    let physical = CString::new(
        physical_root
            .join(
                logical
                    .file_name()
                    .context("virtual FTS test entry has no name")?,
            )
            .as_os_str()
            .as_bytes(),
    )?;
    let mut entry = unsafe { std::mem::zeroed::<DarwinFtsEntry>() };
    entry.fts_accpath = physical.as_ptr().cast_mut();
    entry.fts_path = physical.as_ptr().cast_mut();
    entry.fts_pathlen = u16::try_from(physical.as_bytes().len())?;
    entry.fts_info = info;
    let stream = (&mut entry as *mut DarwinFtsEntry).cast::<libc::c_void>();
    lock(fts_streams()).insert(
        stream as usize,
        FtsStreamState {
            compare: None,
            mappings: vec![FtsRootMapping {
                physical: physical_root.as_os_str().as_bytes().to_vec(),
                logical: logical_root.as_os_str().as_bytes().to_vec(),
                resolved: logical_root.as_os_str().as_bytes().to_vec(),
            }],
            presented: Vec::new(),
            traversal_paths: Vec::new(),
            anchors: Vec::new(),
        },
    );
    TEST_FTS_READ_ENTRY.with(|slot| slot.set(&mut entry as *mut DarwinFtsEntry as usize));
    let returned = unsafe { sandbox_fts_read(stream) };
    TEST_FTS_READ_ENTRY.with(|slot| slot.set(usize::MAX));
    restore_fts_stream(stream);
    let access_path = (!returned.is_null())
        .then(|| unsafe { CStr::from_ptr(entry.fts_accpath).to_bytes().to_vec() });
    lock(fts_streams()).remove(&(stream as usize));
    Ok(access_path)
}

fn darwin_object_type(file_type: &std::fs::FileType) -> u32 {
    if file_type.is_file() {
        1 // VREG
    } else if file_type.is_dir() {
        DARWIN_VNODE_TYPE_DIRECTORY
    } else if file_type.is_block_device() {
        3 // VBLK
    } else if file_type.is_char_device() {
        4 // VCHR
    } else if file_type.is_symlink() {
        5 // VLNK
    } else if file_type.is_socket() {
        6 // VSOCK
    } else if file_type.is_fifo() {
        7 // VFIFO
    } else {
        0 // VNON
    }
}

fn fts_descriptor_identity(descriptor: libc::c_int) -> Result<FtsDescriptorIdentity> {
    let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(descriptor, &mut status) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(FtsDescriptorIdentity {
        device: status.st_dev,
        inode: status.st_ino,
    })
}

fn fts_attr_record(entry: &FtsBulkEntry, attributes: &libc::attrlist) -> Result<Vec<u8>> {
    const ATTRIBUTE_SET_SIZE: usize = 20;
    const ATTRIBUTE_REFERENCE_OFFSET: usize = 4 + ATTRIBUTE_SET_SIZE;
    const FULL_FIXED_SIZE: usize = 156;
    const NOSTAT_FIXED_SIZE: usize = 56;

    let nostat = attributes.commonattr & libc::ATTR_CMN_CRTIME == 0;
    let fixed_size = if nostat {
        NOSTAT_FIXED_SIZE
    } else {
        FULL_FIXED_SIZE
    };
    let unaligned_size = fixed_size
        .checked_add(entry.name.len())
        .and_then(|size| size.checked_add(1))
        .context("FTS attribute record size overflow")?;
    let record_size = unaligned_size
        .checked_add(3)
        .map(|size| size & !3)
        .context("FTS attribute record size overflow")?;
    let record_length = u32::try_from(record_size).context("FTS attribute record is too large")?;
    let name_length = u32::try_from(entry.name.len() + 1).context("FTS name is too large")?;
    let name_offset = i32::try_from(fixed_size - ATTRIBUTE_REFERENCE_OFFSET)
        .context("FTS name offset is too large")?;

    let mut record = vec![0_u8; record_size];
    record[0..4].copy_from_slice(&record_length.to_ne_bytes());
    let returned_common =
        libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE;
    record[4..8].copy_from_slice(&returned_common.to_ne_bytes());
    record[24..28].copy_from_slice(&name_offset.to_ne_bytes());
    record[28..32].copy_from_slice(&name_length.to_ne_bytes());
    record[36..40].copy_from_slice(&entry.object_type.to_ne_bytes());
    record[fixed_size..fixed_size + entry.name.len()].copy_from_slice(&entry.name);
    Ok(record)
}

fn fts_attributes_supported(attributes: &libc::attrlist) -> bool {
    attributes.bitmapcount == libc::ATTR_BIT_MAP_COUNT
        && attributes.commonattr
            & (libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE)
            == (libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE)
        && attributes.volattr == 0
        && attributes.dirattr == 0
        && attributes.forkattr == 0
}

unsafe fn sandbox_getattrlistbulk(
    directory: libc::c_int,
    attributes: *mut libc::c_void,
    buffer: *mut libc::c_void,
    size: libc::size_t,
    options: u64,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_getattrlistbulk() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(guard) = active_fts_bulk_guard(FilesystemHookGuard::enter()) else {
            return unsafe { original(directory, attributes, buffer, size, options) };
        };
        if attributes.is_null() || buffer.is_null() {
            unsafe { set_errno(libc::EFAULT) };
            return -1;
        }
        let attributes = unsafe { std::ptr::read(attributes.cast::<libc::attrlist>()) };
        if !fts_attributes_supported(&attributes) {
            drop(guard);
            return unsafe {
                original(
                    directory,
                    (&raw const attributes).cast_mut().cast(),
                    buffer,
                    size,
                    options,
                )
            };
        }
        let Some(runtime) = FilesystemHookRuntime::global() else {
            drop(guard);
            return unsafe {
                original(
                    directory,
                    (&raw const attributes).cast_mut().cast(),
                    buffer,
                    size,
                    options,
                )
            };
        };
        let identity = match fts_descriptor_identity(directory) {
            Ok(identity) => identity,
            Err(error) => return unsafe { fail(&error, -1) },
        };
        let needs_cursor = FTS_BULK_CURSORS.with(|cursors| {
            cursors
                .borrow()
                .get(&directory)
                .is_none_or(|cursor| cursor.identity != identity)
        });
        if needs_cursor {
            if FtsVirtualBulk::active_stream().is_none()
                && let Err(error) = runtime.synchronize_current_directory()
            {
                return unsafe { fail(&error, -1) };
            }
            let cursor = match FtsBulkCursor::new(runtime, directory, identity) {
                Ok(Some(cursor)) => cursor,
                Ok(None) => {
                    return unsafe {
                        original(
                            directory,
                            (&raw const attributes).cast_mut().cast(),
                            buffer,
                            size,
                            options,
                        )
                    };
                }
                Err(error) => return unsafe { fail(&error, -1) },
            };
            FTS_BULK_CURSORS.with(|cursors| {
                cursors.borrow_mut().insert(directory, cursor);
            });
        }

        let mut written = 0_usize;
        let mut count = 0_i32;
        let mut finished = false;
        let result = FTS_BULK_CURSORS.with(|cursors| -> Result<()> {
            let mut cursors = cursors.borrow_mut();
            let cursor = cursors
                .get_mut(&directory)
                .context("missing FTS bulk cursor")?;
            while let Some(entry) = cursor.entries.get(cursor.next) {
                let record = fts_attr_record(entry, &attributes)?;
                if record.len() > size.saturating_sub(written) {
                    if count == 0 {
                        return Err(io::Error::from_raw_os_error(libc::ERANGE).into());
                    }
                    break;
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        record.as_ptr(),
                        buffer.cast::<u8>().add(written),
                        record.len(),
                    );
                }
                written += record.len();
                count += 1;
                cursor.next += 1;
            }
            finished = cursor.next == cursor.entries.len() && count == 0;
            Ok(())
        });
        if let Err(error) = result {
            FTS_BULK_CURSORS.with(|cursors| {
                cursors.borrow_mut().remove(&directory);
            });
            return unsafe { fail(&error, -1) };
        }
        if finished {
            FTS_BULK_CURSORS.with(|cursors| {
                cursors.borrow_mut().remove(&directory);
            });
        }
        unsafe { set_errno(0) };
        count
    })
}

fn active_fts_bulk_guard(guard: Option<FilesystemHookGuard>) -> Option<FilesystemHookGuard> {
    active_fts_bulk_guard_with(guard, FtsVirtualBulk::is_active)
}

fn active_fts_bulk_guard_with(
    guard: Option<FilesystemHookGuard>,
    is_active: impl FnOnce() -> bool,
) -> Option<FilesystemHookGuard> {
    let guard = guard?;
    is_active().then_some(guard)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_getattrlistbulk(
    directory: libc::c_int,
    attributes: *mut libc::c_void,
    buffer: *mut libc::c_void,
    size: libc::size_t,
    options: u64,
) -> libc::c_int {
    unsafe { sandbox_getattrlistbulk(directory, attributes, buffer, size, options) }
}

#[cfg(test)]
pub(in crate::platform::hook::filesystem) unsafe fn fts_getattrlistbulk_for_test(
    directory: libc::c_int,
    attributes: *mut libc::c_void,
    buffer: *mut libc::c_void,
    size: libc::size_t,
) -> libc::c_int {
    let stream = std::ptr::NonNull::<libc::c_void>::dangling().as_ptr();
    let _bulk = FtsVirtualBulk::enter(stream);
    unsafe { agora_sandbox_getattrlistbulk(directory, attributes, buffer, size, 0) }
}

fn fts_entry_is_visible(
    runtime: &FilesystemHookRuntime,
    entry: *mut DarwinFtsEntry,
) -> Result<bool> {
    let raw_path = unsafe { (*entry).fts_accpath };
    if raw_path.is_null() {
        return Ok(true);
    }
    unsafe { trusted_fts_logical_path(runtime, raw_path) }
        .and_then(|logical| runtime.path_exists(&logical))
}

unsafe fn trusted_fts_logical_path(
    runtime: &FilesystemHookRuntime,
    path: *const libc::c_char,
) -> Result<PathBuf> {
    let requested = Path::new(OsStr::from_bytes(
        unsafe { CStr::from_ptr(path) }.to_bytes(),
    ));
    if let Some(logical) = active_fts_logical_path(requested) {
        return Ok(logical);
    }
    if requested.is_absolute() && runtime.filesystem.is_internal(requested) {
        runtime.filesystem.logical_path(requested)
    } else {
        unsafe { runtime.logical_path(path, libc::AT_FDCWD) }
    }
}

unsafe fn sandbox_fts_open(
    paths: *const *mut libc::c_char,
    options: libc::c_int,
    compare: Option<FtsCompareFn>,
) -> *mut libc::c_void {
    catch_filesystem_panic(std::ptr::null_mut(), || {
        let Some(original) = original_fts_open() else {
            unsafe { set_errno(libc::ENOSYS) };
            return std::ptr::null_mut();
        };
        if paths.is_null() {
            unsafe { set_errno(libc::EFAULT) };
            return std::ptr::null_mut();
        }
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(paths, options, compare) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(paths, options, compare) };
        };
        if let Err(error) = runtime.synchronize_current_directory_for_fts() {
            return unsafe { fail(&error, std::ptr::null_mut()) };
        }

        let mut mapped_paths = Vec::new();
        let mut mappings = Vec::new();
        let mut anchors = Vec::new();
        let mut index = 0_usize;
        loop {
            let path = unsafe { *paths.add(index) };
            if path.is_null() {
                break;
            }
            let original_path = unsafe { CStr::from_ptr(path) };
            let logical = match unsafe { runtime.logical_path(path, libc::AT_FDCWD) } {
                Ok(logical) => logical,
                Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
            };
            let (mapped, _, _, anchor) = match runtime.map_metadata(
                path,
                libc::AT_FDCWD,
                false,
                &Credentials::effective(),
            ) {
                Ok(mapped) => mapped,
                Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
            };
            if mapped.to_bytes() == logical.as_os_str().as_bytes() {
                match CString::new(original_path.to_bytes()) {
                    Ok(path) => mapped_paths.push(path),
                    Err(error) => return unsafe { fail(&error.into(), std::ptr::null_mut()) },
                }
            } else {
                mappings.push(FtsRootMapping {
                    physical: mapped.to_bytes().to_vec(),
                    logical: original_path.to_bytes().to_vec(),
                    resolved: logical.as_os_str().as_bytes().to_vec(),
                });
                mapped_paths.push(mapped);
            }
            anchors.extend(anchor);
            index += 1;
        }
        let mut mapped_argv = mapped_paths
            .iter_mut()
            .map(|path| path.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        mapped_argv.push(std::ptr::null_mut());
        mappings.sort_by_key(|mapping| std::cmp::Reverse(mapping.physical.len()));
        let translated_compare = compare
            .filter(|_| !mappings.is_empty())
            .map(|_| logical_fts_compare as FtsCompareFn)
            .or(compare);
        let _compare_guard = FtsCompareGuard::enter(compare, &mappings);
        let stream = unsafe {
            original(
                mapped_argv.as_ptr(),
                options | FTS_NOCHDIR,
                translated_compare,
            )
        };
        if !stream.is_null() {
            lock(fts_streams()).insert(
                stream as usize,
                FtsStreamState {
                    compare,
                    mappings,
                    presented: Vec::new(),
                    traversal_paths: Vec::new(),
                    anchors,
                },
            );
        }
        stream
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fts_open(
    paths: *const *mut libc::c_char,
    options: libc::c_int,
    compare: Option<FtsCompareFn>,
) -> *mut libc::c_void {
    unsafe { sandbox_fts_open(paths, options, compare) }
}

fn repair_virtual_fts_entry(
    runtime: &FilesystemHookRuntime,
    stream: *mut libc::c_void,
    entry: *mut DarwinFtsEntry,
) -> Result<bool> {
    let info = unsafe { (*entry).fts_info };
    if info == FTS_NSOK
        || (info != FTS_NS && info != FTS_D && unsafe { (*entry).fts_statp.is_null() })
    {
        return Ok(false);
    }
    let path = unsafe { (*entry).fts_accpath };
    if path.is_null() {
        return Ok(false);
    }
    let logical_path = unsafe { trusted_fts_logical_path(runtime, path) }?;
    let logical = CString::new(logical_path.as_os_str().as_bytes())
        .context("logical FTS metadata path contains NUL")?;
    let (mapped, plaintext_size, attributes, mut anchor) = runtime.map_metadata(
        logical.as_ptr(),
        libc::AT_FDCWD,
        false,
        &Credentials::effective(),
    )?;
    if info != FTS_NS {
        if unsafe { !(*entry).fts_statp.is_null() } {
            unsafe {
                patch_stat(
                    &mut *(*entry).fts_statp,
                    plaintext_size,
                    attributes.as_ref(),
                )
            };
        }
        if info == FTS_D
            && let Some(state) = lock(fts_streams()).get_mut(&(stream as usize))
        {
            state.retarget_directory(entry, mapped, &logical_path)?;
            state.anchors.extend(anchor.take());
        }
        return Ok(false);
    }
    let original = original_lstat().context("lstat is unavailable")?;
    let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { original(mapped.as_ptr(), &mut status) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    unsafe { patch_stat(&mut status, plaintext_size, attributes.as_ref()) };
    drop(anchor);
    let name = unsafe { CStr::from_ptr((*entry).fts_name.as_ptr()) }.to_bytes();
    let repaired = match status.st_mode & libc::S_IFMT {
        libc::S_IFDIR if matches!(name, b"." | b"..") => FTS_DOT,
        libc::S_IFDIR => FTS_D,
        libc::S_IFREG => FTS_F,
        libc::S_IFLNK => FTS_SL,
        _ => FTS_DEFAULT,
    };
    unsafe {
        if !(*entry).fts_statp.is_null() {
            *(*entry).fts_statp = status;
        }
        (*entry).fts_errno = 0;
        (*entry).fts_info = repaired;
    }
    if repaired == FTS_D
        && let Some(state) = lock(fts_streams()).get_mut(&(stream as usize))
    {
        state.retarget_directory(entry, mapped, &logical_path)?;
    }
    Ok(true)
}

unsafe fn skip_fts_directory(stream: *mut libc::c_void, entry: *mut DarwinFtsEntry) -> libc::c_int {
    if unsafe { (*entry).fts_info } == FTS_D {
        unsafe { darwin_fts_set(stream, entry, FTS_SKIP) }
    } else {
        0
    }
}

unsafe fn sandbox_fts_children(
    stream: *mut libc::c_void,
    options: libc::c_int,
) -> *mut DarwinFtsEntry {
    catch_filesystem_panic(std::ptr::null_mut(), || {
        let Some(original) = original_fts_children() else {
            unsafe { set_errno(libc::ENOSYS) };
            return std::ptr::null_mut();
        };
        let Some(guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(stream, options) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(stream, options) };
        };
        restore_fts_stream(stream);
        drop(guard);
        unsafe { set_errno(0) };
        let mut head = {
            let _compare = FtsCompareGuard::for_stream(stream);
            let _bulk = FtsVirtualBulk::enter(stream);
            unsafe { original(stream, options) }
        };
        let original_errno = unsafe { *libc::__error() };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            unsafe { set_errno(original_errno) };
            return head;
        };
        if fts_stream_may_change_current_directory(stream) {
            let _ = runtime.synchronize_current_directory();
        }
        let mut current = head;
        let mut previous: *mut DarwinFtsEntry = std::ptr::null_mut();
        let mut unresolved_error = false;
        while !current.is_null() {
            let next = unsafe { (*current).fts_link };
            let visible = match fts_entry_is_visible(runtime, current) {
                Ok(visible) => visible,
                Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
            };
            if visible {
                if let Err(error) = repair_virtual_fts_entry(runtime, stream, current) {
                    return unsafe { fail(&error, std::ptr::null_mut()) };
                }
                unresolved_error |=
                    matches!(unsafe { (*current).fts_info }, FTS_DNR | FTS_ERR | FTS_NS);
                previous = current;
            } else {
                if unsafe { skip_fts_directory(stream, current) } != 0 {
                    return std::ptr::null_mut();
                }
                if previous.is_null() {
                    head = next;
                } else {
                    unsafe { (*previous).fts_link = next };
                }
            }
            current = next;
        }
        let successful = !head.is_null() && !unresolved_error;
        if successful {
            let parent = unsafe { (*head).fts_parent };
            if !parent.is_null() {
                unsafe { (*parent).fts_errno = 0 };
            }
        }
        unsafe { set_errno(if successful { 0 } else { original_errno }) };
        if let Err(error) = present_fts_list(stream, head) {
            return unsafe { fail(&error, std::ptr::null_mut()) };
        }
        head
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fts_children(
    stream: *mut libc::c_void,
    options: libc::c_int,
) -> *mut DarwinFtsEntry {
    unsafe { sandbox_fts_children(stream, options) }
}

unsafe fn sandbox_fts_close(stream: *mut libc::c_void) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_fts_close() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let may_change_current_directory = fts_stream_may_change_current_directory(stream);
        restore_fts_stream(stream);
        let Some(guard) = FilesystemHookGuard::enter() else {
            let result = unsafe { original(stream) };
            lock(fts_streams()).remove(&(stream as usize));
            return result;
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            let result = unsafe { original(stream) };
            lock(fts_streams()).remove(&(stream as usize));
            return result;
        };
        drop(guard);
        let result = unsafe { original(stream) };
        let original_errno = unsafe { *libc::__error() };
        lock(fts_streams()).remove(&(stream as usize));
        let Some(_guard) = FilesystemHookGuard::enter() else {
            unsafe { set_errno(original_errno) };
            return result;
        };
        if may_change_current_directory {
            let _ = runtime.synchronize_current_directory();
        }
        unsafe { set_errno(original_errno) };
        result
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fts_close(stream: *mut libc::c_void) -> libc::c_int {
    unsafe { sandbox_fts_close(stream) }
}

unsafe fn sandbox_fts_read(stream: *mut libc::c_void) -> *mut DarwinFtsEntry {
    catch_filesystem_panic(std::ptr::null_mut(), || {
        let Some(original) = original_fts_read() else {
            unsafe { set_errno(libc::ENOSYS) };
            return std::ptr::null_mut();
        };
        let Some(guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(stream) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(stream) };
        };
        restore_fts_stream(stream);
        drop(guard);
        loop {
            unsafe { set_errno(0) };
            let _compare = FtsCompareGuard::for_stream(stream);
            let _bulk = FtsVirtualBulk::enter(stream);
            let entry = unsafe { original(stream) };
            let original_errno = unsafe { *libc::__error() };
            let Some(_guard) = FilesystemHookGuard::enter() else {
                unsafe { set_errno(original_errno) };
                return entry;
            };
            if fts_stream_may_change_current_directory(stream) {
                let _ = runtime.synchronize_current_directory();
            }
            if entry.is_null() {
                unsafe { set_errno(original_errno) };
                return entry;
            }
            let visible = match fts_entry_is_visible(runtime, entry) {
                Ok(visible) => visible,
                Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
            };
            if visible {
                let repaired = match repair_virtual_fts_entry(runtime, stream, entry) {
                    Ok(repaired) => repaired,
                    Err(error) => return unsafe { fail(&error, std::ptr::null_mut()) },
                };
                let unresolved_error =
                    matches!(unsafe { (*entry).fts_info }, FTS_DNR | FTS_ERR | FTS_NS);
                if !unresolved_error {
                    unsafe { (*entry).fts_errno = 0 };
                }
                unsafe {
                    set_errno(if repaired || !unresolved_error {
                        0
                    } else {
                        original_errno
                    })
                };
                if let Err(error) = present_fts_entry(stream, entry) {
                    return unsafe { fail(&error, std::ptr::null_mut()) };
                }
                return entry;
            }
            if unsafe { skip_fts_directory(stream, entry) } != 0 {
                return std::ptr::null_mut();
            }
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_fts_read(stream: *mut libc::c_void) -> *mut DarwinFtsEntry {
    unsafe { sandbox_fts_read(stream) }
}

fn original_fts_open() -> Option<FtsOpenFn> {
    function_from_interpose(&INTERPOSE_FTS_OPEN)
}

fn original_fts_children() -> Option<FtsChildrenFn> {
    function_from_interpose(&INTERPOSE_FTS_CHILDREN)
}

fn original_fts_close() -> Option<FtsCloseFn> {
    function_from_interpose(&INTERPOSE_FTS_CLOSE)
}

fn original_fts_read() -> Option<FtsReadFn> {
    #[cfg(test)]
    if TEST_FTS_READ_ENTRY.with(|entry| entry.get() != usize::MAX) {
        return Some(test_fts_read);
    }
    function_from_interpose(&INTERPOSE_FTS_READ)
}

#[cfg(test)]
unsafe extern "C" fn test_fts_read(_stream: *mut libc::c_void) -> *mut DarwinFtsEntry {
    let entry = TEST_FTS_READ_ENTRY.with(|entry| entry.replace(0));
    unsafe { set_errno(0) };
    entry as *mut DarwinFtsEntry
}

fn original_getattrlistbulk() -> Option<GetattrlistbulkFn> {
    function_from_interpose(&INTERPOSE_GETATTRLISTBULK)
}

dyld_interpose!(INTERPOSE_FTS_OPEN, agora_sandbox_fts_open, darwin_fts_open);

dyld_interpose!(
    INTERPOSE_FTS_CHILDREN,
    agora_sandbox_fts_children,
    darwin_fts_children
);

dyld_interpose!(
    INTERPOSE_FTS_CLOSE,
    agora_sandbox_fts_close,
    darwin_fts_close
);

dyld_interpose!(INTERPOSE_FTS_READ, agora_sandbox_fts_read, darwin_fts_read);

dyld_interpose!(
    INTERPOSE_GETATTRLISTBULK,
    agora_sandbox_getattrlistbulk,
    darwin_getattrlistbulk
);

#[cfg(test)]
mod tests;
