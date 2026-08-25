use super::*;
use crate::filesystem::FileCipher;
use crate::filesystem::broker::{LocalClient, LocalController};
use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::Duration;

unsafe extern "C" fn interrupt_write(_signal: libc::c_int) {}

fn c_path(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap()
}

#[test]
fn untracked_blocking_write_is_interruptible_outside_the_rust_hook_guard() {
    let signal = super::super::super::tests::SignalMaskProbe::unblocked(libc::SIGUSR2);
    let mut previous = unsafe { std::mem::zeroed::<libc::sigaction>() };
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = interrupt_write as *const () as usize;
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    assert_eq!(
        unsafe { libc::sigaction(libc::SIGUSR2, &action, &mut previous) },
        0
    );
    let mut descriptors = [-1; 2];
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    let read = descriptors[0];
    let write = descriptors[1];
    let flags = unsafe { libc::fcntl(write, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(write, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let bytes = [0_u8; 4096];
    while unsafe { libc::write(write, bytes.as_ptr().cast(), bytes.len()) } >= 0 {}
    assert_eq!(
        std::io::Error::last_os_error().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(unsafe { libc::fcntl(write, libc::F_SETFL, flags) }, 0);

    let writer = unsafe { libc::pthread_self() } as usize;
    let interrupter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            unsafe { libc::pthread_kill(writer as libc::pthread_t, libc::SIGUSR2) },
            0
        );
        std::thread::sleep(Duration::from_millis(100));
        let mut buffer = [0_u8; 4096];
        unsafe { libc::read(read, buffer.as_mut_ptr().cast(), buffer.len()) };
        unsafe { libc::close(read) };
    });

    let result = unsafe { agora_sandbox_write_shim(write, bytes.as_ptr().cast(), 1) };
    let error = std::io::Error::last_os_error();
    unsafe { libc::close(write) };
    interrupter.join().unwrap();
    assert_eq!(
        unsafe { libc::sigaction(libc::SIGUSR2, &previous, std::ptr::null_mut()) },
        0
    );

    assert_eq!(result, -1);
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    assert!(!signal.is_blocked());
}

#[test]
fn high_untracked_blocking_write_is_interruptible_outside_the_rust_hook_guard() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = FilesystemHookRuntime::new(directory.path().join("fs")).unwrap();
    let signal = super::super::super::tests::SignalMaskProbe::unblocked(libc::SIGUSR2);
    let mut previous = unsafe { std::mem::zeroed::<libc::sigaction>() };
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = interrupt_write as *const () as usize;
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    assert_eq!(
        unsafe { libc::sigaction(libc::SIGUSR2, &action, &mut previous) },
        0
    );
    let mut descriptors = [-1; 2];
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    let read = descriptors[0];
    let original_write = descriptors[1];
    let write = unsafe { libc::fcntl(original_write, libc::F_DUPFD, 65_536) };
    assert!(write >= 65_536);
    assert_eq!(unsafe { libc::close(original_write) }, 0);
    let flags = unsafe { libc::fcntl(write, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(write, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let bytes = [0_u8; 4096];
    while unsafe { libc::write(write, bytes.as_ptr().cast(), bytes.len()) } >= 0 {}
    assert_eq!(
        std::io::Error::last_os_error().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(unsafe { libc::fcntl(write, libc::F_SETFL, flags) }, 0);

    let writer = unsafe { libc::pthread_self() } as usize;
    let interrupter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            unsafe { libc::pthread_kill(writer as libc::pthread_t, libc::SIGUSR2) },
            0
        );
        std::thread::sleep(Duration::from_millis(100));
        let mut buffer = [0_u8; 4096];
        unsafe { libc::read(read, buffer.as_mut_ptr().cast(), buffer.len()) };
        unsafe { libc::close(read) };
    });

    let result = with_test_runtime(&runtime, || unsafe {
        agora_sandbox_write_shim(write, bytes.as_ptr().cast(), 1)
    });
    let error = std::io::Error::last_os_error();
    unsafe { libc::close(write) };
    interrupter.join().unwrap();
    assert_eq!(
        unsafe { libc::sigaction(libc::SIGUSR2, &previous, std::ptr::null_mut()) },
        0
    );

    assert_eq!(result, -1);
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    assert!(!signal.is_blocked());
}

unsafe fn raw_pread(
    descriptor: libc::c_int,
    buffer: *mut libc::c_void,
    length: usize,
    offset: libc::off_t,
) -> libc::ssize_t {
    const DARWIN_SYS_PREAD: libc::c_int = 153;
    unsafe { libc::syscall(DARWIN_SYS_PREAD, descriptor, buffer, length, offset) as libc::ssize_t }
}

unsafe fn write_managed_file(path: &CString, contents: &[u8]) {
    let descriptor = unsafe {
        super::super::agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
            0o600,
        )
    };
    assert!(descriptor >= 0);
    assert_eq!(
        unsafe { agora_sandbox_write(descriptor, contents.as_ptr().cast(), contents.len()) },
        contents.len() as libc::ssize_t
    );
    assert_eq!(unsafe { super::super::agora_sandbox_close(descriptor) }, 0);
}

unsafe fn read_managed_file(path: &CString) -> Vec<u8> {
    let descriptor =
        unsafe { super::super::agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0) };
    assert!(descriptor >= 0);
    let mut contents = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read =
            unsafe { agora_sandbox_read(descriptor, buffer.as_mut_ptr().cast(), buffer.len()) };
        assert!(read >= 0);
        if read == 0 {
            break;
        }
        contents.extend_from_slice(&buffer[..read as usize]);
    }
    assert_eq!(unsafe { super::super::agora_sandbox_close(descriptor) }, 0);
    contents
}

unsafe fn await_submitted_aio(control: &mut libc::aiocb) -> libc::ssize_t {
    let controls = [control as *const libc::aiocb];
    while unsafe { libc::aio_error(control) } == libc::EINPROGRESS {
        assert_eq!(
            unsafe {
                libc::aio_suspend(
                    controls.as_ptr(),
                    controls.len() as libc::c_int,
                    std::ptr::null(),
                )
            },
            0
        );
    }
    unsafe { libc::aio_return(control) }
}

async fn broker_runtime(directory: &Path) -> (FilesystemHookRuntime, LocalController) {
    const KEY: &[u8] = b"broker-hook-test-key";
    const SALT: &[u8] = b"0123456789abcdef";

    let root = directory.join("workdir/fs");
    let mut runtime = FilesystemHookRuntime::new_encrypted(&root, KEY, SALT).unwrap();
    let controller = LocalController::start(
        &root,
        FileCipher::derive(KEY, SALT).unwrap(),
        &directory.join("runtime"),
    )
    .await
    .unwrap();
    runtime.local = Some(LocalClient::new(
        controller.runtime().socket(),
        controller.runtime().token(),
    ));
    (runtime, controller)
}

unsafe fn duplicate_inherited_local_descriptors(encoded: &str) -> InheritedLocalDescriptors {
    let mut inherited = serde_json::from_str::<InheritedLocalDescriptors>(encoded).unwrap();
    let mut state_descriptors = HashMap::new();
    let mut lock_descriptors = HashMap::new();
    for inherited in &mut inherited.descriptors {
        inherited.descriptor = unsafe { libc::fcntl(inherited.descriptor, libc::F_DUPFD, 0) };
        assert!(inherited.descriptor >= 0);
        inherited.state_descriptor = *state_descriptors
            .entry(inherited.state_descriptor)
            .or_insert_with(|| unsafe {
                libc::fcntl(inherited.state_descriptor, libc::F_DUPFD, 0)
            });
        inherited.lock_descriptor = *lock_descriptors
            .entry(inherited.lock_descriptor)
            .or_insert_with(|| unsafe { libc::fcntl(inherited.lock_descriptor, libc::F_DUPFD, 0) });
        assert!(inherited.state_descriptor >= 0);
        assert!(inherited.lock_descriptor >= 0);
    }
    inherited
}

fn descriptor_is_open(descriptor: libc::c_int) -> bool {
    unsafe { libc::fcntl(descriptor, libc::F_GETFD) >= 0 }
}

fn close_if_open(descriptor: libc::c_int) {
    if descriptor_is_open(descriptor) {
        unsafe { libc::close(descriptor) };
    }
}

#[test]
fn lazy_read_ranges_apply_bounded_readahead_and_conservative_sendfile_fallbacks() {
    assert_eq!(read_materialization_length(1), 16 * 1024);
    assert_eq!(read_materialization_length(8 * 1024), 32 * 1024);
    assert_eq!(read_materialization_length(128 * 1024), 256 * 1024);
    assert_eq!(read_materialization_length(512 * 1024), 512 * 1024);

    let exact_length = 32;
    assert_eq!(
        unsafe { sendfile_materialization_range(7, &exact_length, std::ptr::null()) },
        Ok(Some(LocalByteRange { start: 7, end: 39 }))
    );
    let empty_headers = unsafe { std::mem::zeroed::<libc::sf_hdtr>() };
    assert_eq!(
        unsafe { sendfile_materialization_range(7, &exact_length, &empty_headers) },
        Ok(Some(LocalByteRange { start: 7, end: 39 }))
    );
    let mut actual_headers = unsafe { std::mem::zeroed::<libc::sf_hdtr>() };
    actual_headers.hdr_cnt = 1;
    assert_eq!(
        unsafe { sendfile_materialization_range(7, &exact_length, &actual_headers) },
        Ok(None)
    );
    let through_eof = 0;
    assert_eq!(
        unsafe { sendfile_materialization_range(7, &through_eof, std::ptr::null()) },
        Ok(Some(LocalByteRange {
            start: 7,
            end: u64::MAX,
        }))
    );
}

#[test]
fn recursive_write_hooks_delegate_to_native_vectored_and_positioned_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("writes");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    let descriptor = file.as_raw_fd();
    let first = b"ab";
    let second = b"cd";
    let vectors = [
        libc::iovec {
            iov_base: first.as_ptr().cast_mut().cast(),
            iov_len: first.len(),
        },
        libc::iovec {
            iov_base: second.as_ptr().cast_mut().cast(),
            iov_len: second.len(),
        },
    ];

    unsafe {
        let _guard = FilesystemHookGuard::enter().unwrap();
        assert_eq!(
            agora_sandbox_write(descriptor, first.as_ptr().cast(), first.len()),
            first.len() as libc::ssize_t
        );
        assert_eq!(
            agora_sandbox_pwrite(descriptor, second.as_ptr().cast(), second.len(), 4),
            second.len() as libc::ssize_t
        );
        assert_eq!(
            agora_sandbox_writev(descriptor, vectors.as_ptr(), vectors.len() as libc::c_int),
            4
        );
        assert_eq!(
            agora_sandbox_pwritev(
                descriptor,
                vectors.as_ptr(),
                vectors.len() as libc::c_int,
                8,
            ),
            4
        );
        assert_eq!(libc::lseek(descriptor, 0, libc::SEEK_SET), 0);
        let mut read_left = [0_u8; 2];
        let mut read_right = [0_u8; 2];
        let reads = [
            libc::iovec {
                iov_base: read_left.as_mut_ptr().cast(),
                iov_len: read_left.len(),
            },
            libc::iovec {
                iov_base: read_right.as_mut_ptr().cast(),
                iov_len: read_right.len(),
            },
        ];
        assert_eq!(
            agora_sandbox_readv(descriptor, reads.as_ptr(), reads.len() as libc::c_int),
            4
        );
        assert_eq!((&read_left, &read_right), (b"ab", b"ab"));
        assert_eq!(
            agora_sandbox_preadv(descriptor, reads.as_ptr(), reads.len() as libc::c_int, 8),
            4
        );
        assert_eq!((&read_left, &read_right), (b"ab", b"cd"));
    }

    assert_eq!(std::fs::read(path).unwrap(), b"ababcd\0\0abcd");
}

#[test]
fn nocancel_hooks_delegate_to_matching_native_symbols_during_recursion() {
    let file = tempfile::tempfile().unwrap();
    let descriptor = file.as_raw_fd();
    let first = b"ab";
    let left = b"c";
    let right = b"d";
    let writes = [
        libc::iovec {
            iov_base: left.as_ptr().cast_mut().cast(),
            iov_len: left.len(),
        },
        libc::iovec {
            iov_base: right.as_ptr().cast_mut().cast(),
            iov_len: right.len(),
        },
    ];

    unsafe {
        let _guard = FilesystemHookGuard::enter().unwrap();
        assert_eq!(
            agora_sandbox_write_nocancel(descriptor, first.as_ptr().cast(), first.len()),
            2
        );
        assert_eq!(
            agora_sandbox_writev_nocancel(descriptor, writes.as_ptr(), writes.len() as libc::c_int),
            2
        );
        assert_eq!(
            agora_sandbox_pwrite_nocancel(descriptor, b"ef".as_ptr().cast(), 2, 4),
            2
        );
        assert_eq!(
            agora_sandbox_pwritev_nocancel(
                descriptor,
                writes.as_ptr(),
                writes.len() as libc::c_int,
                6,
            ),
            2
        );
        assert_eq!(libc::lseek(descriptor, 0, libc::SEEK_SET), 0);

        let mut sequential = [0_u8; 2];
        assert_eq!(
            agora_sandbox_read_nocancel(descriptor, sequential.as_mut_ptr().cast(), 2),
            2
        );
        assert_eq!(&sequential, b"ab");
        let mut vector_left = [0_u8; 1];
        let mut vector_right = [0_u8; 1];
        let reads = [
            libc::iovec {
                iov_base: vector_left.as_mut_ptr().cast(),
                iov_len: vector_left.len(),
            },
            libc::iovec {
                iov_base: vector_right.as_mut_ptr().cast(),
                iov_len: vector_right.len(),
            },
        ];
        assert_eq!(
            agora_sandbox_readv_nocancel(descriptor, reads.as_ptr(), reads.len() as libc::c_int),
            2
        );
        assert_eq!((&vector_left, &vector_right), (b"c", b"d"));
        assert_eq!(
            agora_sandbox_pread_nocancel(descriptor, sequential.as_mut_ptr().cast(), 2, 4),
            2
        );
        assert_eq!(&sequential, b"ef");
        assert_eq!(
            agora_sandbox_preadv_nocancel(
                descriptor,
                reads.as_ptr(),
                reads.len() as libc::c_int,
                6,
            ),
            2
        );
        assert_eq!((&vector_left, &vector_right), (b"c", b"d"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_managed_descriptors_preserve_complete_posix_io_semantics() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, controller) = broker_runtime(directory.path()).await;
    let logical = directory.path().join("logical.txt");
    let path = c_path(&logical);

    with_test_runtime(&runtime, || unsafe {
        let descriptor = super::super::agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(
            agora_sandbox_write(descriptor, b"abcdefghij".as_ptr().cast(), 10),
            10
        );
        assert_eq!(agora_sandbox_lseek(descriptor, 0, libc::SEEK_SET), 0);

        let mut sequential = [0_u8; 2];
        assert_eq!(
            agora_sandbox_read(descriptor, sequential.as_mut_ptr().cast(), 2),
            2
        );
        assert_eq!(&sequential, b"ab");

        let mut vector_left = [0_u8; 2];
        let mut vector_right = [0_u8; 2];
        let reads = [
            libc::iovec {
                iov_base: vector_left.as_mut_ptr().cast(),
                iov_len: vector_left.len(),
            },
            libc::iovec {
                iov_base: vector_right.as_mut_ptr().cast(),
                iov_len: vector_right.len(),
            },
        ];
        assert_eq!(
            agora_sandbox_readv(descriptor, reads.as_ptr(), reads.len() as libc::c_int),
            4
        );
        assert_eq!((&vector_left, &vector_right), (b"cd", b"ef"));

        assert_eq!(
            agora_sandbox_pread(descriptor, sequential.as_mut_ptr().cast(), 2, 8),
            2
        );
        assert_eq!(&sequential, b"ij");
        let mut positioned_left = [0_u8; 1];
        let mut positioned_right = [0_u8; 2];
        let positioned_reads = [
            libc::iovec {
                iov_base: positioned_left.as_mut_ptr().cast(),
                iov_len: positioned_left.len(),
            },
            libc::iovec {
                iov_base: positioned_right.as_mut_ptr().cast(),
                iov_len: positioned_right.len(),
            },
        ];
        assert_eq!(
            agora_sandbox_preadv(
                descriptor,
                positioned_reads.as_ptr(),
                positioned_reads.len() as libc::c_int,
                0,
            ),
            3
        );
        assert_eq!((&positioned_left, &positioned_right), (b"a", b"bc"));

        assert_eq!(
            agora_sandbox_pwrite(descriptor, b"XY".as_ptr().cast(), 2, 2),
            2
        );
        let write_left = b"Q";
        let write_right = b"RS";
        let writes = [
            libc::iovec {
                iov_base: write_left.as_ptr().cast_mut().cast(),
                iov_len: write_left.len(),
            },
            libc::iovec {
                iov_base: write_right.as_ptr().cast_mut().cast(),
                iov_len: write_right.len(),
            },
        ];
        assert_eq!(
            agora_sandbox_pwritev(descriptor, writes.as_ptr(), writes.len() as libc::c_int, 7),
            3
        );
        assert_eq!(agora_sandbox_lseek(descriptor, -2, libc::SEEK_END), 8);
        assert_eq!(
            agora_sandbox_read_nocancel(descriptor, sequential.as_mut_ptr().cast(), 2),
            2
        );
        assert_eq!(&sequential, b"RS");
        assert_eq!(
            agora_sandbox_write_nocancel(descriptor, b"!".as_ptr().cast(), 1),
            1
        );
        assert_eq!(
            agora_sandbox_writev_nocancel(descriptor, writes.as_ptr(), writes.len() as libc::c_int,),
            3
        );
        assert_eq!(
            agora_sandbox_pwrite_nocancel(descriptor, b"A".as_ptr().cast(), 1, 0),
            1
        );
        assert_eq!(
            agora_sandbox_pwritev_nocancel(
                descriptor,
                writes.as_ptr(),
                writes.len() as libc::c_int,
                1,
            ),
            3
        );

        assert_eq!(agora_sandbox_lseek(descriptor, 0, libc::SEEK_SET), 0);
        let mut byte = [0_u8; 1];
        assert_eq!(
            agora_sandbox_read_nocancel(descriptor, byte.as_mut_ptr().cast(), 1),
            1
        );
        assert_eq!(&byte, b"A");
        assert_eq!(
            agora_sandbox_readv_nocancel(
                descriptor,
                positioned_reads.as_ptr(),
                positioned_reads.len() as libc::c_int,
            ),
            3
        );
        assert_eq!((&positioned_left, &positioned_right), (b"Q", b"RS"));
        assert_eq!(
            agora_sandbox_pread_nocancel(descriptor, byte.as_mut_ptr().cast(), 1, 4),
            1
        );
        assert_eq!(&byte, b"e");
        assert_eq!(
            agora_sandbox_preadv_nocancel(
                descriptor,
                positioned_reads.as_ptr(),
                positioned_reads.len() as libc::c_int,
                5,
            ),
            3
        );
        assert_eq!((&positioned_left, &positioned_right), (b"f", b"gQ"));

        let native_flags = libc::fcntl(descriptor, libc::F_GETFL);
        assert!(native_flags >= 0);
        assert_eq!(
            super::super::agora_sandbox_fcntl_commit_setfl(
                descriptor,
                libc::O_APPEND | libc::O_NONBLOCK,
            ),
            0
        );
        assert_eq!(
            super::super::agora_sandbox_fcntl_setfl_argument(
                descriptor,
                libc::O_APPEND | libc::O_NONBLOCK,
            ),
            0
        );
        let logical_flags = super::super::agora_sandbox_fcntl_getfl(descriptor, native_flags);
        assert_ne!(logical_flags & libc::O_APPEND, 0);
        assert_ne!(logical_flags & libc::O_NONBLOCK, 0);
        assert_eq!(agora_sandbox_lseek(descriptor, 0, libc::SEEK_SET), 0);
        assert_eq!(agora_sandbox_write(descriptor, b"Z".as_ptr().cast(), 1), 1);
        assert_eq!(agora_sandbox_write(descriptor, b"".as_ptr().cast(), 0), 0);

        let duplicate = super::super::agora_sandbox_dup(descriptor);
        assert!(duplicate >= 0);
        assert_eq!(agora_sandbox_lseek(descriptor, 0, libc::SEEK_SET), 0);
        assert_eq!(
            agora_sandbox_read(duplicate, byte.as_mut_ptr().cast(), 1),
            1
        );
        assert_eq!(&byte, b"A");
        assert_eq!(
            agora_sandbox_read(descriptor, byte.as_mut_ptr().cast(), 1),
            1
        );
        assert_eq!(&byte, b"Q");
        assert_eq!(super::super::agora_sandbox_fsync(descriptor), 0);
        assert_eq!(super::super::agora_sandbox_close(duplicate), 0);
        assert_eq!(super::super::agora_sandbox_close(descriptor), 0);

        let reader = super::super::agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(reader >= 0);
        assert_eq!(agora_sandbox_write(reader, b"x".as_ptr().cast(), 1), -1);
        assert_eq!(*libc::__error(), libc::EBADF);
        assert_ne!(super::super::agora_sandbox_lock_descriptor(reader), reader);
        assert_eq!(super::super::agora_sandbox_close(reader), 0);

        let writer = super::super::agora_sandbox_open_with_mode(path.as_ptr(), libc::O_WRONLY, 0);
        assert!(writer >= 0);
        assert_eq!(agora_sandbox_read(writer, byte.as_mut_ptr().cast(), 1), -1);
        assert_eq!(*libc::__error(), libc::EBADF);
        assert_eq!(
            agora_sandbox_pread(writer, byte.as_mut_ptr().cast(), 1, 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::EBADF);
        let writer_read = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: byte.len(),
        };
        assert_eq!(agora_sandbox_preadv(writer, &writer_read, 1, 0), -1);
        assert_eq!(*libc::__error(), libc::EBADF);
        assert_eq!(super::super::agora_sandbox_close(writer), 0);

        let guarded_logical = directory.path().join("guarded.txt");
        let guarded_path = c_path(&guarded_logical);
        let guard = 0xa60a_5a7d_b001_u64;
        let guarded = super::super::agora_sandbox_guarded_open_with_mode(
            guarded_path.as_ptr(),
            &guard,
            (1_u32 << 0) | (1_u32 << 1),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(guarded >= 0);
        assert_eq!(
            agora_sandbox_guarded_write(guarded, &guard, b"guard".as_ptr().cast(), 5),
            5
        );
        assert_eq!(
            agora_sandbox_guarded_writev(guarded, &guard, writes.as_ptr(), -1),
            -1
        );
        assert_eq!(*libc::__error(), libc::EINVAL);
        assert_eq!(
            super::super::agora_sandbox_guarded_close(guarded, &guard),
            0
        );
    });

    controller.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_managed_synchronous_writes_preserve_flags_and_complete_durably() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, controller) = broker_runtime(directory.path()).await;
    let path = c_path(&directory.path().join("synchronous.txt"));

    with_test_runtime(&runtime, || unsafe {
        let descriptor = super::super::agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC | libc::O_SYNC | libc::O_DSYNC,
            0o600,
        );
        assert!(descriptor >= 0);
        let native = libc::fcntl(descriptor, libc::F_GETFL);
        assert!(native >= 0);
        let logical = super::super::agora_sandbox_fcntl_getfl(descriptor, native);
        assert_eq!(
            logical & (libc::O_SYNC | libc::O_DSYNC),
            libc::O_SYNC | libc::O_DSYNC
        );
        assert_eq!(
            super::super::agora_sandbox_fcntl_setfl_argument(descriptor, logical)
                & (libc::O_SYNC | libc::O_DSYNC),
            0
        );
        assert_eq!(
            agora_sandbox_pwrite(descriptor, b"durable".as_ptr().cast(), 7, 0),
            7
        );
        assert_eq!(super::super::agora_sandbox_close(descriptor), 0);
    });

    controller.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lazy_broker_reads_materialize_ranges_before_native_io() {
    let directory = tempfile::tempdir().unwrap();
    let logical = directory.path().join("lazy-read.bin");
    let path = c_path(&logical);
    let sendfile_logical = directory.path().join("lazy-sendfile.bin");
    let sendfile_path = c_path(&sendfile_logical);
    let copyfile_logical = directory.path().join("lazy-copyfile.bin");
    let copyfile_path = c_path(&copyfile_logical);
    let copyfile_destination_logical = directory.path().join("copyfile-destination.bin");
    let copyfile_destination_path = c_path(&copyfile_destination_logical);
    let aio_logical = directory.path().join("lazy-aio.bin");
    let aio_path = c_path(&aio_logical);
    let lio_logical = directory.path().join("lazy-lio.bin");
    let lio_path = c_path(&lio_logical);
    let contents = (0..2 * 1024 * 1024)
        .map(|index| (index % 251 + 1) as u8)
        .collect::<Vec<_>>();

    let (writer_runtime, writer_controller) = broker_runtime(directory.path()).await;
    with_test_runtime(&writer_runtime, || unsafe {
        write_managed_file(&path, &contents);
        write_managed_file(&sendfile_path, &contents);
        write_managed_file(&copyfile_path, &contents);
        write_managed_file(&aio_path, &contents);
        write_managed_file(&lio_path, &contents);
    });
    writer_controller.shutdown().await.unwrap();
    drop(writer_runtime);

    let (runtime, controller) = broker_runtime(directory.path()).await;
    with_test_runtime(&runtime, || unsafe {
        let reader = super::super::agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(reader >= 0);
        assert!(
            runtime
                .tracked_open(reader)
                .unwrap()
                .local_inheritance()
                .unwrap()
                .lazy
        );

        let first_offset = 512 * 1024_i64 + 17;
        let mut first = [1_u8; 32];
        assert_eq!(
            raw_pread(reader, first.as_mut_ptr().cast(), first.len(), first_offset),
            first.len() as libc::ssize_t
        );
        assert_eq!(first, [0_u8; 32]);
        assert_eq!(
            agora_sandbox_pread(reader, first.as_mut_ptr().cast(), first.len(), first_offset),
            first.len() as libc::ssize_t
        );
        assert_eq!(
            &first,
            &contents[first_offset as usize..first_offset as usize + first.len()]
        );
        let open = runtime.tracked_open(reader).unwrap();
        let cached = lock(&open.managed().state.materialized);
        assert!(cached.iter().any(|range| {
            range.start <= first_offset as u64
                && range.end >= first_offset as u64 + 16 * 1024
                && range.end < first_offset as u64 + READ_AHEAD_MAX_BYTES
        }));
        drop(cached);
        let mut readahead = [0_u8; 16];
        let readahead_offset = first_offset + 64;
        assert_eq!(
            raw_pread(
                reader,
                readahead.as_mut_ptr().cast(),
                readahead.len(),
                readahead_offset,
            ),
            readahead.len() as libc::ssize_t
        );
        assert_eq!(
            &readahead,
            &contents[readahead_offset as usize..readahead_offset as usize + readahead.len()]
        );
        let adaptive_cold_offset = first_offset + 32 * 1024;
        let mut adaptive_cold = [1_u8; 16];
        assert_eq!(
            raw_pread(
                reader,
                adaptive_cold.as_mut_ptr().cast(),
                adaptive_cold.len(),
                adaptive_cold_offset,
            ),
            adaptive_cold.len() as libc::ssize_t
        );
        assert_eq!(adaptive_cold, [0_u8; 16]);

        let sequential_offset = 768 * 1024_i64 + 23;
        assert_eq!(
            agora_sandbox_lseek(reader, sequential_offset, libc::SEEK_SET),
            sequential_offset
        );
        let mut sequential = [0_u8; 16];
        assert_eq!(
            agora_sandbox_read(reader, sequential.as_mut_ptr().cast(), sequential.len()),
            sequential.len() as libc::ssize_t
        );
        assert_eq!(
            &sequential,
            &contents[sequential_offset as usize..sequential_offset as usize + sequential.len()]
        );

        let vector_offset = 1024 * 1024_i64 + 29;
        let mut left = [0_u8; 7];
        let mut right = [0_u8; 11];
        let vectors = [
            libc::iovec {
                iov_base: left.as_mut_ptr().cast(),
                iov_len: left.len(),
            },
            libc::iovec {
                iov_base: right.as_mut_ptr().cast(),
                iov_len: right.len(),
            },
        ];
        assert_eq!(
            agora_sandbox_preadv(
                reader,
                vectors.as_ptr(),
                vectors.len() as libc::c_int,
                vector_offset
            ),
            (left.len() + right.len()) as libc::ssize_t
        );
        assert_eq!(
            [&left[..], &right[..]].concat(),
            contents[vector_offset as usize..vector_offset as usize + left.len() + right.len()]
        );

        let far_offset = 1792 * 1024_i64 + 31;
        let mut far = [1_u8; 16];
        assert_eq!(
            raw_pread(reader, far.as_mut_ptr().cast(), far.len(), far_offset),
            far.len() as libc::ssize_t
        );
        assert_eq!(far, [0_u8; 16]);

        let mapping_offset = 1792 * 1024_i64;
        let mapping = super::super::mapping::agora_sandbox_mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            reader,
            mapping_offset,
        );
        assert_ne!(mapping, libc::MAP_FAILED);
        assert_eq!(
            std::slice::from_raw_parts(mapping.cast::<u8>(), 32),
            &contents[mapping_offset as usize..mapping_offset as usize + 32]
        );
        assert_eq!(
            super::super::mapping::agora_sandbox_munmap(mapping, 4096),
            0
        );

        let far_offset = 1984 * 1024_i64 + 31;
        assert_eq!(agora_sandbox_lseek(reader, 0, libc::SEEK_DATA), 0);
        assert_eq!(
            raw_pread(reader, far.as_mut_ptr().cast(), far.len(), far_offset),
            far.len() as libc::ssize_t
        );
        assert_eq!(
            &far,
            &contents[far_offset as usize..far_offset as usize + far.len()]
        );
        assert_eq!(super::super::agora_sandbox_close(reader), 0);

        let copyfile_reader =
            super::super::agora_sandbox_open_with_mode(copyfile_path.as_ptr(), libc::O_RDONLY, 0);
        assert!(copyfile_reader >= 0);
        let destination = tempfile::tempfile().unwrap();
        assert_eq!(
            agora_sandbox_fcopyfile(
                copyfile_reader,
                destination.as_raw_fd(),
                std::ptr::null_mut(),
                libc::COPYFILE_DATA,
            ),
            0
        );
        let mut copied = [0_u8; 32];
        destination.read_exact_at(&mut copied, 0).unwrap();
        assert_eq!(&copied, &contents[..copied.len()]);

        let managed_destination = super::super::agora_sandbox_open_with_mode(
            copyfile_destination_path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(managed_destination >= 0);
        assert_eq!(
            agora_sandbox_fcopyfile(
                copyfile_reader,
                managed_destination,
                std::ptr::null_mut(),
                libc::COPYFILE_DATA,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(super::super::agora_sandbox_close(managed_destination), 0);
        assert_eq!(super::super::agora_sandbox_close(copyfile_reader), 0);

        let aio_reader =
            super::super::agora_sandbox_open_with_mode(aio_path.as_ptr(), libc::O_RDONLY, 0);
        assert!(aio_reader >= 0);
        let mut aio_contents = [0_u8; 32];
        let aio_offset = 256 * 1024_i64 + 13;
        let mut control = std::mem::zeroed::<libc::aiocb>();
        control.aio_fildes = aio_reader;
        control.aio_offset = aio_offset;
        control.aio_buf = aio_contents.as_mut_ptr().cast();
        control.aio_nbytes = aio_contents.len();
        assert_eq!(agora_sandbox_aio_read(&mut control), 0);
        let controls = [&control as *const libc::aiocb];
        while libc::aio_error(&control) == libc::EINPROGRESS {
            assert_eq!(
                libc::aio_suspend(
                    controls.as_ptr(),
                    controls.len() as libc::c_int,
                    std::ptr::null()
                ),
                0
            );
        }
        assert_eq!(
            libc::aio_return(&mut control),
            aio_contents.len() as libc::ssize_t
        );
        assert_eq!(
            &aio_contents,
            &contents[aio_offset as usize..aio_offset as usize + aio_contents.len()]
        );
        let mut aio_cold = [1_u8; 16];
        assert_eq!(
            raw_pread(
                aio_reader,
                aio_cold.as_mut_ptr().cast(),
                aio_cold.len(),
                768 * 1024,
            ),
            aio_cold.len() as libc::ssize_t
        );
        assert_eq!(aio_cold, [0_u8; 16]);
        assert_eq!(super::super::agora_sandbox_close(aio_reader), 0);

        let lio_reader =
            super::super::agora_sandbox_open_with_mode(lio_path.as_ptr(), libc::O_RDONLY, 0);
        assert!(lio_reader >= 0);
        let mut lio_contents = [0_u8; 32];
        let lio_offset = 512 * 1024_i64 + 19;
        let mut lio_control = std::mem::zeroed::<libc::aiocb>();
        lio_control.aio_fildes = lio_reader;
        lio_control.aio_offset = lio_offset;
        lio_control.aio_buf = lio_contents.as_mut_ptr().cast();
        lio_control.aio_nbytes = lio_contents.len();
        lio_control.aio_lio_opcode = libc::LIO_READ;
        let lio_controls = [&mut lio_control as *mut libc::aiocb];
        assert_eq!(
            agora_sandbox_lio_listio(
                libc::LIO_WAIT,
                lio_controls.as_ptr(),
                lio_controls.len() as libc::c_int,
                std::ptr::null_mut(),
            ),
            0
        );
        assert_eq!(
            &lio_contents,
            &contents[lio_offset as usize..lio_offset as usize + lio_contents.len()]
        );
        let mut lio_cold = [1_u8; 16];
        assert_eq!(
            raw_pread(
                lio_reader,
                lio_cold.as_mut_ptr().cast(),
                lio_cold.len(),
                1024 * 1024,
            ),
            lio_cold.len() as libc::ssize_t
        );
        assert_eq!(lio_cold, [0_u8; 16]);
        assert_eq!(super::super::agora_sandbox_close(lio_reader), 0);

        let sendfile_reader =
            super::super::agora_sandbox_open_with_mode(sendfile_path.as_ptr(), libc::O_RDONLY, 0);
        assert!(sendfile_reader >= 0);
        let mut cold = [1_u8; 32];
        assert_eq!(
            raw_pread(sendfile_reader, cold.as_mut_ptr().cast(), cold.len(), 0),
            cold.len() as libc::ssize_t
        );
        assert_eq!(cold, [0_u8; 32]);
        let mut sockets = [-1; 2];
        assert_eq!(
            libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sockets.as_mut_ptr()),
            0
        );
        let sendfile_offset = 1024 * 1024_i64 + 23;
        let mut sent = 32;
        assert_eq!(
            agora_sandbox_sendfile(
                sendfile_reader,
                sockets[0],
                sendfile_offset,
                &mut sent,
                std::ptr::null_mut(),
                0,
            ),
            0
        );
        assert_eq!(sent, 32);
        let mut received = [0_u8; 32];
        assert_eq!(
            libc::read(sockets[1], received.as_mut_ptr().cast(), received.len()),
            received.len() as libc::ssize_t
        );
        assert_eq!(
            &received,
            &contents[sendfile_offset as usize..sendfile_offset as usize + received.len()]
        );
        let mut sendfile_cold = [1_u8; 16];
        assert_eq!(
            raw_pread(
                sendfile_reader,
                sendfile_cold.as_mut_ptr().cast(),
                sendfile_cold.len(),
                1536 * 1024,
            ),
            sendfile_cold.len() as libc::ssize_t
        );
        assert_eq!(sendfile_cold, [0_u8; 16]);

        let empty_headers_offset = 1280 * 1024_i64 + 29;
        let mut empty_headers = std::mem::zeroed::<libc::sf_hdtr>();
        sent = 32;
        assert_eq!(
            agora_sandbox_sendfile(
                sendfile_reader,
                sockets[0],
                empty_headers_offset,
                &mut sent,
                &mut empty_headers,
                0,
            ),
            0
        );
        assert_eq!(sent, 32);
        assert_eq!(
            libc::read(sockets[1], received.as_mut_ptr().cast(), received.len()),
            received.len() as libc::ssize_t
        );
        assert_eq!(
            &received,
            &contents
                [empty_headers_offset as usize..empty_headers_offset as usize + received.len()]
        );
        assert_eq!(
            raw_pread(
                sendfile_reader,
                sendfile_cold.as_mut_ptr().cast(),
                sendfile_cold.len(),
                1984 * 1024,
            ),
            sendfile_cold.len() as libc::ssize_t
        );
        assert_eq!(sendfile_cold, [0_u8; 16]);
        assert_eq!(libc::close(sockets[0]), 0);
        assert_eq!(libc::close(sockets[1]), 0);
        assert_eq!(super::super::agora_sandbox_close(sendfile_reader), 0);
    });

    controller.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_managed_aio_write_is_rejected_before_native_io() {
    let directory = tempfile::tempdir().unwrap();
    let logical = directory.path().join("lazy-aio-write.bin");
    let path = c_path(&logical);
    let contents = (0..2 * 1024 * 1024)
        .map(|index| (index % 251 + 1) as u8)
        .collect::<Vec<_>>();

    let (writer_runtime, writer_controller) = broker_runtime(directory.path()).await;
    with_test_runtime(&writer_runtime, || unsafe {
        write_managed_file(&path, &contents);
    });
    writer_controller.shutdown().await.unwrap();
    drop(writer_runtime);

    let (runtime, controller) = broker_runtime(directory.path()).await;
    let (result, errno) = with_test_runtime(&runtime, || unsafe {
        let descriptor = super::super::agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        assert!(
            runtime
                .tracked_open(descriptor)
                .unwrap()
                .local_inheritance()
                .unwrap()
                .lazy
        );
        let mut replacement = [0x7f_u8];
        let mut control = std::mem::zeroed::<libc::aiocb>();
        control.aio_fildes = descriptor;
        control.aio_offset = 1024 * 1024 + 17;
        control.aio_buf = replacement.as_mut_ptr().cast();
        control.aio_nbytes = replacement.len();
        let result = agora_sandbox_aio_write(&mut control);
        let errno = *libc::__error();
        if result == 0 {
            assert_eq!(
                await_submitted_aio(&mut control),
                replacement.len() as isize
            );
        }
        assert_eq!(super::super::agora_sandbox_close(descriptor), 0);
        (result, errno)
    });
    controller.shutdown().await.unwrap();
    drop(runtime);

    let (reader_runtime, reader_controller) = broker_runtime(directory.path()).await;
    let actual = with_test_runtime(&reader_runtime, || unsafe { read_managed_file(&path) });
    reader_controller.shutdown().await.unwrap();

    assert_eq!(result, -1);
    assert_eq!(errno, libc::ENOTSUP);
    assert!(
        actual == contents,
        "rejected AIO write changed managed contents"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_managed_lio_write_is_rejected_before_native_io() {
    let directory = tempfile::tempdir().unwrap();
    let logical = directory.path().join("lazy-lio-write.bin");
    let path = c_path(&logical);
    let contents = (0..2 * 1024 * 1024)
        .map(|index| (index % 251 + 1) as u8)
        .collect::<Vec<_>>();

    let (writer_runtime, writer_controller) = broker_runtime(directory.path()).await;
    with_test_runtime(&writer_runtime, || unsafe {
        write_managed_file(&path, &contents);
    });
    writer_controller.shutdown().await.unwrap();
    drop(writer_runtime);

    let (runtime, controller) = broker_runtime(directory.path()).await;
    let (result, errno) = with_test_runtime(&runtime, || unsafe {
        let descriptor = super::super::agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        assert!(
            runtime
                .tracked_open(descriptor)
                .unwrap()
                .local_inheritance()
                .unwrap()
                .lazy
        );
        let mut replacement = [0x5a_u8];
        let mut control = std::mem::zeroed::<libc::aiocb>();
        control.aio_fildes = descriptor;
        control.aio_offset = 512 * 1024 + 19;
        control.aio_buf = replacement.as_mut_ptr().cast();
        control.aio_nbytes = replacement.len();
        control.aio_lio_opcode = libc::LIO_WRITE;
        let controls = [&mut control as *mut libc::aiocb];
        let result = agora_sandbox_lio_listio(
            libc::LIO_WAIT,
            controls.as_ptr(),
            controls.len() as libc::c_int,
            std::ptr::null_mut(),
        );
        let errno = *libc::__error();
        if result == 0 {
            assert_eq!(libc::aio_return(&mut control), replacement.len() as isize);
        }
        assert_eq!(super::super::agora_sandbox_close(descriptor), 0);
        (result, errno)
    });
    controller.shutdown().await.unwrap();
    drop(runtime);

    let (reader_runtime, reader_controller) = broker_runtime(directory.path()).await;
    let actual = with_test_runtime(&reader_runtime, || unsafe { read_managed_file(&path) });
    reader_controller.shutdown().await.unwrap();

    assert_eq!(result, -1);
    assert_eq!(errno, libc::ENOTSUP);
    assert!(
        actual == contents,
        "rejected LIO write changed managed contents"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lazy_writable_open_preserves_partial_blocks_and_append_data() {
    let directory = tempfile::tempdir().unwrap();
    let logical = directory.path().join("lazy-write.bin");
    let path = c_path(&logical);
    let contents = vec![b'x'; 2 * 1024 * 1024];

    let (writer_runtime, writer_controller) = broker_runtime(directory.path()).await;
    with_test_runtime(&writer_runtime, || unsafe {
        write_managed_file(&path, &contents);
    });
    writer_controller.shutdown().await.unwrap();
    drop(writer_runtime);

    let (runtime, controller) = broker_runtime(directory.path()).await;
    with_test_runtime(&runtime, || unsafe {
        let writer = super::super::agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(writer >= 0);
        assert!(
            runtime
                .tracked_open(writer)
                .unwrap()
                .local_inheritance()
                .unwrap()
                .lazy
        );

        let mut cold = [1_u8; 16];
        assert_eq!(
            raw_pread(
                writer,
                cold.as_mut_ptr().cast(),
                cold.len(),
                contents.len() as libc::off_t - cold.len() as libc::off_t,
            ),
            cold.len() as libc::ssize_t
        );
        assert_eq!(cold, [0_u8; 16]);

        let offset = 512 * 1024_i64 + 17;
        let replacement = b"changed";
        assert_eq!(
            agora_sandbox_pwrite(
                writer,
                replacement.as_ptr().cast(),
                replacement.len(),
                offset,
            ),
            replacement.len() as libc::ssize_t
        );
        let mut around = [0_u8; 9];
        assert_eq!(
            agora_sandbox_pread(writer, around.as_mut_ptr().cast(), around.len(), offset - 1,),
            around.len() as libc::ssize_t
        );
        assert_eq!(&around, b"xchangedx");
        assert_eq!(super::super::agora_sandbox_close(writer), 0);

        let appender = super::super::agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_WRONLY | libc::O_APPEND,
            0,
        );
        assert!(appender >= 0);
        assert!(
            runtime
                .tracked_open(appender)
                .unwrap()
                .local_inheritance()
                .unwrap()
                .lazy
        );
        assert_eq!(agora_sandbox_write(appender, b"tail".as_ptr().cast(), 4), 4);
        assert_eq!(super::super::agora_sandbox_close(appender), 0);

        let reader = super::super::agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(reader >= 0);
        let mut changed = [0_u8; 9];
        assert_eq!(
            agora_sandbox_pread(
                reader,
                changed.as_mut_ptr().cast(),
                changed.len(),
                offset - 1,
            ),
            changed.len() as libc::ssize_t
        );
        assert_eq!(&changed, b"xchangedx");
        let mut tail = [0_u8; 4];
        assert_eq!(
            agora_sandbox_pread(
                reader,
                tail.as_mut_ptr().cast(),
                tail.len(),
                contents.len() as libc::off_t,
            ),
            tail.len() as libc::ssize_t
        );
        assert_eq!(&tail, b"tail");
        assert_eq!(super::super::agora_sandbox_close(reader), 0);
    });

    controller.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_managed_io_failures_preserve_errno_and_recoverable_state() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, controller) = broker_runtime(directory.path()).await;
    let logical = directory.path().join("failures.txt");
    let path = c_path(&logical);

    with_test_runtime(&runtime, || unsafe {
        let descriptor = super::super::agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(
            agora_sandbox_write(descriptor, b"0123456789".as_ptr().cast(), 10),
            10
        );

        let mut byte = [0_u8; 1];
        let mut vector_byte = [0_u8; 1];
        let read_vector = libc::iovec {
            iov_base: vector_byte.as_mut_ptr().cast(),
            iov_len: vector_byte.len(),
        };
        let write_vector = libc::iovec {
            iov_base: b"x".as_ptr().cast_mut().cast(),
            iov_len: 1,
        };
        assert_eq!(
            sandbox_read_with(descriptor, byte.as_mut_ptr().cast(), 1, None, None,),
            -1
        );
        assert_eq!(
            sandbox_pread_with(descriptor, byte.as_mut_ptr().cast(), 1, 0, None),
            -1
        );
        assert_eq!(
            sandbox_readv_with(descriptor, &read_vector, 1, None, None),
            -1
        );
        assert_eq!(
            sandbox_preadv_with(descriptor, &read_vector, 1, 0, None),
            -1
        );
        assert_eq!(
            sandbox_write_with(descriptor, b"x".as_ptr().cast(), 1, None, None),
            -1
        );
        assert_eq!(
            sandbox_pwrite_with(descriptor, b"x".as_ptr().cast(), 1, 0, None),
            -1
        );
        assert_eq!(
            sandbox_writev_with(descriptor, &write_vector, 1, None, None),
            -1
        );
        assert_eq!(
            sandbox_pwritev_with(descriptor, &write_vector, 1, 0, None),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOSYS);

        assert_eq!(
            sandbox_read_with(
                descriptor,
                byte.as_mut_ptr().cast(),
                1,
                original_read(),
                None,
            ),
            -1
        );
        assert_eq!(
            sandbox_readv_with(descriptor, &read_vector, 1, original_readv(), None),
            -1
        );
        assert_eq!(
            sandbox_write_with(descriptor, b"x".as_ptr().cast(), 1, original_write(), None,),
            -1
        );
        assert_eq!(
            sandbox_writev_with(descriptor, &write_vector, 1, original_writev(), None),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOSYS);

        assert_eq!(
            managed_write_io(
                descriptor,
                ContentIoOffset::Positioned(0),
                Some(1),
                || Ok(1),
                |_, _| unreachable!(),
                |_| {
                    set_errno(libc::EAGAIN);
                    -1
                },
                || {
                    set_errno(libc::EAGAIN);
                    -1
                },
            ),
            Some(-1)
        );
        assert_eq!(*libc::__error(), libc::EAGAIN);
        assert_eq!(
            managed_write_io(
                descriptor,
                ContentIoOffset::Positioned(8),
                Some(1),
                || Ok(1),
                |_, _| unreachable!(),
                |_| 2,
                || unreachable!(),
            ),
            Some(2)
        );

        let open = runtime.tracked_open(descriptor).unwrap();
        let content = open.managed();
        let registration = open.local_inheritance().unwrap();
        assert_eq!(
            lock(&content.state.dirty).as_slice(),
            &[LocalByteRange::new(8, 10).unwrap()]
        );
        {
            let state = registration.state.lock().unwrap();
            state.set_offset(-1).unwrap();
        }
        assert_eq!(
            managed_read_io(
                descriptor,
                ContentIoOffset::Sequential,
                || Ok(0),
                |_, _| unreachable!(),
                |_| 0,
                || unreachable!(),
            ),
            Some(-1)
        );
        assert_eq!(
            managed_write_io(
                descriptor,
                ContentIoOffset::Sequential,
                Some(0),
                || Ok(0),
                |_, _| unreachable!(),
                |_| 0,
                || unreachable!(),
            ),
            Some(-1)
        );
        assert_eq!(
            managed_write_io(
                descriptor,
                ContentIoOffset::Sequential,
                Some(1),
                || Ok(1),
                |_, _| unreachable!(),
                |_| 0,
                || unreachable!(),
            ),
            Some(-1)
        );
        assert_eq!(agora_sandbox_lseek(descriptor, 0, libc::SEEK_CUR), -1);
        assert_eq!(*libc::__error(), libc::EINVAL);
        {
            let state = registration.state.lock().unwrap();
            state.set_offset(libc::off_t::MAX).unwrap();
        }
        assert_eq!(
            managed_read_io(
                descriptor,
                ContentIoOffset::Sequential,
                || Ok(0),
                |_, _| unreachable!(),
                |_| 1,
                || unreachable!(),
            ),
            Some(-1)
        );
        assert_eq!(*libc::__error(), libc::EOVERFLOW);
        {
            let state = registration.state.lock().unwrap();
            state.set_offset(0).unwrap();
        }
        assert_eq!(agora_sandbox_lseek(descriptor, -1, libc::SEEK_SET), -1);
        assert_eq!(*libc::__error(), libc::EINVAL);
        assert_eq!(agora_sandbox_lseek(descriptor, 0, -1), -1);
        assert_eq!(*libc::__error(), libc::EINVAL);

        assert_eq!(
            super::super::agora_sandbox_validate_content_fcntl(descriptor),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            super::super::agora_sandbox_flock(descriptor, libc::LOCK_EX | libc::LOCK_NB),
            0
        );
        assert_eq!(
            super::super::agora_sandbox_flock(descriptor, libc::LOCK_UN),
            0
        );
        set_descriptor_close_on_exec(descriptor, false).unwrap();
        super::super::agora_sandbox_fcntl_commit_setfd(descriptor);
        let inherited = runtime.inheritable_local_descriptors();
        assert_eq!(inherited.len(), 3);

        assert_eq!(libc::ftruncate(registration.state.as_raw_fd(), 0), 0);
        assert_eq!(
            managed_read_io(
                descriptor,
                ContentIoOffset::Sequential,
                || Ok(0),
                |_, _| unreachable!(),
                |_| 0,
                || unreachable!(),
            ),
            Some(-1)
        );
        assert_eq!(
            managed_write_io(
                descriptor,
                ContentIoOffset::Sequential,
                Some(1),
                || Ok(1),
                |_, _| unreachable!(),
                |_| 0,
                || unreachable!(),
            ),
            Some(-1)
        );
        assert_eq!(agora_sandbox_lseek(descriptor, 0, libc::SEEK_CUR), -1);
        assert_eq!(super::super::agora_sandbox_close(descriptor), 0);
    });

    controller.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_local_descriptors_restore_aliases_and_shared_offsets() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, controller) = broker_runtime(directory.path()).await;
    let logical = directory.path().join("inherited.txt");
    let path = c_path(&logical);

    let (descriptor, alias, encoded) = with_test_runtime(&runtime, || unsafe {
        let descriptor = super::super::agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(
            agora_sandbox_write(descriptor, b"inherit".as_ptr().cast(), 7),
            7
        );
        let alias = super::super::agora_sandbox_dup(descriptor);
        assert!(alias >= 0);
        set_descriptor_close_on_exec(descriptor, false).unwrap();
        set_descriptor_close_on_exec(alias, false).unwrap();
        let open = runtime.tracked_open(descriptor).unwrap();
        runtime.refresh_local_state_inheritance(&open);
        let inherited = runtime.encode_inherited_local_descriptors().unwrap();
        let encoded = inherited.encoded;
        assert_eq!(
            serde_json::from_str::<InheritedLocalDescriptors>(&encoded)
                .unwrap()
                .descriptors
                .len(),
            2
        );
        (descriptor, alias, encoded)
    });

    let retained = runtime.retain_local_files_before_fork().unwrap();
    assert_eq!(retained.len(), 1);
    let inherited = unsafe { duplicate_inherited_local_descriptors(&encoded) };
    let restored_descriptors = inherited
        .descriptors
        .iter()
        .map(|descriptor| descriptor.descriptor)
        .collect::<Vec<_>>();
    let inherited = serde_json::to_string(&inherited).unwrap();

    let mut restored = FilesystemHookRuntime::new_encrypted(
        directory.path().join("workdir/fs"),
        b"broker-hook-test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    restored.local = runtime.local.clone();
    restored.restore_inherited_local_descriptors(None).unwrap();
    restored
        .restore_inherited_local_descriptors(Some("not-json"))
        .unwrap();
    let mut wrong_version = serde_json::from_str::<InheritedLocalDescriptors>(&inherited).unwrap();
    wrong_version.version = INHERITED_LOCAL_DESCRIPTOR_VERSION + 1;
    restored
        .restore_inherited_local_descriptors(Some(&serde_json::to_string(&wrong_version).unwrap()))
        .unwrap();
    restored
        .restore_inherited_local_descriptors(Some(&inherited))
        .unwrap();
    assert_eq!(lock(&restored.open_files).len(), 2);
    assert!(Arc::ptr_eq(
        &restored.tracked_open(restored_descriptors[0]).unwrap(),
        &restored.tracked_open(restored_descriptors[1]).unwrap(),
    ));

    with_test_runtime(&restored, || unsafe {
        assert_eq!(
            agora_sandbox_lseek(restored_descriptors[0], 0, libc::SEEK_SET),
            0
        );
        let mut content = [0_u8; 3];
        assert_eq!(
            agora_sandbox_read(
                restored_descriptors[1],
                content.as_mut_ptr().cast(),
                content.len(),
            ),
            3
        );
        assert_eq!(&content, b"inh");
        assert_eq!(
            super::super::agora_sandbox_close(restored_descriptors[0]),
            0
        );
        assert_eq!(
            super::super::agora_sandbox_close(restored_descriptors[1]),
            0
        );
    });
    with_test_runtime(&runtime, || unsafe {
        let mut next = [0_u8; 1];
        assert_eq!(
            agora_sandbox_read(descriptor, next.as_mut_ptr().cast(), 1),
            1
        );
        assert_eq!(&next, b"e");
        assert_eq!(super::super::agora_sandbox_close(alias), 0);
        assert_eq!(super::super::agora_sandbox_close(descriptor), 0);
    });

    controller.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_local_descriptors_release_handles_when_file_actions_close_all_aliases() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, controller) = broker_runtime(directory.path()).await;
    let logical = directory.path().join("spawn-closed.txt");
    let path = c_path(&logical);

    let (descriptor, inherited) = with_test_runtime(&runtime, || unsafe {
        let descriptor = super::super::agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        set_descriptor_close_on_exec(descriptor, false).unwrap();
        let open = runtime.tracked_open(descriptor).unwrap();
        runtime.refresh_local_state_inheritance(&open);
        let inherited = runtime.encode_inherited_local_descriptors().unwrap();
        (descriptor, inherited)
    });
    let retained = runtime.retain_local_files_before_fork().unwrap();
    assert_eq!(retained, inherited.handles);

    let child = unsafe { duplicate_inherited_local_descriptors(&inherited.encoded) };
    let state_descriptor = child.descriptors[0].state_descriptor;
    let lock_descriptor = child.descriptors[0].lock_descriptor;
    for inherited in &child.descriptors {
        close_if_open(inherited.descriptor);
    }

    let mut restored = FilesystemHookRuntime::new_encrypted(
        directory.path().join("workdir/fs"),
        b"broker-hook-test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    restored.local = runtime.local.clone();
    restored
        .restore_inherited_local_descriptors(Some(&serde_json::to_string(&child).unwrap()))
        .unwrap();

    let state_closed = !descriptor_is_open(state_descriptor);
    let lock_closed = !descriptor_is_open(lock_descriptor);
    close_if_open(state_descriptor);
    close_if_open(lock_descriptor);
    with_test_runtime(&runtime, || unsafe {
        assert_eq!(super::super::agora_sandbox_close(descriptor), 0);
    });
    let released = runtime
        .local
        .as_ref()
        .unwrap()
        .release_retained(retained)
        .is_err();
    controller.shutdown().await.unwrap();

    assert!(state_closed, "inherited state descriptor was leaked");
    assert!(lock_closed, "inherited lock descriptor was leaked");
    assert!(released, "unrestored Broker retain was leaked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_local_descriptors_close_valid_content_when_auxiliary_fd_was_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, controller) = broker_runtime(directory.path()).await;
    let logical = directory.path().join("spawn-replaced.txt");
    let path = c_path(&logical);

    let (descriptor, inherited) = with_test_runtime(&runtime, || unsafe {
        let descriptor = super::super::agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        set_descriptor_close_on_exec(descriptor, false).unwrap();
        let open = runtime.tracked_open(descriptor).unwrap();
        runtime.refresh_local_state_inheritance(&open);
        let inherited = runtime.encode_inherited_local_descriptors().unwrap();
        (descriptor, inherited)
    });
    let retained = runtime.retain_local_files_before_fork().unwrap();
    assert_eq!(retained, inherited.handles);

    let child = unsafe { duplicate_inherited_local_descriptors(&inherited.encoded) };
    let content_descriptor = child.descriptors[0].descriptor;
    let state_descriptor = child.descriptors[0].state_descriptor;
    let lock_descriptor = child.descriptors[0].lock_descriptor;
    let replacement = std::fs::File::open("/dev/null").unwrap();
    assert_eq!(
        unsafe { libc::dup2(replacement.as_raw_fd(), state_descriptor) },
        state_descriptor
    );

    let mut restored = FilesystemHookRuntime::new_encrypted(
        directory.path().join("workdir/fs"),
        b"broker-hook-test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    restored.local = runtime.local.clone();
    restored
        .restore_inherited_local_descriptors(Some(&serde_json::to_string(&child).unwrap()))
        .unwrap();

    let content_closed = !descriptor_is_open(content_descriptor);
    let replacement_preserved = descriptor_is_open(state_descriptor);
    let lock_closed = !descriptor_is_open(lock_descriptor);
    close_if_open(content_descriptor);
    close_if_open(state_descriptor);
    close_if_open(lock_descriptor);
    with_test_runtime(&runtime, || unsafe {
        assert_eq!(super::super::agora_sandbox_close(descriptor), 0);
    });
    let released = runtime
        .local
        .as_ref()
        .unwrap()
        .release_retained(retained)
        .is_err();
    controller.shutdown().await.unwrap();

    assert!(
        content_closed,
        "untracked plaintext descriptor remained open"
    );
    assert!(
        replacement_preserved,
        "unrelated replacement descriptor was closed"
    );
    assert!(
        lock_closed,
        "validated inherited lock descriptor was leaked"
    );
    assert!(released, "unrestored Broker retain was leaked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_local_descriptors_close_plaintext_moved_into_an_auxiliary_fd() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, controller) = broker_runtime(directory.path()).await;
    let logical = directory.path().join("spawn-role-change.txt");
    let path = c_path(&logical);

    let (descriptor, inherited) = with_test_runtime(&runtime, || unsafe {
        let descriptor = super::super::agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        set_descriptor_close_on_exec(descriptor, false).unwrap();
        let open = runtime.tracked_open(descriptor).unwrap();
        runtime.refresh_local_state_inheritance(&open);
        let inherited = runtime.encode_inherited_local_descriptors().unwrap();
        (descriptor, inherited)
    });
    let retained = runtime.retain_local_files_before_fork().unwrap();
    assert_eq!(retained, inherited.handles);

    let child = unsafe { duplicate_inherited_local_descriptors(&inherited.encoded) };
    let content_descriptor = child.descriptors[0].descriptor;
    let state_descriptor = child.descriptors[0].state_descriptor;
    let lock_descriptor = child.descriptors[0].lock_descriptor;
    assert_eq!(
        unsafe { libc::dup2(content_descriptor, state_descriptor) },
        state_descriptor
    );

    let mut restored = FilesystemHookRuntime::new_encrypted(
        directory.path().join("workdir/fs"),
        b"broker-hook-test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    restored.local = runtime.local.clone();
    restored
        .restore_inherited_local_descriptors(Some(&serde_json::to_string(&child).unwrap()))
        .unwrap();

    let content_closed = !descriptor_is_open(content_descriptor);
    let moved_plaintext_closed = !descriptor_is_open(state_descriptor);
    let lock_closed = !descriptor_is_open(lock_descriptor);
    close_if_open(content_descriptor);
    close_if_open(state_descriptor);
    close_if_open(lock_descriptor);
    with_test_runtime(&runtime, || unsafe {
        assert_eq!(super::super::agora_sandbox_close(descriptor), 0);
    });
    let released = runtime
        .local
        .as_ref()
        .unwrap()
        .release_retained(retained)
        .is_err();
    controller.shutdown().await.unwrap();

    assert!(
        content_closed,
        "unrestored content descriptor remained open"
    );
    assert!(
        moved_plaintext_closed,
        "plaintext moved into an auxiliary descriptor remained open"
    );
    assert!(
        lock_closed,
        "validated inherited lock descriptor was leaked"
    );
    assert!(released, "unrestored Broker retain was leaked");
}
