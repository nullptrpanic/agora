use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

pub mod generate;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<HttpProxy>,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    pub channels: Vec<ChannelConfig>,
    pub agents: Vec<AgentConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(default = "default_max_in_flight_tasks")]
    pub max_in_flight_tasks: usize,
    #[serde(default = "default_max_in_flight_runs")]
    pub max_in_flight_runs: usize,
    #[serde(default = "default_max_concurrent_runs")]
    pub max_concurrent_runs: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_in_flight_tasks: default_max_in_flight_tasks(),
            max_in_flight_runs: default_max_in_flight_runs(),
            max_concurrent_runs: default_max_concurrent_runs(),
        }
    }
}

fn default_max_in_flight_tasks() -> usize {
    32
}

fn default_max_in_flight_runs() -> usize {
    64
}

fn default_max_concurrent_runs() -> usize {
    4
}

impl NodeConfig {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        if self.runtime.max_in_flight_tasks == 0 {
            anyhow::bail!("runtime max_in_flight_tasks must be positive");
        }
        if self.runtime.max_in_flight_runs == 0 {
            anyhow::bail!("runtime max_in_flight_runs must be positive");
        }
        if self.runtime.max_concurrent_runs == 0 {
            anyhow::bail!("runtime max_concurrent_runs must be positive");
        }
        if self.runtime.max_concurrent_runs > self.runtime.max_in_flight_runs {
            anyhow::bail!("runtime max_concurrent_runs must not exceed max_in_flight_runs");
        }

        for (name, value) in [
            ("max_in_flight_tasks", self.runtime.max_in_flight_tasks),
            ("max_in_flight_runs", self.runtime.max_in_flight_runs),
            ("max_concurrent_runs", self.runtime.max_concurrent_runs),
        ] {
            if value > tokio::sync::Semaphore::MAX_PERMITS {
                anyhow::bail!("runtime {name} exceeds Semaphore::MAX_PERMITS");
            }
        }
        let mut channel_accounts = HashSet::new();
        let mut channel_names = HashSet::new();
        for channel in &self.channels {
            let name = channel.name();
            if name.trim().is_empty() {
                anyhow::bail!("channel name must not be empty");
            }
            if !channel_names.insert(name) {
                anyhow::bail!("duplicate channel name: {name}");
            }
            channel.validate()?;
            let account = match channel {
                ChannelConfig::Lark(config) => ("lark", config.app_id.trim().to_string()),
                ChannelConfig::Telegram(config) => {
                    ("telegram", config.bot_id()?.parse::<u64>()?.to_string())
                }
            };
            if !channel_accounts.insert(account) {
                anyhow::bail!("duplicate provider account: channel {name}");
            }
        }

        let mut agent_names = HashSet::new();
        for agent in &self.agents {
            if agent.name.trim().is_empty() {
                anyhow::bail!("agent name must not be empty");
            }
            if agent.name.chars().any(char::is_whitespace) {
                anyhow::bail!("agent name must not contain whitespace: {}", agent.name);
            }
            if !agent_names.insert(agent.name.as_str()) {
                anyhow::bail!("duplicate agent name: {}", agent.name);
            }
            if agent.path.trim().is_empty() {
                anyhow::bail!("agent path must not be empty: {}", agent.name);
            }
            if !Path::new(&agent.workspace).is_absolute() {
                anyhow::bail!("agent workspace must be absolute: {}", agent.name);
            }
            if agent.timeout_seconds == 0 {
                anyhow::bail!("agent timeout_seconds must be positive: {}", agent.name);
            }
            if agent.max_output_bytes == 0 {
                anyhow::bail!("agent max_output_bytes must be positive: {}", agent.name);
            }
            for subscription in &agent.subscribe {
                let has_filter_conditions = subscription.filter.as_ref().is_some_and(
                    |filter| !matches!(filter, Value::Object(fields) if fields.is_empty()),
                );
                if has_filter_conditions {
                    anyhow::bail!(
                        "agent subscription filter is not implemented: {} -> {}",
                        agent.name,
                        subscription.channel
                    );
                }
                if !channel_names.contains(subscription.channel.as_str()) {
                    anyhow::bail!(
                        "agent subscription references an unknown channel: {} -> {}",
                        agent.name,
                        subscription.channel
                    );
                }
            }
        }

        for channel in &self.channels {
            let required = self
                .agents
                .iter()
                .filter(|agent| {
                    agent
                        .subscribe
                        .iter()
                        .any(|subscription| subscription.channel == channel.name())
                })
                .count();
            if required > self.runtime.max_in_flight_runs {
                anyhow::bail!(
                    "runtime max_in_flight_runs cannot admit channel fan-out: {} requires {}, limit is {}",
                    channel.name(),
                    required,
                    self.runtime.max_in_flight_runs
                );
            }
        }
        Ok(())
    }

    pub(crate) fn apply_proxy_defaults(&mut self) {
        let Some(proxy) = &self.proxy else {
            return;
        };
        for agent in &mut self.agents {
            agent.proxy.get_or_insert_with(|| proxy.clone());
        }
        for channel in &mut self.channels {
            channel.proxy_mut().get_or_insert_with(|| proxy.clone());
        }
    }

    #[cfg(test)]
    pub(crate) fn validate_filesystem(&self) -> anyhow::Result<()> {
        for agent in &self.agents {
            resolve_agent_executable(&agent.name, Path::new(&agent.path))?;
        }
        Ok(())
    }
}

pub(crate) fn resolve_agent_executable(
    name: &str,
    configured_path: &Path,
) -> anyhow::Result<PathBuf> {
    let path = resolve_executable(configured_path).ok_or_else(|| {
        anyhow::anyhow!(
            "agent executable does not exist: {}: {}",
            name,
            configured_path.display()
        )
    })?;
    let metadata = std::fs::metadata(&path).map_err(|error| {
        anyhow::anyhow!(
            "inspect agent executable failed: {}: {}: {}",
            name,
            path.display(),
            error
        )
    })?;
    if !metadata.is_file() {
        anyhow::bail!(
            "agent executable is not a regular file: {}: {}",
            name,
            path.display()
        );
    }
    if !is_executable(&metadata) {
        anyhow::bail!(
            "agent executable is not executable: {}: {}",
            name,
            path.display()
        );
    }
    Ok(std::path::absolute(path)?)
}

fn resolve_executable(path: &Path) -> Option<PathBuf> {
    let is_bare_name = !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        && path.components().count() == 1;
    if !is_bare_name {
        return path.exists().then(|| path.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|search_path| find_in_search_path(path, &search_path))
}

fn find_in_search_path(path: &Path, search_path: &std::ffi::OsStr) -> Option<PathBuf> {
    executable_names(path.as_os_str()).find_map(|name| {
        std::env::split_paths(search_path)
            .map(|directory| directory.join(&name))
            .find(|candidate| {
                std::fs::metadata(candidate)
                    .is_ok_and(|metadata| metadata.is_file() && is_executable(&metadata))
            })
    })
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    true
}

#[cfg(not(windows))]
fn executable_names(name: &std::ffi::OsStr) -> impl Iterator<Item = std::ffi::OsString> {
    [name.to_os_string()].into_iter()
}

#[cfg(windows)]
fn executable_names(name: &std::ffi::OsStr) -> impl Iterator<Item = std::ffi::OsString> {
    let extensions = std::env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
    std::iter::once(name.to_os_string()).chain(
        extensions
            .to_string_lossy()
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| {
                std::ffi::OsString::from(format!("{}{extension}", name.to_string_lossy()))
            })
            .collect::<Vec<_>>(),
    )
}

fn encode_proxy_credential(value: &str) -> String {
    let mut url = reqwest::Url::parse("http://proxy.invalid").expect("static HTTP URL");
    // URL setters retain percent escapes; escape literal percent signs first.
    url.set_username(&value.replace('%', "%25"))
        .expect("HTTP URL supports credentials");
    url.username().to_string()
}

fn decode_proxy_credential(value: &str) -> Result<String, String> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let mut lookahead = bytes.clone();
            if let Some((high, low)) = lookahead.next().and_then(|high| {
                Some((
                    (high as char).to_digit(16)?,
                    (lookahead.next()? as char).to_digit(16)?,
                ))
            }) {
                decoded.push((high * 16 + low) as u8);
                bytes = lookahead;
                continue;
            }
        }
        decoded.push(byte);
    }
    String::from_utf8(decoded).map_err(|_| "proxy credentials are not valid UTF-8".to_string())
}

impl Serialize for HttpProxy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.environment_value())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    pub isolate: IsolateMode,
    #[serde(default = "default_workspace")]
    pub workspace: String,
    #[serde(rename = "type")]
    pub agent_type: AgentType,
    pub path: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub agent_sandbox: Option<AgentSandbox>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<HttpProxy>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
    pub subscribe: Vec<AgentSubscription>,
}

fn default_timeout_seconds() -> u64 {
    3600
}

fn default_max_output_bytes() -> usize {
    64 * 1024 * 1024
}

fn default_workspace() -> String {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".agora")
        .join("workspace")
        .to_string_lossy()
        .into_owned()
}

impl AgentConfig {
    pub fn isolation_scope(
        &self,
        channel_name: impl Into<String>,
        session_id: impl Into<String>,
    ) -> IsolationScope {
        match self.isolate {
            IsolateMode::None => IsolationScope::Shared,
            IsolateMode::Session => IsolationScope::session(channel_name, session_id),
        }
    }

    pub fn workdir(&self) -> PathBuf {
        PathBuf::from(&self.workspace)
    }
}

impl TelegramChannelConfig {
    pub(crate) fn bot_id(&self) -> anyhow::Result<&str> {
        let (bot_id, secret) = self.token.split_once(':').ok_or_else(|| {
            anyhow::anyhow!(
                "telegram token must have the form <bot-id>:<secret>: {}",
                self.name
            )
        })?;
        let numeric_id = bot_id.parse::<u64>().map_err(|_| {
            anyhow::anyhow!(
                "telegram token has an invalid numeric bot id: {}",
                self.name
            )
        })?;
        if numeric_id == 0 || secret.trim().is_empty() {
            anyhow::bail!(
                "telegram token has an empty bot id or secret: {}",
                self.name
            );
        }
        Ok(bot_id)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentSubscription {
    pub channel: String,
    #[serde(default)]
    pub filter: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelConfig {
    Lark(LarkChannelConfig),
    Telegram(TelegramChannelConfig),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LarkChannelConfig {
    pub name: String,
    pub app_id: String,
    pub secret: String,
    #[serde(default)]
    pub permission: ChannelPermissionConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<HttpProxy>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TelegramChannelConfig {
    pub name: String,
    pub token: String,
    #[serde(default)]
    pub permission: ChannelPermissionConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<HttpProxy>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelPermissionConfig {
    #[serde(default)]
    pub users: Vec<ChannelUserPermissionConfig>,
    #[serde(default)]
    pub groups: Vec<ChannelGroupPermissionConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelUserPermissionConfig {
    pub id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelGroupPermissionConfig {
    pub id: String,
    #[serde(default)]
    pub require_mention: bool,
}

impl ChannelConfig {
    pub fn name(&self) -> &str {
        match self {
            ChannelConfig::Lark(config) => &config.name,
            ChannelConfig::Telegram(config) => &config.name,
        }
    }

    fn proxy_mut(&mut self) -> &mut Option<HttpProxy> {
        match self {
            ChannelConfig::Lark(config) => &mut config.proxy,
            ChannelConfig::Telegram(config) => &mut config.proxy,
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        let permission = match self {
            ChannelConfig::Lark(config) => {
                if config.app_id.trim().is_empty() {
                    anyhow::bail!("lark app_id must not be empty: {}", config.name);
                }
                if config.secret.trim().is_empty() {
                    anyhow::bail!("lark secret must not be empty: {}", config.name);
                }
                &config.permission
            }
            ChannelConfig::Telegram(config) => {
                if config.token.trim().is_empty() {
                    anyhow::bail!("telegram token must not be empty: {}", config.name);
                }
                config.bot_id()?;
                &config.permission
            }
        };
        if permission
            .users
            .iter()
            .any(|user| user.id.trim().is_empty())
        {
            anyhow::bail!(
                "channel user permission id must not be empty: {}",
                self.name()
            );
        }
        if permission
            .groups
            .iter()
            .any(|group| group.id.trim().is_empty())
        {
            anyhow::bail!(
                "channel group permission id must not be empty: {}",
                self.name()
            );
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct HttpProxy {
    address: String,
    credentials: Option<(String, String)>,
}

impl HttpProxy {
    pub fn environment_value(&self) -> String {
        match &self.credentials {
            Some((username, password)) => {
                format!(
                    "http://{}:{}@{}",
                    encode_proxy_credential(username),
                    encode_proxy_credential(password),
                    self.address
                )
            }
            None => format!("http://{}", self.address),
        }
    }

    pub(crate) fn address(&self) -> &str {
        &self.address
    }

    pub(crate) fn credentials(&self) -> Option<(&str, &str)> {
        self.credentials
            .as_ref()
            .map(|(username, password)| (username.as_str(), password.as_str()))
    }
}

impl fmt::Debug for HttpProxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpProxy")
            .field("address", &self.address)
            .field("authenticated", &self.credentials.is_some())
            .finish()
    }
}

impl FromStr for HttpProxy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.strip_prefix("http://").unwrap_or(value);
        if value.contains("://") {
            return Err("proxy must use HTTP".to_string());
        }
        let (credentials, address) = match value.rsplit_once('@') {
            Some((credentials, address)) => {
                let (username, password) = credentials
                    .split_once(':')
                    .ok_or_else(|| "proxy credentials must use user:password".to_string())?;
                (
                    Some((
                        decode_proxy_credential(username)?,
                        decode_proxy_credential(password)?,
                    )),
                    address,
                )
            }
            None => (None, value),
        };
        let (host, port) = address
            .rsplit_once(':')
            .ok_or_else(|| "proxy address must include a port".to_string())?;
        let valid_host = !host.is_empty()
            && !host.chars().any(char::is_whitespace)
            && !host.contains('/')
            && !host.contains('@')
            && (host.starts_with('[') == host.ends_with(']'))
            && (!host.contains(':') || (host.starts_with('[') && host.ends_with(']')));
        if !valid_host {
            return Err("proxy host is invalid".to_string());
        }
        if port.parse::<u16>().ok().filter(|port| *port > 0).is_none() {
            return Err("proxy port is invalid".to_string());
        }
        let endpoint = reqwest::Url::parse(&format!("http://{address}"))
            .map_err(|_| "proxy address is invalid".to_string())?;
        if endpoint.path() != "/" || endpoint.query().is_some() || endpoint.fragment().is_some() {
            return Err("proxy address is invalid".to_string());
        }
        Ok(Self {
            address: address.to_string(),
            credentials,
        })
    }
}

impl<'de> Deserialize<'de> for HttpProxy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IsolateMode {
    None,
    Session,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum IsolationScope {
    Shared,
    Session {
        channel_name: String,
        session_id: String,
    },
}

impl IsolationScope {
    pub fn session(channel_name: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self::Session {
            channel_name: channel_name.into(),
            session_id: session_id.into(),
        }
    }

    pub fn channel_name(&self) -> Option<&str> {
        match self {
            Self::Shared => None,
            Self::Session { channel_name, .. } => Some(channel_name),
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Shared => None,
            Self::Session { session_id, .. } => Some(session_id),
        }
    }

    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Session { .. } => "session",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    Codex,
    Custom,
}

impl AgentType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Custom => "custom",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AgentSandbox {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl AgentSandbox {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

#[cfg(test)]
mod tests;
