use super::*;

#[derive(Clone, Debug)]
pub(crate) struct ProtectedEnvironment {
    additions: BTreeMap<OsString, OsString>,
    removals: Vec<OsString>,
}

impl ProtectedEnvironment {
    fn new() -> Self {
        Self {
            additions: BTreeMap::new(),
            removals: vec![
                REMOTE_CONTROL.into(),
                REMOTE_TOKEN.into(),
                REMOTE_ROOTS.into(),
                REMOTE_CURRENT_DIRECTORY.into(),
                NATIVE_PASSTHROUGH_ROOTS.into(),
                LOCAL_FILESYSTEM_CONTROL.into(),
                LOCAL_FILESYSTEM_TOKEN.into(),
                INHERITED_LOCAL_DESCRIPTORS.into(),
                FILESYSTEM_CIPHER_KEY.into(),
                TLS_TRUST_ANCHOR_DER.into(),
                TLS_TRUST_BUNDLE.into(),
                JAVA_TRUST_STORE_ENVIRONMENT.into(),
            ],
        }
    }

    fn insert(&mut self, key: impl Into<OsString>, value: impl Into<OsString>) {
        self.additions.insert(key.into(), value.into());
    }

    pub(crate) fn additions(&self) -> &BTreeMap<OsString, OsString> {
        &self.additions
    }

    pub(crate) fn removals(&self) -> &[OsString] {
        &self.removals
    }

    pub(crate) fn from_parts(
        additions: BTreeMap<OsString, OsString>,
        removals: Vec<OsString>,
    ) -> Self {
        Self {
            additions,
            removals,
        }
    }

    pub(crate) fn apply(&self, command: &mut Command) {
        for key in &self.removals {
            command.env_remove(key);
        }
        command.envs(&self.additions);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedLaunch {
    program: PathBuf,
    argument_prefix: Vec<OsString>,
    environment: ProtectedEnvironment,
    launch_id: String,
}

impl PreparedLaunch {
    pub(crate) fn new(
        program: PathBuf,
        argument_prefix: Vec<OsString>,
        environment: ProtectedEnvironment,
        launch_id: String,
    ) -> Self {
        Self {
            program,
            argument_prefix,
            environment,
            launch_id,
        }
    }

    pub(crate) fn program(&self) -> &Path {
        &self.program
    }

    pub(crate) fn argument_prefix(&self) -> &[OsString] {
        &self.argument_prefix
    }

    pub(crate) fn environment(&self) -> &ProtectedEnvironment {
        &self.environment
    }

    pub(crate) fn launch_id(&self) -> &str {
        &self.launch_id
    }
}

pub(crate) struct SandboxRuntime {
    filesystem: FilesystemWorkspace,
    _runtime_directory: tempfile::TempDir,
    local_filesystem: Option<LocalController>,
    execution: ExecutionController,
    network: NetworkController,
    audit: AuditController,
    #[cfg(feature = "remote-smb")]
    remote: Option<RemoteController>,
    #[cfg(feature = "remote-smb")]
    smb_remotes: Vec<SmbRemoteConfig>,
    environment: ProtectedEnvironment,
    sandbox_id: String,
    run_id: String,
}

impl SandboxRuntime {
    pub(crate) async fn start<C>(config: SandboxConfig, callback: C) -> Result<Self>
    where
        C: Callback,
    {
        config.validate()?;
        let native_passthrough_roots =
            serde_json::to_string(&config.effective_native_passthrough_roots()?)
                .context("failed to encode native passthrough roots")?;
        let callback = std::sync::Arc::new(callback);
        let filesystem_workdir = config.workdir.clone();
        let filesystem_mode = config.filesystem_mode;
        let filesystem_key = config.encrypted_workspace_key().map(<[u8]>::to_vec);
        let filesystem = filesystem_blocking(move || {
            FilesystemWorkspace::start(
                &filesystem_workdir,
                filesystem_mode,
                filesystem_key.as_deref(),
            )
        })
        .await?;
        #[cfg(feature = "remote-smb")]
        let remote_preflight_errors = config
            .smb_remotes
            .iter()
            .map(|remote| remote_logical_parent_errno(&filesystem, remote.logical_root()))
            .collect::<Result<Vec<_>>>()?;
        let runtime_directory = tempfile::Builder::new()
            .prefix("agora-sandbox-run-")
            .tempdir_in("/tmp")
            .context("failed to create sandbox runtime directory")?;
        let filesystem_cipher = filesystem
            .encrypted_cipher_key()
            .map(|key| crate::filesystem::FileCipher::from_key(key))
            .transpose()?;
        let tls_ca_files = config.tls_ca_for_workdir()?;
        let hook_library = config.hook_library.canonicalize().with_context(|| {
            format!(
                "failed to resolve sandbox hook library {}",
                config.hook_library.display()
            )
        })?;
        let tls_ca = tls_ca_files
            .as_ref()
            .map(|ca| {
                let certificate_path = ca.certificate.canonicalize().with_context(|| {
                    format!(
                        "failed to resolve TLS CA certificate {}",
                        ca.certificate.display()
                    )
                })?;
                let certificate = std::fs::read(&certificate_path).with_context(|| {
                    format!(
                        "failed to read TLS CA certificate {}",
                        certificate_path.display()
                    )
                })?;
                let private_key = std::fs::read(&ca.private_key).with_context(|| {
                    format!(
                        "failed to read TLS CA private key {}",
                        ca.private_key.display()
                    )
                })?;
                let trust_bundle =
                    config.write_tls_trust_bundle(runtime_directory.path(), &certificate)?;
                let java_trust_store =
                    config.write_java_trust_store(runtime_directory.path(), &certificate)?;
                Ok::<_, anyhow::Error>((certificate, private_key, trust_bundle, java_trust_store))
            })
            .transpose()?;
        let injected_libraries = injected_libraries(&hook_library)?;
        #[cfg(feature = "remote-smb")]
        let remote_routes = config
            .smb_remotes
            .iter()
            .enumerate()
            .map(|(root, remote)| {
                Ok(RemoteRoute {
                    root: u32::try_from(root).context("too many SMB remote roots")?,
                    logical_root: remote.logical_root().to_string_lossy().into_owned(),
                })
            })
            .collect::<Result<Vec<_>>>()
            .and_then(|routes| serde_json::to_string(&routes).map_err(Into::into))?;
        let sandbox_id = Uuid::new_v4().to_string();
        let run_id = Uuid::new_v4().to_string();
        let mut local_filesystem = match filesystem_cipher.as_ref() {
            Some(cipher) => Some(
                LocalController::start(
                    filesystem.root(),
                    cipher.clone(),
                    &runtime_directory.path().join("filesystem"),
                )
                .await?,
            ),
            None => None,
        };
        let audit_callback = {
            let callback = std::sync::Arc::clone(&callback);
            move |event| {
                let callback = std::sync::Arc::clone(&callback);
                async move { callback.on_event(event).await }
            }
        };
        let audit = match AuditController::start(
            sandbox_id.clone(),
            run_id.clone(),
            audit_callback,
            config.network.callback_timeout,
        )
        .await
        {
            Ok(audit) => audit,
            Err(error) => {
                if let Some(local) = local_filesystem.take() {
                    let _ = local.shutdown().await;
                }
                return Err(error);
            }
        };
        let execution_result = match filesystem_cipher {
            Some(cipher) => {
                ExecutionController::start_encrypted(filesystem.root().to_path_buf(), cipher).await
            }
            None => ExecutionController::start(filesystem.root().to_path_buf()).await,
        };
        let execution = match execution_result {
            Ok(execution) => execution,
            Err(error) => {
                let _ = audit.shutdown().await;
                if let Some(local) = local_filesystem.take() {
                    let _ = local.shutdown().await;
                }
                return Err(error);
            }
        };
        let context = NetworkRunContext::new(&sandbox_id, &run_id);
        let network_callback = {
            let callback = std::sync::Arc::clone(&callback);
            move |event| {
                let callback = std::sync::Arc::clone(&callback);
                async move { callback.on_event(event).await }
            }
        };
        #[cfg(test)]
        let upstream_tls_roots = config.upstream_tls_roots.clone();
        #[cfg(test)]
        let network_result = match (tls_ca.as_ref(), upstream_tls_roots) {
            (Some((certificate, private_key, _, _)), Some(roots)) => {
                NetworkController::start_with_tls_ca_and_roots(
                    config.network,
                    context,
                    network_callback,
                    certificate,
                    private_key,
                    roots,
                )
                .await
            }
            (tls_ca, None) => match tls_ca {
                Some((certificate, private_key, _, _)) => {
                    NetworkController::start_with_tls_ca(
                        config.network,
                        context,
                        network_callback,
                        certificate,
                        private_key,
                    )
                    .await
                }
                None => NetworkController::start(config.network, context, network_callback).await,
            },
            (None, Some(_)) => Err(anyhow::anyhow!(
                "test upstream TLS roots require TLS interception"
            )),
        };
        #[cfg(not(test))]
        let network_result = match tls_ca.as_ref() {
            Some((certificate, private_key, _, _)) => {
                NetworkController::start_with_tls_ca(
                    config.network,
                    context,
                    network_callback,
                    certificate,
                    private_key,
                )
                .await
            }
            None => NetworkController::start(config.network, context, network_callback).await,
        };
        let network = match network_result {
            Ok(network) => network,
            Err(error) => {
                let _ = execution.shutdown().await;
                let _ = audit.shutdown().await;
                if let Some(local) = local_filesystem.take() {
                    let _ = local.shutdown().await;
                }
                return Err(error);
            }
        };
        #[cfg(feature = "remote-smb")]
        let remote = if config.smb_remotes.is_empty() {
            None
        } else {
            match crate::nfs::start_controller(
                &config.smb_remotes,
                &runtime_directory.path().join("nfs"),
                &remote_preflight_errors,
            )
            .await
            {
                Ok(remote) => Some(remote),
                Err(error) => {
                    let _ = network.shutdown().await;
                    let _ = execution.shutdown().await;
                    let _ = audit.shutdown().await;
                    if let Some(local) = local_filesystem.take() {
                        let _ = local.shutdown().await;
                    }
                    return Err(error);
                }
            }
        };
        let mut environment = ProtectedEnvironment::new();
        let network_runtime = network.runtime();
        let execution_runtime = execution.runtime();
        let audit_runtime = audit.runtime();
        environment.insert(TOKEN, network_runtime.token());
        environment.insert(PROXY_IPV4, network_runtime.proxy_ipv4().to_string());
        environment.insert(PROXY_IPV6, network_runtime.proxy_ipv6().to_string());
        environment.insert(EXECUTION_CONTROL, execution_runtime.control().to_string());
        environment.insert(EXECUTION_TOKEN, execution_runtime.token());
        environment.insert(AUDIT_CONTROL, audit_runtime.control().to_string());
        environment.insert(AUDIT_TOKEN, audit_runtime.token());
        environment.insert(HOOK_LIBRARIES, injected_libraries.clone());
        environment.insert(FILESYSTEM_ROOT, filesystem.root());
        environment.insert(NATIVE_PASSTHROUGH_ROOTS, native_passthrough_roots);
        environment.insert(
            FILESYSTEM_MODE,
            match config.filesystem_mode {
                FilesystemMode::Encrypted => "encrypted",
                FilesystemMode::Plain => "plain",
            },
        );
        environment.insert("DYLD_INSERT_LIBRARIES", injected_libraries);
        if let Some(local) = &local_filesystem {
            environment.insert(LOCAL_FILESYSTEM_CONTROL, local.runtime().socket());
            environment.insert(LOCAL_FILESYSTEM_TOKEN, local.runtime().token());
        }
        #[cfg(feature = "remote-smb")]
        if let Some(remote) = &remote {
            environment.insert(REMOTE_CONTROL, remote.runtime().socket());
            environment.insert(REMOTE_TOKEN, remote.runtime().token());
            environment.insert(REMOTE_ROOTS, remote_routes);
        }
        if let Some(key) = filesystem.encrypted_cipher_key() {
            environment.insert(
                FILESYSTEM_CIPHER_KEY,
                base64::engine::general_purpose::STANDARD.encode(key),
            );
        }
        if let Some(anchor) = network_runtime.tls_trust_anchor_der() {
            environment.insert(
                TLS_TRUST_ANCHOR_DER,
                base64::engine::general_purpose::STANDARD.encode(anchor),
            );
        }
        if let Some((_, _, trust_bundle, java_trust_store)) = &tls_ca {
            environment.insert(TLS_TRUST_BUNDLE, trust_bundle);
            for key in TLS_CLIENT_TRUST_ENVIRONMENT {
                environment.insert(key, trust_bundle);
            }
            environment.insert(JAVA_TRUST_STORE_ENVIRONMENT, java_trust_store);
        }
        Ok(Self {
            filesystem,
            _runtime_directory: runtime_directory,
            local_filesystem,
            execution,
            network,
            audit,
            #[cfg(feature = "remote-smb")]
            remote,
            #[cfg(feature = "remote-smb")]
            smb_remotes: config.smb_remotes,
            environment,
            sandbox_id,
            run_id,
        })
    }

    pub(crate) fn sandbox_id(&self) -> &str {
        &self.sandbox_id
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    pub(crate) async fn prepare(&self, executable: PathBuf) -> Result<PreparedLaunch> {
        let prepared = self.execution.prepare(executable).await?;
        let (program, argument_prefix) = if let Some(shebang) = resolve_shebang(&prepared)? {
            let interpreter = self.execution.prepare(shebang.interpreter).await?;
            let mut arguments = Vec::with_capacity(2);
            arguments.extend(shebang.argument);
            arguments.push(prepared.into_os_string());
            (interpreter, arguments)
        } else {
            (prepared, Vec::new())
        };
        let mut environment = self.environment.clone();
        environment.insert(TRACE_ID_ENVIRONMENT, TraceContext::root().encode());
        Ok(PreparedLaunch::new(
            program,
            argument_prefix,
            environment,
            Uuid::new_v4().to_string(),
        ))
    }

    async fn wait_event(&mut self) -> RuntimeEvent {
        let local_failure = async {
            match &mut self.local_filesystem {
                Some(controller) => controller.wait_failure().await,
                None => std::future::pending().await,
            }
        };
        #[cfg(feature = "remote-smb")]
        let remote_event = async {
            match &mut self.remote {
                Some(controller) => RuntimeEvent::Remote(controller.wait_event().await),
                None => std::future::pending().await,
            }
        };
        #[cfg(not(feature = "remote-smb"))]
        let remote_event = std::future::pending::<RuntimeEvent>();
        tokio::pin!(local_failure);
        tokio::pin!(remote_event);
        tokio::select! {
            error = self.network.wait_failure() => {
                RuntimeEvent::Failure(error.context("sandbox network proxy failed"))
            }
            error = self.execution.wait_failure() => {
                RuntimeEvent::Failure(error.context("sandbox execution controller failed"))
            }
            error = self.audit.wait_failure() => {
                RuntimeEvent::Failure(error.context("sandbox audit controller failed"))
            }
            error = &mut local_failure => {
                RuntimeEvent::Failure(error.context("sandbox local filesystem failed"))
            }
            event = &mut remote_event => event,
        }
    }

    pub(crate) async fn wait_failure(&mut self) -> anyhow::Error {
        #[cfg(feature = "remote-smb")]
        loop {
            match self.wait_event().await {
                RuntimeEvent::Failure(error) => return error,
                RuntimeEvent::Remote(RemoteControllerEvent::Connection(status)) => {
                    log_remote_connection_status(&self.smb_remotes, status);
                }
                RuntimeEvent::Remote(RemoteControllerEvent::Failure(error)) => {
                    return error.context("sandbox remote filesystem failed");
                }
            }
        }

        #[cfg(not(feature = "remote-smb"))]
        match self.wait_event().await {
            RuntimeEvent::Failure(error) => error,
        }
    }

    pub(crate) async fn shutdown(mut self) -> Result<()> {
        let mut first_error = None;
        record_shutdown(&mut first_error, self.network.shutdown().await);
        record_shutdown(&mut first_error, self.execution.shutdown().await);
        record_shutdown(&mut first_error, self.audit.shutdown().await);
        if let Some(local) = self.local_filesystem.take() {
            record_shutdown(&mut first_error, local.shutdown().await);
        }
        #[cfg(feature = "remote-smb")]
        if let Some(remote) = self.remote.take() {
            record_shutdown(&mut first_error, remote.shutdown().await);
        }
        drop(self.filesystem);
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

enum RuntimeEvent {
    Failure(anyhow::Error),
    #[cfg(feature = "remote-smb")]
    Remote(RemoteControllerEvent),
}

fn record_shutdown(first: &mut Option<anyhow::Error>, result: Result<()>) {
    if let Err(error) = result
        && first.is_none()
    {
        *first = Some(error);
    }
}

pub(crate) struct RunningSandboxCommand {
    child: tokio::process::Child,
    process_group: Option<libc::pid_t>,
    terminal: Option<ForegroundTerminal>,
}

impl RunningSandboxCommand {
    pub(crate) fn spawn(
        mut command: SandboxCommand,
        launch: &PreparedLaunch,
        foreground: bool,
    ) -> Result<Self> {
        let inherited_libraries = command.effective_environment("DYLD_INSERT_LIBRARIES");
        let inherited_java_options = command.effective_environment(JAVA_TOOL_OPTIONS_ENVIRONMENT);
        let inherited_java_store = command.effective_environment(JAVA_TRUST_STORE_ENVIRONMENT);
        command.apply_prepared(launch);
        let mut child = command.into_command();
        child.kill_on_drop(true);
        launch.environment.apply(&mut child);
        if let Some(java_store) = launch
            .environment
            .additions()
            .get(OsStr::new(JAVA_TRUST_STORE_ENVIRONMENT))
        {
            let options = merged_java_tool_options(
                inherited_java_options
                    .as_deref()
                    .map(std::os::unix::ffi::OsStrExt::as_bytes),
                inherited_java_store
                    .as_deref()
                    .map(std::os::unix::ffi::OsStrExt::as_bytes),
                java_store.as_os_str().as_bytes(),
            );
            child.env(JAVA_TOOL_OPTIONS_ENVIRONMENT, OsString::from_vec(options));
        }
        if let Some(inherited) = inherited_libraries {
            let base = launch
                .environment
                .additions()
                .get(OsStr::new("DYLD_INSERT_LIBRARIES"))
                .context("prepared launch has no sandbox hook libraries")?;
            let mut libraries = std::env::split_paths(base).collect::<Vec<_>>();
            for library in std::env::split_paths(&inherited) {
                if !libraries.contains(&library) {
                    libraries.push(library);
                }
            }
            let libraries = std::env::join_paths(libraries)
                .context("invalid inherited DYLD_INSERT_LIBRARIES path")?;
            child
                .env("DYLD_INSERT_LIBRARIES", &libraries)
                .env(HOOK_LIBRARIES, libraries);
        }
        child.as_std_mut().process_group(0);
        let mut terminal = if foreground {
            ForegroundTerminal::capture()?
        } else {
            None
        };
        let child = child.spawn().context("failed to start sandbox child")?;
        let process_group = child
            .id()
            .and_then(|id| libc::pid_t::try_from(id).ok())
            .context("sandbox child has no valid process id")?;
        if let Some(terminal) = terminal.as_mut()
            && let Err(error) = terminal.handoff(process_group)
        {
            let _ = signal_process_group(process_group, libc::SIGKILL);
            return Err(error);
        }
        Ok(Self {
            child,
            process_group: Some(process_group),
            terminal,
        })
    }

    pub(crate) fn take_stdio(
        &mut self,
    ) -> (
        Option<tokio::process::ChildStdin>,
        Option<tokio::process::ChildStdout>,
        Option<tokio::process::ChildStderr>,
    ) {
        (
            self.child.stdin.take(),
            self.child.stdout.take(),
            self.child.stderr.take(),
        )
    }

    pub(crate) fn id(&self) -> Option<u32> {
        self.child.id()
    }

    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub(crate) async fn kill(&mut self) -> Result<()> {
        let process_group = self
            .process_group
            .context("running sandbox command has no process group")?;
        signal_process_group(process_group, libc::SIGKILL)?;
        if self.child.try_wait()?.is_none() {
            self.child.wait().await?;
        }
        Ok(())
    }

    pub(crate) async fn wait_or_failure<F>(&mut self, failure: F) -> Result<ExitStatus>
    where
        F: std::future::Future<Output = anyhow::Error>,
    {
        tokio::pin!(failure);
        enum Completion {
            Child(std::io::Result<ExitStatus>),
            Runtime(anyhow::Error),
        }
        let completion = tokio::select! {
            status = self.child.wait() => Completion::Child(status),
            error = &mut failure => Completion::Runtime(error),
        };
        let result = match completion {
            Completion::Child(status) => status.context("sandbox child wait failed"),
            Completion::Runtime(error) => Err(error),
        };
        let process_group = self
            .process_group
            .context("running sandbox command has no process group")?;
        let termination = terminate_process_group(&mut self.child, process_group).await;
        if termination.is_ok() {
            self.process_group = None;
        }
        let terminal_restore = self
            .terminal
            .as_mut()
            .map(ForegroundTerminal::restore)
            .transpose();
        match result {
            Ok(status) => {
                termination?;
                terminal_restore?;
                Ok(status)
            }
            Err(error) => {
                let _ = termination;
                let _ = terminal_restore;
                Err(error)
            }
        }
    }
}

impl Drop for RunningSandboxCommand {
    fn drop(&mut self) {
        if let Some(process_group) = self.process_group.take() {
            let _ = signal_process_group(process_group, libc::SIGKILL);
        }
        if let Some(terminal) = self.terminal.as_mut() {
            let _ = terminal.restore();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn dropping_running_command_terminates_its_entire_process_group() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "trap '' TERM; sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let mut child = command.spawn().unwrap();
        let process_group = child.id().unwrap() as libc::pid_t;
        let stdout = child.stdout.take().unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            BufReader::new(stdout).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let descendant = line.trim().parse::<libc::pid_t>().unwrap();
        assert_eq!(unsafe { libc::getpgid(descendant) }, process_group);

        drop(RunningSandboxCommand {
            child,
            process_group: Some(process_group),
            terminal: None,
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while process_group_exists(process_group).unwrap() && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let survived = process_group_exists(process_group).unwrap();
        if survived {
            signal_process_group(process_group, libc::SIGKILL).unwrap();
        }

        assert!(!survived, "a descendant survived command cancellation");
    }
}
