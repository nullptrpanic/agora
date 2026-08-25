use anyhow::Result;
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
use anyhow::Context;
#[cfg(target_os = "macos")]
use md5::{Digest, Md5};
#[cfg(target_os = "macos")]
use std::fs::{self, File, OpenOptions};
#[cfg(target_os = "macos")]
use std::io::{Read, Write};
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

#[cfg(target_os = "macos")]
static EMBEDDED_HOOK: &[u8] = include_bytes!(env!("AGORA_SANDBOX_EMBEDDED_HOOK_PATH"));
#[cfg(target_os = "macos")]
const EMBEDDED_HOOK_MD5: &str = env!("AGORA_SANDBOX_EMBEDDED_HOOK_MD5");
#[cfg(target_os = "macos")]
const HOOK_FILE_NAME: &str = "libagora_sandbox.dylib";

#[cfg(target_os = "macos")]
pub fn materialize(workdir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(workdir).with_context(|| {
        format!(
            "failed to create embedded hook work directory {}",
            workdir.display()
        )
    })?;
    anyhow::ensure!(
        workdir.is_dir(),
        "embedded hook work path is not a directory: {}",
        workdir.display()
    );
    let runtime = workdir.join("runtime");
    let hooks = runtime.join("hook");
    let version = hooks.join(EMBEDDED_HOOK_MD5);
    for directory in [&runtime, &hooks] {
        prepare_directory(directory)?;
    }

    let _lock = lock(&hooks)?;
    prepare_directory(&version)?;
    let destination = version.join(HOOK_FILE_NAME);
    if is_matching_regular_file(&destination)? {
        let metadata = fs::metadata(&destination)?;
        if metadata.permissions().mode() & 0o777 != 0o500 {
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o500))?;
        }
        return Ok(destination);
    }

    publish(&version, &destination)?;
    anyhow::ensure!(
        is_matching_regular_file(&destination)?,
        "published embedded sandbox hook is not a matching regular file: {}",
        destination.display()
    );
    Ok(destination)
}

#[cfg(not(target_os = "macos"))]
pub fn materialize(_workdir: &Path) -> Result<PathBuf> {
    anyhow::bail!("embedded sandbox hook is supported only on macOS")
}

#[cfg(target_os = "macos")]
fn prepare_directory(path: &Path) -> Result<()> {
    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                anyhow::ensure!(
                    !metadata.file_type().is_symlink(),
                    "embedded hook runtime directory is a symbolic link: {}",
                    path.display()
                );
                anyhow::ensure!(
                    metadata.is_dir(),
                    "embedded hook runtime path is not a directory: {}",
                    path.display()
                );
                anyhow::ensure!(
                    metadata.uid() == unsafe { libc::geteuid() },
                    "embedded hook runtime directory is not owned by the current user: {}",
                    path.display()
                );
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(path) {
                    Ok(()) => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to create hook runtime directory {}", path.display())
                        });
                    }
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect hook runtime directory {}",
                        path.display()
                    )
                });
            }
        }
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to secure hook runtime directory {}", path.display()))
}

#[cfg(target_os = "macos")]
fn lock(directory: &Path) -> Result<File> {
    let path = directory.join(".lock");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "embedded hook lock is a symbolic link: {}",
                path.display()
            );
            anyhow::ensure!(
                metadata.is_file(),
                "embedded hook lock is not a regular file: {}",
                path.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect embedded hook lock {}", path.display())
            });
        }
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("failed to open embedded hook lock {}", path.display()))?;
    let metadata = lock
        .metadata()
        .with_context(|| format!("failed to inspect embedded hook lock {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "embedded hook lock is not a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "embedded hook lock is not owned by the current user: {}",
        path.display()
    );
    lock.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure embedded hook lock {}", path.display()))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to lock embedded hook cache {}", path.display()));
    }
    Ok(lock)
}

#[cfg(target_os = "macos")]
fn checksum(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {} for MD5", path.display()))?;
    let mut digest = Md5::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {} for MD5", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex_digest(digest.finalize().as_slice()))
}

#[cfg(target_os = "macos")]
fn is_matching_regular_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            anyhow::ensure!(
                metadata.uid() == unsafe { libc::geteuid() },
                "embedded sandbox hook is not owned by the current user: {}",
                path.display()
            );
            if metadata.len() != EMBEDDED_HOOK.len() as u64 {
                return Ok(false);
            }
            match checksum(path) {
                Ok(checksum) => Ok(checksum == EMBEDDED_HOOK_MD5),
                Err(error)
                    if error.chain().any(|cause| {
                        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
                            error.kind() == std::io::ErrorKind::PermissionDenied
                        })
                    }) =>
                {
                    Ok(false)
                }
                Err(error) => Err(error),
            }
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect embedded sandbox hook {}", path.display())),
    }
}

#[cfg(target_os = "macos")]
fn publish(directory: &Path, destination: &Path) -> Result<()> {
    let mut temporary = tempfile::Builder::new()
        .prefix(".libagora_sandbox-")
        .suffix(".tmp")
        .tempfile_in(directory)
        .with_context(|| {
            format!(
                "failed to create embedded hook staging file in {}",
                directory.display()
            )
        })?;
    temporary
        .write_all(EMBEDDED_HOOK)
        .context("failed to write embedded sandbox hook")?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o500))
        .context("failed to secure embedded sandbox hook")?;
    temporary
        .as_file()
        .sync_all()
        .context("failed to sync embedded sandbox hook")?;
    anyhow::ensure!(
        checksum(temporary.path())? == EMBEDDED_HOOK_MD5,
        "embedded sandbox hook staging checksum mismatch"
    );
    temporary.persist(destination).map_err(|error| {
        anyhow::anyhow!(
            "failed to publish embedded sandbox hook {}: {}",
            destination.display(),
            error.error
        )
    })?;
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .with_context(|| {
            format!(
                "failed to sync embedded hook directory {}",
                directory.display()
            )
        })
}

#[cfg(target_os = "macos")]
fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests;
