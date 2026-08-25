use anyhow::{Context, Result, anyhow, bail};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatePaths {
    root: PathBuf,
    db_dir: PathBuf,
    store_path: PathBuf,
    lock_path: PathBuf,
}

impl StatePaths {
    pub fn from_home(home: &Path) -> Self {
        let root = home.join(".agora");
        let db_dir = root.join("db");
        Self {
            store_path: db_dir.join("store.db"),
            lock_path: root.join("node.lock"),
            root,
            db_dir,
        }
    }

    pub fn from_environment() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set; cannot resolve agora state paths")?;
        Ok(Self::from_home(&home))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db_dir(&self) -> &Path {
        &self.db_dir
    }

    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

#[derive(Debug)]
pub struct NodeInstanceGuard {
    _file: File,
}

impl NodeInstanceGuard {
    pub fn acquire(paths: StatePaths) -> Result<Self> {
        secure_directory(paths.root())?;
        secure_directory(paths.db_dir())?;
        let file = open_private_file(paths.lock_path())?;
        lock_exclusive_nonblocking(&file, paths.store_path())?;
        Ok(Self { _file: file })
    }
}

pub fn secure_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("create private directory failed: {}", path.display()))?;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect private directory failed: {}", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!("private state path is not a directory: {}", path.display());
    }
    set_directory_permissions(path)
}

pub fn secure_file(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect private file failed: {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "private state path is not a regular file: {}",
            path.display()
        );
    }
    set_file_permissions(path)
}

fn open_private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    set_creation_mode(&mut options);
    let file = options
        .open(path)
        .with_context(|| format!("open private file failed: {}", path.display()))?;
    secure_file(path)?;
    Ok(file)
}

#[cfg(unix)]
fn lock_exclusive_nonblocking(file: &File, store_path: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;

    // SAFETY: `file` owns a valid descriptor for the duration of this call and the guard retains it.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        bail!(
            "agora-node is already running for store {}",
            store_path.display()
        );
    }
    Err(anyhow!(error)).context("acquire node instance lock failed")
}

#[cfg(not(unix))]
fn lock_exclusive_nonblocking(_file: &File, _store_path: &Path) -> Result<()> {
    bail!("the node instance lock is not supported on this platform")
}

#[cfg(unix)]
fn set_creation_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_creation_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("secure private directory failed: {}", path.display()))
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("secure private file failed: {}", path.display()))
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}
