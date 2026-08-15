use super::{
    PROTOCOL_VERSION, RemoteConnectionStatus, RemoteController, RemoteControllerEvent,
    RemoteRuntime, configure_server_stream,
};
use crate::ipc::{InheritedControlLock, InheritedControlStream};
use crate::nfs::client::RemoteClient;
use crate::nfs::protocol::{
    RemotePath, Request, RequestEnvelope, RequestId, Response, ResponseEnvelope,
};
use crate::nfs::testing::MemoryStorage;
use crate::nfs::transport;
use anyhow::Result;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinSet;

fn idle_controller(
    root: &Path,
    tasks: JoinSet<Result<()>>,
    connection_probes: JoinSet<RemoteConnectionStatus>,
) -> RemoteController {
    let (shutdown, _receiver) = watch::channel(false);
    RemoteController {
        runtime: RemoteRuntime {
            socket: root.join("unused.sock"),
            token: "token".to_string(),
        },
        shutdown,
        tasks,
        connection_probes,
    }
}

async fn raw_response(
    controller: &RemoteController,
    version: u16,
    include_descriptor: bool,
) -> ResponseEnvelope {
    let socket = controller.runtime().socket().to_path_buf();
    let token = controller.runtime().token().to_string();
    tokio::task::spawn_blocking(move || {
        let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
        let descriptor = tempfile::tempfile().unwrap();
        transport::send(
            &mut stream,
            &RequestEnvelope {
                version,
                token,
                request_id: RequestId::new("0123456789abcdef0123456789abcdef").unwrap(),
                request: Request::Stat {
                    path: RemotePath::new(0, "file.txt").unwrap(),
                    name_capacity: 0,
                },
            },
            include_descriptor.then_some(descriptor.as_raw_fd()),
        )
        .unwrap();
        transport::receive::<ResponseEnvelope>(&mut stream)
            .unwrap()
            .0
    })
    .await
    .unwrap()
}

#[test]
fn controller_bounds_blocking_request_and_response_io() {
    let (server, _client) = std::os::unix::net::UnixStream::pair().unwrap();
    let read = Duration::from_millis(25);
    let write = Duration::from_millis(50);

    configure_server_stream(&server, read, write).unwrap();

    assert_eq!(server.read_timeout().unwrap(), Some(read));
    assert_eq!(server.write_timeout().unwrap(), Some(write));
}

#[tokio::test]
async fn controller_probes_remote_roots_without_blocking_startup() {
    let runtime = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    storage.block_connections();
    storage.fail_connection(1, libc::EACCES, "credentials rejected");

    let mut controller = tokio::time::timeout(
        Duration::from_millis(100),
        RemoteController::start_with_storage_and_connection_probes(
            Arc::clone(&storage),
            runtime.path(),
            &[None, None],
        ),
    )
    .await
    .expect("controller startup must not await connection probes")
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), controller.wait_event())
            .await
            .is_err(),
        "blocked connection probes must still be running in the background"
    );

    storage.release_connections();
    let mut statuses = Vec::new();
    for _ in 0..2 {
        let event = tokio::time::timeout(Duration::from_secs(1), controller.wait_event())
            .await
            .unwrap();
        let RemoteControllerEvent::Connection(status) = event else {
            panic!("connection failure must not stop the controller");
        };
        statuses.push(status);
    }
    statuses.sort_by_key(RemoteConnectionStatus::root);
    assert_eq!(
        statuses,
        vec![
            RemoteConnectionStatus::Connected { root: 0 },
            RemoteConnectionStatus::Unavailable {
                root: 1,
                errno: libc::EACCES,
            },
        ]
    );
    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn controller_reports_a_failed_local_preflight_without_connecting() {
    let runtime = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    storage.block_connections();

    let mut controller = RemoteController::start_with_storage_and_connection_probes(
        storage,
        runtime.path(),
        &[Some(libc::ENOENT)],
    )
    .await
    .unwrap();

    let event = tokio::time::timeout(Duration::from_millis(100), controller.wait_event())
        .await
        .expect("a local preflight failure must not wait for the remote connection");
    assert!(matches!(
        event,
        RemoteControllerEvent::Connection(RemoteConnectionStatus::Unavailable {
            root: 0,
            errno: libc::ENOENT,
        })
    ));
    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn controller_authenticates_requests_and_transfers_open_descriptors() {
    let runtime = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    storage.insert_file(0, "file.txt", b"through broker");
    let controller = RemoteController::start_with_storage(Arc::clone(&storage), runtime.path())
        .await
        .unwrap();
    let client = RemoteClient::new(controller.runtime().socket(), controller.runtime().token());

    let open_client = client.clone();
    let reply = tokio::task::spawn_blocking(move || {
        open_client.request(Request::Open {
            path: RemotePath::new(0, "file.txt").unwrap(),
            flags: libc::O_RDONLY,
            mode: 0,
        })
    })
    .await
    .unwrap()
    .unwrap();

    let Response::Open { handle, .. } = reply.response else {
        panic!("expected open response");
    };
    let read = tokio::task::spawn_blocking(move || {
        client.request(Request::Read {
            handle,
            offset: 0,
            length: 64,
        })
    })
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(read.response, Response::Read { length: 14, .. }));
    let mut file = std::fs::File::from(read.descriptor.unwrap());
    let mut contents = String::new();
    std::io::Read::read_to_string(&mut file, &mut contents).unwrap();
    assert_eq!(contents, "through broker");
    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn authenticated_remote_control_stream_survives_new_connection_denial() {
    let runtime = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    storage.insert_file(0, "file.txt", b"remote");
    let controller = RemoteController::start_with_storage(storage, runtime.path())
        .await
        .unwrap();
    let socket = controller.runtime().socket().to_path_buf();
    let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let shared =
        InheritedControlStream::new(stream, InheritedControlLock::anonymous().unwrap(), 0).unwrap();
    let client = RemoteClient::with_shared(&socket, controller.runtime().token(), shared);

    let client = tokio::task::spawn_blocking(move || {
        client.ping_shared().unwrap();
        client
    })
    .await
    .unwrap();
    std::fs::remove_file(&socket).unwrap();
    let (reply, client) = tokio::task::spawn_blocking(move || {
        let reply = client.request(Request::Access {
            path: RemotePath::new(0, "file.txt").unwrap(),
            mode: libc::R_OK,
        });
        (reply, client)
    })
    .await
    .unwrap();
    let reply = reply.unwrap();
    assert_eq!(reply.response, Response::Success);

    drop(client);
    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_closes_idle_persistent_remote_control_streams() {
    let runtime = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    let controller = RemoteController::start_with_storage(storage, runtime.path())
        .await
        .unwrap();
    let socket = controller.runtime().socket().to_path_buf();
    let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let mut observer = stream.try_clone().unwrap();
    observer
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let shared =
        InheritedControlStream::new(stream, InheritedControlLock::anonymous().unwrap(), 0).unwrap();
    let client = RemoteClient::with_shared(&socket, controller.runtime().token(), shared);

    let client = tokio::task::spawn_blocking(move || {
        client.ping_shared().unwrap();
        client
    })
    .await
    .unwrap();

    tokio::time::timeout(Duration::from_secs(1), controller.shutdown())
        .await
        .expect("idle persistent remote stream blocked broker shutdown")
        .unwrap();
    let disconnected = tokio::task::spawn_blocking(move || {
        let mut byte = [0_u8; 1];
        std::io::Read::read(&mut observer, &mut byte)
    })
    .await
    .unwrap();
    assert_eq!(disconnected.unwrap(), 0);
    drop(client);
}

#[tokio::test]
async fn controller_rejects_an_invalid_token_before_storage_access() {
    let runtime = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    storage.insert_file(0, "file.txt", b"secret");
    let controller = RemoteController::start_with_storage(storage, runtime.path())
        .await
        .unwrap();
    let client = RemoteClient::new(controller.runtime().socket(), "wrong-token");

    let error = tokio::task::spawn_blocking(move || {
        client.request(Request::Stat {
            path: RemotePath::new(0, "file.txt").unwrap(),
            name_capacity: 0,
        })
    })
    .await
    .unwrap()
    .unwrap_err();

    assert_eq!(error.errno(), libc::EACCES);
    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn controller_reports_an_injected_service_failure() {
    let runtime = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    let mut controller = RemoteController::start_with_storage(storage, runtime.path())
        .await
        .unwrap();

    controller.abort_server_for_test();

    let error = controller.wait_failure().await;
    assert!(format!("{error:#}").contains("injected remote filesystem failure"));
}

#[tokio::test]
async fn controller_rejects_protocol_versions_and_request_descriptors() {
    let runtime = tempfile::tempdir().unwrap();
    let controller =
        RemoteController::start_with_storage(Arc::new(MemoryStorage::default()), runtime.path())
            .await
            .unwrap();

    for response in [
        raw_response(&controller, PROTOCOL_VERSION + 1, false).await,
        raw_response(&controller, PROTOCOL_VERSION, true).await,
    ] {
        assert!(matches!(
            response.response,
            Response::Error {
                errno: libc::EPROTO,
                ..
            }
        ));
    }

    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn controller_reports_all_service_and_probe_task_outcomes() {
    let root = tempfile::tempdir().unwrap();

    let mut successful = JoinSet::new();
    successful.spawn(async { Ok(()) });
    let mut controller = idle_controller(root.path(), successful, JoinSet::new());
    let RemoteControllerEvent::Failure(error) = controller.wait_event().await else {
        panic!("completed server task must be reported as a failure");
    };
    assert!(error.to_string().contains("stopped unexpectedly"));

    let mut panicked = JoinSet::new();
    panicked.spawn(async {
        panic!("server panic");
        #[allow(unreachable_code)]
        Ok(())
    });
    let mut controller = idle_controller(root.path(), panicked, JoinSet::new());
    let RemoteControllerEvent::Failure(error) = controller.wait_event().await else {
        panic!("panicked server task must be reported as a failure");
    };
    assert!(format!("{error:#}").contains("broker task failed"));

    let mut controller = idle_controller(root.path(), JoinSet::new(), JoinSet::new());
    let RemoteControllerEvent::Failure(error) = controller.wait_event().await else {
        panic!("missing server task must be reported as a failure");
    };
    assert!(error.to_string().contains("no active task"));

    let mut tasks = JoinSet::new();
    tasks.spawn(async {
        std::future::pending::<()>().await;
        Ok(())
    });
    let mut probes = JoinSet::new();
    probes.spawn(async {
        panic!("probe panic");
        #[allow(unreachable_code)]
        RemoteConnectionStatus::Connected { root: 0 }
    });
    let mut controller = idle_controller(root.path(), tasks, probes);
    let RemoteControllerEvent::Failure(error) = controller.wait_event().await else {
        panic!("panicked probe task must be reported as a failure");
    };
    assert!(format!("{error:#}").contains("connection probe task failed"));

    let mut tasks = JoinSet::new();
    tasks.spawn(async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        anyhow::bail!("failure after probe")
    });
    let mut probes = JoinSet::new();
    probes.spawn(async { RemoteConnectionStatus::Connected { root: 0 } });
    let mut controller = idle_controller(root.path(), tasks, probes);
    assert!(
        format!("{:#}", controller.wait_failure().await).contains("failure after probe"),
        "wait_failure must skip connection status events"
    );
}

#[tokio::test]
async fn controller_shutdown_preserves_task_and_socket_cleanup_errors() {
    let root = tempfile::tempdir().unwrap();
    let mut tasks = JoinSet::new();
    tasks.spawn(async { anyhow::bail!("first shutdown failure") });
    tasks.spawn(async { anyhow::bail!("second shutdown failure") });
    let mut probes = JoinSet::new();
    probes.spawn(async { std::future::pending::<RemoteConnectionStatus>().await });
    let controller = idle_controller(root.path(), tasks, probes);
    assert!(
        controller
            .shutdown()
            .await
            .unwrap_err()
            .to_string()
            .contains("shutdown failure")
    );

    let socket = root.path().join("unused.sock");
    std::fs::create_dir(&socket).unwrap();
    let controller = idle_controller(root.path(), JoinSet::new(), JoinSet::new());
    assert!(controller.shutdown().await.is_err());
}

#[tokio::test]
async fn controller_startup_reports_runtime_directory_and_socket_errors() {
    let root = tempfile::tempdir().unwrap();
    let runtime_file = root.path().join("runtime-file");
    std::fs::write(&runtime_file, b"not a directory").unwrap();
    assert!(
        RemoteController::start_with_storage(Arc::new(MemoryStorage::default()), &runtime_file,)
            .await
            .is_err()
    );

    let runtime = root.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::create_dir(runtime.join("nfs.sock")).unwrap();
    assert!(
        RemoteController::start_with_storage(Arc::new(MemoryStorage::default()), &runtime)
            .await
            .is_err()
    );
}
