mod encrypted;
mod nfs;
mod plain;

pub(super) use encrypted::EagerEncryptedContent;
pub(crate) use encrypted::EncryptedContent;
pub(crate) use nfs::NfsContent;
pub(super) use plain::PlainContent;

use super::super::{FilesystemHookRuntime, LocalByteRange, OpenFile};
use super::state::ContentState;
use crate::filesystem::FileAttributes;
use crate::filesystem::broker::{LocalFileIdentity, LocalOpenState};
use anyhow::Result;
use std::fs::File;
use std::sync::Mutex;

#[derive(Clone, Copy)]
pub(crate) enum ContentIoOffset {
    Sequential,
    Positioned(libc::off_t),
}

pub(super) struct ReadOperations<'a> {
    pub(super) requested_length: &'a mut dyn FnMut() -> std::result::Result<usize, libc::c_int>,
    pub(super) copy_from_payload: &'a mut dyn FnMut(libc::c_int, usize) -> libc::ssize_t,
    pub(super) positioned: &'a mut dyn FnMut(libc::off_t) -> libc::ssize_t,
    pub(super) native: &'a mut dyn FnMut() -> libc::ssize_t,
}

pub(super) struct WriteOperations<'a> {
    pub(super) requested_length: &'a mut dyn FnMut() -> std::result::Result<usize, libc::c_int>,
    pub(super) copy_to_payload: &'a mut dyn FnMut(libc::c_int, usize) -> libc::ssize_t,
    pub(super) positioned: &'a mut dyn FnMut(libc::off_t) -> libc::ssize_t,
    pub(super) native: &'a mut dyn FnMut() -> libc::ssize_t,
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
    fn read(
        &self,
        _state: &ContentState,
        _runtime: &FilesystemHookRuntime,
        _descriptor: libc::c_int,
        _offset: ContentIoOffset,
        operations: &mut ReadOperations<'_>,
    ) -> Result<libc::ssize_t> {
        Ok((operations.native)())
    }

    fn write(
        &self,
        _state: &ContentState,
        _runtime: &FilesystemHookRuntime,
        _descriptor: libc::c_int,
        _offset: ContentIoOffset,
        _reservation_length: Option<usize>,
        operations: &mut WriteOperations<'_>,
    ) -> Result<libc::ssize_t> {
        Ok((operations.native)())
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

    fn prepare_mapping(
        &self,
        _state: &ContentState,
        _runtime: &FilesystemHookRuntime,
        _descriptor: libc::c_int,
        _range: LocalByteRange,
        _protection: libc::c_int,
        _flags: libc::c_int,
    ) -> Result<()> {
        Ok(())
    }

    fn prepare_writable_mapping(
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

    fn records_native_snapshot_writes(&self) -> bool {
        false
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
