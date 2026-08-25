use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::future::Future;
use std::net::IpAddr;

pub const EVENT_SCHEMA_VERSION: u16 = 9;

pub trait Callback: Send + Sync + 'static {
    fn on_event(&self, event: Event) -> impl Future<Output = Decision> + Send;
}

impl<F, Fut> Callback for F
where
    F: Fn(Event) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Decision> + Send,
{
    fn on_event(&self, event: Event) -> impl Future<Output = Decision> + Send {
        self(event)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopCallback;

impl Callback for NoopCallback {
    fn on_event(&self, _event: Event) -> impl Future<Output = Decision> + Send {
        std::future::ready(Decision::Allow)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Network(NetworkEvent),
    Process(ProcessEvent),
    File(FileEvent),
}

impl Event {
    pub fn as_network(&self) -> Option<&NetworkEvent> {
        match self {
            Self::Network(event) => Some(event),
            Self::Process(_) | Self::File(_) => None,
        }
    }

    pub fn into_network(self) -> Option<NetworkEvent> {
        match self {
            Self::Network(event) => Some(event),
            Self::Process(_) | Self::File(_) => None,
        }
    }
}

impl Redact for Event {}

impl Serialize for Redacted<'_, Event> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Event::Network(event) => event.redacted().serialize(serializer),
            Event::Process(event) => event.serialize(serializer),
            Event::File(event) => event.serialize(serializer),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny { reason: Option<String> },
    Proxy { proxy: Proxy },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Proxy {
    Http(HttpProxy),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct HttpProxy {
    pub address: String,
    pub basic_auth: Option<BasicAuth>,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct BasicAuth {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for BasicAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BasicAuth")
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .finish()
    }
}

pub trait Redact {
    fn redacted(&self) -> Redacted<'_, Self>
    where
        Self: Sized,
    {
        Redacted(self)
    }
}

pub struct Redacted<'a, T: ?Sized>(&'a T);

impl Redact for BasicAuth {}
impl Redact for Decision {}
impl Redact for Proxy {}

impl Serialize for Redacted<'_, BasicAuth> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("BasicAuth", 2)?;
        state.serialize_field("username", &self.0.username)?;
        state.serialize_field("password", "[redacted]")?;
        state.end()
    }
}

impl Serialize for Redacted<'_, Proxy> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Proxy::Http(proxy) => {
                let mut state = serializer.serialize_struct("Proxy", 3)?;
                state.serialize_field("type", "http")?;
                state.serialize_field("address", &proxy.address)?;
                state.serialize_field(
                    "basic_auth",
                    &proxy.basic_auth.as_ref().map(Redact::redacted),
                )?;
                state.end()
            }
        }
    }
}

impl Serialize for Redacted<'_, Decision> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Decision::Allow => {
                let mut state = serializer.serialize_struct("Decision", 1)?;
                state.serialize_field("action", "allow")?;
                state.end()
            }
            Decision::Deny { reason } => {
                let mut state = serializer.serialize_struct("Decision", 2)?;
                state.serialize_field("action", "deny")?;
                state.serialize_field("reason", reason)?;
                state.end()
            }
            Decision::Proxy { proxy } => {
                let mut state = serializer.serialize_struct("Decision", 2)?;
                state.serialize_field("action", "proxy")?;
                state.serialize_field("proxy", &proxy.redacted())?;
                state.end()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct NetworkEvent {
    pub schema_version: u16,
    pub event_id: String,
    pub occurred_at: String,
    pub subsystem: Subsystem,
    pub event_type: EventType,
    pub sandbox_id: String,
    pub run_id: String,
    pub trace_id: String,
    pub connection_id: Option<String>,
    pub sequence: Option<u64>,
    pub process: ProcessContext,
    pub network: Option<NetworkContext>,
    pub tls: Option<TlsContext>,
    pub decision: Option<Decision>,
    pub result: EventResult,
    pub metrics: Option<EventMetrics>,
}

impl Redact for NetworkEvent {}

impl Serialize for Redacted<'_, NetworkEvent> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let event = self.0;
        let mut state = serializer.serialize_struct("NetworkEvent", 17)?;
        state.serialize_field("schema_version", &event.schema_version)?;
        state.serialize_field("event_id", &event.event_id)?;
        state.serialize_field("occurred_at", &event.occurred_at)?;
        state.serialize_field("subsystem", &event.subsystem)?;
        state.serialize_field("event_type", &event.event_type)?;
        state.serialize_field("sandbox_id", &event.sandbox_id)?;
        state.serialize_field("run_id", &event.run_id)?;
        state.serialize_field("trace_id", &event.trace_id)?;
        state.serialize_field("connection_id", &event.connection_id)?;
        state.serialize_field("sequence", &event.sequence)?;
        state.serialize_field("process", &event.process)?;
        state.serialize_field("network", &event.network)?;
        state.serialize_field("tls", &event.tls)?;
        state.serialize_field("decision", &event.decision.as_ref().map(Redact::redacted))?;
        state.serialize_field("result", &event.result)?;
        state.serialize_field("metrics", &event.metrics)?;
        state.end()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subsystem {
    Network,
    Filesystem,
    Process,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventType {
    #[serde(rename = "network.connect.attempt")]
    NetworkConnectAttempt,
    #[serde(rename = "network.connect.denied")]
    NetworkConnectDenied,
    #[serde(rename = "network.connect.established")]
    NetworkConnectEstablished,
    #[serde(rename = "network.connect.failed")]
    NetworkConnectFailed,
    #[serde(rename = "network.connection.closed")]
    NetworkConnectionClosed,
    #[serde(rename = "filesystem.open")]
    FilesystemOpen,
    #[serde(rename = "filesystem.close")]
    FilesystemClose,
    #[serde(rename = "process.started")]
    ProcessStarted,
    #[serde(rename = "process.exec.attempt")]
    ProcessExecAttempt,
    #[serde(rename = "process.exited")]
    ProcessExited,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessContext {
    pub pid: u32,
    pub ppid: u32,
    pub executable: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessEvent {
    pub schema_version: u16,
    pub event_id: String,
    pub occurred_at: String,
    pub subsystem: Subsystem,
    pub event_type: EventType,
    pub sandbox_id: String,
    pub run_id: String,
    pub trace_id: String,
    pub process: ProcessContext,
    pub command: CommandContext,
    pub result: EventResult,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEvent {
    pub schema_version: u16,
    pub event_id: String,
    pub occurred_at: String,
    pub subsystem: Subsystem,
    pub event_type: EventType,
    pub sandbox_id: String,
    pub run_id: String,
    pub trace_id: String,
    pub process: ProcessContext,
    pub file: FileContext,
    pub result: EventResult,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileContext {
    pub path: String,
    pub mode: FileOpenMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOpenMode {
    pub access: FileAccessMode,
    pub create: bool,
    pub truncate: bool,
    pub append: bool,
    pub exclusive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAccessMode {
    Read,
    Write,
    ReadWrite,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandContext {
    pub executable: String,
    pub arguments: Vec<String>,
    pub current_dir: String,
    pub operation: ProcessOperation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOperation {
    PosixSpawn,
    PosixSpawnp,
    Execve,
    Execv,
    Execvp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkContext {
    pub protocol: NetworkProtocol,
    pub destination_ip: IpAddr,
    pub destination_port: u16,
    #[serde(flatten)]
    pub target: Option<Box<NetworkTarget>>,
    pub http_host: Option<String>,
    pub tls_sni: Option<String>,
    pub domain: Option<String>,
    pub domain_source: Option<DomainSource>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkTarget {
    #[serde(rename = "target_host")]
    pub host: String,
    #[serde(rename = "target_port")]
    pub port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkProtocol {
    Tcp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainSource {
    HttpHost,
    TlsSni,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsContext {
    pub policy: TlsPolicy,
    pub outcome: TlsOutcome,
    pub alpn: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsPolicy {
    Off,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsOutcome {
    NotAttempted,
    Terminated,
    Passthrough,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventResult {
    pub status: EventStatus,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    Started,
    Succeeded,
    Failed,
    Denied,
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventMetrics {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub duration_ms: u64,
}
