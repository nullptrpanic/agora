use super::*;
use crate::filesystem::broker::LocalClient;
use crate::filesystem::broker::protocol::{
    BackingPath, ByteRange, Request, RequestEnvelope, Response, ResponseEnvelope,
};
use crate::ipc::{InheritedControlLock, InheritedControlStream};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

fn cipher() -> FileCipher {
    FileCipher::derive(b"controller-key", b"0123456789abcdef").unwrap()
}

fn idle_controller(root: &Path, tasks: JoinSet<Result<()>>) -> LocalController {
    let (shutdown, _receiver) = watch::channel(false);
    LocalController {
        runtime: LocalRuntime {
            socket: root.join("unused.sock"),
            token: "token".to_string(),
        },
        broker: Arc::new(LocalBroker::new(root, cipher()).unwrap()),
        shutdown,
        tasks,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controller_serves_the_complete_local_client_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("fs");
    let runtime = directory.path().join("runtime");
    std::fs::create_dir(&root).unwrap();
    let cipher = cipher();
    let backing = root.join("content");
    let mut source = tempfile::tempfile().unwrap();
    source.write_all(b"before").unwrap();
    cipher.encrypt(&mut source, &backing).unwrap();
    let controller = LocalController::start(&root, cipher.clone(), &runtime)
        .await
        .unwrap();
    assert_eq!(
        controller.runtime().socket(),
        runtime.join("local-filesystem.sock")
    );
    assert_eq!(controller.runtime().token().len(), 32);
    let client = LocalClient::new(controller.runtime().socket(), controller.runtime().token());
    let handle = tokio::task::spawn_blocking(move || {
        let opened = client.open(&backing, libc::O_RDWR).unwrap();
        client
            .materialize(&opened.handle, Some(ByteRange::new(0, 3).unwrap()))
            .unwrap();
        let write = client
            .begin_write(&opened.handle, ByteRange::new(0, 6).unwrap())
            .unwrap();
        opened.descriptor.write_all_at(b"after!", 0).unwrap();
        client
            .finish_write(&opened.handle, &write, ByteRange::new(0, 6).unwrap())
            .unwrap();
        let cancelled = client
            .begin_write(&opened.handle, ByteRange::new(0, 1).unwrap())
            .unwrap();
        client.cancel_write(&opened.handle, &cancelled).unwrap();
        client
            .potentially_dirty(&opened.handle, ByteRange::new(0, 6).unwrap())
            .unwrap();
        client
            .sync(&opened.handle, vec![ByteRange::new(0, 3).unwrap()], false)
            .unwrap();
        client.retain(vec![opened.handle.clone()]).unwrap();
        client.close(&opened.handle, Vec::new()).unwrap();
        client.close(&opened.handle, Vec::new()).unwrap();
        opened.handle
    })
    .await
    .unwrap();
    assert_eq!(handle.len(), 32);
    controller.shutdown().await.unwrap();

    let mut restored = tempfile::tempfile().unwrap();
    cipher
        .decrypt(&root.join("content"), &mut restored)
        .unwrap();
    restored.seek(SeekFrom::Start(0)).unwrap();
    let mut contents = String::new();
    restored.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "after!");
    assert!(!runtime.join("local-filesystem.sock").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_local_control_stream_survives_new_connection_denial() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("fs");
    std::fs::create_dir(&root).unwrap();
    let controller = LocalController::start(&root, cipher(), &directory.path().join("runtime"))
        .await
        .unwrap();
    let socket = controller.runtime().socket().to_path_buf();
    let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let shared =
        InheritedControlStream::new(stream, InheritedControlLock::anonymous().unwrap(), 0).unwrap();
    let client = LocalClient::with_shared(&socket, controller.runtime().token(), shared);

    client.ping_shared().unwrap();
    std::fs::remove_file(&socket).unwrap();
    client.close("missing", Vec::new()).unwrap();

    drop(client);
    controller.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_closes_idle_persistent_control_streams() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("fs");
    std::fs::create_dir(&root).unwrap();
    let controller = LocalController::start(&root, cipher(), &directory.path().join("runtime"))
        .await
        .unwrap();
    let socket = controller.runtime().socket().to_path_buf();
    let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let shared =
        InheritedControlStream::new(stream, InheritedControlLock::anonymous().unwrap(), 0).unwrap();
    let client = LocalClient::with_shared(&socket, controller.runtime().token(), shared);

    client.ping_shared().unwrap();

    tokio::time::timeout(Duration::from_secs(1), controller.shutdown())
        .await
        .expect("idle persistent control stream blocked broker shutdown")
        .unwrap();
    assert!(client.close("missing", Vec::new()).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_service_work_before_the_final_flush() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("fs");
    std::fs::create_dir(&root).unwrap();
    let cipher = cipher();
    let backing = root.join("content");
    let mut source = tempfile::tempfile().unwrap();
    source.write_all(b"before").unwrap();
    cipher.encrypt(&mut source, &backing).unwrap();

    let broker = Arc::new(LocalBroker::new(&root, cipher.clone()).unwrap());
    let mut response = broker.handle(
        Request::Open {
            path: BackingPath::from_path(&backing),
            flags: libc::O_RDWR,
        },
        None,
    );
    let Response::Open { handle, .. } = response.response else {
        panic!("unexpected open response: {:?}", response.response);
    };
    let plaintext = response.descriptors.remove(0);

    let (shutdown, mut receiver) = watch::channel(false);
    let mut tasks = JoinSet::new();
    let task_broker = Arc::clone(&broker);
    tasks.spawn(async move {
        receiver.changed().await.unwrap();
        plaintext.write_all_at(b"after!", 0).unwrap();
        assert_eq!(
            task_broker
                .handle(
                    Request::PotentiallyDirty {
                        handle,
                        range: ByteRange::new(0, 6).unwrap(),
                    },
                    None,
                )
                .response,
            Response::Success
        );
        Ok(())
    });
    let controller = LocalController {
        runtime: LocalRuntime {
            socket: directory.path().join("unused.sock"),
            token: "token".to_string(),
        },
        broker,
        shutdown,
        tasks,
    };

    controller.shutdown().await.unwrap();

    let mut restored = tempfile::tempfile().unwrap();
    cipher.decrypt(&backing, &mut restored).unwrap();
    restored.seek(SeekFrom::Start(0)).unwrap();
    let mut contents = String::new();
    restored.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "after!");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_shutdown_waits_for_an_accepted_request() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("fs");
    std::fs::create_dir(&root).unwrap();
    let socket = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let token = "token".to_string();
    let state = Arc::new(ServerState {
        token: token.clone(),
        broker: Arc::new(LocalBroker::new(&root, cipher()).unwrap()),
    });
    let server = Server::new(listener, state);
    let connections = Arc::clone(&server.connections);
    let (shutdown, receiver) = watch::channel(false);
    let mut task = tokio::spawn(server.run(receiver));
    let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while connections.available_permits() == MAX_CONNECTIONS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    shutdown.send(true).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut task)
            .await
            .is_err()
    );

    let response = tokio::task::spawn_blocking(move || {
        let mut stream = stream;
        ipc::send(
            &mut stream,
            &RequestEnvelope {
                version: PROTOCOL_VERSION,
                token,
                request_id: "0".repeat(32),
                request: Request::Close {
                    handle: "missing".to_string(),
                    ranges: Vec::new(),
                },
            },
            None,
        )
        .unwrap();
        ipc::receive::<ResponseEnvelope>(&mut stream).unwrap().0
    })
    .await
    .unwrap();
    assert_eq!(response.response, Response::Success);
    task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_shutdown_does_not_miss_a_late_persistent_handshake() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("fs");
    std::fs::create_dir(&root).unwrap();
    let socket = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let token = "token".to_string();
    let state = Arc::new(ServerState {
        token: token.clone(),
        broker: Arc::new(LocalBroker::new(&root, cipher()).unwrap()),
    });
    let server = Server::new(listener, state);
    let connections = Arc::clone(&server.connections);
    let (shutdown, receiver) = watch::channel(false);
    let task = tokio::spawn(server.run(receiver));
    let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while connections.available_permits() == MAX_CONNECTIONS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    shutdown.send(true).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (response, stream) = tokio::task::spawn_blocking(move || {
        let mut stream = stream;
        ipc::send(
            &mut stream,
            &RequestEnvelope {
                version: PROTOCOL_VERSION,
                token,
                request_id: "0".repeat(32),
                request: Request::Ping,
            },
            None,
        )
        .unwrap();
        let response = ipc::receive::<ResponseEnvelope>(&mut stream).unwrap().0;
        (response, stream)
    })
    .await
    .unwrap();
    assert_eq!(response.response, Response::Success);
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("late persistent handshake escaped broker shutdown")
        .unwrap()
        .unwrap();
    drop(stream);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controller_rejects_wrong_tokens_versions_and_unexpected_descriptors() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("fs");
    std::fs::create_dir(&root).unwrap();
    let controller = LocalController::start(&root, cipher(), &directory.path().join("runtime"))
        .await
        .unwrap();

    let wrong = LocalClient::new(controller.runtime().socket(), "wrong");
    let error = tokio::task::spawn_blocking(move || wrong.close("missing", Vec::new()))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.errno(), libc::EACCES);

    let socket = controller.runtime().socket().to_path_buf();
    let token = controller.runtime().token().to_string();
    let invalid_id_socket = socket.clone();
    let invalid_id_token = token.clone();
    let invalid_write_socket = socket.clone();
    let invalid_write_token = token.clone();
    let response = tokio::task::spawn_blocking(move || {
        let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
        ipc::send(
            &mut stream,
            &RequestEnvelope {
                version: PROTOCOL_VERSION + 1,
                token,
                request_id: "old-version".to_string(),
                request: Request::Close {
                    handle: "missing".to_string(),
                    ranges: Vec::new(),
                },
            },
            Some(tempfile::tempfile().unwrap().as_raw_fd()),
        )
        .unwrap();
        ipc::receive::<ResponseEnvelope>(&mut stream).unwrap().0
    })
    .await
    .unwrap();
    assert!(matches!(
        response.response,
        Response::Error {
            errno: libc::EPROTO,
            ..
        }
    ));

    let response = tokio::task::spawn_blocking(move || {
        let mut stream = std::os::unix::net::UnixStream::connect(invalid_id_socket).unwrap();
        ipc::send(
            &mut stream,
            &RequestEnvelope {
                version: PROTOCOL_VERSION,
                token: invalid_id_token,
                request_id: "not-a-request-id".to_string(),
                request: Request::Close {
                    handle: "missing".to_string(),
                    ranges: Vec::new(),
                },
            },
            None,
        )
        .unwrap();
        ipc::receive::<ResponseEnvelope>(&mut stream).unwrap().0
    })
    .await
    .unwrap();
    assert!(matches!(
        response.response,
        Response::Error {
            errno: libc::EPROTO,
            ..
        }
    ));

    let response = tokio::task::spawn_blocking(move || {
        let mut stream = std::os::unix::net::UnixStream::connect(invalid_write_socket).unwrap();
        ipc::send(
            &mut stream,
            &RequestEnvelope {
                version: PROTOCOL_VERSION,
                token: invalid_write_token,
                request_id: "11111111111111111111111111111111".to_string(),
                request: Request::BeginWrite {
                    handle: "missing".to_string(),
                    write_id: "invalid".to_string(),
                    range: ByteRange::new(0, 1).unwrap(),
                },
            },
            None,
        )
        .unwrap();
        ipc::receive::<ResponseEnvelope>(&mut stream).unwrap().0
    })
    .await
    .unwrap();
    assert!(matches!(
        response.response,
        Response::Error {
            errno: libc::EPROTO,
            ..
        }
    ));
    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn controller_failure_reporting_distinguishes_task_outcomes() {
    let root = tempfile::tempdir().unwrap();

    let mut successful = JoinSet::new();
    successful.spawn(async { Ok(()) });
    let mut controller = idle_controller(root.path(), successful);
    assert!(
        controller
            .wait_failure()
            .await
            .to_string()
            .contains("stopped unexpectedly")
    );

    let mut failed = JoinSet::new();
    failed.spawn(async { anyhow::bail!("server error") });
    let mut controller = idle_controller(root.path(), failed);
    assert!(
        format!("{:#}", controller.wait_failure().await).contains("local filesystem broker failed")
    );

    let mut panicked = JoinSet::new();
    panicked.spawn(async {
        panic!("server panic");
        #[allow(unreachable_code)]
        Ok(())
    });
    let mut controller = idle_controller(root.path(), panicked);
    assert!(
        format!("{:#}", controller.wait_failure().await)
            .contains("local filesystem broker task failed")
    );

    let mut controller = idle_controller(root.path(), JoinSet::new());
    assert!(
        controller
            .wait_failure()
            .await
            .to_string()
            .contains("no active task")
    );
}

#[tokio::test]
async fn shutdown_reports_service_errors_and_drop_removes_the_socket() {
    let root = tempfile::tempdir().unwrap();
    let mut tasks = JoinSet::new();
    tasks.spawn(async { anyhow::bail!("shutdown failure") });
    let controller = idle_controller(root.path(), tasks);
    assert!(
        controller
            .shutdown()
            .await
            .unwrap_err()
            .to_string()
            .contains("shutdown failure")
    );

    let runtime = tempfile::tempdir().unwrap();
    let controller = LocalController::start(root.path(), cipher(), runtime.path())
        .await
        .unwrap();
    let socket = controller.runtime().socket().to_path_buf();
    assert!(socket.exists());
    drop(controller);
    assert!(!socket.exists());
}

#[tokio::test]
async fn startup_errors_and_constant_time_token_checks_are_explicit() {
    let directory = tempfile::tempdir().unwrap();
    let error = match LocalController::start(
        &directory.path().join("missing-root"),
        cipher(),
        &directory.path().join("runtime"),
    )
    .await
    {
        Ok(_) => panic!("unexpectedly started with a missing root"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("No such file"));

    assert!(constant_time_equal(b"same", b"same"));
    assert!(!constant_time_equal(b"same", b"diff"));
    assert!(!constant_time_equal(b"short", b"longer"));

    let request = Request::Open {
        path: BackingPath::from_path(Path::new("/tmp/example")),
        flags: libc::O_RDONLY,
    };
    assert!(matches!(request, Request::Open { .. }));
}
