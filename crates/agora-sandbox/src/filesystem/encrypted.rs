use super::crypto::FileCipher;
use super::metadata::{EntryState, Materializer, MetadataStore};
use super::namespace;
use anyhow::{Context, Result, bail};
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_KEY_METADATA_PARENT_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const ROOT_DIRECTORY: &str = "fs";
const LOCK_FILE: &str = ".fs.lock";
const KEY_FILE: &str = ".key.json";
const VFS_LOCK_FILE: &str = ".vfs.lock";
const DIRECTORY_METADATA_FILE: &str = ".metadata";
const REKEY_JOURNAL_FILE: &str = ".rekey.json";
const KEY_METADATA_VERSION: u32 = 1;
const REKEY_JOURNAL_VERSION: u32 = 1;
const SALT_SIZE: usize = 16;
const MAX_KEY_SIZE: usize = 64 * 1024;
const MAX_KEY_METADATA_BYTES: usize = 64 * 1024;
const MAX_REKEY_JOURNAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_REKEY_JOURNAL_ENTRIES: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyMigrationStage {
    Validating,
    AcquiringLock,
    ReencryptingFiles,
    VerifyingNewKey,
    UpdatingMetadata,
    Completed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct KeyMetadata {
    version: u32,
    salt: String,
    key_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct RekeyJournal {
    version: u32,
    old_key: KeyMetadata,
    new_key: KeyMetadata,
    entries: Vec<RekeyEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RekeyEntry {
    destination: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    renamed_destination: Option<String>,
    staged: String,
    backup: String,
}

struct PreparedRekeyEntry {
    destination: PathBuf,
    renamed_destination: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
    plaintext_digest: Option<[u8; 32]>,
}

enum KeyMetadataPublication {
    Durable,
    PublishedUnconfirmed(anyhow::Error),
}

pub(crate) struct EncryptedWorkspace {
    root: PathBuf,
    _lock: File,
    cipher: FileCipher,
    #[cfg(test)]
    salt: Vec<u8>,
    #[cfg(test)]
    key: Vec<u8>,
}

impl std::fmt::Debug for EncryptedWorkspace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EncryptedWorkspace")
            .field("root", &self.root)
            .field("cipher", &self.cipher)
            .field("key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl EncryptedWorkspace {
    pub(crate) fn start(workdir: &Path, passphrase: &[u8]) -> Result<Self> {
        Self::validate_passphrase(passphrase)?;
        let workdir = Self::resolved_destination(workdir)?;
        let root = workdir.join(ROOT_DIRECTORY);
        Self::prepare_directory(&root)?;
        let lock = Self::lock(&root)?;
        Self::recover_migration(&root)?;
        let metadata = if root.join(KEY_FILE).exists() {
            Self::read_key_metadata(&root)?
        } else {
            if Self::contains_unmanaged_data(&root)? {
                bail!(
                    "unencrypted filesystem data exists at {}; move or remove it before starting encrypted mode",
                    root.display()
                );
            }
            let metadata = Self::new_key_metadata(passphrase)?;
            match Self::write_key_metadata(&root, &metadata)? {
                KeyMetadataPublication::Durable => {}
                KeyMetadataPublication::PublishedUnconfirmed(error) => return Err(error),
            }
            metadata
        };
        let salt = Self::decode_salt(&metadata)?;
        let cipher = FileCipher::derive(passphrase, &salt)?;
        if cipher.key_id() != metadata.key_id {
            bail!("sandbox filesystem key is incorrect");
        }
        Ok(Self {
            root,
            _lock: lock,
            cipher,
            #[cfg(test)]
            salt,
            #[cfg(test)]
            key: passphrase.to_vec(),
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    #[cfg(test)]
    pub(crate) fn salt(&self) -> &[u8] {
        &self.salt
    }

    #[cfg(test)]
    pub(crate) fn key(&self) -> &[u8] {
        &self.key
    }

    pub(crate) fn cipher_key(&self) -> &[u8; 32] {
        self.cipher.key_material()
    }

    pub(crate) fn migrate_key(
        workdir: &Path,
        old_passphrase: &[u8],
        new_passphrase: &[u8],
    ) -> Result<()> {
        Self::migrate_key_with_progress(workdir, old_passphrase, new_passphrase, |_| {})
    }

    pub(crate) fn migrate_key_with_progress(
        workdir: &Path,
        old_passphrase: &[u8],
        new_passphrase: &[u8],
        mut on_progress: impl FnMut(KeyMigrationStage),
    ) -> Result<()> {
        on_progress(KeyMigrationStage::Validating);
        Self::validate_passphrase(old_passphrase)?;
        Self::validate_passphrase(new_passphrase)?;
        if old_passphrase == new_passphrase {
            bail!("new filesystem key must differ from the current key");
        }

        on_progress(KeyMigrationStage::AcquiringLock);
        let workdir = Self::resolved_destination(workdir)?;
        let root = workdir.join(ROOT_DIRECTORY);
        let _root_directory =
            crate::managed_fs::open_owned_directory(&root, "encrypted filesystem migration root")?;
        if !root.join(KEY_FILE).is_file() {
            bail!(
                "encrypted filesystem key metadata does not exist: {}",
                root.join(KEY_FILE).display()
            );
        }
        let _lock = Self::lock(&root)?;
        Self::recover_migration(&root)?;
        Self::cleanup_orphaned_staging(&root)?;
        let metadata = Self::read_key_metadata(&root)?;
        let old_salt = Self::decode_salt(&metadata)?;
        let old_cipher = FileCipher::derive(old_passphrase, &old_salt)?;
        if old_cipher.key_id() != metadata.key_id {
            bail!("sandbox filesystem key is incorrect");
        }
        let new_salt = Self::random_salt()?;
        let new_cipher = FileCipher::derive(new_passphrase, &new_salt)?;
        let new_metadata = KeyMetadata {
            version: KEY_METADATA_VERSION,
            salt: base64::engine::general_purpose::STANDARD.encode(&new_salt),
            key_id: new_cipher.key_id().to_string(),
        };

        on_progress(KeyMigrationStage::ReencryptingFiles);
        let metadata_store = MetadataStore::encrypted(&root, old_cipher.clone())?;
        let sources = Self::prepare_migration_sources(&root, &metadata_store)?;
        let filename_plan = metadata_store.prepare_filename_migration(&new_cipher)?;
        let mut entries = Vec::new();
        let prepared = (|| {
            let mut handled = HashSet::new();
            for source in sources {
                let parent = source
                    .parent()
                    .context("encrypted filesystem file has no parent")?;
                let temporary =
                    parent.join(format!(".agora-rekey-{}.tmp", Uuid::new_v4().simple()));
                let backup = parent.join(format!(".agora-rekey-old-{}", Uuid::new_v4().simple()));
                let plaintext_digest =
                    old_cipher.reencrypt_sparse(&source, &new_cipher, &temporary)?;
                let renamed_destination = filename_plan
                    .renames
                    .get(&source)
                    .cloned()
                    .unwrap_or_else(|| source.clone());
                handled.insert(source.clone());
                entries.push(PreparedRekeyEntry {
                    destination: source,
                    renamed_destination,
                    staged: temporary,
                    backup,
                    plaintext_digest: Some(plaintext_digest),
                });
            }
            for (source, renamed_destination) in &filename_plan.renames {
                if handled.contains(source) {
                    continue;
                }
                let source_metadata = match source.symlink_metadata() {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if !source_metadata.file_type().is_symlink() {
                    bail!(
                        "encrypted filename migration found an untracked file: {}",
                        source.display()
                    );
                }
                let parent = source
                    .parent()
                    .context("encrypted filesystem symlink has no parent")?;
                let temporary =
                    parent.join(format!(".agora-rekey-{}.tmp", Uuid::new_v4().simple()));
                std::os::unix::fs::symlink(fs::read_link(source)?, &temporary)?;
                entries.push(PreparedRekeyEntry {
                    destination: source.clone(),
                    renamed_destination: renamed_destination.clone(),
                    staged: temporary,
                    backup: parent.join(format!(".agora-rekey-old-{}", Uuid::new_v4().simple())),
                    plaintext_digest: None,
                });
            }
            for (destination, renamed_destination, contents) in &filename_plan.leases {
                entries.push(Self::stage_rekey_file(
                    destination,
                    renamed_destination,
                    contents,
                )?);
            }
            for (destination, contents) in &filename_plan.metadata {
                entries.push(Self::stage_rekey_file(destination, destination, contents)?);
            }
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(error) = prepared {
            for entry in entries {
                let _ = fs::remove_file(entry.staged);
            }
            return Err(error);
        }

        on_progress(KeyMigrationStage::VerifyingNewKey);
        let verified = (|| {
            for entry in entries
                .iter()
                .filter_map(|entry| entry.plaintext_digest.map(|digest| (entry, digest)))
            {
                if new_cipher.plaintext_digest(&entry.0.staged)? != entry.1 {
                    bail!(
                        "re-encrypted filesystem file content does not match its source: {}",
                        entry.0.destination.display()
                    );
                }
            }
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(error) = verified {
            for entry in entries {
                let _ = fs::remove_file(entry.staged);
            }
            return Err(error);
        }

        Self::sync_parent_directories(entries.iter().map(|entry| entry.staged.as_path()))?;

        let journal_entries = entries
            .iter()
            .map(|entry| {
                Ok(RekeyEntry {
                    destination: Self::encode_relative_path(&root, &entry.destination)?,
                    renamed_destination: (entry.renamed_destination != entry.destination)
                        .then(|| Self::encode_relative_path(&root, &entry.renamed_destination))
                        .transpose()?,
                    staged: Self::encode_relative_path(&root, &entry.staged)?,
                    backup: Self::encode_relative_path(&root, &entry.backup)?,
                })
            })
            .collect::<Result<Vec<_>>>();
        let journal_entries = match journal_entries {
            Ok(entries) => entries,
            Err(error) => {
                for entry in entries {
                    let _ = fs::remove_file(entry.staged);
                }
                return Err(error);
            }
        };
        let journal = RekeyJournal {
            version: REKEY_JOURNAL_VERSION,
            old_key: metadata,
            new_key: new_metadata.clone(),
            entries: journal_entries,
        };
        if let Err(error) = Self::write_journal(&root, &journal) {
            for entry in entries {
                let _ = fs::remove_file(entry.staged);
            }
            return Err(error);
        }
        let publication = (|| {
            for entry in &entries {
                fs::rename(&entry.destination, &entry.backup).with_context(|| {
                    format!(
                        "failed to preserve encrypted file {}",
                        entry.destination.display()
                    )
                })?;
                Self::sync_parent(&entry.destination)?;
                fs::rename(&entry.staged, &entry.renamed_destination).with_context(|| {
                    format!(
                        "failed to publish re-encrypted filesystem file {}",
                        entry.renamed_destination.display()
                    )
                })?;
                Self::sync_parent(&entry.renamed_destination)?;
            }
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(error) = publication {
            let recovery = Self::recover_migration(&root);
            return match recovery {
                Ok(()) => Err(error),
                Err(recovery) => Err(error.context(format!(
                    "filesystem key migration recovery also failed: {recovery:#}"
                ))),
            };
        }
        on_progress(KeyMigrationStage::UpdatingMetadata);
        let key_publication = match Self::write_key_metadata(&root, &new_metadata) {
            Ok(publication) => publication,
            Err(error) => {
                let recovery = Self::recover_migration(&root);
                return match recovery {
                    Ok(()) => Err(error),
                    Err(recovery) => Err(error.context(format!(
                        "filesystem key migration recovery also failed: {recovery:#}"
                    ))),
                };
            }
        };
        if let KeyMetadataPublication::PublishedUnconfirmed(error) = key_publication {
            return Err(error.context(
                "key metadata publication was not durably confirmed; migration recovery state was preserved",
            ));
        }
        if let Err(cleanup) = Self::recover_migration(&root)
            && let Err(retry) = Self::recover_migration(&root)
        {
            return Err(cleanup.context(format!(
                "new filesystem key is active, but migration cleanup remains incomplete: {retry:#}"
            )));
        }
        on_progress(KeyMigrationStage::Completed);
        Ok(())
    }

    pub(crate) fn validate_passphrase(passphrase: &[u8]) -> Result<()> {
        if passphrase.is_empty() {
            bail!("sandbox filesystem key is empty");
        }
        if passphrase.len() > MAX_KEY_SIZE {
            bail!("sandbox filesystem key exceeds {MAX_KEY_SIZE} bytes");
        }
        Ok(())
    }

    pub(crate) fn resolved_destination(workdir: &Path) -> Result<PathBuf> {
        if workdir.is_absolute() {
            Ok(workdir.to_path_buf())
        } else {
            Ok(std::env::current_dir()
                .context("failed to resolve current directory")?
                .join(workdir))
        }
    }

    fn prepare_directory(directory: &Path) -> Result<()> {
        crate::managed_fs::prepare_owned_directory(directory, "encrypted filesystem root").map(drop)
    }

    fn lock(root: &Path) -> Result<File> {
        let path = root.join(LOCK_FILE);
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600);
        let lock = crate::managed_fs::open_owned_regular(&mut options, &path, Some(0o600))
            .with_context(|| format!("failed to open filesystem lock {}", path.display()))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("filesystem is already in use: {}", root.display()));
        }
        Ok(lock)
    }

    fn new_key_metadata(passphrase: &[u8]) -> Result<KeyMetadata> {
        let salt = Self::random_salt()?;
        let cipher = FileCipher::derive(passphrase, &salt)?;
        Ok(KeyMetadata {
            version: KEY_METADATA_VERSION,
            salt: base64::engine::general_purpose::STANDARD.encode(salt),
            key_id: cipher.key_id().to_string(),
        })
    }

    fn random_salt() -> Result<Vec<u8>> {
        let mut salt = vec![0_u8; SALT_SIZE];
        SystemRandom::new()
            .fill(&mut salt)
            .map_err(|_| anyhow::anyhow!("failed to generate filesystem salt"))?;
        Ok(salt)
    }

    fn decode_salt(metadata: &KeyMetadata) -> Result<Vec<u8>> {
        if metadata.version != KEY_METADATA_VERSION {
            bail!(
                "unsupported encrypted filesystem key metadata version {}",
                metadata.version
            );
        }
        let salt = base64::engine::general_purpose::STANDARD
            .decode(&metadata.salt)
            .context("invalid encrypted filesystem salt")?;
        if salt.len() != SALT_SIZE {
            bail!("invalid encrypted filesystem salt length");
        }
        Ok(salt)
    }

    fn read_key_metadata(root: &Path) -> Result<KeyMetadata> {
        let path = root.join(KEY_FILE);
        let mut options = OpenOptions::new();
        options.read(true);
        let mut file = crate::managed_fs::open_owned_regular(&mut options, &path, Some(0o600))
            .with_context(|| {
                format!(
                    "failed to open encrypted filesystem key metadata {}",
                    path.display()
                )
            })?;
        if file.metadata()?.len() > MAX_KEY_METADATA_BYTES as u64 {
            bail!(
                "encrypted filesystem key metadata exceeds {MAX_KEY_METADATA_BYTES} bytes: {}",
                path.display()
            );
        }
        let contents = super::read_control_file(
            &mut file,
            MAX_KEY_METADATA_BYTES,
            &format!("encrypted filesystem key metadata {}", path.display()),
        )?;
        serde_json::from_slice(&contents).with_context(|| {
            format!(
                "failed to parse encrypted filesystem key metadata {}",
                path.display()
            )
        })
    }

    fn write_key_metadata(root: &Path, metadata: &KeyMetadata) -> Result<KeyMetadataPublication> {
        let path = root.join(KEY_FILE);
        let temporary = root.join(format!(".key.json.{}.tmp", Uuid::new_v4().simple()));
        let contents = serde_json::to_vec_pretty(metadata)
            .context("failed to serialize encrypted filesystem key metadata")?;
        if contents.len() > MAX_KEY_METADATA_BYTES {
            bail!("encrypted filesystem key metadata exceeds {MAX_KEY_METADATA_BYTES} bytes");
        }
        let publication = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            std::io::Write::write_all(&mut file, &contents)?;
            file.sync_all()?;
            fs::rename(&temporary, &path)
        })();
        if publication.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        publication.with_context(|| {
            format!(
                "failed to write encrypted filesystem key metadata {}",
                path.display()
            )
        })?;
        match Self::sync_key_metadata_parent(&path) {
            Ok(()) => Ok(KeyMetadataPublication::Durable),
            Err(error) => Ok(KeyMetadataPublication::PublishedUnconfirmed(error.context(
                format!(
                    "failed to sync encrypted filesystem key metadata directory {}",
                    root.display()
                ),
            ))),
        }
    }

    fn contains_unmanaged_data(root: &Path) -> Result<bool> {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_name() != LOCK_FILE {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn sync_key_metadata_parent(path: &Path) -> Result<()> {
        #[cfg(test)]
        if FAIL_NEXT_KEY_METADATA_PARENT_SYNC.with(std::cell::Cell::take) {
            bail!("injected key metadata parent sync failure");
        }
        Self::sync_parent(path)
    }

    #[cfg(test)]
    fn fail_next_key_metadata_parent_sync() {
        FAIL_NEXT_KEY_METADATA_PARENT_SYNC.with(|fail| fail.set(true));
    }

    fn prepare_migration_sources(root: &Path, metadata: &MetadataStore) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        let mut directories = vec![root.to_path_buf()];
        while let Some(directory) = directories.pop() {
            let logical_directory = namespace::logical_path(root, &directory)?;
            let aliases = metadata
                .encrypted_names(&logical_directory)?
                .into_iter()
                .map(|(logical, backing)| (backing, logical))
                .collect::<std::collections::HashMap<_, _>>();
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    directories.push(entry.path());
                    continue;
                }
                if Self::is_control_file(&entry.path()) {
                    continue;
                }
                let physical_name = entry.file_name();
                let logical_name = aliases
                    .get(&physical_name)
                    .cloned()
                    .unwrap_or(namespace::decode_name(&physical_name)?);
                let logical = logical_directory.join(logical_name);
                if file_type.is_socket() {
                    fs::remove_file(entry.path())?;
                    metadata.remove(&logical)?;
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                match metadata.state(&logical)? {
                    Some(EntryState::Cached {
                        materializer: Materializer::Copy,
                        ..
                    }) => {
                        fs::remove_file(entry.path())?;
                        let lease = MetadataStore::lease_path(&entry.path())?;
                        match fs::remove_file(lease) {
                            Ok(()) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                        }
                        metadata.remove(&logical)?;
                    }
                    Some(EntryState::Cached { .. } | EntryState::Whiteout) => {}
                    Some(EntryState::Cow) | None => files.push(entry.path()),
                }
            }
        }
        Ok(files)
    }

    fn stage_rekey_file(
        destination: &Path,
        renamed_destination: &Path,
        contents: &[u8],
    ) -> Result<PreparedRekeyEntry> {
        let parent = destination
            .parent()
            .context("filesystem key migration file has no parent")?;
        let staged = parent.join(format!(".agora-rekey-{}.tmp", Uuid::new_v4().simple()));
        let backup = parent.join(format!(".agora-rekey-old-{}", Uuid::new_v4().simple()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)?;
        std::io::Write::write_all(&mut file, contents)?;
        file.sync_all()?;
        Ok(PreparedRekeyEntry {
            destination: destination.to_path_buf(),
            renamed_destination: renamed_destination.to_path_buf(),
            staged,
            backup,
            plaintext_digest: None,
        })
    }

    fn is_control_file(path: &Path) -> bool {
        let Some(name) = path.file_name().map(|name| name.as_bytes()) else {
            return false;
        };
        name == LOCK_FILE.as_bytes()
            || name == KEY_FILE.as_bytes()
            || name == VFS_LOCK_FILE.as_bytes()
            || name == REKEY_JOURNAL_FILE.as_bytes()
            || name == DIRECTORY_METADATA_FILE.as_bytes()
            || name.starts_with(b".key.json.")
            || name.starts_with(b".rekey.json.")
            || name.starts_with(b".metadata.")
            || name.starts_with(b".agora-encrypted-")
            || name.starts_with(b".agora-rekey-")
            || name.starts_with(namespace::WRITE_LEASE_PREFIX)
    }

    fn write_journal(root: &Path, journal: &RekeyJournal) -> Result<()> {
        let path = root.join(REKEY_JOURNAL_FILE);
        let temporary = root.join(format!(
            "{REKEY_JOURNAL_FILE}.{}.tmp",
            Uuid::new_v4().simple()
        ));
        Self::validate_rekey_journal(journal)?;
        let contents = serde_json::to_vec_pretty(journal)
            .context("failed to serialize filesystem key migration journal")?;
        if contents.len() > MAX_REKEY_JOURNAL_BYTES {
            bail!("filesystem key migration journal exceeds {MAX_REKEY_JOURNAL_BYTES} bytes");
        }
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            std::io::Write::write_all(&mut file, &contents)?;
            file.sync_all()?;
            fs::rename(&temporary, &path)?;
            Self::sync_parent(&path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.with_context(|| format!("failed to write key migration journal {}", path.display()))
    }

    fn recover_migration(root: &Path) -> Result<()> {
        let _root_directory =
            crate::managed_fs::open_owned_directory(root, "encrypted filesystem migration root")?;
        let path = root.join(REKEY_JOURNAL_FILE);
        let mut options = OpenOptions::new();
        options.read(true);
        let mut file = match crate::managed_fs::open_owned_regular(&mut options, &path, Some(0o600))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("failed to open key migration journal"),
        };
        if file.metadata()?.len() > MAX_REKEY_JOURNAL_BYTES as u64 {
            bail!("filesystem key migration journal exceeds {MAX_REKEY_JOURNAL_BYTES} bytes");
        }
        let contents = super::read_control_file(
            &mut file,
            MAX_REKEY_JOURNAL_BYTES,
            "filesystem key migration journal",
        )?;
        let journal: RekeyJournal =
            serde_json::from_slice(&contents).context("failed to parse key migration journal")?;
        if journal.version != REKEY_JOURNAL_VERSION {
            bail!(
                "unsupported key migration journal version {}",
                journal.version
            );
        }
        Self::validate_rekey_journal(&journal)?;
        let current = Self::read_key_metadata(root)?;
        let committed = current == journal.new_key;
        if !committed && current != journal.old_key {
            bail!("key migration journal does not match current key metadata");
        }
        for entry in &journal.entries {
            let destination = Self::decode_relative_path(root, &entry.destination)?;
            let renamed_destination = entry
                .renamed_destination
                .as_deref()
                .map(|path| Self::decode_relative_path(root, path))
                .transpose()?
                .unwrap_or_else(|| destination.clone());
            let staged = Self::decode_relative_path(root, &entry.staged)?;
            let backup = Self::decode_relative_path(root, &entry.backup)?;
            if committed {
                Self::remove_migration_file_if_exists(root, &backup)?;
                Self::remove_migration_file_if_exists(root, &staged)?;
                Self::sync_migration_parent_directories(
                    root,
                    [backup.as_path(), staged.as_path()],
                )?;
            } else {
                if Self::migration_path_entry_exists(root, &backup)? {
                    Self::remove_migration_file_if_exists(root, &renamed_destination)?;
                    Self::rename_migration_path(root, &backup, &destination).with_context(
                        || format!("failed to restore encrypted file {}", destination.display()),
                    )?;
                    Self::sync_migration_parent_directories(
                        root,
                        [renamed_destination.as_path(), destination.as_path()],
                    )?;
                }
                Self::remove_migration_file_if_exists(root, &staged)?;
                Self::sync_migration_parent(root, &staged)?;
            }
        }
        Self::remove_migration_file_if_exists(root, &path).with_context(|| {
            format!("failed to remove key migration journal {}", path.display())
        })?;
        Self::sync_migration_parent(root, &path)
    }

    fn encode_relative_path(root: &Path, path: &Path) -> Result<String> {
        let relative = path.strip_prefix(root).with_context(|| {
            format!(
                "migration path is outside filesystem root: {}",
                path.display()
            )
        })?;
        Ok(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(relative.as_os_str().as_bytes()),
        )
    }

    fn decode_relative_path(root: &Path, encoded: &str) -> Result<PathBuf> {
        Ok(root.join(Self::decode_relative_path_value(encoded)?))
    }

    fn decode_relative_path_value(encoded: &str) -> Result<PathBuf> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .context("invalid migration path encoding")?;
        let relative = PathBuf::from(std::ffi::OsString::from_vec(bytes));
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            bail!("invalid path in key migration journal");
        }
        Ok(relative)
    }

    fn open_migration_parent(root: &Path, path: &Path) -> Result<(File, CString)> {
        let relative = path.strip_prefix(root).with_context(|| {
            format!(
                "migration path is outside filesystem root: {}",
                path.display()
            )
        })?;
        let components = relative
            .components()
            .map(|component| match component {
                Component::Normal(name) => Ok(name),
                _ => bail!("invalid filesystem migration path: {}", path.display()),
            })
            .collect::<Result<Vec<_>>>()?;
        let (leaf, parents) = components
            .split_last()
            .context("filesystem migration path has no filename")?;
        let mut directory =
            crate::managed_fs::open_owned_directory(root, "encrypted filesystem migration root")?;
        for component in parents {
            let component = CString::new(component.as_bytes())
                .context("filesystem migration path contains a null byte")?;
            let descriptor = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    component.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!(
                        "failed to open filesystem migration parent {}",
                        path.display()
                    )
                });
            }
            directory = unsafe { File::from_raw_fd(descriptor) };
        }
        let leaf = CString::new(leaf.as_bytes())
            .context("filesystem migration filename contains a null byte")?;
        Ok((directory, leaf))
    }

    fn remove_migration_file_if_exists(root: &Path, path: &Path) -> Result<()> {
        let (parent, leaf) = Self::open_migration_parent(root, path)?;
        if unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error)
                .with_context(|| format!("failed to remove migration file {}", path.display()))
        }
    }

    fn migration_path_entry_exists(root: &Path, path: &Path) -> Result<bool> {
        let (parent, leaf) = Self::open_migration_parent(root, path)?;
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(error)
                .with_context(|| format!("failed to inspect migration file {}", path.display()))
        }
    }

    fn rename_migration_path(root: &Path, source: &Path, destination: &Path) -> Result<()> {
        let (source_parent, source_leaf) = Self::open_migration_parent(root, source)?;
        let (destination_parent, destination_leaf) =
            Self::open_migration_parent(root, destination)?;
        if unsafe {
            libc::renameat(
                source_parent.as_raw_fd(),
                source_leaf.as_ptr(),
                destination_parent.as_raw_fd(),
                destination_leaf.as_ptr(),
            )
        } == 0
        {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "failed to rename migration file {} to {}",
                    source.display(),
                    destination.display()
                )
            })
        }
    }

    fn sync_migration_parent(root: &Path, path: &Path) -> Result<()> {
        let (parent, _) = Self::open_migration_parent(root, path)?;
        parent
            .sync_all()
            .with_context(|| format!("failed to sync migration parent for {}", path.display()))
    }

    fn sync_migration_parent_directories<'a>(
        root: &Path,
        paths: impl IntoIterator<Item = &'a Path>,
    ) -> Result<()> {
        let mut synced = HashSet::new();
        for path in paths {
            let parent = path
                .parent()
                .context("filesystem migration path has no parent")?;
            if synced.insert(parent.to_path_buf()) {
                Self::sync_migration_parent(root, path)?;
            }
        }
        Ok(())
    }

    fn sync_parent(path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("filesystem migration path has no parent")?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("failed to sync directory {}", parent.display()))
    }

    fn sync_parent_directories<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Result<()> {
        let mut synced = HashSet::new();
        for path in paths {
            let parent = path
                .parent()
                .context("filesystem migration path has no parent")?;
            if synced.insert(parent.to_path_buf()) {
                File::open(parent)
                    .and_then(|directory| directory.sync_all())
                    .with_context(|| format!("failed to sync directory {}", parent.display()))?;
            }
        }
        Ok(())
    }

    fn cleanup_orphaned_staging(root: &Path) -> Result<()> {
        let mut directories = vec![root.to_path_buf()];
        let mut removed = Vec::new();
        while let Some(directory) = directories.pop() {
            for entry in fs::read_dir(&directory).with_context(|| {
                format!(
                    "failed to inspect encrypted filesystem directory {}",
                    directory.display()
                )
            })? {
                let entry = entry?;
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    directories.push(entry.path());
                    continue;
                }
                let path = entry.path();
                if !Self::is_internal_migration_name(&path, b".agora-rekey-", b".tmp")
                    && !Self::is_internal_migration_name(&path, b".agora-encrypted-", b".tmp")
                {
                    continue;
                }
                Self::remove_migration_file_if_exists(root, &path)?;
                removed.push(path);
            }
        }
        Self::sync_migration_parent_directories(root, removed.iter().map(PathBuf::as_path))
    }

    fn validate_rekey_journal(journal: &RekeyJournal) -> Result<()> {
        if journal.entries.len() > MAX_REKEY_JOURNAL_ENTRIES {
            bail!("filesystem key migration journal exceeds {MAX_REKEY_JOURNAL_ENTRIES} entries");
        }
        let mut assigned = HashSet::new();
        for entry in &journal.entries {
            let paths = [
                Some(entry.destination.as_str()),
                entry.renamed_destination.as_deref(),
                Some(entry.staged.as_str()),
                Some(entry.backup.as_str()),
            ];
            if paths
                .into_iter()
                .flatten()
                .any(|path| path.len() > super::MAX_CONTROL_PATH_BYTES)
            {
                bail!(
                    "filesystem key migration journal path exceeds {} bytes",
                    super::MAX_CONTROL_PATH_BYTES
                );
            }
            let destination = Self::decode_relative_path_value(&entry.destination)?;
            let renamed_destination = entry
                .renamed_destination
                .as_deref()
                .map(Self::decode_relative_path_value)
                .transpose()?;
            let staged = Self::decode_relative_path_value(&entry.staged)?;
            let backup = Self::decode_relative_path_value(&entry.backup)?;
            if Self::is_migration_control_target(&destination)
                || renamed_destination
                    .as_deref()
                    .is_some_and(Self::is_migration_control_target)
            {
                bail!("key migration journal targets a managed control file");
            }
            if !Self::is_internal_migration_name(&staged, b".agora-rekey-", b".tmp")
                || !Self::is_internal_migration_name(&backup, b".agora-rekey-old-", b"")
            {
                bail!("key migration journal contains an invalid internal filename");
            }
            let parent = destination.parent();
            if staged.parent() != parent
                || backup.parent() != parent
                || renamed_destination
                    .as_deref()
                    .is_some_and(|renamed| renamed.parent() != parent)
            {
                bail!("key migration journal paths do not share one parent directory");
            }
            for path in [
                Some(&destination),
                renamed_destination.as_ref(),
                Some(&staged),
                Some(&backup),
            ]
            .into_iter()
            .flatten()
            {
                if !assigned.insert(path.clone()) {
                    bail!("key migration journal contains overlapping paths");
                }
            }
        }
        Ok(())
    }

    fn is_migration_control_target(path: &Path) -> bool {
        let Some(name) = path.file_name().map(|name| name.as_bytes()) else {
            return true;
        };
        name == LOCK_FILE.as_bytes()
            || name == KEY_FILE.as_bytes()
            || name == VFS_LOCK_FILE.as_bytes()
            || name == REKEY_JOURNAL_FILE.as_bytes()
            || name.starts_with(b".key.json.")
            || name.starts_with(b".rekey.json.")
            || name.starts_with(b".agora-encrypted-")
            || name.starts_with(b".agora-rekey-")
    }

    fn is_internal_migration_name(path: &Path, prefix: &[u8], suffix: &[u8]) -> bool {
        let Some(name) = path.file_name().map(|name| name.as_bytes()) else {
            return false;
        };
        let Some(identifier) = name
            .strip_prefix(prefix)
            .and_then(|name| name.strip_suffix(suffix))
        else {
            return false;
        };
        identifier.len() == 32 && identifier.iter().all(u8::is_ascii_hexdigit)
    }
}

#[cfg(test)]
mod tests;
