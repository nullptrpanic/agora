use super::{EncryptedWorkspace, FilesystemMode};
#[cfg(not(agora_sandbox_hook_build))]
use super::{FileCipher, OverlayStore};
use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const ROOT_DIRECTORY: &str = "fs";
const LOCK_FILE: &str = ".fs.lock";
const KEY_FILE: &str = ".key.json";

#[derive(Debug)]
pub(crate) enum FilesystemWorkspace {
    Encrypted(Box<EncryptedWorkspace>),
    Plain(PlainWorkspace),
}

impl FilesystemWorkspace {
    pub(crate) fn start(
        workdir: &Path,
        mode: FilesystemMode,
        encrypted_key: Option<&[u8]>,
    ) -> Result<Self> {
        match (mode, encrypted_key) {
            (FilesystemMode::Encrypted, Some(key)) => EncryptedWorkspace::start(workdir, key)
                .map(Box::new)
                .map(Self::Encrypted),
            (FilesystemMode::Encrypted, None) => bail!("sandbox filesystem key is required"),
            (FilesystemMode::Plain, None) => PlainWorkspace::start(workdir).map(Self::Plain),
            (FilesystemMode::Plain, Some(_)) => {
                bail!("encrypted filesystem key cannot be used with plain filesystem mode")
            }
        }
    }

    pub(crate) fn root(&self) -> &Path {
        match self {
            Self::Encrypted(workspace) => workspace.root(),
            Self::Plain(workspace) => workspace.root(),
        }
    }

    pub(crate) fn encrypted_cipher_key(&self) -> Option<&[u8; 32]> {
        match self {
            Self::Encrypted(workspace) => Some(workspace.cipher_key()),
            Self::Plain(_) => None,
        }
    }

    #[cfg(not(agora_sandbox_hook_build))]
    pub(crate) fn visible_directory(&self, path: &Path) -> Result<Option<bool>> {
        let overlay = match self {
            Self::Encrypted(workspace) => OverlayStore::encrypted(
                workspace.root(),
                FileCipher::from_key(workspace.cipher_key())?,
            )?,
            Self::Plain(workspace) => OverlayStore::new(workspace.root())?,
        };
        overlay.visible_directory(path)
    }
}

#[derive(Debug)]
pub(crate) struct PlainWorkspace {
    root: PathBuf,
    _lock: File,
}

impl PlainWorkspace {
    fn start(workdir: &Path) -> Result<Self> {
        let workdir = EncryptedWorkspace::resolved_destination(workdir)?;
        let root = workdir.join(ROOT_DIRECTORY);
        Self::prepare_directory(&root, "plain filesystem root")?;
        if root.join(KEY_FILE).exists() {
            bail!(
                "encrypted filesystem state exists at {}; use encrypted filesystem mode",
                root.display()
            );
        }
        let lock = Self::lock(&root)?;
        Ok(Self { root, _lock: lock })
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn prepare_directory(directory: &Path, description: &str) -> Result<()> {
        crate::managed_fs::prepare_owned_directory(directory, description).map(drop)
    }

    fn lock(directory: &Path) -> Result<File> {
        let path = directory.join(LOCK_FILE);
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
                .with_context(|| format!("filesystem is already in use: {}", directory.display()));
        }
        Ok(lock)
    }
}

#[cfg(test)]
mod tests;
