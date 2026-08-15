//! SMB backend configuration.

use anyhow::{Result, bail};
use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf};

#[derive(Clone)]
pub struct SmbRemoteConfig {
    logical_root: PathBuf,
    server: String,
    share: String,
    remote_path: String,
    domain: String,
    username: String,
    password: SecretString,
}

impl SmbRemoteConfig {
    pub fn new(
        logical_root: impl AsRef<Path>,
        server: impl Into<String>,
        share: impl Into<String>,
    ) -> Result<Self> {
        let logical_root = normalize_logical_root(logical_root.as_ref())?;
        let server = normalize_server(server.into())?;
        let share = share.into();
        if share.is_empty() || share == "." || share == ".." || share.contains(['/', '\\']) {
            bail!("SMB share name is invalid");
        }
        Ok(Self {
            logical_root,
            server,
            share,
            remote_path: String::new(),
            domain: String::new(),
            username: String::new(),
            password: SecretString::default(),
        })
    }

    pub fn with_remote_path(mut self, path: impl AsRef<str>) -> Result<Self> {
        self.remote_path = normalize_remote_path(path.as_ref())?;
        Ok(self)
    }

    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = domain.into();
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = username.into();
        self.password = SecretString(password.into());
        self
    }

    pub fn logical_root(&self) -> &Path {
        &self.logical_root
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn share(&self) -> &str {
        &self.share
    }

    pub fn remote_path(&self) -> &str {
        &self.remote_path
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    #[cfg(feature = "remote-smb")]
    pub(super) fn password(&self) -> &str {
        &self.password.0
    }
}

impl fmt::Debug for SmbRemoteConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SmbRemoteConfig")
            .field("logical_root", &self.logical_root)
            .field("server", &self.server)
            .field("share", &self.share)
            .field("remote_path", &self.remote_path)
            .field("domain", &self.domain)
            .field("username", &self.username)
            .field("password", &self.password)
            .finish()
    }
}

#[cfg_attr(not(feature = "remote-smb"), allow(dead_code))]
#[derive(Clone, Default)]
struct SecretString(String);

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

fn normalize_logical_root(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("SMB logical root must be absolute: {}", path.display());
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                bail!("SMB logical root cannot contain '..': {}", path.display())
            }
            Component::Prefix(_) => bail!("unsupported SMB logical root: {}", path.display()),
        }
    }
    if normalized == Path::new("/") {
        bail!("SMB logical root cannot be '/'");
    }
    Ok(normalized)
}

fn normalize_server(server: String) -> Result<String> {
    let server = server.trim();
    if server.is_empty() || server.contains(['/', '\\']) {
        bail!("SMB server is invalid");
    }
    if let Ok(address) = server.parse::<std::net::SocketAddr>() {
        if address.is_ipv6() {
            bail!("SMB server IPv6 literals are unsupported");
        }
        return Ok(server.to_owned());
    }
    if server.starts_with('[') && server.ends_with(']') {
        if server[1..server.len() - 1]
            .parse::<std::net::Ipv6Addr>()
            .is_ok()
        {
            bail!("SMB server IPv6 literals are unsupported");
        }
        return Ok(format!("{server}:445"));
    }
    if server.parse::<std::net::Ipv6Addr>().is_ok() {
        bail!("SMB server IPv6 literals are unsupported");
    }
    if server.contains(':') {
        if server
            .rsplit_once(':')
            .is_some_and(|(_, port)| !port.is_empty() && port.parse::<u16>().is_ok())
        {
            return Ok(server.to_owned());
        }
        bail!("SMB server endpoint is invalid");
    }
    Ok(format!("{server}:445"))
}

fn normalize_remote_path(path: &str) -> Result<String> {
    if path.contains('\\') || path.as_bytes().contains(&0) {
        bail!("SMB remote path is invalid");
    }
    let mut components = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => components.push(value),
            Component::ParentDir => bail!("SMB remote path cannot contain '..'"),
            Component::Prefix(_) => bail!("unsupported SMB remote path"),
        }
    }
    components
        .into_iter()
        .map(OsStr::to_str)
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.join("/"))
        .ok_or_else(|| anyhow::anyhow!("SMB remote path must be valid UTF-8"))
}
