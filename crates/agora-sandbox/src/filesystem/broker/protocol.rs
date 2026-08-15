use anyhow::{Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

pub(crate) use crate::filesystem::ByteRange;

pub(crate) const PROTOCOL_VERSION: u16 = 8;

pub(crate) fn valid_request_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RequestEnvelope {
    pub(crate) version: u16,
    pub(crate) token: String,
    pub(crate) request_id: String,
    pub(crate) request: Request,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResponseEnvelope {
    pub(crate) version: u16,
    pub(crate) request_id: String,
    pub(crate) response: Response,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub(crate) enum Request {
    Ping,
    Open {
        path: BackingPath,
        flags: libc::c_int,
    },
    Materialize {
        handle: String,
        range: Option<ByteRange>,
    },
    Sync {
        handle: String,
        ranges: Vec<ByteRange>,
        durable: bool,
    },
    PotentiallyDirty {
        handle: String,
        range: ByteRange,
    },
    BeginWrite {
        handle: String,
        write_id: String,
        range: ByteRange,
    },
    BeginAppend {
        handle: String,
        write_id: String,
    },
    FinishWrite {
        handle: String,
        write_id: String,
        range: ByteRange,
    },
    CancelWrite {
        handle: String,
        write_id: String,
    },
    Claim {
        request_id: String,
    },
    Abort {
        handle: String,
    },
    Retain {
        handles: Vec<String>,
    },
    ReleaseRetain {
        handles: Vec<String>,
    },
    Close {
        handle: String,
        ranges: Vec<ByteRange>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum Response {
    Success,
    Open {
        handle: String,
        device: u64,
        inode: u64,
        links: u64,
        lazy: bool,
    },
    Offset {
        offset: u64,
    },
    Error {
        errno: i32,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct BackingPath(String);

impl BackingPath {
    pub(crate) fn from_path(path: &Path) -> Self {
        Self(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(path.as_os_str().as_bytes()))
    }

    pub(crate) fn to_path(&self) -> Result<PathBuf> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&self.0)
            .map_err(|error| anyhow::anyhow!("invalid local filesystem backing path: {error}"))?;
        if bytes.contains(&0) {
            bail!("local filesystem backing path contains NUL");
        }
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
}

impl<'de> Deserialize<'de> for BackingPath {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let path = Self(String::deserialize(deserializer)?);
        path.to_path().map_err(serde::de::Error::custom)?;
        Ok(path)
    }
}
