use super::normalize_path as normalize;
use anyhow::{Context, Result, bail};
use base64::Engine;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

pub(super) const METADATA_FILE: &str = ".metadata";
pub(super) const FILESYSTEM_LOCK_FILE: &str = ".fs.lock";
pub(super) const KEY_FILE: &str = ".key.json";
pub(super) const VFS_LOCK_FILE: &str = ".vfs.lock";
pub(super) const REKEY_JOURNAL_FILE: &str = ".rekey.json";
pub(super) const WRITE_LEASE_PREFIX: &[u8] = b".agora-write-lease-";
const ESCAPED_PREFIX: &[u8] = b".agora-entry-";

pub(super) fn backing_path(root: &Path, logical: &Path) -> Result<PathBuf> {
    let logical = normalize(logical)?;
    let mut destination = root.to_path_buf();
    for component in logical.components() {
        if let Component::Normal(name) = component {
            destination.push(encode_name(name));
        }
    }
    Ok(destination)
}

pub(super) fn logical_path(root: &Path, backing: &Path) -> Result<PathBuf> {
    let relative = backing
        .strip_prefix(root)
        .with_context(|| format!("path is not inside filesystem root: {}", backing.display()))?;
    let mut logical = PathBuf::from("/");
    for component in relative.components() {
        let Component::Normal(name) = component else {
            bail!("invalid filesystem backing path: {}", backing.display());
        };
        let decoded = decode_name(name)?;
        let mut decoded_components = Path::new(&decoded).components();
        if decoded.as_bytes().contains(&0)
            || !matches!(decoded_components.next(), Some(Component::Normal(_)))
            || decoded_components.next().is_some()
        {
            bail!("invalid filesystem backing path: {}", backing.display());
        }
        logical.push(decoded);
    }
    Ok(logical)
}

pub(super) fn encode_name(name: &OsStr) -> OsString {
    let bytes = name.as_bytes();
    if is_control_name(name) || bytes.starts_with(ESCAPED_PREFIX) {
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut physical = ESCAPED_PREFIX.to_vec();
        physical.extend_from_slice(encoded.as_bytes());
        OsString::from_vec(physical)
    } else {
        name.to_os_string()
    }
}

pub(super) fn decode_name(name: &OsStr) -> Result<OsString> {
    let bytes = name.as_bytes();
    let Some(encoded) = bytes.strip_prefix(ESCAPED_PREFIX) else {
        return Ok(name.to_os_string());
    };
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .context("invalid escaped filesystem entry name")?;
    Ok(OsString::from_vec(decoded))
}

pub(super) fn is_control_name(name: &OsStr) -> bool {
    let name = name.as_bytes();
    is_reserved_or_variant(name)
        || is_file_backing_name(name)
        || name.starts_with(super::crypto::ENCRYPTED_NAME_PREFIX.as_bytes())
        || name.starts_with(b".agora-executable-")
        || name.starts_with(b".agora-encrypted-")
        || name.starts_with(b".agora-rekey-")
        || name.starts_with(WRITE_LEASE_PREFIX)
}

pub(super) fn is_file_backing_name(name: &[u8]) -> bool {
    name.len() == 32 && name.iter().all(u8::is_ascii_hexdigit)
}

fn is_reserved(name: &[u8]) -> bool {
    name == METADATA_FILE.as_bytes()
        || name == FILESYSTEM_LOCK_FILE.as_bytes()
        || name == KEY_FILE.as_bytes()
        || name == VFS_LOCK_FILE.as_bytes()
        || name == REKEY_JOURNAL_FILE.as_bytes()
}

fn is_reserved_or_variant(name: &[u8]) -> bool {
    if is_reserved(name) {
        return true;
    }
    [
        METADATA_FILE,
        FILESYSTEM_LOCK_FILE,
        KEY_FILE,
        VFS_LOCK_FILE,
        REKEY_JOURNAL_FILE,
    ]
    .into_iter()
    .any(|reserved| {
        name.strip_prefix(reserved.as_bytes())
            .is_some_and(|suffix| suffix.starts_with(b"."))
    })
}

#[cfg(test)]
mod tests;
