use super::crypto::FileCipher;
use super::metadata::{EntryState, FileAttributes, Materializer, MetadataStore, SourceIdentity};
use super::namespace;
use super::{normalize_path, resolve_existing_ancestor};
use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

const LOCK_DESCRIPTOR_POOL_CAPACITY: usize = 16;
// Increment when executable or loader preparation can produce different bytes
// for the same source and target platform.
const PREPARED_FILE_CACHE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackingIdentity {
    device: u64,
    inode: u64,
}

impl BackingIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

pub(crate) struct OverlayStore {
    root: PathBuf,
    canonical_root: PathBuf,
    metadata: MetadataStore,
    lock_path: PathBuf,
    lock_pool: Mutex<LockDescriptorPool>,
    cipher: Option<FileCipher>,
    #[cfg(test)]
    lock_open_count: AtomicUsize,
    #[cfg(test)]
    transaction_count: AtomicUsize,
    #[cfg(test)]
    reconciliation_count: AtomicUsize,
    #[cfg(test)]
    resolution_count: AtomicUsize,
}

struct LockDescriptorPool {
    pid: libc::pid_t,
    files: Vec<File>,
}

pub(super) struct OverlayTransaction<'a> {
    store: &'a OverlayStore,
}

impl OverlayTransaction<'_> {
    pub(super) fn logical_path(&self, path: &Path) -> Result<PathBuf> {
        self.store.logical_path_locked(path)
    }

    pub(super) fn prepare_read(&self, path: &Path) -> Result<PathBuf> {
        let path = self.store.normalize(path)?;
        if self.store.is_internal(&path) {
            return Ok(path);
        }
        self.store.prepare_read_locked(&path)
    }

    pub(super) fn visible_exists(&self, path: &Path) -> Result<bool> {
        let path = self.store.normalize(path)?;
        self.store.visible_exists_locked(&path)
    }

    pub(super) fn bind_socket<T>(
        &self,
        path: &Path,
        bind: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        let path = self.store.normalize(path)?;
        self.store.bind_socket_locked(&path, bind)
    }

    pub(super) fn resolve_final(&self, path: &Path, allow_missing: bool) -> Result<PathBuf> {
        let path = self.store.normalize(path)?;
        self.store.resolve_final_locked(path, allow_missing)
    }

    pub(super) fn visible_path(&self, path: &Path) -> Result<PathBuf> {
        let path = self.store.logical_entry_path_locked(path)?;
        self.store.visible_path_locked(&path)
    }

    pub(super) fn attributes(&self, path: &Path) -> Result<Option<FileAttributes>> {
        let path = self.store.logical_entry_path_locked(path)?;
        let state = self.store.reconciled_state_locked(&path)?;
        let attributes = self.store.metadata.attributes(&path)?;
        match (state.as_ref(), attributes) {
            (None, attributes) => Ok(attributes),
            (Some(state), Some(attributes))
                if state.stored_attributes_are_authoritative(&attributes) =>
            {
                Ok(Some(attributes))
            }
            (Some(_), _) => Ok(None),
        }
    }

    pub(super) fn records(
        &self,
        paths: &[&Path],
    ) -> Result<Vec<(Option<EntryState>, Option<FileAttributes>)>> {
        let paths = paths
            .iter()
            .map(|path| self.store.normalize(path))
            .collect::<Result<Vec<_>>>()?;
        let recorded_paths = self.store.reconcile_records_locked(&paths)?;
        let stored_paths = paths
            .iter()
            .filter(|path| recorded_paths.contains(path.as_path()))
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        let mut stored = self.store.metadata.records(&stored_paths)?.into_iter();
        Ok(paths
            .iter()
            .map(|path| {
                if !recorded_paths.contains(path.as_path()) {
                    (None, None)
                } else {
                    stored
                        .next()
                        .expect("metadata record count matches non-root path count")
                }
            })
            .collect())
    }

    pub(super) fn native_metadata_passthrough(
        &self,
        path: &Path,
        follow_final: bool,
        ancestor_access: impl Fn(&FileAttributes) -> bool,
    ) -> Result<bool> {
        let path = self.store.normalize(path)?;
        if self.store.is_internal(&path) {
            return Ok(false);
        }
        if path == Path::new("/") {
            return Ok(true);
        }
        let native_resolved = if follow_final {
            resolve_existing_ancestor(&path)?
        } else {
            let parent = path.parent().context("filesystem path has no parent")?;
            let name = path
                .file_name()
                .context("filesystem path has no file name")?;
            resolve_existing_ancestor(parent)?.join(name)
        };
        let overlay_resolved = if follow_final {
            match path.symlink_metadata() {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    match self.resolve_final(&path, false) {
                        Ok(resolved) => Some(resolved),
                        Err(error) if OverlayStore::is_not_found(&error) => return Ok(false),
                        Err(error) => return Err(error),
                    }
                }
                Ok(_) => None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            }
        } else {
            None
        };
        let mut checked = HashSet::new();
        let mut paths = Vec::new();
        let mut final_entries = Vec::new();
        for candidate in [&path, &native_resolved]
            .into_iter()
            .chain(overlay_resolved.as_ref())
        {
            for ancestor in candidate.ancestors() {
                if ancestor == Path::new("/") || !checked.insert(ancestor.to_path_buf()) {
                    continue;
                }
                paths.push(ancestor);
                final_entries.push(ancestor == candidate.as_path());
            }
        }
        for ((state, attributes), final_entry) in
            self.records(&paths)?.into_iter().zip(final_entries)
        {
            if state.is_some() {
                return Ok(false);
            }
            if let Some(attributes) = attributes
                && (final_entry || !ancestor_access(&attributes))
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn stage_file_open(
        &self,
        path: &Path,
        create: bool,
        exclusive: bool,
    ) -> Result<(StagedWrite, bool, Option<File>)> {
        let path = self.store.normalize(path)?;
        if self.store.is_internal(&path) {
            return Ok((
                StagedWrite {
                    destination: path.clone(),
                    logical: path,
                    reservation: None,
                },
                true,
                None,
            ));
        }
        self.store.stage_file_open_locked(path, create, exclusive)
    }

    pub(super) fn prepare_directory(&self, path: &Path) -> Result<PathBuf> {
        let path = self.store.normalize(path)?;
        if self.store.is_internal(&path) {
            return Ok(path);
        }
        self.store.prepare_directory_locked(&path)
    }

    pub(super) fn directory_view(&self, path: &Path) -> Result<DirectoryView> {
        let path = self.store.normalize(path)?;
        self.store.directory_view_locked(&path)
    }

    pub(super) fn set_attributes(&self, path: &Path, attributes: FileAttributes) -> Result<()> {
        let path = self.store.logical_entry_path_locked(path)?;
        self.store.set_attributes_locked(&path, attributes)
    }

    pub(super) fn create_directory(&self, path: &Path, mode: u32) -> Result<PathBuf> {
        let path = self.store.normalize(path)?;
        self.store.create_directory_locked(&path, mode)
    }

    pub(super) fn create_symlink(&self, path: &Path, target: &Path) -> Result<PathBuf> {
        let path = self.store.normalize(path)?;
        self.store.create_symlink_locked(&path, target)
    }

    pub(super) fn remove(&self, path: &Path, directory: bool) -> Result<()> {
        let path = self.store.normalize(path)?;
        self.store.remove_locked(&path, directory)
    }

    pub(super) fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let from = self.store.normalize(from)?;
        let to = self.store.normalize(to)?;
        self.store.rename_locked(&from, &to)
    }
}

pub(crate) struct DirectoryView {
    logical: PathBuf,
    primary: PathBuf,
    lower: Option<PathBuf>,
    hidden: BTreeSet<OsString>,
    aliases: HashMap<OsString, OsString>,
    native_snapshot: Option<NativeDirectorySnapshot>,
}

#[derive(Clone)]
pub(crate) struct NativeDirectorySnapshot {
    generation: u64,
    absent_upper: PathBuf,
}

pub(crate) struct StagedWrite {
    logical: PathBuf,
    destination: PathBuf,
    reservation: Option<WriteReservation>,
}

struct WriteReservation {
    file: File,
    lock_path: PathBuf,
}

impl StagedWrite {
    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    fn commit(&mut self) {
        drop(self.reservation.take());
    }
}

impl Drop for StagedWrite {
    fn drop(&mut self) {
        let Some(reservation) = self.reservation.take() else {
            return;
        };
        let Ok(lock) = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&reservation.lock_path)
        else {
            return;
        };
        if OverlayStore::flock(&lock, libc::LOCK_EX).is_err() {
            return;
        }
        let reserved = reservation
            .file
            .metadata()
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino()));
        let current = self
            .destination
            .symlink_metadata()
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino()));
        if reserved.is_some() && reserved == current {
            let _ = fs::remove_file(&self.destination);
        }
        let _ = OverlayStore::flock(&lock, libc::LOCK_UN);
    }
}

fn is_private_path_with_roots(root: &Path, canonical_root: &Path, path: &Path) -> Result<bool> {
    let path = normalize_path(path)?;
    if root
        .parent()
        .is_some_and(|workdir| path.starts_with(workdir))
        || canonical_root
            .parent()
            .is_some_and(|workdir| path.starts_with(workdir))
    {
        return Ok(true);
    }
    let resolved = resolve_existing_ancestor(&path)?;
    Ok(canonical_root
        .parent()
        .is_some_and(|workdir| resolved.starts_with(workdir)))
}

impl DirectoryView {
    pub(crate) fn passthrough(path: PathBuf) -> Self {
        Self {
            logical: path.clone(),
            primary: path,
            lower: None,
            hidden: BTreeSet::new(),
            aliases: HashMap::new(),
            native_snapshot: None,
        }
    }

    pub(crate) fn logical(&self) -> &Path {
        &self.logical
    }

    pub(crate) fn primary(&self) -> &Path {
        &self.primary
    }

    pub(crate) fn lower(&self) -> Option<&Path> {
        self.lower.as_deref()
    }

    pub(crate) fn hidden(&self) -> &BTreeSet<OsString> {
        &self.hidden
    }

    pub(crate) fn aliases(&self) -> &HashMap<OsString, OsString> {
        &self.aliases
    }

    pub(crate) fn is_passthrough(&self) -> bool {
        self.primary == self.logical
            && self.lower.is_none()
            && self.hidden.is_empty()
            && self.aliases.is_empty()
    }

    pub(crate) fn native_snapshot(&self) -> Option<&NativeDirectorySnapshot> {
        self.is_passthrough()
            .then_some(self.native_snapshot.as_ref())
            .flatten()
    }
}

impl OverlayStore {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Result<Self> {
        Self::with_cipher(root, None)
    }

    pub(crate) fn encrypted(root: impl Into<PathBuf>, cipher: FileCipher) -> Result<Self> {
        Self::with_cipher(root, Some(cipher))
    }

    fn with_cipher(root: impl Into<PathBuf>, cipher: Option<FileCipher>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create filesystem root {}", root.display()))?;
        let canonical_root = root
            .canonicalize()
            .with_context(|| format!("failed to resolve filesystem root {}", root.display()))?;
        let lock_path = canonical_root.join(namespace::VFS_LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("failed to open overlay lock {}", lock_path.display()))?;
        Self::flock(&lock, libc::LOCK_EX)?;
        let root_marker_present = match canonical_root
            .join(namespace::METADATA_FILE)
            .symlink_metadata()
        {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let metadata =
            MetadataStore::with_generation(&canonical_root, lock.try_clone()?, cipher.clone())
                .and_then(|metadata| {
                    if !root_marker_present {
                        Self::discard_untrusted_root_entries(&canonical_root)?;
                        metadata.invalidate()?;
                    }
                    Ok(metadata)
                });
        let metadata = metadata?;
        let store = Self {
            root,
            canonical_root,
            metadata,
            lock_path,
            lock_pool: Mutex::new(LockDescriptorPool {
                pid: unsafe { libc::getpid() },
                files: Vec::new(),
            }),
            cipher,
            #[cfg(test)]
            lock_open_count: AtomicUsize::new(1),
            #[cfg(test)]
            transaction_count: AtomicUsize::new(0),
            #[cfg(test)]
            reconciliation_count: AtomicUsize::new(0),
            #[cfg(test)]
            resolution_count: AtomicUsize::new(0),
        };
        let unlock = Self::flock(&lock, libc::LOCK_UN);
        if unlock.is_ok() {
            store.return_lock_descriptor(lock);
        }
        unlock?;
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    #[cfg(test)]
    fn lock_open_count(&self) -> usize {
        self.lock_open_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn transaction_count_for_test(&self) -> usize {
        self.transaction_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn reconciliation_count_for_test(&self) -> usize {
        self.reconciliation_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn resolution_count_for_test(&self) -> usize {
        self.resolution_count.load(Ordering::Relaxed)
    }

    pub(super) fn transaction<T>(
        &self,
        operation: impl FnOnce(&OverlayTransaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.with_lock(|| operation(&OverlayTransaction { store: self }))
    }

    pub(crate) fn is_internal(&self, path: &Path) -> bool {
        path.starts_with(&self.root) || path.starts_with(&self.canonical_root)
    }

    pub(crate) fn is_private(&self, path: &Path) -> Result<bool> {
        is_private_path_with_roots(&self.root, &self.canonical_root, path)
    }

    pub(crate) fn logical_path(&self, path: &Path) -> Result<PathBuf> {
        self.with_lock(|| self.logical_path_locked(path))
    }

    fn logical_path_locked(&self, path: &Path) -> Result<PathBuf> {
        let logical = namespace::logical_path(&self.root, path)
            .or_else(|_| namespace::logical_path(&self.canonical_root, path))?;
        let Some(encrypted_name) = path.file_name() else {
            return Ok(logical);
        };
        let Some(parent) = logical.parent() else {
            return Ok(logical);
        };
        if let Some((logical_name, _)) = self
            .metadata
            .encrypted_names(parent)?
            .into_iter()
            .find(|(_, physical)| physical == encrypted_name)
        {
            return Ok(parent.join(logical_name));
        }
        Ok(logical)
    }

    fn logical_entry_path_locked(&self, path: &Path) -> Result<PathBuf> {
        if self.is_internal(path) {
            self.logical_path_locked(path)
        } else {
            self.normalize(path)
        }
    }

    #[cfg(test)]
    pub(crate) fn prepare_read(&self, path: &Path) -> Result<PathBuf> {
        self.transaction(|transaction| transaction.prepare_read(path))
    }

    #[cfg(test)]
    pub(crate) fn resolve_final(&self, path: &Path, allow_missing: bool) -> Result<PathBuf> {
        self.transaction(|transaction| transaction.resolve_final(path, allow_missing))
    }

    pub(crate) fn visible_path(&self, path: &Path) -> Result<PathBuf> {
        self.transaction(|transaction| transaction.visible_path(path))
    }

    pub(crate) fn state(&self, path: &Path) -> Result<Option<EntryState>> {
        self.with_lock(|| {
            let path = self.logical_entry_path_locked(path)?;
            self.reconciled_state_locked(&path)
        })
    }

    pub(crate) fn attributes(&self, path: &Path) -> Result<Option<FileAttributes>> {
        self.transaction(|transaction| transaction.attributes(path))
    }

    pub(crate) fn set_attributes(&self, path: &Path, attributes: FileAttributes) -> Result<()> {
        self.transaction(|transaction| transaction.set_attributes(path, attributes))
    }

    pub(crate) fn cipher(&self) -> Option<&FileCipher> {
        self.cipher.as_ref()
    }

    pub(crate) fn exists(&self, path: &Path) -> Result<bool> {
        self.transaction(|transaction| transaction.visible_exists(path))
    }

    #[cfg(not(agora_sandbox_hook_build))]
    pub(crate) fn visible_directory(&self, path: &Path) -> Result<Option<bool>> {
        self.transaction(|transaction| {
            if !transaction.visible_exists(path)? {
                return Ok(None);
            }
            let visible = transaction.visible_path(path)?;
            match visible.metadata() {
                Ok(metadata) => Ok(Some(metadata.is_dir())),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn native_metadata_passthrough(
        &self,
        path: &Path,
        follow_final: bool,
        ancestor_access: impl Fn(&FileAttributes) -> bool,
    ) -> Result<bool> {
        self.transaction(|transaction| {
            transaction.native_metadata_passthrough(path, follow_final, ancestor_access)
        })
    }

    pub(crate) fn mark_executable(&self, path: &Path) -> Result<()> {
        self.with_lock(|| {
            let path = self.logical_entry_path_locked(path)?;
            if let Some(EntryState::Cached {
                checksum, source, ..
            }) = self.reconciled_state_locked(&path)?
            {
                self.metadata.set(
                    &path,
                    EntryState::Cached {
                        checksum,
                        materializer: Materializer::Executable,
                        source,
                        variant: Some(Self::executable_variant()),
                        destination: None,
                    },
                )?;
            }
            Ok(())
        })
    }

    #[cfg(test)]
    pub(crate) fn prepare_write(&self, path: &Path, create: bool) -> Result<PathBuf> {
        let staged = self.stage_write(path, create)?;
        let destination = staged.destination.clone();
        self.commit_write(staged)?;
        Ok(destination)
    }

    pub(crate) fn stage_write(&self, path: &Path, create: bool) -> Result<StagedWrite> {
        let path = self.normalize(path)?;
        if self.is_internal(&path) {
            return Ok(StagedWrite {
                destination: path.clone(),
                logical: path,
                reservation: None,
            });
        }
        let destination = self.with_lock(|| self.stage_write_locked(&path, create))?;
        Ok(StagedWrite {
            logical: path,
            destination,
            reservation: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn stage_file_open(
        &self,
        path: &Path,
        create: bool,
        exclusive: bool,
    ) -> Result<(StagedWrite, bool, Option<File>)> {
        self.transaction(|transaction| transaction.stage_file_open(path, create, exclusive))
    }

    pub(crate) fn commit_write(&self, mut staged: StagedWrite) -> Result<()> {
        if self.is_internal(&staged.logical) {
            staged.commit();
            return Ok(());
        }
        self.with_lock(|| self.metadata.set(&staged.logical, EntryState::Cow))?;
        staged.commit();
        Ok(())
    }

    pub(crate) fn commit_created_file(&self, mut staged: StagedWrite, mode: u32) -> Result<()> {
        if self.is_internal(&staged.logical) {
            staged.commit();
            return Ok(());
        }
        self.with_lock(|| {
            self.metadata.set_with_attributes(
                &staged.logical,
                EntryState::Cow,
                Some(FileAttributes::created_file(mode)),
            )
        })?;
        staged.commit();
        Ok(())
    }

    pub(crate) fn publish_encrypted(
        &self,
        plaintext: &mut File,
        lease: &File,
    ) -> Result<Option<PathBuf>> {
        let cipher = self
            .cipher
            .as_ref()
            .context("encrypted writeback requires a filesystem cipher")?;
        self.with_lock(|| {
            let Some(destination) = Self::read_write_lease_destination(lease)? else {
                return Ok(None);
            };
            if !self.is_internal(&destination) {
                return Err(std::io::Error::from_raw_os_error(libc::EIO).into());
            }
            let current_lease = match File::open(Self::write_lease_path(&destination)?) {
                Ok(current) => current,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let held_identity = lease
                .metadata()
                .map(|metadata| (metadata.dev(), metadata.ino()))?;
            let current_identity = current_lease
                .metadata()
                .map(|metadata| (metadata.dev(), metadata.ino()))?;
            if held_identity != current_identity {
                return Ok(None);
            }
            cipher.encrypt(plaintext, &destination)?;
            Ok(Some(destination))
        })
    }

    pub(crate) fn overwrite_encrypted(
        &self,
        plaintext: &mut File,
        lease: &File,
    ) -> Result<Option<PathBuf>> {
        let cipher = self
            .cipher
            .as_ref()
            .context("encrypted writeback requires a filesystem cipher")?;
        self.with_lock(|| {
            let Some(destination) = Self::read_write_lease_destination(lease)? else {
                return Ok(None);
            };
            if !self.is_internal(&destination) {
                return Err(std::io::Error::from_raw_os_error(libc::EIO).into());
            }
            let current_lease = match File::open(Self::write_lease_path(&destination)?) {
                Ok(current) => current,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let held_identity = lease
                .metadata()
                .map(|metadata| (metadata.dev(), metadata.ino()))?;
            let current_identity = current_lease
                .metadata()
                .map(|metadata| (metadata.dev(), metadata.ino()))?;
            if held_identity != current_identity {
                return Ok(None);
            }
            cipher.overwrite(plaintext, &destination)?;
            Ok(Some(destination))
        })
    }

    #[cfg(test)]
    pub(crate) fn prepare_directory(&self, path: &Path) -> Result<PathBuf> {
        let path = self.normalize(path)?;
        if self.is_internal(&path) {
            return Ok(path);
        }
        self.with_lock(|| self.prepare_directory_locked(&path))
    }

    pub(crate) fn directory_view(&self, path: &Path) -> Result<DirectoryView> {
        self.transaction(|transaction| transaction.directory_view(path))
    }

    pub(crate) fn native_directory_snapshot_is_current(
        &self,
        snapshot: &NativeDirectorySnapshot,
    ) -> Result<bool> {
        if self.metadata.current_generation()? != snapshot.generation {
            return Ok(false);
        }
        match snapshot.absent_upper.symlink_metadata() {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(test)]
    pub(crate) fn create_directory(&self, path: &Path, mode: u32) -> Result<PathBuf> {
        self.transaction(|transaction| transaction.create_directory(path, mode))
    }

    #[cfg(test)]
    pub(crate) fn remove(&self, path: &Path, directory: bool) -> Result<()> {
        self.transaction(|transaction| transaction.remove(path, directory))
    }

    #[cfg(test)]
    pub(crate) fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        self.transaction(|transaction| transaction.rename(from, to))
    }

    pub(crate) fn prepare_executable<F>(&self, source: &Path, prepare: F) -> Result<PathBuf>
    where
        F: FnOnce(&Path) -> Result<()>,
    {
        self.prepare_plain_cached(
            source,
            Materializer::Executable,
            Some(Self::executable_variant()),
            true,
            "executable",
            prepare,
        )
    }

    pub(crate) fn prepare_loader_image(&self, source: &Path) -> Result<PathBuf> {
        let source = self.normalize(source)?;
        self.prepare_plain_cached(
            &source,
            Materializer::Loader,
            Some(Self::loader_variant()),
            false,
            "loader",
            |temporary| {
                fs::copy(&source, temporary)?;
                Ok(())
            },
        )
    }

    pub(crate) fn prepare_loader_tree(&self, source: &Path) -> Result<PathBuf> {
        let source = self.normalize(source)?;
        self.with_lock(|| {
            let source_metadata = source.symlink_metadata()?;
            if !source_metadata.is_dir() {
                return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR).into());
            }
            let destination = self.plain_destination(&source)?;
            let source_identity = SourceIdentity::from_metadata(&source_metadata);
            let source_checksum = Self::tree_checksum(&source)?;
            let (cached, cow_ancestor) = self.reconciled_entry_locked(&source)?;
            if cow_ancestor {
                bail!(
                    "cannot prepare loader tree below a sandbox-modified directory: {}",
                    source.display()
                );
            }
            match &cached {
                Some(EntryState::Cow) => bail!(
                    "cannot prepare sandbox-modified loader tree: {}",
                    source.display()
                ),
                Some(EntryState::Whiteout) => return Self::not_found(&source),
                Some(EntryState::Cached { .. }) | None => {}
            }
            if let (
                Ok(destination_metadata),
                Some(EntryState::Cached {
                    checksum: Some(cached_checksum),
                    materializer: Materializer::LoaderTree,
                    source: Some(cached_source),
                    variant: cached_variant,
                    destination: cached_destination,
                }),
            ) = (destination.symlink_metadata(), cached.as_ref())
                && destination_metadata.is_dir()
                && *cached_source == source_identity
                && cached_variant.as_ref() == Some(&Self::loader_variant())
                && cached_checksum == &source_checksum
                && *cached_destination == Some(SourceIdentity::from_metadata(&destination_metadata))
                && Self::tree_checksum(&destination)
                    .is_ok_and(|checksum| checksum == source_checksum)
            {
                return Ok(destination);
            }
            if destination.exists()
                && !matches!(
                    cached,
                    Some(EntryState::Cached {
                        materializer: Materializer::LoaderTree,
                        ..
                    })
                )
                && !self.loader_tree_destination_is_replaceable_locked(&source)?
            {
                bail!(
                    "cannot replace sandbox-modified framework loader tree: {}",
                    source.display()
                );
            }
            let parent = destination
                .parent()
                .context("cached loader tree destination has no parent")?;
            self.ensure_parent_locked(&source)?;
            let temporary = parent.join(format!(
                ".agora-loader-tree-{}.tmp",
                Uuid::new_v4().simple()
            ));
            let result = (|| {
                Self::copy_plain_tree(&source, &temporary)?;
                if Self::tree_checksum(&temporary)? != source_checksum {
                    bail!("loader tree changed while it was being prepared");
                }
                Self::remove_existing(&destination)?;
                fs::rename(&temporary, &destination)?;
                self.metadata.set(
                    &source,
                    EntryState::Cached {
                        checksum: Some(source_checksum),
                        materializer: Materializer::LoaderTree,
                        source: Some(source_identity),
                        variant: Some(Self::loader_variant()),
                        destination: Some(SourceIdentity::from_metadata(
                            &destination.symlink_metadata()?,
                        )),
                    },
                )?;
                Ok(destination.clone())
            })();
            if result.is_err() {
                let _ = Self::remove_existing(&temporary);
            }
            result
        })
    }

    fn prepare_plain_cached<F>(
        &self,
        source: &Path,
        materializer: Materializer,
        variant: Option<String>,
        require_executable: bool,
        temporary_label: &str,
        prepare: F,
    ) -> Result<PathBuf>
    where
        F: FnOnce(&Path) -> Result<()>,
    {
        let source = self.normalize(source)?;
        self.with_lock(|| {
            let destination = self.plain_destination(&source)?;
            let source_identity = SourceIdentity::from_metadata(&source.metadata()?);
            let cached = self.reconciled_state_locked(&source)?;
            if materializer == Materializer::Loader {
                match &cached {
                    Some(EntryState::Cow) => bail!(
                        "cannot prepare sandbox-modified loader image: {}",
                        source.display()
                    ),
                    Some(EntryState::Whiteout) => return Self::not_found(&source),
                    Some(EntryState::Cached { .. }) | None => {}
                }
            }
            let destination_metadata = destination.symlink_metadata().ok().filter(|metadata| {
                metadata.is_file() && (!require_executable || metadata.mode() & 0o111 != 0)
            });
            if let (
                Some(destination_metadata),
                Some(EntryState::Cached {
                    checksum: Some(checksum),
                    materializer: cached_materializer,
                    source: Some(cached_source),
                    variant: cached_variant,
                    destination: cached_destination,
                }),
            ) = (destination_metadata, cached)
                && cached_source == source_identity
                && cached_materializer == materializer
                && cached_variant == variant
            {
                let destination_identity = SourceIdentity::from_metadata(&destination_metadata);
                if cached_destination == Some(destination_identity) {
                    return Ok(destination);
                }
                if Self::checksum(&destination).is_ok_and(|current| current == checksum) {
                    self.metadata.set(
                        &source,
                        EntryState::Cached {
                            checksum: Some(checksum),
                            materializer,
                            source: Some(source_identity),
                            variant: variant.clone(),
                            destination: Some(destination_identity),
                        },
                    )?;
                    return Ok(destination);
                }
            }
            let parent = destination
                .parent()
                .context("cached destination has no parent")?;
            self.ensure_parent_locked(&source)?;
            let temporary = parent.join(format!(
                ".agora-{temporary_label}-{}.tmp",
                Uuid::new_v4().simple()
            ));
            let result = (|| {
                prepare(&temporary)?;
                let checksum = Self::checksum(&temporary)?;
                Self::remove_existing(&destination)?;
                fs::rename(&temporary, &destination)?;
                let destination_identity =
                    SourceIdentity::from_metadata(&destination.symlink_metadata()?);
                self.metadata.set(
                    &source,
                    EntryState::Cached {
                        checksum: Some(checksum),
                        materializer,
                        source: Some(source_identity),
                        variant,
                        destination: Some(destination_identity),
                    },
                )?;
                Ok(destination.clone())
            })();
            if result.is_err() {
                let _ = fs::remove_file(temporary);
            }
            result
        })
    }

    pub(crate) fn checksum(path: &Path) -> Result<String> {
        let mut file = File::open(path)
            .with_context(|| format!("failed to open {} for checksum", path.display()))?;
        let mut digest = Md5::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        Ok(Self::hex_digest(digest.finalize().as_slice()))
    }

    fn tree_checksum(root: &Path) -> Result<String> {
        let root_metadata = root.symlink_metadata()?;
        if !root_metadata.is_dir() {
            return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR).into());
        }
        let mut digest = Md5::new();
        digest.update(b"loader-tree-v1");
        digest.update((root_metadata.mode() | 0o700).to_le_bytes());
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            let mut entries = fs::read_dir(&directory)?.collect::<std::io::Result<Vec<_>>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries.into_iter().rev() {
                let path = entry.path();
                let relative = path
                    .strip_prefix(root)
                    .context("loader tree entry escaped its root")?;
                let metadata = path.symlink_metadata()?;
                let file_type = metadata.file_type();
                let kind = if file_type.is_dir() {
                    b'd'
                } else if file_type.is_file() {
                    b'f'
                } else if file_type.is_symlink() {
                    b'l'
                } else {
                    return Err(std::io::Error::from_raw_os_error(libc::ENOTSUP).into());
                };
                digest.update([kind]);
                Self::update_digest_bytes(&mut digest, relative.as_os_str().as_bytes());
                let mode = if file_type.is_dir() {
                    metadata.mode() | 0o700
                } else {
                    metadata.mode()
                };
                digest.update(mode.to_le_bytes());
                if file_type.is_dir() {
                    pending.push(path);
                } else if file_type.is_symlink() {
                    let target = fs::read_link(path)?;
                    Self::update_digest_bytes(&mut digest, target.as_os_str().as_bytes());
                } else {
                    digest.update(metadata.len().to_le_bytes());
                    let mut file = File::open(path)?;
                    let mut buffer = [0_u8; 64 * 1024];
                    loop {
                        let read = file.read(&mut buffer)?;
                        if read == 0 {
                            break;
                        }
                        digest.update(&buffer[..read]);
                    }
                }
            }
        }
        Ok(Self::hex_digest(digest.finalize().as_slice()))
    }

    fn loader_tree_destination_is_replaceable_locked(&self, source: &Path) -> Result<bool> {
        let mut pending = vec![source.to_path_buf()];
        while let Some(directory) = pending.pop() {
            if !self
                .metadata
                .contains_only_loader_cache_records(&directory)?
            {
                return Ok(false);
            }
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    pending.push(entry.path());
                }
            }
        }
        Ok(true)
    }

    fn update_digest_bytes(digest: &mut Md5, bytes: &[u8]) {
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }

    fn copy_plain_tree(source: &Path, destination: &Path) -> Result<()> {
        use std::os::unix::fs::symlink;

        let source_metadata = source.symlink_metadata()?;
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let from = entry.path();
            let to = destination.join(entry.file_name());
            let metadata = from.symlink_metadata()?;
            let file_type = metadata.file_type();
            if file_type.is_dir() {
                Self::copy_plain_tree(&from, &to)?;
            } else if file_type.is_symlink() {
                symlink(fs::read_link(&from)?, &to)?;
            } else if file_type.is_file() {
                fs::copy(&from, &to)?;
                fs::set_permissions(&to, fs::Permissions::from_mode(metadata.mode() & 0o7777))?;
            } else {
                return Err(std::io::Error::from_raw_os_error(libc::ENOTSUP).into());
            }
        }
        fs::set_permissions(
            destination,
            fs::Permissions::from_mode((source_metadata.mode() & 0o7777) | 0o700),
        )?;
        Ok(())
    }

    fn executable_variant() -> String {
        format!(
            "{}/{}/prepare-v{PREPARED_FILE_CACHE_VERSION}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    }

    fn loader_variant() -> String {
        format!("prepare-v{PREPARED_FILE_CACHE_VERSION}")
    }

    #[cfg(test)]
    pub(crate) fn state_for_test(&self, path: &Path) -> Result<Option<EntryState>> {
        self.with_lock(|| {
            let path = self.logical_entry_path_locked(path)?;
            self.metadata.state(&path)
        })
    }

    #[cfg(test)]
    pub(crate) fn set_state_for_test(&self, path: &Path, state: EntryState) -> Result<()> {
        let path = self.normalize(path)?;
        self.with_lock(|| self.metadata.set(&path, state))
    }

    #[cfg(test)]
    pub(crate) fn remove_state_for_test(&self, path: &Path) -> Result<()> {
        self.metadata.remove(path)
    }

    fn prepare_read_locked(&self, path: &Path) -> Result<PathBuf> {
        let destination = self.destination(path)?;
        let (state, cow_ancestor) = self.reconciled_entry_locked(path)?;
        if cow_ancestor {
            return destination
                .symlink_metadata()
                .map(|_| destination)
                .map_err(Into::into);
        }
        match state {
            Some(EntryState::Whiteout) => Self::not_found(path),
            Some(EntryState::Cow) => destination
                .symlink_metadata()
                .map(|_| destination)
                .map_err(Into::into),
            Some(EntryState::Cached { .. }) => match path.symlink_metadata() {
                Ok(_) => Ok(path.to_path_buf()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Self::remove_existing(&destination)?;
                    self.metadata.remove(path)?;
                    Err(error.into())
                }
                Err(error) => Err(error.into()),
            },
            None => path
                .symlink_metadata()
                .map(|_| path.to_path_buf())
                .map_err(Into::into),
        }
    }

    fn resolve_final_locked(&self, mut logical: PathBuf, allow_missing: bool) -> Result<PathBuf> {
        #[cfg(test)]
        self.resolution_count.fetch_add(1, Ordering::Relaxed);
        for _ in 0..40 {
            let visible = match self.prepare_read_locked(&logical) {
                Ok(visible) => visible,
                Err(error) if allow_missing && Self::is_not_found(&error) => return Ok(logical),
                Err(error) => return Err(error),
            };
            if !visible.symlink_metadata()?.file_type().is_symlink() {
                return Ok(logical);
            }
            let target = fs::read_link(&visible)?;
            let target = if target.is_absolute() {
                target
            } else {
                logical
                    .parent()
                    .context("filesystem symlink has no parent")?
                    .join(target)
            };
            logical = self.normalize(&target)?;
        }
        Err(std::io::Error::from_raw_os_error(libc::ELOOP).into())
    }

    fn visible_path_locked(&self, path: &Path) -> Result<PathBuf> {
        let destination = self.destination(path)?;
        let (state, cow_ancestor) = self.reconciled_entry_locked(path)?;
        match state {
            Some(EntryState::Whiteout) => Self::not_found(path),
            Some(EntryState::Cow) => destination
                .symlink_metadata()
                .map(|_| destination)
                .map_err(Into::into),
            Some(EntryState::Cached { .. }) => path
                .canonicalize()
                .with_context(|| format!("failed to resolve visible path {}", path.display())),
            None if cow_ancestor => destination
                .symlink_metadata()
                .map(|_| destination)
                .map_err(Into::into),
            None => {
                let canonical = path.canonicalize().with_context(|| {
                    format!("failed to resolve visible path {}", path.display())
                })?;
                let (state, cow_ancestor) = self.reconciled_entry_locked(&canonical)?;
                match state {
                    Some(EntryState::Whiteout) => Self::not_found(&canonical),
                    Some(EntryState::Cow) => {
                        let destination = self.destination(&canonical)?;
                        destination
                            .symlink_metadata()
                            .map(|_| destination)
                            .map_err(Into::into)
                    }
                    Some(EntryState::Cached { .. }) => Ok(canonical),
                    None if cow_ancestor => {
                        let destination = self.destination(&canonical)?;
                        destination
                            .symlink_metadata()
                            .map(|_| destination)
                            .map_err(Into::into)
                    }
                    None => Ok(canonical),
                }
            }
        }
    }

    fn stage_write_locked(&self, path: &Path, create: bool) -> Result<PathBuf> {
        self.detach_loader_tree_locked(path)?;
        let (state, cow_ancestor) = self.reconciled_entry_locked(path)?;
        if cow_ancestor {
            let destination = match self.metadata.encrypted_name(path)? {
                Some(_) => self.destination(path)?,
                None if self.plain_destination(path)?.is_dir() => self.plain_destination(path)?,
                None => self.file_destination(path, create)?,
            };
            if !destination.exists() && !create {
                return Self::not_found(path);
            }
            self.ensure_parent_locked(path)?;
            return Ok(destination);
        }
        match state {
            Some(EntryState::Cow) => {
                let destination = self.destination(path)?;
                if !destination.exists() && !create {
                    return Self::not_found(path);
                }
                Ok(destination)
            }
            Some(EntryState::Whiteout) if !create => Self::not_found(path),
            Some(EntryState::Whiteout) => {
                self.ensure_parent_locked(path)?;
                self.file_destination(path, true)
            }
            Some(EntryState::Cached {
                checksum,
                materializer,
                ..
            }) => match path.symlink_metadata() {
                Ok(metadata) if metadata.is_file() => {
                    let destination = self.destination(path)?;
                    let reusable = destination.exists()
                        && materializer == Materializer::Copy
                        && checksum.as_ref().is_some_and(|checksum| {
                            Self::checksum(path).is_ok_and(|current| &current == checksum)
                        });
                    if !reusable {
                        return self.materialize_file_locked(path, Materializer::Copy);
                    }
                    Ok(destination)
                }
                Ok(_) => Ok(path.to_path_buf()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                    let destination = self.destination(path)?;
                    Self::remove_existing(&destination)?;
                    self.metadata.remove(path)?;
                    self.ensure_parent_locked(path)?;
                    self.file_destination(path, true)
                }
                Err(error) => Err(error.into()),
            },
            None if path.exists() => {
                let metadata = path.metadata()?;
                if !metadata.is_file() {
                    return Ok(path.to_path_buf());
                }
                self.materialize_file_locked(path, Materializer::Copy)
            }
            None if create => {
                self.ensure_parent_locked(path)?;
                self.file_destination(path, true)
            }
            None => Self::not_found(path),
        }
    }

    fn stage_file_open_locked(
        &self,
        path: PathBuf,
        create: bool,
        exclusive: bool,
    ) -> Result<(StagedWrite, bool, Option<File>)> {
        let existed = self.visible_exists_locked(&path)?;
        if create && exclusive && existed {
            return Err(std::io::Error::from_raw_os_error(libc::EEXIST).into());
        }
        let destination = self.stage_write_locked(&path, create)?;
        let reserve = self.cipher.is_some() && create && exclusive && !existed;
        let reservation = reserve
            .then(|| {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&destination)
                    .map_err(|error| {
                        if error.kind() == std::io::ErrorKind::AlreadyExists {
                            std::io::Error::from_raw_os_error(libc::EEXIST)
                        } else {
                            error
                        }
                    })
            })
            .transpose()?;
        let lease = match self.acquire_write_lease(&destination, libc::LOCK_SH) {
            Ok(lease) => lease,
            Err(error) => {
                if reserve {
                    let _ = fs::remove_file(&destination);
                }
                return Err(error);
            }
        };
        let staged = StagedWrite {
            logical: path,
            destination,
            reservation: reservation.map(|file| WriteReservation {
                file,
                lock_path: self.lock_path.clone(),
            }),
        };
        Ok((staged, existed, lease))
    }

    fn set_attributes_locked(&self, path: &Path, attributes: FileAttributes) -> Result<()> {
        self.detach_loader_tree_locked(path)?;
        self.reconciled_state_locked(path)?;
        self.metadata.set_attributes(path, attributes)
    }

    fn create_directory_locked(&self, path: &Path, mode: u32) -> Result<PathBuf> {
        let destination = self.plain_destination(path)?;
        if self.visible_exists_locked(path)? {
            return Err(std::io::Error::from_raw_os_error(libc::EEXIST).into());
        }
        self.ensure_parent_locked(path)?;
        fs::create_dir(&destination)?;
        let result = (|| {
            Self::secure_backing_directory(&destination, mode)?;
            self.metadata.ensure_marker(path)?;
            self.metadata.set_with_attributes(
                path,
                EntryState::Cow,
                Some(FileAttributes::created_directory(mode)),
            )?;
            Ok(destination.clone())
        })();
        if result.is_err() {
            let _ = Self::remove_existing(&destination);
        }
        result
    }

    fn create_symlink_locked(&self, path: &Path, target: &Path) -> Result<PathBuf> {
        use std::os::unix::fs::symlink;

        if self.visible_exists_locked(path)? {
            return Err(std::io::Error::from_raw_os_error(libc::EEXIST).into());
        }
        let previous_state = self.reconciled_state_locked(path)?;
        let previous_attributes = self.metadata.attributes(path)?;
        self.ensure_parent_locked(path)?;
        let destination = self.file_destination(path, true)?;
        let result = (|| {
            symlink(target, &destination)?;
            let attributes = FileAttributes::from_metadata(&destination.symlink_metadata()?);
            self.metadata
                .set_with_attributes(path, EntryState::Cow, Some(attributes))?;
            Ok(destination.clone())
        })();
        if result.is_err() {
            let _ = Self::remove_existing(&destination);
            match previous_state {
                Some(state) => {
                    let _ = self
                        .metadata
                        .set_with_attributes(path, state, previous_attributes);
                }
                None => {
                    let _ = self.metadata.remove(path);
                }
            }
        }
        result
    }

    fn bind_socket_locked<T>(
        &self,
        path: &Path,
        bind: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        if self.visible_exists_locked(path)? {
            return Err(std::io::Error::from_raw_os_error(libc::EADDRINUSE).into());
        }
        let previous_state = self.reconciled_state_locked(path)?;
        let previous_attributes = self.metadata.attributes(path)?;
        self.ensure_parent_locked(path)?;
        let destination = self.file_destination(path, true)?;
        let result = (|| {
            let value = bind(&destination)?;
            let metadata = destination.symlink_metadata()?;
            if !metadata.file_type().is_socket() {
                return Err(std::io::Error::from_raw_os_error(libc::EINVAL).into());
            }
            self.metadata.set_with_attributes(
                path,
                EntryState::Cow,
                Some(FileAttributes::from_metadata(&metadata)),
            )?;
            Ok(value)
        })();
        if result.is_err() {
            let _ = Self::remove_existing(&destination);
            match previous_state {
                Some(state) => {
                    let _ = self
                        .metadata
                        .set_with_attributes(path, state, previous_attributes);
                }
                None => {
                    let _ = self.metadata.remove(path);
                }
            }
        }
        result
    }

    fn prepare_directory_locked(&self, path: &Path) -> Result<PathBuf> {
        if self.loader_tree_ancestor_locked(path, true)?.is_some() {
            return path
                .is_dir()
                .then(|| path.to_path_buf())
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT).into());
        }
        let destination = self.destination(path)?;
        let (state, cow_ancestor) = self.reconciled_entry_locked(path)?;
        if matches!(state, Some(EntryState::Whiteout)) {
            return Self::not_found(path);
        }
        let cow = cow_ancestor || matches!(state, Some(EntryState::Cow));
        if cow {
            return destination
                .is_dir()
                .then_some(destination)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT).into());
        }
        if destination.is_dir() {
            return Ok(destination);
        }
        path.is_dir()
            .then(|| path.to_path_buf())
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT).into())
    }

    fn ensure_directory_locked(&self, path: &Path) -> Result<PathBuf> {
        self.detach_loader_tree_locked(path)?;
        let destination = self.destination(path)?;
        let (state, cow_ancestor) = self.reconciled_entry_locked(path)?;
        if matches!(state, Some(EntryState::Whiteout)) {
            return Self::not_found(path);
        }
        let cow = cow_ancestor || matches!(state, Some(EntryState::Cow));
        if !cow && !path.is_dir() {
            return Self::not_found(path);
        }
        fs::create_dir_all(&destination)?;
        let mut ancestors = path
            .ancestors()
            .take_while(|ancestor| *ancestor != Path::new("/"))
            .collect::<Vec<_>>();
        ancestors.reverse();
        for ancestor in ancestors {
            let backing = self.plain_destination(ancestor)?;
            Self::secure_backing_directory(&backing, 0o700)?;
            self.metadata.ensure_marker(ancestor)?;
        }
        if !cow && path.is_dir() {
            let metadata = path.metadata()?;
            Self::secure_backing_directory(&destination, metadata.mode())?;
        } else {
            Self::secure_backing_directory(&destination, 0o700)?;
        }
        Ok(destination)
    }

    fn directory_view_locked(&self, path: &Path) -> Result<DirectoryView> {
        if self.loader_tree_ancestor_locked(path, true)?.is_some() {
            return path
                .is_dir()
                .then(|| DirectoryView::passthrough(path.to_path_buf()))
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT).into());
        }
        let (state, cow_ancestor) = self.reconciled_entry_locked(path)?;
        if matches!(state, Some(EntryState::Whiteout)) {
            return Self::not_found(path);
        }
        let upper = self.plain_destination(path)?;
        let cow = cow_ancestor || matches!(state, Some(EntryState::Cow));
        let lower = (!cow && path.is_dir()).then(|| path.to_path_buf());
        let upper_exists = upper.is_dir();
        let (primary, lower) = if upper_exists {
            (upper.clone(), lower)
        } else if let Some(lower) = lower {
            (lower, None)
        } else {
            return Self::not_found(path);
        };
        if upper_exists {
            self.reconcile_directory_locked(path)?;
        }
        let mut hidden = self
            .metadata
            .entries(path)?
            .into_iter()
            .filter_map(|(name, state)| matches!(state, EntryState::Whiteout).then_some(name))
            .collect::<BTreeSet<_>>();
        let mut aliases = HashMap::new();
        aliases.extend(
            self.metadata
                .encrypted_names(path)?
                .into_iter()
                .map(|(logical, backing)| (backing, logical)),
        );
        if upper_exists {
            for entry in fs::read_dir(&upper)? {
                let name = entry?.file_name();
                if namespace::is_control_name(&name) && !aliases.contains_key(&name) {
                    hidden.insert(name);
                    continue;
                }
                let logical = aliases
                    .get(&name)
                    .cloned()
                    .unwrap_or(namespace::decode_name(&name)?);
                if logical != name {
                    aliases.insert(name, logical);
                }
            }
        }
        let native_snapshot = if upper_exists {
            None
        } else {
            Some(NativeDirectorySnapshot {
                generation: self.metadata.current_generation()?,
                absent_upper: upper,
            })
        };
        Ok(DirectoryView {
            logical: path.to_path_buf(),
            primary,
            lower,
            hidden,
            aliases,
            native_snapshot,
        })
    }

    fn materialize_file_locked(
        &self,
        source: &Path,
        materializer: Materializer,
    ) -> Result<PathBuf> {
        self.ensure_parent_locked(source)?;
        let previous_state = self.metadata.state(source)?;
        let previous_attributes = self.metadata.attributes(source)?;
        let logical_attributes = match (previous_state.as_ref(), previous_attributes) {
            (None, attributes) => attributes,
            (Some(state), Some(attributes))
                if state.stored_attributes_are_authoritative(&attributes) =>
            {
                Some(attributes)
            }
            (Some(_), _) => None,
        };
        let destination = self.file_destination(source, true)?;
        let parent = destination
            .parent()
            .context("filesystem destination has no parent")?;
        let temporary = parent.join(format!(".agora-copy-{}.tmp", Uuid::new_v4().simple()));
        let result = (|| {
            let mut input = File::open(source)?;
            let source_metadata = input.metadata()?;
            let mut digest = Md5::new();
            if let Some(cipher) = &self.cipher {
                let mut plaintext = tempfile::tempfile()
                    .context("failed to create anonymous filesystem materialization file")?;
                let mut buffer = [0_u8; 64 * 1024];
                loop {
                    let read = input.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    digest.update(&buffer[..read]);
                    plaintext.write_all(&buffer[..read])?;
                }
                cipher.encrypt(&mut plaintext, &temporary)?;
            } else {
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)?;
                let mut buffer = [0_u8; 64 * 1024];
                loop {
                    let read = input.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    digest.update(&buffer[..read]);
                    output.write_all(&buffer[..read])?;
                }
                output.sync_all()?;
            }
            let attributes = logical_attributes
                .unwrap_or_else(|| FileAttributes::from_metadata(&source_metadata));
            if self.cipher.is_none() {
                fs::set_permissions(&temporary, fs::Permissions::from_mode(attributes.mode))?;
            }
            Self::remove_existing(&destination)?;
            fs::rename(&temporary, &destination)?;
            let plain_destination = self.plain_destination(source)?;
            if plain_destination != destination {
                Self::remove_existing(&plain_destination)?;
            }
            self.metadata.set_with_attributes(
                source,
                EntryState::Cached {
                    checksum: Some(Self::hex_digest(digest.finalize().as_slice())),
                    materializer,
                    source: Some(SourceIdentity::from_metadata(&source_metadata)),
                    variant: None,
                    destination: None,
                },
                Some(attributes),
            )?;
            Ok(destination.clone())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    fn remove_locked(&self, path: &Path, directory: bool) -> Result<()> {
        self.detach_loader_tree_locked(path)?;
        if !self.visible_exists_locked(path)? {
            return Self::not_found(path);
        }
        let destination = self.destination(path)?;
        let metadata = destination
            .symlink_metadata()
            .or_else(|_| path.symlink_metadata())?;
        if directory {
            if !metadata.is_dir() {
                return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR).into());
            }
            if !self.directory_is_empty_locked(path)? {
                return Err(std::io::Error::from_raw_os_error(libc::ENOTEMPTY).into());
            }
        } else if metadata.is_dir() {
            return Err(std::io::Error::from_raw_os_error(libc::EISDIR).into());
        }
        let lease = (!metadata.is_dir())
            .then(|| Self::write_lease_path(&destination))
            .transpose()?;
        self.metadata.set_whiteout(path, !metadata.is_dir())?;
        Self::remove_existing(&destination)?;
        if let Some(lease) = lease.as_deref() {
            Self::remove_existing(lease)?;
        }
        Ok(())
    }

    fn directory_is_empty_locked(&self, path: &Path) -> Result<bool> {
        self.reconcile_directory_locked(path)?;
        let hidden = self
            .metadata
            .entries(path)?
            .into_iter()
            .filter_map(|(name, state)| matches!(state, EntryState::Whiteout).then_some(name))
            .collect::<BTreeSet<_>>();
        let upper = self.destination(path)?;
        if upper.is_dir() {
            let aliases = self
                .metadata
                .encrypted_names(path)?
                .into_iter()
                .map(|(logical, backing)| (backing, logical))
                .collect::<HashMap<_, _>>();
            for entry in fs::read_dir(&upper)? {
                let name = entry?.file_name();
                let logical = aliases.get(&name);
                if logical.is_some_and(|logical| !hidden.contains(logical))
                    || (logical.is_none()
                        && !namespace::is_control_name(&name)
                        && !hidden.contains(&name))
                {
                    return Ok(false);
                }
            }
        }
        let (state, cow_ancestor) = self.reconciled_entry_locked(path)?;
        let cow = cow_ancestor || matches!(state, Some(EntryState::Cow));
        if !cow && path.is_dir() {
            for entry in fs::read_dir(path)? {
                if !hidden.contains(&entry?.file_name()) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn rename_locked(&self, from: &Path, to: &Path) -> Result<()> {
        self.detach_loader_tree_locked(from)?;
        self.detach_loader_tree_locked(to)?;
        if from == to {
            return self
                .visible_exists_locked(from)?
                .then_some(())
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT).into());
        }
        let from_visible = self.prepare_read_locked(from)?;
        let from_visible_metadata = from_visible.symlink_metadata()?;
        if from_visible_metadata.is_dir() && to.starts_with(from) {
            return Err(std::io::Error::from_raw_os_error(libc::EINVAL).into());
        }
        let visible_identity = BackingIdentity::from_metadata(&from_visible_metadata);
        let to_destination_before_materialization = self.destination(to)?;
        if Self::exact_path_identity(to)? == Some(visible_identity)
            || Self::exact_path_identity(&to_destination_before_materialization)?
                == Some(visible_identity)
        {
            return Ok(());
        }
        let case_only_alias = Self::is_case_only_alias(to, visible_identity)?
            || Self::is_case_only_alias(&to_destination_before_materialization, visible_identity)?;
        let mut namespace_leases = Vec::new();
        if !case_only_alias && self.visible_exists_locked(to)? {
            let to_visible = self.prepare_read_locked(to)?;
            let to_metadata = to_visible.symlink_metadata()?;
            if from_visible_metadata.is_dir() && !to_metadata.is_dir() {
                return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR).into());
            }
            if !from_visible_metadata.is_dir() && to_metadata.is_dir() {
                return Err(std::io::Error::from_raw_os_error(libc::EISDIR).into());
            }
            if to_metadata.is_dir() && !self.directory_is_empty_locked(to)? {
                return Err(std::io::Error::from_raw_os_error(libc::ENOTEMPTY).into());
            }
            if to_metadata.is_dir() {
                namespace_leases.extend(self.acquire_namespace_leases(&to_visible, true)?);
            }
        }
        if !self.is_internal(&from_visible) && from_visible_metadata.is_dir() {
            self.validate_materializable_tree_locked(from)?;
        }
        let from_destination = if self.is_internal(&from_visible) {
            from_visible
        } else if from_visible_metadata.is_dir() {
            self.materialize_tree_locked(from)?;
            self.destination(from)?
        } else if from_visible_metadata.is_file() {
            self.materialize_file_locked(from, Materializer::Copy)?
        } else if from_visible_metadata.file_type().is_symlink() {
            self.materialize_symlink_locked(from)?
        } else {
            return Err(std::io::Error::from_raw_os_error(libc::ENOTSUP).into());
        };
        if from_visible_metadata.is_dir() {
            namespace_leases.extend(self.acquire_namespace_leases(&from_destination, true)?);
        }
        self.ensure_parent_locked(to)?;
        let to_destination = if from_visible_metadata.is_dir() {
            self.plain_destination(to)?
        } else {
            self.file_destination(to, true)?
        };
        let attributes = self.metadata.attributes(from)?;
        fs::rename(&from_destination, &to_destination)?;
        if !from_visible_metadata.is_dir() {
            self.move_write_lease(&from_destination, &to_destination)?;
        }
        self.metadata.move_entry(from, to, attributes)
    }

    fn materialize_tree_locked(&self, source: &Path) -> Result<()> {
        let destination = self.ensure_directory_locked(source)?;
        let source_metadata = source.symlink_metadata()?;
        Self::secure_backing_directory(&destination, source_metadata.mode())?;
        self.metadata
            .set_attributes(source, FileAttributes::from_metadata(&source_metadata))?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let path = entry.path();
            if matches!(
                self.reconciled_state_locked(&path)?,
                Some(EntryState::Whiteout)
            ) {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                self.materialize_tree_locked(&path)?;
            } else if file_type.is_symlink() {
                if !self.destination(&path)?.exists() {
                    self.materialize_symlink_locked(&path)?;
                }
            } else if !self.destination(&path)?.exists() {
                self.materialize_file_locked(&path, Materializer::Copy)?;
            }
        }
        Ok(())
    }

    fn validate_materializable_tree_locked(&self, source: &Path) -> Result<()> {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let path = entry.path();
            if matches!(
                self.reconciled_state_locked(&path)?,
                Some(EntryState::Whiteout)
            ) {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                self.validate_materializable_tree_locked(&path)?;
            } else if !file_type.is_file() && !file_type.is_symlink() {
                return Err(std::io::Error::from_raw_os_error(libc::ENOTSUP).into());
            }
        }
        Ok(())
    }

    fn materialize_symlink_locked(&self, source: &Path) -> Result<PathBuf> {
        use std::os::unix::fs::symlink;

        self.ensure_parent_locked(source)?;
        let destination = self.destination(source)?;
        Self::remove_existing(&destination)?;
        symlink(fs::read_link(source)?, &destination)?;
        let attributes = FileAttributes::from_metadata(&source.symlink_metadata()?);
        self.metadata
            .set_with_attributes(source, EntryState::Cow, Some(attributes))?;
        Ok(destination)
    }

    fn ensure_parent_locked(&self, path: &Path) -> Result<()> {
        let parent = path.parent().context("filesystem path has no parent")?;
        if parent == Path::new("/") {
            return Ok(());
        }
        self.ensure_directory_locked(parent).map(|_| ())
    }

    fn visible_exists_locked(&self, path: &Path) -> Result<bool> {
        let (state, cow_ancestor) = self.reconciled_entry_locked(path)?;
        match state {
            Some(EntryState::Whiteout) => Ok(false),
            Some(EntryState::Cow) => Ok(self.destination(path)?.symlink_metadata().is_ok()),
            Some(EntryState::Cached { .. }) => Ok(path.symlink_metadata().is_ok()),
            None if cow_ancestor => Ok(self.destination(path)?.symlink_metadata().is_ok()),
            None => Ok(path.symlink_metadata().is_ok()),
        }
    }

    fn reconciled_state_locked(&self, path: &Path) -> Result<Option<EntryState>> {
        self.reconciled_entry_locked(path).map(|(state, _)| state)
    }

    fn reconcile_records_locked(&self, paths: &[PathBuf]) -> Result<HashSet<PathBuf>> {
        self.reconcile_root_marker_locked()?;
        let mut candidates = paths
            .iter()
            .flat_map(|path| {
                path.ancestors()
                    .take_while(|ancestor| *ancestor != Path::new("/"))
                    .map(Path::to_path_buf)
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            left.components()
                .count()
                .cmp(&right.components().count())
                .then_with(|| left.cmp(right))
        });
        let mut active_directories = HashSet::from([PathBuf::from("/")]);
        let mut stored_paths = HashSet::new();
        let mut whiteouts = HashSet::new();
        let mut hidden = HashSet::new();
        for path in candidates {
            let parent = path.parent().unwrap_or(Path::new("/"));
            if hidden.contains(parent) || whiteouts.contains(parent) {
                hidden.insert(path);
                continue;
            }
            if !active_directories.contains(parent) {
                continue;
            }
            stored_paths.insert(path.clone());
            let state = self.reconcile_entry_locked(&path)?;
            if matches!(state, Some(EntryState::Whiteout)) {
                whiteouts.insert(path);
                continue;
            }
            if matches!(
                state,
                Some(EntryState::Cached {
                    materializer: Materializer::LoaderTree,
                    ..
                })
            ) {
                continue;
            }
            let destination = self.plain_destination(&path)?;
            match destination.symlink_metadata() {
                Ok(metadata) if metadata.is_dir() && self.metadata.has_marker(&path)? => {
                    active_directories.insert(path);
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.metadata.has_marker(&path)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(path) = paths.iter().find(|path| hidden.contains(path.as_path())) {
            return Self::not_found(path);
        }
        Ok(stored_paths)
    }

    fn reconciled_entry_locked(&self, path: &Path) -> Result<(Option<EntryState>, bool)> {
        if self.loader_tree_ancestor_locked(path, false)?.is_some() {
            return Ok((None, false));
        }
        let cow_ancestor = self.reconcile_ancestors_locked(path)?;
        Ok((self.reconcile_entry_locked(path)?, cow_ancestor))
    }

    fn loader_tree_ancestor_locked(
        &self,
        path: &Path,
        include_self: bool,
    ) -> Result<Option<PathBuf>> {
        let mut ancestors = path
            .ancestors()
            .skip(usize::from(!include_self))
            .take_while(|ancestor| *ancestor != Path::new("/"))
            .collect::<Vec<_>>();
        ancestors.reverse();
        for ancestor in ancestors {
            if matches!(
                self.metadata.state(ancestor)?,
                Some(EntryState::Cached {
                    materializer: Materializer::LoaderTree,
                    ..
                })
            ) && matches!(
                self.reconcile_entry_locked(ancestor)?,
                Some(EntryState::Cached {
                    materializer: Materializer::LoaderTree,
                    ..
                })
            ) {
                return Ok(Some(ancestor.to_path_buf()));
            }
        }
        Ok(None)
    }

    fn detach_loader_tree_locked(&self, path: &Path) -> Result<()> {
        let Some(root) = self.loader_tree_ancestor_locked(path, true)? else {
            return Ok(());
        };
        Self::remove_existing(&self.plain_destination(&root)?)?;
        self.metadata.remove(&root)
    }

    fn reconcile_ancestors_locked(&self, path: &Path) -> Result<bool> {
        self.reconcile_root_marker_locked()?;
        let mut ancestors = path
            .ancestors()
            .skip(1)
            .take_while(|ancestor| *ancestor != Path::new("/"))
            .collect::<Vec<_>>();
        ancestors.reverse();
        let mut cow_ancestor = false;
        for ancestor in ancestors {
            match self.reconcile_entry_locked(ancestor)? {
                Some(EntryState::Whiteout) => return Self::not_found(path),
                Some(EntryState::Cow) => cow_ancestor = true,
                Some(EntryState::Cached { .. }) | None => {}
            }
            match self.plain_destination(ancestor)?.symlink_metadata() {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.metadata.has_marker(ancestor)?;
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(cow_ancestor)
    }

    fn reconcile_root_marker_locked(&self) -> Result<()> {
        if self.metadata.has_marker(Path::new("/"))? {
            return Ok(());
        }
        Self::discard_untrusted_root_entries(&self.root)?;
        self.metadata.ensure_marker(Path::new("/"))
    }

    fn discard_untrusted_root_entries(root: &Path) -> Result<()> {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let name = entry.file_name();
            if matches!(
                name.as_bytes(),
                value if value == namespace::METADATA_FILE.as_bytes()
                    || value == namespace::VFS_LOCK_FILE.as_bytes()
                    || value == namespace::FILESYSTEM_LOCK_FILE.as_bytes()
                    || value == namespace::KEY_FILE.as_bytes()
                    || value == namespace::REKEY_JOURNAL_FILE.as_bytes()
            ) {
                continue;
            }
            Self::remove_existing(&entry.path())?;
        }
        Ok(())
    }

    fn reconcile_entry_locked(&self, path: &Path) -> Result<Option<EntryState>> {
        #[cfg(test)]
        self.reconciliation_count.fetch_add(1, Ordering::Relaxed);
        if path == Path::new("/") {
            return Ok(None);
        }
        let state = self.metadata.state(path)?;
        let destination = self.destination(path)?;
        let upper = match destination.symlink_metadata() {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        match state {
            Some(EntryState::Cow) => match upper {
                Some(metadata) if metadata.is_dir() && !self.metadata.has_marker(path)? => {
                    self.discard_upper_entry_locked(path, &destination, true)?;
                    Ok(None)
                }
                Some(_) => Ok(Some(EntryState::Cow)),
                None => {
                    self.discard_upper_entry_locked(path, &destination, true)?;
                    Ok(None)
                }
            },
            Some(state @ EntryState::Cached { .. }) => {
                if upper.is_some() {
                    Ok(Some(state))
                } else {
                    self.discard_upper_entry_locked(path, &destination, true)?;
                    Ok(None)
                }
            }
            Some(EntryState::Whiteout) => {
                if upper.is_some() {
                    self.discard_upper_entry_locked(path, &destination, false)?;
                }
                Ok(Some(EntryState::Whiteout))
            }
            None => match upper {
                Some(metadata) if metadata.is_dir() && self.metadata.has_marker(path)? => Ok(None),
                Some(_) if self.write_lease_is_active(&destination)? => Ok(None),
                Some(_) => {
                    self.discard_upper_entry_locked(path, &destination, false)?;
                    Ok(None)
                }
                None => Ok(None),
            },
        }
    }

    fn write_lease_is_active(&self, destination: &Path) -> Result<bool> {
        let path = Self::write_lease_path(destination)?;
        let lease = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(lease) => lease,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        match Self::flock(&lease, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => {
                Self::flock(&lease, libc::LOCK_UN)?;
                Ok(false)
            }
            Err(error) if error.raw_os_error() == Some(libc::EWOULDBLOCK) => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    fn reconcile_directory_locked(&self, path: &Path) -> Result<()> {
        let upper = self.plain_destination(path)?;
        if !upper.is_dir() {
            return Ok(());
        }
        if !self.metadata.has_marker(path)? {
            self.discard_upper_entry_locked(path, &upper, self.metadata.state(path)?.is_some())?;
            return Ok(());
        }
        let aliases = self
            .metadata
            .encrypted_names(path)?
            .into_iter()
            .map(|(logical, backing)| (backing, logical))
            .collect::<HashMap<_, _>>();
        for entry in fs::read_dir(&upper)? {
            let entry = entry?;
            let name = entry.file_name();
            if let Some(logical) = aliases.get(&name) {
                self.reconcile_entry_locked(&path.join(logical))?;
                continue;
            }
            let encrypted_or_legacy = name
                .as_bytes()
                .starts_with(super::crypto::ENCRYPTED_NAME_PREFIX.as_bytes())
                || namespace::is_file_backing_name(name.as_bytes());
            if encrypted_or_legacy {
                self.discard_upper_entry_locked(&path.join(&name), &entry.path(), false)?;
                continue;
            }
            if namespace::is_control_name(&name) {
                continue;
            }
            let logical = namespace::decode_name(&name)?;
            self.reconcile_entry_locked(&path.join(logical))?;
        }
        Ok(())
    }

    fn discard_upper_entry_locked(
        &self,
        path: &Path,
        destination: &Path,
        clear_metadata: bool,
    ) -> Result<()> {
        if self.cipher.is_some()
            && let Ok(lease) = Self::write_lease_path(destination)
        {
            Self::remove_existing(&lease)?;
        }
        Self::remove_existing(destination)?;
        if clear_metadata {
            self.metadata.remove(path)
        } else {
            self.metadata.invalidate()
        }
    }

    fn destination(&self, path: &Path) -> Result<PathBuf> {
        if self.cipher.is_none() || path == Path::new("/") {
            return self.plain_destination(path);
        }
        match self.metadata.encrypted_name(path)? {
            Some(name) => {
                let parent = path.parent().context("filesystem path has no parent")?;
                Ok(namespace::backing_path(&self.root, parent)?.join(name))
            }
            None => self.plain_destination(path),
        }
    }

    fn file_destination(&self, path: &Path, create: bool) -> Result<PathBuf> {
        if self.cipher.is_none() {
            return self.plain_destination(path);
        }
        let parent = path.parent().context("filesystem path has no parent")?;
        let name = if create {
            self.metadata.ensure_encrypted_name(path)?
        } else {
            self.metadata
                .encrypted_name(path)?
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?
        };
        Ok(namespace::backing_path(&self.root, parent)?.join(name))
    }

    fn plain_destination(&self, path: &Path) -> Result<PathBuf> {
        namespace::backing_path(&self.root, path)
    }

    fn move_write_lease(&self, from: &Path, to: &Path) -> Result<()> {
        if self.cipher.is_none() {
            return Ok(());
        }
        let from_lease = Self::write_lease_path(from)?;
        let to_lease = Self::write_lease_path(to)?;
        let lease = match OpenOptions::new().read(true).write(true).open(&from_lease) {
            Ok(lease) => lease,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::remove_existing(&to_lease);
            }
            Err(error) => return Err(error.into()),
        };
        Self::write_write_lease_destination(&lease, to)?;
        drop(lease);
        fs::rename(&from_lease, &to_lease)?;
        Ok(())
    }

    fn path_identity(path: &Path) -> Result<Option<BackingIdentity>> {
        match path.symlink_metadata() {
            Ok(metadata) => Ok(Some(BackingIdentity::from_metadata(&metadata))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn exact_path_identity(path: &Path) -> Result<Option<BackingIdentity>> {
        let parent = path.parent().context("filesystem path has no parent")?;
        let name = path
            .file_name()
            .context("filesystem path has no file name")?;
        let entries = match fs::read_dir(parent) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_name().as_bytes() == name.as_bytes() {
                return Self::path_identity(&entry.path());
            }
        }
        Ok(None)
    }

    fn is_case_only_alias(path: &Path, identity: BackingIdentity) -> Result<bool> {
        Ok(Self::path_identity(path)? == Some(identity)
            && Self::exact_path_identity(path)?.is_none())
    }

    fn normalize(&self, path: &Path) -> Result<PathBuf> {
        normalize_path(path)
    }

    fn remove_existing(path: &Path) -> Result<()> {
        match path.symlink_metadata() {
            Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)?,
            Ok(_) => fs::remove_file(path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn secure_backing_directory(path: &Path, logical_mode: u32) -> Result<()> {
        fs::set_permissions(
            path,
            fs::Permissions::from_mode((logical_mode & 0o7777) | 0o700),
        )?;
        Ok(())
    }

    fn write_lease_path(destination: &Path) -> Result<PathBuf> {
        let name = destination
            .file_name()
            .context("filesystem destination has no file name")?;
        let mut lease = namespace::WRITE_LEASE_PREFIX.to_vec();
        lease.extend_from_slice(name.as_bytes());
        Ok(destination.with_file_name(OsString::from_vec(lease)))
    }

    fn read_write_lease_destination(lease: &File) -> Result<Option<PathBuf>> {
        let length = lease.metadata()?.len();
        if length > super::MAX_CONTROL_PATH_BYTES as u64 {
            anyhow::bail!(
                "filesystem write lease path exceeds {} bytes",
                super::MAX_CONTROL_PATH_BYTES
            );
        }
        let length = usize::try_from(length).context("filesystem write lease path is too long")?;
        if length == 0 {
            return Ok(None);
        }
        let mut contents = vec![0_u8; length];
        let mut offset = 0;
        while offset < contents.len() {
            let read = lease.read_at(&mut contents[offset..], offset as u64)?;
            if read == 0 {
                return Err(std::io::Error::from_raw_os_error(libc::EIO).into());
            }
            offset += read;
        }
        let mut excess = [0_u8; 1];
        if lease.read_at(&mut excess, length as u64)? != 0 {
            anyhow::bail!(
                "filesystem write lease path exceeds {} bytes",
                super::MAX_CONTROL_PATH_BYTES
            );
        }
        Ok(Some(PathBuf::from(OsString::from_vec(contents))))
    }

    fn write_write_lease_destination(lease: &File, destination: &Path) -> Result<()> {
        let contents = destination.as_os_str().as_bytes();
        if contents.len() > super::MAX_CONTROL_PATH_BYTES {
            anyhow::bail!(
                "filesystem write lease path exceeds {} bytes",
                super::MAX_CONTROL_PATH_BYTES
            );
        }
        lease.set_len(0)?;
        let mut offset = 0;
        while offset < contents.len() {
            let written = lease.write_at(&contents[offset..], offset as u64)?;
            if written == 0 {
                return Err(std::io::Error::from_raw_os_error(libc::EIO).into());
            }
            offset += written;
        }
        lease.set_len(contents.len() as u64)?;
        Ok(())
    }

    fn acquire_write_lease(
        &self,
        destination: &Path,
        operation: libc::c_int,
    ) -> Result<Option<File>> {
        if !self.is_internal(destination) {
            return Ok(None);
        }
        let path = Self::write_lease_path(destination)?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("failed to open write lease {}", path.display()))?;
        if let Err(error) = Self::flock(&lease, operation) {
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(std::io::Error::from_raw_os_error(libc::EBUSY).into());
            }
            return Err(error)
                .with_context(|| format!("failed to acquire write lease {}", path.display()));
        }
        if Self::read_write_lease_destination(&lease)?.as_deref() != Some(destination) {
            Self::write_write_lease_destination(&lease, destination)?;
        }
        Ok(Some(lease))
    }

    fn acquire_namespace_leases(&self, destination: &Path, directory: bool) -> Result<Vec<File>> {
        if self.cipher.is_none() || !self.is_internal(destination) {
            return Ok(Vec::new());
        }
        if !directory {
            return self
                .acquire_write_lease(destination, libc::LOCK_EX | libc::LOCK_NB)
                .map(|lease| lease.into_iter().collect());
        }
        let mut pending = vec![destination.to_path_buf()];
        let mut leases = Vec::new();
        while let Some(current) = pending.pop() {
            for entry in fs::read_dir(current)? {
                let entry = entry?;
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    pending.push(entry.path());
                    continue;
                }
                if !entry
                    .file_name()
                    .as_bytes()
                    .starts_with(namespace::WRITE_LEASE_PREFIX)
                {
                    continue;
                }
                let lease = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(entry.path())?;
                if let Err(error) = Self::flock(&lease, libc::LOCK_EX | libc::LOCK_NB) {
                    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                        return Err(std::io::Error::from_raw_os_error(libc::EBUSY).into());
                    }
                    return Err(error.into());
                }
                leases.push(lease);
            }
        }
        Ok(leases)
    }

    fn with_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        #[cfg(test)]
        self.transaction_count.fetch_add(1, Ordering::Relaxed);
        let lock = self.take_lock_descriptor()?;
        if let Err(error) = Self::flock(&lock, libc::LOCK_EX) {
            self.return_lock_descriptor(lock);
            return Err(error.into());
        }
        let result = operation();
        let unlock = Self::flock(&lock, libc::LOCK_UN);
        if unlock.is_ok() {
            self.return_lock_descriptor(lock);
        }
        match result {
            Ok(value) => {
                unlock?;
                Ok(value)
            }
            Err(error) => {
                let _ = unlock;
                Err(error)
            }
        }
    }

    fn take_lock_descriptor(&self) -> Result<File> {
        let pid = unsafe { libc::getpid() };
        let mut pool = self
            .lock_pool
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if pool.pid != pid {
            pool.pid = pid;
            pool.files.clear();
        }
        if let Some(lock) = pool.files.pop() {
            return Ok(lock);
        }
        drop(pool);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .with_context(|| format!("failed to open overlay lock {}", self.lock_path.display()))?;
        #[cfg(test)]
        self.lock_open_count.fetch_add(1, Ordering::Relaxed);
        Ok(lock)
    }

    fn return_lock_descriptor(&self, lock: File) {
        let pid = unsafe { libc::getpid() };
        let mut pool = self
            .lock_pool
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if pool.pid != pid {
            pool.pid = pid;
            pool.files.clear();
        }
        if pool.files.len() < LOCK_DESCRIPTOR_POOL_CAPACITY {
            pool.files.push(lock);
        }
    }

    fn flock(file: &File, operation: libc::c_int) -> std::io::Result<()> {
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn hex_digest(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            output.push(HEX[(byte >> 4) as usize] as char);
            output.push(HEX[(byte & 0x0f) as usize] as char);
        }
        output
    }

    fn not_found<T>(path: &Path) -> Result<T> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("filesystem path is not visible: {}", path.display()),
        )
        .into())
    }

    fn is_not_found(error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    }
}

#[cfg(test)]
mod tests;
