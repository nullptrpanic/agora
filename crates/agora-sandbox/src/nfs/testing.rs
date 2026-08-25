use crate::filesystem::ByteRange;
use crate::nfs::backend::{RemoteStorage, StorageError, StorageResult};
use crate::nfs::protocol::{RemoteEntry, RemoteFileType, RemoteMetadata, RemotePath};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone)]
struct MemoryEntry {
    data: Option<Vec<u8>>,
    generation: u64,
}

type SharedMemoryEntry = Arc<Mutex<MemoryEntry>>;

pub(crate) struct MemoryFileHandle {
    entry: SharedMemoryEntry,
    readable: bool,
    writable: bool,
}

#[derive(Default)]
pub(crate) struct MemoryStorage {
    entries: Mutex<HashMap<(u32, String), SharedMemoryEntry>>,
    connection_errors: Mutex<HashMap<u32, (libc::c_int, String)>>,
    connections_blocked: AtomicBool,
    connection_release: tokio::sync::Notify,
    yield_operations: AtomicBool,
    stat_operations: AtomicUsize,
    flush_operations: AtomicUsize,
    list_visits: AtomicUsize,
    reset_operations: AtomicUsize,
    resets_blocked: AtomicBool,
    reset_release: tokio::sync::Notify,
    stats_blocked: AtomicBool,
    stat_release: tokio::sync::Notify,
    reads_blocked: AtomicBool,
    read_started: tokio::sync::Notify,
    read_release: tokio::sync::Notify,
    snapshot_replacement: Mutex<Option<Vec<u8>>>,
    read_ranges: Mutex<Vec<ByteRange>>,
}

impl MemoryStorage {
    pub(crate) fn insert_file(&self, root: u32, path: &str, data: &[u8]) {
        lock(&self.entries).insert(
            (root, path.to_string()),
            Arc::new(Mutex::new(MemoryEntry {
                data: Some(data.to_vec()),
                generation: 1,
            })),
        );
    }

    pub(crate) fn insert_directory(&self, root: u32, path: &str) {
        lock(&self.entries).insert(
            (root, path.to_string()),
            Arc::new(Mutex::new(MemoryEntry {
                data: None,
                generation: 1,
            })),
        );
    }

    pub(crate) fn data(&self, root: u32, path: &str) -> Option<Vec<u8>> {
        lock(&self.entries)
            .get(&(root, path.to_string()))
            .and_then(|entry| lock(entry).data.clone())
    }

    pub(crate) fn exists(&self, root: u32, path: &str) -> bool {
        lock(&self.entries).contains_key(&(root, path.to_string()))
    }

    pub(crate) fn replace(&self, root: u32, path: &str, data: &[u8]) {
        let entries = lock(&self.entries);
        let entry = entries.get(&(root, path.to_string())).unwrap();
        let mut entry = lock(entry);
        entry.data = Some(data.to_vec());
        entry.generation += 1;
    }

    pub(crate) fn yield_operations(&self) {
        self.yield_operations.store(true, Ordering::Relaxed);
    }

    pub(crate) fn stat_operations(&self) -> usize {
        self.stat_operations.load(Ordering::Relaxed)
    }

    pub(crate) fn flush_operations(&self) -> usize {
        self.flush_operations.load(Ordering::Relaxed)
    }

    pub(crate) fn list_visits(&self) -> usize {
        self.list_visits.load(Ordering::Relaxed)
    }

    pub(crate) fn reset_operations(&self) -> usize {
        self.reset_operations.load(Ordering::Relaxed)
    }

    pub(crate) fn block_resets(&self) {
        self.resets_blocked.store(true, Ordering::Release);
    }

    pub(crate) fn release_resets(&self) {
        self.resets_blocked.store(false, Ordering::Release);
        self.reset_release.notify_waiters();
    }

    pub(crate) fn block_stats(&self) {
        self.stats_blocked.store(true, Ordering::Release);
    }

    pub(crate) fn release_stats(&self) {
        self.stats_blocked.store(false, Ordering::Release);
        self.stat_release.notify_waiters();
    }

    pub(crate) fn block_reads(&self) {
        self.reads_blocked.store(true, Ordering::Release);
    }

    pub(crate) async fn wait_until_read_started(&self) {
        self.read_started.notified().await;
    }

    pub(crate) fn release_reads(&self) {
        self.reads_blocked.store(false, Ordering::Release);
        self.read_release.notify_waiters();
    }

    pub(crate) fn replace_during_snapshot_read(&self, data: &[u8]) {
        *lock(&self.snapshot_replacement) = Some(data.to_vec());
    }

    pub(crate) fn read_ranges(&self) -> Vec<ByteRange> {
        lock(&self.read_ranges).clone()
    }

    pub(crate) fn fail_connection(
        &self,
        root: u32,
        errno: libc::c_int,
        message: impl Into<String>,
    ) {
        lock(&self.connection_errors).insert(root, (errno, message.into()));
    }

    pub(crate) fn block_connections(&self) {
        self.connections_blocked.store(true, Ordering::Release);
    }

    pub(crate) fn release_connections(&self) {
        self.connections_blocked.store(false, Ordering::Release);
        self.connection_release.notify_waiters();
    }

    pub(crate) async fn connection_result(&self, root: u32) -> StorageResult<()> {
        loop {
            let released = self.connection_release.notified();
            if !self.connections_blocked.load(Ordering::Acquire) {
                break;
            }
            released.await;
        }
        match lock(&self.connection_errors).get(&root).cloned() {
            Some((errno, message)) => Err(StorageError::new(errno, message)),
            None => Ok(()),
        }
    }

    async fn yield_if_requested(&self) {
        if self.yield_operations.load(Ordering::Relaxed) {
            tokio::task::yield_now().await;
        }
    }

    async fn wait_for_stat_release(&self) {
        loop {
            let released = self.stat_release.notified();
            if !self.stats_blocked.load(Ordering::Acquire) {
                break;
            }
            released.await;
        }
    }

    async fn wait_for_reset_release(&self) {
        loop {
            let released = self.reset_release.notified();
            if !self.resets_blocked.load(Ordering::Acquire) {
                break;
            }
            released.await;
        }
    }

    async fn wait_for_read_release(&self) {
        self.read_started.notify_one();
        loop {
            let released = self.read_release.notified();
            if !self.reads_blocked.load(Ordering::Acquire) {
                break;
            }
            released.await;
        }
    }

    fn metadata(entry: &MemoryEntry) -> RemoteMetadata {
        RemoteMetadata {
            file_type: if entry.data.is_some() {
                RemoteFileType::File
            } else {
                RemoteFileType::Directory
            },
            size: entry.data.as_ref().map_or(0, |data| data.len() as u64),
            modified_seconds: entry.generation as i64,
            modified_nanoseconds: 0,
            identity: entry.generation.to_string(),
        }
    }
}

impl RemoteStorage for MemoryStorage {
    type FileHandle = MemoryFileHandle;

    async fn reset(&self, _root: u32) {
        self.reset_operations.fetch_add(1, Ordering::Relaxed);
        self.wait_for_reset_release().await;
    }

    async fn connect(&self, root: u32) -> StorageResult<()> {
        self.connection_result(root).await
    }

    async fn stat(&self, path: &RemotePath) -> StorageResult<RemoteMetadata> {
        self.stat_operations.fetch_add(1, Ordering::Relaxed);
        self.wait_for_stat_release().await;
        self.yield_if_requested().await;
        lock(&self.entries)
            .get(&(path.root(), path.path().to_string()))
            .map(|entry| Self::metadata(&lock(entry)))
            .ok_or_else(StorageError::not_found)
    }

    async fn open_file(
        &self,
        path: &RemotePath,
        flags: libc::c_int,
        _mode: u32,
    ) -> StorageResult<(Self::FileHandle, RemoteMetadata, bool)> {
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
        let key = (path.root(), path.path().to_string());
        let mut entries = lock(&self.entries);
        let existing = entries.get(&key).cloned();
        let created = existing.is_none();
        if existing.is_some()
            && flags & (libc::O_CREAT | libc::O_EXCL) == libc::O_CREAT | libc::O_EXCL
        {
            return Err(StorageError::new(libc::EEXIST, "path already exists"));
        }
        let entry = match existing {
            Some(entry) => entry,
            None if flags & libc::O_CREAT != 0 => {
                let entry = Arc::new(Mutex::new(MemoryEntry {
                    data: Some(Vec::new()),
                    generation: 1,
                }));
                entries.insert(key, Arc::clone(&entry));
                entry
            }
            None => return Err(StorageError::not_found()),
        };
        drop(entries);
        {
            let mut entry = lock(&entry);
            if entry.data.is_none() {
                return Err(StorageError::new(libc::EISDIR, "path is a directory"));
            }
            if flags & libc::O_TRUNC != 0 {
                entry.data = Some(Vec::new());
                entry.generation += 1;
            }
        }
        let metadata = Self::metadata(&lock(&entry));
        Ok((
            MemoryFileHandle {
                entry,
                readable,
                writable,
            },
            metadata,
            created,
        ))
    }

    async fn read_at(
        &self,
        handle: &mut Self::FileHandle,
        offset: u64,
        length: u32,
        destination: &mut File,
    ) -> StorageResult<u32> {
        if !handle.readable {
            return Err(StorageError::new(
                libc::EBADF,
                "file is not open for reading",
            ));
        }
        lock(&self.read_ranges).push(ByteRange {
            start: offset,
            end: offset.saturating_add(u64::from(length)),
        });
        let entry = lock(&handle.entry);
        let data = entry
            .data
            .as_deref()
            .ok_or_else(|| StorageError::new(libc::EISDIR, "path is a directory"))?;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(data.len());
        let end = start.saturating_add(length as usize).min(data.len());
        destination
            .set_len(0)
            .and_then(|()| destination.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| destination.write_all(&data[start..end]))
            .map_err(|error| memory_io("failed to stream memory file range", error))?;
        let actual = u32::try_from(end - start).expect("read length is bounded by u32");
        drop(entry);
        if let Some(replacement) = lock(&self.snapshot_replacement).take() {
            let mut entry = lock(&handle.entry);
            entry.data = Some(replacement);
            entry.generation += 1;
        }
        Ok(actual)
    }

    async fn write_at(
        &self,
        handle: &mut Self::FileHandle,
        offset: u64,
        source: &mut File,
        length: u32,
    ) -> StorageResult<(u32, u64)> {
        if !handle.writable {
            return Err(StorageError::new(
                libc::EBADF,
                "file is not open for writing",
            ));
        }
        source
            .seek(SeekFrom::Start(0))
            .map_err(|error| memory_io("failed to rewind memory write payload", error))?;
        let mut bytes = vec![0_u8; length as usize];
        source
            .read_exact(&mut bytes)
            .map_err(|error| memory_io("failed to read memory write payload", error))?;
        let mut entry = lock(&handle.entry);
        let data = entry
            .data
            .as_mut()
            .ok_or_else(|| StorageError::new(libc::EISDIR, "path is a directory"))?;
        let start = usize::try_from(offset)
            .map_err(|_| StorageError::new(libc::EFBIG, "write offset is too large"))?;
        let end = start
            .checked_add(bytes.len())
            .ok_or_else(|| StorageError::new(libc::EFBIG, "write range is too large"))?;
        if data.len() < end {
            data.resize(end, 0);
        }
        data[start..end].copy_from_slice(&bytes);
        entry.generation += 1;
        Ok((
            length,
            entry.data.as_ref().map_or(0, |data| data.len() as u64),
        ))
    }

    async fn set_length(&self, handle: &mut Self::FileHandle, length: u64) -> StorageResult<u64> {
        if !handle.writable {
            return Err(StorageError::new(
                libc::EBADF,
                "file is not open for writing",
            ));
        }
        let length = usize::try_from(length)
            .map_err(|_| StorageError::new(libc::EFBIG, "file length is too large"))?;
        let mut entry = lock(&handle.entry);
        entry
            .data
            .as_mut()
            .ok_or_else(|| StorageError::new(libc::EISDIR, "path is a directory"))?
            .resize(length, 0);
        entry.generation += 1;
        Ok(length as u64)
    }

    async fn flush_file(&self, handle: &mut Self::FileHandle) -> StorageResult<RemoteMetadata> {
        self.flush_operations.fetch_add(1, Ordering::Relaxed);
        Ok(Self::metadata(&lock(&handle.entry)))
    }

    async fn file_metadata(&self, handle: &mut Self::FileHandle) -> StorageResult<RemoteMetadata> {
        Ok(Self::metadata(&lock(&handle.entry)))
    }

    async fn close_file(&self, _handle: &mut Self::FileHandle) -> StorageResult<()> {
        Ok(())
    }

    async fn read_into(
        &self,
        handle: &mut Self::FileHandle,
        destination: &mut File,
        max_length: u64,
    ) -> StorageResult<RemoteMetadata> {
        self.wait_for_read_release().await;
        if !handle.readable {
            return Err(StorageError::new(
                libc::EBADF,
                "file is not open for reading",
            ));
        }
        let entry = lock(&handle.entry);
        let data = entry
            .data
            .as_deref()
            .ok_or_else(|| StorageError::new(libc::EISDIR, "path is a directory"))?;
        if data.len() as u64 > max_length {
            return Err(StorageError::new(
                libc::EFBIG,
                "remote file exceeds the sandbox snapshot limit",
            ));
        }
        destination
            .set_len(0)
            .and_then(|()| destination.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| destination.write_all(data))
            .map_err(|error| memory_io("failed to stream memory file", error))?;
        let metadata = Self::metadata(&entry);
        drop(entry);
        if let Some(replacement) = lock(&self.snapshot_replacement).take() {
            let mut entry = lock(&handle.entry);
            entry.data = Some(replacement);
            entry.generation += 1;
        }
        if Self::metadata(&lock(&handle.entry)).identity != metadata.identity {
            return Err(StorageError::new(
                libc::ESTALE,
                "remote file changed while creating a snapshot",
            ));
        }
        Ok(metadata)
    }

    async fn write_from_if_unchanged(
        &self,
        path: &RemotePath,
        expected: Option<&RemoteMetadata>,
        source: &mut File,
        length: u64,
    ) -> StorageResult<RemoteMetadata> {
        self.yield_if_requested().await;
        source
            .seek(SeekFrom::Start(0))
            .map_err(|error| memory_io("failed to rewind memory file", error))?;
        let mut data = Vec::new();
        source
            .take(length)
            .read_to_end(&mut data)
            .map_err(|error| memory_io("failed to stream memory file", error))?;
        if data.len() as u64 != length {
            return Err(StorageError::new(
                libc::EIO,
                "memory file ended before its declared length",
            ));
        }
        let mut entries = lock(&self.entries);
        let key = (path.root(), path.path().to_string());
        let current = entries.get(&key).map(|entry| Self::metadata(&lock(entry)));
        let unchanged = match (expected, current.as_ref()) {
            (None, None) => true,
            (Some(expected), Some(current)) => expected.identity == current.identity,
            _ => false,
        };
        if !unchanged {
            return Err(StorageError::new(
                libc::ESTALE,
                "remote file changed since it was opened",
            ));
        }
        let generation = entries
            .get(&key)
            .map_or(1, |entry| lock(entry).generation + 1);
        let entry = MemoryEntry {
            data: Some(data),
            generation,
        };
        let metadata = Self::metadata(&entry);
        entries.insert(key, Arc::new(Mutex::new(entry)));
        Ok(metadata)
    }

    async fn list(
        &self,
        path: &RemotePath,
        emit: &mut (impl FnMut(RemoteEntry) -> StorageResult<()> + Send),
    ) -> StorageResult<()> {
        let entries = lock(&self.entries);
        let directory = entries
            .get(&(path.root(), path.path().to_string()))
            .ok_or_else(StorageError::not_found)?;
        if lock(directory).data.is_some() {
            return Err(StorageError::new(libc::ENOTDIR, "path is not a directory"));
        }
        let prefix = if path.path().is_empty() {
            String::new()
        } else {
            format!("{}/", path.path())
        };
        for ((root, child), entry) in entries.iter() {
            let Some(name) = (*root == path.root())
                .then(|| child.strip_prefix(&prefix))
                .flatten()
                .filter(|suffix| !suffix.is_empty() && !suffix.contains('/'))
            else {
                continue;
            };
            self.list_visits.fetch_add(1, Ordering::Relaxed);
            emit(RemoteEntry {
                name: name.to_string(),
                metadata: Self::metadata(&lock(entry)),
            })?;
        }
        Ok(())
    }

    async fn create_directory(&self, path: &RemotePath) -> StorageResult<()> {
        let mut entries = lock(&self.entries);
        let key = (path.root(), path.path().to_string());
        if entries.contains_key(&key) {
            return Err(StorageError::new(libc::EEXIST, "path already exists"));
        }
        entries.insert(
            key,
            Arc::new(Mutex::new(MemoryEntry {
                data: None,
                generation: 1,
            })),
        );
        Ok(())
    }

    async fn remove(&self, path: &RemotePath, directory: bool) -> StorageResult<()> {
        let mut entries = lock(&self.entries);
        let key = (path.root(), path.path().to_string());
        let entry = entries.get(&key).ok_or_else(StorageError::not_found)?;
        if directory != lock(entry).data.is_none() {
            return Err(StorageError::new(
                if directory {
                    libc::ENOTDIR
                } else {
                    libc::EISDIR
                },
                "entry type does not match remove operation",
            ));
        }
        if directory {
            let prefix = format!("{}/", path.path());
            if entries
                .keys()
                .any(|(root, child)| *root == path.root() && child.starts_with(&prefix))
            {
                return Err(StorageError::new(libc::ENOTEMPTY, "directory is not empty"));
            }
        }
        entries.remove(&key);
        Ok(())
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath) -> StorageResult<()> {
        if from.root() != to.root() {
            return Err(StorageError::new(libc::EXDEV, "cross-root rename"));
        }
        let mut entries = lock(&self.entries);
        let entry = entries
            .remove(&(from.root(), from.path().to_string()))
            .ok_or_else(StorageError::not_found)?;
        let descendants = if lock(&entry).data.is_none() {
            let prefix = format!("{}/", from.path());
            entries
                .keys()
                .filter(|(root, path)| *root == from.root() && path.starts_with(&prefix))
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        entries.insert((to.root(), to.path().to_string()), entry);
        for (root, source) in descendants {
            let entry = entries.remove(&(root, source.clone())).unwrap();
            let suffix = source.strip_prefix(from.path()).unwrap();
            entries.insert((root, format!("{}{}", to.path(), suffix)), entry);
        }
        Ok(())
    }
}

fn memory_io(context: &str, error: std::io::Error) -> StorageError {
    StorageError::new(
        error.raw_os_error().unwrap_or(libc::EIO),
        format!("{context}: {error}"),
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_with_contents(contents: &[u8]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(contents).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }

    fn assert_errno<T>(result: StorageResult<T>, expected: libc::c_int) {
        match result {
            Ok(_) => panic!("operation unexpectedly succeeded"),
            Err(error) => assert_eq!(error.errno(), expected),
        }
    }

    #[tokio::test]
    async fn memory_storage_preserves_posix_conflict_type_and_directory_errors() {
        let storage = MemoryStorage::default();
        storage.insert_file(0, "file", b"data");
        storage.insert_directory(0, "directory");
        storage.insert_file(0, "directory/child", b"child");

        assert_errno(
            storage
                .create_directory(&RemotePath::new(0, "file").unwrap())
                .await,
            libc::EEXIST,
        );
        assert_errno(
            storage
                .remove(&RemotePath::new(0, "file").unwrap(), true)
                .await,
            libc::ENOTDIR,
        );
        assert_errno(
            storage
                .remove(&RemotePath::new(0, "directory").unwrap(), false)
                .await,
            libc::EISDIR,
        );
        assert_errno(
            storage
                .remove(&RemotePath::new(0, "directory").unwrap(), true)
                .await,
            libc::ENOTEMPTY,
        );
        assert_errno(
            storage
                .rename(
                    &RemotePath::new(0, "file").unwrap(),
                    &RemotePath::new(1, "file").unwrap(),
                )
                .await,
            libc::EXDEV,
        );

        let path = RemotePath::new(0, "file").unwrap();
        let expected = storage.stat(&path).await.unwrap();
        storage.replace(0, "file", b"outside");
        let mut source = file_with_contents(b"sandbox");
        assert_errno(
            storage
                .write_from_if_unchanged(&path, Some(&expected), &mut source, 7)
                .await,
            libc::ESTALE,
        );
        let mut source = file_with_contents(b"sandbox");
        assert_errno(
            storage
                .write_from_if_unchanged(&path, None, &mut source, 7)
                .await,
            libc::ESTALE,
        );
    }

    #[tokio::test]
    async fn memory_storage_transfers_file_contents_through_streams() {
        let storage = MemoryStorage::default();
        let path = RemotePath::new(0, "file").unwrap();
        storage.insert_file(0, "file", b"remote");

        let mut downloaded = tempfile::tempfile().unwrap();
        let (mut handle, _, _) = storage.open_file(&path, libc::O_RDONLY, 0).await.unwrap();
        let baseline = storage
            .read_into(&mut handle, &mut downloaded, u64::MAX)
            .await
            .unwrap();
        downloaded.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = String::new();
        downloaded.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "remote");

        let mut uploaded = tempfile::tempfile().unwrap();
        uploaded.write_all(b"sandbox").unwrap();
        uploaded.seek(SeekFrom::Start(0)).unwrap();
        storage
            .write_from_if_unchanged(&path, Some(&baseline), &mut uploaded, 7)
            .await
            .unwrap();
        assert_eq!(storage.data(0, "file").unwrap(), b"sandbox");
    }
}
