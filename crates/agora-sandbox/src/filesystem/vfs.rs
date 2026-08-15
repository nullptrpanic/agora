use super::overlay::OverlayTransaction;
use super::{
    AccessRequest, Credentials, DirectoryView, EntryState, FileCipher, OverlayStore, StagedWrite,
};
use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub(crate) struct VirtualFilesystem {
    overlay: OverlayStore,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OpenIntent {
    flags: libc::c_int,
    mode: u32,
}

pub(crate) struct OpenPlan {
    logical: PathBuf,
    prepared: PreparedFile,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum AccessPlan {
    Allowed,
    Native(PathBuf),
}

pub(crate) struct MetadataPlan {
    logical: PathBuf,
    mapped: PathBuf,
    plaintext_size: Option<u64>,
    attributes: Option<super::FileAttributes>,
}

enum OpenMapping {
    Directory(PathBuf),
    File {
        mapped: PathBuf,
        staged: Option<StagedWrite>,
        existed: bool,
        lease: Option<File>,
    },
}

impl OpenIntent {
    pub(crate) fn new(flags: libc::c_int, mode: u32) -> Result<Self> {
        VirtualFilesystem::validate_open_flags(flags)?;
        Ok(Self { flags, mode })
    }

    pub(crate) fn flags(self) -> libc::c_int {
        self.flags
    }

    pub(crate) fn mode(self) -> u32 {
        self.mode
    }

    pub(crate) fn access(self) -> AccessRequest {
        AccessRequest::from_open_flags(self.flags)
    }
}

impl OpenPlan {
    pub(crate) fn logical(&self) -> &Path {
        &self.logical
    }

    pub(crate) fn into_parts(self) -> (PathBuf, PreparedFile) {
        (self.logical, self.prepared)
    }
}

impl MetadataPlan {
    #[cfg(test)]
    pub(crate) fn logical(&self) -> &Path {
        &self.logical
    }

    #[cfg(test)]
    pub(crate) fn mapped(&self) -> &Path {
        &self.mapped
    }

    pub(crate) fn into_parts(
        self,
    ) -> (PathBuf, PathBuf, Option<u64>, Option<super::FileAttributes>) {
        (
            self.logical,
            self.mapped,
            self.plaintext_size,
            self.attributes,
        )
    }
}

pub(crate) enum OpenTarget {
    Path(PathBuf),
    Descriptor(File),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileLayer {
    Lower,
    Upper,
}

pub(crate) struct PreparedFile {
    target: OpenTarget,
    staged: Option<StagedWrite>,
    staging_lease: Option<File>,
    writeback: Option<Writeback>,
    publish_on_open: bool,
    overwrite_on_open: bool,
    created_mode: Option<u32>,
    layer: FileLayer,
    encrypted_backing: Option<PathBuf>,
    broker_flags: Option<libc::c_int>,
}

pub(crate) struct Writeback {
    plaintext: Mutex<File>,
    lease: Mutex<File>,
    baseline: Mutex<PlaintextIdentity>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PlaintextIdentity {
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

impl PlaintextIdentity {
    fn from_file(file: &File) -> Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            len: metadata.len(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        })
    }
}

impl PreparedFile {
    fn for_path(target: PathBuf, staged: Option<StagedWrite>, layer: FileLayer) -> Self {
        Self {
            target: OpenTarget::Path(target),
            staged,
            staging_lease: None,
            writeback: None,
            publish_on_open: false,
            overwrite_on_open: false,
            created_mode: None,
            layer,
            encrypted_backing: None,
            broker_flags: None,
        }
    }

    pub(crate) fn target(&self) -> &OpenTarget {
        &self.target
    }

    pub(crate) fn target_mut(&mut self) -> &mut OpenTarget {
        &mut self.target
    }

    pub(crate) fn into_parts(self) -> (OpenTarget, Option<Writeback>, FileLayer) {
        (self.target, self.writeback, self.layer)
    }

    pub(crate) fn local_broker_request(&self) -> Option<(PathBuf, libc::c_int)> {
        let Some(backing) = &self.encrypted_backing else {
            return None;
        };
        Some((backing.clone(), self.broker_flags?))
    }

    pub(crate) fn encrypted_backing_identity(&self) -> Result<Option<(u64, u64)>> {
        self.encrypted_backing
            .as_ref()
            .map(|path| {
                let metadata = path.metadata()?;
                Ok((metadata.dev(), metadata.ino()))
            })
            .transpose()
    }
}

impl VirtualFilesystem {
    pub(crate) fn plain(root: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            overlay: OverlayStore::new(root)?,
        })
    }

    pub(crate) fn encrypted(root: impl Into<PathBuf>, cipher: FileCipher) -> Result<Self> {
        Ok(Self {
            overlay: OverlayStore::encrypted(root, cipher)?,
        })
    }

    pub(crate) fn prepare_authorized_open(
        &self,
        requested: &Path,
        intent: OpenIntent,
        credentials: &Credentials,
    ) -> Result<OpenPlan> {
        self.prepare_authorized_open_with_materialization(requested, intent, credentials, true)
    }

    pub(crate) fn prepare_authorized_broker_open(
        &self,
        requested: &Path,
        intent: OpenIntent,
        credentials: &Credentials,
    ) -> Result<OpenPlan> {
        self.prepare_authorized_open_with_materialization(requested, intent, credentials, false)
    }

    fn prepare_authorized_open_with_materialization(
        &self,
        requested: &Path,
        intent: OpenIntent,
        credentials: &Credentials,
        materialize_encrypted_content: bool,
    ) -> Result<OpenPlan> {
        let (logical, mapping) = self.overlay.transaction(|transaction| {
            if Self::native_read_eligible(intent.flags)
                && transaction.native_metadata_passthrough(requested, true, |attributes| {
                    credentials.allows(attributes, AccessRequest::EXECUTE)
                })?
            {
                return Ok((
                    requested.to_path_buf(),
                    OpenMapping::File {
                        mapped: requested.to_path_buf(),
                        staged: None,
                        existed: true,
                        lease: None,
                    },
                ));
            }
            let endpoint_is_resolved = intent.flags & libc::O_NOFOLLOW == 0
                && intent.flags & (libc::O_CREAT | libc::O_EXCL) != (libc::O_CREAT | libc::O_EXCL);
            let (logical, parent_attributes) = if endpoint_is_resolved {
                Self::resolve_final_with_search_in(
                    transaction,
                    requested,
                    intent.flags & libc::O_CREAT != 0,
                    credentials,
                )?
            } else {
                (
                    requested.to_path_buf(),
                    Self::require_search_in(transaction, requested, credentials)?,
                )
            };
            if transaction.visible_exists(&logical)? {
                if intent.flags & (libc::O_CREAT | libc::O_EXCL) == (libc::O_CREAT | libc::O_EXCL) {
                    return Err(std::io::Error::from_raw_os_error(libc::EEXIST).into());
                }
                let final_is_symlink = transaction
                    .prepare_read(&logical)?
                    .symlink_metadata()?
                    .file_type()
                    .is_symlink();
                if intent.flags & libc::O_NOFOLLOW == 0 || !final_is_symlink {
                    if endpoint_is_resolved {
                        Self::require_resolved_entry_access_in(
                            transaction,
                            &logical,
                            intent.access(),
                            credentials,
                        )?;
                    } else {
                        Self::require_entry_access_in(
                            transaction,
                            &logical,
                            intent.access(),
                            credentials,
                        )?;
                    }
                }
            } else if intent.flags & libc::O_CREAT != 0 {
                Self::require_parent_access(&logical, parent_attributes.as_ref(), credentials)?;
            }
            let mapping = Self::stage_open_in(transaction, &logical, intent)?;
            Ok((logical, mapping))
        })?;
        let prepared =
            self.prepare_mapped_open(&logical, intent, mapping, materialize_encrypted_content)?;
        Ok(OpenPlan { logical, prepared })
    }

    fn native_read_eligible(flags: libc::c_int) -> bool {
        let mutating_or_special = libc::O_CREAT
            | libc::O_TRUNC
            | libc::O_EXCL
            | libc::O_DIRECTORY
            | libc::O_NOFOLLOW
            | libc::O_NOFOLLOW_ANY
            | libc::O_SYMLINK;
        flags & libc::O_ACCMODE == libc::O_RDONLY && flags & mutating_or_special == 0
    }

    fn stage_open_in(
        transaction: &OverlayTransaction<'_>,
        logical: &Path,
        intent: OpenIntent,
    ) -> Result<OpenMapping> {
        if intent.flags & libc::O_DIRECTORY != 0 {
            return transaction
                .prepare_directory(logical)
                .map(OpenMapping::Directory);
        }
        let writes = intent.flags & libc::O_ACCMODE != libc::O_RDONLY
            || intent.flags & (libc::O_CREAT | libc::O_TRUNC) != 0;
        if writes {
            let (staged, existed, lease) = transaction.stage_file_open(
                logical,
                intent.flags & libc::O_CREAT != 0,
                intent.flags & libc::O_EXCL != 0,
            )?;
            return Ok(OpenMapping::File {
                mapped: staged.destination().to_path_buf(),
                staged: Some(staged),
                existed,
                lease,
            });
        }
        Ok(OpenMapping::File {
            mapped: transaction.prepare_read(logical)?,
            staged: None,
            existed: true,
            lease: None,
        })
    }

    fn entry_attributes_in(
        transaction: &OverlayTransaction<'_>,
        path: &Path,
    ) -> Result<super::FileAttributes> {
        if let Some(attributes) = transaction.attributes(path)? {
            return Ok(attributes);
        }
        let mapped = transaction.prepare_read(path)?;
        Ok(super::FileAttributes::from_metadata(
            &mapped.symlink_metadata()?,
        ))
    }

    fn require_entry_access_in(
        transaction: &OverlayTransaction<'_>,
        path: &Path,
        request: AccessRequest,
        credentials: &Credentials,
    ) -> Result<()> {
        let logical = transaction.resolve_final(path, false)?;
        Self::require_resolved_entry_access_in(transaction, &logical, request, credentials)
    }

    fn require_resolved_entry_access_in(
        transaction: &OverlayTransaction<'_>,
        logical: &Path,
        request: AccessRequest,
        credentials: &Credentials,
    ) -> Result<()> {
        let attributes = Self::entry_attributes_in(transaction, logical)?;
        Self::require_attributes_access(&attributes, request, credentials)
    }

    fn require_attributes_access(
        attributes: &super::FileAttributes,
        request: AccessRequest,
        credentials: &Credentials,
    ) -> Result<()> {
        if credentials.allows(attributes, request) {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(libc::EACCES).into())
        }
    }

    fn require_search_in(
        transaction: &OverlayTransaction<'_>,
        path: &Path,
        credentials: &Credentials,
    ) -> Result<Option<super::FileAttributes>> {
        let Some(parent) = path.parent() else {
            return Ok(None);
        };
        let ancestors = parent.ancestors().collect::<Vec<_>>();
        let resolved = ancestors
            .iter()
            .rev()
            .map(|ancestor| transaction.resolve_final(ancestor, false))
            .collect::<Result<Vec<_>>>()?;
        let records =
            transaction.records(&resolved.iter().map(PathBuf::as_path).collect::<Vec<_>>())?;
        let mut parent_attributes = None;
        for (logical, (state, attributes)) in resolved.into_iter().zip(records) {
            let attributes = match (state, attributes) {
                (Some(EntryState::Cow) | None, Some(attributes)) => attributes,
                _ => {
                    let mapped = transaction.prepare_read(&logical)?;
                    super::FileAttributes::from_metadata(&mapped.metadata()?)
                }
            };
            if !credentials.allows(&attributes, AccessRequest::EXECUTE) {
                return Err(std::io::Error::from_raw_os_error(libc::EACCES).into());
            }
            parent_attributes = Some(attributes);
        }
        Ok(parent_attributes)
    }

    fn resolve_final_with_search_in(
        transaction: &OverlayTransaction<'_>,
        path: &Path,
        allow_missing: bool,
        credentials: &Credentials,
    ) -> Result<(PathBuf, Option<super::FileAttributes>)> {
        let mut parent_attributes = Self::require_search_in(transaction, path, credentials)?;
        let logical = transaction.resolve_final(path, allow_missing)?;
        if logical != path {
            parent_attributes = Self::require_search_in(transaction, &logical, credentials)?;
        }
        Ok((logical, parent_attributes))
    }

    fn resolve_access_path_in(
        transaction: &OverlayTransaction<'_>,
        path: &Path,
        follow_final: bool,
        credentials: &Credentials,
    ) -> Result<PathBuf> {
        if follow_final {
            return Ok(
                Self::resolve_final_with_search_in(transaction, path, false, credentials)?.0,
            );
        }
        Self::require_search_in(transaction, path, credentials)?;
        Ok(path.to_path_buf())
    }

    fn require_parent_mutation_in(
        transaction: &OverlayTransaction<'_>,
        path: &Path,
        credentials: &Credentials,
    ) -> Result<()> {
        let parent_attributes = Self::require_search_in(transaction, path, credentials)?;
        Self::require_parent_access(path, parent_attributes.as_ref(), credentials)
    }

    fn require_parent_access(
        path: &Path,
        parent_attributes: Option<&super::FileAttributes>,
        credentials: &Credentials,
    ) -> Result<()> {
        path.parent()
            .context("filesystem mutation path has no parent")?;
        let parent_attributes =
            parent_attributes.context("filesystem mutation parent attributes are unavailable")?;
        Self::require_attributes_access(
            parent_attributes,
            AccessRequest::WRITE_EXECUTE,
            credentials,
        )
    }

    #[cfg(test)]
    pub(crate) fn transaction_count_for_test(&self) -> usize {
        self.overlay.transaction_count_for_test()
    }

    #[cfg(test)]
    fn resolution_count_for_test(&self) -> usize {
        self.overlay.resolution_count_for_test()
    }

    #[cfg(test)]
    pub(crate) fn prepare_open(
        &self,
        logical: &Path,
        flags: libc::c_int,
        mode: u32,
    ) -> Result<PreparedFile> {
        let intent = OpenIntent::new(flags, mode)?;
        let mapping = self
            .overlay
            .transaction(|transaction| Self::stage_open_in(transaction, logical, intent))?;
        self.prepare_mapped_open(logical, intent, mapping, true)
    }

    fn prepare_mapped_open(
        &self,
        logical: &Path,
        intent: OpenIntent,
        mapping: OpenMapping,
        materialize_encrypted_content: bool,
    ) -> Result<PreparedFile> {
        let flags = intent.flags;
        let mode = intent.mode;
        if let OpenMapping::Directory(target) = mapping {
            let layer = if self.overlay.is_internal(&target) {
                FileLayer::Upper
            } else {
                FileLayer::Lower
            };
            return Ok(PreparedFile::for_path(target, None, layer));
        }
        let OpenMapping::File {
            mapped,
            staged,
            existed,
            lease,
        } = mapping
        else {
            unreachable!("directory mapping returned above")
        };
        let writes = flags & libc::O_ACCMODE != libc::O_RDONLY
            || flags & (libc::O_CREAT | libc::O_TRUNC) != 0;
        let create = flags & libc::O_CREAT != 0;
        if !self.overlay.is_internal(&mapped) {
            return Ok(PreparedFile::for_path(mapped, None, FileLayer::Lower));
        }
        let Some(cipher) = self.overlay.cipher().cloned() else {
            let mut prepared = PreparedFile::for_path(mapped, staged, FileLayer::Upper);
            prepared.staging_lease = lease;
            return Ok(prepared);
        };
        if mapped.exists() && !mapped.symlink_metadata()?.is_file() {
            return Ok(PreparedFile::for_path(mapped, staged, FileLayer::Upper));
        }

        let created_mode = (create && !existed)
            .then(|| Self::effective_creation_mode(mode))
            .transpose()?;
        if !materialize_encrypted_content {
            if !existed && !create {
                bail!("filesystem path is not visible: {}", logical.display());
            }
            let plaintext = tempfile::tempfile()?;
            let target = plaintext.try_clone()?;
            let baseline = PlaintextIdentity::from_file(&plaintext)?;
            return Ok(PreparedFile {
                target: OpenTarget::Descriptor(target),
                staged,
                staging_lease: None,
                writeback: if writes {
                    Some(Writeback {
                        plaintext: Mutex::new(plaintext),
                        lease: Mutex::new(
                            lease.context("encrypted write open did not acquire a lease")?,
                        ),
                        baseline: Mutex::new(baseline),
                    })
                } else {
                    None
                },
                publish_on_open: writes && (!existed || flags & libc::O_TRUNC != 0),
                overwrite_on_open: writes && existed && flags & libc::O_TRUNC != 0,
                created_mode,
                layer: FileLayer::Upper,
                encrypted_backing: Some(mapped),
                broker_flags: Some(flags),
            });
        }
        let plaintext = tempfile::NamedTempFile::new()?;
        let access = flags & libc::O_ACCMODE;
        let mut exposed = OpenOptions::new();
        exposed
            .read(access != libc::O_WRONLY)
            .write(access != libc::O_RDONLY)
            .append(flags & libc::O_APPEND != 0)
            .custom_flags(libc::O_CLOEXEC);
        let exposed = exposed.open(plaintext.path())?;
        let mut plaintext = plaintext.into_file();
        if existed && mapped.is_file() {
            cipher.decrypt(&mapped, &mut plaintext)?;
        } else if !create {
            bail!("filesystem path is not visible: {}", logical.display());
        }
        if flags & libc::O_TRUNC != 0 {
            plaintext.set_len(0)?;
        }
        if flags & libc::O_APPEND != 0 {
            plaintext.seek(SeekFrom::End(0))?;
        } else {
            plaintext.seek(SeekFrom::Start(0))?;
        }
        let baseline = PlaintextIdentity::from_file(&plaintext)?;
        Ok(PreparedFile {
            target: OpenTarget::Descriptor(exposed),
            staged,
            staging_lease: None,
            writeback: if writes {
                Some(Writeback {
                    plaintext: Mutex::new(plaintext),
                    lease: Mutex::new(
                        lease.context("encrypted write open did not acquire a lease")?,
                    ),
                    baseline: Mutex::new(baseline),
                })
            } else {
                None
            },
            publish_on_open: writes && (!existed || flags & libc::O_TRUNC != 0),
            overwrite_on_open: writes && existed && flags & libc::O_TRUNC != 0,
            created_mode,
            layer: FileLayer::Upper,
            encrypted_backing: Some(mapped),
            broker_flags: None,
        })
    }

    pub(crate) fn prepare_native_open(&self, logical: &Path) -> PreparedFile {
        PreparedFile::for_path(logical.to_path_buf(), None, FileLayer::Lower)
    }

    pub(crate) fn commit_open(&self, prepared: &mut PreparedFile) -> Result<()> {
        if let Some(writeback) = &prepared.writeback {
            if prepared.overwrite_on_open {
                let mut plaintext = writeback
                    .plaintext
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let lease = writeback
                    .lease
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                self.overlay.overwrite_encrypted(&mut plaintext, &lease)?;
            } else {
                self.publish_writeback(writeback, prepared.publish_on_open)?;
            }
            prepared.publish_on_open = false;
            prepared.overwrite_on_open = false;
        }
        if let Some(staged) = prepared.staged.take() {
            if let Some(mode) = prepared.created_mode.take() {
                self.overlay.commit_created_file(staged, mode)?;
            } else {
                self.overlay.commit_write(staged)?;
            }
        }
        prepared.staging_lease.take();
        Ok(())
    }

    pub(crate) fn commit_writeback(&self, writeback: &Writeback) -> Result<Option<PathBuf>> {
        self.publish_writeback(writeback, false)
    }

    fn publish_writeback(&self, writeback: &Writeback, force: bool) -> Result<Option<PathBuf>> {
        let mut plaintext = writeback
            .plaintext
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = PlaintextIdentity::from_file(&plaintext)?;
        let mut baseline = writeback
            .baseline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !force && current == *baseline {
            return Ok(None);
        }
        let lease = writeback
            .lease
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let published = self
            .overlay
            .publish_encrypted(&mut plaintext, &lease)?
            .map(|destination| self.overlay.logical_path(&destination))
            .transpose()?;
        *baseline = current;
        Ok(published)
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        self.overlay.root()
    }

    pub(crate) fn is_internal(&self, path: &Path) -> bool {
        self.overlay.is_internal(path)
    }

    pub(crate) fn is_private(&self, path: &Path) -> Result<bool> {
        self.overlay.is_private(path)
    }

    pub(crate) fn logical_path(&self, path: &Path) -> Result<PathBuf> {
        self.overlay.logical_path(path)
    }

    pub(crate) fn check_access(
        &self,
        path: &Path,
        follow_final: bool,
        request: AccessRequest,
        credentials: &Credentials,
    ) -> Result<AccessPlan> {
        self.overlay.transaction(|transaction| {
            if transaction.native_metadata_passthrough(path, follow_final, |attributes| {
                credentials.allows(attributes, AccessRequest::EXECUTE)
            })? {
                return Ok(AccessPlan::Native(path.to_path_buf()));
            }
            let logical =
                Self::resolve_access_path_in(transaction, path, follow_final, credentials)?;
            let attributes = Self::entry_attributes_in(transaction, &logical)?;
            Self::require_attributes_access(&attributes, request, credentials)?;
            Ok(AccessPlan::Allowed)
        })
    }

    pub(crate) fn prepare_authorized_metadata(
        &self,
        path: &Path,
        follow_final: bool,
        credentials: &Credentials,
    ) -> Result<MetadataPlan> {
        let (logical, mapped, attributes) = self.overlay.transaction(|transaction| {
            if transaction.native_metadata_passthrough(path, follow_final, |attributes| {
                credentials.allows(attributes, AccessRequest::EXECUTE)
            })? {
                return Ok((path.to_path_buf(), path.to_path_buf(), None));
            }
            let logical =
                Self::resolve_access_path_in(transaction, path, follow_final, credentials)?;
            let mapped = transaction.prepare_read(&logical)?;
            let attributes = transaction.attributes(&logical)?;
            Ok((logical, mapped, attributes))
        })?;
        let plaintext_size = if let Some(cipher) = self
            .overlay
            .cipher()
            .filter(|_| self.overlay.is_internal(&mapped))
        {
            if mapped.symlink_metadata()?.is_file() {
                Some(cipher.open_file(&mapped)?.len())
            } else {
                None
            }
        } else {
            None
        };
        Ok(MetadataPlan {
            logical,
            mapped,
            plaintext_size,
            attributes,
        })
    }

    pub(crate) fn canonicalize_authorized(
        &self,
        path: &Path,
        credentials: &Credentials,
    ) -> Result<PathBuf> {
        self.overlay.transaction(|transaction| {
            Self::require_search_in(transaction, path, credentials)?;
            let resolved = transaction.resolve_final(path, false)?;
            let visible = transaction.visible_path(&resolved)?;
            let canonical = visible.canonicalize()?;
            if self.overlay.is_internal(&canonical) {
                transaction.logical_path(&canonical)
            } else {
                Ok(canonical)
            }
        })
    }

    pub(crate) fn prepare_change_directory(
        &self,
        path: &Path,
        credentials: &Credentials,
    ) -> Result<(PathBuf, PathBuf)> {
        self.overlay.transaction(|transaction| {
            let (logical, _) =
                Self::resolve_final_with_search_in(transaction, path, false, credentials)?;
            Self::require_resolved_entry_access_in(
                transaction,
                &logical,
                AccessRequest::EXECUTE,
                credentials,
            )?;
            Ok((transaction.prepare_directory(&logical)?, logical))
        })
    }

    pub(crate) fn directory_view_authorized(
        &self,
        path: &Path,
        credentials: &Credentials,
    ) -> Result<DirectoryView> {
        self.overlay.transaction(|transaction| {
            Self::require_search_in(transaction, path, credentials)?;
            Self::require_entry_access_in(transaction, path, AccessRequest::READ, credentials)?;
            transaction.directory_view(path)
        })
    }

    pub(crate) fn require_descriptor_access(
        &self,
        path: &Path,
        request: AccessRequest,
        credentials: &Credentials,
    ) -> Result<()> {
        self.overlay.transaction(|transaction| {
            let attributes = Self::entry_attributes_in(transaction, path)?;
            Self::require_attributes_access(&attributes, request, credentials)
        })
    }

    #[cfg(test)]
    pub(crate) fn prepare_read(&self, path: &Path) -> Result<PathBuf> {
        self.overlay.prepare_read(path)
    }

    #[cfg(test)]
    pub(crate) fn prepare_metadata(
        &self,
        path: &Path,
        follow_final: bool,
    ) -> Result<(PathBuf, Option<u64>, PathBuf)> {
        let logical = if follow_final {
            self.overlay.resolve_final(path, false)?
        } else {
            path.to_path_buf()
        };
        let mapped = self.overlay.prepare_read(&logical)?;
        let Some(cipher) = self
            .overlay
            .cipher()
            .filter(|_| self.overlay.is_internal(&mapped))
        else {
            return Ok((mapped, None, logical));
        };
        if !mapped.symlink_metadata()?.is_file() {
            return Ok((mapped, None, logical));
        }

        let mut plaintext = tempfile::tempfile()?;
        cipher.decrypt(&mapped, &mut plaintext)?;
        let size = plaintext.metadata()?.len();
        Ok((mapped, Some(size), logical))
    }

    pub(crate) fn attributes(&self, path: &Path) -> Result<Option<super::FileAttributes>> {
        self.overlay.attributes(path)
    }

    pub(crate) fn exists(&self, path: &Path) -> Result<bool> {
        self.overlay.exists(path)
    }

    #[cfg(test)]
    pub(crate) fn native_metadata_passthrough(
        &self,
        path: &Path,
        follow_final: bool,
        credentials: &Credentials,
    ) -> Result<bool> {
        self.overlay
            .native_metadata_passthrough(path, follow_final, |attributes| {
                credentials.allows(attributes, AccessRequest::EXECUTE)
            })
    }

    pub(crate) fn create_directory_authorized(
        &self,
        path: &Path,
        mode: u32,
        credentials: &Credentials,
    ) -> Result<PathBuf> {
        let mode = Self::effective_creation_mode(mode)?;
        self.overlay.transaction(|transaction| {
            let parent_attributes = Self::require_search_in(transaction, path, credentials)?;
            if transaction.visible_exists(path)? {
                return Err(std::io::Error::from_raw_os_error(libc::EEXIST).into());
            }
            Self::require_parent_access(path, parent_attributes.as_ref(), credentials)?;
            transaction.create_directory(path, mode)
        })
    }

    pub(crate) fn create_symlink_authorized(
        &self,
        path: &Path,
        target: &Path,
        credentials: &Credentials,
    ) -> Result<PathBuf> {
        self.overlay.transaction(|transaction| {
            Self::require_parent_mutation_in(transaction, path, credentials)?;
            transaction.create_symlink(path, target)
        })
    }

    pub(crate) fn remove_authorized(
        &self,
        path: &Path,
        directory: bool,
        credentials: &Credentials,
    ) -> Result<()> {
        self.overlay.transaction(|transaction| {
            Self::require_parent_mutation_in(transaction, path, credentials)?;
            transaction.remove(path, directory)
        })
    }

    pub(crate) fn rename_authorized(
        &self,
        from: &Path,
        to: &Path,
        credentials: &Credentials,
    ) -> Result<()> {
        self.overlay.transaction(|transaction| {
            Self::require_parent_mutation_in(transaction, from, credentials)?;
            Self::require_parent_mutation_in(transaction, to, credentials)?;
            transaction.rename(from, to)
        })
    }

    pub(crate) fn chmod_authorized(
        &self,
        path: &Path,
        mode: u32,
        follow_final: bool,
        credentials: &Credentials,
    ) -> Result<()> {
        self.overlay.transaction(|transaction| {
            let logical =
                Self::resolve_access_path_in(transaction, path, follow_final, credentials)?;
            let mut attributes = Self::entry_attributes_in(transaction, &logical)?;
            if !credentials.can_chmod(&attributes) {
                return Err(std::io::Error::from_raw_os_error(libc::EPERM).into());
            }
            attributes.mode = attributes.mode & !0o7777 | mode & 0o7777;
            transaction.set_attributes(&logical, attributes)
        })
    }

    pub(crate) fn set_attributes(
        &self,
        path: &Path,
        attributes: super::FileAttributes,
    ) -> Result<()> {
        self.overlay.set_attributes(path, attributes)
    }

    pub(crate) fn refresh_timestamps(&self, path: &Path, status: &libc::stat) -> Result<()> {
        let mut attributes = match self.overlay.attributes(path)? {
            Some(attributes) => attributes,
            None => super::FileAttributes::from_stat(status),
        };
        attributes.refresh_timestamps(status);
        self.overlay.set_attributes(path, attributes)
    }

    pub(crate) fn visible_identity(&self, path: &Path) -> Result<(u64, u64)> {
        let metadata = self.overlay.visible_path(path)?.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }

    #[cfg(test)]
    pub(crate) fn prepare_write(&self, path: &Path, create: bool) -> Result<PathBuf> {
        self.overlay.prepare_write(path, create)
    }

    pub(crate) fn stage_write(&self, path: &Path, create: bool) -> Result<StagedWrite> {
        self.overlay.stage_write(path, create)
    }

    pub(crate) fn commit_write(&self, staged: StagedWrite) -> Result<()> {
        self.overlay.commit_write(staged)
    }

    #[cfg(test)]
    pub(crate) fn prepare_directory(&self, path: &Path) -> Result<PathBuf> {
        self.overlay.prepare_directory(path)
    }

    pub(crate) fn directory_view(&self, path: &Path) -> Result<DirectoryView> {
        self.overlay.directory_view(path)
    }

    pub(crate) fn native_directory_snapshot_is_current(
        &self,
        snapshot: &super::NativeDirectorySnapshot,
    ) -> Result<bool> {
        self.overlay.native_directory_snapshot_is_current(snapshot)
    }

    #[cfg(test)]
    pub(crate) fn create_directory(&self, path: &Path, mode: u32) -> Result<PathBuf> {
        self.overlay
            .create_directory(path, Self::effective_creation_mode(mode)?)
    }

    #[cfg(test)]
    pub(crate) fn remove(&self, path: &Path, directory: bool) -> Result<()> {
        self.overlay.remove(path, directory)
    }

    #[cfg(test)]
    pub(crate) fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        self.overlay.rename(from, to)
    }

    fn validate_open_flags(flags: libc::c_int) -> Result<()> {
        if flags & (libc::O_NOFOLLOW_ANY | libc::O_SYMLINK) != 0 {
            return Err(std::io::Error::from_raw_os_error(libc::ENOTSUP).into());
        }
        Ok(())
    }

    fn effective_creation_mode(mode: u32) -> Result<u32> {
        let probe = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(mode))
            .tempfile()?;
        Ok(probe.as_file().metadata()?.permissions().mode() & 0o7777)
    }

    #[cfg(test)]
    pub(crate) fn state_for_test(&self, path: &Path) -> Result<Option<EntryState>> {
        self.overlay.state_for_test(path)
    }
}

#[cfg(test)]
mod tests;
