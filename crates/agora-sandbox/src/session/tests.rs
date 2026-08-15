use super::protocol::{
    ClientMessage, ServerMessage, WireOsString, WirePreparedLaunch, read_frame, write_frame,
};
use crate::runner::{PreparedLaunch, ProtectedEnvironment};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

#[test]
fn wire_os_string_round_trips_non_utf8_bytes() {
    let original = OsString::from_vec(vec![b'a', 0xff, b'z']);

    let encoded = serde_json::to_vec(&WireOsString::from(original.clone())).unwrap();
    let decoded: WireOsString = serde_json::from_slice(&encoded).unwrap();

    assert_eq!(decoded.into_os_string().as_bytes(), original.as_bytes());
}

#[tokio::test]
async fn session_frame_round_trips_a_typed_message() {
    let (mut writer, mut reader) = tokio::io::duplex(4096);
    let message = ClientMessage::Join {
        protocol: 1,
        build: "build-a".to_string(),
        config: "config-a".to_string(),
    };

    write_frame(&mut writer, &message).await.unwrap();
    let decoded: ClientMessage = read_frame(&mut reader).await.unwrap();

    assert_eq!(decoded, message);
}

#[tokio::test]
async fn session_frame_rejects_a_payload_over_the_control_limit() {
    let (mut writer, _reader) = tokio::io::duplex(16);
    let oversized = ClientMessage::Join {
        protocol: 1,
        build: "b".repeat(crate::ipc::MAX_FRAME_SIZE),
        config: String::new(),
    };

    let error = write_frame(&mut writer, &oversized).await.unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[tokio::test]
async fn session_client_times_out_when_the_daemon_stops_after_join() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workspace");
    std::fs::create_dir(&workdir).unwrap();
    let paths = super::startup::SessionPaths::resolve(&workdir).unwrap();
    let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _: ClientMessage = read_frame(&mut stream).await.unwrap();
        std::future::pending::<()>().await;
    });

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        super::client::run(
            &root.path().join("sandbox.json"),
            &workdir,
            "config-a",
            crate::runner::SandboxCommand::new("/usr/bin/true"),
        ),
    )
    .await;
    server.abort();

    let error = result
        .expect("the session client did not enforce its control deadline")
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("sandbox session join response timed out"),
        "unexpected error: {error:#}"
    );
}

#[tokio::test]
async fn session_client_retries_an_idle_daemon_retirement() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workspace");
    std::fs::create_dir(&workdir).unwrap();
    let paths = super::startup::SessionPaths::resolve(&workdir).unwrap();
    let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
    let server = tokio::spawn(async move {
        let (mut stale, _) = listener.accept().await.unwrap();
        let _: ClientMessage = read_frame(&mut stale).await.unwrap();
        write_frame(
            &mut stale,
            &ServerMessage::Retiring {
                message: "sandbox session build mismatch".to_string(),
            },
        )
        .await
        .unwrap();
        drop(stale);

        let (mut current, _) = listener.accept().await.unwrap();
        let _: ClientMessage = read_frame(&mut current).await.unwrap();
        write_frame(
            &mut current,
            &ServerMessage::Joined {
                sandbox_id: "sandbox-a".to_string(),
                run_id: "run-a".to_string(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<_, ClientMessage>(&mut current).await.unwrap(),
            ClientMessage::Prepare { .. }
        ));
        let launch = PreparedLaunch::new(
            "/usr/bin/true".into(),
            Vec::new(),
            ProtectedEnvironment::from_parts(BTreeMap::new(), Vec::new()),
            "launch-a".to_string(),
        );
        write_frame(
            &mut current,
            &ServerMessage::Prepared {
                launch: WirePreparedLaunch::from(&launch),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<_, ClientMessage>(&mut current).await.unwrap(),
            ClientMessage::Finished { .. }
        ));
        write_frame(&mut current, &ServerMessage::Released)
            .await
            .unwrap();
    });

    let outcome = super::client::run(
        &root.path().join("sandbox.json"),
        &workdir,
        "config-a",
        crate::runner::SandboxCommand::new("/usr/bin/true"),
    )
    .await
    .unwrap();

    assert!(outcome.status().success());
    server.await.unwrap();
}

#[test]
fn session_paths_are_stable_short_and_owner_only() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workspace");
    std::fs::create_dir(&workdir).unwrap();

    let first = super::startup::SessionPaths::resolve(&workdir).unwrap();
    let second = super::startup::SessionPaths::resolve(&workdir.join(".")).unwrap();

    assert_eq!(first.socket(), second.socket());
    assert!(first.socket().as_os_str().as_bytes().len() < 100);
    assert_eq!(
        first
            .socket()
            .parent()
            .unwrap()
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        first.startup_lock(),
        workdir
            .canonicalize()
            .unwrap()
            .join("runtime/session-start.lock")
    );
}

#[test]
fn session_startup_lock_excludes_a_second_daemon_candidate() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workspace");
    std::fs::create_dir(&workdir).unwrap();
    let paths = super::startup::SessionPaths::resolve(&workdir).unwrap();

    let first = super::startup::StartupLock::try_acquire(paths.startup_lock())
        .unwrap()
        .expect("first candidate acquires the startup lock");
    assert!(
        super::startup::StartupLock::try_acquire(paths.startup_lock())
            .unwrap()
            .is_none()
    );
    drop(first);
    assert!(
        super::startup::StartupLock::try_acquire(paths.startup_lock())
            .unwrap()
            .is_some()
    );
}

#[test]
fn inherited_daemon_descriptors_are_restored_close_on_exec() {
    let descriptor = tempfile::tempfile().unwrap().into_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
        0
    );

    let descriptor = super::startup::inherited_descriptor(descriptor, "test").unwrap();
    let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) };
    assert_ne!(flags & libc::FD_CLOEXEC, 0);
}
