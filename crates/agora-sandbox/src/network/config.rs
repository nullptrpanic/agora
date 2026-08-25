use anyhow::{Result, bail};
use std::time::Duration;

const DEFAULT_MAX_CONNECTIONS: usize = 256;
const DEFAULT_DOMAIN_INSPECTION_TIMEOUT: Duration = Duration::from_millis(500);
const DEFAULT_CALLBACK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetworkEnforcement {
    #[default]
    Intercept,
    Strict,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TlsMode {
    #[default]
    Off,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetworkConfig {
    pub enforcement: NetworkEnforcement,
    pub tls: TlsMode,
    pub domain_inspection_timeout: Duration,
    pub callback_timeout: Duration,
    pub upstream_connect_timeout: Duration,
    pub max_connections: usize,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            enforcement: NetworkEnforcement::Intercept,
            tls: TlsMode::Off,
            domain_inspection_timeout: DEFAULT_DOMAIN_INSPECTION_TIMEOUT,
            callback_timeout: DEFAULT_CALLBACK_TIMEOUT,
            upstream_connect_timeout: Duration::from_secs(10),
            max_connections: DEFAULT_MAX_CONNECTIONS,
        }
    }
}

impl NetworkConfig {
    pub fn validate(self) -> Result<()> {
        if self.enforcement == NetworkEnforcement::Strict {
            bail!("strict network enforcement is unavailable until native egress denial is active");
        }
        if self.upstream_connect_timeout.is_zero() {
            bail!("upstream_connect_timeout must be greater than zero");
        }
        if self.domain_inspection_timeout.is_zero() {
            bail!("domain_inspection_timeout must be greater than zero");
        }
        if self.callback_timeout.is_zero() {
            bail!("callback_timeout must be greater than zero");
        }
        if self.max_connections == 0 {
            bail!("max_connections must be greater than zero");
        }
        Ok(())
    }
}
