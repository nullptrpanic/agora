//! Configuration adapter for the `agora-sandbox` binary.

use agora_sandbox::network::TlsMode;
use agora_sandbox::runner::{SandboxConfig, SmbRemoteConfig};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

const DEFAULT_LOG_FILE: &str = "runtime/logs/sandbox.log";

pub(super) struct RunConfig {
    workdir: PathBuf,
    tls: TlsMode,
    local: LocalFilesystem,
    native_passthrough_roots: Vec<PathBuf>,
    remotes: Vec<SmbRemoteConfig>,
    log_file: PathBuf,
    identity_seed: [u8; 32],
}

impl RunConfig {
    pub(super) fn load(path: &Path) -> Result<Self> {
        let path = absolute_path(path)?;
        let file = open_config(&path)?;
        let stored: StoredConfig = serde_json::from_reader(file)
            .with_context(|| format!("failed to parse sandbox config {}", path.display()))?;
        let directory = path.parent().unwrap_or(Path::new("/"));
        stored.resolve(directory)
    }

    pub(super) fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub(super) fn log_file(&self) -> &Path {
        &self.log_file
    }

    pub(super) fn session_identity(&self, hook: &Path) -> Result<String> {
        let hook = hook
            .canonicalize()
            .with_context(|| format!("failed to resolve sandbox hook {}", hook.display()))?;
        let mut file = File::open(&hook)
            .with_context(|| format!("failed to open sandbox hook {}", hook.display()))?;
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        digest.update(b"agora-sandbox-session-identity-v1");
        digest.update(&self.identity_seed);
        update_digest_field(&mut digest, hook.as_os_str().as_bytes());
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .with_context(|| format!("failed to read sandbox hook {}", hook.display()))?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        Ok(hex(digest.finish().as_ref()))
    }

    pub(super) fn into_runtime(self, hook: PathBuf) -> SandboxConfig {
        let mut config = SandboxConfig::new(hook).with_workdir(&self.workdir);
        config.network.tls = self.tls;
        config = match self.local {
            LocalFilesystem::Plain => config.with_plain_workspace(),
            LocalFilesystem::Encrypted(key) => config.with_encrypted_workspace(key),
        };
        for root in self.native_passthrough_roots {
            config = config.with_native_passthrough_root(root);
        }
        for remote in self.remotes {
            config = config.with_smb_remote(remote);
        }
        config
    }
}

enum LocalFilesystem {
    Plain,
    Encrypted(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredConfig {
    #[serde(default)]
    workdir: Option<PathBuf>,
    #[serde(default)]
    tls: StoredTlsMode,
    #[serde(default)]
    filesystem: StoredFilesystem,
    #[serde(default)]
    log: StoredLog,
}

impl StoredConfig {
    fn resolve(self, directory: &Path) -> Result<RunConfig> {
        let local = match (self.filesystem.local.encrypt, self.filesystem.local.key) {
            (StoredEncryption::Plain, None) => LocalFilesystem::Plain,
            (StoredEncryption::Plain, Some(_)) => {
                bail!("filesystem.local.key is not allowed when encrypt is plain")
            }
            (StoredEncryption::Encrypted, Some(key)) if !key.is_empty() => {
                LocalFilesystem::Encrypted(key)
            }
            (StoredEncryption::Encrypted, _) => {
                bail!("filesystem.local.key is required when encrypt is encrypted")
            }
        };
        let workdir = self
            .workdir
            .map(|path| resolve_path(directory, &path))
            .transpose()?
            .unwrap_or_else(SandboxConfig::default_workdir);
        let workdir = normalize_absolute_path(&workdir)?;
        let identity_workdir = canonicalize_with_missing(&workdir)?;
        let log_file = self
            .log
            .file
            .map(|path| resolve_path(&workdir, &path))
            .transpose()?
            .unwrap_or_else(|| workdir.join(DEFAULT_LOG_FILE));
        let log_file = normalize_absolute_path(&log_file)?;
        let identity_log_file = canonicalize_with_missing(&log_file)?;
        let tls: TlsMode = self.tls.into();
        let mut identity = IdentityBuilder::new();
        identity.path(&identity_workdir);
        identity.path(&identity_log_file);
        identity.byte(match tls {
            TlsMode::Off => 0,
            TlsMode::Auto => 1,
        });
        match &local {
            LocalFilesystem::Plain => identity.byte(0),
            LocalFilesystem::Encrypted(key) => {
                identity.byte(1);
                identity.field(key.as_bytes());
            }
        }
        let mut native_passthrough_roots = self
            .filesystem
            .bypass
            .into_iter()
            .map(|root| {
                if !root.is_absolute() {
                    bail!(
                        "filesystem bypass root must be absolute: {}",
                        root.display()
                    );
                }
                normalize_absolute_path(&root)
            })
            .collect::<Result<Vec<_>>>()?;
        native_passthrough_roots.sort();
        native_passthrough_roots.dedup();
        identity.usize(native_passthrough_roots.len());
        for root in &native_passthrough_roots {
            identity.path(root);
        }
        let remotes = self.filesystem.nfs;
        identity.usize(remotes.len());
        let remotes = remotes
            .into_iter()
            .map(|remote| remote.resolve(&mut identity))
            .collect::<Result<Vec<_>>>()?;
        Ok(RunConfig {
            workdir,
            tls,
            local,
            native_passthrough_roots,
            remotes,
            log_file,
            identity_seed: identity.finish(),
        })
    }
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StoredTlsMode {
    #[default]
    Off,
    Auto,
}

impl From<StoredTlsMode> for TlsMode {
    fn from(value: StoredTlsMode) -> Self {
        match value {
            StoredTlsMode::Off => Self::Off,
            StoredTlsMode::Auto => Self::Auto,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFilesystem {
    #[serde(default)]
    bypass: Vec<PathBuf>,
    #[serde(default)]
    local: StoredLocalFilesystem,
    #[serde(default)]
    nfs: Vec<StoredRemote>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredLocalFilesystem {
    #[serde(default)]
    encrypt: StoredEncryption,
    #[serde(default)]
    key: Option<String>,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StoredEncryption {
    #[default]
    Plain,
    Encrypted,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum StoredRemote {
    Smb {
        dir: PathBuf,
        server: String,
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: String,
    },
}

impl StoredRemote {
    fn resolve(self, identity: &mut IdentityBuilder) -> Result<SmbRemoteConfig> {
        match self {
            Self::Smb {
                dir,
                server,
                username,
                password,
            } => {
                let remote = smb_remote(dir, &server, username, password.clone())?;
                identity.field(b"smb");
                identity.path(remote.logical_root());
                identity.field(remote.server().as_bytes());
                identity.field(remote.share().as_bytes());
                identity.field(remote.remote_path().as_bytes());
                identity.field(remote.domain().as_bytes());
                identity.field(remote.username().as_bytes());
                identity.field(password.as_bytes());
                Ok(remote)
            }
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredLog {
    #[serde(default)]
    file: Option<PathBuf>,
}

fn smb_remote(
    dir: PathBuf,
    uri: &str,
    username: String,
    password: String,
) -> Result<SmbRemoteConfig> {
    let location = uri
        .strip_prefix("smb://")
        .context("filesystem.nfs SMB server must start with 'smb://'")?;
    if location.contains(['?', '#', '@']) {
        bail!("filesystem.nfs SMB server contains unsupported URI components");
    }
    let (server, path) = location
        .split_once('/')
        .context("filesystem.nfs SMB server must include a share")?;
    if server.is_empty() {
        bail!("filesystem.nfs SMB server endpoint is empty");
    }
    let (share, remote_path) = path.split_once('/').unwrap_or((path, ""));
    if share.is_empty() {
        bail!("filesystem.nfs SMB share is empty");
    }
    Ok(SmbRemoteConfig::new(dir, server, share)?
        .with_remote_path(remote_path)?
        .with_credentials(username, password))
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .context("failed to resolve current directory")?
        .join(path))
}

fn resolve_path(directory: &Path, path: &Path) -> Result<PathBuf> {
    if path == Path::new("~") || path.starts_with("~/") {
        let home = std::env::var_os("HOME").context("HOME is required to expand '~'")?;
        let suffix = path.strip_prefix("~").expect("checked tilde prefix");
        return Ok(PathBuf::from(home).join(suffix));
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(directory.join(path))
    }
}

fn canonicalize_with_missing(path: &Path) -> Result<PathBuf> {
    let normalized = normalize_absolute_path(path)?;
    let mut missing = Vec::new();
    let mut ancestor = normalized.as_path();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = ancestor.file_name().with_context(|| {
                    format!("failed to resolve sandbox path {}", path.display())
                })?;
                missing.push(name.to_os_string());
                ancestor = ancestor.parent().with_context(|| {
                    format!("failed to resolve sandbox path {}", path.display())
                })?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to resolve sandbox path {}", path.display()));
            }
        }
    }
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("sandbox path is not absolute: {}", path.display());
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
            Component::Prefix(_) => bail!("unsupported sandbox path: {}", path.display()),
        }
    }
    Ok(normalized)
}

struct IdentityBuilder(ring::digest::Context);

impl IdentityBuilder {
    fn new() -> Self {
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        digest.update(b"agora-sandbox-effective-config-v1");
        Self(digest)
    }

    fn byte(&mut self, value: u8) {
        self.field(&[value]);
    }

    fn usize(&mut self, value: usize) {
        self.field(&(value as u64).to_be_bytes());
    }

    fn path(&mut self, value: &Path) {
        self.field(value.as_os_str().as_bytes());
    }

    fn field(&mut self, value: &[u8]) {
        update_digest_field(&mut self.0, value);
    }

    fn finish(self) -> [u8; 32] {
        self.0
            .finish()
            .as_ref()
            .try_into()
            .expect("SHA-256 digest has 32 bytes")
    }
}

fn update_digest_field(digest: &mut ring::digest::Context, value: &[u8]) {
    digest.update(&(value.len() as u64).to_be_bytes());
    digest.update(value);
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

fn open_config(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("failed to open sandbox config {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to verify sandbox config {}", path.display()))?;
    if !metadata.is_file() {
        bail!("sandbox config is not a regular file: {}", path.display());
    }
    Ok(file)
}

#[cfg(test)]
mod tests;
