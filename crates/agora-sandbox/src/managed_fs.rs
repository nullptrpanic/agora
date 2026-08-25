use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub(crate) fn prepare_owned_directory(path: &Path, description: &str) -> Result<File> {
    prepare_owned_directory_with_policy(path, description, true)
}

pub(crate) fn prepare_owned_directory_preserving_mode(
    path: &Path,
    description: &str,
) -> Result<File> {
    prepare_owned_directory_with_policy(path, description, false)
}

fn prepare_owned_directory_with_policy(
    path: &Path,
    description: &str,
    secure_existing: bool,
) -> Result<File> {
    let existed = match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {description} {}", path.display()));
        }
    };
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create {description} {}", path.display()))?;
    let directory = open_owned_directory(path, description)?;
    let metadata = directory
        .metadata()
        .with_context(|| format!("failed to inspect {description} {}", path.display()))?;
    if (secure_existing || !existed) && metadata.permissions().mode() & 0o7777 != 0o700 {
        directory
            .set_permissions(fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure {description} {}", path.display()))?;
    }
    Ok(directory)
}

pub(crate) fn open_owned_directory(path: &Path, description: &str) -> Result<File> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("failed to open {description} {}", path.display()))?;
    let metadata = directory
        .metadata()
        .with_context(|| format!("failed to inspect {description} {}", path.display()))?;
    if !metadata.is_dir() {
        bail!("{description} is not a directory: {}", path.display());
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!(
            "{description} is not owned by the current user: {}",
            path.display()
        );
    }
    Ok(directory)
}

pub(crate) fn open_owned_regular(
    options: &mut OpenOptions,
    path: &Path,
    mode: Option<u32>,
) -> std::io::Result<File> {
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("managed file is not a regular file: {}", path.display()),
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "managed file is not owned by the current user: {}",
                path.display()
            ),
        ));
    }
    if let Some(mode) = mode
        && metadata.permissions().mode() & 0o7777 != mode
    {
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    Ok(file)
}
