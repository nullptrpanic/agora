use super::{
    FilesystemMode, RuntimeServices, Sandbox, SandboxCommand, SandboxConfig, SandboxOutcome,
    SecretBytes, SmbRemoteConfig, process_group_exists, signal_process_group,
    terminate_process_group, wait_for_child_or_service,
};
use crate::audit::AuditController;
use crate::callback::{Decision, Event, EventType, NoopCallback, TlsOutcome};
use crate::execution::ExecutionController;
#[cfg(target_os = "macos")]
use crate::filesystem::{EncryptedWorkspace, FilesystemWorkspace};
use crate::network::{NetworkConfig, NetworkController, NetworkRunContext, TlsMode};
#[cfg(feature = "remote-smb")]
use crate::nfs::{
    controller::{RemoteConnectionStatus, RemoteController},
    testing::MemoryStorage,
};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "current_thread")]
async fn filesystem_blocking_runs_on_a_blocking_worker() {
    let caller = std::thread::current().id();

    let worker = super::filesystem_blocking(|| Ok::<_, anyhow::Error>(std::thread::current().id()))
        .await
        .unwrap();

    assert_ne!(worker, caller);
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "current_thread")]
async fn filesystem_key_migration_reports_progress_on_the_runtime_thread() {
    let workdir = std::env::temp_dir().join(format!(
        "agora-runner-key-migration-{}",
        uuid::Uuid::new_v4()
    ));
    drop(EncryptedWorkspace::start(&workdir, b"old-key").unwrap());
    let runtime_thread = std::thread::current().id();
    let stages = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let observed = std::rc::Rc::clone(&stages);

    super::migrate_filesystem_key_with_progress(&workdir, b"old-key", b"new-key", |stage| {
        assert_eq!(std::thread::current().id(), runtime_thread);
        observed.borrow_mut().push(stage);
    })
    .await
    .unwrap();

    assert_eq!(
        stages.borrow().last(),
        Some(&super::FilesystemKeyMigrationProgress::Completed)
    );
    std::fs::remove_dir_all(workdir).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "current_thread")]
async fn filesystem_key_migration_runs_without_a_progress_callback() {
    let workdir = std::env::temp_dir().join(format!(
        "agora-runner-key-migration-simple-{}",
        uuid::Uuid::new_v4()
    ));
    drop(EncryptedWorkspace::start(&workdir, b"old-key").unwrap());

    super::migrate_filesystem_key(&workdir, b"old-key", b"new-key")
        .await
        .unwrap();

    drop(EncryptedWorkspace::start(&workdir, b"new-key").unwrap());
    std::fs::remove_dir_all(workdir).unwrap();
}

fn sleeping_child() -> tokio::process::Child {
    let mut command = tokio::process::Command::new("/bin/sleep");
    command.arg("30").kill_on_drop(true);
    command.as_std_mut().process_group(0);
    command.spawn().unwrap()
}

#[cfg(target_os = "macos")]
fn built_hook_library() -> PathBuf {
    static HOOK: OnceLock<PathBuf> = OnceLock::new();
    HOOK.get_or_init(|| {
        if std::env::var_os("CARGO_LLVM_COV").is_some() {
            let library = std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .join("libagora_sandbox.dylib");
            assert!(library.is_file(), "missing {}", library.display());
            return library;
        }
        let workdir =
            std::env::temp_dir().join(format!("agora-sandbox-unit-hook-{}", std::process::id()));
        crate::hook_library::materialize(&workdir).unwrap()
    })
    .clone()
}

#[cfg(target_os = "macos")]
async fn local_https_origin(
    identity: &str,
) -> (
    std::net::SocketAddr,
    rustls::pki_types::CertificateDer<'static>,
    tokio::task::JoinHandle<()>,
) {
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose,
    };
    use rustls::ServerConfig;
    use rustls::pki_types::PrivatePkcs8KeyDer;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsAcceptor;

    let root_key = KeyPair::generate().unwrap();
    let mut root_params = CertificateParams::new(vec!["Agora Origin Test CA".to_string()]).unwrap();
    root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    root_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let issuer = CertifiedIssuer::self_signed(root_params, root_key).unwrap();
    let origin_root = issuer.der().clone();
    let server_key = KeyPair::generate().unwrap();
    let mut server_params = CertificateParams::new(vec![identity.to_string()]).unwrap();
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_certificate = server_params.signed_by(&server_key, &issuer).unwrap();
    let mut server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![server_certificate.der().clone(), origin_root.clone()],
            PrivatePkcs8KeyDer::from(server_key.serialize_der()).into(),
        )
        .unwrap();
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = acceptor.accept(stream).await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut bytes = [0_u8; 1024];
            let read = stream.read(&mut bytes).await.unwrap();
            assert_ne!(read, 0);
            request.extend_from_slice(&bytes[..read]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
    });
    (address, origin_root, task)
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn system_curl_completes_the_transparent_tls_chain() {
    let identity = "origin.agora.test";
    let (origin, origin_root, origin_task) = local_https_origin(identity).await;
    let root = std::env::temp_dir().join(format!("agora-curl-tls-{}", uuid::Uuid::new_v4()));
    let source = root.join("source");
    let workdir = root.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    let output = source.join("response.txt");
    let events = Arc::new(Mutex::new(Vec::new()));
    let callback = {
        let events = Arc::clone(&events);
        move |event| {
            events.lock().unwrap().push(event);
            std::future::ready(Decision::Allow)
        }
    };
    let mut config = SandboxConfig::new(built_hook_library())
        .with_workdir(&workdir)
        .with_encrypted_workspace("test-filesystem-key")
        .with_upstream_tls_roots(vec![origin_root]);
    config.network.tls = TlsMode::Auto;
    let url = format!("https://{identity}:{}/", origin.port());
    let resolve = format!("{identity}:{}:127.0.0.1", origin.port());
    let script = format!(
        "/usr/bin/curl --silent --show-error --fail --connect-timeout 5 --max-time 10 --resolve {resolve} {url} --output {}",
        output.display()
    );
    let command = SandboxCommand::new("/bin/bash").args(["-c", script.as_str()]);

    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        Sandbox::new(config, callback).run(command),
    )
    .await
    .unwrap()
    .unwrap();

    assert!(
        outcome.status().success(),
        "curl exited with {:?}; events: {:#?}",
        outcome.status().code(),
        events.lock().unwrap()
    );
    tokio::time::timeout(Duration::from_secs(2), origin_task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !output.exists(),
        "sandbox output unexpectedly changed the host filesystem"
    );

    let events = events.lock().unwrap();
    let process = events
        .iter()
        .find_map(|event| match event {
            Event::Process(event) if event.command.executable == "/usr/bin/curl" => Some(event),
            Event::Network(_) | Event::Process(_) | Event::File(_) => None,
        })
        .expect("curl process event");
    let established = events
        .iter()
        .find_map(|event| match event {
            Event::Network(event)
                if event.event_type == EventType::NetworkConnectEstablished
                    && event
                        .network
                        .as_ref()
                        .is_some_and(|network| network.destination_port == origin.port()) =>
            {
                Some(event)
            }
            Event::Network(_) | Event::Process(_) | Event::File(_) => None,
        })
        .expect("curl TLS connection event");
    assert_eq!(process.trace_id, established.trace_id);
    assert!(process.trace_id.split(',').count() >= 2);
    assert_eq!(
        established.tls.as_ref().map(|tls| tls.outcome),
        Some(TlsOutcome::Terminated)
    );
    assert_eq!(
        established
            .network
            .as_ref()
            .and_then(|network| network.domain.as_deref()),
        Some(identity)
    );
    drop(events);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn encrypted_overlay_preserves_the_host_while_the_child_uses_cow_and_whiteouts() {
    let root = std::env::temp_dir().join(format!("agora-overlay-run-{}", uuid::Uuid::new_v4()));
    let source = root.join("source");
    let workdir = root.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    let existing = source.join("existing");
    let removed = source.join("removed");
    let created = source.join("created");
    let directory = source.join("directory");
    std::fs::write(&existing, b"host").unwrap();
    std::fs::write(&removed, b"host removed").unwrap();
    let script = format!(
        "set -eu; test \"$(cat '{existing}')\" = host; printf sandbox > '{existing}'; test \"$(cat '{existing}')\" = sandbox; printf created > '{created}'; test \"$(cat '{created}')\" = created; rm '{removed}'; test ! -e '{removed}'; mkdir '{directory}'; printf nested > '{directory}/nested'; test \"$(cat '{directory}/nested')\" = nested",
        existing = existing.display(),
        created = created.display(),
        removed = removed.display(),
        directory = directory.display(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let callback = {
        let events = Arc::clone(&events);
        move |event| {
            events.lock().unwrap().push(event);
            std::future::ready(Decision::Allow)
        }
    };
    let config = SandboxConfig::new(built_hook_library())
        .with_workdir(&workdir)
        .with_encrypted_workspace("test-filesystem-key");
    let outcome = Sandbox::new(config, callback)
        .run(SandboxCommand::new("/bin/bash").args(["-c", script.as_str()]))
        .await
        .unwrap();

    assert!(
        outcome.status().success(),
        "sandbox child exited with {}",
        outcome.status()
    );
    assert_eq!(std::fs::read(&existing).unwrap(), b"host");
    assert_eq!(std::fs::read(&removed).unwrap(), b"host removed");
    assert!(!created.exists());
    assert!(!directory.exists());
    let events = events.lock().unwrap();
    let file_events = events
        .iter()
        .filter_map(|event| match event {
            Event::File(event) if event.file.path == existing.to_string_lossy() => Some(event),
            Event::Network(_) | Event::Process(_) | Event::File(_) => None,
        })
        .collect::<Vec<_>>();
    assert!(
        file_events
            .iter()
            .any(|event| event.event_type == EventType::FilesystemOpen)
    );
    assert!(
        file_events
            .iter()
            .any(|event| event.event_type == EventType::FilesystemClose)
    );
    drop(events);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn tls_interception_starts_with_native_upstream_roots() {
    let root =
        std::env::temp_dir().join(format!("agora-native-tls-roots-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let mut config = SandboxConfig::new(built_hook_library())
        .with_workdir(&root)
        .with_encrypted_workspace("test-filesystem-key");
    config.network.tls = TlsMode::Auto;

    let outcome = Sandbox::new(config, NoopCallback)
        .run(SandboxCommand::new("/usr/bin/true"))
        .await
        .unwrap();

    assert!(outcome.status().success());
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn test_upstream_roots_require_tls_interception() {
    let root =
        std::env::temp_dir().join(format!("agora-roots-without-tls-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let config = SandboxConfig::new(built_hook_library())
        .with_workdir(&root)
        .with_encrypted_workspace("test-filesystem-key")
        .with_upstream_tls_roots(Vec::new());

    let error = Sandbox::new(config, NoopCallback)
        .run(SandboxCommand::new("/usr/bin/true"))
        .await
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("test upstream TLS roots require TLS interception")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn sandbox_outcome_exposes_status_and_identifiers() {
    let status = std::process::Command::new("/usr/bin/true")
        .status()
        .unwrap();
    let outcome = SandboxOutcome {
        status,
        sandbox_id: "sandbox-id".to_string(),
        run_id: "run-id".to_string(),
    };

    assert!(outcome.status().success());
    assert_eq!(outcome.sandbox_id(), "sandbox-id");
    assert_eq!(outcome.run_id(), "run-id");
}

#[test]
fn sandbox_config_and_command_builders_preserve_runtime_inputs() {
    let missing_hook = std::env::temp_dir().join("agora-missing-hook.dylib");
    let config = SandboxConfig::new(&missing_hook);
    assert_eq!(config.hook_library(), missing_hook);
    let expected_workdir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agora-sandbox");
    assert_eq!(config.workdir(), expected_workdir);
    assert_eq!(
        config.clone().with_workdir("/tmp/agora-cache").workdir(),
        Path::new("/tmp/agora-cache")
    );
    assert_eq!(config.tls_ca(), None);
    assert_eq!(config.filesystem_mode(), FilesystemMode::Plain);
    let encrypted = config.clone().with_encrypted_workspace("top secret");
    assert_eq!(
        encrypted.encrypted_workspace_key(),
        Some(b"top secret".as_slice())
    );
    assert!(!format!("{encrypted:?}").contains("top secret"));
    let plain = encrypted.with_plain_workspace();
    assert_eq!(plain.filesystem_mode(), FilesystemMode::Plain);
    assert_eq!(plain.encrypted_workspace_key(), None);
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("hook library does not exist")
    );

    let command = SandboxCommand::new("sh")
        .arg("-c")
        .args(["printf", "ok"])
        .env("KEY", "value")
        .current_dir("/tmp");
    assert_eq!(command.program, "sh");
    assert_eq!(command.arguments, ["-c", "printf", "ok"]);
    assert_eq!(command.environment.get(OsStr::new("KEY")).unwrap(), "value");
    assert_eq!(command.current_dir.as_deref(), Some(Path::new("/tmp")));
    assert_eq!(
        command.clone().into_command().as_std().get_current_dir(),
        Some(Path::new("/tmp"))
    );
    assert_eq!(
        SandboxCommand::from(OsStr::new("/bin/sh")).program,
        "/bin/sh"
    );
}

#[test]
fn smb_remote_config_normalizes_paths_and_redacts_credentials() {
    let remote = SmbRemoteConfig::new("/remote/./team", "files.example.com", "documents")
        .unwrap()
        .with_remote_path("/projects/./current/")
        .unwrap()
        .with_domain("CORP")
        .with_credentials("alice", "top secret");

    assert_eq!(remote.logical_root(), Path::new("/remote/team"));
    assert_eq!(remote.server(), "files.example.com:445");
    assert_eq!(remote.share(), "documents");
    assert_eq!(remote.remote_path(), "projects/current");
    assert_eq!(remote.domain(), "CORP");
    assert_eq!(remote.username(), "alice");
    assert!(!format!("{remote:?}").contains("top secret"));
    assert_eq!(
        SmbRemoteConfig::new("/explicit-port", "files.example.com:1445", "documents")
            .unwrap()
            .server(),
        "files.example.com:1445"
    );
}

#[test]
fn smb_remote_config_rejects_ipv6_literal_endpoints() {
    for server in [
        "2001:db8::1",
        "[2001:db8::1]",
        "[2001:db8::1]:445",
        "[2001:db8::1]:1445",
    ] {
        let error = SmbRemoteConfig::new("/remote", server, "documents").unwrap_err();
        assert!(
            error.to_string().contains("IPv6 literals are unsupported"),
            "unexpected error for {server}: {error:#}"
        );
    }
}

#[test]
fn smb_remote_config_rejects_unsafe_roots_and_remote_paths() {
    assert!(SmbRemoteConfig::new("relative", "server", "share").is_err());
    assert!(SmbRemoteConfig::new("/", "server", "share").is_err());
    assert!(SmbRemoteConfig::new("/remote/../escape", "server", "share").is_err());
    assert!(SmbRemoteConfig::new("/remote", "", "share").is_err());
    assert!(SmbRemoteConfig::new("/remote", "server:invalid", "share").is_err());
    assert!(SmbRemoteConfig::new("/remote", "server", "bad/share").is_err());
    assert!(
        SmbRemoteConfig::new("/remote", "server", "share")
            .unwrap()
            .with_remote_path("../escape")
            .is_err()
    );
    for path in ["bad\\path", "bad\0path"] {
        assert!(
            SmbRemoteConfig::new("/remote", "server", "share")
                .unwrap()
                .with_remote_path(path)
                .is_err()
        );
    }
}

#[cfg(feature = "remote-smb")]
#[test]
fn nfs_connection_status_log_is_structured_and_redacts_credentials() {
    let remote = SmbRemoteConfig::new("/smb", "files.example.com", "documents")
        .unwrap()
        .with_remote_path("projects/current")
        .unwrap()
        .with_credentials("alice", "top secret");
    let remotes = [remote];
    let connected = serde_json::to_value(super::remote_connection_log(
        &remotes,
        RemoteConnectionStatus::Connected { root: 0 },
    ))
    .unwrap();
    let unknown = serde_json::to_value(super::remote_connection_log(
        &remotes,
        RemoteConnectionStatus::Connected { root: 9 },
    ))
    .unwrap();
    let unavailable = serde_json::to_value(super::remote_connection_log(
        &remotes,
        RemoteConnectionStatus::Unavailable {
            root: 0,
            errno: libc::EACCES,
        },
    ))
    .unwrap();

    assert_eq!(connected["route"], 0);
    assert_eq!(connected["root"], "/smb");
    assert_eq!(
        connected["endpoint"],
        "smb://files.example.com:445/documents/projects/current"
    );
    assert_eq!(connected["status"], "connected");
    assert!(connected.get("errno").is_none());
    assert_eq!(unknown["route"], 9);
    assert_eq!(unknown["status"], "unknown");
    assert!(unknown.get("root").is_none());
    assert_eq!(unavailable["status"], "unavailable");
    assert_eq!(unavailable["errno"], libc::EACCES);
    let logs = format!("{connected}{unknown}{unavailable}");
    assert!(!logs.contains("alice"));
    assert!(!logs.contains("top secret"));
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_config_rejects_remote_root_collisions() {
    let root = std::env::temp_dir().join(format!("agora-remote-config-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let hook = root.join("hook.dylib");
    std::fs::write(&hook, b"hook").unwrap();

    let duplicate = SandboxConfig::new(&hook)
        .with_workdir(root.join("workspace"))
        .with_smb_remote(SmbRemoteConfig::new("/remote", "one", "share").unwrap())
        .with_smb_remote(SmbRemoteConfig::new("/remote", "two", "share").unwrap());
    assert!(
        duplicate
            .validate()
            .unwrap_err()
            .to_string()
            .contains("overlap")
    );

    let nested = SandboxConfig::new(&hook)
        .with_workdir(root.join("workspace"))
        .with_smb_remote(SmbRemoteConfig::new("/remote", "one", "share").unwrap())
        .with_smb_remote(SmbRemoteConfig::new("/remote/team", "two", "share").unwrap());
    assert!(
        nested
            .validate()
            .unwrap_err()
            .to_string()
            .contains("overlap")
    );

    let native = SandboxConfig::new(&hook)
        .with_workdir(root.join("workspace"))
        .with_smb_remote(SmbRemoteConfig::new("/dev/remote", "one", "share").unwrap());
    assert!(native.validate().unwrap_err().to_string().contains("/dev"));

    let private = SandboxConfig::new(&hook)
        .with_workdir(root.join("workspace"))
        .with_smb_remote(SmbRemoteConfig::new(root.join("workspace/fs"), "one", "share").unwrap());
    assert!(
        private
            .validate()
            .unwrap_err()
            .to_string()
            .contains("work directory")
    );

    let sibling = SandboxConfig::new(&hook)
        .with_workdir(root.join("workspace"))
        .with_smb_remote(
            SmbRemoteConfig::new(root.join("workspace/remote"), "one", "share").unwrap(),
        );
    assert!(
        sibling
            .validate()
            .unwrap_err()
            .to_string()
            .contains("work directory")
    );

    let dotted = SandboxConfig::new(&hook)
        .with_workdir(root.join("state/../workspace"))
        .with_smb_remote(
            SmbRemoteConfig::new(root.join("workspace/remote"), "one", "share").unwrap(),
        );
    assert!(
        dotted
            .validate()
            .unwrap_err()
            .to_string()
            .contains("work directory")
    );

    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let alias = root.join("workspace-alias");
    std::os::unix::fs::symlink(&workspace, &alias).unwrap();
    let aliased = SandboxConfig::new(&hook)
        .with_workdir(&workspace)
        .with_smb_remote(SmbRemoteConfig::new(alias.join("remote"), "one", "share").unwrap());
    assert!(
        aliased
            .validate()
            .unwrap_err()
            .to_string()
            .contains("work directory")
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_config_validates_native_passthrough_roots() {
    let root = std::env::temp_dir().join(format!(
        "agora-native-passthrough-config-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let hook = root.join("hook.dylib");
    std::fs::write(&hook, b"hook").unwrap();
    let workdir = root.join("workspace");

    let valid = SandboxConfig::new(&hook)
        .with_workdir(&workdir)
        .with_native_passthrough_root("/opt/agora-tools");
    assert_eq!(
        valid.native_passthrough_roots(),
        [PathBuf::from("/dev"), PathBuf::from("/opt/agora-tools")]
    );
    assert!(valid.validate().is_ok());

    let relative = SandboxConfig::new(&hook)
        .with_workdir(&workdir)
        .with_native_passthrough_root("relative/tools");
    assert!(
        relative
            .validate()
            .unwrap_err()
            .to_string()
            .contains("must be absolute")
    );

    let private = SandboxConfig::new(&hook)
        .with_workdir(&workdir)
        .with_native_passthrough_root(workdir.join("fs"));
    assert!(
        private
            .validate()
            .unwrap_err()
            .to_string()
            .contains("work directory")
    );

    let remote = SandboxConfig::new(&hook)
        .with_workdir(&workdir)
        .with_native_passthrough_root("/remote/tools")
        .with_smb_remote(SmbRemoteConfig::new("/remote", "server", "share").unwrap());
    assert!(
        remote
            .validate()
            .unwrap_err()
            .to_string()
            .contains("SMB logical root")
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(target_os = "macos", feature = "remote-smb"))]
#[test]
fn remote_connection_probe_requires_a_visible_logical_parent_directory() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workspace");
    let filesystem = FilesystemWorkspace::start(&workdir, FilesystemMode::Plain, None).unwrap();
    let existing = root.path().join("existing");
    std::fs::create_dir(&existing).unwrap();
    let regular_file = root.path().join("regular-file");
    std::fs::write(&regular_file, b"not a directory").unwrap();

    assert_eq!(
        super::remote_logical_parent_errno(&filesystem, &existing.join("smb")).unwrap(),
        None
    );
    assert_eq!(
        super::remote_logical_parent_errno(&filesystem, &root.path().join("missing").join("smb"))
            .unwrap(),
        Some(libc::ENOENT)
    );
    assert_eq!(
        super::remote_logical_parent_errno(&filesystem, &regular_file.join("smb")).unwrap(),
        Some(libc::ENOTDIR)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_config_defaults_to_plain_without_a_filesystem_key() {
    let root = std::env::temp_dir().join(format!(
        "agora-required-filesystem-key-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let hook = root.join("hook.dylib");
    std::fs::write(&hook, b"hook").unwrap();

    let config = SandboxConfig::new(&hook);

    assert!(config.validate().is_ok());
    assert_eq!(config.filesystem_mode(), FilesystemMode::Plain);
    assert_eq!(config.encrypted_workspace_key(), None);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_config_allows_an_explicit_plain_workspace_without_a_key() {
    let root = std::env::temp_dir().join(format!(
        "agora-plain-filesystem-config-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let hook = root.join("hook.dylib");
    std::fs::write(&hook, b"hook").unwrap();

    let config = SandboxConfig::new(&hook).with_plain_workspace();

    assert!(config.validate().is_ok());
    assert_eq!(config.filesystem_mode(), FilesystemMode::Plain);
    assert_eq!(config.encrypted_workspace_key(), None);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_config_rejects_an_encrypted_key_in_plain_mode() {
    let root = std::env::temp_dir().join(format!(
        "agora-plain-filesystem-key-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let hook = root.join("hook.dylib");
    std::fs::write(&hook, b"hook").unwrap();
    let mut config = SandboxConfig::new(&hook).with_plain_workspace();
    config.encrypted_workspace_key = Some(SecretBytes::new("unexpected"));

    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("cannot be used with plain filesystem mode")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_config_rejects_encrypted_mode_without_a_key_and_resolves_relative_workdirs() {
    let root = std::env::temp_dir().join(format!(
        "agora-encrypted-filesystem-missing-key-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let hook = root.join("hook.dylib");
    std::fs::write(&hook, b"hook").unwrap();

    let mut missing_key = SandboxConfig::new(&hook);
    missing_key.filesystem_mode = FilesystemMode::Encrypted;
    assert!(
        missing_key
            .validate()
            .unwrap_err()
            .to_string()
            .contains("filesystem key is required")
    );

    let relative = SandboxConfig::new(&hook).with_workdir("relative-sandbox-workdir");
    assert!(relative.validate().is_ok());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn command_workdir_resolution_and_disabled_tls_defaults_are_explicit() {
    let current = std::env::current_dir().unwrap().canonicalize().unwrap();
    assert_eq!(
        SandboxCommand::new("/bin/true")
            .effective_current_dir()
            .unwrap(),
        current
    );
    assert_eq!(
        SandboxCommand::new("/bin/true")
            .current_dir(".")
            .effective_current_dir()
            .unwrap(),
        current
    );

    let root = std::env::temp_dir().join(format!("agora-workdir-{}", uuid::Uuid::new_v4()));
    assert!(
        SandboxCommand::new("/bin/true")
            .current_dir(&root)
            .effective_current_dir()
            .unwrap_err()
            .to_string()
            .contains("failed to resolve sandbox command workdir")
    );
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("not-a-directory");
    std::fs::write(&file, b"file").unwrap();
    assert!(
        SandboxCommand::new("/bin/true")
            .current_dir(&file)
            .effective_current_dir()
            .unwrap_err()
            .to_string()
            .contains("not a directory")
    );

    let config = SandboxConfig::new(root.join("unused-hook"));
    assert!(config.tls_ca_for_workdir().unwrap().is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn sandbox_config_allows_default_tls_ca_for_interception() {
    let root = std::env::temp_dir().join(format!("agora-missing-ca-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let hook = root.join("hook.dylib");
    std::fs::write(&hook, b"hook").unwrap();
    let mut config = SandboxConfig::new(&hook).with_encrypted_workspace("test-key");
    config.network.tls = TlsMode::Auto;

    assert!(config.validate().is_ok());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn configured_tls_ca_reuses_a_complete_pair_and_replaces_a_partial_pair() {
    let root =
        std::env::temp_dir().join(format!("agora-missing-ca-files-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let hook = root.join("hook.dylib");
    let certificate = root.join("ca.pem");
    let private_key = root.join("ca-key.pem");
    std::fs::write(&hook, b"hook").unwrap();
    let mut config = SandboxConfig::new(&hook)
        .with_encrypted_workspace("test-key")
        .with_tls_ca(&certificate, &private_key);
    config.network.tls = TlsMode::Auto;

    assert!(config.validate().is_ok());
    let first = config.tls_ca_for_workdir().unwrap().unwrap();
    let first_certificate = std::fs::read(&first.certificate).unwrap();
    let first_private_key = std::fs::read(&first.private_key).unwrap();
    assert!(first_certificate.starts_with(b"-----BEGIN CERTIFICATE-----"));
    assert!(first_private_key.starts_with(b"-----BEGIN PRIVATE KEY-----"));

    let reused = config.tls_ca_for_workdir().unwrap().unwrap();
    assert_eq!(
        std::fs::read(&reused.certificate).unwrap(),
        first_certificate
    );
    assert_eq!(
        std::fs::read(&reused.private_key).unwrap(),
        first_private_key
    );

    std::fs::remove_file(&reused.private_key).unwrap();
    let replaced = config.tls_ca_for_workdir().unwrap().unwrap();
    assert_ne!(
        std::fs::read(&replaced.certificate).unwrap(),
        first_certificate
    );
    assert_ne!(
        std::fs::read(&replaced.private_key).unwrap(),
        first_private_key
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_reports_unreadable_configured_tls_files() {
    use std::os::unix::fs::PermissionsExt;

    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    for (unreadable_certificate, expected) in [
        (true, "failed to read TLS CA certificate"),
        (false, "failed to read TLS CA private key"),
    ] {
        let root =
            std::env::temp_dir().join(format!("agora-unreadable-tls-ca-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let certificate = root.join("ca.pem");
        let private_key = root.join("ca-key.pem");
        std::fs::write(&certificate, b"certificate").unwrap();
        std::fs::write(&private_key, b"private key").unwrap();
        let unreadable = if unreadable_certificate {
            &certificate
        } else {
            &private_key
        };
        std::fs::set_permissions(unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut config = SandboxConfig::new(built_hook_library())
            .with_workdir(root.join("workdir"))
            .with_tls_ca(&certificate, &private_key);
        config.network.tls = TlsMode::Auto;

        let result = Sandbox::new(config, NoopCallback)
            .run(SandboxCommand::new("/usr/bin/true"))
            .await;

        std::fs::set_permissions(unreadable, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = result.unwrap_err();
        assert!(error.to_string().contains(expected), "{error:#}");
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn sandbox_config_preserves_tls_ca_paths() {
    let root = std::env::temp_dir().join(format!("agora-tls-ca-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let hook = root.join("hook.dylib");
    let certificate = root.join("ca.pem");
    let private_key = root.join("ca-key.pem");
    std::fs::write(&hook, b"hook").unwrap();
    std::fs::write(&certificate, b"certificate").unwrap();
    std::fs::write(&private_key, b"private key").unwrap();
    let mut config = SandboxConfig::new(&hook)
        .with_encrypted_workspace("test-key")
        .with_tls_ca(&certificate, &private_key);
    config.network.tls = TlsMode::Auto;

    assert_eq!(
        config.tls_ca(),
        Some((certificate.as_path(), private_key.as_path()))
    );
    assert!(config.validate().is_ok());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn default_tls_ca_reuses_a_complete_pair_and_replaces_a_partial_pair() {
    let root = std::env::temp_dir().join(format!("agora-default-ca-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let hook = root.join("hook.dylib");
    std::fs::write(&hook, b"hook").unwrap();
    let mut config = SandboxConfig::new(&hook).with_workdir(&root);
    config.network.tls = TlsMode::Auto;

    let first = config.tls_ca_for_workdir().unwrap().unwrap();
    assert_eq!(first.certificate, root.join("ca/ca.crt"));
    assert_eq!(first.private_key, root.join("ca/ca.key"));
    let first_certificate = std::fs::read(&first.certificate).unwrap();
    let first_private_key = std::fs::read(&first.private_key).unwrap();

    let reused = config.tls_ca_for_workdir().unwrap().unwrap();
    assert_eq!(
        std::fs::read(&reused.certificate).unwrap(),
        first_certificate
    );
    assert_eq!(
        std::fs::read(&reused.private_key).unwrap(),
        first_private_key
    );

    std::fs::remove_file(&reused.private_key).unwrap();
    let replaced = config.tls_ca_for_workdir().unwrap().unwrap();
    assert_ne!(
        std::fs::read(&replaced.certificate).unwrap(),
        first_certificate
    );
    assert_ne!(
        std::fs::read(&replaced.private_key).unwrap(),
        first_private_key
    );

    let replaced_certificate = std::fs::read(&replaced.certificate).unwrap();
    let replaced_private_key = std::fs::read(&replaced.private_key).unwrap();
    std::fs::write(&replaced.private_key, b"corrupt private key").unwrap();
    let recovered = config.tls_ca_for_workdir().unwrap().unwrap();
    assert_ne!(
        std::fs::read(&recovered.certificate).unwrap(),
        replaced_certificate
    );
    assert_ne!(
        std::fs::read(&recovered.private_key).unwrap(),
        replaced_private_key
    );
    assert!(
        std::fs::read(&recovered.private_key)
            .unwrap()
            .starts_with(b"-----BEGIN PRIVATE KEY-----")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn default_tls_ca_rejects_symlinked_managed_files() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().join("workdir");
    let external = directory.path().join("external");
    std::fs::create_dir_all(workdir.join("ca")).unwrap();
    let external_certificate = external.join("ca.crt");
    let external_key = external.join("ca.key");
    crate::network::generate_tls_ca(&external_certificate, &external_key).unwrap();
    let certificate = std::fs::read(&external_certificate).unwrap();
    let private_key = std::fs::read(&external_key).unwrap();
    std::os::unix::fs::symlink(&external_certificate, workdir.join("ca/ca.crt")).unwrap();
    std::os::unix::fs::symlink(&external_key, workdir.join("ca/ca.key")).unwrap();
    let hook = workdir.join("hook.dylib");
    let mut config = SandboxConfig::new(&hook)
        .with_workdir(&workdir)
        .with_encrypted_workspace("test-key");
    config.network.tls = TlsMode::Auto;

    let accepted = config.tls_ca_for_workdir().is_ok();

    assert!(!accepted, "symbolic-link managed CA files were accepted");
    assert_eq!(std::fs::read(external_certificate).unwrap(), certificate);
    assert_eq!(std::fs::read(external_key).unwrap(), private_key);
}

#[cfg(target_os = "macos")]
#[test]
fn tls_trust_bundles_are_stable_per_ca_and_isolated_between_cas() {
    let root = std::env::temp_dir().join(format!("agora-trust-bundle-{}", uuid::Uuid::new_v4()));
    let config = SandboxConfig::new(root.join("hook.dylib")).with_workdir(&root);

    let first = config.write_tls_trust_bundle(&root, b"first CA").unwrap();
    let reused = config.write_tls_trust_bundle(&root, b"first CA").unwrap();
    let second = config.write_tls_trust_bundle(&root, b"second CA").unwrap();

    assert_eq!(first, reused);
    assert_ne!(first, second);
    assert!(first.is_file());
    assert!(second.is_file());
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn tls_trust_artifacts_reject_a_symlinked_managed_directory() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    let external = directory.path().join("external");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::create_dir(&external).unwrap();
    std::os::unix::fs::symlink(&external, runtime.join("ca")).unwrap();

    let accepted = super::write_tls_trust_artifact(
        &runtime,
        "trust-bundle",
        "crt",
        b"certificate",
        b"bundle",
        "TLS client trust bundle",
    )
    .is_ok();

    assert!(!accepted, "a symbolic-link trust directory was accepted");
    assert_eq!(std::fs::read_dir(external).unwrap().count(), 0);
}

#[cfg(target_os = "macos")]
#[test]
fn java_trust_store_contains_the_interception_ca_and_native_roots() {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    use std::os::unix::fs::PermissionsExt;

    let root =
        std::env::temp_dir().join(format!("agora-java-trust-store-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let certificate = root.join("ca.crt");
    let private_key = root.join("ca.key");
    crate::network::generate_tls_ca(&certificate, &private_key).unwrap();
    let certificate_pem = std::fs::read(&certificate).unwrap();
    let interception_ca = CertificateDer::pem_slice_iter(&certificate_pem)
        .next()
        .unwrap()
        .unwrap();
    let config = SandboxConfig::new(root.join("hook.dylib")).with_workdir(&root);

    let store = config
        .write_java_trust_store(&root, &certificate_pem)
        .unwrap();
    let certificates = read_jks_trusted_certificates(&store, "changeit");

    assert_eq!(certificates.first().unwrap(), interception_ca.as_ref());
    assert!(certificates.len() > 1);
    assert_eq!(
        store.metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
fn read_jks_trusted_certificates(path: &Path, password: &str) -> Vec<Vec<u8>> {
    use std::io::{Cursor, Read};

    fn read_u16(cursor: &mut Cursor<&[u8]>) -> u16 {
        let mut bytes = [0_u8; 2];
        cursor.read_exact(&mut bytes).unwrap();
        u16::from_be_bytes(bytes)
    }

    fn read_u32(cursor: &mut Cursor<&[u8]>) -> u32 {
        let mut bytes = [0_u8; 4];
        cursor.read_exact(&mut bytes).unwrap();
        u32::from_be_bytes(bytes)
    }

    fn read_i64(cursor: &mut Cursor<&[u8]>) -> i64 {
        let mut bytes = [0_u8; 8];
        cursor.read_exact(&mut bytes).unwrap();
        i64::from_be_bytes(bytes)
    }

    fn read_utf(cursor: &mut Cursor<&[u8]>) -> Vec<u8> {
        let mut value = vec![0_u8; usize::from(read_u16(cursor))];
        cursor.read_exact(&mut value).unwrap();
        value
    }

    let encoded = std::fs::read(path).unwrap();
    let (body, checksum) = encoded.split_at(encoded.len() - 20);
    let mut digest = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    for character in password.encode_utf16() {
        digest.update(&character.to_be_bytes());
    }
    digest.update(b"Mighty Aphrodite");
    digest.update(body);
    assert_eq!(checksum, digest.finish().as_ref());

    let mut cursor = Cursor::new(body);
    assert_eq!(read_u32(&mut cursor), 0xfeed_feed);
    assert_eq!(read_u32(&mut cursor), 2);
    let entries = read_u32(&mut cursor);
    let mut certificates = Vec::new();
    for _ in 0..entries {
        assert_eq!(read_u32(&mut cursor), 2);
        assert!(!read_utf(&mut cursor).is_empty());
        let _created_at = read_i64(&mut cursor);
        assert_eq!(read_utf(&mut cursor), b"X.509");
        let mut certificate = vec![0_u8; read_u32(&mut cursor) as usize];
        cursor.read_exact(&mut certificate).unwrap();
        certificates.push(certificate);
    }
    assert_eq!(cursor.position() as usize, body.len());
    certificates
}

#[cfg(target_os = "macos")]
#[test]
fn tls_trust_bundle_reports_directory_and_write_failures() {
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join(format!("agora-trust-errors-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("ca"), b"not a directory").unwrap();
    let config = SandboxConfig::new(root.join("hook.dylib")).with_workdir(&root);

    assert!(
        config
            .write_tls_trust_bundle(&root, b"CA")
            .unwrap_err()
            .to_string()
            .contains("failed to create TLS client trust bundle directory")
    );

    std::fs::remove_file(root.join("ca")).unwrap();
    std::fs::create_dir(root.join("ca")).unwrap();
    std::fs::set_permissions(root.join("ca"), std::fs::Permissions::from_mode(0o500)).unwrap();
    assert!(
        config
            .write_tls_trust_bundle(&root, b"CA")
            .unwrap_err()
            .to_string()
            .contains("failed to write TLS client trust bundle")
    );
    std::fs::set_permissions(root.join("ca"), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn process_group_helpers_treat_a_missing_group_as_already_stopped() {
    let missing = libc::pid_t::MAX;
    assert!(!process_group_exists(missing).unwrap());
    signal_process_group(missing, libc::SIGTERM).unwrap();
    assert!(process_group_exists(0).unwrap());
    assert!(signal_process_group(0, libc::c_int::MAX).is_err());
    assert!(process_group_exists(-1).unwrap());
}

#[cfg(target_os = "macos")]
#[test]
fn foreground_terminal_reports_invalid_descriptors_and_skips_unhanded_restore() {
    let mut terminal = super::ForegroundTerminal {
        descriptor: -1,
        original_process_group: unsafe { libc::getpgrp() },
        handed_off: false,
    };
    terminal.restore().unwrap();
    assert!(terminal.handoff(unsafe { libc::getpgrp() }).is_err());

    terminal.handed_off = true;
    assert!(terminal.restore().is_err());
    terminal.handed_off = false;
    assert!(super::set_terminal_process_group(-1, unsafe { libc::getpgrp() }).is_err());
}

#[tokio::test]
async fn proxy_failure_terminates_the_child_process() {
    let workdir = std::env::temp_dir().join(format!(
        "agora-proxy-failure-cache-{}",
        uuid::Uuid::new_v4()
    ));
    let mut controller = NetworkController::start(
        NetworkConfig::default(),
        NetworkRunContext::new("sandbox", "run"),
        NoopCallback,
    )
    .await
    .unwrap();
    let mut child = sleeping_child();
    let mut execution = ExecutionController::start(workdir.clone()).await.unwrap();
    let mut audit = AuditController::start(
        "sandbox".to_string(),
        "run".to_string(),
        NoopCallback,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    controller.abort_listener_for_test();
    let process_group = child.id().unwrap() as libc::pid_t;
    let mut local_filesystem = None;
    #[cfg(feature = "remote-smb")]
    let mut remote = None;

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        wait_for_child_or_service(
            &mut child,
            process_group,
            RuntimeServices {
                network: &mut controller,
                execution: &mut execution,
                audit: &mut audit,
                local_filesystem: &mut local_filesystem,
                #[cfg(feature = "remote-smb")]
                remote: &mut remote,
            },
            #[cfg(feature = "remote-smb")]
            |_| {},
        ),
    )
    .await
    .unwrap();

    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("sandbox network proxy failed")
    );
    assert!(child.try_wait().unwrap().is_some());
    controller.shutdown().await.unwrap();
    execution.shutdown().await.unwrap();
    audit.shutdown().await.unwrap();
    std::fs::remove_dir_all(workdir).unwrap();
}

#[tokio::test]
async fn execution_controller_failure_terminates_the_child_process() {
    let workdir = std::env::temp_dir().join(format!(
        "agora-execution-failure-cache-{}",
        uuid::Uuid::new_v4()
    ));
    let mut controller = NetworkController::start(
        NetworkConfig::default(),
        NetworkRunContext::new("sandbox", "run"),
        NoopCallback,
    )
    .await
    .unwrap();
    let mut child = sleeping_child();
    let mut execution = ExecutionController::start(workdir.clone()).await.unwrap();
    let mut audit = AuditController::start(
        "sandbox".to_string(),
        "run".to_string(),
        NoopCallback,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    execution.abort_server_for_test();
    let process_group = child.id().unwrap() as libc::pid_t;
    let mut local_filesystem = None;
    #[cfg(feature = "remote-smb")]
    let mut remote = None;

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        wait_for_child_or_service(
            &mut child,
            process_group,
            RuntimeServices {
                network: &mut controller,
                execution: &mut execution,
                audit: &mut audit,
                local_filesystem: &mut local_filesystem,
                #[cfg(feature = "remote-smb")]
                remote: &mut remote,
            },
            #[cfg(feature = "remote-smb")]
            |_| {},
        ),
    )
    .await
    .unwrap();

    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("sandbox execution controller failed")
    );
    assert!(child.try_wait().unwrap().is_some());
    controller.shutdown().await.unwrap();
    assert!(execution.shutdown().await.is_ok());
    audit.shutdown().await.unwrap();
    std::fs::remove_dir_all(workdir).unwrap();
}

#[tokio::test]
async fn audit_controller_failure_terminates_the_child_process() {
    let workdir = std::env::temp_dir().join(format!(
        "agora-audit-failure-cache-{}",
        uuid::Uuid::new_v4()
    ));
    let mut controller = NetworkController::start(
        NetworkConfig::default(),
        NetworkRunContext::new("sandbox", "run"),
        NoopCallback,
    )
    .await
    .unwrap();
    let mut child = sleeping_child();
    let mut execution = ExecutionController::start(workdir.clone()).await.unwrap();
    let mut audit = AuditController::start(
        "sandbox".to_string(),
        "run".to_string(),
        NoopCallback,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    audit.abort_server_for_test();
    let process_group = child.id().unwrap() as libc::pid_t;
    let mut local_filesystem = None;
    #[cfg(feature = "remote-smb")]
    let mut remote = None;

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        wait_for_child_or_service(
            &mut child,
            process_group,
            RuntimeServices {
                network: &mut controller,
                execution: &mut execution,
                audit: &mut audit,
                local_filesystem: &mut local_filesystem,
                #[cfg(feature = "remote-smb")]
                remote: &mut remote,
            },
            #[cfg(feature = "remote-smb")]
            |_| {},
        ),
    )
    .await
    .unwrap();

    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("sandbox audit controller failed")
    );
    assert!(child.try_wait().unwrap().is_some());
    controller.shutdown().await.unwrap();
    execution.shutdown().await.unwrap();
    assert!(audit.shutdown().await.is_ok());
    std::fs::remove_dir_all(workdir).unwrap();
}

#[cfg(all(target_os = "macos", feature = "remote-smb"))]
#[tokio::test]
async fn nfs_broker_failure_terminates_the_child_process() {
    let workdir =
        std::env::temp_dir().join(format!("agora-nfs-failure-cache-{}", uuid::Uuid::new_v4()));
    let runtime = tempfile::Builder::new()
        .prefix("agora-nfs-runner-")
        .tempdir_in("/tmp")
        .unwrap();
    let mut controller = NetworkController::start(
        NetworkConfig::default(),
        NetworkRunContext::new("sandbox", "run"),
        NoopCallback,
    )
    .await
    .unwrap();
    let mut child = sleeping_child();
    let mut execution = ExecutionController::start(workdir.clone()).await.unwrap();
    let mut audit = AuditController::start(
        "sandbox".to_string(),
        "run".to_string(),
        NoopCallback,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    let mut remote = Some(
        RemoteController::start_with_storage(Arc::new(MemoryStorage::default()), runtime.path())
            .await
            .unwrap(),
    );
    remote.as_mut().unwrap().abort_server_for_test();
    let process_group = child.id().unwrap() as libc::pid_t;
    let mut local_filesystem = None;

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        wait_for_child_or_service(
            &mut child,
            process_group,
            RuntimeServices {
                network: &mut controller,
                execution: &mut execution,
                audit: &mut audit,
                local_filesystem: &mut local_filesystem,
                remote: &mut remote,
            },
            |_| {},
        ),
    )
    .await
    .unwrap();

    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("sandbox remote filesystem failed")
    );
    assert!(child.try_wait().unwrap().is_some());
    controller.shutdown().await.unwrap();
    execution.shutdown().await.unwrap();
    audit.shutdown().await.unwrap();
    remote.take().unwrap().shutdown().await.unwrap();
    std::fs::remove_dir_all(workdir).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn child_spawn_failure_shuts_down_started_services() {
    let root = std::env::temp_dir().join(format!(
        "agora-child-spawn-failure-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let hook = root.join("hook.dylib");
    std::fs::write(&hook, b"hook").unwrap();
    let config = SandboxConfig::new(&hook)
        .with_workdir(&root)
        .with_plain_workspace()
        .with_smb_remote(
            SmbRemoteConfig::new("/remote", "127.0.0.1", "missing")
                .unwrap()
                .with_credentials("user", "password"),
        );
    let invalid_argument = std::ffi::OsString::from_vec(b"invalid\0argument".to_vec());

    let error = Sandbox::new(config, NoopCallback)
        .run(SandboxCommand::new("/usr/bin/true").arg(invalid_argument))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("failed to start sandbox child"));
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn termination_kills_a_process_group_that_ignores_sigterm() {
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(["-c", "trap '' TERM; while :; do :; done"])
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let mut child = command.spawn().unwrap();
    let process_group = child.id().unwrap() as libc::pid_t;
    assert_eq!(unsafe { libc::getpgid(process_group) }, process_group);
    tokio::time::sleep(Duration::from_millis(50)).await;

    terminate_process_group(&mut child, process_group)
        .await
        .unwrap();

    assert!(child.try_wait().unwrap().is_some());
    assert!(!process_group_exists(process_group).unwrap());
}
