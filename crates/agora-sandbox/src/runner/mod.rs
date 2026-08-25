#[cfg(target_os = "macos")]
mod runtime;

#[cfg(target_os = "macos")]
pub(crate) use runtime::{
    PreparedLaunch, ProtectedEnvironment, RunningSandboxCommand, SandboxRuntime,
};

#[cfg(target_os = "macos")]
use crate::audit::AuditController;
use crate::callback::Callback;
#[cfg(target_os = "macos")]
use crate::execution::{ExecutionController, resolve_executable, resolve_shebang};
pub use crate::filesystem::FilesystemMode;
#[cfg(target_os = "macos")]
use crate::filesystem::{
    EncryptedWorkspace, FilesystemWorkspace, KeyMigrationStage, broker::LocalController,
};
#[cfg(target_os = "macos")]
use crate::network::client_trust::{
    JAVA_TOOL_OPTIONS_ENVIRONMENT, JAVA_TRUST_STORE_ENVIRONMENT, encode_java_trust_store,
    merged_java_tool_options,
};
use crate::network::{NetworkConfig, NetworkController, NetworkRunContext, TlsMode};
pub use crate::nfs::SmbRemoteConfig;
#[cfg(all(target_os = "macos", feature = "remote-smb"))]
use crate::nfs::{
    controller::{RemoteConnectionStatus, RemoteController, RemoteControllerEvent},
    protocol::RemoteRoute,
};
use crate::trace::{TRACE_ID_ENVIRONMENT, TraceContext};
#[cfg(all(target_os = "macos", feature = "remote-smb"))]
use agora_core::logger::{self, LoggerEntry};
use anyhow::{Context, Result, bail};
#[cfg(target_os = "macos")]
use base64::Engine;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::OpenOptions;
#[cfg(target_os = "macos")]
use std::io::Write;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(target_os = "macos")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "macos")]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
#[cfg(target_os = "macos")]
use tokio::process::Command;
use uuid::Uuid;

const TOKEN: &str = "AGORA_SANDBOX_TOKEN";
const PROXY_IPV4: &str = "AGORA_SANDBOX_PROXY_IPV4";
const PROXY_IPV6: &str = "AGORA_SANDBOX_PROXY_IPV6";
#[cfg(target_os = "macos")]
const EXECUTION_CONTROL: &str = "AGORA_SANDBOX_EXECUTION_CONTROL";
#[cfg(target_os = "macos")]
const EXECUTION_TOKEN: &str = "AGORA_SANDBOX_EXECUTION_TOKEN";
#[cfg(target_os = "macos")]
const AUDIT_CONTROL: &str = "AGORA_SANDBOX_AUDIT_CONTROL";
#[cfg(target_os = "macos")]
const AUDIT_TOKEN: &str = "AGORA_SANDBOX_AUDIT_TOKEN";
#[cfg(target_os = "macos")]
const HOOK_LIBRARIES: &str = "AGORA_SANDBOX_HOOK_LIBRARIES";
#[cfg(target_os = "macos")]
const FILESYSTEM_ROOT: &str = "AGORA_SANDBOX_FILESYSTEM_ROOT";
#[cfg(target_os = "macos")]
const FILESYSTEM_MODE: &str = "AGORA_SANDBOX_FILESYSTEM_MODE";
#[cfg(target_os = "macos")]
const FILESYSTEM_CIPHER_KEY: &str = "AGORA_SANDBOX_FILESYSTEM_CIPHER_KEY";
#[cfg(target_os = "macos")]
const LOCAL_FILESYSTEM_CONTROL: &str = "AGORA_SANDBOX_LOCAL_FILESYSTEM_CONTROL";
#[cfg(target_os = "macos")]
const LOCAL_FILESYSTEM_TOKEN: &str = "AGORA_SANDBOX_LOCAL_FILESYSTEM_TOKEN";
#[cfg(target_os = "macos")]
const INHERITED_LOCAL_DESCRIPTORS: &str = "AGORA_SANDBOX_INHERITED_LOCAL_DESCRIPTORS";
#[cfg(target_os = "macos")]
const REMOTE_CONTROL: &str = "AGORA_SANDBOX_REMOTE_CONTROL";
#[cfg(target_os = "macos")]
const REMOTE_TOKEN: &str = "AGORA_SANDBOX_REMOTE_TOKEN";
#[cfg(target_os = "macos")]
const REMOTE_ROOTS: &str = "AGORA_SANDBOX_REMOTE_ROOTS";
#[cfg(target_os = "macos")]
const REMOTE_CURRENT_DIRECTORY: &str = "AGORA_SANDBOX_REMOTE_CURRENT_DIRECTORY";
#[cfg(target_os = "macos")]
const NATIVE_PASSTHROUGH_ROOTS: &str = "AGORA_SANDBOX_NATIVE_PASSTHROUGH_ROOTS";
#[cfg(target_os = "macos")]
const TLS_TRUST_ANCHOR_DER: &str = "AGORA_SANDBOX_TLS_TRUST_ANCHOR_DER";
#[cfg(target_os = "macos")]
const TLS_TRUST_BUNDLE: &str = "AGORA_SANDBOX_TLS_TRUST_BUNDLE";
const DEFAULT_TLS_CA_CERTIFICATE: &str = "ca/ca.crt";
const DEFAULT_TLS_CA_PRIVATE_KEY: &str = "ca/ca.key";
const DEFAULT_NATIVE_PASSTHROUGH_ROOT: &str = "/dev";
#[cfg(target_os = "macos")]
const TLS_TRUST_BUNDLE_DIRECTORY: &str = "ca";
#[cfg(target_os = "macos")]
const TLS_CLIENT_TRUST_ENVIRONMENT: [&str; 6] = [
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
    "PIP_CERT",
    "NODE_EXTRA_CA_CERTS",
    "GIT_SSL_CAINFO",
];

#[cfg(target_os = "macos")]
async fn filesystem_blocking<T>(operation: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .context("filesystem blocking task failed")?
}

#[derive(Clone, Debug)]
pub struct SandboxConfig {
    pub network: NetworkConfig,
    hook_library: PathBuf,
    workdir: PathBuf,
    filesystem_mode: FilesystemMode,
    encrypted_workspace_key: Option<SecretBytes>,
    native_passthrough_roots: Vec<PathBuf>,
    tls_ca: Option<TlsCaFiles>,
    smb_remotes: Vec<SmbRemoteConfig>,
    #[cfg(test)]
    upstream_tls_roots: Option<Vec<rustls::pki_types::CertificateDer<'static>>>,
}

#[derive(Clone)]
struct SecretBytes(Vec<u8>);

impl SecretBytes {
    fn new(value: impl AsRef<[u8]>) -> Self {
        Self(value.as_ref().to_vec())
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Clone, Debug)]
struct TlsCaFiles {
    certificate: PathBuf,
    private_key: PathBuf,
}

impl SandboxConfig {
    pub fn new(hook_library: impl Into<PathBuf>) -> Self {
        Self {
            network: NetworkConfig::default(),
            hook_library: hook_library.into(),
            workdir: Self::default_workdir(),
            filesystem_mode: FilesystemMode::default(),
            encrypted_workspace_key: None,
            native_passthrough_roots: vec![PathBuf::from(DEFAULT_NATIVE_PASSTHROUGH_ROOT)],
            tls_ca: None,
            smb_remotes: Vec::new(),
            #[cfg(test)]
            upstream_tls_roots: None,
        }
    }

    pub fn default_workdir() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".agora-sandbox")
    }

    pub fn hook_library(&self) -> &Path {
        &self.hook_library
    }

    pub fn with_workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.workdir = workdir.into();
        self
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn with_encrypted_workspace(mut self, key: impl AsRef<[u8]>) -> Self {
        self.filesystem_mode = FilesystemMode::Encrypted;
        self.encrypted_workspace_key = Some(SecretBytes::new(key));
        self
    }

    pub fn with_plain_workspace(mut self) -> Self {
        self.filesystem_mode = FilesystemMode::Plain;
        self.encrypted_workspace_key = None;
        self
    }

    pub fn filesystem_mode(&self) -> FilesystemMode {
        self.filesystem_mode
    }

    pub fn encrypted_workspace_key(&self) -> Option<&[u8]> {
        self.encrypted_workspace_key
            .as_ref()
            .map(SecretBytes::as_bytes)
    }

    pub fn with_native_passthrough_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.native_passthrough_roots.push(root.into());
        self
    }

    pub fn native_passthrough_roots(&self) -> &[PathBuf] {
        &self.native_passthrough_roots
    }

    pub fn with_tls_ca(
        mut self,
        certificate: impl Into<PathBuf>,
        private_key: impl Into<PathBuf>,
    ) -> Self {
        self.tls_ca = Some(TlsCaFiles {
            certificate: certificate.into(),
            private_key: private_key.into(),
        });
        self
    }

    pub fn tls_ca(&self) -> Option<(&Path, &Path)> {
        self.tls_ca
            .as_ref()
            .map(|ca| (ca.certificate.as_path(), ca.private_key.as_path()))
    }

    pub fn with_smb_remote(mut self, remote: SmbRemoteConfig) -> Self {
        self.smb_remotes.push(remote);
        self
    }

    pub fn smb_remotes(&self) -> &[SmbRemoteConfig] {
        &self.smb_remotes
    }

    #[cfg(test)]
    fn with_upstream_tls_roots(
        mut self,
        roots: Vec<rustls::pki_types::CertificateDer<'static>>,
    ) -> Self {
        self.upstream_tls_roots = Some(roots);
        self
    }

    pub fn validate(&self) -> Result<()> {
        self.network.validate()?;
        let native_passthrough_roots = self.native_passthrough_root_aliases()?;
        self.validate_smb_remotes(&native_passthrough_roots)?;
        #[cfg(not(feature = "remote-smb"))]
        if !self.smb_remotes.is_empty() {
            bail!("this build does not include SMB remote filesystem support");
        }
        #[cfg(not(target_os = "macos"))]
        bail!("the network hook is currently supported only on macOS");
        if !self.hook_library.is_file() {
            bail!(
                "sandbox hook library does not exist: {}",
                self.hook_library.display()
            );
        }
        #[cfg(target_os = "macos")]
        match (self.filesystem_mode, &self.encrypted_workspace_key) {
            (FilesystemMode::Encrypted, Some(key)) => {
                EncryptedWorkspace::validate_passphrase(key.as_bytes())?
            }
            (FilesystemMode::Encrypted, None) => bail!("sandbox filesystem key is required"),
            (FilesystemMode::Plain, None) => {}
            (FilesystemMode::Plain, Some(_)) => {
                bail!("encrypted filesystem key cannot be used with plain filesystem mode")
            }
        }
        Ok(())
    }

    fn normalized_native_passthrough_roots(&self) -> Result<Vec<PathBuf>> {
        let mut roots = self
            .native_passthrough_roots
            .iter()
            .map(|root| {
                if !root.is_absolute() {
                    bail!(
                        "native passthrough root must be absolute: {}",
                        root.display()
                    );
                }
                crate::filesystem::normalize_path(root)
            })
            .collect::<Result<Vec<_>>>()?;
        roots.sort();
        roots.dedup();
        Ok(roots)
    }

    fn native_passthrough_root_aliases(&self) -> Result<Vec<(PathBuf, PathBuf)>> {
        let workdir = if self.workdir.is_absolute() {
            self.workdir.clone()
        } else {
            std::env::current_dir()
                .context("failed to resolve current directory")?
                .join(&self.workdir)
        };
        let workdir = crate::filesystem::normalize_path(&workdir)?;
        let resolved_workdir = crate::filesystem::resolve_existing_ancestor(&workdir)?;
        self.normalized_native_passthrough_roots()?
            .into_iter()
            .map(|root| {
                let resolved = crate::filesystem::resolve_existing_ancestor(&root)?;
                if path_aliases_overlap(&root, &resolved, &workdir, &resolved_workdir) {
                    bail!(
                        "native passthrough root overlaps sandbox work directory: {}",
                        root.display()
                    );
                }
                Ok((root, resolved))
            })
            .collect()
    }

    fn effective_native_passthrough_roots(&self) -> Result<Vec<PathBuf>> {
        let roots = self.normalized_native_passthrough_roots()?;
        let mut effective = roots.clone();
        for root in roots {
            effective.push(crate::filesystem::resolve_existing_ancestor(&root)?);
        }
        effective.sort();
        effective.dedup();
        Ok(effective)
    }

    fn validate_smb_remotes(&self, native_passthrough_roots: &[(PathBuf, PathBuf)]) -> Result<()> {
        let workdir = if self.workdir.is_absolute() {
            self.workdir.clone()
        } else {
            std::env::current_dir()
                .context("failed to resolve current directory")?
                .join(&self.workdir)
        };
        let workdir = crate::filesystem::normalize_path(&workdir)?;
        let resolved_workdir = crate::filesystem::resolve_existing_ancestor(&workdir)?;
        let roots = self
            .smb_remotes
            .iter()
            .map(|remote| {
                let logical = remote.logical_root();
                let resolved = crate::filesystem::resolve_existing_ancestor(logical)?;
                Ok((logical, resolved))
            })
            .collect::<Result<Vec<_>>>()?;
        for (index, (root, resolved_root)) in roots.iter().enumerate() {
            if let Some((native, _)) = native_passthrough_roots.iter().find(|(native, resolved)| {
                path_aliases_overlap(root, resolved_root, native, resolved)
            }) {
                bail!(
                    "SMB logical root overlaps native passthrough root {}: {}",
                    native.display(),
                    root.display()
                );
            }
            if path_aliases_overlap(root, resolved_root, &workdir, &resolved_workdir) {
                bail!(
                    "SMB logical root overlaps sandbox work directory: {}",
                    root.display()
                );
            }
            for (other, resolved_other) in &roots[..index] {
                if path_aliases_overlap(root, resolved_root, other, resolved_other) {
                    bail!(
                        "SMB logical roots overlap: {} and {}",
                        other.display(),
                        root.display()
                    );
                }
            }
        }
        Ok(())
    }

    fn tls_ca_for_workdir(&self) -> Result<Option<TlsCaFiles>> {
        if self.network.tls == TlsMode::Off {
            return Ok(None);
        }
        let managed = self.tls_ca.is_none();
        let ca = self.tls_ca.clone().unwrap_or_else(|| TlsCaFiles {
            certificate: self.workdir.join(DEFAULT_TLS_CA_CERTIFICATE),
            private_key: self.workdir.join(DEFAULT_TLS_CA_PRIVATE_KEY),
        });
        let (missing, invalid_managed_pair) = if managed {
            let directory = ca
                .certificate
                .parent()
                .context("managed TLS CA certificate has no parent directory")?;
            crate::managed_fs::prepare_owned_directory(directory, "managed TLS CA directory")?;
            let certificate = read_managed_file(&ca.certificate, "TLS CA certificate")?;
            let private_key = read_managed_file(&ca.private_key, "TLS CA private key")?;
            match (certificate, private_key) {
                (Some(certificate), Some(private_key)) => (
                    false,
                    crate::network::validate_tls_ca(&certificate, &private_key).is_err(),
                ),
                _ => (true, false),
            }
        } else {
            (
                !ca.certificate.is_file() || !ca.private_key.is_file(),
                false,
            )
        };
        if missing || invalid_managed_pair {
            crate::network::generate_tls_ca(&ca.certificate, &ca.private_key)?;
        }
        Ok(Some(ca))
    }

    #[cfg(target_os = "macos")]
    fn write_tls_trust_bundle(
        &self,
        runtime_directory: &Path,
        ca_certificate: &[u8],
    ) -> Result<PathBuf> {
        let native = crate::network::native_root_certificates()
            .context("failed to load native TLS roots for client trust bundle")?;
        let mut bundle = ca_certificate.to_vec();
        if !bundle.ends_with(b"\n") {
            bundle.push(b'\n');
        }
        for certificate in native {
            bundle.extend_from_slice(b"-----BEGIN CERTIFICATE-----\n");
            let encoded = base64::engine::general_purpose::STANDARD.encode(certificate.as_ref());
            for line in encoded.as_bytes().chunks(64) {
                bundle.extend_from_slice(line);
                bundle.push(b'\n');
            }
            bundle.extend_from_slice(b"-----END CERTIFICATE-----\n");
        }

        write_tls_trust_artifact(
            runtime_directory,
            "trust-bundle",
            "crt",
            ca_certificate,
            &bundle,
            "TLS client trust bundle",
        )
    }

    #[cfg(target_os = "macos")]
    fn write_java_trust_store(
        &self,
        runtime_directory: &Path,
        ca_certificate: &[u8],
    ) -> Result<PathBuf> {
        use rustls::pki_types::{CertificateDer, pem::PemObject};

        let mut certificates = CertificateDer::pem_slice_iter(ca_certificate)
            .collect::<Result<Vec<_>, _>>()
            .context("failed to parse TLS CA certificate for Java trust store")?;
        if certificates.len() != 1 {
            bail!("TLS CA certificate must contain exactly one certificate");
        }
        certificates.extend(
            crate::network::native_root_certificates()
                .context("failed to load native TLS roots for Java trust store")?,
        );
        let encoded =
            encode_java_trust_store(certificates.iter().map(|certificate| certificate.as_ref()))?;

        write_tls_trust_artifact(
            runtime_directory,
            "java-trust-store",
            "jks",
            ca_certificate,
            &encoded,
            "Java trust store",
        )
    }
}

fn read_managed_file(path: &Path, description: &str) -> Result<Option<Vec<u8>>> {
    let mut options = OpenOptions::new();
    options.read(true);
    let mut file = match crate::managed_fs::open_owned_regular(&mut options, path, Some(0o600)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to open {description} {}", path.display()));
        }
    };
    let mut contents = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut contents)
        .with_context(|| format!("failed to read {description} {}", path.display()))?;
    Ok(Some(contents))
}

#[cfg(target_os = "macos")]
fn write_tls_trust_artifact(
    runtime_directory: &Path,
    name: &str,
    extension: &str,
    ca_certificate: &[u8],
    contents: &[u8],
    description: &str,
) -> Result<PathBuf> {
    let mut fingerprint = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d_u128;
    for byte in ca_certificate {
        fingerprint ^= u128::from(*byte);
        fingerprint = fingerprint.wrapping_mul(309_485_009_821_345_068_724_781_371);
    }
    let path = runtime_directory
        .join(TLS_TRUST_BUNDLE_DIRECTORY)
        .join(format!("{name}-{fingerprint:032x}.{extension}"));
    let parent = path
        .parent()
        .with_context(|| format!("{description} path has no parent"))?;
    let directory_description = format!("{description} directory");
    let _directory =
        crate::managed_fs::prepare_owned_directory_preserving_mode(parent, &directory_description)?;
    let temporary = parent.join(format!(".{name}-{}.tmp", Uuid::new_v4().simple()));
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.flush()?;
        std::fs::rename(&temporary, &path)?;
        Ok::<_, std::io::Error>(())
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("failed to write {description} {}", path.display()));
    }
    path.canonicalize()
        .with_context(|| format!("failed to resolve {description}"))
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn path_aliases_overlap(
    left: &Path,
    resolved_left: &Path,
    right: &Path,
    resolved_right: &Path,
) -> bool {
    [left, resolved_left].into_iter().any(|left| {
        [right, resolved_right]
            .into_iter()
            .any(|right| paths_overlap(left, right))
    })
}

#[derive(Debug)]
pub struct SandboxCommand {
    program: OsString,
    arguments: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
    current_dir: Option<PathBuf>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

impl SandboxCommand {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            current_dir: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.arguments.extend(arguments.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    pub fn stdin<T>(mut self, stdio: T) -> Self
    where
        T: Into<Stdio>,
    {
        self.stdin = Some(stdio.into());
        self
    }

    pub fn stdout<T>(mut self, stdio: T) -> Self
    where
        T: Into<Stdio>,
    {
        self.stdout = Some(stdio.into());
        self
    }

    pub fn stderr<T>(mut self, stdio: T) -> Self
    where
        T: Into<Stdio>,
    {
        self.stderr = Some(stdio.into());
        self
    }

    #[cfg(target_os = "macos")]
    fn into_command(self) -> Command {
        let mut command = Command::new(self.program);
        command.args(self.arguments);
        command.envs(self.environment);
        if let Some(current_dir) = self.current_dir {
            command.current_dir(current_dir);
        }
        if let Some(stdin) = self.stdin {
            command.stdin(stdin);
        }
        if let Some(stdout) = self.stdout {
            command.stdout(stdout);
        }
        if let Some(stderr) = self.stderr {
            command.stderr(stderr);
        }
        command
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn resolved_program(&self) -> Result<PathBuf> {
        resolve_executable(
            &self.program,
            self.current_dir.as_deref(),
            &self.environment,
        )
    }

    #[cfg(test)]
    fn effective_current_dir(&self) -> Result<PathBuf> {
        let directory = match &self.current_dir {
            Some(directory) if directory.is_absolute() => directory.clone(),
            Some(directory) => std::env::current_dir()?.join(directory),
            None => std::env::current_dir()?,
        };
        let directory = directory.canonicalize().with_context(|| {
            format!(
                "failed to resolve sandbox command workdir {}",
                directory.display()
            )
        })?;
        if !directory.is_dir() {
            bail!(
                "sandbox command workdir is not a directory: {}",
                directory.display()
            );
        }
        Ok(directory)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn apply_prepared(&mut self, launch: &PreparedLaunch) {
        self.program = launch.program().as_os_str().to_owned();
        let mut arguments = Vec::with_capacity(
            launch
                .argument_prefix()
                .len()
                .saturating_add(self.arguments.len()),
        );
        arguments.extend_from_slice(launch.argument_prefix());
        arguments.append(&mut self.arguments);
        self.arguments = arguments;
    }

    #[cfg(target_os = "macos")]
    fn effective_environment(&self, key: &str) -> Option<OsString> {
        self.environment
            .get(OsStr::new(key))
            .cloned()
            .or_else(|| std::env::var_os(key))
    }
}

#[cfg(target_os = "macos")]
pub struct SandboxChild {
    pub stdin: Option<tokio::process::ChildStdin>,
    pub stdout: Option<tokio::process::ChildStdout>,
    pub stderr: Option<tokio::process::ChildStderr>,
    command: RunningSandboxCommand,
    runtime: Option<SandboxRuntime>,
    shutdown: Option<tokio::task::JoinHandle<Result<()>>>,
    status: Option<ExitStatus>,
    failure: Option<String>,
    sandbox_id: String,
    run_id: String,
}

#[cfg(target_os = "macos")]
impl SandboxChild {
    pub fn id(&self) -> Option<u32> {
        self.command.id()
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.command.try_wait()
    }

    pub async fn kill(&mut self) -> Result<()> {
        self.command.kill().await
    }

    pub async fn wait(&mut self) -> Result<SandboxOutcome> {
        if self.status.is_none() && self.failure.is_none() {
            drop(self.stdin.take());
            let runtime = self
                .runtime
                .as_mut()
                .context("sandbox child runtime is unavailable")?;
            match self.command.wait_or_failure(runtime.wait_failure()).await {
                Ok(status) => self.status = Some(status),
                Err(error) => self.failure = Some(error.to_string()),
            }
        }
        if self.shutdown.is_none()
            && let Some(runtime) = self.runtime.take()
        {
            self.shutdown = Some(tokio::spawn(runtime.shutdown()));
        }
        if let Some(shutdown) = self.shutdown.as_mut() {
            let result = shutdown.await;
            self.shutdown = None;
            if let Err(error) = result
                .context("sandbox runtime shutdown task failed")
                .and_then(|result| result)
            {
                self.failure.get_or_insert_with(|| error.to_string());
            }
        }
        if let Some(error) = &self.failure {
            bail!("{error}");
        }
        let status = self.status.context("sandbox child status is unavailable")?;
        Ok(SandboxOutcome {
            status,
            sandbox_id: self.sandbox_id.clone(),
            run_id: self.run_id.clone(),
        })
    }
}

pub struct Sandbox<C>
where
    C: Callback,
{
    config: SandboxConfig,
    callback: C,
}

impl<C> Sandbox<C>
where
    C: Callback,
{
    pub fn new(config: SandboxConfig, callback: C) -> Self {
        Self { config, callback }
    }

    #[cfg(target_os = "macos")]
    pub async fn spawn(self, command: SandboxCommand) -> Result<SandboxChild> {
        self.spawn_with_foreground(command, false).await
    }

    #[cfg(target_os = "macos")]
    async fn spawn_with_foreground(
        self,
        command: SandboxCommand,
        foreground: bool,
    ) -> Result<SandboxChild> {
        let executable = command.resolved_program()?;
        let runtime = SandboxRuntime::start(self.config, self.callback).await?;
        let sandbox_id = runtime.sandbox_id().to_owned();
        let run_id = runtime.run_id().to_owned();
        let launch = match runtime.prepare(executable).await {
            Ok(launch) => launch,
            Err(error) => {
                let _ = runtime.shutdown().await;
                return Err(error);
            }
        };
        let mut command = match RunningSandboxCommand::spawn(command, &launch, foreground) {
            Ok(child) => child,
            Err(error) => {
                let _ = runtime.shutdown().await;
                return Err(error);
            }
        };
        let (stdin, stdout, stderr) = command.take_stdio();
        Ok(SandboxChild {
            stdin,
            stdout,
            stderr,
            command,
            runtime: Some(runtime),
            shutdown: None,
            status: None,
            failure: None,
            sandbox_id,
            run_id,
        })
    }

    #[cfg(target_os = "macos")]
    pub async fn run(self, command: SandboxCommand) -> Result<SandboxOutcome> {
        let mut child = self.spawn_with_foreground(command, true).await?;
        child.wait().await
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn run(self, _command: SandboxCommand) -> Result<SandboxOutcome> {
        self.config.validate()?;
        unreachable!("sandbox validation must reject unsupported platforms")
    }
}

#[cfg(target_os = "macos")]
fn injected_libraries(hook_library: &Path) -> Result<OsString> {
    std::env::join_paths([hook_library]).context("invalid sandbox hook DYLD_INSERT_LIBRARIES path")
}

#[cfg(target_os = "macos")]
pub async fn migrate_filesystem_key(
    workdir: impl AsRef<Path>,
    old_key: impl AsRef<[u8]>,
    new_key: impl AsRef<[u8]>,
) -> Result<()> {
    let workdir = workdir.as_ref().to_path_buf();
    let old_key = old_key.as_ref().to_vec();
    let new_key = new_key.as_ref().to_vec();
    filesystem_blocking(move || EncryptedWorkspace::migrate_key(&workdir, &old_key, &new_key)).await
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilesystemKeyMigrationProgress {
    Validating,
    AcquiringLock,
    ReencryptingFiles,
    VerifyingNewKey,
    UpdatingMetadata,
    Completed,
}

#[cfg(target_os = "macos")]
impl FilesystemKeyMigrationProgress {
    pub const fn percent(self) -> u8 {
        match self {
            Self::Validating => 5,
            Self::AcquiringLock => 15,
            Self::ReencryptingFiles => 40,
            Self::VerifyingNewKey => 75,
            Self::UpdatingMetadata => 90,
            Self::Completed => 100,
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Validating => "Validating keys",
            Self::AcquiringLock => "Acquiring filesystem lock",
            Self::ReencryptingFiles => "Re-encrypting filesystem files",
            Self::VerifyingNewKey => "Verifying encrypted filesystem",
            Self::UpdatingMetadata => "Updating key metadata",
            Self::Completed => "Migration complete",
        }
    }
}

#[cfg(target_os = "macos")]
impl From<KeyMigrationStage> for FilesystemKeyMigrationProgress {
    fn from(stage: KeyMigrationStage) -> Self {
        match stage {
            KeyMigrationStage::Validating => Self::Validating,
            KeyMigrationStage::AcquiringLock => Self::AcquiringLock,
            KeyMigrationStage::ReencryptingFiles => Self::ReencryptingFiles,
            KeyMigrationStage::VerifyingNewKey => Self::VerifyingNewKey,
            KeyMigrationStage::UpdatingMetadata => Self::UpdatingMetadata,
            KeyMigrationStage::Completed => Self::Completed,
        }
    }
}

#[cfg(target_os = "macos")]
pub async fn migrate_filesystem_key_with_progress(
    workdir: impl AsRef<Path>,
    old_key: impl AsRef<[u8]>,
    new_key: impl AsRef<[u8]>,
    mut on_progress: impl FnMut(FilesystemKeyMigrationProgress),
) -> Result<()> {
    let workdir = workdir.as_ref().to_path_buf();
    let old_key = old_key.as_ref().to_vec();
    let new_key = new_key.as_ref().to_vec();
    let (progress_sender, mut progress_receiver) = tokio::sync::mpsc::unbounded_channel();
    let migration = tokio::task::spawn_blocking(move || {
        EncryptedWorkspace::migrate_key_with_progress(&workdir, &old_key, &new_key, |stage| {
            let _ = progress_sender.send(stage);
        })
    });

    while let Some(stage) = progress_receiver.recv().await {
        on_progress(stage.into());
    }

    migration.await.context("filesystem blocking task failed")?
}

#[cfg(target_os = "macos")]
struct ForegroundTerminal {
    descriptor: libc::c_int,
    original_process_group: libc::pid_t,
    handed_off: bool,
}

#[cfg(target_os = "macos")]
impl ForegroundTerminal {
    fn capture() -> Result<Option<Self>> {
        let descriptor = libc::STDIN_FILENO;
        if unsafe { libc::isatty(descriptor) } != 1 {
            return Ok(None);
        }
        let original_process_group = unsafe { libc::tcgetpgrp(descriptor) };
        if original_process_group == -1 {
            return Err(std::io::Error::last_os_error())
                .context("failed to inspect sandbox terminal process group");
        }
        if original_process_group != unsafe { libc::getpgrp() } {
            return Ok(None);
        }
        Ok(Some(Self {
            descriptor,
            original_process_group,
            handed_off: false,
        }))
    }

    fn handoff(&mut self, process_group: libc::pid_t) -> Result<()> {
        set_terminal_process_group(self.descriptor, process_group)
            .context("failed to hand terminal to sandbox child")?;
        self.handed_off = true;
        if unsafe { libc::kill(-process_group, libc::SIGCONT) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).context("failed to continue sandbox child process group");
            }
        }
        Ok(())
    }

    fn restore(&mut self) -> Result<()> {
        if !self.handed_off {
            return Ok(());
        }
        set_terminal_process_group(self.descriptor, self.original_process_group)
            .context("failed to restore sandbox terminal process group")?;
        self.handed_off = false;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for ForegroundTerminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(target_os = "macos")]
fn set_terminal_process_group(
    descriptor: libc::c_int,
    process_group: libc::pid_t,
) -> std::io::Result<()> {
    let mut blocked = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    let mut previous = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    unsafe {
        libc::sigemptyset(blocked.as_mut_ptr());
        libc::sigaddset(blocked.as_mut_ptr(), libc::SIGTTOU);
        let blocked = blocked.assume_init();
        let block_error = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, previous.as_mut_ptr());
        if block_error != 0 {
            return Err(std::io::Error::from_raw_os_error(block_error));
        }
        let previous = previous.assume_init();
        let result = libc::tcsetpgrp(descriptor, process_group);
        let error = (result == -1).then(std::io::Error::last_os_error);
        let restore_error =
            libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if restore_error != 0 {
            return Err(std::io::Error::from_raw_os_error(restore_error));
        }
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
struct RuntimeServices<'a> {
    network: &'a mut NetworkController,
    execution: &'a mut ExecutionController,
    audit: &'a mut AuditController,
    local_filesystem: &'a mut Option<LocalController>,
    #[cfg(feature = "remote-smb")]
    remote: &'a mut Option<RemoteController>,
}

#[cfg(all(test, target_os = "macos"))]
async fn wait_for_child_or_service(
    child: &mut tokio::process::Child,
    process_group: libc::pid_t,
    services: RuntimeServices<'_>,
    #[cfg(feature = "remote-smb")] mut remote_status: impl FnMut(RemoteConnectionStatus),
) -> Result<ExitStatus> {
    let RuntimeServices {
        network: controller,
        execution,
        audit,
        local_filesystem,
        #[cfg(feature = "remote-smb")]
        remote,
    } = services;
    enum Completion {
        Child(std::io::Result<ExitStatus>),
        Proxy(anyhow::Error),
        Execution(anyhow::Error),
        Audit(anyhow::Error),
        Remote(anyhow::Error),
        LocalFilesystem(anyhow::Error),
    }

    #[cfg(feature = "remote-smb")]
    let remote_failure = wait_for_remote_failure(remote, &mut remote_status);
    #[cfg(not(feature = "remote-smb"))]
    let remote_failure = std::future::pending::<anyhow::Error>();
    tokio::pin!(remote_failure);
    let local_filesystem_failure = async {
        match local_filesystem {
            Some(controller) => controller.wait_failure().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(local_filesystem_failure);
    let completion = tokio::select! {
        status = child.wait() => Completion::Child(status),
        error = controller.wait_failure() => Completion::Proxy(error),
        error = execution.wait_failure() => Completion::Execution(error),
        error = audit.wait_failure() => Completion::Audit(error),
        error = &mut remote_failure => Completion::Remote(error),
        error = &mut local_filesystem_failure => Completion::LocalFilesystem(error),
    };
    let result = match completion {
        Completion::Child(status) => status.context("sandbox child wait failed"),
        Completion::Proxy(error) => Err(error).context("sandbox network proxy failed"),
        Completion::Execution(error) => Err(error).context("sandbox execution controller failed"),
        Completion::Audit(error) => Err(error).context("sandbox audit controller failed"),
        Completion::Remote(error) => Err(error).context("sandbox remote filesystem failed"),
        Completion::LocalFilesystem(error) => Err(error).context("sandbox local filesystem failed"),
    };
    let termination = terminate_process_group(child, process_group).await;
    match result {
        Ok(status) => {
            termination?;
            Ok(status)
        }
        Err(error) => {
            let _ = termination;
            Err(error)
        }
    }
}

#[cfg(all(target_os = "macos", feature = "remote-smb"))]
fn remote_logical_parent_errno(
    filesystem: &FilesystemWorkspace,
    logical_root: &Path,
) -> Result<Option<libc::c_int>> {
    let parent = logical_root
        .parent()
        .with_context(|| format!("SMB logical root has no parent: {}", logical_root.display()))?;
    match filesystem.visible_directory(parent)? {
        Some(true) => Ok(None),
        Some(false) => Ok(Some(libc::ENOTDIR)),
        None => Ok(Some(libc::ENOENT)),
    }
}

#[cfg(all(test, target_os = "macos", feature = "remote-smb"))]
async fn wait_for_remote_failure(
    remote: &mut Option<RemoteController>,
    status: &mut impl FnMut(RemoteConnectionStatus),
) -> anyhow::Error {
    match remote {
        Some(remote) => loop {
            match remote.wait_event().await {
                RemoteControllerEvent::Connection(connection) => status(connection),
                RemoteControllerEvent::Failure(error) => return error,
            }
        },
        None => std::future::pending().await,
    }
}

#[cfg(all(target_os = "macos", feature = "remote-smb"))]
#[derive(Clone, serde::Serialize)]
struct RemoteConnectionLog {
    route: u32,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    errno: Option<libc::c_int>,
}

#[cfg(all(target_os = "macos", feature = "remote-smb"))]
fn remote_connection_log(
    remotes: &[SmbRemoteConfig],
    status: RemoteConnectionStatus,
) -> RemoteConnectionLog {
    let route = status.root();
    let Some(remote) = remotes.get(route as usize) else {
        return RemoteConnectionLog {
            route,
            status: "unknown",
            root: None,
            endpoint: None,
            errno: None,
        };
    };
    let mut endpoint = format!("smb://{}/{}", remote.server(), remote.share());
    if !remote.remote_path().is_empty() {
        endpoint.push('/');
        endpoint.push_str(remote.remote_path());
    }
    let (status, errno) = match status {
        RemoteConnectionStatus::Connected { .. } => ("connected", None),
        RemoteConnectionStatus::Unavailable { errno, .. } => ("unavailable", Some(errno)),
    };
    RemoteConnectionLog {
        route,
        status,
        root: Some(remote.logical_root().display().to_string()),
        endpoint: Some(endpoint),
        errno,
    }
}

#[cfg(all(target_os = "macos", feature = "remote-smb"))]
fn log_remote_connection_status(remotes: &[SmbRemoteConfig], status: RemoteConnectionStatus) {
    let event = remote_connection_log(remotes, status);
    let entry = LoggerEntry::new().with_entry("nfs", event.clone());
    match event.status {
        "connected" => logger::info!(entry = entry, "sandbox NFS connection status"),
        _ => logger::error!(entry = entry, "sandbox NFS connection status"),
    }
}

#[cfg(target_os = "macos")]
async fn terminate_process_group(
    child: &mut tokio::process::Child,
    process_group: libc::pid_t,
) -> Result<()> {
    signal_process_group(process_group, libc::SIGTERM)?;
    if child.try_wait()?.is_none() {
        let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
    }
    for _ in 0..10 {
        if !process_group_exists(process_group)? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    signal_process_group(process_group, libc::SIGKILL)?;
    if child.try_wait()?.is_none() {
        child.wait().await?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn signal_process_group(process_group: libc::pid_t, signal: libc::c_int) -> Result<()> {
    if unsafe { libc::kill(-process_group, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error).context("failed to signal sandbox process group")
    }
}

#[cfg(target_os = "macos")]
fn process_group_exists(process_group: libc::pid_t) -> Result<bool> {
    if unsafe { libc::kill(-process_group, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).context("failed to inspect sandbox process group"),
    }
}

#[derive(Debug)]
pub struct SandboxOutcome {
    status: ExitStatus,
    sandbox_id: String,
    run_id: String,
}

impl SandboxOutcome {
    pub(crate) fn new(status: ExitStatus, sandbox_id: String, run_id: String) -> Self {
        Self {
            status,
            sandbox_id,
            run_id,
        }
    }

    pub fn status(&self) -> ExitStatus {
        self.status
    }

    pub fn sandbox_id(&self) -> &str {
        &self.sandbox_id
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }
}

impl From<&OsStr> for SandboxCommand {
    fn from(program: &OsStr) -> Self {
        Self::new(program)
    }
}

#[cfg(test)]
mod tests;
