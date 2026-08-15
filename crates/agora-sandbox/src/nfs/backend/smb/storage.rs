use super::SmbRemoteConfig;
use crate::nfs::backend::{RemoteStorage, StorageError, StorageResult};
use crate::nfs::protocol::{RemoteEntry, RemoteFileType, RemoteMetadata, RemotePath};
use md5::{Digest, Md5};
use smb2::msg::close::{CloseRequest, CloseResponse, SMB2_CLOSE_FLAG_POSTQUERY_ATTRIB};
use smb2::msg::create::{
    CreateAction, CreateDisposition, CreateRequest, CreateResponse, ImpersonationLevel, ShareAccess,
};
use smb2::msg::flush::FlushRequest;
use smb2::msg::query_directory::{
    FileInformationClass, QueryDirectoryFlags, QueryDirectoryRequest, QueryDirectoryResponse,
};
use smb2::msg::query_info::{InfoType as QueryInfoType, QueryInfoRequest, QueryInfoResponse};
use smb2::msg::read::{ReadRequest, ReadResponse, SMB2_CHANNEL_NONE};
use smb2::msg::set_info::{InfoType, SetInfoRequest};
use smb2::msg::write::{WriteRequest, WriteResponse};
use smb2::pack::{ReadCursor, Unpack};
use smb2::types::flags::FileAccessMask;
use smb2::types::status::NtStatus;
use smb2::types::{Command, CreditCharge, FileId, OplockLevel};
use smb2::{ClientConfig, CompoundOp, ErrorKind, SmbClient, Tree, client::Connection};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::Mutex;
use uuid::Uuid;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_INTERNAL_INFORMATION: u8 = 6;
const FILE_BASIC_INFORMATION: u8 = 4;
const FILE_STANDARD_INFORMATION: u8 = 5;
const FILE_END_OF_FILE_INFORMATION: u8 = 20;
const FILE_RENAME_INFORMATION: u8 = 10;
const FILE_DISPOSITION_INFORMATION: u8 = 13;
const TRANSFER_CHUNK_SIZE: usize = 64 * 1024;
const WRITE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const SMB_SESSION_POOL_SIZE: usize = 4;

struct SmbStorage {
    roots: Vec<SmbRoot>,
}

pub(super) struct SmbFileHandle {
    root: u32,
    slot: usize,
    generation: u64,
    file_id: FileId,
    size: u64,
}

pub(in crate::nfs) fn configured_storage(
    remotes: &[SmbRemoteConfig],
) -> Arc<impl RemoteStorage + use<>> {
    Arc::new(SmbStorage::new(remotes))
}

impl SmbStorage {
    fn new(remotes: &[SmbRemoteConfig]) -> Self {
        Self {
            roots: remotes.iter().cloned().map(SmbRoot::new).collect(),
        }
    }

    fn root_by_index(&self, root: u32) -> StorageResult<&SmbRoot> {
        self.roots
            .get(root as usize)
            .ok_or_else(|| StorageError::new(libc::EINVAL, "unknown SMB root"))
    }

    fn root(&self, path: &RemotePath) -> StorageResult<&SmbRoot> {
        self.root_by_index(path.root())
    }

    async fn file_slot(
        &self,
        handle: &SmbFileHandle,
    ) -> StorageResult<tokio::sync::OwnedMutexGuard<SmbSlot>> {
        let root = self.root_by_index(handle.root)?;
        let slot = root.slot(handle.slot)?;
        let slot = slot.lock_owned().await;
        if slot.generation != handle.generation || slot.session.is_none() {
            return Err(StorageError::new(
                libc::ESTALE,
                "SMB file handle belongs to an expired session",
            ));
        }
        Ok(slot)
    }
}

impl RemoteStorage for SmbStorage {
    type FileHandle = SmbFileHandle;

    async fn reset(&self, root: u32) {
        if let Some(root) = self.roots.get(root as usize) {
            for slot in &root.slots {
                slot.lock().await.invalidate_session();
            }
        }
    }

    async fn connect(&self, root: u32) -> StorageResult<()> {
        let root = self.root_by_index(root)?;
        let remote_path = root.config.remote_path().to_string();
        let mut slot = root.slot(0)?.lock_owned().await;
        let session = slot.session(&root.config).await?;
        if remote_path.is_empty() {
            return Ok(());
        }
        let result = session.stat(&remote_path).await;
        validate_remote_root(result)
    }

    async fn stat(&self, path: &RemotePath) -> StorageResult<RemoteMetadata> {
        ensure_public_path(path)?;
        let root = self.root(path)?;
        let remote = root.path(path);
        let (_, slot) = root.next_slot();
        let mut slot = slot.lock_owned().await;
        for attempt in 0..2 {
            let result = {
                let session = slot.session(&root.config).await?;
                stat_locked_path(session, &remote)
                    .await
                    .map_err(storage_error)
            };
            if attempt == 0 && result.as_ref().is_err_and(is_connection_failure) {
                slot.invalidate_session();
                continue;
            }
            return result;
        }
        unreachable!()
    }

    async fn open_file(
        &self,
        path: &RemotePath,
        flags: libc::c_int,
        _mode: u32,
    ) -> StorageResult<(Self::FileHandle, RemoteMetadata, bool)> {
        ensure_public_path(path)?;
        let access = flags & libc::O_ACCMODE;
        let (readable, writable) = match access {
            libc::O_RDONLY => (true, false),
            libc::O_WRONLY => (false, true),
            libc::O_RDWR => (true, true),
            _ => return Err(StorageError::new(libc::EINVAL, "invalid open access mode")),
        };
        if flags & libc::O_TRUNC != 0 && !writable {
            return Err(StorageError::new(
                libc::EINVAL,
                "O_TRUNC requires write access",
            ));
        }
        let disposition = match (
            flags & libc::O_CREAT != 0,
            flags & libc::O_EXCL != 0,
            flags & libc::O_TRUNC != 0,
        ) {
            (true, true, _) => CreateDisposition::FileCreate,
            (true, false, true) => CreateDisposition::FileOverwriteIf,
            (true, false, false) => CreateDisposition::FileOpenIf,
            (false, _, true) => CreateDisposition::FileOverwrite,
            (false, _, false) => CreateDisposition::FileOpen,
        };
        let mut desired = FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE;
        if readable {
            desired |= FileAccessMask::FILE_READ_DATA;
        }
        if writable {
            desired |= FileAccessMask::FILE_WRITE_ATTRIBUTES;
            desired |= FileAccessMask::FILE_WRITE_DATA;
        }
        let root = self.root(path)?;
        let remote = root.path(path);
        let (slot_index, slot) = root.next_slot();
        let mut slot = slot.lock_owned().await;
        for attempt in 0..2 {
            let result = {
                let session = slot.session(&root.config).await?;
                open_direct_handle(session, &remote, disposition, FileAccessMask::new(desired))
                    .await
                    .map_err(storage_error)
            };
            match result {
                Ok((opened, file_index)) => {
                    let metadata = metadata_from_create(&opened, file_index);
                    let created = opened.create_action == CreateAction::FileCreated;
                    return Ok((
                        SmbFileHandle {
                            root: path.root(),
                            slot: slot_index,
                            generation: slot.generation,
                            file_id: opened.file_id,
                            size: metadata.size,
                        },
                        metadata,
                        created,
                    ));
                }
                Err(error) if attempt == 0 && is_connection_failure(&error) => {
                    slot.invalidate_session();
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!()
    }

    async fn read_at(
        &self,
        handle: &mut Self::FileHandle,
        offset: u64,
        length: u32,
        destination: &mut File,
    ) -> StorageResult<u32> {
        let mut slot = self.file_slot(handle).await?;
        let result = read_handle_at(
            slot.session
                .as_mut()
                .expect("validated SMB handle has an active session"),
            handle.file_id,
            offset,
            length,
            destination,
        )
        .await;
        slot.finish_file_operation(result.map_err(storage_error))
    }

    async fn write_at(
        &self,
        handle: &mut Self::FileHandle,
        offset: u64,
        source: &mut File,
        length: u32,
    ) -> StorageResult<(u32, u64)> {
        source
            .seek(SeekFrom::Start(0))
            .map_err(|error| storage_error(error.into()))?;
        let mut slot = self.file_slot(handle).await?;
        let result = write_handle_at(
            slot.session
                .as_mut()
                .expect("validated SMB handle has an active session"),
            handle.file_id,
            offset,
            source,
            length,
        )
        .await
        .map_err(storage_error);
        let written = slot.finish_file_operation(result)?;
        handle.size = handle.size.max(offset.saturating_add(u64::from(written)));
        Ok((written, handle.size))
    }

    async fn set_length(&self, handle: &mut Self::FileHandle, length: u64) -> StorageResult<u64> {
        let mut slot = self.file_slot(handle).await?;
        let result = set_handle_length(
            slot.session
                .as_mut()
                .expect("validated SMB handle has an active session"),
            handle.file_id,
            length,
        )
        .await
        .map_err(storage_error);
        slot.finish_file_operation(result)?;
        handle.size = length;
        Ok(length)
    }

    async fn flush_file(&self, handle: &mut Self::FileHandle) -> StorageResult<RemoteMetadata> {
        let mut slot = self.file_slot(handle).await?;
        let session = slot
            .session
            .as_mut()
            .expect("validated SMB handle has an active session");
        let result = async {
            flush_handle(session, handle.file_id).await?;
            query_handle_metadata(session, handle.file_id).await
        }
        .await
        .map_err(storage_error);
        let metadata = slot.finish_file_operation(result)?;
        handle.size = metadata.size;
        Ok(metadata)
    }

    async fn file_metadata(&self, handle: &mut Self::FileHandle) -> StorageResult<RemoteMetadata> {
        let mut slot = self.file_slot(handle).await?;
        let result = query_handle_metadata(
            slot.session
                .as_mut()
                .expect("validated SMB handle has an active session"),
            handle.file_id,
        )
        .await
        .map_err(storage_error);
        let metadata = slot.finish_file_operation(result)?;
        handle.size = metadata.size;
        Ok(metadata)
    }

    async fn close_file(&self, handle: &mut Self::FileHandle) -> StorageResult<()> {
        let mut slot = self.file_slot(handle).await?;
        let result = close_handle(
            slot.session
                .as_mut()
                .expect("validated SMB handle has an active session"),
            handle.file_id,
            false,
        )
        .await
        .map(|_| ())
        .map_err(storage_error);
        match slot.finish_file_operation(result) {
            Err(error) if is_connection_failure(&error) => {
                // Dropping the failed session releases all of its server-side
                // handles. A lost CLOSE response is therefore locally complete
                // and must not strand an otherwise synchronized descriptor.
                Ok(())
            }
            result => result,
        }
    }

    async fn read_into(
        &self,
        handle: &mut Self::FileHandle,
        destination: &mut File,
        max_length: u64,
    ) -> StorageResult<RemoteMetadata> {
        let mut slot = self.file_slot(handle).await?;
        let session = slot
            .session
            .as_mut()
            .expect("validated SMB handle has an active session");
        let result = async {
            let before = query_handle_metadata(session, handle.file_id).await?;
            validate_transfer_size(before.size, max_length)?;
            destination.set_len(0)?;
            destination.seek(SeekFrom::Start(0))?;
            read_handle(session, handle.file_id, before.size, destination).await?;
            let after = query_handle_metadata(session, handle.file_id).await?;
            if before.identity != after.identity || before.size != after.size {
                return Err(std::io::Error::from_raw_os_error(libc::ESTALE).into());
            }
            Ok(after)
        }
        .await
        .map_err(storage_error);
        let metadata = slot.finish_file_operation(result)?;
        handle.size = metadata.size;
        Ok(metadata)
    }

    async fn write_from_if_unchanged(
        &self,
        path: &RemotePath,
        expected: Option<&RemoteMetadata>,
        source: &mut File,
        length: u64,
    ) -> StorageResult<RemoteMetadata> {
        ensure_public_path(path)?;
        let root = self.root(path)?;
        let remote = root.path(path);
        let (_, slot) = root.next_slot();
        let mut slot = slot.lock_owned().await;
        let mut operation = WriteOperation::new(&remote);
        for attempt in 0..2 {
            let result = {
                let session = slot.session(&root.config).await?;
                write_locked_file(session, &remote, expected, source, length, &mut operation).await
            };
            if attempt == 0 && result.as_ref().is_err_and(is_connection_failure) {
                slot.invalidate_session();
                continue;
            }
            return result;
        }
        unreachable!()
    }

    async fn list(
        &self,
        path: &RemotePath,
        emit: &mut (impl FnMut(RemoteEntry) -> StorageResult<()> + Send),
    ) -> StorageResult<()> {
        ensure_public_path(path)?;
        let root = self.root(path)?;
        let remote = root.path(path);
        let (_, slot) = root.next_slot();
        let mut slot = slot.lock_owned().await;
        for attempt in 0..2 {
            let mut emitted = 0_usize;
            let result = {
                let mut counting_emit = |entry| {
                    emit(entry)?;
                    emitted += 1;
                    Ok(())
                };
                let session = slot.session(&root.config).await?;
                list_directory_stream(session, &remote, &mut counting_emit).await
            };
            if attempt == 0 && emitted == 0 && result.as_ref().is_err_and(is_connection_failure) {
                slot.invalidate_session();
                continue;
            }
            if result.as_ref().is_err_and(is_connection_failure) {
                slot.invalidate_session();
            }
            return result;
        }
        unreachable!()
    }

    async fn create_directory(&self, path: &RemotePath) -> StorageResult<()> {
        ensure_public_path(path)?;
        let root = self.root(path)?;
        let remote = root.path(path);
        let (_, slot) = root.next_slot();
        let mut slot = slot.lock_owned().await;
        let result = slot
            .session(&root.config)
            .await?
            .create_directory(&remote)
            .await
            .map_err(storage_error);
        slot.finish_file_operation(result)
    }

    async fn remove(&self, path: &RemotePath, directory: bool) -> StorageResult<()> {
        ensure_public_path(path)?;
        let root = self.root(path)?;
        let remote = root.path(path);
        let (_, slot) = root.next_slot();
        let mut slot = slot.lock_owned().await;
        let session = slot.session(&root.config).await?;
        let result = if directory {
            session
                .delete_directory(&remote)
                .await
                .map_err(storage_error)
        } else {
            session.delete_file(&remote).await.map_err(storage_error)
        };
        slot.finish_file_operation(result)
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath) -> StorageResult<()> {
        if from.root() != to.root() {
            return Err(StorageError::new(libc::EXDEV, "cross-root SMB rename"));
        }
        ensure_public_path(from)?;
        ensure_public_path(to)?;
        let root = self.root(from)?;
        let from = root.path(from);
        let to = root.path(to);
        let (_, slot) = root.next_slot();
        let mut slot = slot.lock_owned().await;
        let mut source_attempt = 0;
        let source = loop {
            let result = {
                let session = slot.session(&root.config).await?;
                stat_locked_path(session, &from)
                    .await
                    .map_err(storage_error)
            };
            if source_attempt == 0 && result.as_ref().is_err_and(is_connection_failure) {
                slot.invalidate_session();
                source_attempt += 1;
                continue;
            }
            break result?;
        };
        for attempt in 0..2 {
            let result = {
                let session = slot.session(&root.config).await?;
                rename_replacing(session, &from, &to)
                    .await
                    .map_err(storage_error)
            };
            if attempt == 0 && result.as_ref().is_err_and(is_connection_failure) {
                slot.invalidate_session();
                let resolved = {
                    let session = slot.session(&root.config).await?;
                    resolve_ambiguous_rename(session, &from, &to, &source).await
                };
                match resolved {
                    Ok(true) => return Ok(()),
                    Ok(false) => continue,
                    Err(error) => return Err(error),
                }
            }
            return result;
        }
        unreachable!()
    }
}

fn is_connection_failure(error: &StorageError) -> bool {
    matches!(
        error.errno(),
        libc::ENETDOWN
            | libc::ETIMEDOUT
            | libc::ECONNRESET
            | libc::ECONNABORTED
            | libc::ENOTCONN
            | libc::EPIPE
            | libc::EIO
    )
}

async fn resolve_ambiguous_rename(
    session: &mut SmbSession,
    from: &str,
    to: &str,
    source: &RemoteMetadata,
) -> StorageResult<bool> {
    match stat_locked_path(session, from).await {
        Ok(current) if same_file_object(&current, source) => return Ok(false),
        Ok(_) => return Err(stale_file()),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(storage_error(error)),
    }
    match stat_locked_path(session, to).await {
        Ok(current) if same_file_object(&current, source) => Ok(true),
        Ok(_) => Err(stale_file()),
        Err(error) => Err(storage_error(error)),
    }
}

async fn list_directory_stream(
    session: &mut SmbSession,
    path: &str,
    emit: &mut (impl FnMut(RemoteEntry) -> StorageResult<()> + Send),
) -> StorageResult<()> {
    let opened = open_path(
        session,
        path,
        CreateDisposition::FileOpen,
        FileAccessMask::new(FileAccessMask::FILE_READ_DATA | FileAccessMask::SYNCHRONIZE),
        ShareAccess(
            ShareAccess::FILE_SHARE_READ
                | ShareAccess::FILE_SHARE_WRITE
                | ShareAccess::FILE_SHARE_DELETE,
        ),
        FILE_DIRECTORY_FILE,
    )
    .await
    .map_err(storage_error)?;
    let output_buffer_length = session
        .params()
        .map_or(64 * 1024, |params| params.max_transact_size.min(64 * 1024))
        .max(1024);
    let result = async {
        let mut restart = true;
        loop {
            let request = QueryDirectoryRequest {
                file_information_class: FileInformationClass::FileBothDirectoryInformation,
                flags: QueryDirectoryFlags(if restart {
                    QueryDirectoryFlags::RESTART_SCANS
                } else {
                    0
                }),
                file_index: 0,
                file_id: opened.file_id,
                output_buffer_length,
                file_name: "*".to_string(),
            };
            let tree_id = session.tree.tree_id;
            let frame = session
                .connection_mut()
                .execute_with_credits(
                    Command::QueryDirectory,
                    &request,
                    Some(tree_id),
                    CreditCharge((u64::from(output_buffer_length).div_ceil(65_536)) as u16),
                )
                .await
                .map_err(storage_error)?;
            if frame.header.status == NtStatus::NO_MORE_FILES {
                return Ok(());
            }
            expect_success(&frame, Command::QueryDirectory).map_err(storage_error)?;
            let response = QueryDirectoryResponse::unpack(&mut ReadCursor::new(&frame.body))
                .map_err(storage_error)?;
            emit_directory_page(&response.output_buffer, emit)?;
            restart = false;
        }
    }
    .await;
    let closed = close_handle(session, opened.file_id, false)
        .await
        .map(|_| ())
        .map_err(storage_error);
    match result {
        Ok(()) => closed,
        Err(error) => Err(error),
    }
}

fn emit_directory_page(
    data: &[u8],
    emit: &mut impl FnMut(RemoteEntry) -> StorageResult<()>,
) -> StorageResult<()> {
    if data.is_empty() {
        return Err(StorageError::new(
            libc::EPROTO,
            "SMB returned an empty directory page",
        ));
    }
    let mut offset = 0_usize;
    loop {
        let entry_data = data.get(offset..).ok_or_else(|| {
            StorageError::new(libc::EPROTO, "SMB directory offset exceeds its page")
        })?;
        if entry_data.len() < 94 {
            return Err(StorageError::new(
                libc::EPROTO,
                "SMB directory entry is shorter than its fixed header",
            ));
        }
        let mut cursor = ReadCursor::new(entry_data);
        let next = cursor.read_u32_le().map_err(storage_error)? as usize;
        let _file_index = cursor.read_u32_le().map_err(storage_error)?;
        let created = smb2::pack::FileTime::unpack(&mut cursor).map_err(storage_error)?;
        let _accessed = smb2::pack::FileTime::unpack(&mut cursor).map_err(storage_error)?;
        let modified = smb2::pack::FileTime::unpack(&mut cursor).map_err(storage_error)?;
        let _changed = smb2::pack::FileTime::unpack(&mut cursor).map_err(storage_error)?;
        let size = cursor.read_u64_le().map_err(storage_error)?;
        let _allocated = cursor.read_u64_le().map_err(storage_error)?;
        let attributes = cursor.read_u32_le().map_err(storage_error)?;
        let name_length = cursor.read_u32_le().map_err(storage_error)? as usize;
        let _ea_size = cursor.read_u32_le().map_err(storage_error)?;
        let _short_name_length = cursor.read_u8().map_err(storage_error)?;
        let _reserved = cursor.read_u8().map_err(storage_error)?;
        cursor.skip(24).map_err(storage_error)?;
        let name = smb2::decode_name(&cursor.read_utf16_le(name_length).map_err(storage_error)?)
            .into_owned();
        if name != "." && name != ".." && !is_smb_control_entry(&name) {
            let file_type = if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                RemoteFileType::Directory
            } else {
                RemoteFileType::File
            };
            let mut entry_metadata = metadata(file_type, size, modified);
            entry_metadata.identity = format!("{}:{}", entry_metadata.identity, created.0);
            emit(RemoteEntry {
                name,
                metadata: entry_metadata,
            })?;
        }
        if next == 0 {
            return Ok(());
        }
        if next < 94
            || offset
                .checked_add(next)
                .is_none_or(|next| next >= data.len())
        {
            return Err(StorageError::new(
                libc::EPROTO,
                "SMB directory entry has an invalid next offset",
            ));
        }
        offset += next;
    }
}

fn is_smb_control_entry(name: &str) -> bool {
    control_identifier(name, ".agora-write-", ".tmp").is_some()
        || control_identifier(name, ".agora-lock-", ".lck").is_some()
}

fn control_identifier<'a>(name: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    let identifier = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    (identifier.len() == 32
        && identifier
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then_some(identifier)
}

fn ensure_public_path(path: &RemotePath) -> StorageResult<()> {
    if path
        .path()
        .rsplit('/')
        .next()
        .is_some_and(is_smb_control_entry)
    {
        return Err(StorageError::new(
            libc::EACCES,
            "SMB transaction artifact is reserved",
        ));
    }
    Ok(())
}

async fn rename_replacing(
    session: &mut SmbSession,
    from: &str,
    to: &str,
) -> Result<(), smb2::Error> {
    rename_path(session, from, to, true).await
}

async fn rename_path(
    session: &mut SmbSession,
    from: &str,
    to: &str,
    replace: bool,
) -> Result<(), smb2::Error> {
    let opened = open_path(
        session,
        from,
        CreateDisposition::FileOpen,
        FileAccessMask::new(FileAccessMask::DELETE | FileAccessMask::FILE_READ_ATTRIBUTES),
        ShareAccess(
            ShareAccess::FILE_SHARE_READ
                | ShareAccess::FILE_SHARE_WRITE
                | ShareAccess::FILE_SHARE_DELETE,
        ),
        0,
    )
    .await?;
    let request = SetInfoRequest {
        info_type: InfoType::File,
        file_info_class: FILE_RENAME_INFORMATION,
        additional_information: 0,
        file_id: opened.file_id,
        buffer: build_rename_information(&smb2::encode_path(to), replace),
    };
    let tree_id = session.tree.tree_id;
    let renamed = session
        .connection_mut()
        .execute(Command::SetInfo, &request, Some(tree_id))
        .await
        .and_then(|frame| {
            expect_success(&frame, Command::SetInfo)?;
            Ok(())
        });
    let _ = close_handle(session, opened.file_id, false).await;
    match renamed {
        Ok(()) => Ok(()),
        Err(error) => Err(error),
    }
}

fn build_rename_information(target: &str, replace: bool) -> Vec<u8> {
    let target = target.encode_utf16().collect::<Vec<_>>();
    let mut buffer = Vec::with_capacity(20 + target.len() * 2);
    buffer.push(u8::from(replace));
    buffer.extend_from_slice(&[0; 7]);
    buffer.extend_from_slice(&0_u64.to_le_bytes());
    buffer.extend_from_slice(
        &u32::try_from(target.len() * 2)
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    for unit in target {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    buffer
}

fn staging_path(target: &str, identifier: &str) -> String {
    let name = format!(".agora-write-{identifier}.tmp");
    target
        .rsplit_once('/')
        .map_or(name.clone(), |(parent, _)| format!("{parent}/{name}"))
}

fn write_lock_path(target: &str) -> String {
    let digest = Md5::digest(target.as_bytes());
    let identifier = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let name = format!(".agora-lock-{identifier}.lck");
    target
        .rsplit_once('/')
        .map_or(name.clone(), |(parent, _)| format!("{parent}/{name}"))
}

fn validate_transfer_size(length: u64, maximum: u64) -> Result<(), smb2::Error> {
    if length > maximum {
        return Err(std::io::Error::from_raw_os_error(libc::EFBIG).into());
    }
    Ok(())
}

async fn write_locked_file(
    session: &mut SmbSession,
    path: &str,
    expected: Option<&RemoteMetadata>,
    source: &mut File,
    length: u64,
    operation: &mut WriteOperation,
) -> StorageResult<RemoteMetadata> {
    if let Some(staged) = operation.staged.as_ref() {
        match stat_locked_path(session, path).await {
            Ok(current) if same_file_object(&current, staged) => return Ok(current),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(storage_error(error)),
        }
        cleanup_staging(session, &operation.temporary).await;
    } else {
        // A previous attempt may have created the sibling before its response was lost.
        cleanup_staging(session, &operation.temporary).await;
    }
    source
        .seek(SeekFrom::Start(0))
        .map_err(|error| storage_error(error.into()))?;
    let temporary = operation.temporary.clone();
    let staged = write_staged_file(session, &temporary, source, length).await;
    let metadata = match staged {
        Ok(metadata) => metadata,
        Err(error) => return Err(storage_error(error)),
    };
    operation.staged = Some(metadata.clone());

    let write_lock = match acquire_write_lock(session, path).await {
        Ok(write_lock) => write_lock,
        Err(error) => {
            cleanup_staging(session, &temporary).await;
            return Err(error);
        }
    };

    let locked = match lock_expected_target(session, path, expected).await {
        Ok(locked) => locked,
        Err(error) => {
            cleanup_staging(session, &temporary).await;
            release_write_lock(session, write_lock).await?;
            return Err(error);
        }
    };
    let verified = verify_expected_target(session, path, expected).await;
    let published = match verified {
        Ok(()) => rename_path(session, &temporary, path, expected.is_some()).await,
        Err(error) => Err(error),
    };
    if let Some(file_id) = locked {
        close_transaction_handle(session, file_id, "expected target").await?;
    }
    let result = match published {
        Ok(()) => match stat_locked_path(session, path).await {
            Ok(published) if same_file_object(&published, &metadata) => Ok(published),
            Ok(_) => Err(stale_file()),
            Err(error) => Err(storage_error(error)),
        },
        Err(error) => {
            cleanup_staging(session, &temporary).await;
            if expected.is_none() && error.kind() == ErrorKind::AlreadyExists {
                Err(stale_file())
            } else {
                Err(storage_error(error))
            }
        }
    };
    release_write_lock(session, write_lock).await?;
    result
}

struct WriteOperation {
    temporary: String,
    staged: Option<RemoteMetadata>,
}

impl WriteOperation {
    fn new(path: &str) -> Self {
        Self {
            temporary: staging_path(path, &Uuid::new_v4().simple().to_string()),
            staged: None,
        }
    }
}

fn same_file_object(left: &RemoteMetadata, right: &RemoteMetadata) -> bool {
    left.identity.rsplit(':').next() == right.identity.rsplit(':').next()
}

async fn acquire_write_lock(session: &mut SmbSession, target: &str) -> StorageResult<FileId> {
    let path = write_lock_path(target);
    let deadline = tokio::time::Instant::now() + WRITE_LOCK_TIMEOUT;
    loop {
        match create_write_lock(session, &path).await {
            Ok(file_id) => return Ok(file_id),
            Err(error) if write_lock_is_busy(&error) && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) if write_lock_is_busy(&error) => {
                return Err(StorageError::new(
                    libc::EBUSY,
                    "timed out waiting for the remote write transaction lock",
                ));
            }
            Err(error) => return Err(storage_error(error)),
        }
    }
}

fn write_lock_is_busy(error: &smb2::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::AlreadyExists | ErrorKind::SharingViolation
    ) || error.status() == Some(NtStatus::DELETE_PENDING)
}

async fn create_write_lock(session: &mut SmbSession, path: &str) -> Result<FileId, smb2::Error> {
    let create = build_open_request(
        &session.tree,
        path,
        CreateDisposition::FileCreate,
        FileAccessMask::new(FileAccessMask::DELETE | FileAccessMask::SYNCHRONIZE),
        ShareAccess(0),
        FILE_NON_DIRECTORY_FILE,
    );
    let disposition = SetInfoRequest {
        info_type: InfoType::File,
        file_info_class: FILE_DISPOSITION_INFORMATION,
        additional_information: 0,
        file_id: FileId::SENTINEL,
        buffer: vec![1],
    };
    let operations = [
        CompoundOp::new(Command::Create, &create, Some(session.tree.tree_id)),
        CompoundOp::new(Command::SetInfo, &disposition, Some(session.tree.tree_id)),
    ];
    let responses = session
        .connection_mut()
        .execute_compound(&operations)
        .await?;
    if responses.len() != operations.len() {
        return Err(smb2::Error::invalid_data(
            "SMB write lock returned an incomplete compound response",
        ));
    }
    let mut responses = responses.into_iter();
    let created = responses
        .next()
        .expect("compound response length was checked")?;
    expect_success(&created, Command::Create)?;
    let opened = CreateResponse::unpack(&mut ReadCursor::new(&created.body))?;
    let disposition = responses
        .next()
        .expect("compound response length was checked");
    match disposition.and_then(|frame| expect_success(&frame, Command::SetInfo)) {
        Ok(()) => Ok(opened.file_id),
        Err(error) => {
            let _ = close_handle(session, opened.file_id, false).await;
            cleanup_staging(session, path).await;
            Err(error)
        }
    }
}

async fn release_write_lock(session: &mut SmbSession, file_id: FileId) -> StorageResult<()> {
    close_transaction_handle(session, file_id, "write lock").await
}

async fn close_transaction_handle(
    session: &mut SmbSession,
    file_id: FileId,
    description: &str,
) -> StorageResult<()> {
    close_handle(session, file_id, false)
        .await
        .map(|_| ())
        .map_err(|error| {
            StorageError::new(
                libc::EIO,
                format!("failed to close SMB {description}: {error}"),
            )
        })
}

async fn verify_expected_target(
    session: &mut SmbSession,
    path: &str,
    expected: Option<&RemoteMetadata>,
) -> Result<(), smb2::Error> {
    match (expected, stat_locked_path(session, path).await) {
        (Some(expected), Ok(current)) if current.identity == expected.identity => Ok(()),
        (None, Err(error)) if error.kind() == ErrorKind::NotFound => Ok(()),
        (Some(_), Err(error)) if error.kind() == ErrorKind::NotFound => Err(smb2::Error::Io(
            std::io::Error::from_raw_os_error(libc::ESTALE),
        )),
        (Some(_), Ok(_)) | (None, Ok(_)) => Err(smb2::Error::Io(
            std::io::Error::from_raw_os_error(libc::ESTALE),
        )),
        (_, Err(error)) => Err(error),
    }
}

async fn write_staged_file(
    session: &mut SmbSession,
    path: &str,
    source: &mut File,
    length: u64,
) -> Result<RemoteMetadata, smb2::Error> {
    let opened = open_file(
        session,
        path,
        CreateDisposition::FileCreate,
        FileAccessMask::new(
            FileAccessMask::FILE_WRITE_DATA
                | FileAccessMask::FILE_WRITE_ATTRIBUTES
                | FileAccessMask::SYNCHRONIZE,
        ),
        ShareAccess(0),
    )
    .await?;
    let file_index = match query_file_index(session, opened.file_id).await {
        Ok(file_index) => file_index,
        Err(error) => {
            let _ = close_handle(session, opened.file_id, false).await;
            cleanup_staging(session, path).await;
            return Err(error);
        }
    };
    let operation = async {
        write_handle(session, opened.file_id, source, length).await?;
        set_handle_length(session, opened.file_id, length).await?;
        flush_handle(session, opened.file_id).await?;
        close_handle(session, opened.file_id, true)
            .await?
            .ok_or_else(|| smb2::Error::invalid_data("SMB close omitted post-query attributes"))
    }
    .await;
    match operation {
        Ok(closed) => Ok(metadata_from_close(&closed, file_index)),
        Err(error) => {
            let _ = close_handle(session, opened.file_id, false).await;
            cleanup_staging(session, path).await;
            Err(error)
        }
    }
}

async fn lock_expected_target(
    session: &mut SmbSession,
    path: &str,
    expected: Option<&RemoteMetadata>,
) -> StorageResult<Option<FileId>> {
    let Some(expected) = expected else {
        return Ok(None);
    };
    let opened = match open_file(
        session,
        path,
        CreateDisposition::FileOpen,
        FileAccessMask::new(FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE),
        ShareAccess(ShareAccess::FILE_SHARE_READ | ShareAccess::FILE_SHARE_DELETE),
    )
    .await
    {
        Ok(opened) => opened,
        Err(error) if error.kind() == ErrorKind::NotFound => return Err(stale_file()),
        Err(error) => return Err(storage_error(error)),
    };
    let file_index = match query_file_index(session, opened.file_id).await {
        Ok(file_index) => file_index,
        Err(error) => {
            let _ = close_handle(session, opened.file_id, false).await;
            return Err(storage_error(error));
        }
    };
    if expected.identity != metadata_from_create(&opened, file_index).identity {
        let _ = close_handle(session, opened.file_id, false).await;
        return Err(stale_file());
    }
    Ok(Some(opened.file_id))
}

async fn stat_locked_path(
    session: &mut SmbSession,
    path: &str,
) -> Result<RemoteMetadata, smb2::Error> {
    let opened = open_path(
        session,
        path,
        CreateDisposition::FileOpen,
        FileAccessMask::new(FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE),
        ShareAccess(
            ShareAccess::FILE_SHARE_READ
                | ShareAccess::FILE_SHARE_WRITE
                | ShareAccess::FILE_SHARE_DELETE,
        ),
        0,
    )
    .await?;
    let result = query_file_index(session, opened.file_id)
        .await
        .map(|file_index| metadata_from_create(&opened, file_index));
    let closed = close_handle(session, opened.file_id, false).await;
    match result {
        Ok(metadata) => {
            closed?;
            Ok(metadata)
        }
        Err(error) => {
            let _ = closed;
            Err(error)
        }
    }
}

async fn query_file_index(session: &mut SmbSession, file_id: FileId) -> Result<u64, smb2::Error> {
    let request = QueryInfoRequest {
        info_type: QueryInfoType::File,
        file_info_class: FILE_INTERNAL_INFORMATION,
        output_buffer_length: 8,
        additional_information: 0,
        flags: 0,
        file_id,
        input_buffer: Vec::new(),
    };
    let tree_id = session.tree.tree_id;
    let frame = session
        .connection_mut()
        .execute(Command::QueryInfo, &request, Some(tree_id))
        .await?;
    expect_success(&frame, Command::QueryInfo)?;
    let response = QueryInfoResponse::unpack(&mut ReadCursor::new(&frame.body))?;
    let bytes: [u8; 8] = response
        .output_buffer
        .as_slice()
        .try_into()
        .map_err(|_| smb2::Error::invalid_data("SMB file identity is not eight bytes"))?;
    Ok(u64::from_le_bytes(bytes))
}

async fn cleanup_staging(session: &mut SmbSession, path: &str) {
    let _ = session.delete_file(path).await;
}

async fn open_direct_handle(
    session: &mut SmbSession,
    path: &str,
    disposition: CreateDisposition,
    desired_access: FileAccessMask,
) -> Result<(CreateResponse, u64), smb2::Error> {
    let opened = open_file(
        session,
        path,
        disposition,
        desired_access,
        ShareAccess(
            ShareAccess::FILE_SHARE_READ
                | ShareAccess::FILE_SHARE_WRITE
                | ShareAccess::FILE_SHARE_DELETE,
        ),
    )
    .await?;
    match query_file_index(session, opened.file_id).await {
        Ok(index) => Ok((opened, index)),
        Err(error) => {
            let _ = close_handle(session, opened.file_id, false).await;
            Err(error)
        }
    }
}

async fn open_file(
    session: &mut SmbSession,
    path: &str,
    disposition: CreateDisposition,
    desired_access: FileAccessMask,
    share_access: ShareAccess,
) -> Result<CreateResponse, smb2::Error> {
    open_path(
        session,
        path,
        disposition,
        desired_access,
        share_access,
        FILE_NON_DIRECTORY_FILE,
    )
    .await
}

async fn open_path(
    session: &mut SmbSession,
    path: &str,
    disposition: CreateDisposition,
    desired_access: FileAccessMask,
    share_access: ShareAccess,
    create_options: u32,
) -> Result<CreateResponse, smb2::Error> {
    let request = build_open_request(
        &session.tree,
        path,
        disposition,
        desired_access,
        share_access,
        create_options,
    );
    let tree_id = session.tree.tree_id;
    let frame = session
        .connection_mut()
        .execute(Command::Create, &request, Some(tree_id))
        .await?;
    expect_success(&frame, Command::Create)?;
    CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
}

fn build_open_request(
    tree: &Tree,
    path: &str,
    disposition: CreateDisposition,
    desired_access: FileAccessMask,
    share_access: ShareAccess,
    create_options: u32,
) -> CreateRequest {
    CreateRequest {
        requested_oplock_level: OplockLevel::None,
        impersonation_level: ImpersonationLevel::Impersonation,
        desired_access,
        file_attributes: FILE_ATTRIBUTE_NORMAL,
        share_access,
        create_disposition: disposition,
        create_options,
        name: wire_path(tree, path),
        create_contexts: Vec::new(),
    }
}

async fn read_handle(
    session: &mut SmbSession,
    file_id: FileId,
    length: u64,
    destination: &mut File,
) -> Result<(), smb2::Error> {
    let mut offset = 0_u64;
    while offset < length {
        let remaining = length - offset;
        let chunk = remaining.min(TRANSFER_CHUNK_SIZE as u64) as u32;
        let request = ReadRequest {
            padding: 0x50,
            flags: 0,
            length: chunk,
            offset,
            file_id,
            minimum_count: 0,
            channel: SMB2_CHANNEL_NONE,
            remaining_bytes: 0,
            read_channel_info: Vec::new(),
        };
        let tree_id = session.tree.tree_id;
        let frame = session
            .connection_mut()
            .execute(Command::Read, &request, Some(tree_id))
            .await?;
        if frame.header.status == NtStatus::END_OF_FILE {
            return Err(smb2::Error::invalid_data(
                "SMB file ended before its advertised length",
            ));
        }
        expect_success(&frame, Command::Read)?;
        let response = ReadResponse::unpack(&mut ReadCursor::new(&frame.body))?;
        offset += validate_read_response_size(chunk, remaining, response.data.len())?;
        destination.write_all(&response.data)?;
    }
    Ok(())
}

async fn read_handle_at(
    session: &mut SmbSession,
    file_id: FileId,
    offset: u64,
    length: u32,
    destination: &mut File,
) -> Result<u32, smb2::Error> {
    destination.set_len(0)?;
    destination.seek(SeekFrom::Start(0))?;
    if length == 0 {
        return Ok(0);
    }
    let request = ReadRequest {
        padding: 0x50,
        flags: 0,
        length,
        offset,
        file_id,
        minimum_count: 0,
        channel: SMB2_CHANNEL_NONE,
        remaining_bytes: 0,
        read_channel_info: Vec::new(),
    };
    let tree_id = session.tree.tree_id;
    let frame = session
        .connection_mut()
        .execute_with_credits(
            Command::Read,
            &request,
            Some(tree_id),
            CreditCharge((u64::from(length).div_ceil(65_536)) as u16),
        )
        .await?;
    if frame.header.status == NtStatus::END_OF_FILE {
        return Ok(0);
    }
    expect_success(&frame, Command::Read)?;
    let response = ReadResponse::unpack(&mut ReadCursor::new(&frame.body))?;
    if response.data.len() > length as usize {
        return Err(smb2::Error::invalid_data(
            "SMB read response exceeds the requested length",
        ));
    }
    destination.write_all(&response.data)?;
    u32::try_from(response.data.len())
        .map_err(|_| smb2::Error::invalid_data("SMB read response is too large"))
}

fn validate_read_response_size(
    requested: u32,
    remaining: u64,
    received: usize,
) -> Result<u64, smb2::Error> {
    let received = u64::try_from(received)
        .map_err(|_| smb2::Error::invalid_data("SMB read response is too large"))?;
    if received == 0 {
        return Err(smb2::Error::invalid_data(
            "SMB read returned no data before the advertised end of file",
        ));
    }
    if received > u64::from(requested) || received > remaining {
        return Err(smb2::Error::invalid_data(
            "SMB read response exceeds the requested length",
        ));
    }
    Ok(received)
}

async fn write_handle(
    session: &mut SmbSession,
    file_id: FileId,
    source: &mut File,
    length: u64,
) -> Result<(), smb2::Error> {
    let mut offset = 0_u64;
    let mut buffer = vec![0_u8; TRANSFER_CHUNK_SIZE];
    while offset < length {
        let chunk = usize::try_from((length - offset).min(TRANSFER_CHUNK_SIZE as u64))
            .map_err(|_| smb2::Error::invalid_data("SMB write chunk is too large"))?;
        source.read_exact(&mut buffer[..chunk])?;
        let request = WriteRequest {
            data_offset: 0x70,
            offset,
            file_id,
            channel: 0,
            remaining_bytes: 0,
            write_channel_info_offset: 0,
            write_channel_info_length: 0,
            flags: 0,
            data: buffer[..chunk].to_vec(),
        };
        let tree_id = session.tree.tree_id;
        let frame = session
            .connection_mut()
            .execute(Command::Write, &request, Some(tree_id))
            .await?;
        expect_success(&frame, Command::Write)?;
        let response = WriteResponse::unpack(&mut ReadCursor::new(&frame.body))?;
        if response.count as usize != chunk {
            return Err(smb2::Error::invalid_data(
                "SMB write returned a short count",
            ));
        }
        offset += chunk as u64;
    }
    Ok(())
}

async fn write_handle_at(
    session: &mut SmbSession,
    file_id: FileId,
    offset: u64,
    source: &mut File,
    length: u32,
) -> Result<u32, smb2::Error> {
    if length == 0 {
        return Ok(0);
    }
    let mut data = vec![0_u8; length as usize];
    source.read_exact(&mut data)?;
    let request = WriteRequest {
        data_offset: 0x70,
        offset,
        file_id,
        channel: 0,
        remaining_bytes: 0,
        write_channel_info_offset: 0,
        write_channel_info_length: 0,
        flags: 0,
        data,
    };
    let tree_id = session.tree.tree_id;
    let frame = session
        .connection_mut()
        .execute_with_credits(
            Command::Write,
            &request,
            Some(tree_id),
            CreditCharge((u64::from(length).div_ceil(65_536)) as u16),
        )
        .await?;
    expect_success(&frame, Command::Write)?;
    let response = WriteResponse::unpack(&mut ReadCursor::new(&frame.body))?;
    if response.count > length {
        return Err(smb2::Error::invalid_data(
            "SMB write response exceeds the requested length",
        ));
    }
    Ok(response.count)
}

async fn query_handle_metadata(
    session: &mut SmbSession,
    file_id: FileId,
) -> Result<RemoteMetadata, smb2::Error> {
    let basic = query_handle_information(session, file_id, FILE_BASIC_INFORMATION, 40).await?;
    let standard =
        query_handle_information(session, file_id, FILE_STANDARD_INFORMATION, 24).await?;
    let index = query_handle_information(session, file_id, FILE_INTERNAL_INFORMATION, 8).await?;
    let mut basic = ReadCursor::new(&basic);
    let created = smb2::pack::FileTime::unpack(&mut basic)?;
    let _accessed = smb2::pack::FileTime::unpack(&mut basic)?;
    let modified = smb2::pack::FileTime::unpack(&mut basic)?;
    let changed = smb2::pack::FileTime::unpack(&mut basic)?;
    let attributes = basic.read_u32_le()?;
    let mut standard = ReadCursor::new(&standard);
    let _allocation = standard.read_u64_le()?;
    let size = standard.read_u64_le()?;
    let _links = standard.read_u32_le()?;
    let _delete_pending = standard.read_u8()?;
    let directory = standard.read_u8()? != 0;
    let bytes: [u8; 8] = index
        .as_slice()
        .try_into()
        .map_err(|_| smb2::Error::invalid_data("SMB file identity is not eight bytes"))?;
    Ok(metadata_with_creation(
        if directory {
            attributes | FILE_ATTRIBUTE_DIRECTORY
        } else {
            attributes & !FILE_ATTRIBUTE_DIRECTORY
        },
        size,
        modified,
        created,
        changed,
        u64::from_le_bytes(bytes),
    ))
}

async fn query_handle_information(
    session: &mut SmbSession,
    file_id: FileId,
    file_info_class: u8,
    output_buffer_length: u32,
) -> Result<Vec<u8>, smb2::Error> {
    let request = QueryInfoRequest {
        info_type: QueryInfoType::File,
        file_info_class,
        output_buffer_length,
        additional_information: 0,
        flags: 0,
        file_id,
        input_buffer: Vec::new(),
    };
    let tree_id = session.tree.tree_id;
    let frame = session
        .connection_mut()
        .execute(Command::QueryInfo, &request, Some(tree_id))
        .await?;
    expect_success(&frame, Command::QueryInfo)?;
    Ok(QueryInfoResponse::unpack(&mut ReadCursor::new(&frame.body))?.output_buffer)
}

async fn set_handle_length(
    session: &mut SmbSession,
    file_id: FileId,
    length: u64,
) -> Result<(), smb2::Error> {
    let request = SetInfoRequest {
        info_type: InfoType::File,
        file_info_class: FILE_END_OF_FILE_INFORMATION,
        additional_information: 0,
        file_id,
        buffer: length.to_le_bytes().to_vec(),
    };
    let tree_id = session.tree.tree_id;
    let frame = session
        .connection_mut()
        .execute(Command::SetInfo, &request, Some(tree_id))
        .await?;
    expect_success(&frame, Command::SetInfo)
}

async fn flush_handle(session: &mut SmbSession, file_id: FileId) -> Result<(), smb2::Error> {
    let tree_id = session.tree.tree_id;
    let frame = session
        .connection_mut()
        .execute(Command::Flush, &FlushRequest { file_id }, Some(tree_id))
        .await?;
    expect_success(&frame, Command::Flush)
}

async fn close_handle(
    session: &mut SmbSession,
    file_id: FileId,
    attributes: bool,
) -> Result<Option<CloseResponse>, smb2::Error> {
    let flags = if attributes {
        SMB2_CLOSE_FLAG_POSTQUERY_ATTRIB
    } else {
        0
    };
    let tree_id = session.tree.tree_id;
    let frame = session
        .connection_mut()
        .execute(
            Command::Close,
            &CloseRequest { flags, file_id },
            Some(tree_id),
        )
        .await?;
    expect_success(&frame, Command::Close)?;
    attributes
        .then(|| CloseResponse::unpack(&mut ReadCursor::new(&frame.body)))
        .transpose()
}

fn expect_success(frame: &smb2::Frame, command: Command) -> Result<(), smb2::Error> {
    if frame.header.status == NtStatus::SUCCESS {
        Ok(())
    } else {
        Err(smb2::Error::Protocol {
            status: frame.header.status,
            command,
        })
    }
}

fn wire_path(tree: &Tree, path: &str) -> String {
    let path = smb2::encode_path(path);
    if !tree.is_dfs {
        return path;
    }
    let server = tree.server.split(':').next().unwrap_or(&tree.server);
    if path.is_empty() {
        format!("{server}\\{}", tree.share_name)
    } else {
        format!("{server}\\{}\\{path}", tree.share_name)
    }
}

fn stale_file() -> StorageError {
    StorageError::new(libc::ESTALE, "remote file changed since it was opened")
}

struct SmbRoot {
    config: SmbRemoteConfig,
    slots: Vec<Arc<Mutex<SmbSlot>>>,
    next_slot: AtomicUsize,
}

struct SmbSlot {
    session: Option<SmbSession>,
    generation: u64,
}

impl SmbRoot {
    fn new(config: SmbRemoteConfig) -> Self {
        Self {
            config,
            slots: (0..SMB_SESSION_POOL_SIZE)
                .map(|_| Arc::new(Mutex::new(SmbSlot::new())))
                .collect(),
            next_slot: AtomicUsize::new(0),
        }
    }

    #[cfg(test)]
    fn with_slots(config: SmbRemoteConfig, slots: Vec<SmbSlot>) -> Self {
        assert!(!slots.is_empty());
        Self {
            config,
            slots: slots
                .into_iter()
                .map(|slot| Arc::new(Mutex::new(slot)))
                .collect(),
            next_slot: AtomicUsize::new(0),
        }
    }

    fn slot(&self, index: usize) -> StorageResult<Arc<Mutex<SmbSlot>>> {
        self.slots
            .get(index)
            .cloned()
            .ok_or_else(|| StorageError::new(libc::EINVAL, "unknown SMB session slot"))
    }

    fn next_slot(&self) -> (usize, Arc<Mutex<SmbSlot>>) {
        let index = self.next_slot.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        (index, Arc::clone(&self.slots[index]))
    }

    fn path(&self, path: &RemotePath) -> String {
        remote_path(self.config.remote_path(), path)
    }
}

impl SmbSlot {
    fn new() -> Self {
        Self {
            session: None,
            generation: 0,
        }
    }

    async fn session(&mut self, config: &SmbRemoteConfig) -> StorageResult<&mut SmbSession> {
        if self.session.is_none() {
            let mut client = SmbClient::connect(ClientConfig {
                addr: config.server().to_string(),
                timeout: Duration::from_secs(5),
                username: config.username().to_string(),
                password: config.password().to_string(),
                domain: config.domain().to_string(),
                auto_reconnect: true,
                compression: true,
                dfs_enabled: true,
                dfs_target_overrides: HashMap::new(),
            })
            .await
            .map_err(storage_error)?;
            let tree = client
                .connect_share(config.share())
                .await
                .map_err(storage_error)?;
            self.generation = self.generation.wrapping_add(1);
            self.session = Some(SmbSession {
                connection: SmbConnection::Client(Box::new(client)),
                tree,
            });
        }
        self.session
            .as_mut()
            .ok_or_else(|| StorageError::new(libc::EIO, "SMB session was not initialized"))
    }

    fn invalidate_session(&mut self) {
        self.session = None;
        self.generation = self.generation.wrapping_add(1);
    }

    fn finish_file_operation<T>(&mut self, result: StorageResult<T>) -> StorageResult<T> {
        if result.as_ref().is_err_and(is_connection_failure) {
            self.invalidate_session();
        }
        result
    }
}

struct SmbSession {
    connection: SmbConnection,
    tree: Tree,
}

enum SmbConnection {
    Client(Box<SmbClient>),
    #[cfg(any(test, coverage))]
    #[cfg_attr(all(coverage, not(test)), allow(dead_code))]
    Transport(Connection),
}

impl SmbSession {
    #[cfg(test)]
    fn from_connection(connection: Connection, tree: Tree) -> Self {
        Self {
            connection: SmbConnection::Transport(connection),
            tree,
        }
    }

    fn connection_mut(&mut self) -> &mut Connection {
        match &mut self.connection {
            SmbConnection::Client(client) => client.connection_mut(),
            #[cfg(any(test, coverage))]
            SmbConnection::Transport(connection) => connection,
        }
    }

    fn params(&self) -> Option<smb2::NegotiatedParams> {
        match &self.connection {
            SmbConnection::Client(client) => client.params(),
            #[cfg(any(test, coverage))]
            SmbConnection::Transport(connection) => connection.params(),
        }
    }

    async fn stat(&mut self, path: &str) -> Result<smb2::FileInfo, smb2::Error> {
        match &mut self.connection {
            SmbConnection::Client(client) => client.stat(&mut self.tree, path).await,
            #[cfg(any(test, coverage))]
            SmbConnection::Transport(connection) => self.tree.stat(connection, path).await,
        }
    }

    async fn create_directory(&mut self, path: &str) -> Result<(), smb2::Error> {
        match &mut self.connection {
            SmbConnection::Client(client) => client.create_directory(&mut self.tree, path).await,
            #[cfg(any(test, coverage))]
            SmbConnection::Transport(connection) => {
                self.tree.create_directory(connection, path).await
            }
        }
    }

    async fn delete_file(&mut self, path: &str) -> Result<(), smb2::Error> {
        match &mut self.connection {
            SmbConnection::Client(client) => client.delete_file(&mut self.tree, path).await,
            #[cfg(any(test, coverage))]
            SmbConnection::Transport(connection) => self.tree.delete_file(connection, path).await,
        }
    }

    async fn delete_directory(&mut self, path: &str) -> Result<(), smb2::Error> {
        match &mut self.connection {
            SmbConnection::Client(client) => client.delete_directory(&mut self.tree, path).await,
            #[cfg(any(test, coverage))]
            SmbConnection::Transport(connection) => {
                self.tree.delete_directory(connection, path).await
            }
        }
    }
}

fn remote_path(base: &str, path: &RemotePath) -> String {
    match (base.is_empty(), path.path().is_empty()) {
        (true, _) => path.path().to_string(),
        (_, true) => base.to_string(),
        (false, false) => format!("{base}/{}", path.path()),
    }
}

#[cfg(test)]
fn metadata_from_file(info: &smb2::FileInfo) -> RemoteMetadata {
    let mut metadata = metadata(
        if info.is_directory {
            RemoteFileType::Directory
        } else {
            RemoteFileType::File
        },
        info.size,
        info.modified,
    );
    metadata.identity = format!("{}:{}", metadata.identity, info.created.0);
    metadata
}

fn validate_remote_root(result: Result<smb2::FileInfo, smb2::Error>) -> StorageResult<()> {
    let info = result.map_err(storage_error)?;
    if !info.is_directory {
        return Err(StorageError::new(
            libc::ENOTDIR,
            "configured SMB root is not a directory",
        ));
    }
    Ok(())
}

fn metadata_from_create(response: &CreateResponse, file_index: u64) -> RemoteMetadata {
    metadata_with_creation(
        response.file_attributes,
        response.end_of_file,
        response.last_write_time,
        response.creation_time,
        response.change_time,
        file_index,
    )
}

fn metadata_from_close(response: &CloseResponse, file_index: u64) -> RemoteMetadata {
    metadata_with_creation(
        response.file_attributes,
        response.end_of_file,
        response.last_write_time,
        response.creation_time,
        response.change_time,
        file_index,
    )
}

fn metadata_with_creation(
    attributes: u32,
    size: u64,
    modified: smb2::pack::FileTime,
    created: smb2::pack::FileTime,
    changed: smb2::pack::FileTime,
    file_index: u64,
) -> RemoteMetadata {
    let mut metadata = metadata(
        if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            RemoteFileType::Directory
        } else {
            RemoteFileType::File
        },
        size,
        modified,
    );
    metadata.identity = format!(
        "{}:{}:{}:{file_index}",
        metadata.identity, created.0, changed.0
    );
    metadata
}

fn metadata(
    file_type: RemoteFileType,
    size: u64,
    modified: smb2::pack::FileTime,
) -> RemoteMetadata {
    let (modified_seconds, modified_nanoseconds) = modified
        .to_system_time()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs() as i64, duration.subsec_nanos()))
        .unwrap_or((0, 0));
    let kind = match file_type {
        RemoteFileType::File => "file",
        RemoteFileType::Directory => "directory",
    };
    RemoteMetadata {
        file_type,
        size,
        modified_seconds,
        modified_nanoseconds,
        identity: format!("{kind}:{size}:{}", modified.0),
    }
}

fn storage_error(error: smb2::Error) -> StorageError {
    StorageError::new(smb_errno(&error), format!("SMB operation failed: {error}"))
}

fn smb_errno(error: &smb2::Error) -> libc::c_int {
    if error.status() == Some(smb2::types::status::NtStatus::DIRECTORY_NOT_EMPTY) {
        return libc::ENOTEMPTY;
    }
    if error.status() == Some(smb2::types::status::NtStatus::DELETE_PENDING) {
        return libc::EBUSY;
    }
    match error.kind() {
        ErrorKind::AuthRequired | ErrorKind::SigningRequired | ErrorKind::AccessDenied => {
            libc::EACCES
        }
        ErrorKind::NotFound => libc::ENOENT,
        ErrorKind::AlreadyExists => libc::EEXIST,
        ErrorKind::SharingViolation => libc::EBUSY,
        ErrorKind::IsADirectory => libc::EISDIR,
        ErrorKind::NotADirectory => libc::ENOTDIR,
        ErrorKind::DiskFull => libc::ENOSPC,
        ErrorKind::ConnectionLost => libc::ENETDOWN,
        ErrorKind::TimedOut => libc::ETIMEDOUT,
        ErrorKind::Cancelled => libc::EINTR,
        ErrorKind::SessionExpired => libc::EIO,
        ErrorKind::DfsReferral => libc::EXDEV,
        ErrorKind::InvalidData => libc::EPROTO,
        ErrorKind::TooLarge => libc::EFBIG,
        ErrorKind::Io => match error {
            smb2::Error::Io(error) => error.raw_os_error().unwrap_or(libc::EIO),
            _ => libc::EIO,
        },
        ErrorKind::InvalidName => libc::EINVAL,
        ErrorKind::Unsupported => libc::ENOTSUP,
        _ => libc::EIO,
    }
}

#[cfg(test)]
mod tests;
