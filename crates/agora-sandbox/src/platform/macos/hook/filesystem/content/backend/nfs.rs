use super::super::super::data::{
    current_offset, positional_write_range, sequential_write_range, set_current_offset_after_io,
};
use super::super::super::{FilesystemHookRuntime, LocalByteRange, OpenFile, lock};
use super::super::io::{errno_error, read_materialization_length};
use super::super::state::ContentState;
use super::{ContentBackend, ContentIoOffset, ReadOperations, TruncateOperation, WriteOperations};
use crate::nfs::protocol::{MAX_REMOTE_IO_BYTES, RemoteMetadata};
use anyhow::{Context, Result};
use std::os::fd::AsRawFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct NfsContent {
    pub(crate) handle: String,
    pub(crate) metadata: Mutex<RemoteMetadata>,
    pub(crate) snapshot: AtomicBool,
    pub(crate) operation: Mutex<()>,
}

impl ContentBackend for NfsContent {
    fn read(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        requested_offset: ContentIoOffset,
        operations: &mut ReadOperations<'_>,
    ) -> Result<libc::ssize_t> {
        let flags = descriptor_flags(descriptor)?;
        if flags & libc::O_ACCMODE == libc::O_WRONLY {
            return Err(errno_error(libc::EBADF));
        }
        let offset = resolve_offset(descriptor, requested_offset)?;
        let length = (operations.requested_length)().map_err(errno_error)?;
        if length == 0 {
            return Ok(0);
        }
        if self.snapshot.load(Ordering::Acquire) {
            let requested = u64::try_from(length).unwrap_or(u64::MAX);
            let end = offset.saturating_add(read_materialization_length(requested));
            let range = LocalByteRange::new(offset, end).expect("non-empty remote read range");
            self.materialize(state, runtime, Some(range))?;
            return Ok((operations.native)());
        }
        let length = length.min(MAX_REMOTE_IO_BYTES as usize);
        let (payload, available) = runtime
            .remote
            .as_ref()
            .context("remote filesystem runtime is unavailable")?
            .read(
                &self.handle,
                offset,
                u32::try_from(length).expect("remote read is protocol bounded"),
            )?;
        let result = (operations.copy_from_payload)(payload.as_raw_fd(), available as usize);
        if result > 0 && matches!(requested_offset, ContentIoOffset::Sequential) {
            set_offset_after_io(descriptor, offset, result as u64)?;
        }
        Ok(result)
    }

    fn write(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        requested_offset: ContentIoOffset,
        _reservation_length: Option<usize>,
        operations: &mut WriteOperations<'_>,
    ) -> Result<libc::ssize_t> {
        let flags = descriptor_flags(descriptor)?;
        if flags & libc::O_ACCMODE == libc::O_RDONLY || !state.writable {
            return Err(errno_error(libc::EBADF));
        }
        if self.snapshot.load(Ordering::Acquire) {
            let before = current_offset(descriptor);
            let result = (operations.native)();
            if result > 0 {
                let written = match requested_offset {
                    ContentIoOffset::Sequential => {
                        sequential_write_range(descriptor, before, result)
                    }
                    ContentIoOffset::Positioned(offset) => positional_write_range(offset, result),
                };
                if let Some((start, end)) = written {
                    record_write_bounds(state, start, end);
                }
            }
            if result > 0 && flags & (libc::O_SYNC | libc::O_DSYNC) != 0 {
                self.sync_dirty(state, runtime)?;
            }
            return Ok(result);
        }
        let length = (operations.requested_length)()
            .map_err(errno_error)?
            .min(MAX_REMOTE_IO_BYTES as usize);
        if length == 0 {
            return Ok(0);
        }
        let payload = tempfile::tempfile()?;
        let copied = (operations.copy_to_payload)(payload.as_raw_fd(), length);
        if copied <= 0 {
            return Ok(copied);
        }
        let copied = u32::try_from(copied).expect("remote write is protocol bounded");
        payload.set_len(u64::from(copied))?;
        let offset = match requested_offset {
            ContentIoOffset::Sequential if flags & libc::O_APPEND != 0 => None,
            _ => Some(resolve_offset(descriptor, requested_offset)?),
        };
        let remote = runtime
            .remote
            .as_ref()
            .context("remote filesystem runtime is unavailable")?;
        let (actual_offset, written, mut size) =
            remote.write(&self.handle, offset, &payload, copied)?;
        if written > copied {
            return Err(errno_error(libc::EIO));
        }
        if flags & (libc::O_SYNC | libc::O_DSYNC) != 0
            && let Some(metadata) = remote.sync(&self.handle, Vec::new())?
        {
            size = metadata.size;
        }
        let local_size = libc::off_t::try_from(size).map_err(|_| errno_error(libc::EFBIG))?;
        if unsafe { libc::ftruncate(descriptor, local_size) } != 0 {
            return Ok(-1);
        }
        lock(&self.metadata).size = size;
        if matches!(requested_offset, ContentIoOffset::Sequential) {
            set_offset_after_io(descriptor, actual_offset, u64::from(written))?;
        }
        Ok(written as libc::ssize_t)
    }

    fn seek(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        _descriptor: libc::c_int,
        _requested_offset: libc::off_t,
        whence: libc::c_int,
        native: &mut dyn FnMut() -> libc::off_t,
    ) -> Result<libc::off_t> {
        if matches!(whence, libc::SEEK_DATA | libc::SEEK_HOLE) {
            self.materialize(state, runtime, None)?;
        }
        Ok(native())
    }

    fn truncate(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        operation: &mut TruncateOperation<'_>,
    ) -> Result<libc::c_int> {
        if !self.snapshot.load(Ordering::Acquire) {
            let size = runtime
                .remote
                .as_ref()
                .context("remote filesystem runtime is unavailable")?
                .set_length(&self.handle, operation.requested_length)?;
            let result = (operation.native)();
            if result == 0 {
                lock(&self.metadata).size = size;
            }
            return Ok(result);
        }
        let result = (operation.native)();
        if result != 0 {
            return Ok(result);
        }
        if let Some(range) = operation.reservation {
            state.record_write(range);
        }
        runtime.refresh_attributes(
            operation.descriptor,
            operation.open.logical().to_string_lossy().as_ref(),
        )?;
        self.sync(state, runtime, operation.descriptor, operation.open, true)?;
        Ok(result)
    }

    fn prepare_mapping(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        range: LocalByteRange,
        protection: libc::c_int,
        flags: libc::c_int,
    ) -> Result<()> {
        let access = descriptor_flags(descriptor)? & libc::O_ACCMODE;
        if access == libc::O_WRONLY
            || (flags & libc::MAP_SHARED != 0
                && protection & libc::PROT_WRITE != 0
                && access == libc::O_RDONLY)
        {
            return Err(errno_error(libc::EACCES));
        }
        self.materialize(state, runtime, Some(range))
    }

    fn prepare_writable_mapping(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        range: LocalByteRange,
    ) -> Result<()> {
        if state.writable {
            runtime
                .remote
                .as_ref()
                .context("remote filesystem runtime is unavailable")?
                .potentially_dirty(&self.handle, range)?;
        }
        Ok(())
    }

    fn materialize(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        range: Option<LocalByteRange>,
    ) -> Result<()> {
        if let Some(range) = range
            && lock(&state.materialized).covers(range)
        {
            return Ok(());
        }
        let metadata = runtime
            .remote
            .as_ref()
            .context("remote filesystem runtime is unavailable")?
            .materialize(&self.handle, range)?;
        let materialized = range.unwrap_or(LocalByteRange {
            start: 0,
            end: metadata.size,
        });
        if materialized.start < metadata.size {
            lock(&state.materialized).insert(LocalByteRange {
                start: materialized.start,
                end: materialized.end.min(metadata.size),
            });
        }
        *lock(&self.metadata) = metadata;
        self.snapshot.store(true, Ordering::Release);
        Ok(())
    }

    fn sync(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        _descriptor: libc::c_int,
        _open: &OpenFile,
        _durable: bool,
    ) -> Result<()> {
        self.sync_dirty(state, runtime)
    }

    fn finish(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        _descriptor: libc::c_int,
        _open: &OpenFile,
    ) -> Result<()> {
        let mut dirty = lock(&state.dirty);
        let result = runtime
            .remote
            .as_ref()
            .context("remote filesystem runtime is unavailable")?
            .close(&self.handle, dirty.to_vec());
        if result.is_ok() {
            dirty.clear();
        }
        result
    }

    fn prepare_native_snapshot(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
    ) -> Result<()> {
        self.materialize(state, runtime, None)
    }

    fn records_native_snapshot_writes(&self) -> bool {
        true
    }

    fn operation_lock(&self) -> Option<&Mutex<()>> {
        Some(&self.operation)
    }

    fn publishes_writes(&self) -> bool {
        true
    }

    fn manages_metadata(&self) -> bool {
        true
    }

    fn file_attributes(
        &self,
        runtime: &FilesystemHookRuntime,
    ) -> Result<Option<crate::filesystem::FileAttributes>> {
        let remote = runtime
            .remote
            .as_ref()
            .context("remote filesystem runtime is unavailable")?;
        Ok(Some(remote.attributes(&lock(&self.metadata))))
    }

    fn is_directory(&self) -> bool {
        lock(&self.metadata).file_type == crate::nfs::protocol::RemoteFileType::Directory
    }

    #[cfg(test)]
    fn handle(&self) -> Option<&str> {
        Some(&self.handle)
    }

    fn is_broker_managed(&self) -> bool {
        true
    }
}

impl NfsContent {
    fn sync_dirty(&self, state: &ContentState, runtime: &FilesystemHookRuntime) -> Result<()> {
        let remote = runtime
            .remote
            .as_ref()
            .context("remote filesystem runtime is unavailable")?;
        let mut dirty = lock(&state.dirty);
        if let Some(metadata) = remote.sync(&self.handle, dirty.to_vec())? {
            *lock(&self.metadata) = metadata;
        }
        dirty.clear();
        Ok(())
    }
}

fn descriptor_flags(descriptor: libc::c_int) -> Result<libc::c_int> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(flags)
    }
}

fn resolve_offset(descriptor: libc::c_int, offset: ContentIoOffset) -> Result<u64> {
    match offset {
        ContentIoOffset::Sequential => {
            current_offset(descriptor).ok_or_else(|| errno_error(libc::EIO))
        }
        ContentIoOffset::Positioned(offset) => {
            u64::try_from(offset).map_err(|_| errno_error(libc::EINVAL))
        }
    }
}

fn set_offset_after_io(descriptor: libc::c_int, offset: u64, length: u64) -> Result<()> {
    if unsafe { set_current_offset_after_io(descriptor, offset, length) } {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

fn record_write_bounds(state: &ContentState, start: u64, end: u64) {
    if let Ok(range) = LocalByteRange::new(start, end) {
        state.record_write(range);
    }
}
