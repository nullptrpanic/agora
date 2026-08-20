use super::*;

mod coordination;

pub(super) use coordination::{OperationCoordinator, OperationLease, OperationRequest};

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
const DESCRIPTOR_STATE_BITS: usize = 2;
const DESCRIPTORS_PER_WORD: usize = DESCRIPTOR_WORD_BITS / DESCRIPTOR_STATE_BITS;
const DATA_TRACKED_BIT: u64 = 1;
const MAPPING_MANAGED_BIT: u64 = 1 << 1;

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
            descriptors: (0..DESCRIPTOR_INDEX_BITS / DESCRIPTORS_PER_WORD)
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

    pub(super) fn set_descriptor(
        &self,
        descriptor: libc::c_int,
        data_tracked: bool,
        mapping_managed: bool,
    ) {
        let Ok(descriptor) = usize::try_from(descriptor) else {
            return;
        };
        let Some(word) = self.descriptors.get(descriptor / DESCRIPTORS_PER_WORD) else {
            return;
        };
        let shift = (descriptor % DESCRIPTORS_PER_WORD) * DESCRIPTOR_STATE_BITS;
        let mask = (DATA_TRACKED_BIT | MAPPING_MANAGED_BIT) << shift;
        let state = ((u64::from(data_tracked) * DATA_TRACKED_BIT)
            | (u64::from(mapping_managed) * MAPPING_MANAGED_BIT))
            << shift;
        let _ = word.fetch_update(Ordering::Release, Ordering::Relaxed, |current| {
            Some((current & !mask) | state)
        });
    }

    fn overlaps(&self, start: usize, end: usize) -> bool {
        self.mappings.read(|mappings| {
            mappings
                .iter()
                .any(|mapping| start < mapping.end && mapping.start < end)
        })
    }

    pub(super) fn data_descriptor_state(&self, descriptor: libc::c_int) -> Option<bool> {
        self.descriptor_routing_state(descriptor)
            .map(|(data_tracked, _)| data_tracked)
    }

    pub(super) fn descriptor_routing_state(&self, descriptor: libc::c_int) -> Option<(bool, bool)> {
        let Ok(descriptor) = usize::try_from(descriptor) else {
            return Some((false, false));
        };
        let word = self.descriptors.get(descriptor / DESCRIPTORS_PER_WORD)?;
        let shift = (descriptor % DESCRIPTORS_PER_WORD) * DESCRIPTOR_STATE_BITS;
        let state = word.load(Ordering::Acquire) >> shift;
        Some((
            state & DATA_TRACKED_BIT != 0,
            state & MAPPING_MANAGED_BIT != 0,
        ))
    }

    fn mapping_descriptor_state(&self, descriptor: libc::c_int) -> Option<bool> {
        self.descriptor_routing_state(descriptor)
            .map(|(_, mapping_managed)| mapping_managed)
    }
}

impl FilesystemHookRuntime {
    fn mmap_operation_request(
        address: *mut libc::c_void,
        length: usize,
        flags: libc::c_int,
        descriptor: libc::c_int,
    ) -> OperationRequest {
        let mut request = OperationRequest::new().mapping_registry_shared();
        if flags & libc::MAP_ANON == 0 {
            request = request
                .descriptor_registry_shared()
                .descriptor_shared(descriptor);
        }
        if flags & libc::MAP_FIXED != 0 {
            request = request.address_exclusive(address as usize, address as usize + length);
        }
        request
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
        let writable = open
            .managed()
            .prepare_mapping(self, descriptor, range, protection, flags)?;
        if flags & libc::MAP_SHARED == 0 {
            return Ok(None);
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

    fn mapping_snapshot_request(&self, slices: &[MappingSlice]) -> OperationRequest {
        slices.iter().fold(
            OperationRequest::new().mapping_registry_shared(),
            |request, slice| request.address_shared(slice.address, slice.address + slice.length),
        )
    }

    fn revalidate_mapping_slices(&self, expected: &[MappingSlice]) -> Vec<MappingSlice> {
        let mappings = lock(&self.mappings);
        let mut slices = Vec::new();
        for expected in expected {
            let expected_end = expected.address + expected.length;
            for mapping in mappings
                .iter()
                .filter(|mapping| Arc::ptr_eq(&mapping.open, &expected.open))
            {
                let start = expected.address.max(mapping.start);
                let end = expected_end.min(mapping.end);
                if start < end
                    && !slices.iter().any(|slice: &MappingSlice| {
                        slice.address == start
                            && slice.length == end - start
                            && Arc::ptr_eq(&slice.open, &mapping.open)
                    })
                {
                    slices.push(MappingSlice {
                        address: start,
                        length: end - start,
                        open: Arc::clone(&mapping.open),
                    });
                }
            }
        }
        slices
    }

    fn flush_mapping_snapshot(&self, expected: &[MappingSlice], durable: bool) -> Result<()> {
        let _operation = self
            .operations
            .acquire(self.mapping_snapshot_request(expected));
        let slices = self.revalidate_mapping_slices(expected);
        self.flush_mapping_slices(&slices, durable)
    }

    #[cfg(any(agora_sandbox_hook_build, test, coverage))]
    fn flush_native_mapping_snapshot(&self, expected: &[MappingSlice]) -> Result<()> {
        let _operation = self
            .operations
            .acquire(self.mapping_snapshot_request(expected));
        let slices = self.revalidate_mapping_slices(expected);
        Self::flush_native_mapping_slices(&slices)
    }

    pub(super) fn flush_logical_mappings(&self, logical: &Path, durable: bool) -> Result<()> {
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
        self.flush_mapping_snapshot(&slices, durable)
    }

    #[cfg(test)]
    fn remove_mappings(&self, start: usize, end: usize) -> Vec<Arc<OpenFile>> {
        let mut mappings = lock(&self.mappings);
        let affected = Self::remove_mappings_locked(&mut mappings, start, end);
        self.memory_index.publish_mappings(&mappings);
        affected
    }

    fn remove_mappings_locked(
        mappings: &mut Vec<MemoryMapping>,
        start: usize,
        end: usize,
    ) -> Vec<Arc<OpenFile>> {
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
            let claimed = {
                let _barrier = self.operations.acquire(
                    OperationRequest::new()
                        .descriptor_registry_exclusive()
                        .mapping_registry_exclusive(),
                );
                !self.has_descriptor(&open)
                    && !self.has_mapping(&open)
                    && !open.finished.swap(true, Ordering::AcqRel)
            };
            if claimed {
                self.finish_claimed_open_file(-1, &open)?;
            }
        }
        Ok(())
    }

    pub(super) fn flush_memory_mappings(&self) -> Result<()> {
        let slices = {
            let _barrier = self
                .operations
                .acquire(OperationRequest::new().mapping_registry_exclusive());
            self.full_mapping_slices(|_| true)
        };
        self.flush_mapping_snapshot(&slices, true)
    }

    #[cfg(any(agora_sandbox_hook_build, test, coverage))]
    pub(super) fn flush_native_memory_mappings(&self) -> Result<()> {
        let slices = {
            let _barrier = self
                .operations
                .acquire(OperationRequest::new().mapping_registry_exclusive());
            self.full_mapping_slices(|_| true)
        };
        self.flush_native_mapping_snapshot(&slices)
    }

    pub(super) fn writeback_descriptor(
        &self,
        descriptor: libc::c_int,
    ) -> Result<Option<Arc<OpenFile>>> {
        loop {
            let expected = self.tracked_open(descriptor);
            let slices = expected
                .as_ref()
                .map(|open| self.full_mapping_slices(|mapping| Arc::ptr_eq(&mapping.open, open)))
                .unwrap_or_default();
            let request = slices.iter().fold(
                OperationRequest::new()
                    .descriptor_registry_shared()
                    .mapping_registry_shared()
                    .descriptor_shared(descriptor),
                |request, slice| {
                    request.address_shared(slice.address, slice.address + slice.length)
                },
            );
            let _operation = self.operations.acquire(request);
            let current = self.tracked_open(descriptor);
            let unchanged = match (&expected, &current) {
                (Some(expected), Some(current)) => Arc::ptr_eq(expected, current),
                (None, None) => true,
                _ => false,
            };
            if !unchanged {
                continue;
            }
            let Some(open) = current else {
                return Ok(None);
            };
            let slices = self.revalidate_mapping_slices(&slices);
            self.flush_mapping_slices(&slices, true)?;
            self.commit_open_file(descriptor, &open, true)?;
            return Ok(Some(open));
        }
    }

    pub(super) fn acquire_descriptor_replacement(
        &self,
        source: Option<libc::c_int>,
        destination: libc::c_int,
    ) -> Result<OperationLease<'_>> {
        loop {
            let expected = self.tracked_open(destination);
            let slices = expected
                .as_ref()
                .map(|open| self.full_mapping_slices(|mapping| Arc::ptr_eq(&mapping.open, open)))
                .unwrap_or_default();
            let mut request = OperationRequest::new()
                .descriptor_registry_shared()
                .mapping_registry_shared()
                .descriptor_exclusive(destination);
            if let Some(source) = source {
                request = request.descriptor_shared(source);
            }
            request = slices.iter().fold(request, |request, slice| {
                request.address_shared(slice.address, slice.address + slice.length)
            });
            let operation = self.operations.acquire(request);
            let current = self.tracked_open(destination);
            let unchanged = match (&expected, &current) {
                (Some(expected), Some(current)) => Arc::ptr_eq(expected, current),
                (None, None) => true,
                _ => false,
            };
            if !unchanged {
                drop(operation);
                continue;
            }
            if let Some(open) = current {
                let slices = self.revalidate_mapping_slices(&slices);
                self.flush_mapping_slices(&slices, true)?;
                self.commit_open_file(destination, &open, true)?;
            }
            return Ok(operation);
        }
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

    match runtime.memory_index.mapping_descriptor_state(descriptor) {
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
        let operation = runtime
            .operations
            .acquire(FilesystemHookRuntime::mmap_operation_request(
                address, length, flags, descriptor,
            ));
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
        let fixed = flags & libc::MAP_FIXED != 0;
        let (mapped, replaced) = if fixed {
            let start = address as usize;
            let end = start + length;
            let mut mappings = lock(&runtime.mappings);
            let provisional = pending.as_ref().map(|pending| {
                mappings.push(MemoryMapping {
                    start,
                    end,
                    file_offset: pending.file_offset,
                    writable: pending.writable,
                    open: Arc::clone(&pending.open),
                });
                mappings.len() - 1
            });
            if provisional.is_some() {
                runtime.memory_index.publish_mappings(&mappings);
            }
            let mapped =
                unsafe { original(address, length, protection, flags, descriptor, offset) };
            if mapped == libc::MAP_FAILED {
                let errno = unsafe { *libc::__error() };
                if let Some(provisional) = provisional {
                    mappings.remove(provisional);
                    runtime.memory_index.publish_mappings(&mappings);
                }
                unsafe { set_errno(errno) };
                return mapped;
            }
            let replaced = FilesystemHookRuntime::remove_mappings_locked(&mut mappings, start, end);
            if let Some(pending) = pending {
                mappings.push(MemoryMapping {
                    start,
                    end,
                    file_offset: pending.file_offset,
                    writable: pending.writable,
                    open: pending.open,
                });
            }
            runtime.memory_index.publish_mappings(&mappings);
            (mapped, replaced)
        } else {
            let mapped =
                unsafe { original(address, length, protection, flags, descriptor, offset) };
            if mapped == libc::MAP_FAILED {
                return mapped;
            }
            let start = mapped as usize;
            let Some(_end) = start.checked_add(length) else {
                if let Some(munmap) = original_munmap() {
                    unsafe { munmap(mapped, length) };
                }
                unsafe { set_errno(libc::EOVERFLOW) };
                return libc::MAP_FAILED;
            };
            if let Err(error) = runtime.register_mapping(mapped, length, pending) {
                if let Some(munmap) = original_munmap() {
                    unsafe { munmap(mapped, length) };
                }
                return unsafe { fail(&error, libc::MAP_FAILED) };
            }
            (mapped, Vec::new())
        };
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
        let _operation = runtime.operations.acquire(
            OperationRequest::new()
                .mapping_registry_shared()
                .address_shared(address as usize, end),
        );
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
        let operation = runtime.operations.acquire(
            OperationRequest::new()
                .mapping_registry_shared()
                .address_exclusive(address as usize, end),
        );
        match mapping_range_route(address as usize, end) {
            MemoryRoute::Native => return unsafe { original(address, length) },
            MemoryRoute::Managed(_) => {}
            MemoryRoute::Busy => return unsafe { fail_closed_memory(-1) },
        }
        if let Err(error) = sync_native_mappings(runtime, address as usize, length, true) {
            return unsafe { fail(&error, -1) };
        }
        let mut mappings = lock(&runtime.mappings);
        let result = unsafe { original(address, length) };
        if result != 0 {
            return result;
        }
        let affected =
            FilesystemHookRuntime::remove_mappings_locked(&mut mappings, address as usize, end);
        runtime.memory_index.publish_mappings(&mappings);
        drop(mappings);
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
