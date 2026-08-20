//! Broker protocol shared by the sandbox hook and parent process.

use crate::filesystem::ByteRange;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub(crate) const PROTOCOL_VERSION: u16 = 9;
pub(crate) const MAX_REMOTE_IO_BYTES: u32 = 64 * 1024;
#[cfg(not(agora_sandbox_hook_build))]
pub(crate) const MAX_REMOTE_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub(crate) const MAX_REMOTE_DIRECTORY_ENTRIES: usize = 100_000;
pub(crate) const MAX_REMOTE_DIRECTORY_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(not(agora_sandbox_hook_build))]
pub(crate) const REMOTE_OPERATION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(15 * 60);
#[cfg(not(agora_sandbox_hook_build))]
pub(crate) const REMOTE_RESET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
pub(crate) const REMOTE_CLIENT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(15 * 60 + 5);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RequestEnvelope {
    pub(crate) version: u16,
    pub(crate) token: String,
    pub(crate) request_id: RequestId,
    pub(crate) request: Request,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResponseEnvelope {
    pub(crate) version: u16,
    pub(crate) request_id: RequestId,
    pub(crate) response: Response,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize)]
pub(crate) struct RequestId(String);

impl RequestId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!("invalid remote request ID");
        }
        Ok(Self(value))
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub(crate) enum Request {
    Ping,
    Open {
        path: RemotePath,
        flags: i32,
        mode: u32,
    },
    Stat {
        path: RemotePath,
        name_capacity: u16,
    },
    List {
        path: RemotePath,
        name_capacity: u16,
    },
    Access {
        path: RemotePath,
        mode: i32,
    },
    Metadata {
        handle: String,
    },
    Read {
        handle: String,
        offset: u64,
        length: u32,
    },
    Write {
        handle: String,
        offset: Option<u64>,
        length: u32,
        checksum: [u8; 16],
    },
    SetLength {
        handle: String,
        length: u64,
    },
    Materialize {
        handle: String,
        range: Option<ByteRange>,
    },
    PotentiallyDirty {
        handle: String,
        range: ByteRange,
    },
    Sync {
        handle: String,
        ranges: Vec<ByteRange>,
    },
    Close {
        handle: String,
        ranges: Vec<ByteRange>,
    },
    Abort {
        handle: String,
    },
    Claim {
        request_id: RequestId,
    },
    CreateDirectory {
        path: RemotePath,
        mode: u32,
    },
    Remove {
        path: RemotePath,
        directory: bool,
    },
    Rename {
        from: RemotePath,
        to: RemotePath,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum Response {
    Success,
    Open {
        handle: String,
        metadata: RemoteMetadata,
    },
    Metadata {
        metadata: RemoteMetadata,
    },
    Read {
        payload: String,
        length: u32,
    },
    Written {
        offset: u64,
        length: u32,
        size: u64,
    },
    Resized {
        size: u64,
    },
    Materialized {
        metadata: RemoteMetadata,
    },
    Synced {
        metadata: Option<RemoteMetadata>,
    },
    Stat {
        metadata: RemoteMetadata,
        anchor: String,
    },
    List {
        anchor: String,
    },
    Error {
        errno: i32,
        message: String,
    },
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize)]
pub(crate) struct RemotePath {
    root: u32,
    path: String,
}

impl RemotePath {
    pub(crate) fn new(root: u32, path: impl Into<String>) -> Result<Self> {
        let path = path.into();
        if path.starts_with('/') || path.contains('\\') || path.as_bytes().contains(&0) {
            bail!("invalid remote path");
        }
        if !path.is_empty() {
            for component in path.split('/') {
                if component.is_empty() || component == "." || component == ".." {
                    bail!("remote path is not normalized");
                }
            }
        }
        Ok(Self { root, path })
    }

    pub(crate) fn root(&self) -> u32 {
        self.root
    }

    #[cfg(any(feature = "remote-smb", test))]
    pub(crate) fn path(&self) -> &str {
        &self.path
    }
}

impl<'de> Deserialize<'de> for RemotePath {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WirePath {
            root: u32,
            path: String,
        }

        let path = WirePath::deserialize(deserializer)?;
        Self::new(path.root, path.path).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteFileType {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RemoteMetadata {
    pub(crate) file_type: RemoteFileType,
    pub(crate) size: u64,
    pub(crate) modified_seconds: i64,
    pub(crate) modified_nanoseconds: u32,
    pub(crate) identity: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RemoteEntry {
    pub(crate) name: String,
    pub(crate) metadata: RemoteMetadata,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RemoteRoute {
    pub(crate) root: u32,
    pub(crate) logical_root: String,
}

#[cfg(test)]
mod tests;
