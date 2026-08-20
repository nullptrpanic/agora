use super::super::super::{FilesystemHookRuntime, LocalByteRange, OpenFile, lock};
use super::super::policy::{ReadOperations, WriteOperations, errno_error};
use super::super::state::ContentState;
use super::{
    ContentBackend, ContentReadMode, ContentWriteMode, ContentWritePosition, ContentWriteResult,
    TruncateOperation,
};
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
    fn read_mode(&self) -> ContentReadMode {
        if self.snapshot.load(Ordering::Acquire) {
            ContentReadMode::Positioned { materialize: true }
        } else {
            ContentReadMode::Direct
        }
    }

    fn write_mode(&self) -> ContentWriteMode {
        if self.snapshot.load(Ordering::Acquire) {
            ContentWriteMode::Native {
                track_snapshot: true,
            }
        } else {
            ContentWriteMode::Explicit
        }
    }

    fn direct_read(
        &self,
        _state: &ContentState,
        runtime: &FilesystemHookRuntime,
        offset: u64,
        operations: &mut ReadOperations<'_>,
    ) -> Result<libc::ssize_t> {
        let length = (operations.requested_length)().map_err(errno_error)?;
        if length == 0 {
            return Ok(0);
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
        Ok((operations.copy_from_payload)(
            payload.as_raw_fd(),
            available as usize,
        ))
    }

    fn write_explicit(
        &self,
        _state: &ContentState,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        position: ContentWritePosition,
        _reservation_length: Option<usize>,
        operations: &mut WriteOperations<'_>,
    ) -> Result<ContentWriteResult> {
        let length = (operations.requested_length)()
            .map_err(errno_error)?
            .min(MAX_REMOTE_IO_BYTES as usize);
        if length == 0 {
            return Ok(ContentWriteResult {
                result: 0,
                start: match position {
                    ContentWritePosition::At(start) => Some(start),
                    ContentWritePosition::Append => None,
                },
                published: true,
                recoverable: false,
            });
        }
        let payload = tempfile::tempfile()?;
        let copied = (operations.copy_to_payload)(payload.as_raw_fd(), length);
        if copied <= 0 {
            return Ok(ContentWriteResult {
                result: copied,
                start: None,
                published: true,
                recoverable: false,
            });
        }
        let copied = u32::try_from(copied).expect("remote write is protocol bounded");
        payload.set_len(u64::from(copied))?;
        let offset = match position {
            ContentWritePosition::At(start) => Some(start),
            ContentWritePosition::Append => None,
        };
        let remote = runtime
            .remote
            .as_ref()
            .context("remote filesystem runtime is unavailable")?;
        let (actual_offset, written, size) =
            remote.write(&self.handle, offset, &payload, copied)?;
        if written > copied {
            return Err(errno_error(libc::EIO));
        }
        let local_size = libc::off_t::try_from(size).map_err(|_| errno_error(libc::EFBIG))?;
        if unsafe { libc::ftruncate(descriptor, local_size) } != 0 {
            return Ok(ContentWriteResult {
                result: -1,
                start: Some(actual_offset),
                published: true,
                recoverable: false,
            });
        }
        lock(&self.metadata).size = size;
        Ok(ContentWriteResult {
            result: written as libc::ssize_t,
            start: Some(actual_offset),
            published: true,
            recoverable: false,
        })
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

    fn potentially_dirty(
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
        descriptor: libc::c_int,
        _open: &OpenFile,
        _durable: bool,
    ) -> Result<()> {
        self.sync_dirty(state, runtime, descriptor)
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
    fn sync_dirty(
        &self,
        state: &ContentState,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
    ) -> Result<()> {
        let remote = runtime
            .remote
            .as_ref()
            .context("remote filesystem runtime is unavailable")?;
        let mut dirty = lock(&state.dirty);
        if let Some(metadata) = remote.sync(&self.handle, dirty.to_vec())? {
            if descriptor >= 0 {
                let size =
                    libc::off_t::try_from(metadata.size).map_err(|_| errno_error(libc::EFBIG))?;
                if unsafe { libc::ftruncate(descriptor, size) } != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            *lock(&self.metadata) = metadata;
        }
        dirty.clear();
        Ok(())
    }
}
