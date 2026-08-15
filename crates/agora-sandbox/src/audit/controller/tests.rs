use super::*;
use crate::audit::protocol::{decode_response, encode_ping_request, encode_request};
use crate::callback::{Decision, FileAccessMode, FileContext, FileOpenMode, ProcessContext};
use std::sync::atomic::{AtomicUsize, Ordering};

async fn controller() -> AuditController {
    AuditController::start(
        "sandbox".to_string(),
        "run".to_string(),
        |_| std::future::ready(Decision::Allow),
        Duration::from_secs(1),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn audit_server_drops_connections_above_its_concurrency_limit() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = Arc::new(AuditState {
        token: "token".to_string(),
        sandbox_id: "sandbox".to_string(),
        run_id: "run".to_string(),
        callback: |_| std::future::ready(Decision::Allow),
        callback_timeout: Duration::from_secs(1),
        requests: Mutex::new(AuditRequestCache::default()),
    });
    let server = AuditServer::new(listener, state);
    let permits = (0..AUDIT_MAX_CONNECTIONS)
        .map(|_| Arc::clone(&server.connections).try_acquire_owned().unwrap())
        .collect::<Vec<_>>();
    let (shutdown, receiver) = watch::channel(false);
    let task = tokio::spawn(server.run(receiver));

    let mut stream = TcpStream::connect(address).await.unwrap();
    let mut byte = [0_u8; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );

    drop(permits);
    shutdown.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn audit_controller_reports_empty_successful_and_panicked_task_sets() {
    let mut empty = controller().await;
    empty.tasks.shutdown().await;
    assert!(
        empty
            .wait_failure()
            .await
            .to_string()
            .contains("no active task")
    );

    let mut stopped = controller().await;
    stopped.tasks.shutdown().await;
    stopped.tasks.spawn(async { Ok(()) });
    assert!(
        stopped
            .wait_failure()
            .await
            .to_string()
            .contains("stopped unexpectedly")
    );

    let mut panicked = controller().await;
    panicked.tasks.shutdown().await;
    panicked.tasks.spawn(async {
        panic!("injected audit task panic");
        #[allow(unreachable_code)]
        Ok(())
    });
    assert!(
        panicked
            .wait_failure()
            .await
            .to_string()
            .contains("audit task failed")
    );

    let mut shutdown = controller().await;
    shutdown.tasks.shutdown().await;
    shutdown.tasks.spawn(async {
        panic!("injected audit shutdown panic");
        #[allow(unreachable_code)]
        Ok(())
    });
    assert!(shutdown.shutdown().await.is_err());
}

#[tokio::test]
async fn audit_server_times_out_idle_established_connections() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(address).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    let state = Arc::new(AuditState {
        token: "token".to_string(),
        sandbox_id: "sandbox".to_string(),
        run_id: "run".to_string(),
        callback: |_| std::future::ready(Decision::Allow),
        callback_timeout: Duration::from_secs(1),
        requests: Mutex::new(AuditRequestCache::default()),
    });
    let task = tokio::spawn(AuditServer::handle_with_timeouts(
        server,
        state,
        Duration::from_secs(1),
        Duration::from_millis(20),
    ));
    let event = AuditEventRequest::File {
        trace_id: "trace".to_string(),
        process: ProcessContext {
            pid: 1,
            ppid: 0,
            executable: "/bin/tool".to_string(),
        },
        operation: FileOperation::Open,
        file: FileContext {
            path: "/tmp/file".to_string(),
            mode: FileOpenMode {
                access: FileAccessMode::Read,
                create: false,
                truncate: false,
                append: false,
                exclusive: false,
            },
        },
    };
    client
        .write_all(&encode_request("token", event).unwrap())
        .await
        .unwrap();
    let mut prefix = [0_u8; 4];
    client.read_exact(&mut prefix).await.unwrap();
    let mut response = vec![0_u8; frame_length(prefix).unwrap()];
    client.read_exact(&mut response).await.unwrap();

    let error = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("connection timed out"));
}

#[tokio::test]
async fn audit_server_keeps_an_authenticated_control_stream() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(address).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    let state = Arc::new(AuditState {
        token: "token".to_string(),
        sandbox_id: "sandbox".to_string(),
        run_id: "run".to_string(),
        callback: |_| std::future::ready(Decision::Allow),
        callback_timeout: Duration::from_secs(1),
        requests: Mutex::new(AuditRequestCache::default()),
    });
    let task = tokio::spawn(AuditServer::handle_with_timeouts(
        server,
        state,
        Duration::from_secs(1),
        Duration::from_millis(20),
    ));
    client
        .write_all(&encode_ping_request("token").unwrap())
        .await
        .unwrap();
    let mut prefix = [0_u8; 4];
    client.read_exact(&mut prefix).await.unwrap();
    let mut response = vec![0_u8; frame_length(prefix).unwrap()];
    client.read_exact(&mut response).await.unwrap();
    assert_eq!(decode_response(&response).unwrap(), AuditResponse::Accepted);

    tokio::time::sleep(Duration::from_millis(50)).await;
    client
        .write_all(
            &encode_request(
                "token",
                AuditEventRequest::File {
                    trace_id: "trace".to_string(),
                    process: ProcessContext {
                        pid: 1,
                        ppid: 0,
                        executable: "/bin/tool".to_string(),
                    },
                    operation: FileOperation::Open,
                    file: FileContext {
                        path: "/tmp/file".to_string(),
                        mode: FileOpenMode {
                            access: FileAccessMode::Read,
                            create: false,
                            truncate: false,
                            append: false,
                            exclusive: false,
                        },
                    },
                },
            )
            .unwrap(),
        )
        .await
        .unwrap();
    client.read_exact(&mut prefix).await.unwrap();
    let mut response = vec![0_u8; frame_length(prefix).unwrap()];
    client.read_exact(&mut response).await.unwrap();
    assert_eq!(decode_response(&response).unwrap(), AuditResponse::Accepted);

    drop(client);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn duplicate_audit_request_ids_publish_one_logical_event() {
    let published = Arc::new(AtomicUsize::new(0));
    let callback_count = Arc::clone(&published);
    let state = Arc::new(AuditState {
        token: "token".to_string(),
        sandbox_id: "sandbox".to_string(),
        run_id: "run".to_string(),
        callback: move |_| {
            callback_count.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Decision::Allow)
        },
        callback_timeout: Duration::from_secs(1),
        requests: Mutex::new(AuditRequestCache::default()),
    });
    let event = AuditEventRequest::File {
        trace_id: "trace".to_string(),
        process: ProcessContext {
            pid: 1,
            ppid: 0,
            executable: "/bin/tool".to_string(),
        },
        operation: FileOperation::Open,
        file: FileContext {
            path: "/tmp/file".to_string(),
            mode: FileOpenMode {
                access: FileAccessMode::Read,
                create: false,
                truncate: false,
                append: false,
                exclusive: false,
            },
        },
    };

    let (first, replay) = tokio::join!(
        state.publish_once("request".to_string(), event.clone()),
        state.publish_once("request".to_string(), event),
    );

    assert_eq!(first, AuditResponse::Accepted);
    assert_eq!(replay, AuditResponse::Accepted);
    assert_eq!(published.load(Ordering::Relaxed), 1);

    let different = AuditEventRequest::File {
        trace_id: "different".to_string(),
        process: ProcessContext {
            pid: 1,
            ppid: 0,
            executable: "/bin/tool".to_string(),
        },
        operation: FileOperation::Open,
        file: FileContext {
            path: "/tmp/file".to_string(),
            mode: FileOpenMode {
                access: FileAccessMode::Read,
                create: false,
                truncate: false,
                append: false,
                exclusive: false,
            },
        },
    };
    assert!(matches!(
        state.publish_once("request".to_string(), different).await,
        AuditResponse::Error {
            errno: libc::EPROTO,
            ..
        }
    ));
    assert_eq!(published.load(Ordering::Relaxed), 1);
}
