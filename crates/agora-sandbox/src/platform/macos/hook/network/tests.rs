use super::super::{HOOK_INITIALIZED, flush_filesystem_at_exit, initialize_hook, initialized};
use super::*;
use crate::platform::hook::config::HookConfig;
use crate::protocol::parse_connect_request_prefix;
use std::ffi::{CStr, CString};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

static CAPTURED: Mutex<Option<(ConnectRequest, SocketAddr)>> = Mutex::new(None);
static SHORT_WRITE: AtomicBool = AtomicBool::new(false);

fn serve_until_stopped(
    listener: TcpListener,
    stopped: Arc<AtomicBool>,
    handler: impl Fn(TcpStream) + Send + 'static,
) -> thread::JoinHandle<usize> {
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        let mut accepted = 0;
        while !stopped.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    accepted += 1;
                    stream.set_nonblocking(false).unwrap();
                    handler(stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("test listener failed: {error}"),
            }
        }
        accepted
    })
}

fn read_frame_result(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut prefix = [0_u8; 4];
    stream.read_exact(&mut prefix)?;
    let mut frame = vec![0_u8; u32::from_be_bytes(prefix) as usize];
    stream.read_exact(&mut frame)?;
    Ok(frame)
}

fn accepted_audit_server(
    listener: TcpListener,
    stopped: Arc<AtomicBool>,
) -> thread::JoinHandle<usize> {
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        let requests = Arc::new(AtomicUsize::new(0));
        let mut connections = Vec::new();
        while !stopped.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let requests = Arc::clone(&requests);
                    connections.push(thread::spawn(move || {
                        loop {
                            match read_frame_result(&mut stream) {
                                Ok(_request) => {
                                    let response = br#""Accepted""#;
                                    stream
                                        .write_all(&(response.len() as u32).to_be_bytes())
                                        .unwrap();
                                    stream.write_all(response).unwrap();
                                    requests.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        std::io::ErrorKind::UnexpectedEof
                                            | std::io::ErrorKind::ConnectionAborted
                                            | std::io::ErrorKind::ConnectionReset
                                            | std::io::ErrorKind::BrokenPipe
                                    ) =>
                                {
                                    break;
                                }
                                Err(error) => panic!("audit fixture failed: {error}"),
                            }
                        }
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("test listener failed: {error}"),
            }
        }
        for connection in connections {
            connection.join().unwrap();
        }
        requests.load(Ordering::Relaxed)
    })
}

fn denied_execution_server(
    listener: TcpListener,
    stopped: Arc<AtomicBool>,
) -> thread::JoinHandle<usize> {
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        let requests = Arc::new(AtomicUsize::new(0));
        let mut connections = Vec::new();
        while !stopped.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let requests = Arc::clone(&requests);
                    connections.push(thread::spawn(move || {
                        let mut persistent = false;
                        while let Ok(request) = read_frame_result(&mut stream) {
                            let ping = request.get(2) == Some(&0);
                            let message = b"denied by coverage fixture";
                            let mut body = Vec::with_capacity(11 + message.len());
                            body.extend_from_slice(
                                &crate::execution::EXECUTION_PROTOCOL_VERSION.to_be_bytes(),
                            );
                            if ping {
                                body.push(0);
                                body.extend_from_slice(&0_u32.to_be_bytes());
                                persistent = true;
                            } else {
                                body.push(2);
                                body.extend_from_slice(&((4 + message.len()) as u32).to_be_bytes());
                                body.extend_from_slice(&libc::EACCES.to_be_bytes());
                                body.extend_from_slice(message);
                                requests.fetch_add(1, Ordering::Relaxed);
                            }
                            stream
                                .write_all(&(body.len() as u32).to_be_bytes())
                                .unwrap();
                            stream.write_all(&body).unwrap();
                            if !ping && !persistent {
                                break;
                            }
                        }
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("test listener failed: {error}"),
            }
        }
        for connection in connections {
            connection.join().unwrap();
        }
        requests.load(Ordering::Relaxed)
    })
}

fn proxy_capture_server(
    listener: TcpListener,
    stopped: Arc<AtomicBool>,
) -> thread::JoinHandle<usize> {
    serve_until_stopped(listener, stopped, |mut stream| {
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut request = Vec::new();
        loop {
            let mut bytes = [0_u8; 1024];
            match stream.read(&mut bytes) {
                Ok(0) => break,
                Ok(read) => {
                    request.extend_from_slice(&bytes[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(error) => panic!("proxy capture failed: {error}"),
            }
        }
    })
}

fn c_path(path: &std::path::Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap()
}

fn configured_hook_child() {
    use crate::platform::hook::{filesystem, process};

    assert!(initialized());

    let destination = RawSocketAddress::new("203.0.113.10:443".parse().unwrap());
    let stream = socket(libc::SOCK_STREAM);
    assert_eq!(
        unsafe { agora_sandbox_connect(stream, destination.as_ptr(), destination.len()) },
        0
    );
    unsafe { libc::close(stream) };

    let stream = socket(libc::SOCK_STREAM);
    let endpoints = SocketEndpoints {
        source_interface: 0,
        source_address: std::ptr::null(),
        source_address_length: 0,
        destination_address: destination.as_ptr(),
        destination_address_length: destination.len(),
    };
    let mut bytes_written = usize::MAX;
    let mut connection_id = u32::MAX;
    assert_eq!(
        unsafe {
            agora_sandbox_connectx(
                stream,
                &endpoints,
                0,
                0,
                std::ptr::null(),
                0,
                &mut bytes_written,
                &mut connection_id,
            )
        },
        0
    );
    assert_eq!(bytes_written, 0);
    assert_eq!(connection_id, 0);
    unsafe { libc::close(stream) };

    let stream = socket(libc::SOCK_STREAM);
    assert_eq!(
        unsafe {
            agora_sandbox_connectx(
                stream,
                &endpoints,
                0,
                1,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EACCES)
    );
    unsafe { libc::close(stream) };

    let datagram = socket(libc::SOCK_DGRAM);
    assert_eq!(
        unsafe { agora_sandbox_connect(datagram, destination.as_ptr(), destination.len()) },
        0
    );
    unsafe { libc::close(datagram) };

    assert_eq!(
        unsafe { agora_sandbox_connect(-1, destination.as_ptr(), destination.len()) },
        -1
    );

    let proxy: std::net::SocketAddr = std::env::var("AGORA_SANDBOX_PROXY_IPV4")
        .unwrap()
        .parse()
        .unwrap();
    let proxy = RawSocketAddress::new(proxy);
    let stream = socket(libc::SOCK_STREAM);
    assert_eq!(
        unsafe { agora_sandbox_connect(stream, proxy.as_ptr(), proxy.len()) },
        0
    );
    unsafe { libc::close(stream) };

    let stream = socket(libc::SOCK_STREAM);
    let endpoints = SocketEndpoints {
        source_interface: 0,
        source_address: std::ptr::null(),
        source_address_length: 0,
        destination_address: proxy.as_ptr(),
        destination_address_length: proxy.len(),
    };
    assert_eq!(
        unsafe {
            agora_sandbox_connectx(
                stream,
                &endpoints,
                0,
                0,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    unsafe { libc::close(stream) };

    let source = PathBuf::from(std::env::var("AGORA_SANDBOX_COVERAGE_SOURCE").unwrap());
    let directory = source.parent().unwrap().to_path_buf();
    let source = c_path(&source);
    let directory_path = c_path(&directory);

    unsafe {
        let descriptor =
            filesystem::agora_sandbox_open_with_mode(source.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        assert_eq!(filesystem::agora_sandbox_close(descriptor), 0);

        let file = filesystem::agora_sandbox_fopen(source.as_ptr(), c"r".as_ptr());
        assert!(!file.is_null());
        assert_eq!(filesystem::agora_sandbox_fclose(file), 0);

        let mut status = std::mem::zeroed();
        assert_eq!(
            filesystem::agora_sandbox_stat(source.as_ptr(), &mut status),
            0
        );
        assert_eq!(
            filesystem::agora_sandbox_lstat(source.as_ptr(), &mut status),
            0
        );
        assert_eq!(
            filesystem::agora_sandbox_access(source.as_ptr(), libc::R_OK),
            0
        );

        let directory_descriptor = filesystem::agora_sandbox_open_with_mode(
            directory_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        );
        assert!(directory_descriptor >= 0);
        let descriptor = filesystem::agora_sandbox_openat_with_mode(
            directory_descriptor,
            c"source.txt".as_ptr(),
            libc::O_RDONLY,
            0,
        );
        assert!(descriptor >= 0);
        assert_eq!(filesystem::agora_sandbox_close(descriptor), 0);
        assert_eq!(
            filesystem::agora_sandbox_fstatat(
                directory_descriptor,
                c"source.txt".as_ptr(),
                &mut status,
                0,
            ),
            0
        );
        assert_eq!(filesystem::agora_sandbox_close(directory_descriptor), 0);

        let created = directory.join("created.txt");
        let created_path = c_path(&created);
        let descriptor = filesystem::agora_sandbox_open_with_mode(
            created_path.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(libc::write(descriptor, b"created".as_ptr().cast(), 7), 7);
        assert_eq!(filesystem::agora_sandbox_close(descriptor), 0);

        let created_directory = directory.join("created-directory");
        let created_directory_path = c_path(&created_directory);
        assert_eq!(
            filesystem::agora_sandbox_mkdir(created_directory_path.as_ptr(), 0o700),
            0
        );
        let renamed = created_directory.join("renamed.txt");
        let renamed_path = c_path(&renamed);
        assert_eq!(
            filesystem::agora_sandbox_rename(created_path.as_ptr(), renamed_path.as_ptr()),
            0
        );

        let handle = filesystem::agora_sandbox_opendir(directory_path.as_ptr());
        assert!(!handle.is_null());
        while !filesystem::agora_sandbox_readdir(handle).is_null() {}
        assert_eq!(filesystem::agora_sandbox_closedir(handle), 0);

        let original_directory = std::env::current_dir().unwrap();
        assert_eq!(
            filesystem::agora_sandbox_chdir(created_directory_path.as_ptr()),
            0
        );
        let current = filesystem::agora_sandbox_getcwd(std::ptr::null_mut(), 0);
        assert!(!current.is_null());
        assert_eq!(
            CStr::from_ptr(current).to_bytes(),
            created_directory.as_os_str().as_bytes()
        );
        libc::free(current.cast());
        assert_eq!(libc::chdir(c_path(&original_directory).as_ptr()), 0);

        assert_eq!(filesystem::agora_sandbox_unlink(renamed_path.as_ptr()), 0);
        assert_eq!(
            filesystem::agora_sandbox_rmdir(created_directory_path.as_ptr()),
            0
        );
    }

    let executable = CString::new("/usr/bin/true").unwrap();
    let command = CString::new("true").unwrap();
    let mut direct_arguments = [executable.as_ptr().cast_mut(), std::ptr::null_mut()];
    let mut searched_arguments = [command.as_ptr().cast_mut(), std::ptr::null_mut()];
    let mut pid = 0;
    assert_eq!(
        unsafe {
            process::agora_sandbox_posix_spawn(
                &mut pid,
                executable.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                direct_arguments.as_mut_ptr(),
                std::ptr::null(),
            )
        },
        libc::EACCES
    );
    assert_eq!(
        unsafe {
            process::agora_sandbox_posix_spawnp(
                &mut pid,
                command.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                searched_arguments.as_mut_ptr(),
                std::ptr::null(),
            )
        },
        libc::EACCES
    );
    let direct_arguments = [executable.as_ptr(), std::ptr::null()];
    let searched_arguments = [command.as_ptr(), std::ptr::null()];
    for result in [
        unsafe {
            process::agora_sandbox_execve(
                executable.as_ptr(),
                direct_arguments.as_ptr(),
                std::ptr::null(),
            )
        },
        unsafe { process::agora_sandbox_execv(executable.as_ptr(), direct_arguments.as_ptr()) },
        unsafe { process::agora_sandbox_execvp(command.as_ptr(), searched_arguments.as_ptr()) },
    ] {
        assert_eq!(result, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EACCES)
        );
    }
}

#[test]
fn configured_hook_runtime_exercises_exported_entry_points() {
    if std::env::var_os("AGORA_SANDBOX_COVERAGE_CHILD").is_some() {
        configured_hook_child();
        return;
    }

    let directory =
        std::env::temp_dir().join(format!("agora-configured-hook-{}", uuid::Uuid::new_v4()));
    let lower = directory.join("lower");
    std::fs::create_dir_all(&lower).unwrap();
    let source = lower.join("source.txt");
    std::fs::write(&source, b"source").unwrap();

    let proxy_ipv4 = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_ipv4_address = proxy_ipv4.local_addr().unwrap();
    let proxy_ipv6 = TcpListener::bind("[::1]:0").unwrap();
    let proxy_ipv6_address = proxy_ipv6.local_addr().unwrap();
    let execution = TcpListener::bind("127.0.0.1:0").unwrap();
    let execution_address = execution.local_addr().unwrap();
    let audit = TcpListener::bind("127.0.0.1:0").unwrap();
    let audit_address = audit.local_addr().unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let proxy_ipv4_server = proxy_capture_server(proxy_ipv4, Arc::clone(&stopped));
    let proxy_ipv6_server = proxy_capture_server(proxy_ipv6, Arc::clone(&stopped));
    let execution_server = denied_execution_server(execution, Arc::clone(&stopped));
    let audit_server = accepted_audit_server(audit, Arc::clone(&stopped));

    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "platform::hook::network::tests::configured_hook_runtime_exercises_exported_entry_points",
            "--nocapture",
        ])
        .env("AGORA_SANDBOX_COVERAGE_CHILD", "1")
        .env("AGORA_SANDBOX_TOKEN", "coverage-token")
        .env("AGORA_SANDBOX_PROXY_IPV4", proxy_ipv4_address.to_string())
        .env("AGORA_SANDBOX_PROXY_IPV6", proxy_ipv6_address.to_string())
        .env(
            "AGORA_SANDBOX_EXECUTION_CONTROL",
            execution_address.to_string(),
        )
        .env("AGORA_SANDBOX_EXECUTION_TOKEN", "execution-token")
        .env("AGORA_SANDBOX_AUDIT_CONTROL", audit_address.to_string())
        .env("AGORA_SANDBOX_AUDIT_TOKEN", "audit-token")
        .env(
            "AGORA_SANDBOX_HOOK_LIBRARIES",
            std::env::current_exe().unwrap(),
        )
        .env(
            "AGORA_SANDBOX_FILESYSTEM_ROOT",
            directory.join("workdir/fs"),
        )
        .env("AGORA_SANDBOX_FILESYSTEM_MODE", "plain")
        .env("AGORA_SANDBOX_TRACE_ID", "coverage-root")
        .env("AGORA_SANDBOX_COVERAGE_SOURCE", &source)
        .output()
        .unwrap();

    stopped.store(true, Ordering::Release);
    let proxy_ipv4_connections = proxy_ipv4_server.join().unwrap();
    let proxy_ipv6_connections = proxy_ipv6_server.join().unwrap();
    let execution_requests = execution_server.join().unwrap();
    let audit_requests = audit_server.join().unwrap();
    let _ = std::fs::remove_dir_all(&directory);

    assert!(
        output.status.success(),
        "configured hook child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(proxy_ipv4_connections >= 3);
    assert_eq!(proxy_ipv6_connections, 0);
    assert_eq!(execution_requests, 5);
    assert!(audit_requests >= 15);
}

unsafe extern "C" fn recording_connectx(
    _socket: libc::c_int,
    endpoints: *const SocketEndpoints,
    _association_id: AssociationId,
    _flags: libc::c_uint,
    vectors: *const libc::iovec,
    vector_count: libc::c_uint,
    bytes_written: *mut libc::size_t,
    _connection_id: *mut ConnectionId,
) -> libc::c_int {
    let endpoints = unsafe { &*endpoints };
    let proxy = unsafe {
        socket_addr_from_raw(
            endpoints.destination_address,
            endpoints.destination_address_length,
        )
    }
    .unwrap();
    let vector = unsafe { &*vectors };
    let request =
        unsafe { std::slice::from_raw_parts(vector.iov_base.cast::<u8>(), vector.iov_len) };
    let request = parse_connect_request_prefix(request).unwrap().unwrap().0;
    *CAPTURED.lock().unwrap() = Some((request, proxy));
    if !bytes_written.is_null() {
        unsafe {
            *bytes_written = if SHORT_WRITE.load(Ordering::Relaxed) && vector_count == 1 {
                vector.iov_len.saturating_sub(1)
            } else {
                vector.iov_len
            };
        }
    }
    0
}

fn runtime() -> HookRuntime {
    let config = HookConfig::from_getter(|key| {
        Some(
            match key {
                "AGORA_SANDBOX_TOKEN" => "hook-token",
                "AGORA_SANDBOX_PROXY_IPV4" => "127.0.0.1:41000",
                "AGORA_SANDBOX_PROXY_IPV6" => "[::1]:41001",
                "AGORA_SANDBOX_EXECUTION_CONTROL" => "127.0.0.1:41002",
                "AGORA_SANDBOX_EXECUTION_TOKEN" => "execution-token",
                "AGORA_SANDBOX_AUDIT_CONTROL" => "127.0.0.1:41003",
                "AGORA_SANDBOX_AUDIT_TOKEN" => "audit-token",
                "AGORA_SANDBOX_HOOK_LIBRARIES" => "/tmp/hook.dylib",
                "AGORA_SANDBOX_FILESYSTEM_ROOT" => "/tmp/agora-fs",
                "AGORA_SANDBOX_FILESYSTEM_MODE" => "plain",
                "AGORA_SANDBOX_TRACE_ID" => "trace-root",
                _ => return None,
            }
            .to_string(),
        )
    })
    .unwrap();
    HookRuntime {
        config,
        process: ProcessContext::new("/tmp/client".to_string()),
    }
}

fn socket(kind: libc::c_int) -> libc::c_int {
    let socket = unsafe { libc::socket(libc::AF_INET, kind, 0) };
    assert!(socket >= 0);
    socket
}

#[test]
fn hook_guard_blocks_recursion_and_reopens_after_drop() {
    let guard = HookGuard::enter().expect("first hook entry should succeed");
    assert!(HookGuard::enter().is_none());
    drop(guard);
    assert!(HookGuard::enter().is_some());
}

#[test]
fn network_hook_guard_blocks_catchable_signals_while_state_is_active() {
    let signal = super::super::tests::SignalMaskProbe::unblocked(libc::SIGUSR2);
    let guard = HookGuard::enter().unwrap();

    assert!(signal.is_blocked());
    drop(guard);
    assert!(!signal.is_blocked());
}

unsafe extern "C" fn connectx_requiring_unblocked_signals(
    _socket: libc::c_int,
    _endpoints: *const SocketEndpoints,
    _association_id: AssociationId,
    _flags: libc::c_uint,
    vectors: *const libc::iovec,
    vector_count: libc::c_uint,
    bytes_written: *mut libc::size_t,
    _connection_id: *mut ConnectionId,
) -> libc::c_int {
    if super::super::tests::SignalMaskProbe::signal_is_blocked(libc::SIGUSR2) {
        unsafe { set_errno(libc::EBUSY) };
        return -1;
    }
    if vector_count == 1 && !bytes_written.is_null() {
        unsafe { *bytes_written = (*vectors).iov_len };
    }
    0
}

#[test]
fn native_connect_runs_after_network_hook_state_is_released() {
    let signal = super::super::tests::SignalMaskProbe::unblocked(libc::SIGUSR2);
    let runtime = runtime();
    let destination = "203.0.113.10:443".parse().unwrap();
    let socket = socket(libc::SOCK_STREAM);
    let guard = HookGuard::enter().unwrap();
    let prepared = runtime
        .prepare_connect(destination, HookOperation::Connect)
        .unwrap();

    assert!(signal.is_blocked());
    drop(guard);
    assert_eq!(
        unsafe { prepared.connect(socket, connectx_requiring_unblocked_signals) },
        0
    );
    assert!(!signal.is_blocked());
    unsafe { libc::close(socket) };
}

#[test]
fn intercepted_destination_accepts_only_valid_stream_sockets() {
    let address = RawSocketAddress::new("203.0.113.10:443".parse().unwrap());
    let stream = socket(libc::SOCK_STREAM);
    let datagram = socket(libc::SOCK_DGRAM);

    assert_eq!(
        unsafe { HookRuntime::intercepted_destination(stream, address.as_ptr(), address.len()) },
        Ok(Some("203.0.113.10:443".parse().unwrap()))
    );
    assert_eq!(
        unsafe { HookRuntime::intercepted_destination(datagram, address.as_ptr(), address.len()) },
        Ok(None)
    );
    assert_eq!(
        unsafe { HookRuntime::intercepted_destination(stream, std::ptr::null(), 0) },
        Ok(None)
    );
    assert_eq!(
        unsafe { HookRuntime::intercepted_destination(-1, address.as_ptr(), address.len()) },
        Err(())
    );

    unsafe {
        libc::close(stream);
        libc::close(datagram);
    }
}

#[test]
fn runtime_encodes_connect_metadata_and_rejects_short_proxy_writes() {
    let runtime = runtime();
    let destination: SocketAddr = "203.0.113.10:443".parse().unwrap();
    let socket = socket(libc::SOCK_STREAM);

    SHORT_WRITE.store(false, Ordering::Relaxed);
    assert_eq!(
        unsafe {
            runtime
                .prepare_connect(destination, HookOperation::Connectx)
                .unwrap()
                .connect(socket, recording_connectx)
        },
        0
    );
    let (request, proxy) = CAPTURED.lock().unwrap().take().unwrap();
    assert_eq!(request.token, "hook-token");
    assert_eq!(request.destination, destination);
    assert_eq!(request.process.executable, "/tmp/client");
    assert_eq!(request.operation, HookOperation::Connectx);
    assert_eq!(proxy, "127.0.0.1:41000".parse().unwrap());

    SHORT_WRITE.store(true, Ordering::Relaxed);
    assert_eq!(
        unsafe {
            runtime
                .prepare_connect(destination, HookOperation::Connect)
                .unwrap()
                .connect(socket, recording_connectx)
        },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EPROTO)
    );
    SHORT_WRITE.store(false, Ordering::Relaxed);

    unsafe { libc::close(socket) };
}

#[test]
fn exported_hooks_validate_initialization_and_pointer_shapes() {
    initialize_hook();
    assert!(HOOK_INITIALIZED.load(Ordering::Acquire));
    assert!(original_connect().is_some());
    assert!(original_connectx().is_some());

    let null = DyldInterpose {
        replacement: std::ptr::null(),
        replacee: std::ptr::null(),
    };
    assert!(function_from_interpose::<ConnectFn>(&null).is_none());
    let present = DyldInterpose {
        replacement: std::ptr::null(),
        replacee: libc::connect as *const () as *const libc::c_void,
    };
    assert!(function_from_interpose::<ConnectFn>(&present).is_some());

    assert_eq!(
        unsafe {
            agora_sandbox_connectx(
                -1,
                std::ptr::null(),
                0,
                0,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EINVAL)
    );

    let stream = socket(libc::SOCK_STREAM);
    let destination = RawSocketAddress::new("203.0.113.10:443".parse().unwrap());
    HOOK_INITIALIZED.store(false, Ordering::Release);
    assert_eq!(
        unsafe { agora_sandbox_connect(stream, destination.as_ptr(), destination.len()) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EACCES)
    );
    initialize_hook();
    flush_filesystem_at_exit();

    unsafe { libc::close(stream) };
}
