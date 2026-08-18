use crate::network::client_trust::JAVA_TRUST_STORE_ENVIRONMENT;
use crate::trace::{TRACE_ID_ENVIRONMENT, TraceContext};
use base64::Engine;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static CONFIG: OnceLock<Result<Option<HookConfig>, String>> = OnceLock::new();

const TOKEN: &str = "AGORA_SANDBOX_TOKEN";
const PROXY_IPV4: &str = "AGORA_SANDBOX_PROXY_IPV4";
const PROXY_IPV6: &str = "AGORA_SANDBOX_PROXY_IPV6";
const EXECUTION_CONTROL: &str = "AGORA_SANDBOX_EXECUTION_CONTROL";
const EXECUTION_TOKEN: &str = "AGORA_SANDBOX_EXECUTION_TOKEN";
const AUDIT_CONTROL: &str = "AGORA_SANDBOX_AUDIT_CONTROL";
const AUDIT_TOKEN: &str = "AGORA_SANDBOX_AUDIT_TOKEN";
pub(super) const CONTROL_LOCK_DESCRIPTOR: &str = "AGORA_SANDBOX_CONTROL_LOCK_FD";
pub(super) const EXECUTION_CONTROL_DESCRIPTOR: &str = "AGORA_SANDBOX_EXECUTION_FD";
pub(super) const AUDIT_CONTROL_DESCRIPTOR: &str = "AGORA_SANDBOX_AUDIT_FD";
pub(super) const LOCAL_CONTROL_DESCRIPTOR: &str = "AGORA_SANDBOX_LOCAL_FILESYSTEM_FD";
pub(super) const REMOTE_CONTROL_DESCRIPTOR: &str = "AGORA_SANDBOX_REMOTE_FD";
const HOOK_LIBRARIES: &str = "AGORA_SANDBOX_HOOK_LIBRARIES";
const FILESYSTEM_ROOT: &str = "AGORA_SANDBOX_FILESYSTEM_ROOT";
const FILESYSTEM_MODE: &str = "AGORA_SANDBOX_FILESYSTEM_MODE";
const NATIVE_PASSTHROUGH_ROOTS: &str = "AGORA_SANDBOX_NATIVE_PASSTHROUGH_ROOTS";
const FILESYSTEM_CIPHER_KEY: &str = "AGORA_SANDBOX_FILESYSTEM_CIPHER_KEY";
const LOCAL_FILESYSTEM_CONTROL: &str = "AGORA_SANDBOX_LOCAL_FILESYSTEM_CONTROL";
const LOCAL_FILESYSTEM_TOKEN: &str = "AGORA_SANDBOX_LOCAL_FILESYSTEM_TOKEN";
pub(super) const INHERITED_LOCAL_DESCRIPTORS: &str = "AGORA_SANDBOX_INHERITED_LOCAL_DESCRIPTORS";
const REMOTE_CONTROL: &str = "AGORA_SANDBOX_REMOTE_CONTROL";
const REMOTE_TOKEN: &str = "AGORA_SANDBOX_REMOTE_TOKEN";
const REMOTE_ROOTS: &str = "AGORA_SANDBOX_REMOTE_ROOTS";
pub(super) const REMOTE_CURRENT_DIRECTORY: &str = "AGORA_SANDBOX_REMOTE_CURRENT_DIRECTORY";
const TLS_TRUST_ANCHOR_DER: &str = "AGORA_SANDBOX_TLS_TRUST_ANCHOR_DER";
const TLS_TRUST_BUNDLE: &str = "AGORA_SANDBOX_TLS_TRUST_BUNDLE";

const TLS_CLIENT_TRUST_ENVIRONMENT: [&str; 6] = [
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
    "PIP_CERT",
    "NODE_EXTRA_CA_CERTS",
    "GIT_SSL_CAINFO",
];

pub(super) const CHILD_RUNTIME_ENVIRONMENT: [&str; 34] = [
    TOKEN,
    PROXY_IPV4,
    PROXY_IPV6,
    EXECUTION_CONTROL,
    EXECUTION_TOKEN,
    AUDIT_CONTROL,
    AUDIT_TOKEN,
    CONTROL_LOCK_DESCRIPTOR,
    EXECUTION_CONTROL_DESCRIPTOR,
    AUDIT_CONTROL_DESCRIPTOR,
    LOCAL_CONTROL_DESCRIPTOR,
    REMOTE_CONTROL_DESCRIPTOR,
    HOOK_LIBRARIES,
    FILESYSTEM_ROOT,
    FILESYSTEM_MODE,
    NATIVE_PASSTHROUGH_ROOTS,
    FILESYSTEM_CIPHER_KEY,
    LOCAL_FILESYSTEM_CONTROL,
    LOCAL_FILESYSTEM_TOKEN,
    INHERITED_LOCAL_DESCRIPTORS,
    REMOTE_CONTROL,
    REMOTE_TOKEN,
    REMOTE_ROOTS,
    REMOTE_CURRENT_DIRECTORY,
    TLS_TRUST_ANCHOR_DER,
    TLS_TRUST_BUNDLE,
    JAVA_TRUST_STORE_ENVIRONMENT,
    TRACE_ID_ENVIRONMENT,
    TLS_CLIENT_TRUST_ENVIRONMENT[0],
    TLS_CLIENT_TRUST_ENVIRONMENT[1],
    TLS_CLIENT_TRUST_ENVIRONMENT[2],
    TLS_CLIENT_TRUST_ENVIRONMENT[3],
    TLS_CLIENT_TRUST_ENVIRONMENT[4],
    TLS_CLIENT_TRUST_ENVIRONMENT[5],
];

#[derive(Clone, Debug)]
pub(super) struct HookConfig {
    token: String,
    proxy_ipv4: SocketAddr,
    proxy_ipv6: SocketAddr,
    execution_control: SocketAddr,
    execution_token: String,
    audit_control: SocketAddr,
    audit_token: String,
    hook_libraries: String,
    filesystem_root: String,
    filesystem_mode: String,
    filesystem_cipher_key: Option<String>,
    filesystem_cipher: Option<crate::filesystem::FileCipher>,
    native_passthrough_roots: Vec<PathBuf>,
    local_filesystem: Option<(String, String)>,
    inherited_local_descriptors: Option<String>,
    remote_filesystem: Option<RemoteHookConfig>,
    remote_current_directory: Option<PathBuf>,
    tls_trust_anchor_der: Option<String>,
    tls_trust_bundle: Option<String>,
    java_trust_store: Option<String>,
    trace: TraceContext,
    #[cfg(any(agora_sandbox_hook_build, test, coverage))]
    inherited_control_descriptors: InheritedControlDescriptors,
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct InheritedControlDescriptors {
    pub(super) lock: Option<libc::c_int>,
    pub(super) execution: Option<libc::c_int>,
    pub(super) audit: Option<libc::c_int>,
    pub(super) local: Option<libc::c_int>,
    pub(super) remote: Option<libc::c_int>,
}

#[derive(Clone, Debug)]
struct RemoteHookConfig {
    control: String,
    token: String,
    roots: String,
}

impl HookConfig {
    pub(super) fn from_environment() -> Result<Self, String> {
        Self::from_getter(|key| std::env::var(key).ok())
    }

    pub(super) fn from_getter(mut get: impl FnMut(&str) -> Option<String>) -> Result<Self, String> {
        let token = Self::required(&mut get, TOKEN)?;
        let proxy_ipv4 = Self::required(&mut get, PROXY_IPV4)?
            .parse::<SocketAddr>()
            .map_err(|error| format!("invalid {PROXY_IPV4}: {error}"))?;
        let proxy_ipv6 = Self::required(&mut get, PROXY_IPV6)?
            .parse::<SocketAddr>()
            .map_err(|error| format!("invalid {PROXY_IPV6}: {error}"))?;
        let execution_control = Self::required(&mut get, EXECUTION_CONTROL)?
            .parse::<SocketAddr>()
            .map_err(|error| format!("invalid {EXECUTION_CONTROL}: {error}"))?;
        let execution_token = Self::required(&mut get, EXECUTION_TOKEN)?;
        let audit_control = Self::required(&mut get, AUDIT_CONTROL)?
            .parse::<SocketAddr>()
            .map_err(|error| format!("invalid {AUDIT_CONTROL}: {error}"))?;
        let audit_token = Self::required(&mut get, AUDIT_TOKEN)?;
        #[cfg(any(agora_sandbox_hook_build, test, coverage))]
        let inherited_control_descriptors = InheritedControlDescriptors {
            lock: Self::optional_descriptor(&mut get, CONTROL_LOCK_DESCRIPTOR)?,
            execution: Self::optional_descriptor(&mut get, EXECUTION_CONTROL_DESCRIPTOR)?,
            audit: Self::optional_descriptor(&mut get, AUDIT_CONTROL_DESCRIPTOR)?,
            local: Self::optional_descriptor(&mut get, LOCAL_CONTROL_DESCRIPTOR)?,
            remote: Self::optional_descriptor(&mut get, REMOTE_CONTROL_DESCRIPTOR)?,
        };
        let hook_libraries = Self::required(&mut get, HOOK_LIBRARIES)?;
        let filesystem_root = Self::required(&mut get, FILESYSTEM_ROOT)?;
        let filesystem_mode = Self::required(&mut get, FILESYSTEM_MODE)?;
        let filesystem_cipher_key = get(FILESYSTEM_CIPHER_KEY).filter(|value| !value.is_empty());
        match filesystem_mode.as_str() {
            "plain" if filesystem_cipher_key.is_none() => {}
            "encrypted" if filesystem_cipher_key.is_some() => {}
            "plain" => return Err("plain filesystem mode cannot include a cipher key".into()),
            "encrypted" => return Err("encrypted filesystem mode requires a cipher key".into()),
            _ => return Err(format!("invalid {FILESYSTEM_MODE}: {filesystem_mode}")),
        }
        let filesystem_cipher =
            Self::decode_filesystem_cipher(&filesystem_mode, filesystem_cipher_key.as_deref())?;
        let mut native_passthrough_roots = get(NATIVE_PASSTHROUGH_ROOTS)
            .filter(|value| !value.is_empty())
            .map(|value| {
                serde_json::from_str::<Vec<PathBuf>>(&value)
                    .map_err(|error| format!("invalid {NATIVE_PASSTHROUGH_ROOTS}: {error}"))
            })
            .transpose()?
            .unwrap_or_default();
        native_passthrough_roots.push(PathBuf::from("/dev"));
        native_passthrough_roots = native_passthrough_roots
            .into_iter()
            .map(|root| {
                if !root.is_absolute() {
                    return Err(format!(
                        "native passthrough root must be absolute: {}",
                        root.display()
                    ));
                }
                crate::filesystem::normalize_path(&root).map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        native_passthrough_roots.sort();
        native_passthrough_roots.dedup();
        let local_control = get(LOCAL_FILESYSTEM_CONTROL).filter(|value| !value.is_empty());
        let local_token = get(LOCAL_FILESYSTEM_TOKEN).filter(|value| !value.is_empty());
        let local_filesystem = match (local_control, local_token) {
            (None, None) => None,
            (Some(control), Some(token)) => {
                if !Path::new(&control).is_absolute() {
                    return Err(format!(
                        "{LOCAL_FILESYSTEM_CONTROL} must be an absolute path"
                    ));
                }
                Some((control, token))
            }
            _ => {
                return Err("local filesystem requires control and token together".to_string());
            }
        };
        let inherited_local_descriptors =
            get(INHERITED_LOCAL_DESCRIPTORS).filter(|value| !value.is_empty());
        let remote_control = get(REMOTE_CONTROL).filter(|value| !value.is_empty());
        let remote_token = get(REMOTE_TOKEN).filter(|value| !value.is_empty());
        let remote_roots = get(REMOTE_ROOTS).filter(|value| !value.is_empty());
        let remote_filesystem = match (remote_control, remote_token, remote_roots) {
            (None, None, None) => None,
            (Some(control), Some(token), Some(roots)) => {
                if !Path::new(&control).is_absolute() {
                    return Err(format!("{REMOTE_CONTROL} must be an absolute path"));
                }
                let parsed: Vec<crate::nfs::protocol::RemoteRoute> =
                    serde_json::from_str(&roots)
                        .map_err(|error| format!("invalid {REMOTE_ROOTS}: {error}"))?;
                if parsed.is_empty() {
                    return Err(format!("{REMOTE_ROOTS} cannot be empty"));
                }
                Some(RemoteHookConfig {
                    control,
                    token,
                    roots,
                })
            }
            _ => {
                return Err(
                    "remote filesystem requires control, token, and roots together".to_string(),
                );
            }
        };
        let remote_current_directory = get(REMOTE_CURRENT_DIRECTORY)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        if let Some(directory) = &remote_current_directory {
            if remote_filesystem.is_none() {
                return Err(format!(
                    "{REMOTE_CURRENT_DIRECTORY} requires a remote filesystem"
                ));
            }
            if !directory.is_absolute() {
                return Err(format!(
                    "{REMOTE_CURRENT_DIRECTORY} must be an absolute path"
                ));
            }
        }
        let tls_trust_anchor_der = get(TLS_TRUST_ANCHOR_DER).filter(|value| !value.is_empty());
        let tls_trust_bundle = get(TLS_TRUST_BUNDLE).filter(|value| !value.is_empty());
        let java_trust_store = get(JAVA_TRUST_STORE_ENVIRONMENT).filter(|value| !value.is_empty());
        let trace = TraceContext::parse(&Self::required(&mut get, TRACE_ID_ENVIRONMENT)?)
            .map_err(|error| format!("invalid {TRACE_ID_ENVIRONMENT}: {error}"))?;
        if !proxy_ipv4.ip().is_loopback() || !matches!(proxy_ipv4.ip(), IpAddr::V4(_)) {
            return Err(format!("{PROXY_IPV4} must be an IPv4 loopback address"));
        }
        if !proxy_ipv6.ip().is_loopback() || !matches!(proxy_ipv6.ip(), IpAddr::V6(_)) {
            return Err(format!("{PROXY_IPV6} must be an IPv6 loopback address"));
        }
        if !execution_control.ip().is_loopback() || !matches!(execution_control.ip(), IpAddr::V4(_))
        {
            return Err(format!(
                "{EXECUTION_CONTROL} must be an IPv4 loopback address"
            ));
        }
        if !audit_control.ip().is_loopback() || !matches!(audit_control.ip(), IpAddr::V4(_)) {
            return Err(format!("{AUDIT_CONTROL} must be an IPv4 loopback address"));
        }
        Ok(Self {
            token,
            proxy_ipv4,
            proxy_ipv6,
            execution_control,
            execution_token,
            audit_control,
            audit_token,
            hook_libraries,
            filesystem_root,
            filesystem_mode,
            filesystem_cipher_key,
            filesystem_cipher,
            native_passthrough_roots,
            local_filesystem,
            inherited_local_descriptors,
            remote_filesystem,
            remote_current_directory,
            tls_trust_anchor_der,
            tls_trust_bundle,
            java_trust_store,
            trace,
            #[cfg(any(agora_sandbox_hook_build, test, coverage))]
            inherited_control_descriptors,
        })
    }

    pub(super) fn token(&self) -> &str {
        &self.token
    }

    pub(super) fn proxy_for(&self, destination: SocketAddr) -> SocketAddr {
        match destination {
            SocketAddr::V4(_) => self.proxy_ipv4,
            SocketAddr::V6(_) => self.proxy_ipv6,
        }
    }

    pub(super) fn execution_control(&self) -> SocketAddr {
        self.execution_control
    }

    pub(super) fn execution_token(&self) -> &str {
        &self.execution_token
    }

    pub(super) fn audit_control(&self) -> SocketAddr {
        self.audit_control
    }

    pub(super) fn audit_token(&self) -> &str {
        &self.audit_token
    }

    pub(super) fn hook_libraries(&self) -> &str {
        &self.hook_libraries
    }

    pub(super) fn filesystem_root(&self) -> &str {
        &self.filesystem_root
    }

    pub(super) fn filesystem_cipher(&self) -> Option<crate::filesystem::FileCipher> {
        self.filesystem_cipher.clone()
    }

    pub(super) fn native_passthrough_roots(&self) -> &[PathBuf] {
        &self.native_passthrough_roots
    }

    pub(super) fn local_filesystem(&self) -> Option<(&str, &str)> {
        self.local_filesystem
            .as_ref()
            .map(|(control, token)| (control.as_str(), token.as_str()))
    }

    pub(super) fn inherited_local_descriptors(&self) -> Option<&str> {
        self.inherited_local_descriptors.as_deref()
    }

    pub(super) fn remote_filesystem(&self) -> Option<(&str, &str, &str)> {
        self.remote_filesystem.as_ref().map(|remote| {
            (
                remote.control.as_str(),
                remote.token.as_str(),
                remote.roots.as_str(),
            )
        })
    }

    pub(super) fn remote_current_directory(&self) -> Option<&Path> {
        self.remote_current_directory.as_deref()
    }

    fn decode_filesystem_cipher(
        mode: &str,
        key: Option<&str>,
    ) -> Result<Option<crate::filesystem::FileCipher>, String> {
        if mode == "plain" {
            return Ok(None);
        }
        let key = base64::engine::general_purpose::STANDARD
            .decode(key.unwrap_or_default())
            .map_err(|error| format!("invalid {FILESYSTEM_CIPHER_KEY}: {error}"))?;
        crate::filesystem::FileCipher::from_key(&key)
            .map(Some)
            .map_err(|error| format!("invalid encrypted filesystem configuration: {error:#}"))
    }

    pub(super) fn tls_trust_anchor_der(&self) -> Option<&str> {
        self.tls_trust_anchor_der.as_deref()
    }

    #[cfg(test)]
    pub(super) fn tls_trust_bundle(&self) -> Option<&str> {
        self.tls_trust_bundle.as_deref()
    }

    pub(super) fn java_trust_store(&self) -> Option<&str> {
        self.java_trust_store.as_deref()
    }

    pub(super) fn trace(&self) -> &TraceContext {
        &self.trace
    }

    #[cfg(any(agora_sandbox_hook_build, test, coverage))]
    pub(super) fn inherited_control_descriptors(&self) -> InheritedControlDescriptors {
        self.inherited_control_descriptors
    }

    #[cfg(test)]
    pub(super) fn child_environment(&self) -> Vec<(&'static str, String)> {
        self.child_environment_for(&self.trace)
    }

    pub(super) fn child_environment_for(
        &self,
        trace: &TraceContext,
    ) -> Vec<(&'static str, String)> {
        let mut environment = vec![
            (TOKEN, self.token.clone()),
            (PROXY_IPV4, self.proxy_ipv4.to_string()),
            (PROXY_IPV6, self.proxy_ipv6.to_string()),
            (EXECUTION_CONTROL, self.execution_control.to_string()),
            (EXECUTION_TOKEN, self.execution_token.clone()),
            (AUDIT_CONTROL, self.audit_control.to_string()),
            (AUDIT_TOKEN, self.audit_token.clone()),
            (HOOK_LIBRARIES, self.hook_libraries.clone()),
            (FILESYSTEM_ROOT, self.filesystem_root.clone()),
            (FILESYSTEM_MODE, self.filesystem_mode.clone()),
            (
                NATIVE_PASSTHROUGH_ROOTS,
                serde_json::to_string(&self.native_passthrough_roots)
                    .expect("validated native passthrough roots must be serializable"),
            ),
            (TRACE_ID_ENVIRONMENT, trace.encode()),
        ];
        if let Some(key) = &self.filesystem_cipher_key {
            environment.push((FILESYSTEM_CIPHER_KEY, key.clone()));
        }
        if let Some((control, token)) = &self.local_filesystem {
            environment.extend([
                (LOCAL_FILESYSTEM_CONTROL, control.clone()),
                (LOCAL_FILESYSTEM_TOKEN, token.clone()),
            ]);
        }
        if let Some(remote) = &self.remote_filesystem {
            environment.extend([
                (REMOTE_CONTROL, remote.control.clone()),
                (REMOTE_TOKEN, remote.token.clone()),
                (REMOTE_ROOTS, remote.roots.clone()),
            ]);
        }
        if let Some(anchor) = &self.tls_trust_anchor_der {
            environment.push((TLS_TRUST_ANCHOR_DER, anchor.clone()));
        }
        if let Some(bundle) = &self.tls_trust_bundle {
            environment.push((TLS_TRUST_BUNDLE, bundle.clone()));
            environment.extend(
                TLS_CLIENT_TRUST_ENVIRONMENT
                    .into_iter()
                    .map(|key| (key, bundle.clone())),
            );
        }
        if let Some(store) = &self.java_trust_store {
            environment.push((JAVA_TRUST_STORE_ENVIRONMENT, store.clone()));
        }
        environment
    }

    pub(super) fn is_internal(&self, destination: SocketAddr) -> bool {
        destination == self.proxy_ipv4
            || destination == self.proxy_ipv6
            || destination == self.execution_control
            || destination == self.audit_control
    }

    fn required(get: &mut impl FnMut(&str) -> Option<String>, key: &str) -> Result<String, String> {
        get(key)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("missing {key}"))
    }

    #[cfg(any(agora_sandbox_hook_build, test, coverage))]
    fn optional_descriptor(
        get: &mut impl FnMut(&str) -> Option<String>,
        key: &str,
    ) -> Result<Option<libc::c_int>, String> {
        get(key)
            .filter(|value| !value.is_empty())
            .map(|value| {
                value
                    .parse::<libc::c_int>()
                    .ok()
                    .filter(|descriptor| *descriptor >= 0)
                    .ok_or_else(|| format!("invalid {key}"))
            })
            .transpose()
    }
}

#[cfg(any(agora_sandbox_hook_build, test, coverage))]
pub(super) fn initialize() -> Result<(), String> {
    if global_result()?.is_some() {
        for key in [
            FILESYSTEM_ROOT,
            FILESYSTEM_MODE,
            NATIVE_PASSTHROUGH_ROOTS,
            FILESYSTEM_CIPHER_KEY,
            LOCAL_FILESYSTEM_CONTROL,
            LOCAL_FILESYSTEM_TOKEN,
            INHERITED_LOCAL_DESCRIPTORS,
            REMOTE_CONTROL,
            REMOTE_TOKEN,
            REMOTE_ROOTS,
            REMOTE_CURRENT_DIRECTORY,
            CONTROL_LOCK_DESCRIPTOR,
            EXECUTION_CONTROL_DESCRIPTOR,
            AUDIT_CONTROL_DESCRIPTOR,
            LOCAL_CONTROL_DESCRIPTOR,
            REMOTE_CONTROL_DESCRIPTOR,
        ] {
            unsafe { std::env::remove_var(key) };
        }
    }
    Ok(())
}

pub(super) fn global() -> Option<&'static HookConfig> {
    global_result().ok().flatten()
}

fn global_result() -> Result<Option<&'static HookConfig>, String> {
    CONFIG
        .get_or_init(|| {
            if std::env::var_os(TOKEN).is_none() {
                Ok(None)
            } else {
                HookConfig::from_environment().map(Some)
            }
        })
        .as_ref()
        .map(Option::as_ref)
        .map_err(Clone::clone)
}
