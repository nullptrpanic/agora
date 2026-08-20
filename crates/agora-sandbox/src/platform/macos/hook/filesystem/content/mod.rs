mod backend;
mod policy;
mod state;

pub(super) use backend::{EncryptedContent, LocalContentInheritance, NfsContent};
pub(super) use policy::{ContentIoOffset, managed_read_io, managed_seek_io, managed_write_io};
#[cfg(test)]
pub(super) use policy::{READ_AHEAD_MAX_BYTES, read_materialization_length};
pub(super) use state::ManagedContent;

use super::{FilesystemHookRuntime, LocalByteRange, OpenFile, lock};
use anyhow::Result;
use backend::{ContentWriteMode, EagerEncryptedContent, PlainContent, TruncateOperation};
use std::sync::MutexGuard;

impl ManagedContent {
    fn mutation_guard(&self) -> Option<MutexGuard<'_, ()>> {
        self.backend.operation_lock().map(lock)
    }

    pub(crate) fn plain(writable: bool) -> Self {
        Self::new(PlainContent, writable)
    }

    pub(crate) fn encrypted(content: EncryptedContent, writable: bool) -> Self {
        Self::new(content, writable)
    }

    pub(crate) fn nfs(content: NfsContent, writable: bool) -> Self {
        Self::new(content, writable)
    }

    pub(crate) fn eager_encrypted(writeback: crate::filesystem::Writeback) -> Self {
        Self::new(EagerEncryptedContent::new(writeback), true)
    }

    pub(super) fn supports_exec_inheritance(&self) -> bool {
        self.backend.local_inheritance().is_some()
    }

    pub(super) fn is_broker_managed(&self) -> bool {
        self.backend.is_broker_managed()
    }

    pub(super) fn publishes_writes(&self) -> bool {
        self.state.writable && self.backend.publishes_writes()
    }

    pub(super) fn accepts_opaque_copy(&self) -> bool {
        self.backend.accepts_opaque_copy()
    }

    pub(super) fn local_inheritance(&self) -> Option<LocalContentInheritance<'_>> {
        self.backend.local_inheritance()
    }

    pub(super) fn manages_metadata(&self) -> bool {
        self.backend.manages_metadata()
    }

    pub(super) fn file_attributes(
        &self,
        runtime: &FilesystemHookRuntime,
    ) -> Result<Option<crate::filesystem::FileAttributes>> {
        self.backend.file_attributes(runtime)
    }

    pub(super) fn is_directory(&self) -> bool {
        self.backend.is_directory()
    }

    #[cfg(test)]
    pub(super) fn handle(&self) -> Option<&str> {
        self.backend.handle()
    }

    pub(super) fn prepare_native_snapshot_if_needed(
        &self,
        runtime: &FilesystemHookRuntime,
    ) -> Result<()> {
        let _mutation = self.mutation_guard();
        self.backend.prepare_native_snapshot(&self.state, runtime)
    }

    pub(super) fn supports_async_write(&self) -> bool {
        self.backend.supports_async_write()
    }

    pub(super) fn record_snapshot_write(&self, range: Option<LocalByteRange>) {
        if matches!(
            self.backend.write_mode(),
            ContentWriteMode::Native {
                track_snapshot: true
            }
        ) && let Some(range) = range
        {
            self.state.record_write(range);
        }
    }

    pub(super) fn merge_status_flags(&self, native: libc::c_int) -> Result<libc::c_int> {
        let _mutation = self.mutation_guard();
        self.backend.merge_status_flags(&self.state, native)
    }

    pub(super) fn native_status_flags(&self, requested: libc::c_int) -> libc::c_int {
        self.backend.native_status_flags(requested)
    }

    pub(super) fn commit_status_flags(&self, requested: libc::c_int) -> Result<()> {
        let _mutation = self.mutation_guard();
        self.backend.commit_status_flags(&self.state, requested)
    }

    pub(super) fn lock_descriptor(&self, descriptor: libc::c_int) -> libc::c_int {
        self.backend.lock_descriptor(descriptor)
    }

    pub(super) fn release_close_locks(&self, last_alias: bool) {
        self.backend.release_close_locks(last_alias);
    }

    pub(super) unsafe fn truncate(
        &self,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        open: &OpenFile,
        requested_length: u64,
        reservation: Option<LocalByteRange>,
        mut native: impl FnMut() -> libc::c_int,
    ) -> Result<libc::c_int> {
        if !self.state.writable {
            return Err(std::io::Error::from_raw_os_error(libc::EBADF).into());
        }
        let _mutation = self.mutation_guard();
        let mut operation = TruncateOperation {
            descriptor,
            open,
            requested_length,
            reservation,
            native: &mut native,
        };
        self.backend.truncate(&self.state, runtime, &mut operation)
    }

    pub(super) fn prepare_mapping(
        &self,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        range: LocalByteRange,
        protection: libc::c_int,
        flags: libc::c_int,
    ) -> Result<bool> {
        let _mutation = self.mutation_guard();
        policy::prepare_mapping(self, runtime, descriptor, range, protection, flags)
    }

    pub(super) fn materialize(
        &self,
        runtime: &FilesystemHookRuntime,
        range: Option<LocalByteRange>,
    ) -> Result<()> {
        let _mutation = self.mutation_guard();
        self.backend.materialize(&self.state, runtime, range)
    }

    pub(super) fn sync(
        &self,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        open: &OpenFile,
        durable: bool,
    ) -> Result<()> {
        let _mutation = self.mutation_guard();
        self.backend
            .sync(&self.state, runtime, descriptor, open, durable)
    }

    pub(super) fn finish(
        &self,
        runtime: &FilesystemHookRuntime,
        descriptor: libc::c_int,
        open: &OpenFile,
    ) -> Result<()> {
        let _mutation = self.mutation_guard();
        self.backend.finish(&self.state, runtime, descriptor, open)
    }
}
