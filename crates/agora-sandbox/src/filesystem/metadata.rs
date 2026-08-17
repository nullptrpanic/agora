use super::namespace::{self, METADATA_FILE};
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::IntoRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

const LEGACY_METADATA_VERSION: u32 = 1;
const READABLE_METADATA_VERSION: u32 = 2;
const METADATA_VERSION: u32 = 3;
const METADATA_CACHE_CAPACITY: usize = 1024;
const MAX_DIRECTORY_METADATA_BYTES: usize = 64 * 1024 * 1024;
const MAX_DIRECTORY_METADATA_RECORDS: usize = 100_000;
const MAX_METADATA_NAME_BYTES: usize = 4 * 1024;
const ENCODED_NAME_PREFIX: &str = "base64:";
const VERSION_THREE_OBJECT_SUFFIX: &[u8] = b"\n  }\n}";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Materializer {
    Copy,
    Executable,
    Loader,
    LoaderTree,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum EntryState {
    Cached {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checksum: Option<String>,
        materializer: Materializer,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<SourceIdentity>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        variant: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<SourceIdentity>,
    },
    Cow,
    Whiteout,
}

impl EntryState {
    pub(crate) fn stored_attributes_are_authoritative(&self, attributes: &FileAttributes) -> bool {
        match self {
            Self::Cow => true,
            Self::Cached {
                source: Some(source),
                ..
            } => !source.matches_materialized_attributes(attributes),
            Self::Cached { source: None, .. } | Self::Whiteout => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SourceIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    mode: u32,
}

impl SourceIdentity {
    pub(crate) fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            mode: metadata.mode(),
        }
    }

    fn matches_materialized_attributes(&self, attributes: &FileAttributes) -> bool {
        self.mode == attributes.mode
            && self.modified_seconds == attributes.mtime
            && self.modified_nanoseconds == attributes.mtime_nsec
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct FileAttributes {
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) atime: i64,
    pub(crate) atime_nsec: i64,
    pub(crate) mtime: i64,
    pub(crate) mtime_nsec: i64,
}

impl FileAttributes {
    pub(crate) fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            atime: metadata.atime(),
            atime_nsec: metadata.atime_nsec(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn from_stat(status: &libc::stat) -> Self {
        Self {
            mode: u32::from(status.st_mode),
            uid: status.st_uid,
            gid: status.st_gid,
            atime: status.st_atime,
            atime_nsec: status.st_atime_nsec,
            mtime: status.st_mtime,
            mtime_nsec: status.st_mtime_nsec,
        }
    }

    pub(crate) fn created_file(mode: u32) -> Self {
        Self::created(u32::from(libc::S_IFREG), mode)
    }

    pub(crate) fn created_directory(mode: u32) -> Self {
        Self::created(u32::from(libc::S_IFDIR), mode)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn refresh_timestamps(&mut self, status: &libc::stat) {
        self.atime = status.st_atime;
        self.atime_nsec = status.st_atime_nsec;
        self.mtime = status.st_mtime;
        self.mtime_nsec = status.st_mtime_nsec;
    }

    fn created(kind: u32, mode: u32) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            mode: kind | mode & 0o7777,
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            atime: i64::try_from(now.as_secs()).unwrap_or(i64::MAX),
            atime_nsec: i64::from(now.subsec_nanos()),
            mtime: i64::try_from(now.as_secs()).unwrap_or(i64::MAX),
            mtime_nsec: i64::from(now.subsec_nanos()),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct DirectoryMetadata {
    version: u32,
    entries: BTreeMap<String, EntryState>,
    attributes: BTreeMap<String, FileAttributes>,
    encrypted_names: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct StoredDirectoryMetadataV2 {
    version: u32,
    entries: BTreeMap<String, EntryState>,
    #[serde(default)]
    attributes: BTreeMap<String, FileAttributes>,
    #[serde(default)]
    backing_names: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct StoredMetadataVersion {
    version: u32,
}

#[derive(Deserialize, Serialize)]
struct StoredDirectoryMetadataV3 {
    version: u32,
    entries: BTreeMap<String, StoredMetadataRecord>,
}

#[derive(Default, Deserialize, Serialize)]
struct StoredMetadataRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entry: Option<EntryState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attributes: Option<FileAttributes>,
}

impl Default for DirectoryMetadata {
    fn default() -> Self {
        Self {
            version: METADATA_VERSION,
            entries: BTreeMap::new(),
            attributes: BTreeMap::new(),
            encrypted_names: BTreeMap::new(),
        }
    }
}

pub(super) struct MetadataStore {
    root: PathBuf,
    generation: Mutex<Option<File>>,
    cipher: Option<super::FileCipher>,
    cache: Mutex<MetadataCache>,
    append_file: Mutex<Option<AppendFile>>,
    #[cfg(test)]
    parse_count: AtomicUsize,
    #[cfg(test)]
    probe_count: AtomicUsize,
    #[cfg(test)]
    publication_count: AtomicUsize,
}

pub(super) struct FilenameMigrationPlan {
    pub(super) renames: HashMap<PathBuf, PathBuf>,
    pub(super) metadata: Vec<(PathBuf, Vec<u8>)>,
    pub(super) leases: Vec<(PathBuf, PathBuf, Vec<u8>)>,
}

struct MetadataCache {
    generation: Option<u64>,
    directories: HashMap<PathBuf, CachedDirectoryMetadata>,
}

struct AppendFile {
    path: PathBuf,
    file: File,
}

#[derive(Clone)]
struct CachedDirectoryMetadata {
    identity: Option<SourceIdentity>,
    metadata: Option<DirectoryMetadata>,
}

impl MetadataStore {
    #[cfg(test)]
    pub(super) fn new(root: &Path) -> Result<Self> {
        Self::with_cipher(root, None)
    }

    pub(super) fn encrypted(root: &Path, cipher: super::FileCipher) -> Result<Self> {
        Self::with_cipher(root, Some(cipher))
    }

    fn with_cipher(root: &Path, cipher: Option<super::FileCipher>) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| {
            format!(
                "failed to create sandbox filesystem root {}",
                root.display()
            )
        })?;
        let generation = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(root.join(namespace::VFS_LOCK_FILE))?;
        Self::with_generation(root, generation, cipher)
    }

    pub(super) fn with_generation(
        root: &Path,
        generation: File,
        cipher: Option<super::FileCipher>,
    ) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| {
            format!(
                "failed to create sandbox filesystem root {}",
                root.display()
            )
        })?;
        Self::initialize_generation(&generation)?;
        let store = Self {
            root: root.to_path_buf(),
            generation: Mutex::new(Some(generation)),
            cipher,
            cache: Mutex::new(MetadataCache {
                generation: None,
                directories: HashMap::new(),
            }),
            append_file: Mutex::new(None),
            #[cfg(test)]
            parse_count: AtomicUsize::new(0),
            #[cfg(test)]
            probe_count: AtomicUsize::new(0),
            #[cfg(test)]
            publication_count: AtomicUsize::new(0),
        };
        store.ensure_marker(Path::new("/"))?;
        Ok(store)
    }

    pub(super) fn prepare_filename_migration(
        &self,
        new_cipher: &super::FileCipher,
    ) -> Result<FilenameMigrationPlan> {
        let mut plan = FilenameMigrationPlan {
            renames: HashMap::new(),
            metadata: Vec::new(),
            leases: Vec::new(),
        };
        let mut pending = vec![self.root.clone()];
        while let Some(backing_directory) = pending.pop() {
            for entry in fs::read_dir(&backing_directory)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    pending.push(entry.path());
                }
            }
            let metadata_path = backing_directory.join(METADATA_FILE);
            if !metadata_path.try_exists()? {
                continue;
            }
            let logical_directory = namespace::logical_path(&self.root, &backing_directory)?;
            let mut metadata = self.load(&logical_directory)?;
            let previous = metadata.encrypted_names.clone();
            let mut assigned = HashSet::new();
            for (logical_name, old_name) in previous {
                let logical = Self::decode(&logical_name)?;
                let new_name = loop {
                    let candidate = new_cipher.encrypt_name(logical.as_bytes())?;
                    if assigned.insert(candidate.clone())
                        && !backing_directory.join(&candidate).try_exists()?
                    {
                        break candidate;
                    }
                };
                let old_path = backing_directory.join(&old_name);
                let new_path = backing_directory.join(&new_name);
                if plan
                    .renames
                    .insert(old_path.clone(), new_path.clone())
                    .is_some()
                {
                    bail!("duplicate encrypted filesystem filename migration source");
                }
                let old_lease = Self::lease_path(&old_path)?;
                if old_lease.try_exists()? {
                    let new_lease = Self::lease_path(&new_path)?;
                    plan.leases.push((
                        old_lease,
                        new_lease,
                        new_path.as_os_str().as_bytes().to_vec(),
                    ));
                }
                metadata.encrypted_names.insert(logical_name, new_name);
            }
            if metadata.version != METADATA_VERSION || !metadata.encrypted_names.is_empty() {
                plan.metadata
                    .push((metadata_path, self.serialize_metadata(&metadata)?));
            }
        }
        Ok(plan)
    }

    fn lease_path(destination: &Path) -> Result<PathBuf> {
        let name = destination
            .file_name()
            .context("encrypted filesystem destination has no filename")?;
        let mut lease = namespace::WRITE_LEASE_PREFIX.to_vec();
        lease.extend_from_slice(name.as_bytes());
        Ok(destination.with_file_name(OsString::from_vec(lease)))
    }

    pub(super) fn state(&self, path: &Path) -> Result<Option<EntryState>> {
        if path == Path::new("/") {
            return Ok(None);
        }
        let (parent, name) = Self::split(path)?;
        Ok(self.load(parent)?.entries.get(&Self::encode(name)).cloned())
    }

    pub(super) fn records(
        &self,
        paths: &[&Path],
    ) -> Result<Vec<(Option<EntryState>, Option<FileAttributes>)>> {
        let generation = self.current_generation()?;
        paths
            .iter()
            .map(|path| {
                let (parent, name) = Self::split(path)?;
                let metadata = self.load_at_generation(parent, generation)?;
                let name = Self::encode(name);
                Ok((
                    metadata.entries.get(&name).cloned(),
                    metadata.attributes.get(&name).cloned(),
                ))
            })
            .collect()
    }

    pub(super) fn ensure_marker(&self, directory: &Path) -> Result<()> {
        let path = self.path(directory)?;
        match path.symlink_metadata() {
            Ok(metadata) if metadata.is_file() => self.load(directory).map(|_| ()),
            Ok(_) => bail!(
                "failed to read filesystem metadata {}: marker is not a file",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.write(directory, &DirectoryMetadata::default())
            }
            Err(error) => Err(error).with_context(|| {
                format!("failed to inspect filesystem metadata {}", path.display())
            }),
        }
    }

    pub(super) fn has_marker(&self, directory: &Path) -> Result<bool> {
        let path = self.path(directory)?;
        let identity = Self::marker_identity(&path)?;
        let generation = self.current_generation()?;
        let cached_identity = {
            let mut cache = self.cache();
            if cache.generation != Some(generation) {
                cache.generation = Some(generation);
                cache.directories.clear();
            }
            cache.directories.get(&path).map(|cached| cached.identity)
        };
        if cached_identity.is_some_and(|cached| cached != identity) {
            self.advance_generation()?;
        }
        if identity.is_some() {
            self.load(directory)?;
        }
        Ok(identity.is_some())
    }

    pub(super) fn invalidate(&self) -> Result<()> {
        self.advance_generation()
    }

    pub(super) fn set(&self, path: &Path, state: EntryState) -> Result<()> {
        let (parent, name) = Self::split(path)?;
        let mut metadata = self.load(parent)?;
        let name = Self::encode(name);
        metadata.entries.insert(name, state);
        self.write(parent, &metadata)
    }

    pub(super) fn set_with_attributes(
        &self,
        path: &Path,
        state: EntryState,
        attributes: Option<FileAttributes>,
    ) -> Result<()> {
        let (parent, name) = Self::split(path)?;
        let mut metadata = self.load(parent)?;
        let name = Self::encode(name);
        metadata.entries.insert(name.clone(), state);
        match attributes {
            Some(attributes) => {
                metadata.attributes.insert(name, attributes);
            }
            None => {
                metadata.attributes.remove(&name);
            }
        }
        self.write(parent, &metadata)
    }

    pub(super) fn set_whiteout(&self, path: &Path, reserve_encrypted_name: bool) -> Result<()> {
        let (parent, name) = Self::split(path)?;
        if self.append_new_whiteout(parent, name, reserve_encrypted_name)? {
            return Ok(());
        }
        let mut metadata = self.load(parent)?;
        let name = Self::encode(name);
        if reserve_encrypted_name
            && !metadata.encrypted_names.contains_key(&name)
            && let Some(cipher) = &self.cipher
        {
            let logical = Self::decode(&name)?;
            metadata
                .encrypted_names
                .insert(name.clone(), cipher.encrypt_name(logical.as_bytes())?);
        }
        metadata.entries.insert(name.clone(), EntryState::Whiteout);
        metadata.attributes.remove(&name);
        self.write(parent, &metadata)
    }

    pub(super) fn move_entry(
        &self,
        from: &Path,
        to: &Path,
        attributes: Option<FileAttributes>,
    ) -> Result<()> {
        let (from_parent, from_name) = Self::split(from)?;
        let (to_parent, to_name) = Self::split(to)?;
        if from_parent != to_parent {
            self.set_with_attributes(from, EntryState::Whiteout, None)?;
            return self.set_with_attributes(to, EntryState::Cow, attributes);
        }
        let mut metadata = self.load(from_parent)?;
        let from_name = Self::encode(from_name);
        metadata
            .entries
            .insert(from_name.clone(), EntryState::Whiteout);
        metadata.attributes.remove(&from_name);
        let to_name = Self::encode(to_name);
        metadata.entries.insert(to_name.clone(), EntryState::Cow);
        match attributes {
            Some(attributes) => {
                metadata.attributes.insert(to_name, attributes);
            }
            None => {
                metadata.attributes.remove(&to_name);
            }
        }
        self.write(from_parent, &metadata)
    }

    pub(super) fn attributes(&self, path: &Path) -> Result<Option<FileAttributes>> {
        if path == Path::new("/") {
            return Ok(None);
        }
        let (parent, name) = Self::split(path)?;
        Ok(self
            .load(parent)?
            .attributes
            .get(&Self::encode(name))
            .cloned())
    }

    pub(super) fn set_attributes(&self, path: &Path, attributes: FileAttributes) -> Result<()> {
        if path == Path::new("/") {
            return Ok(());
        }
        let (parent, name) = Self::split(path)?;
        let mut metadata = self.load(parent)?;
        let name = Self::encode(name);
        if metadata.attributes.get(&name) == Some(&attributes) {
            return Ok(());
        }
        metadata.attributes.insert(name, attributes);
        self.write(parent, &metadata)
    }

    pub(super) fn remove(&self, path: &Path) -> Result<()> {
        if path == Path::new("/") {
            return Ok(());
        }
        let (parent, name) = Self::split(path)?;
        let mut metadata = self.load(parent)?;
        let name = Self::encode(name);
        metadata.entries.remove(&name);
        metadata.attributes.remove(&name);
        metadata.encrypted_names.remove(&name);
        self.write(parent, &metadata)
    }

    pub(super) fn entries(&self, directory: &Path) -> Result<Vec<(OsString, EntryState)>> {
        self.load(directory)?
            .entries
            .into_iter()
            .map(|(name, state)| Ok((Self::decode(&name)?, state)))
            .collect()
    }

    pub(super) fn contains_only_loader_cache_records(&self, directory: &Path) -> Result<bool> {
        let metadata = self.load(directory)?;
        Ok(metadata.attributes.is_empty()
            && metadata.encrypted_names.is_empty()
            && metadata.entries.values().all(|state| {
                matches!(
                    state,
                    EntryState::Cached {
                        materializer: Materializer::Loader,
                        ..
                    }
                )
            }))
    }

    pub(super) fn encrypted_name(&self, path: &Path) -> Result<Option<OsString>> {
        let (parent, name) = Self::split(path)?;
        Ok(self
            .load(parent)?
            .encrypted_names
            .get(&Self::encode(name))
            .map(OsString::from))
    }

    pub(super) fn ensure_encrypted_name(&self, path: &Path) -> Result<OsString> {
        let (parent, name) = Self::split(path)?;
        let mut metadata = self.load(parent)?;
        let canonical_name = Self::encode(name);
        if let Some(encrypted) = metadata.encrypted_names.get(&canonical_name) {
            return Ok(OsString::from(encrypted));
        }
        let cipher = self
            .cipher
            .as_ref()
            .context("encrypted filesystem filename requires a filesystem cipher")?;
        let encrypted = cipher.encrypt_name(name.as_bytes())?;
        metadata
            .encrypted_names
            .insert(canonical_name, encrypted.clone());
        self.write(parent, &metadata)?;
        Ok(OsString::from(encrypted))
    }

    pub(super) fn encrypted_names(&self, directory: &Path) -> Result<Vec<(OsString, OsString)>> {
        self.load(directory)?
            .encrypted_names
            .into_iter()
            .map(|(logical, backing)| Ok((Self::decode(&logical)?, OsString::from(backing))))
            .collect()
    }

    fn split(path: &Path) -> Result<(&Path, &OsStr)> {
        if !path.is_absolute() {
            bail!(
                "filesystem metadata path is not absolute: {}",
                path.display()
            );
        }
        let parent = path
            .parent()
            .context("filesystem metadata path has no parent")?;
        let name = path
            .file_name()
            .context("filesystem metadata path has no file name")?;
        Ok((parent, name))
    }

    fn load(&self, directory: &Path) -> Result<DirectoryMetadata> {
        let generation = self.current_generation()?;
        self.load_at_generation(directory, generation)
    }

    fn load_at_generation(&self, directory: &Path, generation: u64) -> Result<DirectoryMetadata> {
        let path = self.path(directory)?;
        {
            let mut cache = self.cache();
            if cache.generation != Some(generation) {
                cache.generation = Some(generation);
                cache.directories.clear();
            }
            if let Some(cached) = cache.directories.get(&path) {
                return Ok(cached.metadata.clone().unwrap_or_default());
            }
        }
        #[cfg(test)]
        self.probe_count.fetch_add(1, Ordering::Relaxed);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut cache = self.cache();
                if cache.generation == Some(generation) {
                    cache.directories.insert(
                        path,
                        CachedDirectoryMetadata {
                            identity: None,
                            metadata: None,
                        },
                    );
                }
                return Ok(DirectoryMetadata::default());
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read filesystem metadata {}", path.display())
                });
            }
        };
        let file_metadata = file.metadata()?;
        if file_metadata.len() > MAX_DIRECTORY_METADATA_BYTES as u64 {
            bail!(
                "filesystem metadata exceeds {MAX_DIRECTORY_METADATA_BYTES} bytes: {}",
                path.display()
            );
        }
        let identity = Some(SourceIdentity::from_metadata(&file_metadata));
        let contents = super::read_control_file(
            &mut file,
            MAX_DIRECTORY_METADATA_BYTES,
            &format!("filesystem metadata {}", path.display()),
        )?;
        let metadata = self.decode_metadata(&contents, &path)?;
        #[cfg(test)]
        self.parse_count.fetch_add(1, Ordering::Relaxed);
        let mut cache = self.cache();
        if cache.generation != Some(generation) {
            return Ok(metadata);
        }
        if cache.directories.len() >= METADATA_CACHE_CAPACITY
            && !cache.directories.contains_key(&path)
        {
            cache.directories.clear();
        }
        cache.directories.insert(
            path,
            CachedDirectoryMetadata {
                identity,
                metadata: Some(metadata.clone()),
            },
        );
        Ok(metadata)
    }

    fn marker_identity(path: &Path) -> Result<Option<SourceIdentity>> {
        match path.symlink_metadata() {
            Ok(metadata) if metadata.is_file() => {
                Ok(Some(SourceIdentity::from_metadata(&metadata)))
            }
            Ok(_) => bail!(
                "failed to read filesystem metadata {}: marker is not a file",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| {
                format!("failed to inspect filesystem metadata {}", path.display())
            }),
        }
    }

    fn write(&self, directory: &Path, metadata: &DirectoryMetadata) -> Result<()> {
        let mut ancestors = directory.ancestors().skip(1).collect::<Vec<_>>();
        ancestors.reverse();
        for ancestor in ancestors {
            let marker = self.path(ancestor)?;
            match Self::marker_identity(&marker)? {
                Some(_) => {
                    self.load(ancestor)?;
                }
                None => self.write_one(ancestor, &DirectoryMetadata::default())?,
            }
        }
        self.write_one(directory, metadata)
    }

    fn write_one(&self, directory: &Path, metadata: &DirectoryMetadata) -> Result<()> {
        let path = self.path(directory)?;
        let parent = path.parent().context("metadata path has no parent")?;
        let parent_exists = match parent.metadata() {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        fs::create_dir_all(parent)?;
        if !parent_exists {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let contents = self.serialize_metadata(metadata)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("failed to create filesystem metadata {}", path.display()))?;
        file.write_all(&contents)
            .with_context(|| format!("failed to write filesystem metadata {}", path.display()))?;
        let identity = SourceIdentity::from_metadata(&file.metadata()?);
        drop(file);
        #[cfg(test)]
        self.publication_count.fetch_add(1, Ordering::Relaxed);
        self.record_publication(path, identity, metadata.clone())?;
        Ok(())
    }

    fn append_new_whiteout(
        &self,
        directory: &Path,
        name: &OsStr,
        reserve_encrypted_name: bool,
    ) -> Result<bool> {
        let generation = self.current_generation()?;
        let path = self.path(directory)?;
        let cache_ready = {
            let mut cache = self.cache();
            if cache.generation != Some(generation) {
                cache.generation = Some(generation);
                cache.directories.clear();
            }
            cache.directories.contains_key(&path)
        };
        if !cache_ready {
            let _ = self.load_at_generation(directory, generation)?;
        }
        let logical_name = Self::encode(name);
        let candidate = {
            let cache = self.cache();
            let Some(cached) = cache.directories.get(&path) else {
                return Ok(false);
            };
            let (Some(expected_identity), Some(metadata)) =
                (cached.identity, cached.metadata.as_ref())
            else {
                return Ok(false);
            };
            if metadata.entries.contains_key(&logical_name)
                || metadata.attributes.contains_key(&logical_name)
                || metadata.encrypted_names.contains_key(&logical_name)
            {
                return Ok(false);
            }
            let encrypted_name = if reserve_encrypted_name {
                self.cipher
                    .as_ref()
                    .map(|cipher| cipher.encrypt_name(name.as_bytes()))
                    .transpose()?
            } else {
                None
            };
            let stored_name = match encrypted_name.as_ref() {
                Some(encrypted_name) => encrypted_name.clone(),
                None => Self::storage_name(&logical_name)?,
            };
            if stored_name.len() > MAX_METADATA_NAME_BYTES {
                bail!("filesystem metadata name exceeds {MAX_METADATA_NAME_BYTES} bytes");
            }
            let record_upper_bound = metadata
                .entries
                .len()
                .saturating_add(metadata.attributes.len())
                .saturating_add(metadata.encrypted_names.len());
            if record_upper_bound >= MAX_DIRECTORY_METADATA_RECORDS
                && metadata
                    .entries
                    .keys()
                    .chain(metadata.attributes.keys())
                    .chain(metadata.encrypted_names.keys())
                    .collect::<BTreeSet<_>>()
                    .len()
                    >= MAX_DIRECTORY_METADATA_RECORDS
            {
                bail!("filesystem metadata exceeds {MAX_DIRECTORY_METADATA_RECORDS} records");
            }
            if encrypted_name.as_ref().is_some_and(|candidate| {
                metadata
                    .encrypted_names
                    .values()
                    .any(|existing| existing == candidate)
            }) {
                bail!("duplicate filesystem metadata record {stored_name:?}");
            }
            (expected_identity, stored_name, encrypted_name)
        };
        let (expected_identity, stored_name, encrypted_name) = candidate;
        let record = StoredMetadataRecord {
            entry: Some(EntryState::Whiteout),
            attributes: None,
        };
        let Some(identity) =
            self.append_serialized_record(&path, expected_identity, &stored_name, &record)?
        else {
            return Ok(false);
        };
        #[cfg(test)]
        self.publication_count.fetch_add(1, Ordering::Relaxed);
        let next_generation = generation.wrapping_add(1);
        self.write_generation(next_generation)?;
        let mut cache = self.cache();
        if cache.generation != Some(generation) {
            cache.directories.clear();
        } else if let Some(cached) = cache.directories.get_mut(&path)
            && cached.identity == Some(expected_identity)
            && let Some(metadata) = cached.metadata.as_mut()
        {
            metadata
                .entries
                .insert(logical_name.clone(), EntryState::Whiteout);
            metadata.attributes.remove(&logical_name);
            if let Some(encrypted_name) = encrypted_name {
                metadata
                    .encrypted_names
                    .insert(logical_name, encrypted_name);
            }
            cached.identity = Some(identity);
        } else {
            cache.directories.clear();
        }
        cache.generation = Some(next_generation);
        Ok(true)
    }

    fn append_serialized_record(
        &self,
        path: &Path,
        expected_identity: SourceIdentity,
        name: &str,
        record: &StoredMetadataRecord,
    ) -> Result<Option<SourceIdentity>> {
        let mut append = self
            .append_file
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let reusable = append.as_ref().is_some_and(|cached| {
            cached.path == path
                && cached.file.metadata().is_ok_and(|metadata| {
                    SourceIdentity::from_metadata(&metadata) == expected_identity
                })
        });
        if !reusable {
            if let Some(cached) = append.take()
                && !Self::descriptor_matches_path(&cached.file, &cached.path)
            {
                // The application may have closed and reused this cached
                // descriptor. Do not close the descriptor's new owner.
                let _ = cached.file.into_raw_fd();
            }
            let file = match OpenOptions::new().read(true).write(true).open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            if SourceIdentity::from_metadata(&file.metadata()?) != expected_identity {
                return Ok(None);
            }
            *append = Some(AppendFile {
                path: path.to_path_buf(),
                file,
            });
        }
        let file = &append
            .as_ref()
            .context("filesystem metadata append file is missing")?
            .file;
        let file_metadata = file.metadata()?;
        let length = file_metadata.len();
        let suffix_length = VERSION_THREE_OBJECT_SUFFIX.len() as u64;
        if length < suffix_length {
            return Ok(None);
        }
        let mut suffix = [0_u8; VERSION_THREE_OBJECT_SUFFIX.len()];
        let mut read = 0;
        while read < suffix.len() {
            let count = file.read_at(&mut suffix[read..], length - suffix_length + read as u64)?;
            if count == 0 {
                return Ok(None);
            }
            read += count;
        }
        if suffix != VERSION_THREE_OBJECT_SUFFIX {
            return Ok(None);
        }
        let mut replacement = format!(
            ",\n    {}: ",
            serde_json::to_string(name).context("failed to serialize filesystem metadata name")?
        )
        .into_bytes();
        replacement.extend_from_slice(
            &serde_json::to_vec(record)
                .context("failed to serialize filesystem metadata record")?,
        );
        replacement.extend_from_slice(VERSION_THREE_OBJECT_SUFFIX);
        let offset = length - suffix_length;
        let final_length = offset
            .checked_add(replacement.len() as u64)
            .context("filesystem metadata size overflow")?;
        if final_length > MAX_DIRECTORY_METADATA_BYTES as u64 {
            bail!("filesystem metadata exceeds {MAX_DIRECTORY_METADATA_BYTES} bytes");
        }
        file.write_all_at(&replacement, offset)?;
        file.set_len(final_length)?;
        Ok(Some(SourceIdentity::from_metadata(&file.metadata()?)))
    }

    fn record_publication(
        &self,
        path: PathBuf,
        identity: SourceIdentity,
        metadata: DirectoryMetadata,
    ) -> Result<()> {
        let previous = self.current_generation()?;
        let generation = previous.wrapping_add(1);
        self.write_generation(generation)?;
        let mut cache = self.cache();
        if cache.generation != Some(previous) {
            cache.directories.clear();
        }
        cache.generation = Some(generation);
        if cache.directories.len() >= METADATA_CACHE_CAPACITY
            && !cache.directories.contains_key(&path)
        {
            cache.directories.clear();
        }
        cache.directories.insert(
            path,
            CachedDirectoryMetadata {
                identity: Some(identity),
                metadata: Some(metadata),
            },
        );
        Ok(())
    }

    fn decode_metadata(&self, contents: &[u8], path: &Path) -> Result<DirectoryMetadata> {
        let stored: StoredMetadataVersion = serde_json::from_slice(contents)
            .with_context(|| format!("failed to parse filesystem metadata {}", path.display()))?;
        match stored.version {
            LEGACY_METADATA_VERSION | READABLE_METADATA_VERSION => {
                self.decode_legacy_metadata(contents, stored.version, path)
            }
            METADATA_VERSION => self.decode_version_three_metadata(contents, path),
            version => bail!(
                "unsupported filesystem metadata version {version} in {}",
                path.display()
            ),
        }
    }

    fn decode_legacy_metadata(
        &self,
        contents: &[u8],
        version: u32,
        path: &Path,
    ) -> Result<DirectoryMetadata> {
        let stored: StoredDirectoryMetadataV2 = serde_json::from_slice(contents)
            .with_context(|| format!("failed to parse filesystem metadata {}", path.display()))?;
        if stored.version != version {
            bail!(
                "filesystem metadata version changed while parsing {}",
                path.display()
            );
        }
        let metadata = DirectoryMetadata {
            version,
            entries: Self::canonical_map(stored.entries, version)?,
            attributes: Self::canonical_map(stored.attributes, version)?,
            encrypted_names: Self::canonical_map(stored.backing_names, version)?,
        };
        Self::validate_metadata_limits(&metadata, path)?;
        Self::validate_legacy_backing_names(&metadata.encrypted_names, path)?;
        Ok(metadata)
    }

    fn decode_version_three_metadata(
        &self,
        contents: &[u8],
        path: &Path,
    ) -> Result<DirectoryMetadata> {
        let stored: StoredDirectoryMetadataV3 = serde_json::from_slice(contents)
            .with_context(|| format!("failed to parse filesystem metadata {}", path.display()))?;
        if stored.entries.len() > MAX_DIRECTORY_METADATA_RECORDS {
            bail!(
                "filesystem metadata exceeds {MAX_DIRECTORY_METADATA_RECORDS} records in {}",
                path.display()
            );
        }
        let mut metadata = DirectoryMetadata::default();
        let mut logical_names = HashSet::new();
        for (stored_name, record) in stored.entries {
            if stored_name.len() > MAX_METADATA_NAME_BYTES {
                bail!(
                    "filesystem metadata name exceeds {MAX_METADATA_NAME_BYTES} bytes in {}",
                    path.display()
                );
            }
            let encrypted = stored_name.starts_with(super::crypto::ENCRYPTED_NAME_PREFIX);
            if !encrypted && record.entry.is_none() && record.attributes.is_none() {
                bail!(
                    "empty filesystem metadata record {stored_name:?} in {}",
                    path.display()
                );
            }
            let logical_name = if encrypted {
                let cipher = self.cipher.as_ref().with_context(|| {
                    format!(
                        "encrypted filesystem metadata requires a cipher in {}",
                        path.display()
                    )
                })?;
                let bytes = cipher.decrypt_name(&stored_name).with_context(|| {
                    format!(
                        "failed to decrypt filesystem metadata name in {}",
                        path.display()
                    )
                })?;
                Self::validate_logical_name(&bytes, path)?;
                let logical = Self::encode(OsStr::from_bytes(&bytes));
                metadata
                    .encrypted_names
                    .insert(logical.clone(), stored_name.clone());
                logical
            } else {
                Self::canonical_name(&stored_name, READABLE_METADATA_VERSION)?
            };
            if !logical_names.insert(logical_name.clone()) {
                bail!("duplicate filesystem metadata name {stored_name:?}");
            }
            if let Some(entry) = record.entry {
                metadata.entries.insert(logical_name.clone(), entry);
            }
            if let Some(attributes) = record.attributes {
                metadata.attributes.insert(logical_name, attributes);
            }
        }
        Ok(metadata)
    }

    fn serialize_metadata(&self, metadata: &DirectoryMetadata) -> Result<Vec<u8>> {
        let names = metadata
            .entries
            .keys()
            .chain(metadata.attributes.keys())
            .chain(metadata.encrypted_names.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        if names.len() > MAX_DIRECTORY_METADATA_RECORDS {
            bail!("filesystem metadata exceeds {MAX_DIRECTORY_METADATA_RECORDS} records");
        }
        let mut entries = BTreeMap::new();
        for logical_name in names {
            let stored_name =
                if let Some(encrypted_name) = metadata.encrypted_names.get(&logical_name) {
                    if !encrypted_name.starts_with(super::crypto::ENCRYPTED_NAME_PREFIX) {
                        bail!("invalid encrypted filesystem filename {encrypted_name:?}");
                    }
                    encrypted_name.clone()
                } else {
                    Self::storage_name(&logical_name)?
                };
            if stored_name.len() > MAX_METADATA_NAME_BYTES {
                bail!("filesystem metadata name exceeds {MAX_METADATA_NAME_BYTES} bytes");
            }
            let record = StoredMetadataRecord {
                entry: metadata.entries.get(&logical_name).cloned(),
                attributes: metadata.attributes.get(&logical_name).cloned(),
            };
            if entries.insert(stored_name.clone(), record).is_some() {
                bail!("duplicate filesystem metadata record {stored_name:?}");
            }
        }
        let contents = serde_json::to_vec_pretty(&StoredDirectoryMetadataV3 {
            version: METADATA_VERSION,
            entries,
        })
        .context("failed to serialize filesystem metadata")?;
        if contents.len() > MAX_DIRECTORY_METADATA_BYTES {
            bail!("filesystem metadata exceeds {MAX_DIRECTORY_METADATA_BYTES} bytes");
        }
        Ok(contents)
    }

    fn validate_metadata_limits(metadata: &DirectoryMetadata, path: &Path) -> Result<()> {
        let names = metadata
            .entries
            .keys()
            .chain(metadata.attributes.keys())
            .chain(metadata.encrypted_names.keys())
            .collect::<BTreeSet<_>>();
        if names.len() > MAX_DIRECTORY_METADATA_RECORDS {
            bail!(
                "filesystem metadata exceeds {MAX_DIRECTORY_METADATA_RECORDS} records in {}",
                path.display()
            );
        }
        if names
            .iter()
            .any(|name| name.len() > MAX_METADATA_NAME_BYTES)
        {
            bail!(
                "filesystem metadata name exceeds {MAX_METADATA_NAME_BYTES} bytes in {}",
                path.display()
            );
        }
        Ok(())
    }

    fn validate_legacy_backing_names(
        backing_names: &BTreeMap<String, String>,
        path: &Path,
    ) -> Result<()> {
        let mut unique = HashSet::new();
        for backing in backing_names.values() {
            if !namespace::is_file_backing_name(backing.as_bytes()) || !unique.insert(backing) {
                bail!(
                    "invalid filesystem backing name {backing:?} in {}",
                    path.display()
                );
            }
        }
        Ok(())
    }

    fn validate_logical_name(name: &[u8], path: &Path) -> Result<()> {
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.contains(&b'/')
            || name.contains(&0)
        {
            bail!(
                "invalid encrypted filesystem metadata name in {}",
                path.display()
            );
        }
        Ok(())
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, MetadataCache> {
        self.cache.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn initialize_generation(generation: &File) -> Result<()> {
        match generation.metadata()?.len() {
            0 => {
                generation.write_all_at(&0_u64.to_be_bytes(), 0)?;
                generation.set_len(8)?;
            }
            8 => {}
            _ => bail!("filesystem metadata generation is invalid"),
        }
        Ok(())
    }

    pub(super) fn current_generation(&self) -> Result<u64> {
        self.with_generation_file(Self::read_generation)
    }

    fn read_generation(generation: &File) -> Result<u64> {
        let mut bytes = [0_u8; 8];
        let mut offset = 0;
        while offset < bytes.len() {
            let read = generation.read_at(&mut bytes[offset..], offset as u64)?;
            if read == 0 {
                bail!("filesystem metadata generation is incomplete");
            }
            offset += read;
        }
        Ok(u64::from_be_bytes(bytes))
    }

    fn advance_generation(&self) -> Result<()> {
        let generation = self.current_generation()?.wrapping_add(1);
        self.write_generation(generation)?;
        let mut cache = self.cache();
        cache.generation = Some(generation);
        cache.directories.clear();
        Ok(())
    }

    fn write_generation(&self, generation: u64) -> Result<()> {
        self.with_generation_file(|file| {
            file.write_all_at(&generation.to_be_bytes(), 0)?;
            Ok(())
        })
    }

    fn with_generation_file<T>(&self, operation: impl FnOnce(&File) -> Result<T>) -> Result<T> {
        let mut generation = self
            .generation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let current = generation
            .as_ref()
            .is_some_and(|file| self.generation_descriptor_is_current(file));
        if !current {
            if let Some(stale) = generation.take() {
                // The application may have closed and reused this descriptor.
                // Relinquish the numeric descriptor without closing its new owner.
                let _ = stale.into_raw_fd();
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(self.root.join(namespace::VFS_LOCK_FILE))?;
            Self::initialize_generation(&file)?;
            *generation = Some(file);
        }
        operation(
            generation
                .as_ref()
                .context("filesystem metadata generation file is missing")?,
        )
    }

    fn generation_descriptor_is_current(&self, generation: &File) -> bool {
        Self::descriptor_matches_path(generation, &self.root.join(namespace::VFS_LOCK_FILE))
    }

    fn descriptor_matches_path(file: &File, path: &Path) -> bool {
        file.metadata()
            .and_then(|open| path.metadata().map(|expected| (open, expected)))
            .is_ok_and(|(open, expected)| {
                open.dev() == expected.dev() && open.ino() == expected.ino()
            })
    }

    fn path(&self, directory: &Path) -> Result<PathBuf> {
        if !directory.is_absolute() {
            bail!(
                "filesystem metadata directory is not absolute: {}",
                directory.display()
            );
        }
        Ok(namespace::backing_path(&self.root, directory)?.join(METADATA_FILE))
    }

    fn encode(value: &OsStr) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.as_bytes())
    }

    fn decode(value: &str) -> Result<OsString> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .context("invalid encoded filesystem metadata name")?;
        Ok(OsString::from_vec(bytes))
    }

    fn storage_name(name: &str) -> Result<String> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(name)
            .context("invalid encoded filesystem metadata name")?;
        match std::str::from_utf8(&bytes) {
            Ok(name)
                if !name.starts_with(ENCODED_NAME_PREFIX)
                    && !name.starts_with(super::crypto::ENCRYPTED_NAME_PREFIX) =>
            {
                Ok(name.to_string())
            }
            _ => Ok(format!(
                "{ENCODED_NAME_PREFIX}{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
            )),
        }
    }

    fn canonical_map<T>(values: BTreeMap<String, T>, version: u32) -> Result<BTreeMap<String, T>> {
        let mut canonical = BTreeMap::new();
        for (stored, value) in values {
            let name = Self::canonical_name(&stored, version)?;
            if canonical.insert(name, value).is_some() {
                bail!("duplicate filesystem metadata name {stored:?}");
            }
        }
        Ok(canonical)
    }

    fn canonical_name(name: &str, version: u32) -> Result<String> {
        let bytes = if version == LEGACY_METADATA_VERSION {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(name)
                .context("invalid encoded filesystem metadata name")?
        } else if let Some(encoded) = name.strip_prefix(ENCODED_NAME_PREFIX) {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .context("invalid encoded filesystem metadata name")?
        } else {
            name.as_bytes().to_vec()
        };
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    #[cfg(test)]
    fn parse_count(&self) -> usize {
        self.parse_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn probe_count(&self) -> usize {
        self.probe_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn publication_count_for_test(&self) -> usize {
        self.publication_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests;
