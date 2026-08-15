use super::*;

type MmapFn = unsafe extern "C" fn(
    *mut libc::c_void,
    usize,
    libc::c_int,
    libc::c_int,
    libc::c_int,
    libc::off_t,
) -> *mut libc::c_void;
type MemoryFn = unsafe extern "C" fn(*mut libc::c_void, usize) -> libc::c_int;
type MemoryFlagsFn = unsafe extern "C" fn(*mut libc::c_void, usize, libc::c_int) -> libc::c_int;
const DESCRIPTOR_INDEX_BITS: usize = 65_536;
const DESCRIPTOR_WORD_BITS: usize = u64::BITS as usize;

#[derive(Clone)]
struct MappingSlice {
    address: usize,
    length: usize,
    open: Arc<OpenFile>,
}

struct PendingMapping {
    file_offset: u64,
    writable: bool,
    open: Arc<OpenFile>,
}

#[derive(Clone, Copy)]
struct MappingRange {
    start: usize,
    end: usize,
}

struct AtomicSnapshot<T: Send + Sync> {
    current: AtomicPtr<T>,
    readers: AtomicUsize,
    retired: Mutex<Vec<Box<T>>>,
}

struct SnapshotReadGuard<'a, T: Send + Sync> {
    snapshot: &'a AtomicSnapshot<T>,
}

impl<T: Send + Sync> AtomicSnapshot<T> {
    fn new(value: T) -> Self {
        Self {
            current: AtomicPtr::new(Box::into_raw(Box::new(value))),
            readers: AtomicUsize::new(0),
            retired: Mutex::new(Vec::new()),
        }
    }

    fn read<R>(&self, operation: impl FnOnce(&T) -> R) -> R {
        self.readers.fetch_add(1, Ordering::SeqCst);
        let _guard = SnapshotReadGuard { snapshot: self };
        let current = self.current.load(Ordering::SeqCst);
        // The reader count is incremented before loading `current`. Publishers
        // retain every replaced allocation until no snapshot reader exists.
        operation(unsafe { &*current })
    }

    fn publish(&self, value: T) {
        let next = Box::into_raw(Box::new(value));
        let previous = self.current.swap(next, Ordering::SeqCst);
        let mut retired = lock(&self.retired);
        // `previous` came from `Box::into_raw` and remains owned by this
        // snapshot until all readers that could have observed it are gone.
        retired.push(unsafe { Box::from_raw(previous) });
        if self.readers.load(Ordering::SeqCst) == 0 {
            retired.clear();
        }
    }
}

impl<T: Send + Sync> Drop for AtomicSnapshot<T> {
    fn drop(&mut self) {
        debug_assert_eq!(*self.readers.get_mut(), 0);
        let current = *self.current.get_mut();
        // The runtime can only be dropped after its readers have completed, and
        // `current` is the one allocation not owned by `retired`.
        drop(unsafe { Box::from_raw(current) });
    }
}

impl<T: Send + Sync> Drop for SnapshotReadGuard<'_, T> {
    fn drop(&mut self) {
        self.snapshot.readers.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(super) struct MemoryStateIndex {
    mappings: AtomicSnapshot<Vec<MappingRange>>,
    descriptors: Box<[AtomicU64]>,
}

impl MemoryStateIndex {
    pub(super) fn new() -> Self {
        Self {
            mappings: AtomicSnapshot::new(Vec::new()),
            descriptors: (0..DESCRIPTOR_INDEX_BITS / DESCRIPTOR_WORD_BITS)
                .map(|_| AtomicU64::new(0))
                .collect(),
        }
    }

    fn publish_mappings(&self, mappings: &[MemoryMapping]) {
        self.mappings.publish(
            mappings
                .iter()
                .map(|mapping| MappingRange {
                    start: mapping.start,
                    end: mapping.end,
                })
                .collect(),
        );
    }

    pub(super) fn set_descriptor(&self, descriptor: libc::c_int, tracked: bool) {
        let Ok(descriptor) = usize::try_from(descriptor) else {
            return;
        };
        let Some(word) = self.descriptors.get(descriptor / DESCRIPTOR_WORD_BITS) else {
            return;
        };
        let mask = 1_u64 << (descriptor % DESCRIPTOR_WORD_BITS);
        if tracked {
            word.fetch_or(mask, Ordering::Release);
        } else {
            word.fetch_and(!mask, Ordering::Release);
        }
    }

    fn overlaps(&self, start: usize, end: usize) -> bool {
        self.mappings.read(|mappings| {
            mappings
                .iter()
                .any(|mapping| start < mapping.end && mapping.start < end)
        })
    }

    pub(super) fn descriptor_state(&self, descriptor: libc::c_int) -> Option<bool> {
        let Ok(descriptor) = usize::try_from(descriptor) else {
            return Some(false);
        };
        let word = self.descriptors.get(descriptor / DESCRIPTOR_WORD_BITS)?;
        let mask = 1_u64 << (descriptor % DESCRIPTOR_WORD_BITS);
        Some(word.load(Ordering::Acquire) & mask != 0)
    }
}

impl FilesystemHookRuntime {
    fn try_mapping_operation(&self) -> Option<MutexGuard<'_, ()>> {
        match self.mapping_operations.try_lock() {
            Ok(operation) => Some(operation),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
}

impl FilesystemHookRuntime {
    fn prepare_mapping(
        &self,
        length: usize,
        protection: libc::c_int,
        flags: libc::c_int,
        descriptor: libc::c_int,
        offset: libc::off_t,
    ) -> Result<Option<PendingMapping>> {
        if offset < 0 || length == 0 {
            return Ok(None);
        }
        let Some(open) = self.tracked_open(descriptor) else {
            return Ok(None);
        };
        let file_offset = offset as u64;
        let file_end = file_offset
            .checked_add(u64::try_from(length).context("memory mapping length overflowed")?)
            .context("memory mapping file range overflowed")?;
        let range = LocalByteRange::new(file_offset, file_end)?;
        open.managed()
            .prepare_mapping(self, descriptor, range, protection, flags)?;
        if flags & libc::MAP_SHARED == 0 {
            return Ok(None);
        }
        let writable = open.managed().writable();
        if writable {
            self.register_potential_range(&open, file_offset, file_end)?;
        }
        Ok(Some(PendingMapping {
            file_offset,
            writable,
            open,
        }))
    }

    fn register_mapping(
        &self,
        address: *mut libc::c_void,
        length: usize,
        pending: Option<PendingMapping>,
    ) -> Result<()> {
        let Some(pending) = pending else {
            return Ok(());
        };
        let start = address as usize;
        let end = start
            .checked_add(length)
            .context("memory mapping address overflowed")?;
        let mut mappings = lock(&self.mappings);
        mappings.push(MemoryMapping {
            start,
            end,
            file_offset: pending.file_offset,
            writable: pending.writable,
            open: pending.open,
        });
        self.memory_index.publish_mappings(&mappings);
        Ok(())
    }

    fn register_potential_range(&self, open: &OpenFile, start: u64, end: u64) -> Result<()> {
        let range = LocalByteRange::new(start, end)?;
        open.managed().prepare_writable_mapping(self, range)
    }

    fn mapping_slices(&self, start: usize, end: usize, writable_only: bool) -> Vec<MappingSlice> {
        lock(&self.mappings)
            .iter()
            .filter(|mapping| !writable_only || mapping.writable)
            .filter_map(|mapping| {
                let overlap_start = start.max(mapping.start);
                let overlap_end = end.min(mapping.end);
                (overlap_start < overlap_end).then(|| MappingSlice {
                    address: overlap_start,
                    length: overlap_end - overlap_start,
                    open: Arc::clone(&mapping.open),
                })
            })
            .collect()
    }

    fn sync_mapping_slices(&self, slices: &[MappingSlice], durable: bool) -> Result<()> {
        let mut files: Vec<Arc<OpenFile>> = Vec::new();
        for slice in slices {
            if !files.iter().any(|open| Arc::ptr_eq(open, &slice.open)) {
                files.push(Arc::clone(&slice.open));
            }
        }
        for open in files {
            self.commit_open_file(-1, &open, durable)?;
        }
        Ok(())
    }

    fn flush_mapping_slices(&self, slices: &[MappingSlice], durable: bool) -> Result<()> {
        Self::flush_native_mapping_slices(slices)?;
        self.sync_mapping_slices(slices, durable)
    }

    fn flush_native_mapping_slices(slices: &[MappingSlice]) -> Result<()> {
        let msync = original_msync().context("msync is unavailable")?;
        for slice in slices {
            if unsafe {
                msync(
                    slice.address as *mut libc::c_void,
                    slice.length,
                    libc::MS_SYNC,
                )
            } != 0
            {
                return Err(io::Error::last_os_error().into());
            }
        }
        Ok(())
    }

    fn full_mapping_slices(&self, include: impl Fn(&MemoryMapping) -> bool) -> Vec<MappingSlice> {
        lock(&self.mappings)
            .iter()
            .filter(|mapping| mapping.writable && include(mapping))
            .map(|mapping| MappingSlice {
                address: mapping.start,
                length: mapping.end - mapping.start,
                open: Arc::clone(&mapping.open),
            })
            .collect()
    }

    pub(super) fn flush_open_mappings(&self, open: &Arc<OpenFile>, durable: bool) -> Result<()> {
        let _operation = lock(&self.mapping_operations);
        let slices = self.full_mapping_slices(|mapping| Arc::ptr_eq(&mapping.open, open));
        self.flush_mapping_slices(&slices, durable)
    }

    pub(super) fn flush_logical_mappings(&self, logical: &Path, durable: bool) -> Result<()> {
        let _operation = lock(&self.mapping_operations);
        let mappings = lock(&self.mappings).clone();
        let slices = mappings
            .iter()
            .filter(|mapping| mapping.writable && mapping.open.logical() == logical)
            .map(|mapping| MappingSlice {
                address: mapping.start,
                length: mapping.end - mapping.start,
                open: Arc::clone(&mapping.open),
            })
            .collect::<Vec<_>>();
        self.flush_mapping_slices(&slices, durable)
    }

    fn remove_mappings(&self, start: usize, end: usize) -> Vec<Arc<OpenFile>> {
        let mut mappings = lock(&self.mappings);
        let mut retained = Vec::with_capacity(mappings.len() + 1);
        let mut affected = Vec::new();
        for mapping in mappings.drain(..) {
            let overlap_start = start.max(mapping.start);
            let overlap_end = end.min(mapping.end);
            if overlap_start >= overlap_end {
                retained.push(mapping);
                continue;
            }
            if !affected.iter().any(|open| Arc::ptr_eq(open, &mapping.open)) {
                affected.push(Arc::clone(&mapping.open));
            }
            if mapping.start < overlap_start {
                retained.push(MemoryMapping {
                    end: overlap_start,
                    ..mapping.clone()
                });
            }
            if overlap_end < mapping.end {
                retained.push(MemoryMapping {
                    start: overlap_end,
                    file_offset: mapping.file_offset + (overlap_end - mapping.start) as u64,
                    ..mapping
                });
            }
        }
        *mappings = retained;
        self.memory_index.publish_mappings(&mappings);
        affected
    }

    pub(super) fn has_mapping(&self, open: &Arc<OpenFile>) -> bool {
        lock(&self.mappings)
            .iter()
            .any(|mapping| Arc::ptr_eq(&mapping.open, open))
    }

    fn has_descriptor(&self, open: &Arc<OpenFile>) -> bool {
        lock(&self.open_files)
            .values()
            .any(|candidate| Arc::ptr_eq(candidate, open))
    }

    pub(super) fn finish_unreferenced(&self, opens: Vec<Arc<OpenFile>>) -> Result<()> {
        for open in opens {
            if !self.has_descriptor(&open) && !self.has_mapping(&open) {
                self.finish_open_file(-1, &open)?;
            }
        }
        Ok(())
    }

    pub(super) fn flush_memory_mappings(&self) -> Result<()> {
        let _operation = lock(&self.mapping_operations);
        let slices = self.full_mapping_slices(|_| true);
        self.flush_mapping_slices(&slices, true)
    }

    #[cfg(any(agora_sandbox_hook_build, test, coverage))]
    pub(super) fn flush_native_memory_mappings(&self) -> Result<()> {
        let _operation = lock(&self.mapping_operations);
        let slices = self.full_mapping_slices(|_| true);
        Self::flush_native_mapping_slices(&slices)
    }
}

#[derive(Clone, Copy)]
enum MemoryRuntimeState {
    Inactive,
    Ready(&'static FilesystemHookRuntime),
    #[cfg(not(test))]
    Unavailable,
}

#[derive(Clone, Copy)]
enum MemoryRoute {
    Native,
    Managed(&'static FilesystemHookRuntime),
    #[cfg_attr(test, allow(dead_code))]
    Busy,
}

#[cfg(test)]
fn memory_runtime_state() -> MemoryRuntimeState {
    let runtime = TEST_FILESYSTEM_RUNTIME.with(Cell::get);
    if runtime.is_null() {
        MemoryRuntimeState::Inactive
    } else {
        MemoryRuntimeState::Ready(unsafe { &*runtime })
    }
}

#[cfg(not(test))]
fn memory_runtime_state() -> MemoryRuntimeState {
    if !super::super::initialized() {
        return MemoryRuntimeState::Inactive;
    }
    if super::super::config::global().is_none() {
        return MemoryRuntimeState::Inactive;
    }
    match FILESYSTEM_RUNTIME.get().and_then(Option::as_ref) {
        Some(runtime) => MemoryRuntimeState::Ready(runtime),
        None => MemoryRuntimeState::Unavailable,
    }
}

fn mapping_range_route(start: usize, end: usize) -> MemoryRoute {
    let runtime = match memory_runtime_state() {
        MemoryRuntimeState::Inactive => return MemoryRoute::Native,
        MemoryRuntimeState::Ready(runtime) => runtime,
        #[cfg(not(test))]
        MemoryRuntimeState::Unavailable => return MemoryRoute::Busy,
    };
    if runtime.memory_index.overlaps(start, end) {
        MemoryRoute::Managed(runtime)
    } else {
        MemoryRoute::Native
    }
}

fn mmap_route(
    address: *mut libc::c_void,
    length: usize,
    flags: libc::c_int,
    descriptor: libc::c_int,
) -> MemoryRoute {
    let fixed = flags & libc::MAP_FIXED != 0;
    let file_backed = flags & libc::MAP_ANON == 0;
    if !fixed && !file_backed {
        return MemoryRoute::Native;
    }

    let runtime = match memory_runtime_state() {
        MemoryRuntimeState::Inactive => return MemoryRoute::Native,
        MemoryRuntimeState::Ready(runtime) => runtime,
        #[cfg(not(test))]
        MemoryRuntimeState::Unavailable => return MemoryRoute::Busy,
    };
    if fixed {
        let end = (address as usize) + length;
        if runtime.memory_index.overlaps(address as usize, end) {
            return MemoryRoute::Managed(runtime);
        }
    }
    if !file_backed {
        return MemoryRoute::Native;
    }

    match runtime.memory_index.descriptor_state(descriptor) {
        Some(true) | None => MemoryRoute::Managed(runtime),
        Some(false) => MemoryRoute::Native,
    }
}

unsafe fn fail_closed_memory<T>(failure: T) -> T {
    unsafe { set_errno(libc::EAGAIN) };
    failure
}

unsafe fn sandbox_mmap(
    address: *mut libc::c_void,
    length: usize,
    protection: libc::c_int,
    flags: libc::c_int,
    descriptor: libc::c_int,
    offset: libc::off_t,
) -> *mut libc::c_void {
    let Some(original) = original_mmap() else {
        unsafe { set_errno(libc::ENOSYS) };
        return libc::MAP_FAILED;
    };
    if flags & libc::MAP_FIXED != 0 && (address as usize).checked_add(length).is_none() {
        unsafe { set_errno(libc::EOVERFLOW) };
        return libc::MAP_FAILED;
    }
    let route = {
        let _signals = super::super::SignalMaskGuard::block_or_abort();
        mmap_route(address, length, flags, descriptor)
    };
    let runtime = match route {
        MemoryRoute::Native => {
            return unsafe { original(address, length, protection, flags, descriptor, offset) };
        }
        MemoryRoute::Managed(runtime) => runtime,
        MemoryRoute::Busy => return unsafe { fail_closed_memory(libc::MAP_FAILED) },
    };

    catch_filesystem_panic(libc::MAP_FAILED, || {
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { fail_closed_memory(libc::MAP_FAILED) };
        };
        let Some(operation) = runtime.try_mapping_operation() else {
            return unsafe { fail_closed_memory(libc::MAP_FAILED) };
        };
        match mmap_route(address, length, flags, descriptor) {
            MemoryRoute::Native => {
                return unsafe { original(address, length, protection, flags, descriptor, offset) };
            }
            MemoryRoute::Managed(_) => {}
            MemoryRoute::Busy => return unsafe { fail_closed_memory(libc::MAP_FAILED) },
        }
        let pending = match runtime.prepare_mapping(length, protection, flags, descriptor, offset) {
            Ok(pending) => pending,
            Err(error) => return unsafe { fail(&error, libc::MAP_FAILED) },
        };
        if flags & libc::MAP_FIXED != 0
            && let Err(error) = sync_native_mappings(runtime, address as usize, length, true)
        {
            return unsafe { fail(&error, libc::MAP_FAILED) };
        }
        let mapped = unsafe { original(address, length, protection, flags, descriptor, offset) };
        if mapped == libc::MAP_FAILED {
            return mapped;
        }
        let start = mapped as usize;
        let Some(end) = start.checked_add(length) else {
            if let Some(munmap) = original_munmap() {
                unsafe { munmap(mapped, length) };
            }
            unsafe { set_errno(libc::EOVERFLOW) };
            return libc::MAP_FAILED;
        };
        let replaced = if flags & libc::MAP_FIXED != 0 {
            runtime.remove_mappings(start, end)
        } else {
            Vec::new()
        };
        if let Err(error) = runtime.register_mapping(mapped, length, pending) {
            if let Some(munmap) = original_munmap() {
                unsafe { munmap(mapped, length) };
            }
            drop(operation);
            let _ = runtime.finish_unreferenced(replaced);
            return unsafe { fail(&error, libc::MAP_FAILED) };
        }
        drop(operation);
        let _ = runtime.finish_unreferenced(replaced);
        mapped
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_mmap(
    address: *mut libc::c_void,
    length: usize,
    protection: libc::c_int,
    flags: libc::c_int,
    descriptor: libc::c_int,
    offset: libc::off_t,
) -> *mut libc::c_void {
    unsafe { sandbox_mmap(address, length, protection, flags, descriptor, offset) }
}

unsafe fn sandbox_msync(
    address: *mut libc::c_void,
    length: usize,
    flags: libc::c_int,
) -> libc::c_int {
    let Some(original) = original_msync() else {
        unsafe { set_errno(libc::ENOSYS) };
        return -1;
    };
    let Some(end) = (address as usize).checked_add(length) else {
        unsafe { set_errno(libc::EOVERFLOW) };
        return -1;
    };
    let route = {
        let _signals = super::super::SignalMaskGuard::block_or_abort();
        mapping_range_route(address as usize, end)
    };
    let runtime = match route {
        MemoryRoute::Native => return unsafe { original(address, length, flags) },
        MemoryRoute::Managed(runtime) => runtime,
        MemoryRoute::Busy => return unsafe { fail_closed_memory(-1) },
    };

    catch_filesystem_panic(-1, || {
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { fail_closed_memory(-1) };
        };
        let Some(_operation) = runtime.try_mapping_operation() else {
            return unsafe { fail_closed_memory(-1) };
        };
        match mapping_range_route(address as usize, end) {
            MemoryRoute::Native => return unsafe { original(address, length, flags) },
            MemoryRoute::Managed(_) => {}
            MemoryRoute::Busy => return unsafe { fail_closed_memory(-1) },
        }
        let result = unsafe { original(address, length, flags) };
        if result != 0 {
            return result;
        }
        let slices = runtime.mapping_slices(address as usize, end, true);
        match runtime.sync_mapping_slices(&slices, flags & libc::MS_SYNC != 0) {
            Ok(()) => result,
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_msync(
    address: *mut libc::c_void,
    length: usize,
    flags: libc::c_int,
) -> libc::c_int {
    unsafe { sandbox_msync(address, length, flags) }
}

unsafe fn sandbox_munmap(address: *mut libc::c_void, length: usize) -> libc::c_int {
    let Some(original) = original_munmap() else {
        unsafe { set_errno(libc::ENOSYS) };
        return -1;
    };
    let Some(end) = (address as usize).checked_add(length) else {
        unsafe { set_errno(libc::EOVERFLOW) };
        return -1;
    };
    let route = {
        let _signals = super::super::SignalMaskGuard::block_or_abort();
        mapping_range_route(address as usize, end)
    };
    let runtime = match route {
        MemoryRoute::Native => return unsafe { original(address, length) },
        MemoryRoute::Managed(runtime) => runtime,
        MemoryRoute::Busy => return unsafe { fail_closed_memory(-1) },
    };

    catch_filesystem_panic(-1, || {
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { fail_closed_memory(-1) };
        };
        let Some(operation) = runtime.try_mapping_operation() else {
            return unsafe { fail_closed_memory(-1) };
        };
        match mapping_range_route(address as usize, end) {
            MemoryRoute::Native => return unsafe { original(address, length) },
            MemoryRoute::Managed(_) => {}
            MemoryRoute::Busy => return unsafe { fail_closed_memory(-1) },
        }
        if let Err(error) = sync_native_mappings(runtime, address as usize, length, true) {
            return unsafe { fail(&error, -1) };
        }
        let result = unsafe { original(address, length) };
        if result != 0 {
            return result;
        }
        let affected = runtime.remove_mappings(address as usize, end);
        drop(operation);
        let _ = runtime.finish_unreferenced(affected);
        result
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_munmap(
    address: *mut libc::c_void,
    length: usize,
) -> libc::c_int {
    unsafe { sandbox_munmap(address, length) }
}

fn sync_native_mappings(
    runtime: &FilesystemHookRuntime,
    start: usize,
    length: usize,
    durable: bool,
) -> Result<()> {
    let end = start
        .checked_add(length)
        .context("memory mapping address overflowed")?;
    let slices = runtime.mapping_slices(start, end, true);
    runtime.flush_mapping_slices(&slices, durable)
}

fn original_mmap() -> Option<MmapFn> {
    function_from_interpose(&INTERPOSE_MMAP)
}

fn original_msync() -> Option<MemoryFlagsFn> {
    function_from_interpose(&INTERPOSE_MSYNC)
}

fn original_munmap() -> Option<MemoryFn> {
    function_from_interpose(&INTERPOSE_MUNMAP)
}

dyld_interpose!(INTERPOSE_MMAP, agora_sandbox_mmap, libc::mmap);
dyld_interpose!(INTERPOSE_MSYNC, agora_sandbox_msync, libc::msync);
dyld_interpose!(INTERPOSE_MUNMAP, agora_sandbox_munmap, libc::munmap);

#[cfg(test)]
mod tests;
