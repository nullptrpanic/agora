#[cfg(target_os = "macos")]
pub(crate) mod broker;
#[cfg(target_os = "macos")]
mod crypto;
#[cfg(target_os = "macos")]
mod encrypted;
#[cfg(target_os = "macos")]
mod metadata;
#[cfg(target_os = "macos")]
mod namespace;
#[cfg(target_os = "macos")]
mod overlay;
#[cfg(target_os = "macos")]
mod permissions;
mod ranges;
#[cfg(target_os = "macos")]
mod vfs;
#[cfg(target_os = "macos")]
mod workspace;

use anyhow::{Context, Result, bail};
#[cfg(target_os = "macos")]
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub(crate) use ranges::{ByteRange, ByteRangeSet};

#[cfg(target_os = "macos")]
const MAX_CONTROL_PATH_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FilesystemMode {
    Encrypted,
    #[default]
    Plain,
}

pub(crate) fn normalize_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("filesystem path is not absolute: {}", path.display());
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
            Component::Prefix(_) => bail!("unsupported filesystem path: {}", path.display()),
        }
    }
    Ok(normalized)
}

pub(crate) fn resolve_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut suffix = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut resolved) => {
                for component in suffix.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                let name = existing.file_name().with_context(|| {
                    format!("failed to resolve filesystem path {}", path.display())
                })?;
                suffix.push(name.to_os_string());
                existing.pop();
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to resolve filesystem path {}", path.display())
                });
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn read_control_file(
    reader: &mut impl Read,
    maximum_bytes: usize,
    description: &str,
) -> Result<Vec<u8>> {
    let limit = u64::try_from(maximum_bytes)
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1);
    let mut contents = Vec::with_capacity(maximum_bytes.min(8 * 1024));
    reader
        .take(limit)
        .read_to_end(&mut contents)
        .with_context(|| format!("failed to read {description}"))?;
    if contents.len() > maximum_bytes {
        bail!("{description} exceeds {maximum_bytes} bytes");
    }
    Ok(contents)
}

#[cfg(target_os = "macos")]
pub(crate) use crypto::{EncryptedFile, FileCipher};
#[cfg(target_os = "macos")]
pub(crate) use encrypted::{EncryptedWorkspace, KeyMigrationStage};
#[cfg(target_os = "macos")]
pub(crate) use metadata::{EntryState, FileAttributes, Materializer};
#[cfg(target_os = "macos")]
pub(crate) use overlay::{DirectoryView, NativeDirectorySnapshot, OverlayStore, StagedWrite};
#[cfg(target_os = "macos")]
pub(crate) use permissions::{AccessRequest, Credentials};
#[cfg(target_os = "macos")]
pub(crate) use vfs::{
    AccessPlan, FileLayer, MetadataPlan, OpenIntent, OpenTarget, PreparedFile, VirtualFilesystem,
    Writeback,
};
#[cfg(target_os = "macos")]
pub(crate) use workspace::FilesystemWorkspace;

#[cfg(test)]
mod tests;
