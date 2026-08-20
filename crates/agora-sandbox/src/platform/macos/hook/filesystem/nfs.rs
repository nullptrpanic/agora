use crate::filesystem::{ByteRange, FileAttributes};
use crate::nfs::client::{RemoteClient, RemoteClientError, decode_json_descriptor};
use crate::nfs::protocol::{
    MAX_REMOTE_DIRECTORY_ENTRIES, MAX_REMOTE_DIRECTORY_PAYLOAD_BYTES, RemoteEntry, RemoteFileType,
    RemoteMetadata, RemotePath, RemoteRoute, Request, Response,
};
use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::ipc::InheritedControlStream;

pub(super) struct RemoteFilesystem {
    client: RemoteClient,
    routes: Vec<Route>,
    runtime: PathBuf,
}

struct Route {
    root: u32,
    logical_root: PathBuf,
}

pub(super) struct RoutedPath {
    logical: PathBuf,
    remote: RemotePath,
}

pub(super) struct RemoteOpen {
    client: RemoteClient,
    target: Option<super::OpenTarget>,
    handle: Option<String>,
    metadata: RemoteMetadata,
    writable: bool,
    truncate: bool,
}

pub(super) struct RemoteDirectoryView {
    logical: PathBuf,
    anchor: RemoteAnchor,
    entries: Vec<RemoteEntry>,
}

pub(super) struct RemoteAnchor {
    path: CString,
    physical: PathBuf,
}

impl RemoteFilesystem {
    #[cfg(test)]
    pub(super) fn from_json(
        socket: impl Into<PathBuf>,
        token: impl Into<String>,
        routes: &str,
    ) -> Result<Self> {
        Self::from_json_with_shared(socket, token, routes, None)
    }

    pub(super) fn from_json_with_shared(
        socket: impl Into<PathBuf>,
        token: impl Into<String>,
        routes: &str,
        shared: Option<Arc<InheritedControlStream<UnixStream>>>,
    ) -> Result<Self> {
        let routes = serde_json::from_str(routes).context("invalid remote filesystem routes")?;
        Self::new_with_shared(socket, token, routes, shared)
    }

    #[cfg(test)]
    pub(super) fn new(
        socket: impl Into<PathBuf>,
        token: impl Into<String>,
        routes: Vec<RemoteRoute>,
    ) -> Result<Self> {
        Self::new_with_shared(socket, token, routes, None)
    }

    fn new_with_shared(
        socket: impl Into<PathBuf>,
        token: impl Into<String>,
        routes: Vec<RemoteRoute>,
        shared: Option<Arc<InheritedControlStream<UnixStream>>>,
    ) -> Result<Self> {
        let socket = socket.into();
        if !socket.is_absolute() {
            bail!("remote filesystem socket must be absolute");
        }
        let token = token.into();
        if token.is_empty() {
            bail!("remote filesystem token is empty");
        }
        if routes.is_empty() {
            bail!("remote filesystem routes are empty");
        }
        let mut normalized = Vec::with_capacity(routes.len());
        for route in routes {
            let root = normalize_root(Path::new(&route.logical_root))?;
            if normalized.iter().any(|existing: &Route| {
                existing.logical_root.starts_with(&root) || root.starts_with(&existing.logical_root)
            }) {
                bail!("remote filesystem routes overlap");
            }
            normalized.push(Route {
                root: route.root,
                logical_root: root,
            });
        }
        let runtime = socket
            .parent()
            .context("remote filesystem socket has no parent")?;
        let runtime = runtime
            .canonicalize()
            .unwrap_or_else(|_| runtime.to_path_buf());
        let client = shared.map_or_else(
            || RemoteClient::new(&socket, &token),
            |stream| RemoteClient::with_shared(&socket, &token, stream),
        );
        Ok(Self {
            client,
            routes: normalized,
            runtime,
        })
    }

    #[cfg(test)]
    pub(super) fn route(&self, path: &Path) -> Option<RoutedPath> {
        self.route_result(path).ok().flatten()
    }

    pub(super) fn route_result(&self, path: &Path) -> Result<Option<RoutedPath>> {
        let path = normalize_request_path(path)?;
        let Some(route) = self
            .routes
            .iter()
            .find(|route| path.starts_with(&route.logical_root))
        else {
            return Ok(None);
        };
        let relative = path
            .strip_prefix(&route.logical_root)
            .context("failed to strip remote filesystem root")?;
        let relative = relative
            .to_str()
            .context("remote filesystem paths must be valid UTF-8")?
            .trim_start_matches('/')
            .to_string();
        Ok(Some(RoutedPath {
            logical: path,
            remote: RemotePath::new(route.root, relative)?,
        }))
    }

    pub(super) fn route_root_names(&self, parent: &Path) -> Result<Vec<Vec<u8>>> {
        let parent = normalize_request_path(parent)?;
        Ok(self
            .routes
            .iter()
            .filter(|route| route.logical_root.parent() == Some(parent.as_path()))
            .filter_map(|route| route.logical_root.file_name())
            .map(|name| name.as_bytes().to_vec())
            .collect())
    }

    pub(super) fn restore_current_directory(
        &self,
        native: &Path,
        logical: &Path,
    ) -> Result<Option<PathBuf>> {
        if !self.is_anchor(native) {
            return Ok(None);
        }
        let routed = self
            .route_result(logical)?
            .context("inherited remote current directory is outside configured routes")?;
        Ok(Some(routed.logical))
    }

    pub(super) fn open(
        &self,
        path: &RoutedPath,
        flags: libc::c_int,
        mode: u32,
    ) -> Result<RemoteOpen> {
        let reply = self.request(Request::Open {
            path: path.remote.clone(),
            flags,
            mode,
        })?;
        let Response::Open { handle, metadata } = reply.response else {
            return Err(protocol_error(
                "remote open returned an unexpected response",
            ));
        };
        let descriptor = reply
            .descriptor
            .context("remote open response did not include a descriptor")?;
        Ok(RemoteOpen {
            client: self.client.clone(),
            target: Some(super::OpenTarget::Descriptor(descriptor.into())),
            handle: Some(handle),
            metadata,
            writable: flags & libc::O_ACCMODE != libc::O_RDONLY,
            truncate: flags & libc::O_TRUNC != 0,
        })
    }

    pub(super) fn stat(&self, path: &RoutedPath) -> Result<RemoteMetadata> {
        let (metadata, anchor) = self.stat_reply(path)?;
        drop(self.anchor(&anchor)?);
        Ok(metadata)
    }

    pub(super) fn stat_plan(
        &self,
        path: &RoutedPath,
    ) -> Result<(
        RemoteAnchor,
        Option<libc::off_t>,
        FileAttributes,
        RemoteMetadata,
    )> {
        let (metadata, anchor) = self.stat_reply(path)?;
        let (anchor, size, attributes) = self.metadata_plan(&anchor, &metadata)?;
        Ok((anchor, size, attributes, metadata))
    }

    fn stat_reply(&self, path: &RoutedPath) -> Result<(RemoteMetadata, String)> {
        let reply = self.request(Request::Stat {
            path: path.remote.clone(),
            name_capacity: logical_name_capacity(&path.logical)?,
        })?;
        match reply.response {
            Response::Stat { metadata, anchor } => Ok((metadata, anchor)),
            _ => Err(protocol_error(
                "remote stat returned an unexpected response",
            )),
        }
    }

    fn list(&self, path: &RoutedPath) -> Result<(Vec<RemoteEntry>, RemoteAnchor)> {
        let mut reply = self.request(Request::List {
            path: path.remote.clone(),
            name_capacity: logical_name_capacity(&path.logical)?,
        })?;
        match reply.response {
            Response::List { anchor } => {
                let anchor = self.anchor(&anchor)?;
                let descriptor = reply.descriptor.take().ok_or_else(|| {
                    protocol_error("remote list response did not include a descriptor")
                })?;
                let entries = decode_list_descriptor(descriptor)?;
                Ok((validate_entries(entries)?, anchor))
            }
            _ => Err(protocol_error(
                "remote list returned an unexpected response",
            )),
        }
    }

    pub(super) fn directory_view(&self, path: &RoutedPath) -> Result<RemoteDirectoryView> {
        let (entries, anchor) = self.list(path)?;
        Ok(RemoteDirectoryView {
            logical: path.logical.clone(),
            anchor,
            entries,
        })
    }

    pub(super) fn access(&self, path: &RoutedPath, mode: libc::c_int) -> Result<()> {
        self.expect_success(Request::Access {
            path: path.remote.clone(),
            mode,
        })
    }

    pub(super) fn sync(
        &self,
        handle: &str,
        ranges: Vec<ByteRange>,
    ) -> Result<Option<RemoteMetadata>> {
        request_sync(&self.client, handle, ranges)
    }

    pub(super) fn metadata(&self, handle: &str) -> Result<RemoteMetadata> {
        let reply = self.request(Request::Metadata {
            handle: handle.to_string(),
        })?;
        match reply.response {
            Response::Metadata { metadata } => Ok(metadata),
            _ => Err(protocol_error(
                "remote metadata returned an unexpected response",
            )),
        }
    }

    pub(super) fn read(&self, handle: &str, offset: u64, length: u32) -> Result<(OwnedFd, u32)> {
        let mut reply = self.request(Request::Read {
            handle: handle.to_string(),
            offset,
            length,
        })?;
        match reply.response {
            Response::Read { length, .. } => Ok((
                reply
                    .descriptor
                    .take()
                    .context("remote read response did not include a descriptor")?,
                length,
            )),
            _ => Err(protocol_error(
                "remote read returned an unexpected response",
            )),
        }
    }

    pub(super) fn write(
        &self,
        handle: &str,
        offset: Option<u64>,
        payload: &File,
        length: u32,
    ) -> Result<(u64, u32, u64)> {
        let checksum = checksum_payload(payload, length)?;
        let reply = self
            .client
            .request_with_descriptor(
                Request::Write {
                    handle: handle.to_string(),
                    offset,
                    length,
                    checksum,
                },
                payload.as_raw_fd(),
            )
            .map_err(client_error)?;
        match reply.response {
            Response::Written {
                offset,
                length,
                size,
            } => Ok((offset, length, size)),
            _ => Err(protocol_error(
                "remote write returned an unexpected response",
            )),
        }
    }

    pub(super) fn set_length(&self, handle: &str, length: u64) -> Result<u64> {
        request_set_length(&self.client, handle, length)
    }

    pub(super) fn materialize(
        &self,
        handle: &str,
        range: Option<ByteRange>,
    ) -> Result<RemoteMetadata> {
        let reply = self.request(Request::Materialize {
            handle: handle.to_string(),
            range,
        })?;
        match reply.response {
            Response::Materialized { metadata } => Ok(metadata),
            _ => Err(protocol_error(
                "remote materialization returned an unexpected response",
            )),
        }
    }

    pub(super) fn potentially_dirty(&self, handle: &str, range: ByteRange) -> Result<()> {
        self.expect_success(Request::PotentiallyDirty {
            handle: handle.to_string(),
            range,
        })
    }

    pub(super) fn close(&self, handle: &str, ranges: Vec<ByteRange>) -> Result<()> {
        self.expect_success(Request::Close {
            handle: handle.to_string(),
            ranges,
        })
    }

    pub(super) fn create_directory(&self, path: &RoutedPath, mode: libc::mode_t) -> Result<()> {
        self.expect_success(Request::CreateDirectory {
            path: path.remote.clone(),
            mode: u32::from(mode),
        })
    }

    pub(super) fn remove(&self, path: &RoutedPath, directory: bool) -> Result<()> {
        self.expect_success(Request::Remove {
            path: path.remote.clone(),
            directory,
        })
    }

    pub(super) fn rename(&self, from: &RoutedPath, to: &RoutedPath) -> Result<()> {
        if from.remote.root() != to.remote.root() {
            return Err(std::io::Error::from_raw_os_error(libc::EXDEV).into());
        }
        self.expect_success(Request::Rename {
            from: from.remote.clone(),
            to: to.remote.clone(),
        })
    }

    pub(super) fn metadata_plan(
        &self,
        anchor: &str,
        metadata: &RemoteMetadata,
    ) -> Result<(RemoteAnchor, Option<libc::off_t>, FileAttributes)> {
        let path = self.anchor(anchor)?;
        let size =
            libc::off_t::try_from(metadata.size).context("remote filesystem file is too large")?;
        Ok((
            path,
            (metadata.file_type == RemoteFileType::File).then_some(size),
            self.attributes(metadata),
        ))
    }

    pub(super) fn attributes(&self, metadata: &RemoteMetadata) -> FileAttributes {
        let mode = match metadata.file_type {
            RemoteFileType::File => u32::from(libc::S_IFREG) | 0o666,
            RemoteFileType::Directory => u32::from(libc::S_IFDIR) | 0o777,
        };
        FileAttributes {
            mode,
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            atime: metadata.modified_seconds,
            atime_nsec: i64::from(metadata.modified_nanoseconds),
            mtime: metadata.modified_seconds,
            mtime_nsec: i64::from(metadata.modified_nanoseconds),
        }
    }

    fn anchor(&self, anchor: &str) -> Result<RemoteAnchor> {
        let path = Path::new(anchor);
        let mut components = path.components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(protocol_error(
                "remote filesystem returned an invalid anchor",
            ));
        }
        RemoteAnchor::new(self.runtime.join(path))
    }

    fn is_anchor(&self, path: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        let same_runtime = parent == self.runtime
            || parent
                .canonicalize()
                .ok()
                .is_some_and(|parent| parent == self.runtime);
        if !same_runtime {
            return false;
        }
        path.file_name()
            .and_then(|name| name.as_bytes().strip_prefix(b"anchor-"))
            .is_some_and(|identifier| {
                let Some((uuid, padding)) = identifier.split_at_checked(32) else {
                    return false;
                };
                uuid.iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
                    && padding.iter().all(|byte| *byte == b'x')
            })
    }

    fn expect_success(&self, request: Request) -> Result<()> {
        let reply = self.request(request)?;
        match reply.response {
            Response::Success => Ok(()),
            _ => Err(protocol_error(
                "remote filesystem mutation returned an unexpected response",
            )),
        }
    }

    fn request(&self, request: Request) -> Result<crate::nfs::client::RemoteReply> {
        self.client.request(request).map_err(client_error)
    }
}

fn checksum_payload(file: &File, length: u32) -> Result<[u8; 16]> {
    let actual = file
        .metadata()
        .context("failed to inspect remote write payload")?
        .len();
    if actual != u64::from(length) {
        bail!("remote write payload length did not match its request");
    }
    let mut digest = Md5::new();
    let mut offset = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    while offset < actual {
        let requested = usize::try_from((actual - offset).min(buffer.len() as u64))
            .context("remote write payload chunk overflowed")?;
        let read = file
            .read_at(&mut buffer[..requested], offset)
            .context("failed to read remote write payload")?;
        if read == 0 {
            bail!("remote write payload ended before its declared length");
        }
        digest.update(&buffer[..read]);
        offset += read as u64;
    }
    Ok(digest.finalize().into())
}

fn request_sync(
    client: &RemoteClient,
    handle: &str,
    ranges: Vec<ByteRange>,
) -> Result<Option<RemoteMetadata>> {
    let reply = client
        .request(Request::Sync {
            handle: handle.to_string(),
            ranges,
        })
        .map_err(client_error)?;
    match reply.response {
        Response::Synced { metadata } => Ok(metadata),
        _ => Err(protocol_error(
            "remote filesystem sync returned an unexpected response",
        )),
    }
}

fn request_set_length(client: &RemoteClient, handle: &str, length: u64) -> Result<u64> {
    let reply = client
        .request(Request::SetLength {
            handle: handle.to_string(),
            length,
        })
        .map_err(client_error)?;
    match reply.response {
        Response::Resized { size } => Ok(size),
        _ => Err(protocol_error(
            "remote resize returned an unexpected response",
        )),
    }
}

fn logical_name_capacity(path: &Path) -> Result<u16> {
    u16::try_from(
        path.file_name()
            .map(|name| name.as_bytes().len())
            .unwrap_or_default(),
    )
    .context("remote logical file name is too long")
}

impl RemoteOpen {
    pub(super) fn set_length(&mut self, length: u64) -> Result<()> {
        let handle = self
            .handle
            .as_ref()
            .context("remote open handle was already consumed")?;
        let size = request_set_length(&self.client, handle, length)?;
        let super::OpenTarget::Descriptor(file) = self.target_mut() else {
            return Err(protocol_error("remote open did not return a descriptor"));
        };
        file.set_len(size)
            .context("failed to resize remote placeholder")?;
        self.metadata.size = size;
        Ok(())
    }

    pub(super) fn commit(&mut self) -> Result<()> {
        let handle = self
            .handle
            .as_ref()
            .context("remote open handle was already consumed")?;
        if !self.truncate {
            return Ok(());
        }
        let metadata = request_sync(&self.client, handle, Vec::new())?;
        if let Some(metadata) = metadata {
            self.metadata = metadata;
        }
        Ok(())
    }

    pub(super) fn target(&self) -> &super::OpenTarget {
        self.target
            .as_ref()
            .expect("remote open target was already consumed")
    }

    pub(super) fn target_mut(&mut self) -> &mut super::OpenTarget {
        self.target
            .as_mut()
            .expect("remote open target was already consumed")
    }

    pub(super) fn into_parts(mut self) -> (super::OpenTarget, String, RemoteMetadata, bool) {
        let target = self
            .target
            .take()
            .expect("remote open target was already consumed");
        let handle = self
            .handle
            .take()
            .expect("remote open handle was already consumed");
        (target, handle, self.metadata.clone(), self.writable)
    }
}

impl Drop for RemoteOpen {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.client.request(Request::Abort { handle });
        }
    }
}

fn decode_list_descriptor(descriptor: OwnedFd) -> Result<Vec<RemoteEntry>> {
    let descriptor = File::from(descriptor);
    let length = descriptor
        .metadata()
        .context("failed to inspect remote list payload")?
        .len();
    if length > MAX_REMOTE_DIRECTORY_PAYLOAD_BYTES {
        return Err(anyhow::Error::new(std::io::Error::from_raw_os_error(
            libc::EOVERFLOW,
        )))
        .context("remote directory exceeds the sandbox listing limit");
    }
    decode_json_descriptor(descriptor.into())
        .map_err(|_| protocol_error("remote list payload is invalid"))
}

impl RoutedPath {
    pub(super) fn logical(&self) -> &Path {
        &self.logical
    }

    #[cfg(test)]
    pub(super) fn remote(&self) -> &RemotePath {
        &self.remote
    }
}

impl RemoteDirectoryView {
    pub(super) fn logical(&self) -> &Path {
        &self.logical
    }

    pub(super) fn anchor(&self) -> &CStr {
        &self.anchor.path
    }

    pub(super) fn into_entries(self) -> Vec<RemoteEntry> {
        self.entries
    }
}

impl RemoteAnchor {
    fn new(physical: PathBuf) -> Result<Self> {
        let path = CString::new(physical.as_os_str().as_bytes())
            .context("remote filesystem anchor contains NUL")?;
        Ok(Self { path, physical })
    }

    pub(super) fn path(&self) -> &CStr {
        &self.path
    }

    pub(super) fn adopt(path: &Path) -> Result<Self> {
        Self::new(path.to_path_buf())
    }
}

impl Drop for RemoteAnchor {
    fn drop(&mut self) {
        let _ =
            std::fs::remove_file(&self.physical).or_else(|_| std::fs::remove_dir(&self.physical));
    }
}

fn normalize_root(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() || path == Path::new("/") {
        bail!("remote filesystem root must be an absolute non-root path");
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir | Component::Prefix(_) => {
                bail!("remote filesystem root is not normalized")
            }
        }
    }
    Ok(normalized)
}

fn normalize_request_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("remote filesystem request path must be absolute");
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) => bail!("unsupported remote filesystem request path"),
        }
    }
    Ok(normalized)
}

fn validate_entries(entries: Vec<RemoteEntry>) -> Result<Vec<RemoteEntry>> {
    validate_entry_count(entries.len())?;
    let mut seen = HashSet::with_capacity(entries.len());
    for entry in &entries {
        if entry.name.is_empty()
            || entry.name == "."
            || entry.name == ".."
            || entry.name.contains(['/', '\\', '\0'])
            || !seen.insert(entry.name.as_str())
        {
            return Err(protocol_error("remote directory contains an invalid entry"));
        }
    }
    Ok(entries)
}

fn validate_entry_count(count: usize) -> Result<()> {
    if count > MAX_REMOTE_DIRECTORY_ENTRIES {
        return Err(anyhow::Error::new(std::io::Error::from_raw_os_error(
            libc::EOVERFLOW,
        )))
        .context("remote directory exceeds the sandbox listing limit");
    }
    Ok(())
}

fn client_error(error: RemoteClientError) -> anyhow::Error {
    anyhow::Error::new(std::io::Error::from_raw_os_error(error.errno())).context(error.to_string())
}

fn protocol_error(message: &str) -> anyhow::Error {
    anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EPROTO)).context(message.to_string())
}

#[cfg(test)]
mod tests;
