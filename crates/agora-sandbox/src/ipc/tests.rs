#[cfg(target_os = "macos")]
use super::{InheritedControlLock, InheritedControlStream, configure_no_sigpipe};
use super::{MAX_FRAME_SIZE, receive, receive_with_descriptors, send, send_with_descriptors};
use crate::nfs::protocol::{PROTOCOL_VERSION, RequestId, Response, ResponseEnvelope};
use serde::Serialize;
use std::io::{Read, Write};
use std::mem::zeroed;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
#[cfg(target_os = "macos")]
use std::sync::Arc;

#[test]
fn framed_transport_round_trips_without_a_descriptor() {
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    let response = ResponseEnvelope {
        version: PROTOCOL_VERSION,
        request_id: request_id(),
        response: Response::Success,
    };

    send(&mut sender, &response, None).unwrap();
    let (decoded, descriptor) = receive::<ResponseEnvelope>(&mut receiver).unwrap();

    assert_eq!(decoded, response);
    assert!(descriptor.is_none());
}

#[cfg(target_os = "macos")]
#[test]
fn framed_transport_disables_sigpipe_without_a_descriptor() {
    let (mut sender, _receiver) = UnixStream::pair().unwrap();
    let response = ResponseEnvelope {
        version: PROTOCOL_VERSION,
        request_id: request_id(),
        response: Response::Success,
    };

    send(&mut sender, &response, None).unwrap();

    let mut enabled = 0;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    assert_eq!(
        unsafe {
            libc::getsockopt(
                sender.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&mut enabled as *mut libc::c_int).cast(),
                &mut length,
            )
        },
        0
    );
    assert_eq!(enabled, 1);
}

#[cfg(target_os = "macos")]
#[test]
fn inherited_control_streams_are_inheritable_and_serialize_threads() {
    let lock = InheritedControlLock::anonymous().unwrap();
    let lock_flags = unsafe { libc::fcntl(lock.descriptor(), libc::F_GETFD) };
    assert_eq!(lock_flags & libc::FD_CLOEXEC, 0);
    let (stream, mut peer) = UnixStream::pair().unwrap();
    let shared = InheritedControlStream::new(stream, Arc::clone(&lock), 0).unwrap();
    let stream_flags = unsafe { libc::fcntl(shared.descriptor(), libc::F_GETFD) };
    assert_eq!(stream_flags & libc::FD_CLOEXEC, 0);
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let mut byte = [0_u8; 1];
            peer.read_exact(&mut byte).unwrap();
            peer.write_all(&byte).unwrap();
        }
    });
    let clients = (*b"ab").map(|byte| {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            shared
                .transact(|stream| {
                    stream.write_all(&[byte])?;
                    let mut response = [0_u8; 1];
                    stream.read_exact(&mut response)?;
                    Ok::<_, std::io::Error>(response[0])
                })
                .unwrap()
                .unwrap()
        })
    });
    let mut responses = clients.map(|client| client.join().unwrap());
    responses.sort_unstable();
    assert_eq!(responses, [b'a', b'b']);
    server.join().unwrap();

    unsafe { shared.reset_after_fork() };
    let duplicate = unsafe { libc::fcntl(lock.descriptor(), libc::F_DUPFD_CLOEXEC, 0) };
    assert!(duplicate >= 0);
    let adopted = unsafe { InheritedControlLock::from_raw_descriptor(duplicate) }.unwrap();
    let adopted_flags = unsafe { libc::fcntl(adopted.descriptor(), libc::F_GETFD) };
    assert_eq!(adopted_flags & libc::FD_CLOEXEC, 0);
}

#[cfg(target_os = "macos")]
#[test]
fn inherited_control_transaction_blocks_catchable_signals_while_locked() {
    let signal = crate::platform::hook::tests::SignalMaskProbe::unblocked(libc::SIGUSR2);
    let lock = InheritedControlLock::anonymous().unwrap();
    let (stream, _peer) = UnixStream::pair().unwrap();
    let shared = InheritedControlStream::new(stream, lock, 0).unwrap();

    shared.transact(|_| assert!(signal.is_blocked())).unwrap();
    assert!(!signal.is_blocked());
}

#[test]
fn framed_transport_passes_one_close_on_exec_descriptor() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("payload");
    std::fs::write(&path, b"remote bytes").unwrap();
    let file = std::fs::File::open(path).unwrap();
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    let response = ResponseEnvelope {
        version: PROTOCOL_VERSION,
        request_id: request_id(),
        response: Response::Success,
    };

    send(&mut sender, &response, Some(file.as_raw_fd())).unwrap();
    let (_, descriptor) = receive::<ResponseEnvelope>(&mut receiver).unwrap();
    let descriptor = descriptor.unwrap();
    let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) };
    assert_ne!(flags & libc::FD_CLOEXEC, 0);
    let mut received = unsafe { std::fs::File::from_raw_fd(descriptor.as_raw_fd()) };
    std::mem::forget(descriptor);
    let mut contents = String::new();
    received.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "remote bytes");
}

#[test]
fn framed_transport_passes_multiple_close_on_exec_descriptors() {
    let first = std::fs::File::open("/dev/null").unwrap();
    let second = std::fs::File::open("/dev/null").unwrap();
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    let response = ResponseEnvelope {
        version: PROTOCOL_VERSION,
        request_id: request_id(),
        response: Response::Success,
    };

    send_with_descriptors(
        &mut sender,
        &response,
        &[first.as_raw_fd(), second.as_raw_fd()],
    )
    .unwrap();
    let (decoded, descriptors) =
        receive_with_descriptors::<ResponseEnvelope>(&mut receiver).unwrap();

    assert_eq!(decoded, response);
    assert_eq!(descriptors.len(), 2);
    for descriptor in descriptors {
        let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }
}

#[test]
fn framed_transport_rejects_oversized_payloads_before_allocation() {
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    sender.write_all(&[0]).unwrap();
    sender
        .write_all(&u32::try_from(MAX_FRAME_SIZE + 1).unwrap().to_be_bytes())
        .unwrap();

    let error = receive::<ResponseEnvelope>(&mut receiver).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn framed_transport_rejects_oversized_outbound_payloads() {
    let (mut sender, _receiver) = UnixStream::pair().unwrap();

    let error = send(&mut sender, &vec![0_u8; MAX_FRAME_SIZE + 1], None).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn framed_transport_reports_serialization_failures_as_invalid_data() {
    struct InvalidMessage;

    impl Serialize for InvalidMessage {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom(
                "intentional serialization failure",
            ))
        }
    }

    let (mut sender, _receiver) = UnixStream::pair().unwrap();
    let error = send(&mut sender, &InvalidMessage, None).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn framed_transport_reports_a_closed_peer() {
    let (sender, mut receiver) = UnixStream::pair().unwrap();
    drop(sender);

    let error = receive::<ResponseEnvelope>(&mut receiver).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[test]
fn framed_transport_rejects_an_invalid_marker() {
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    sender.write_all(&[1]).unwrap();

    let error = receive::<ResponseEnvelope>(&mut receiver).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn framed_transport_reports_truncated_frames() {
    for frame in [&[0_u8, 0, 0][..], &[0_u8, 0, 0, 0, 2, b'{'][..]] {
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        sender.write_all(frame).unwrap();
        drop(sender);

        let error = receive::<ResponseEnvelope>(&mut receiver).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }
}

#[test]
fn framed_transport_rejects_invalid_json() {
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    sender.write_all(&[0]).unwrap();
    sender.write_all(&1_u32.to_be_bytes()).unwrap();
    sender.write_all(b"{").unwrap();

    let error = receive::<ResponseEnvelope>(&mut receiver).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn framed_transport_rejects_an_invalid_descriptor() {
    let (mut sender, _receiver) = UnixStream::pair().unwrap();
    let response = ResponseEnvelope {
        version: PROTOCOL_VERSION,
        request_id: request_id(),
        response: Response::Success,
    };

    let error = send(&mut sender, &response, Some(-1)).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[cfg(target_os = "macos")]
#[test]
fn configuring_sigpipe_on_an_invalid_descriptor_reports_ebadf() {
    let error = configure_no_sigpipe(-1).unwrap_err();

    assert_eq!(error.raw_os_error(), Some(libc::EBADF));
}

#[test]
fn single_descriptor_wrapper_rejects_multiple_descriptors() {
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    let first = std::fs::File::open("/dev/null").unwrap();
    let second = std::fs::File::open("/dev/null").unwrap();
    send_descriptor_marker(&sender, &[first.as_raw_fd(), second.as_raw_fd()]);
    let payload = serde_json::to_vec(&ResponseEnvelope {
        version: PROTOCOL_VERSION,
        request_id: request_id(),
        response: Response::Success,
    })
    .unwrap();
    sender
        .write_all(&(payload.len() as u32).to_be_bytes())
        .unwrap();
    sender.write_all(&payload).unwrap();

    let error = receive::<ResponseEnvelope>(&mut receiver).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

fn request_id() -> RequestId {
    RequestId::new("0123456789abcdef0123456789abcdef").unwrap()
}

fn send_descriptor_marker(stream: &UnixStream, descriptors: &[RawFd]) {
    let mut marker = [0_u8];
    let mut iov = libc::iovec {
        iov_base: marker.as_mut_ptr().cast(),
        iov_len: marker.len(),
    };
    let descriptor_bytes = std::mem::size_of_val(descriptors) as u32;
    let control_length = unsafe { libc::CMSG_SPACE(descriptor_bytes) as usize };
    let mut control = vec![0_u8; control_length];
    let mut header = unsafe { zeroed::<libc::msghdr>() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = control.len() as _;
    unsafe {
        let message = libc::CMSG_FIRSTHDR(&header);
        assert!(!message.is_null());
        (*message).cmsg_level = libc::SOL_SOCKET;
        (*message).cmsg_type = libc::SCM_RIGHTS;
        (*message).cmsg_len = libc::CMSG_LEN(descriptor_bytes) as _;
        std::ptr::copy_nonoverlapping(
            descriptors.as_ptr(),
            libc::CMSG_DATA(message).cast::<RawFd>(),
            descriptors.len(),
        );
        header.msg_controllen = (*message).cmsg_len as _;
        assert_eq!(libc::sendmsg(stream.as_raw_fd(), &header, 0), 1);
    }
}
