use super::{
    ChildArguments, ChildEnvironment, INSIDE_PROCESS_HOOK, MAX_RECORDED_ARGUMENT_BYTES,
    PrepareError, PreparedExecutable, ProcessHookGuard, ProcessHookRuntime, TRUNCATED_ARGUMENTS,
    agora_sandbox_execv, agora_sandbox_execve, agora_sandbox_execvp, agora_sandbox_posix_spawn,
    agora_sandbox_posix_spawnp, current_environment, execute, io_errno, prepared_executable,
    process_event_request, requested_executable, resolve_current_directory, search_path_executable,
    with_test_runtime,
};
use crate::audit::AuditEventRequest;
use crate::callback::ProcessOperation;
use crate::execution::{EXECUTION_PROTOCOL_VERSION, decode_prepare_request};
use crate::ipc::{InheritedControlLock, InheritedControlStream};
use crate::network::client_trust::merged_java_tool_options;
use crate::platform::hook::config::HookConfig;
use crate::trace::TraceContext;
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use uuid::Uuid;

fn config() -> HookConfig {
    config_with_control("127.0.0.1:41002".parse().unwrap())
}

fn config_with_control(control: SocketAddr) -> HookConfig {
    config_with_control_and_token(control, "execution-token")
}

fn config_with_control_and_token(control: SocketAddr, execution_token: &str) -> HookConfig {
    let control = control.to_string();
    let values = HashMap::from([
        ("AGORA_SANDBOX_TOKEN", "token".to_string()),
        ("AGORA_SANDBOX_PROXY_IPV4", "127.0.0.1:41000".to_string()),
        ("AGORA_SANDBOX_PROXY_IPV6", "[::1]:41001".to_string()),
        ("AGORA_SANDBOX_EXECUTION_CONTROL", control),
        ("AGORA_SANDBOX_EXECUTION_TOKEN", execution_token.to_string()),
        ("AGORA_SANDBOX_AUDIT_CONTROL", "127.0.0.1:41003".to_string()),
        ("AGORA_SANDBOX_AUDIT_TOKEN", "audit-token".to_string()),
        (
            "AGORA_SANDBOX_HOOK_LIBRARIES",
            "/tmp/hook.dylib".to_string(),
        ),
        (
            "AGORA_SANDBOX_FILESYSTEM_ROOT",
            "/tmp/agora-test-workdir/fs".to_string(),
        ),
        ("AGORA_SANDBOX_FILESYSTEM_MODE", "plain".to_string()),
        ("AGORA_SANDBOX_TRACE_ID", "trace-root".to_string()),
    ]);
    HookConfig::from_getter(|key| values.get(key).cloned()).unwrap()
}

fn config_with_tls_bundle() -> HookConfig {
    let control = "127.0.0.1:41002".to_string();
    let values = HashMap::from([
        ("AGORA_SANDBOX_TOKEN", "token".to_string()),
        ("AGORA_SANDBOX_PROXY_IPV4", "127.0.0.1:41000".to_string()),
        ("AGORA_SANDBOX_PROXY_IPV6", "[::1]:41001".to_string()),
        ("AGORA_SANDBOX_EXECUTION_CONTROL", control),
        (
            "AGORA_SANDBOX_EXECUTION_TOKEN",
            "execution-token".to_string(),
        ),
        ("AGORA_SANDBOX_AUDIT_CONTROL", "127.0.0.1:41003".to_string()),
        ("AGORA_SANDBOX_AUDIT_TOKEN", "audit-token".to_string()),
        (
            "AGORA_SANDBOX_HOOK_LIBRARIES",
            "/tmp/hook.dylib".to_string(),
        ),
        (
            "AGORA_SANDBOX_FILESYSTEM_ROOT",
            "/tmp/agora-test-workdir/fs".to_string(),
        ),
        ("AGORA_SANDBOX_FILESYSTEM_MODE", "plain".to_string()),
        ("AGORA_SANDBOX_TRACE_ID", "trace-root".to_string()),
        (
            "AGORA_SANDBOX_TLS_TRUST_BUNDLE",
            "/tmp/agora-ca.pem".to_string(),
        ),
        (
            "AGORA_SANDBOX_JAVA_TRUST_STORE",
            "/tmp/agora-ca.jks".to_string(),
        ),
    ]);
    HookConfig::from_getter(|key| values.get(key).cloned()).unwrap()
}

fn child_trace() -> TraceContext {
    TraceContext::parse("trace-root").unwrap().child()
}

fn response(status: u8, content: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&EXECUTION_PROTOCOL_VERSION.to_be_bytes());
    body.push(status);
    body.extend_from_slice(&(content.len() as u32).to_be_bytes());
    body.extend_from_slice(content);
    let mut frame = Vec::new();
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    frame
}

fn error_response(errno: libc::c_int, message: &[u8]) -> Vec<u8> {
    let mut content = Vec::with_capacity(4 + message.len());
    content.extend_from_slice(&errno.to_be_bytes());
    content.extend_from_slice(message);
    response(2, &content)
}

fn runtime_with_response(response: Vec<u8>) -> (ProcessHookRuntime, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let control = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut prefix = [0_u8; 4];
        stream.read_exact(&mut prefix).unwrap();
        let length = u32::from_be_bytes(prefix) as usize;
        let mut request = vec![0_u8; length];
        stream.read_exact(&mut request).unwrap();
        stream.write_all(&response).unwrap();
        request
    });
    (
        ProcessHookRuntime {
            config: config_with_control(control),
            audit: None,
            execution: None,
            prefer_shared: std::sync::atomic::AtomicBool::new(false),
            observed_pid: std::sync::atomic::AtomicU32::new(std::process::id()),
        },
        server,
    )
}

fn runtime_with_responses(
    responses: Vec<Vec<u8>>,
) -> (ProcessHookRuntime, thread::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let control = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        responses
            .into_iter()
            .map(|response| {
                let (mut stream, _) = listener.accept().unwrap();
                let mut prefix = [0_u8; 4];
                stream.read_exact(&mut prefix).unwrap();
                let mut request = vec![0_u8; u32::from_be_bytes(prefix) as usize];
                stream.read_exact(&mut request).unwrap();
                stream.write_all(&response).unwrap();
                request
            })
            .collect()
    });
    (
        ProcessHookRuntime {
            config: config_with_control(control),
            audit: None,
            execution: None,
            prefer_shared: std::sync::atomic::AtomicBool::new(false),
            observed_pid: std::sync::atomic::AtomicU32::new(std::process::id()),
        },
        server,
    )
}

#[test]
fn process_runtime_reuses_the_inherited_execution_stream() {
    let fallback = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    fallback.set_nonblocking(true).unwrap();
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let mut prefix = [0_u8; 4];
            stream.read_exact(&mut prefix).unwrap();
            let mut request = vec![0_u8; u32::from_be_bytes(prefix) as usize];
            stream.read_exact(&mut request).unwrap();
            stream.write_all(&response(1, b"/tmp/prepared-sh")).unwrap();
            requests.push(request);
        }
        requests
    });
    let execution =
        InheritedControlStream::new(client, InheritedControlLock::anonymous().unwrap(), 0).unwrap();
    let runtime = ProcessHookRuntime {
        config: config_with_control(fallback.local_addr().unwrap()),
        audit: None,
        execution: Some(Arc::clone(&execution)),
        prefer_shared: std::sync::atomic::AtomicBool::new(true),
        observed_pid: std::sync::atomic::AtomicU32::new(std::process::id()),
    };

    assert_eq!(
        runtime.prepare(Path::new("/bin/sh")).unwrap(),
        CString::new("/tmp/prepared-sh").unwrap()
    );
    assert_eq!(
        runtime.prepare(Path::new("/usr/bin/true")).unwrap(),
        CString::new("/tmp/prepared-sh").unwrap()
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        decode_prepare_request(&requests[0]).unwrap().executable,
        Path::new("/bin/sh")
    );
    assert_eq!(
        decode_prepare_request(&requests[1]).unwrap().executable,
        Path::new("/usr/bin/true")
    );
    assert_eq!(
        fallback.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn child_environment_restores_runtime_values_after_the_caller_clears_them() {
    assert_eq!(config().filesystem_root(), "/tmp/agora-test-workdir/fs");
    let path = CString::new("PATH=/usr/bin:/bin").unwrap();
    let values = [path.as_ptr(), std::ptr::null()];

    let environment =
        unsafe { ChildEnvironment::new(values.as_ptr(), &config(), &child_trace(), None) }.unwrap();
    let entries = environment
        .values
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect::<Vec<_>>();

    assert!(entries.contains(&"PATH=/usr/bin:/bin"));
    assert!(entries.contains(&"AGORA_SANDBOX_TOKEN=token"));
    assert!(entries.contains(&"AGORA_SANDBOX_EXECUTION_TOKEN=execution-token"));
    assert!(entries.contains(&"AGORA_SANDBOX_AUDIT_TOKEN=audit-token"));
    assert!(
        entries
            .iter()
            .any(|entry| { entry.starts_with("AGORA_SANDBOX_TRACE_ID=trace-root, ") })
    );
    assert!(entries.contains(&"DYLD_INSERT_LIBRARIES=/tmp/hook.dylib"));
}

#[test]
fn child_environment_accepts_a_null_source_environment() {
    let environment =
        unsafe { ChildEnvironment::new(std::ptr::null(), &config(), &child_trace(), None) }
            .unwrap();

    assert!(!environment.as_exec_ptr().is_null());
    assert_eq!(environment.values.len(), 13);
}

#[test]
fn child_arguments_replace_a_script_with_its_prepared_interpreter() {
    let original = [
        CString::new("/usr/local/bin/codex").unwrap(),
        CString::new("--version").unwrap(),
    ];
    let pointers = [original[0].as_ptr(), original[1].as_ptr(), std::ptr::null()];
    let prepared = PreparedExecutable {
        program: CString::new("/tmp/fs/usr/bin/env").unwrap(),
        arguments: vec![
            CString::new("node").unwrap(),
            CString::new("/usr/local/bin/codex").unwrap(),
        ],
    };

    let arguments = unsafe { ChildArguments::new(pointers.as_ptr(), &prepared) }.unwrap();
    let values = arguments
        .values
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        values,
        [
            "/tmp/fs/usr/bin/env",
            "node",
            "/usr/local/bin/codex",
            "--version",
        ]
    );
    assert!(!arguments.as_exec_ptr().is_null());

    let direct = PreparedExecutable {
        program: CString::new("/usr/local/bin/codex").unwrap(),
        arguments: Vec::new(),
    };
    let arguments = unsafe { ChildArguments::new(pointers.as_ptr(), &direct) }.unwrap();
    let values = arguments
        .values
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(values, ["/usr/local/bin/codex", "--version"]);

    let fallback =
        unsafe { ChildArguments::shell_fallback(pointers.as_ptr(), c"/tmp/prepared-script") }
            .unwrap();
    let values = fallback
        .values
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(values, ["sh", "/tmp/prepared-script", "--version"]);
}

#[test]
fn child_environment_replaces_untrusted_runtime_values() {
    let stale = [
        CString::new("AGORA_SANDBOX_TOKEN=stale").unwrap(),
        CString::new("DYLD_INSERT_LIBRARIES=/tmp/untrusted.dylib").unwrap(),
    ];
    let pointers = [stale[0].as_ptr(), stale[1].as_ptr(), std::ptr::null()];

    let environment =
        unsafe { ChildEnvironment::new(pointers.as_ptr(), &config(), &child_trace(), None) }
            .unwrap();
    let entries = unsafe {
        let mut current = environment.as_exec_ptr();
        let mut entries = Vec::new();
        while !(*current).is_null() {
            entries.push(CStr::from_ptr(*current).to_str().unwrap());
            current = current.add(1);
        }
        entries
    };

    assert!(!entries.contains(&"AGORA_SANDBOX_TOKEN=stale"));
    assert!(!entries.contains(&"DYLD_INSERT_LIBRARIES=/tmp/untrusted.dylib"));
    assert!(entries.contains(&"AGORA_SANDBOX_TOKEN=token"));
    assert!(entries.contains(&"DYLD_INSERT_LIBRARIES=/tmp/hook.dylib"));
}

#[test]
fn child_environment_propagates_only_the_tracked_remote_current_directory() {
    let stale = CString::new("AGORA_SANDBOX_REMOTE_CURRENT_DIRECTORY=/remote/stale").unwrap();
    let values = [stale.as_ptr(), std::ptr::null()];

    let environment = unsafe {
        ChildEnvironment::new(
            values.as_ptr(),
            &config(),
            &child_trace(),
            Some(Path::new("/remote/team/docs")),
        )
    }
    .unwrap();
    let entries = environment
        .values
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect::<Vec<_>>();

    assert!(!entries.contains(&"AGORA_SANDBOX_REMOTE_CURRENT_DIRECTORY=/remote/stale"));
    assert!(entries.contains(&"AGORA_SANDBOX_REMOTE_CURRENT_DIRECTORY=/remote/team/docs"));

    let environment =
        unsafe { ChildEnvironment::new(values.as_ptr(), &config(), &child_trace(), None) }.unwrap();
    assert!(environment.values.iter().all(|value| {
        !value
            .to_bytes()
            .starts_with(b"AGORA_SANDBOX_REMOTE_CURRENT_DIRECTORY=")
    }));
}

#[test]
fn child_environment_restores_tls_trust_after_the_caller_clears_it() {
    let stale = CString::new("SSL_CERT_FILE=/tmp/untrusted.pem").unwrap();
    let values = [stale.as_ptr(), std::ptr::null()];

    let environment = unsafe {
        ChildEnvironment::new(
            values.as_ptr(),
            &config_with_tls_bundle(),
            &child_trace(),
            None,
        )
    }
    .unwrap();
    let entries = environment
        .values
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect::<Vec<_>>();

    assert!(!entries.contains(&"SSL_CERT_FILE=/tmp/untrusted.pem"));
    assert!(entries.contains(&"SSL_CERT_FILE=/tmp/agora-ca.pem"));
    assert!(entries.contains(&"CURL_CA_BUNDLE=/tmp/agora-ca.pem"));
    assert!(entries.contains(&"REQUESTS_CA_BUNDLE=/tmp/agora-ca.pem"));
    assert!(entries.contains(&"PIP_CERT=/tmp/agora-ca.pem"));
    assert!(entries.contains(&"NODE_EXTRA_CA_CERTS=/tmp/agora-ca.pem"));
    assert!(entries.contains(&"GIT_SSL_CAINFO=/tmp/agora-ca.pem"));
}

#[test]
fn child_environment_adds_java_trust_without_discarding_other_options() {
    let options = CString::new("JAVA_TOOL_OPTIONS=-Xmx256m -Duser.language=en").unwrap();
    let values = [options.as_ptr(), std::ptr::null()];

    let environment = unsafe {
        ChildEnvironment::new(
            values.as_ptr(),
            &config_with_tls_bundle(),
            &child_trace(),
            None,
        )
    }
    .unwrap();
    let entries = environment
        .values
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect::<Vec<_>>();

    assert!(entries.contains(&concat!(
        "JAVA_TOOL_OPTIONS=-Xmx256m -Duser.language=en ",
        "-Djavax.net.ssl.trustStore=/tmp/agora-ca.jks ",
        "-Djavax.net.ssl.trustStoreType=JKS ",
        "-Djavax.net.ssl.trustStorePassword=changeit"
    )));
}

#[test]
fn child_environment_preserves_an_explicit_java_trust_store() {
    let options = CString::new(concat!(
        "JAVA_TOOL_OPTIONS=-Xmx256m ",
        "-Djavax.net.ssl.trustStore=/tmp/application.jks"
    ))
    .unwrap();
    let values = [options.as_ptr(), std::ptr::null()];

    let environment = unsafe {
        ChildEnvironment::new(
            values.as_ptr(),
            &config_with_tls_bundle(),
            &child_trace(),
            None,
        )
    }
    .unwrap();
    let entries = environment
        .values
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect::<Vec<_>>();

    assert!(entries.contains(&concat!(
        "JAVA_TOOL_OPTIONS=-Xmx256m ",
        "-Djavax.net.ssl.trustStore=/tmp/application.jks"
    )));
    let java_options = entries
        .iter()
        .find(|entry| entry.starts_with("JAVA_TOOL_OPTIONS="))
        .unwrap();
    assert!(!java_options.contains("/tmp/agora-ca.jks"));
}

#[test]
fn child_environment_preserves_a_quoted_explicit_java_trust_store() {
    let options = CString::new(concat!(
        "JAVA_TOOL_OPTIONS=-Xmx256m ",
        "-Djavax.net.ssl.trustStore=\"/tmp/application trust.jks\""
    ))
    .unwrap();
    let values = [options.as_ptr(), std::ptr::null()];

    let environment = unsafe {
        ChildEnvironment::new(
            values.as_ptr(),
            &config_with_tls_bundle(),
            &child_trace(),
            None,
        )
    }
    .unwrap();
    let java_options = environment
        .values
        .iter()
        .map(|value| value.to_str().unwrap())
        .find(|entry| entry.starts_with("JAVA_TOOL_OPTIONS="))
        .unwrap();

    assert_eq!(java_options, options.to_str().unwrap());
    assert!(!java_options.contains("/tmp/agora-ca.jks"));
}

#[test]
fn java_trust_merge_respects_the_last_effective_store() {
    let options = concat!(
        "-Djavax.net.ssl.trustStore=/tmp/old-agora.jks ",
        "-Djavax.net.ssl.trustStore=/tmp/application.jks"
    );

    assert_eq!(
        merged_java_tool_options(
            Some(options.as_bytes()),
            Some(b"/tmp/old-agora.jks"),
            b"/tmp/new-agora.jks",
        ),
        options.as_bytes()
    );
}

#[test]
fn java_trust_merge_quotes_a_managed_store_with_spaces() {
    assert_eq!(
        merged_java_tool_options(None, None, b"/tmp/agora trust/store.jks"),
        concat!(
            "-Djavax.net.ssl.trustStore=\"/tmp/agora trust/store.jks\" ",
            "-Djavax.net.ssl.trustStoreType=JKS ",
            "-Djavax.net.ssl.trustStorePassword=changeit"
        )
        .as_bytes()
    );
}

#[test]
fn process_hook_guard_blocks_recursion_until_dropped() {
    let guard = ProcessHookGuard::enter_when_ready(true).unwrap();
    assert!(ProcessHookGuard::enter_when_ready(true).is_none());
    drop(guard);
    assert!(ProcessHookGuard::enter_when_ready(true).is_some());
}

#[test]
fn process_hook_guard_blocks_catchable_signals_while_state_is_active() {
    let signal = super::super::tests::SignalMaskProbe::unblocked(libc::SIGUSR2);
    let guard = ProcessHookGuard::enter_when_ready(true).unwrap();

    assert!(signal.is_blocked());
    assert!(!super::super::tests::SignalMaskProbe::signal_is_blocked(
        libc::SIGSEGV
    ));
    drop(guard);
    assert!(!signal.is_blocked());
}

#[test]
fn process_hook_guard_does_not_touch_tls_before_initialization() {
    INSIDE_PROCESS_HOOK.with(|inside| inside.set(false));
    assert!(ProcessHookGuard::enter_when_ready(false).is_none());
    assert!(!INSIDE_PROCESS_HOOK.with(Cell::get));
}

#[test]
fn preparation_errors_preserve_os_and_semantic_errno_categories() {
    for (kind, expected) in [
        (std::io::ErrorKind::NotFound, libc::ENOENT),
        (std::io::ErrorKind::PermissionDenied, libc::EACCES),
        (std::io::ErrorKind::InvalidInput, libc::EINVAL),
        (std::io::ErrorKind::InvalidData, libc::EPROTO),
        (std::io::ErrorKind::TimedOut, libc::ETIMEDOUT),
        (std::io::ErrorKind::Unsupported, libc::ENOTSUP),
        (std::io::ErrorKind::Other, libc::EIO),
    ] {
        assert_eq!(io_errno(&std::io::Error::new(kind, "failure")), expected);
    }
    assert_eq!(
        io_errno(&std::io::Error::from_raw_os_error(libc::EBUSY)),
        libc::EBUSY
    );

    let converted =
        PrepareError::from(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"));
    assert_eq!(converted.errno, libc::ENOENT);
    assert_eq!(converted.to_string(), "missing");

    let nested = PrepareError::from_anyhow(
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into(),
        libc::EIO,
    );
    assert_eq!(nested.errno, libc::EACCES);
    let fallback = PrepareError::from_anyhow(anyhow::anyhow!("invalid image"), libc::ENOEXEC);
    assert_eq!(fallback.errno, libc::ENOEXEC);
    assert_eq!(fallback.to_string(), "invalid image");
}

#[test]
fn requested_executable_resolves_direct_and_path_based_programs() {
    let absolute = CString::new("/bin/sh").unwrap();
    let relative = CString::new("./Cargo.toml").unwrap();
    let shell = CString::new("sh").unwrap();
    let missing = CString::new("agora-command-that-does-not-exist").unwrap();

    assert_eq!(
        unsafe { requested_executable(absolute.as_ptr(), false) }.unwrap(),
        Path::new("/bin/sh")
    );
    assert_eq!(
        unsafe { requested_executable(relative.as_ptr(), false) }.unwrap(),
        std::env::current_dir().unwrap().join("./Cargo.toml")
    );
    assert!(
        unsafe { requested_executable(shell.as_ptr(), true) }
            .unwrap()
            .ends_with("sh")
    );
    assert_eq!(
        unsafe { requested_executable(missing.as_ptr(), true) }
            .unwrap_err()
            .errno,
        libc::ENOENT
    );
    assert_eq!(
        unsafe { requested_executable(std::ptr::null(), false) }
            .unwrap_err()
            .errno,
        libc::EFAULT
    );
}

#[test]
fn path_search_skips_non_executable_files() {
    let root = tempfile::tempdir().unwrap();
    let blocked = root.path().join("blocked");
    let executable = root.path().join("executable");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::create_dir(&executable).unwrap();
    std::fs::write(blocked.join("tool"), b"blocked").unwrap();
    std::fs::write(executable.join("tool"), b"executable").unwrap();
    std::fs::set_permissions(
        executable.join("tool"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let search = std::env::join_paths([&blocked, &executable]).unwrap();

    assert_eq!(
        search_path_executable(OsStr::new("tool"), &search, root.path()).unwrap(),
        executable.join("tool")
    );

    let denied = std::env::join_paths([&blocked]).unwrap();
    assert_eq!(
        search_path_executable(OsStr::new("tool"), &denied, root.path())
            .unwrap_err()
            .errno,
        libc::EACCES
    );
}

#[test]
fn command_request_records_process_context_and_bounds_argument_count() {
    let arguments = [
        CString::new("curl").unwrap(),
        CString::new("https://example.com").unwrap(),
    ];
    let pointers = [
        arguments[0].as_ptr(),
        arguments[1].as_ptr(),
        std::ptr::null(),
    ];
    let trace = TraceContext::parse("trace-root, trace-child").unwrap();

    let request = unsafe {
        process_event_request(
            Path::new("/usr/bin/curl"),
            pointers.as_ptr(),
            ProcessOperation::Execve,
            &trace,
        )
    }
    .unwrap();

    let AuditEventRequest::Process {
        trace_id,
        process,
        command,
    } = request
    else {
        panic!("expected process audit event");
    };
    assert_eq!(trace_id, "trace-root, trace-child");
    assert_eq!(process.pid, std::process::id());
    assert_eq!(process.ppid, unsafe { libc::getppid() as u32 });
    assert!(!process.executable.is_empty());
    assert_eq!(command.executable, "/usr/bin/curl");
    assert_eq!(command.arguments, ["curl", "https://example.com"]);
    assert_eq!(
        command.current_dir,
        std::env::current_dir().unwrap().to_string_lossy()
    );
    assert_eq!(command.operation, ProcessOperation::Execve);

    let argument = CString::new("secret").unwrap();
    let mut pointers = vec![argument.as_ptr(); 257];
    pointers.push(std::ptr::null());
    let request = unsafe {
        process_event_request(
            Path::new("/bin/true"),
            pointers.as_ptr(),
            ProcessOperation::Execv,
            &trace,
        )
    }
    .unwrap();
    let AuditEventRequest::Process { command, .. } = request else {
        panic!("expected process audit event");
    };
    assert_eq!(command.arguments.len(), 257);
    assert!(
        command.arguments[..256]
            .iter()
            .all(|value| value == "secret")
    );
    assert_eq!(command.arguments[256], TRUNCATED_ARGUMENTS);

    let oversized = CString::new("x".repeat(MAX_RECORDED_ARGUMENT_BYTES + 1)).unwrap();
    let pointers = [oversized.as_ptr(), std::ptr::null()];
    let request = unsafe {
        process_event_request(
            Path::new("/bin/true"),
            pointers.as_ptr(),
            ProcessOperation::Execv,
            &trace,
        )
    }
    .unwrap();
    let AuditEventRequest::Process { command, .. } = request else {
        panic!("expected process audit event");
    };
    assert_eq!(command.arguments, [TRUNCATED_ARGUMENTS]);
}

#[test]
fn process_audit_prefers_the_tracked_logical_directory() {
    let native = || {
        Err(std::io::Error::from_raw_os_error(libc::EACCES)) as std::io::Result<std::path::PathBuf>
    };

    let directory = resolve_current_directory(
        Some("/Users/example".into()),
        native,
        Some(OsString::from("/stale")),
    )
    .unwrap();

    assert_eq!(directory, Path::new("/Users/example"));
}

#[test]
fn process_runtime_returns_the_prepared_executable() {
    let (runtime, server) = runtime_with_response(response(1, b"/tmp/prepared-curl"));
    let prepared = runtime.prepare(Path::new("/usr/bin/curl")).unwrap();

    assert_eq!(prepared.to_bytes(), b"/tmp/prepared-curl");
    let request = server.join().unwrap();
    assert!(
        request
            .windows(b"execution-token".len())
            .any(|value| value == b"execution-token")
    );
    assert!(
        request
            .windows(b"/usr/bin/curl".len())
            .any(|value| value == b"/usr/bin/curl")
    );
    let request = decode_prepare_request(&request).unwrap();
    assert_eq!(request.executable, Path::new("/usr/bin/curl"));
}

#[test]
fn process_runtime_prepares_a_shebang_interpreter_and_preserves_the_script() {
    let directory = std::env::temp_dir().join(format!("agora-hook-script-{}", Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let script = directory.join("client");
    std::fs::write(&script, b"#!/usr/bin/env node\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = script.canonicalize().unwrap();
    let (runtime, server) = runtime_with_responses(vec![
        response(1, script.as_os_str().as_encoded_bytes()),
        response(1, b"/tmp/prepared-env"),
    ]);

    let prepared = runtime.prepare_executable(&script).unwrap();

    assert_eq!(prepared.program.to_bytes(), b"/tmp/prepared-env");
    assert_eq!(prepared.arguments[0].to_bytes(), b"node");
    assert_eq!(
        prepared.arguments[1].to_bytes(),
        script.as_os_str().as_encoded_bytes()
    );
    let requests = server.join().unwrap();
    assert!(
        requests[0]
            .windows(script.as_os_str().as_encoded_bytes().len())
            .any(|value| value == script.as_os_str().as_encoded_bytes())
    );
    assert!(
        requests[1]
            .windows(b"/usr/bin/env".len())
            .any(|value| value == b"/usr/bin/env")
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn process_runtime_rejects_a_nul_in_a_shebang_argument() {
    let directory = std::env::temp_dir().join(format!("agora-hook-script-{}", Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let script = directory.join("client");
    std::fs::write(&script, b"#!/bin/sh argument\0suffix\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = script.canonicalize().unwrap();
    let (runtime, server) = runtime_with_responses(vec![
        response(1, script.as_os_str().as_encoded_bytes()),
        response(1, b"/tmp/prepared-sh"),
    ]);

    let error = runtime.prepare_executable(&script).unwrap_err();

    assert_eq!(error.errno, libc::EINVAL);
    assert_eq!(error.to_string(), "shebang argument contains NUL");
    assert_eq!(server.join().unwrap().len(), 2);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn process_runtime_keeps_a_direct_executable_unchanged() {
    let directory = std::env::temp_dir().join(format!("agora-hook-binary-{}", Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let executable = directory.join("client");
    std::fs::write(&executable, b"not a script").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executable = executable.canonicalize().unwrap();
    let (runtime, server) =
        runtime_with_response(response(1, executable.as_os_str().as_encoded_bytes()));

    let prepared = runtime.prepare_executable(&executable).unwrap();

    assert_eq!(
        prepared.program.to_bytes(),
        executable.as_os_str().as_encoded_bytes()
    );
    assert!(prepared.arguments.is_empty());
    server.join().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn process_runtime_propagates_denied_and_invalid_responses() {
    let (runtime, denied_server) =
        runtime_with_response(error_response(libc::ENOENT, b"missing executable"));
    let denied = runtime.prepare(Path::new("/bin/sh")).unwrap_err();
    assert_eq!(denied.errno, libc::ENOENT);
    assert_eq!(denied.to_string(), "missing executable");
    denied_server.join().unwrap();

    let (runtime, invalid_server) = runtime_with_response(response(3, b"invalid"));
    assert_eq!(
        runtime.prepare(Path::new("/bin/sh")).unwrap_err().errno,
        libc::EPROTO
    );
    invalid_server.join().unwrap();

    let (runtime, nul_server) = runtime_with_response(response(1, b"/tmp/a\0b"));
    assert_eq!(
        runtime.prepare(Path::new("/bin/sh")).unwrap_err().errno,
        libc::EINVAL
    );
    nul_server.join().unwrap();
}

#[test]
fn process_runtime_rejects_an_oversized_execution_token_before_sending() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let runtime = ProcessHookRuntime {
        config: config_with_control_and_token(listener.local_addr().unwrap(), &"x".repeat(65_536)),
        audit: None,
        execution: None,
        prefer_shared: std::sync::atomic::AtomicBool::new(false),
        observed_pid: std::sync::atomic::AtomicU32::new(std::process::id()),
    };
    let error = runtime.prepare(Path::new("/bin/sh")).unwrap_err();

    assert_eq!(error.errno, libc::EINVAL);
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn process_interposers_fail_closed_during_recursive_entry() {
    let _guard = ProcessHookGuard::enter_when_ready(true).unwrap();

    assert_eq!(
        unsafe {
            agora_sandbox_posix_spawn(
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        libc::EACCES
    );
    assert_eq!(
        unsafe {
            agora_sandbox_posix_spawnp(
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        libc::EACCES
    );
    assert_eq!(
        unsafe { agora_sandbox_execve(std::ptr::null(), std::ptr::null(), std::ptr::null(),) },
        -1
    );
    assert_eq!(
        unsafe { agora_sandbox_execv(std::ptr::null(), std::ptr::null()) },
        -1
    );
    assert_eq!(
        unsafe { agora_sandbox_execvp(std::ptr::null(), std::ptr::null()) },
        -1
    );
    assert!(!unsafe { current_environment() }.is_null());
}

#[test]
fn process_runtime_and_direct_execution_fail_closed_without_configuration() {
    assert!(ProcessHookRuntime::global().is_none());
    assert!(
        unsafe {
            prepared_executable(
                std::ptr::null(),
                false,
                std::ptr::null(),
                ProcessOperation::Execve,
            )
        }
        .is_err()
    );

    let _guard = ProcessHookGuard::enter_when_ready(true).unwrap();
    assert_eq!(
        unsafe {
            execute(
                std::ptr::null(),
                false,
                std::ptr::null(),
                std::ptr::null(),
                ProcessOperation::Execve,
            )
        },
        -1
    );
}

#[test]
fn prepared_execution_distinguishes_null_and_missing_programs() {
    let runtime = ProcessHookRuntime {
        config: config(),
        audit: None,
        execution: None,
        prefer_shared: std::sync::atomic::AtomicBool::new(false),
        observed_pid: std::sync::atomic::AtomicU32::new(std::process::id()),
    };
    let missing = CString::new("agora-command-that-does-not-exist").unwrap();

    let null_error = with_test_runtime(&runtime, || unsafe {
        prepared_executable(
            std::ptr::null(),
            false,
            std::ptr::null(),
            ProcessOperation::Execve,
        )
        .unwrap_err()
    });
    assert_eq!(null_error.errno, libc::EFAULT);
    assert_eq!(
        null_error.to_string(),
        "requested executable could not be resolved"
    );

    let missing_error = with_test_runtime(&runtime, || unsafe {
        prepared_executable(
            missing.as_ptr(),
            true,
            std::ptr::null(),
            ProcessOperation::Execvp,
        )
        .unwrap_err()
    });
    assert_eq!(missing_error.errno, libc::ENOENT);
    assert_eq!(
        missing_error.to_string(),
        "requested executable could not be resolved through PATH"
    );
}

#[test]
fn process_spawn_interposers_prepare_and_launch_native_children() {
    let executable = CString::new("/usr/bin/true").unwrap();
    let file = CString::new("true").unwrap();

    for search_path in [false, true] {
        let (runtime, server) = runtime_with_response(response(1, b"/usr/bin/true"));
        let requested = if search_path { &file } else { &executable };
        let mut arguments = [requested.as_ptr().cast_mut(), std::ptr::null_mut()];
        let mut pid = 0;
        let result = with_test_runtime(&runtime, || unsafe {
            if search_path {
                agora_sandbox_posix_spawnp(
                    &mut pid,
                    requested.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    arguments.as_mut_ptr(),
                    std::ptr::null(),
                )
            } else {
                agora_sandbox_posix_spawn(
                    &mut pid,
                    requested.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    arguments.as_mut_ptr(),
                    std::ptr::null(),
                )
            }
        });
        assert_eq!(result, 0);
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);

        let request = decode_prepare_request(&server.join().unwrap()).unwrap();
        assert_eq!(request.executable, Path::new("/usr/bin/true"));
    }
}

#[test]
fn spawned_child_inherits_the_callers_signal_mask() {
    let signal = super::super::tests::SignalMaskProbe::unblocked(libc::SIGUSR2);
    let executable = CString::new("/usr/bin/python3").unwrap();
    let option = CString::new("-c").unwrap();
    let program = CString::new(
        "import signal,sys; sys.exit(9 if signal.SIGUSR2 in signal.pthread_sigmask(signal.SIG_BLOCK, []) else 0)",
    )
    .unwrap();
    let (runtime, server) = runtime_with_response(response(1, b"/usr/bin/python3"));
    let mut arguments = [
        executable.as_ptr().cast_mut(),
        option.as_ptr().cast_mut(),
        program.as_ptr().cast_mut(),
        std::ptr::null_mut(),
    ];
    let mut pid = 0;

    let result = with_test_runtime(&runtime, || unsafe {
        agora_sandbox_posix_spawn(
            &mut pid,
            executable.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            arguments.as_mut_ptr(),
            std::ptr::null(),
        )
    });

    assert_eq!(result, 0);
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 0);
    assert!(!signal.is_blocked());
    server.join().unwrap();
}

#[test]
fn process_exec_interposers_prepare_before_native_exec_failure() {
    let directory = std::env::temp_dir().join(format!("agora-exec-hook-{}", Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let invalid = directory.join("not-mach-o");
    std::fs::write(&invalid, b"not a native executable").unwrap();
    std::fs::set_permissions(&invalid, std::fs::Permissions::from_mode(0o755)).unwrap();
    let invalid = invalid.canonicalize().unwrap();
    let path = CString::new("/bin/true").unwrap();
    let file = CString::new("true").unwrap();

    for operation in [ProcessOperation::Execve, ProcessOperation::Execv] {
        let (runtime, server) =
            runtime_with_response(response(1, invalid.as_os_str().as_encoded_bytes()));
        let requested = &path;
        let arguments = [requested.as_ptr(), std::ptr::null()];
        unsafe { *libc::__error() = 0 };
        let result = with_test_runtime(&runtime, || unsafe {
            match operation {
                ProcessOperation::Execve => agora_sandbox_execve(
                    requested.as_ptr(),
                    arguments.as_ptr(),
                    current_environment(),
                ),
                ProcessOperation::Execv => {
                    agora_sandbox_execv(requested.as_ptr(), arguments.as_ptr())
                }
                _ => unreachable!(),
            }
        });
        assert_eq!(result, -1);
        assert_eq!(unsafe { *libc::__error() }, libc::ENOEXEC);

        let request = decode_prepare_request(&server.join().unwrap()).unwrap();
        assert_eq!(request.executable, Path::new("/bin/true"));
    }

    let (runtime, server) = runtime_with_responses(vec![
        response(1, invalid.as_os_str().as_encoded_bytes()),
        response(1, invalid.as_os_str().as_encoded_bytes()),
    ]);
    let arguments = [file.as_ptr(), std::ptr::null()];
    unsafe { *libc::__error() = 0 };
    let result = with_test_runtime(&runtime, || unsafe {
        agora_sandbox_execvp(file.as_ptr(), arguments.as_ptr())
    });
    assert_eq!(result, -1);
    assert_eq!(unsafe { *libc::__error() }, libc::ENOEXEC);
    let requests = server.join().unwrap();
    assert_eq!(
        decode_prepare_request(&requests[0]).unwrap().executable,
        Path::new("/usr/bin/true")
    );
    assert_eq!(
        decode_prepare_request(&requests[1]).unwrap().executable,
        Path::new("/bin/sh")
    );

    std::fs::remove_dir_all(directory).unwrap();
}
