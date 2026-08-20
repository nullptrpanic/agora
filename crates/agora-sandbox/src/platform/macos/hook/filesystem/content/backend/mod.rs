mod encrypted;
mod nfs;
mod plain;

pub(super) use encrypted::EagerEncryptedContent;
pub(crate) use encrypted::EncryptedContent;
pub(crate) use nfs::NfsContent;
pub(super) use plain::PlainContent;

use super::super::{FilesystemHookRuntime, LocalByteRange, OpenFile};
use super::policy::{ReadOperations, WriteOperations};
use super::state::ContentState;
use crate::filesystem::FileAttributes;
use crate::filesystem::broker::{LocalFileIdentity, LocalOpenState};
use anyhow::Result;
use std::fs::File;
use std::sync::Mutex;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ContentReadMode {
    Native,
    Positioned { materialize: bool },
    Direct,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ContentWriteMode {
    Native { track_snapshot: bool },
    Explicit,
}

#[derive(Clone, Copy)]
pub(super) enum ContentWritePosition {
    At(u64),
    Append,
}

pub(super) struct ContentWriteResult {
    pub(super) result: libc::ssize_t,
    pub(super) start: Option<u64>,
    pub(super) published: bool,
    pub(super) recoverable: bool,
}

pub(super) struct TruncateOperation<'a> {
    pub(super) descriptor: libc::c_int,
    pub(super) open: &'a OpenFile,
    pub(super) requested_length: u64,
    pub(super) reservation: Option<LocalByteRange>,
    pub(super) native: &'a mut dyn FnMut() -> libc::c_int,
}

pub(crate) struct LocalContentInheritance<'a> {
    pub(crate) handle: &'a str,
    pub(crate) lazy: bool,
    pub(crate) state: &'a LocalOpenState,
    pub(crate) lock: &'a File,
    pub(crate) identity: &'a LocalFileIdentity,
}

pub(super) trait ContentBackend: Send + Sync {
    fn read_mode(&self) -> ContentReadMode {
        ContentReadMode::Native
    }

    fn write_mode(&self) -> ContentWriteMode {
        ContentWriteMode::Native {
            track_snapshot: false,
        }
    }

    fn logical_open_state(&self) -> Option<&LocalOpenState> {
        None
    }

    fn direct_read(
        &self,
        _state: &ContentState,
        _runtime: &FilesystemHookRuntime,
        _offset: u64,
        _operations: &mut ReadOperations<'_>,
    ) -> Result<libc::ssize_t> {
        Err(std::io::Error::from_raw_os_error(libc::ENOTSUP).into())
    }

    fn write_explicit(
        &self,
        _state: &ContentState,
        _runtime: &FilesystemHookRuntime,
        _descriptor: libc::c_int,
        _position: ContentWritePosition,
        _reservation_length: Option<usize>,
        _operations: &mut WriteOperations<'_>,
    ) -> Result<ContentWriteResult> {
        Err(std::io::Error::from_raw_os_error(libc::ENOTSUP).into())
    }

    fn seek(
        &self,
        _state: &ContentState,
        _runtime: &FilesystemHookRuntime,
        _descriptor: libc::c_int,
        _requested_offset: libc::off_t,
        _whence: libc::c_int,
        native: &mut dyn FnMut() -> libc::off_t,
    ) -> Result<libc::off_t> {
        Ok(native())
    }

    fn truncate(
        &self,
        _state: &ContentState,
        _runtime: &FilesystemHookRuntime,
        operation: &mut TruncateOperation<'_>,
    ) -> Result<libc::c_int> {
        Ok((operation.native)())
    }

    fn potentially_dirty(
        &self,
        _state: &ContentState,
        _runtime: &FilesystemHookRuntime,
        _range: LocalByteRange,
    ) -> Result<()> {
        Ok(())
    }

    fn materialize(
        &self,
        _state: &ContentState,
        _runtime: &FilesystemHookRuntime,
        _range: Option<LocalByteRange>,
    ) -> Result<()> {
        Ok(())
    }

    fn sync(
        &self,
        _state: &ContentState,
        _runtime: &FilesystemHookRuntime,
        _descriptor: libc::c_int,
        _open: &OpenFile,
        _durable: bool,
    ) -> Result<()> {
        Ok(())
    }

    fn finish(
        &self,
        _state: &ContentState,
        _runtime: &FilesystemHookRuntime,
        _descriptor: libc::c_int,
        _open: &OpenFile,
    ) -> Result<()> {
        Ok(())
    }

    fn prepare_native_snapshot(
        &self,
        _state: &ContentState,
        _runtime: &FilesystemHookRuntime,
    ) -> Result<()> {
        Ok(())
    }

    fn supports_async_write(&self) -> bool {
        true
    }

    fn operation_lock(&self) -> Option<&Mutex<()>> {
        None
    }

    fn publishes_writes(&self) -> bool {
        false
    }

    fn manages_metadata(&self) -> bool {
        false
    }

    fn file_attributes(&self, _runtime: &FilesystemHookRuntime) -> Result<Option<FileAttributes>> {
        Ok(None)
    }

    fn is_directory(&self) -> bool {
        false
    }

    #[cfg(test)]
    fn handle(&self) -> Option<&str> {
        None
    }

    fn is_broker_managed(&self) -> bool {
        false
    }

    fn accepts_opaque_copy(&self) -> bool {
        true
    }

    fn local_inheritance(&self) -> Option<LocalContentInheritance<'_>> {
        None
    }

    fn merge_status_flags(
        &self,
        _state: &ContentState,
        native: libc::c_int,
    ) -> Result<libc::c_int> {
        Ok(native)
    }

    fn native_status_flags(&self, requested: libc::c_int) -> libc::c_int {
        requested
    }

    fn commit_status_flags(&self, _state: &ContentState, _requested: libc::c_int) -> Result<()> {
        Ok(())
    }

    fn lock_descriptor(&self, descriptor: libc::c_int) -> libc::c_int {
        descriptor
    }

    fn release_close_locks(&self, _last_alias: bool) {}
}
