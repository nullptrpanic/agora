//! Bounded control protocol for one workspace session.

use crate::runner::{PreparedLaunch, ProtectedEnvironment};
use anyhow::{Result, bail};
use base64::Engine;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum DaemonReadiness {
    Ready,
    Failed { message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WireOsString(Vec<u8>);

impl WireOsString {
    pub(crate) fn into_os_string(self) -> OsString {
        OsString::from_vec(self.0)
    }
}

impl From<OsString> for WireOsString {
    fn from(value: OsString) -> Self {
        Self(value.into_vec())
    }
}

impl From<&OsStr> for WireOsString {
    fn from(value: &OsStr) -> Self {
        Self(value.as_bytes().to_vec())
    }
}

impl Serialize for WireOsString {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for WireOsString {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ClientMessage {
    Join {
        protocol: u32,
        build: String,
        config: String,
    },
    Prepare {
        executable: WireOsString,
    },
    Finished {
        launch_id: String,
    },
    Cancel {
        launch_id: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ServerMessage {
    Joined { sandbox_id: String, run_id: String },
    Prepared { launch: WirePreparedLaunch },
    Released,
    RuntimeFailed { message: String },
    Retiring { message: String },
    Rejected { message: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct WirePreparedLaunch {
    launch_id: String,
    program: WireOsString,
    argument_prefix: Vec<WireOsString>,
    additions: Vec<(WireOsString, WireOsString)>,
    removals: Vec<WireOsString>,
}

impl From<&PreparedLaunch> for WirePreparedLaunch {
    fn from(value: &PreparedLaunch) -> Self {
        Self {
            launch_id: value.launch_id().to_owned(),
            program: value.program().as_os_str().into(),
            argument_prefix: value
                .argument_prefix()
                .iter()
                .map(|value| value.as_os_str().into())
                .collect(),
            additions: value
                .environment()
                .additions()
                .iter()
                .map(|(key, value)| (key.as_os_str().into(), value.as_os_str().into()))
                .collect(),
            removals: value
                .environment()
                .removals()
                .iter()
                .map(|value| value.as_os_str().into())
                .collect(),
        }
    }
}

impl WirePreparedLaunch {
    pub(crate) fn launch_id(&self) -> &str {
        &self.launch_id
    }

    pub(crate) fn into_prepared(self) -> Result<PreparedLaunch> {
        let additions = self
            .additions
            .into_iter()
            .map(|(key, value)| (key.into_os_string(), value.into_os_string()))
            .collect::<BTreeMap<_, _>>();
        let removals = self
            .removals
            .into_iter()
            .map(WireOsString::into_os_string)
            .collect();
        let program = PathBuf::from(self.program.into_os_string());
        if !program.is_absolute() {
            bail!("prepared sandbox executable is not absolute");
        }
        Ok(PreparedLaunch::new(
            program,
            self.argument_prefix
                .into_iter()
                .map(WireOsString::into_os_string)
                .collect(),
            ProtectedEnvironment::from_parts(additions, removals),
            self.launch_id,
        ))
    }
}

pub(crate) async fn write_frame<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value).map_err(invalid_data)?;
    if payload.len() > crate::ipc::MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sandbox session frame is too large",
        ));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame is too large"))?;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}

pub(crate) async fn read_frame<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > crate::ipc::MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sandbox session frame is too large",
        ));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload).map_err(invalid_data)
}

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
