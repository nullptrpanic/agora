use super::*;
use crate::filesystem::broker::LocalOpenState;
use crate::filesystem::broker::protocol::{RequestEnvelope, ResponseEnvelope};
#[cfg(target_os = "macos")]
use crate::ipc::{InheritedControlLock, InheritedControlStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
#[cfg(target_os = "macos")]
use std::os::unix::net::UnixStream;
#[cfg(target_os = "macos")]
use std::sync::atomic::Ordering;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

fn serve(
    listener: UnixListener,
    response: impl FnOnce(RequestEnvelope) -> (ResponseEnvelope, Option<std::fs::File>) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let (request, descriptor) = ipc::receive::<RequestEnvelope>(&mut stream).unwrap();
        assert!(descriptor.is_none());
        let (response, descriptor) = response(request);
        ipc::send(
            &mut stream,
            &response,
            descriptor.as_ref().map(AsRawFd::as_raw_fd),
        )
        .unwrap();
    })
}

#[test]
fn client_retries_idempotent_requests_and_preserves_the_request_id() {
    let runtime = tempfile::tempdir().unwrap();
    let socket = runtime.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut first_stream, _) = listener.accept().unwrap();
        let (first, descriptor) = ipc::receive::<RequestEnvelope>(&mut first_stream).unwrap();
        assert!(descriptor.is_none());
        drop(first_stream);

        let (mut second_stream, _) = listener.accept().unwrap();
        let (second, descriptor) = ipc::receive::<RequestEnvelope>(&mut second_stream).unwrap();
        assert!(descriptor.is_none());
        assert_eq!(first.request_id, second.request_id);
        assert_eq!(first.request, second.request);
        ipc::send(
            &mut second_stream,
            &ResponseEnvelope {
                version: PROTOCOL_VERSION,
                request_id: second.request_id,
                response: Response::Success,
            },
            None,
        )
        .unwrap();
    });
    let client = LocalClient::new(&socket, "token");

    client
        .sync("handle", vec![ByteRange::new(1, 3).unwrap()], false)
        .unwrap();
    server.join().unwrap();
}

#[test]
fn client_retries_busy_mutations_with_a_new_request_id() {
    let runtime = tempfile::tempdir().unwrap();
    let socket = runtime.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut requests = Vec::new();
        while requests.len() < 2 && std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let (request, descriptor) =
                        ipc::receive::<RequestEnvelope>(&mut stream).unwrap();
                    assert!(descriptor.is_none());
                    let response = if requests.is_empty() {
                        Response::Error {
                            errno: libc::EAGAIN,
                            message: "local plaintext file is busy".to_string(),
                        }
                    } else {
                        Response::Success
                    };
                    ipc::send(
                        &mut stream,
                        &ResponseEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: request.request_id.clone(),
                            response,
                        },
                        None,
                    )
                    .unwrap();
                    requests.push(request);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("failed to accept local broker request: {error}"),
            }
        }
        requests
    });
    let client = LocalClient::new(&socket, "token");

    let result = client.begin_write("handle", ByteRange::new(1, 3).unwrap());
    let requests = server.join().unwrap();

    assert!(result.is_ok(), "busy mutation was not retried");
    assert_eq!(requests.len(), 2);
    assert_ne!(requests[0].request_id, requests[1].request_id);
    assert_eq!(requests[0].request, requests[1].request);
}

#[test]
fn client_rejects_mismatched_and_descriptor_bearing_responses() {
    for descriptor_response in [false, true] {
        let runtime = tempfile::tempdir().unwrap();
        let socket = runtime.path().join("broker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = serve(listener, move |request| {
            let descriptor = descriptor_response.then(|| tempfile::tempfile().unwrap());
            (
                ResponseEnvelope {
                    version: if descriptor_response {
                        PROTOCOL_VERSION
                    } else {
                        PROTOCOL_VERSION + 1
                    },
                    request_id: request.request_id,
                    response: Response::Success,
                },
                descriptor,
            )
        });
        let client = LocalClient::new(&socket, "token");

        let error = client.close("handle", Vec::new()).unwrap_err();

        assert_eq!(error.errno(), libc::EPROTO);
        server.join().unwrap();
    }
}

#[test]
fn client_rejects_unexpected_success_shapes_and_maps_broker_errors() {
    let runtime = tempfile::tempdir().unwrap();
    let socket = runtime.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = serve(listener, |request| {
        (
            ResponseEnvelope {
                version: PROTOCOL_VERSION,
                request_id: request.request_id,
                response: Response::Open {
                    handle: "unexpected".to_string(),
                    device: 1,
                    inode: 2,
                    links: 1,
                    lazy: false,
                },
            },
            None,
        )
    });
    let client = LocalClient::new(&socket, "token");
    let error = client.close("handle", Vec::new()).unwrap_err();
    assert_eq!(error.errno(), libc::EPROTO);
    server.join().unwrap();

    let runtime = tempfile::tempdir().unwrap();
    let socket = runtime.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = serve(listener, |request| {
        (
            ResponseEnvelope {
                version: PROTOCOL_VERSION,
                request_id: request.request_id,
                response: Response::Error {
                    errno: libc::ENOSPC,
                    message: "disk full".to_string(),
                },
            },
            None,
        )
    });
    let client = LocalClient::new(&socket, "token");
    let error = client.close("handle", Vec::new()).unwrap_err();
    assert_eq!(error.errno(), libc::ENOSPC);
    assert_eq!(error.to_string(), "disk full");
    server.join().unwrap();
}

#[test]
fn client_open_validates_its_response_and_missing_sockets_keep_errno() {
    let runtime = tempfile::tempdir().unwrap();
    let socket = runtime.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let (request, descriptor) = ipc::receive::<RequestEnvelope>(&mut stream).unwrap();
        assert!(descriptor.is_none());
        ipc::send(
            &mut stream,
            &ResponseEnvelope {
                version: PROTOCOL_VERSION,
                request_id: request.request_id,
                response: Response::Success,
            },
            None,
        )
        .unwrap();
    });
    let client = LocalClient::new(&socket, "token");
    let error = match client.open(Path::new("/tmp/backing"), libc::O_RDWR) {
        Ok(_) => panic!("unexpected valid open response"),
        Err(error) => error,
    };
    assert_eq!(error.errno(), libc::EPROTO);
    server.join().unwrap();

    let missing = LocalClient::new(runtime.path().join("missing.sock"), "token");
    let error = missing.close("handle", Vec::new()).unwrap_err();
    assert_eq!(error.errno(), libc::ENOENT);
    assert!(error.to_string().contains("connect"));
    assert!(format!("{error:?}").contains("LocalClientError"));
}

#[test]
fn client_open_retries_and_claims_a_response_lost_after_execution() {
    let runtime = tempfile::tempdir().unwrap();
    let socket = runtime.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut first_stream, _) = listener.accept().unwrap();
        let (first, descriptor) = ipc::receive::<RequestEnvelope>(&mut first_stream).unwrap();
        assert!(descriptor.is_none());
        drop(first_stream);

        let (mut second_stream, _) = listener.accept().unwrap();
        let (second, descriptor) = ipc::receive::<RequestEnvelope>(&mut second_stream).unwrap();
        assert!(descriptor.is_none());
        assert_eq!(first.request_id, second.request_id);
        assert_eq!(first.request, second.request);
        let content = tempfile::tempfile().unwrap();
        let state = LocalOpenState::create(libc::O_RDWR).unwrap();
        let lock = tempfile::tempfile().unwrap();
        ipc::send_with_descriptors(
            &mut second_stream,
            &ResponseEnvelope {
                version: PROTOCOL_VERSION,
                request_id: second.request_id,
                response: Response::Open {
                    handle: "opened-handle".to_string(),
                    device: 1,
                    inode: 2,
                    links: 1,
                    lazy: true,
                },
            },
            &[content.as_raw_fd(), state.as_raw_fd(), lock.as_raw_fd()],
        )
        .unwrap();

        let (mut claim_stream, _) = listener.accept().unwrap();
        let (claim, descriptor) = ipc::receive::<RequestEnvelope>(&mut claim_stream).unwrap();
        assert!(descriptor.is_none());
        assert_eq!(
            claim.request,
            Request::Claim {
                request_id: first.request_id,
            }
        );
        ipc::send(
            &mut claim_stream,
            &ResponseEnvelope {
                version: PROTOCOL_VERSION,
                request_id: claim.request_id,
                response: Response::Success,
            },
            None,
        )
        .unwrap();
    });
    let client = LocalClient::new(&socket, "token");
    let opened = client
        .open(Path::new("/tmp/backing"), libc::O_RDWR)
        .unwrap();

    assert_eq!(opened.handle, "opened-handle");
    assert_eq!(opened.identity.device, 1);
    assert_eq!(opened.identity.inode, 2);
    assert!(opened.lazy);
    server.join().unwrap();
}

#[test]
fn client_open_aborts_incomplete_invalid_and_unclaimable_replies() {
    for failure in 0..3 {
        let runtime = tempfile::tempdir().unwrap();
        let socket = runtime.path().join("broker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (open, descriptor) = ipc::receive::<RequestEnvelope>(&mut stream).unwrap();
            assert!(descriptor.is_none());
            let content = tempfile::tempfile().unwrap();
            let state = if failure == 1 {
                tempfile::tempfile().unwrap()
            } else {
                LocalOpenState::create(libc::O_RDWR)
                    .unwrap()
                    .try_clone_file()
                    .unwrap()
            };
            let lock = tempfile::tempfile().unwrap();
            let response = ResponseEnvelope {
                version: PROTOCOL_VERSION,
                request_id: open.request_id,
                response: Response::Open {
                    handle: "opened-handle".to_string(),
                    device: 1,
                    inode: 2,
                    links: 1,
                    lazy: false,
                },
            };
            if failure == 0 {
                ipc::send(&mut stream, &response, None).unwrap();
            } else {
                ipc::send_with_descriptors(
                    &mut stream,
                    &response,
                    &[content.as_raw_fd(), state.as_raw_fd(), lock.as_raw_fd()],
                )
                .unwrap();
            }

            if failure == 2 {
                let (mut claim_stream, _) = listener.accept().unwrap();
                let (claim, descriptor) =
                    ipc::receive::<RequestEnvelope>(&mut claim_stream).unwrap();
                assert!(descriptor.is_none());
                assert!(matches!(claim.request, Request::Claim { .. }));
                ipc::send(
                    &mut claim_stream,
                    &ResponseEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: claim.request_id,
                        response: Response::Error {
                            errno: libc::EBUSY,
                            message: "claim failed".to_string(),
                        },
                    },
                    None,
                )
                .unwrap();
            }

            let (mut abort_stream, _) = listener.accept().unwrap();
            let (abort, descriptor) = ipc::receive::<RequestEnvelope>(&mut abort_stream).unwrap();
            assert!(descriptor.is_none());
            assert_eq!(
                abort.request,
                Request::Abort {
                    handle: "opened-handle".to_string(),
                }
            );
            ipc::send(
                &mut abort_stream,
                &ResponseEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: abort.request_id,
                    response: Response::Success,
                },
                None,
            )
            .unwrap();
        });
        let client = LocalClient::new(&socket, "token");

        let error = match client.open(Path::new("/tmp/backing"), libc::O_RDWR) {
            Ok(_) => panic!("unexpected valid open response"),
            Err(error) => error,
        };
        assert_eq!(
            error.errno(),
            match failure {
                0 => libc::EPROTO,
                1 => libc::EIO,
                _ => libc::EBUSY,
            }
        );
        server.join().unwrap();
    }
}

#[test]
fn client_append_rejects_descriptors_and_non_offset_replies() {
    for descriptor_response in [false, true] {
        let runtime = tempfile::tempdir().unwrap();
        let socket = runtime.path().join("broker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = serve(listener, move |request| {
            (
                ResponseEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: request.request_id,
                    response: if descriptor_response {
                        Response::Offset { offset: 3 }
                    } else {
                        Response::Success
                    },
                },
                descriptor_response.then(|| tempfile::tempfile().unwrap()),
            )
        });
        let client = LocalClient::new(&socket, "token");

        let error = match client.begin_append("handle") {
            Ok(_) => panic!("unexpected valid append response"),
            Err(error) => error,
        };
        assert_eq!(error.errno(), libc::EPROTO);
        server.join().unwrap();
    }
}

#[cfg(target_os = "macos")]
#[test]
fn shared_ping_rejects_descriptors_and_non_success_replies() {
    for descriptor_response in [false, true] {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let (request, descriptor) =
                ipc::receive::<RequestEnvelope>(&mut server_stream).unwrap();
            assert!(descriptor.is_none());
            let response = ResponseEnvelope {
                version: PROTOCOL_VERSION,
                request_id: request.request_id,
                response: if descriptor_response {
                    Response::Success
                } else {
                    Response::Offset { offset: 0 }
                },
            };
            let descriptor = descriptor_response.then(|| tempfile::tempfile().unwrap());
            ipc::send(
                &mut server_stream,
                &response,
                descriptor.as_ref().map(AsRawFd::as_raw_fd),
            )
            .unwrap();
        });
        let shared = InheritedControlStream::new(
            client_stream,
            InheritedControlLock::anonymous().unwrap(),
            0,
        )
        .unwrap();
        let client = LocalClient::with_shared("/missing", "token", shared);

        let error = client.ping_shared().unwrap_err();
        assert_eq!(error.errno(), libc::EPROTO);
        server.join().unwrap();
    }
}

#[test]
fn retaining_no_handles_is_a_noop() {
    let client = LocalClient::new("/missing", "token");
    client.retain(Vec::new()).unwrap();
    client.release_retained(Vec::new()).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn inherited_transport_failure_falls_back_to_a_fresh_connection_after_fork() {
    let runtime = tempfile::tempdir().unwrap();
    let socket = runtime.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let (request, descriptor) =
                        ipc::receive::<RequestEnvelope>(&mut stream).unwrap();
                    assert!(descriptor.is_none());
                    ipc::send(
                        &mut stream,
                        &ResponseEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: request.request_id,
                            response: Response::Success,
                        },
                        None,
                    )
                    .unwrap();
                    return true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("failed to accept local broker request: {error}"),
            }
        }
    });
    let (inherited, peer) = UnixStream::pair().unwrap();
    drop(peer);
    let shared =
        InheritedControlStream::new(inherited, InheritedControlLock::anonymous().unwrap(), 0)
            .unwrap();
    let client = LocalClient::with_shared(&socket, "token", shared);
    client.observed_pid.store(0, Ordering::Release);

    let result = client.close("handle", Vec::new());
    let used_fresh_connection = server.join().unwrap();

    assert!(result.is_ok(), "fallback failed: {result:?}");
    assert!(used_fresh_connection);
}
