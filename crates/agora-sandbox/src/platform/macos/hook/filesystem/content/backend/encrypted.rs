use super::super::super::{FilesystemHookRuntime, LocalByteRange, OpenFile, lock, set_errno};
use super::super::io::{errno_error, positional_reservation_range, read_materialization_length};
use super::super::state::ContentState;
use super::{
    ContentBackend, ContentIoOffset, LocalContentInheritance, ReadOperations, TruncateOperation,
    WriteOperations,
};
use crate::filesystem::Writeback;
use crate::filesystem::broker::{LOCAL_STATUS_FLAGS_MASK, LocalFileIdentity, LocalOpenState};
use anyhow::{Context, Result};
use std::fs::File;
use std::os::fd::AsRawFd;

pub(crate) struct EncryptedContent {
    pub(crate) handle: String,
    pub(crate) lazy: bool,
    pub(crate) state: LocalOpenState,
    pub(crate) lock: File,
    pub(crate) identity: LocalFileIdentity,
}

pub(in super::super) struct EagerEncryptedContent {
    writeback: Writeback,
}

impl EagerEncryptedContent {
    pub(in super::super) fn new(writeback: Writeback) -> Self {
        Self { writeback }
    }

    fn publish(&self, runtime: &FilesystemHookRuntime, descriptor: libc::c_int) -> Result<()> {
        let Some(logical) = runtime.filesystem.commit_writeback(&self.writeback)? else {
            return Ok(());
        };
        if descriptor < 0 {
            return Ok(());
        }
        let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::fstat(descriptor, &mut status) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        runtime.filesystem.refresh_timestamps(&logical, &status)
    }
}

impl ContentBackend for EncryptedContent {
    fn supports_async_write(&self) -> bool {
        false
    }

    fn read(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        _descriptor: libc::c_int,
        requested_offset: ContentIoOffset,
        operations: &mut ReadOperations<'_>,
    ) -> Result<libc::ssize_t> {
        let open_state = self.state.lock()?;
        let flags = open_state.flags()?;
        if flags & libc::O_ACCMODE == libc::O_WRONLY {
            return Err(errno_error(libc::EBADF));
        }
        let offset = match requested_offset {
            ContentIoOffset::Sequential => open_state.offset()?,
            ContentIoOffset::Positioned(offset) => offset,
        };
        if offset < 0 {
            return Err(errno_error(libc::EINVAL));
        }
        if self.lazy {
            let length = (operations.requested_length)().map_err(errno_error)?;
            if length != 0 {
                let start = offset as u64;
                let requested = u64::try_from(length).unwrap_or(u64::MAX);
                let end = start.saturating_add(read_materialization_length(requested));
                let range = LocalByteRange::new(start, end).expect("non-empty read range");
                self.materialize(state, runtime, Some(range))?;
            }
        }
        let result = (operations.positioned)(offset);
        if result > 0 && matches!(requested_offset, ContentIoOffset::Sequential) {
            let next = offset
                .checked_add(result as libc::off_t)
                .ok_or_else(|| errno_error(libc::EOVERFLOW))?;
            open_state.set_offset(next)?;
        }
        Ok(result)
    }

    fn write(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        _descriptor: libc::c_int,
        requested_offset: ContentIoOffset,
        reservation_length: Option<usize>,
        operations: &mut WriteOperations<'_>,
    ) -> Result<libc::ssize_t> {
        if !state.writable {
            return Err(errno_error(libc::EBADF));
        }
        let open_state = self.state.lock()?;
        let flags = open_state.flags()?;
        if flags & libc::O_ACCMODE == libc::O_RDONLY {
            return Err(errno_error(libc::EBADF));
        }
        let local = runtime
            .local
            .as_ref()
            .context("local filesystem runtime is unavailable")?;
        let (active, offset) = match requested_offset {
            ContentIoOffset::Sequential if reservation_length == Some(0) => {
                (None, open_state.offset()?)
            }
            ContentIoOffset::Sequential if flags & libc::O_APPEND != 0 => {
                let (active, offset) = local.begin_append(&self.handle)?;
                let offset = libc::off_t::try_from(offset).map_err(|_| {
                    let _ = local.cancel_write(&self.handle, &active);
                    errno_error(libc::EOVERFLOW)
                })?;
                (Some(active), offset)
            }
            ContentIoOffset::Sequential => {
                let offset = open_state.offset()?;
                if offset < 0 {
                    return Err(errno_error(libc::EINVAL));
                }
                let range = positional_reservation_range(offset as u64, reservation_length);
                let active = range
                    .map(|range| local.begin_write(&self.handle, range))
                    .transpose()?;
                (active, offset)
            }
            ContentIoOffset::Positioned(offset) if offset >= 0 => {
                let range = positional_reservation_range(offset as u64, reservation_length);
                let active = range
                    .map(|range| local.begin_write(&self.handle, range))
                    .transpose()?;
                (active, offset)
            }
            ContentIoOffset::Positioned(_) => return Err(errno_error(libc::EINVAL)),
        };
        if offset < 0 {
            if let Some(active) = &active {
                let _ = local.cancel_write(&self.handle, active);
            }
            return Err(errno_error(libc::EINVAL));
        }
        let result = (operations.positioned)(offset);
        if result > 0 {
            if let Ok(range) =
                LocalByteRange::new(offset as u64, (offset as u64).saturating_add(result as u64))
            {
                let finish_failed = active.as_ref().is_none_or(|write| {
                    if local.finish_write(&self.handle, write, range).is_ok() {
                        false
                    } else {
                        let _ = local.cancel_write(&self.handle, write);
                        true
                    }
                });
                if matches!(requested_offset, ContentIoOffset::Sequential) {
                    let end = libc::off_t::try_from(range.end)
                        .map_err(|_| errno_error(libc::EOVERFLOW))?;
                    open_state.set_offset(end)?;
                }
                if flags & (libc::O_SYNC | libc::O_DSYNC) != 0 {
                    let ranges = finish_failed.then_some(range).into_iter().collect();
                    if let Err(error) = local.sync(&self.handle, ranges, true) {
                        state.record_write(range);
                        return Err(error.into());
                    }
                } else if finish_failed {
                    state.record_write(range);
                }
            }
        } else if let Some(active) = &active {
            let errno = unsafe { *libc::__error() };
            let _ = local.cancel_write(&self.handle, active);
            unsafe { set_errno(errno) };
        }
        Ok(result)
    }

    fn seek(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        requested_offset: libc::off_t,
        whence: libc::c_int,
        native: &mut dyn FnMut() -> libc::off_t,
    ) -> Result<libc::off_t> {
        let open_state = self.state.lock()?;
        let next = match whence {
            libc::SEEK_SET => Some(requested_offset),
            libc::SEEK_CUR => open_state.offset()?.checked_add(requested_offset),
            libc::SEEK_END => {
                let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
                if unsafe { libc::fstat(descriptor, &mut status) } != 0 {
                    return Ok(-1);
                }
                status.st_size.checked_add(requested_offset)
            }
            libc::SEEK_DATA | libc::SEEK_HOLE => {
                self.materialize(state, runtime, None)?;
                let next = native();
                if next < 0 {
                    return Ok(-1);
                }
                Some(next)
            }
            _ => return Err(errno_error(libc::EINVAL)),
        };
        let next = next.ok_or_else(|| errno_error(libc::EOVERFLOW))?;
        if next < 0 {
            return Err(errno_error(libc::EINVAL));
        }
        open_state.set_offset(next)?;
        Ok(next)
    }

    fn truncate(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        operation: &mut TruncateOperation<'_>,
    ) -> Result<libc::c_int> {
        let local = runtime
            .local
            .as_ref()
            .context("local filesystem runtime is unavailable")?;
        let active = operation
            .reservation
            .map(|range| local.begin_write(&self.handle, range))
            .transpose()?;
        let result = (operation.native)();
        if result != 0 {
            let errno = unsafe { *libc::__error() };
            if let Some(active) = &active {
                let _ = local.cancel_write(&self.handle, active);
            }
            unsafe { set_errno(errno) };
            return Ok(result);
        }
        runtime.refresh_attributes(
            operation.descriptor,
            operation.open.logical().to_string_lossy().as_ref(),
        )?;
        if let Some(range) = operation.reservation
            && active.as_ref().is_none_or(|active| {
                if local.finish_write(&self.handle, active, range).is_ok() {
                    false
                } else {
                    let _ = local.cancel_write(&self.handle, active);
                    true
                }
            })
        {
            state.record_write(range);
        }
        Ok(result)
    }

    fn prepare_mapping(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        _descriptor: libc::c_int,
        range: LocalByteRange,
        protection: libc::c_int,
        flags: libc::c_int,
    ) -> Result<()> {
        let open_state = self.state.lock()?;
        let access = open_state.flags()? & libc::O_ACCMODE;
        if access == libc::O_WRONLY
            || (flags & libc::MAP_SHARED != 0
                && protection & libc::PROT_WRITE != 0
                && access == libc::O_RDONLY)
        {
            return Err(errno_error(libc::EACCES));
        }
        drop(open_state);
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
                .local
                .as_ref()
                .context("local filesystem runtime is unavailable")?
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
        if !self.lazy {
            return Ok(());
        }
        let requested = range.unwrap_or(LocalByteRange {
            start: 0,
            end: u64::MAX,
        });
        let mut materialized = lock(&state.materialized);
        if materialized.covers(requested) {
            return Ok(());
        }
        runtime
            .local
            .as_ref()
            .context("local filesystem runtime is unavailable")?
            .materialize(&self.handle, range)?;
        materialized.insert(requested);
        Ok(())
    }

    fn sync(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        open: &OpenFile,
        durable: bool,
    ) -> Result<()> {
        let local = runtime
            .local
            .as_ref()
            .context("local filesystem runtime is unavailable")?;
        let mut dirty = lock(&state.dirty);
        local.sync(&self.handle, dirty.to_vec(), durable)?;
        dirty.clear();
        if descriptor >= 0 {
            runtime.refresh_open_attributes(descriptor, open)
        } else {
            Ok(())
        }
    }

    fn finish(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        open: &OpenFile,
    ) -> Result<()> {
        if descriptor >= 0 {
            runtime.refresh_open_attributes(descriptor, open)?;
        }
        let mut dirty = lock(&state.dirty);
        runtime
            .local
            .as_ref()
            .context("local filesystem runtime is unavailable")?
            .close(&self.handle, dirty.to_vec())?;
        dirty.clear();
        Ok(())
    }

    fn is_broker_managed(&self) -> bool {
        true
    }

    fn publishes_writes(&self) -> bool {
        true
    }

    fn accepts_opaque_copy(&self) -> bool {
        false
    }

    fn local_inheritance(&self) -> Option<LocalContentInheritance<'_>> {
        Some(LocalContentInheritance {
            handle: &self.handle,
            lazy: self.lazy,
            state: &self.state,
            lock: &self.lock,
            identity: &self.identity,
        })
    }

    fn merge_status_flags(
        &self,
        _state: &ContentState,
        native: libc::c_int,
    ) -> Result<libc::c_int> {
        let logical = self.state.lock()?.flags()?;
        Ok((native & !LOCAL_STATUS_FLAGS_MASK) | (logical & LOCAL_STATUS_FLAGS_MASK))
    }

    fn native_status_flags(&self, requested: libc::c_int) -> libc::c_int {
        requested & !(libc::O_APPEND | libc::O_NONBLOCK | libc::O_SYNC | libc::O_DSYNC)
    }

    fn commit_status_flags(&self, _state: &ContentState, requested: libc::c_int) -> Result<()> {
        let open_state = self.state.lock()?;
        let current = open_state.flags()?;
        open_state
            .set_flags(
                (current & !(libc::O_APPEND | libc::O_NONBLOCK))
                    | (requested & (libc::O_APPEND | libc::O_NONBLOCK)),
            )
            .map_err(Into::into)
    }

    fn lock_descriptor(&self, _descriptor: libc::c_int) -> libc::c_int {
        self.lock.as_raw_fd()
    }

    fn release_close_locks(&self, last_alias: bool) {
        let errno = unsafe { *libc::__error() };
        let mut unlock = libc::flock {
            l_start: 0,
            l_len: 0,
            l_pid: 0,
            l_type: libc::F_UNLCK,
            l_whence: libc::SEEK_SET as libc::c_short,
        };
        unsafe {
            libc::fcntl(self.lock.as_raw_fd(), libc::F_SETLK, &raw mut unlock);
            if last_alias {
                libc::fcntl(self.lock.as_raw_fd(), libc::F_OFD_SETLK, &raw mut unlock);
                libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN);
            }
            set_errno(errno);
        }
    }
}

impl ContentBackend for EagerEncryptedContent {
    fn publishes_writes(&self) -> bool {
        true
    }

    fn truncate(
        &self,
        _state: &ContentState,
        runtime: &FilesystemHookRuntime,
        operation: &mut TruncateOperation<'_>,
    ) -> Result<libc::c_int> {
        let result = (operation.native)();
        if result != 0 {
            return Ok(result);
        }
        runtime.refresh_attributes(
            operation.descriptor,
            operation.open.logical().to_string_lossy().as_ref(),
        )?;
        Ok(result)
    }

    fn sync(
        &self,
        _state: &ContentState,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        _open: &OpenFile,
        _durable: bool,
    ) -> Result<()> {
        self.publish(runtime, descriptor)
    }

    fn finish(
        &self,
        _state: &ContentState,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        _open: &OpenFile,
    ) -> Result<()> {
        self.publish(runtime, descriptor)
    }
}
