//! Workspace session discovery and startup election.

use super::protocol::{DaemonReadiness, read_frame, write_frame};
use anyhow::{Context, Result, bail};
use ring::digest::{Context as DigestContext, SHA256};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::net::UnixStream;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub(crate) struct SessionPaths {
    socket: PathBuf,
    startup_lock: PathBuf,
}

impl SessionPaths {
    pub(crate) fn resolve(workdir: &Path) -> Result<Self> {
        std::fs::create_dir_all(workdir).with_context(|| {
            format!(
                "failed to create sandbox work directory {}",
                workdir.display()
            )
        })?;
        let workdir = workdir
            .canonicalize()
            .with_context(|| format!("failed to resolve sandbox workdir {}", workdir.display()))?;
        let workspace_runtime = workdir.join("runtime");
        crate::managed_fs::prepare_owned_directory(
            &workspace_runtime,
            "sandbox runtime directory",
        )?;
        let socket_directory = session_socket_directory()?;
        let digest = ring::digest::digest(&SHA256, workdir.as_os_str().as_bytes());
        let socket = socket_directory.join(format!("{}.sock", hex(&digest.as_ref()[..16])));
        Ok(Self {
            startup_lock: workspace_runtime.join("session-start.lock"),
            socket,
        })
    }

    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    pub(crate) fn startup_lock(&self) -> &Path {
        &self.startup_lock
    }
}

pub(crate) struct StartupLock(File);

impl StartupLock {
    pub(super) fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600);
        let file = crate::managed_fs::open_owned_regular(&mut options, path, Some(0o600))
            .with_context(|| format!("failed to open session startup lock {}", path.display()))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error
                .raw_os_error()
                .is_some_and(|code| code == libc::EAGAIN || code == libc::EWOULDBLOCK)
            {
                return Ok(None);
            }
            return Err(error)
                .with_context(|| format!("failed to lock sandbox session {}", path.display()));
        }
        Ok(Some(Self(file)))
    }

    fn make_inheritable(&self) -> Result<()> {
        make_inheritable(self.0.as_raw_fd()).context("failed to inherit session startup lock")
    }

    fn descriptor(&self) -> libc::c_int {
        self.0.as_raw_fd()
    }
}

pub struct DaemonStartup {
    ready: Option<UnixStream>,
    _startup_lock: OwnedFd,
}

impl DaemonStartup {
    /// # Safety
    ///
    /// Both descriptors must be distinct, open descriptors inherited by the
    /// current process. This function takes ownership of them.
    pub unsafe fn from_raw_descriptors(
        ready: libc::c_int,
        startup_lock: libc::c_int,
    ) -> Result<Self> {
        if ready == startup_lock {
            bail!("sandbox daemon descriptors must be distinct");
        }
        let ready = inherited_descriptor(ready, "readiness")?;
        let startup_lock = inherited_descriptor(startup_lock, "startup lock")?;
        let ready: StdUnixStream = ready.into();
        ready
            .set_nonblocking(true)
            .context("failed to configure inherited readiness channel")?;
        let ready = UnixStream::from_std(ready)
            .context("failed to register inherited readiness channel")?;
        Ok(Self {
            ready: Some(ready),
            _startup_lock: startup_lock,
        })
    }

    pub async fn failed(mut self, error: &anyhow::Error) -> Result<()> {
        if let Some(mut ready) = self.ready.take() {
            write_frame(
                &mut ready,
                &DaemonReadiness::Failed {
                    message: format!("{error:#}"),
                },
            )
            .await
            .context("failed to report sandbox daemon startup failure")?;
        }
        Ok(())
    }

    pub(crate) async fn ready(&mut self) -> Result<()> {
        if let Some(mut ready) = self.ready.take() {
            write_frame(&mut ready, &DaemonReadiness::Ready)
                .await
                .context("failed to report sandbox daemon readiness")?;
        }
        Ok(())
    }
}

pub(crate) async fn connect_or_start(config: &Path, paths: &SessionPaths) -> Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match UnixStream::connect(paths.socket()).await {
            Ok(stream) => return Ok(stream),
            Err(error) if retryable_connect_error(&error) => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to connect to sandbox session {}",
                        paths.socket().display()
                    )
                });
            }
        }
        if let Some(startup_lock) = StartupLock::try_acquire(paths.startup_lock())? {
            match UnixStream::connect(paths.socket()).await {
                Ok(stream) => return Ok(stream),
                Err(error) if retryable_connect_error(&error) => {}
                Err(error) => {
                    return Err(error).context("failed to retry sandbox session connection");
                }
            }
            remove_stale_socket(paths.socket())?;
            spawn_daemon(config, startup_lock).await?;
            return UnixStream::connect(paths.socket()).await.with_context(|| {
                format!(
                    "failed to connect to started sandbox session {}",
                    paths.socket().display()
                )
            });
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for sandbox session startup")
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn spawn_daemon(config: &Path, startup_lock: StartupLock) -> Result<()> {
    let (ready_parent, ready_child) =
        StdUnixStream::pair().context("failed to create sandbox daemon readiness channel")?;
    startup_lock.make_inheritable()?;
    make_inheritable(ready_child.as_raw_fd())
        .context("failed to inherit sandbox daemon readiness channel")?;
    let executable = std::env::current_exe().context("failed to resolve sandbox executable")?;
    let mut command = Command::new(executable);
    command
        .arg("__session-daemon")
        .arg("--config")
        .arg(config)
        .arg("--ready-fd")
        .arg(ready_child.as_raw_fd().to_string())
        .arg("--startup-lock-fd")
        .arg(startup_lock.descriptor().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command
        .spawn()
        .context("failed to start sandbox session daemon")?;
    drop(ready_child);
    drop(startup_lock);
    ready_parent
        .set_nonblocking(true)
        .context("failed to configure sandbox daemon readiness channel")?;
    let mut ready_parent = UnixStream::from_std(ready_parent)
        .context("failed to register sandbox daemon readiness channel")?;
    let readiness = tokio::time::timeout(
        STARTUP_TIMEOUT,
        read_frame::<_, DaemonReadiness>(&mut ready_parent),
    )
    .await;
    match readiness {
        Ok(Ok(DaemonReadiness::Ready)) => Ok(()),
        Ok(Ok(DaemonReadiness::Failed { message })) => {
            let _ = child.wait();
            bail!("sandbox session daemon failed to start: {message}")
        }
        Ok(Err(error)) => {
            let status = child.try_wait().ok().flatten();
            Err(error).with_context(|| match status {
                Some(status) => format!("sandbox session daemon exited with {status}"),
                None => "sandbox session daemon closed its readiness channel".to_string(),
            })
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("sandbox session daemon startup timed out")
        }
    }
}

pub(crate) fn build_identity() -> Result<String> {
    let executable = std::env::current_exe()
        .context("failed to resolve sandbox executable")?
        .canonicalize()
        .context("failed to canonicalize sandbox executable")?;
    let mut file = File::open(&executable)
        .with_context(|| format!("failed to open sandbox executable {}", executable.display()))?;
    let mut digest = DigestContext::new(&SHA256);
    digest.update(b"agora-sandbox-session-build-v1");
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", executable.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex(digest.finish().as_ref()))
}

pub(crate) fn inherited_descriptor(raw: libc::c_int, name: &str) -> Result<OwnedFd> {
    if raw < 0 {
        bail!("invalid inherited {name} descriptor")
    }
    let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };
    let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to inspect inherited {name} descriptor"));
    }
    if unsafe {
        libc::fcntl(
            descriptor.as_raw_fd(),
            libc::F_SETFD,
            flags | libc::FD_CLOEXEC,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to secure inherited {name} descriptor"));
    }
    Ok(descriptor)
}

fn session_socket_directory() -> Result<PathBuf> {
    let path = PathBuf::from(format!("/tmp/agora-sandbox-session-{}", unsafe {
        libc::geteuid()
    }));
    loop {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_dir()
                    || metadata.file_type().is_symlink()
                    || metadata.uid() != unsafe { libc::geteuid() }
                {
                    bail!("invalid sandbox session directory: {}", path.display());
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::create_dir(&path) {
                    Ok(()) => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to create session directory {}", path.display())
                        });
                    }
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect session directory {}", path.display())
                });
            }
        }
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to secure session directory {}", path.display()))?;
    Ok(path)
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() } =>
        {
            std::fs::remove_file(path).with_context(|| {
                format!("failed to remove stale session socket {}", path.display())
            })
        }
        Ok(_) => bail!("invalid sandbox session socket: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect session socket {}", path.display())),
    }
}

fn retryable_connect_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

fn make_inheritable(descriptor: libc::c_int) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}
