use super::*;
use crate::filesystem::broker::protocol::{BackingPath, ByteRange, Request, Response};
use crate::filesystem::crypto::CONTENT_HEADER_SIZE;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::{Duration, Instant};

struct Fixture {
    root: tempfile::TempDir,
    cipher: FileCipher,
    broker: LocalBroker,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
        let broker = LocalBroker::new(root.path(), cipher.clone()).unwrap();
        Self {
            root,
            cipher,
            broker,
        }
    }

    fn encrypted(&self, name: &str, contents: &[u8]) -> std::path::PathBuf {
        let path = self.root.path().join(name);
        let mut plaintext = tempfile::tempfile().unwrap();
        plaintext.write_all(contents).unwrap();
        self.cipher.encrypt(&mut plaintext, &path).unwrap();
        path
    }

    fn open(&self, path: &Path, writable: bool) -> (String, File) {
        let mut reply = self.broker.handle(
            Request::Open {
                path: BackingPath::from_path(path),
                flags: if writable {
                    libc::O_RDWR
                } else {
                    libc::O_RDONLY
                },
            },
            None,
        );
        let Response::Open { handle, .. } = reply.response else {
            panic!("unexpected open response: {:?}", reply.response);
        };
        assert_eq!(reply.descriptors.len(), 3);
        let content = reply.descriptors.remove(0);
        (handle, content)
    }

    fn decrypt(&self, path: &Path) -> Vec<u8> {
        let mut plaintext = tempfile::tempfile().unwrap();
        self.cipher.decrypt(path, &mut plaintext).unwrap();
        plaintext.seek(SeekFrom::Start(0)).unwrap();
        let mut output = Vec::new();
        plaintext.read_to_end(&mut output).unwrap();
        output
    }
}

fn assert_error(response: Response, errno: libc::c_int) {
    assert!(
        matches!(response, Response::Error { errno: actual, .. } if actual == errno),
        "unexpected response: {response:?}"
    );
}

#[test]
fn large_read_only_open_materializes_only_requested_ranges() {
    let fixture = Fixture::new();
    let contents = vec![b'x'; 2 * 1024 * 1024];
    let path = fixture.encrypted("lazy-read", &contents);
    let mut reply = fixture.broker.handle(
        Request::Open {
            path: BackingPath::from_path(&path),
            flags: libc::O_RDONLY,
        },
        None,
    );
    let Response::Open {
        handle, lazy: true, ..
    } = reply.response
    else {
        panic!("unexpected lazy open response: {:?}", reply.response);
    };
    let plaintext = reply.descriptors.remove(0);
    assert_eq!(plaintext.metadata().unwrap().len(), contents.len() as u64);

    let offset = 512 * 1024 + 17;
    let mut cold = [1_u8; 32];
    read_exact_at(&plaintext, &mut cold, offset).unwrap();
    assert_eq!(cold, [0_u8; 32]);

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Materialize {
                    handle,
                    range: Some(ByteRange::new(offset, offset + cold.len() as u64).unwrap()),
                },
                None,
            )
            .response,
        Response::Success
    );
    read_exact_at(&plaintext, &mut cold, offset).unwrap();
    assert_eq!(
        &cold,
        &contents[offset as usize..offset as usize + cold.len()]
    );

    let mut untouched = [1_u8; 32];
    read_exact_at(&plaintext, &mut untouched, 1536 * 1024).unwrap();
    assert_eq!(untouched, [0_u8; 32]);
}

#[test]
fn small_read_only_open_remains_eager() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("eager-read", b"plaintext");
    let mut reply = fixture.broker.handle(
        Request::Open {
            path: BackingPath::from_path(&path),
            flags: libc::O_RDONLY,
        },
        None,
    );
    assert!(matches!(reply.response, Response::Open { lazy: false, .. }));
    let plaintext = reply.descriptors.remove(0);
    let mut contents = [0_u8; 9];
    read_exact_at(&plaintext, &mut contents, 0).unwrap();
    assert_eq!(&contents, b"plaintext");
}

#[test]
fn open_state_preserves_synchronous_write_flags() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("sync-flags", b"plaintext");
    let mut reply = fixture.broker.handle(
        Request::Open {
            path: BackingPath::from_path(&path),
            flags: libc::O_RDWR | libc::O_SYNC | libc::O_DSYNC,
        },
        None,
    );
    assert!(matches!(reply.response, Response::Open { .. }));

    let state = LocalOpenState::from_descriptor(reply.descriptors.remove(1).into()).unwrap();
    let flags = state.lock().unwrap().flags().unwrap();
    assert_eq!(
        flags & (libc::O_SYNC | libc::O_DSYNC),
        libc::O_SYNC | libc::O_DSYNC
    );
}

#[test]
fn lazy_read_reports_corruption_only_when_the_block_is_materialized() {
    let fixture = Fixture::new();
    let contents = vec![b'x'; 2 * 1024 * 1024];
    let path = fixture.encrypted("lazy-corruption", &contents);
    let corrupted_plaintext_offset = 1536 * 1024_u64;
    let ciphertext_offset = CONTENT_HEADER_SIZE as u64
        + (corrupted_plaintext_offset / crate::filesystem::crypto::PLAINTEXT_BLOCK_SIZE as u64)
            * crate::filesystem::crypto::CIPHERTEXT_BLOCK_SIZE as u64
        + 12;
    let ciphertext = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut byte = [0_u8; 1];
    ciphertext
        .read_exact_at(&mut byte, ciphertext_offset)
        .unwrap();
    byte[0] ^= 0xff;
    ciphertext.write_all_at(&byte, ciphertext_offset).unwrap();

    let mut reply = fixture.broker.handle(
        Request::Open {
            path: BackingPath::from_path(&path),
            flags: libc::O_RDONLY,
        },
        None,
    );
    let Response::Open {
        handle, lazy: true, ..
    } = reply.response
    else {
        panic!("unexpected lazy open response: {:?}", reply.response);
    };
    let plaintext = reply.descriptors.remove(0);

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Materialize {
                    handle: handle.clone(),
                    range: Some(ByteRange::new(0, 32).unwrap()),
                },
                None,
            )
            .response,
        Response::Success
    );
    let mut prefix = [0_u8; 32];
    read_exact_at(&plaintext, &mut prefix, 0).unwrap();
    assert_eq!(prefix, [b'x'; 32]);

    assert_error(
        fixture
            .broker
            .handle(
                Request::Materialize {
                    handle,
                    range: Some(
                        ByteRange::new(corrupted_plaintext_offset, corrupted_plaintext_offset + 32)
                            .unwrap(),
                    ),
                },
                None,
            )
            .response,
        libc::EIO,
    );
}

#[test]
fn lazy_materialization_does_not_hide_an_unreported_plaintext_change() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("lazy-baseline", &vec![b'x'; 2 * 1024 * 1024]);
    let mut reply = fixture.broker.handle(
        Request::Open {
            path: BackingPath::from_path(&path),
            flags: libc::O_RDONLY,
        },
        None,
    );
    let Response::Open { handle, .. } = reply.response else {
        panic!("unexpected open response: {:?}", reply.response);
    };
    let plaintext = reply.descriptors.remove(0);
    write_all_at(&plaintext, b"changed", 0).unwrap();

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Materialize {
                    handle: handle.clone(),
                    range: Some(ByteRange::new(1024 * 1024, 1024 * 1024 + 32).unwrap()),
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_error(
        fixture
            .broker
            .handle(
                Request::Close {
                    handle,
                    ranges: Vec::new(),
                },
                None,
            )
            .response,
        libc::EBADF,
    );
}

#[test]
fn large_writable_first_open_is_lazy() {
    let fixture = Fixture::new();
    let contents = vec![b'x'; 2 * 1024 * 1024];
    let path = fixture.encrypted("lazy-writable-first-open", &contents);
    let mut reply = fixture.broker.handle(
        Request::Open {
            path: BackingPath::from_path(&path),
            flags: libc::O_RDWR,
        },
        None,
    );
    assert!(matches!(reply.response, Response::Open { lazy: true, .. }));
    let plaintext = reply.descriptors.remove(0);
    let mut cold = [1_u8; 32];
    read_exact_at(&plaintext, &mut cold, contents.len() as u64 - 32).unwrap();
    assert_eq!(cold, [0_u8; 32]);
}

#[test]
fn large_writable_open_reuses_a_live_lazy_plaintext_vnode_without_materializing_it() {
    let fixture = Fixture::new();
    let contents = vec![b'x'; 2 * 1024 * 1024];
    let path = fixture.encrypted("lazy-writable", &contents);
    let (read_handle, reader) = fixture.open(&path, false);

    let mut reply = fixture.broker.handle(
        Request::Open {
            path: BackingPath::from_path(&path),
            flags: libc::O_RDWR,
        },
        None,
    );
    let Response::Open {
        handle: write_handle,
        lazy: true,
        ..
    } = reply.response
    else {
        panic!("unexpected writable lazy open: {:?}", reply.response);
    };
    let writer = reply.descriptors.remove(0);

    let mut untouched = [1_u8; 32];
    read_exact_at(&reader, &mut untouched, contents.len() as u64 - 32).unwrap();
    assert_eq!(untouched, [0_u8; 32]);

    let offset = 512 * 1024 + 17;
    let replacement = b"changed";
    let range = ByteRange::new(offset, offset + replacement.len() as u64).unwrap();
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: write_handle.clone(),
                    write_id: "partial".to_string(),
                    range,
                },
                None,
            )
            .response,
        Response::Success
    );
    write_all_at(&writer, replacement, offset).unwrap();
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::FinishWrite {
                    handle: write_handle.clone(),
                    write_id: "partial".to_string(),
                    range,
                },
                None,
            )
            .response,
        Response::Success
    );

    let block_start = offset - offset % PLAINTEXT_BLOCK_SIZE as u64;
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Materialize {
                    handle: read_handle,
                    range: Some(
                        ByteRange::new(block_start, block_start + PLAINTEXT_BLOCK_SIZE as u64)
                            .unwrap(),
                    ),
                },
                None,
            )
            .response,
        Response::Success
    );
    let mut materialized = vec![0_u8; PLAINTEXT_BLOCK_SIZE];
    read_exact_at(&reader, &mut materialized, block_start).unwrap();
    let within = (offset - block_start) as usize;
    assert_eq!(
        &materialized[..within],
        &contents[block_start as usize..offset as usize]
    );
    assert_eq!(
        &materialized[within..within + replacement.len()],
        replacement
    );
    assert_eq!(
        &materialized[within + replacement.len()..],
        &contents[offset as usize + replacement.len()..block_start as usize + PLAINTEXT_BLOCK_SIZE]
    );

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle: write_handle,
                    ranges: Vec::new(),
                    durable: true,
                },
                None,
            )
            .response,
        Response::Success
    );
    let mut expected = contents;
    expected[offset as usize..offset as usize + replacement.len()].copy_from_slice(replacement);
    assert_eq!(fixture.decrypt(&path), expected);
}

#[test]
fn sync_encrypts_only_reported_ranges_and_propagates_them_to_peer_handles() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("file", b"abcdef");
    let (first_id, first) = fixture.open(&path, true);
    let (_second_id, second) = fixture.open(&path, true);
    write_all_at(&first, b"XY", 2).unwrap();

    let reply = fixture.broker.handle(
        Request::Sync {
            handle: first_id,
            ranges: vec![ByteRange::new(2, 4).unwrap()],
            durable: true,
        },
        None,
    );

    assert_eq!(reply.response, Response::Success);
    assert_eq!(fixture.decrypt(&path), b"abXYef");
    let mut peer = [0_u8; 6];
    read_exact_at(&second, &mut peer, 0).unwrap();
    assert_eq!(&peer, b"abXYef");
}

#[test]
fn subsequent_opens_reuse_the_live_plaintext_without_decrypting_again() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("reused-plaintext", b"plaintext");
    let (_first, first) = fixture.open(&path, false);
    let ciphertext = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let offset = CONTENT_HEADER_SIZE as u64 + 12;
    let mut byte = [0_u8; 1];
    ciphertext.read_exact_at(&mut byte, offset).unwrap();
    byte[0] ^= 0xff;
    ciphertext.write_all_at(&byte, offset).unwrap();

    let (_second, second) = fixture.open(&path, false);
    let mut contents = [0_u8; 9];
    read_exact_at(&second, &mut contents, 0).unwrap();

    assert_eq!(&contents, b"plaintext");
    let mut first_contents = [0_u8; 9];
    read_exact_at(&first, &mut first_contents, 0).unwrap();
    assert_eq!(first_contents, contents);
}

#[test]
fn truncation_remains_dirty_until_a_durable_sync() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("truncate-durability", b"plaintext");
    let reply = fixture.broker.handle(
        Request::Open {
            path: BackingPath::from_path(&path),
            flags: libc::O_RDWR | libc::O_TRUNC,
        },
        None,
    );
    let Response::Open { handle, .. } = reply.response else {
        panic!("unexpected open response: {:?}", reply.response);
    };
    let local = lock(&fixture.broker.handles).get(&handle).unwrap().clone();
    let shared = Arc::clone(&lock(&local).shared);
    assert!(lock(&shared.inner).needs_durable_sync);

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle,
                    ranges: Vec::new(),
                    durable: true,
                },
                None,
            )
            .response,
        Response::Success
    );
    assert!(!lock(&shared.inner).needs_durable_sync);
    assert!(fixture.decrypt(&path).is_empty());
}

#[test]
fn independent_opens_share_content_vnode_but_not_lock_description() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("independent-locks", b"plaintext");
    let mut first = fixture.broker.handle(
        Request::Open {
            path: BackingPath::from_path(&path),
            flags: libc::O_RDWR,
        },
        None,
    );
    let mut second = fixture.broker.handle(
        Request::Open {
            path: BackingPath::from_path(&path),
            flags: libc::O_RDWR,
        },
        None,
    );
    assert!(matches!(first.response, Response::Open { .. }));
    assert!(matches!(second.response, Response::Open { .. }));
    assert_eq!(first.descriptors.len(), 3);
    assert_eq!(second.descriptors.len(), 3);

    let first_content = first.descriptors.remove(0);
    let second_content = second.descriptors.remove(0);
    let first_lock = first.descriptors.remove(1);
    let second_lock = second.descriptors.remove(1);
    let first_content = first_content.metadata().unwrap();
    let second_content = second_content.metadata().unwrap();
    assert_eq!(first_content.dev(), second_content.dev());
    assert_eq!(first_content.ino(), second_content.ino());
    let first_lock_metadata = first_lock.metadata().unwrap();
    let second_lock_metadata = second_lock.metadata().unwrap();
    assert_eq!(first_lock_metadata.dev(), second_lock_metadata.dev());
    assert_eq!(first_lock_metadata.ino(), second_lock_metadata.ino());

    assert_eq!(
        unsafe { libc::flock(first_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert_eq!(
        unsafe { libc::flock(second_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        -1
    );
    assert_eq!(unsafe { *libc::__error() }, libc::EWOULDBLOCK);
    assert_eq!(
        unsafe { libc::flock(first_lock.as_raw_fd(), libc::LOCK_UN) },
        0
    );
}

#[test]
fn completed_single_handle_writes_are_merged_until_the_batch_deadline() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("batched", b"abcdefgh");
    let (handle, plaintext) = fixture.open(&path, true);
    let write_id = "11111111111111111111111111111111";
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: handle.clone(),
                    write_id: write_id.to_string(),
                    range: ByteRange::new(2, 6).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    write_all_at(&plaintext, b"WXYZ", 2).unwrap();
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::FinishWrite {
                    handle: handle.clone(),
                    write_id: write_id.to_string(),
                    range: ByteRange::new(2, 6).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );

    assert_eq!(fixture.decrypt(&path), b"abcdefgh");
    assert!(fixture.broker.writeback_pending());
    let local = lock(&fixture.broker.handles).get(&handle).unwrap().clone();
    let shared = Arc::clone(&lock(&local).shared);
    let pending_since = lock(&shared.inner).pending_since.unwrap();
    fixture.broker.flush_due(pending_since).unwrap();
    assert!(fixture.broker.writeback_pending());
    fixture
        .broker
        .flush_due(Instant::now() + WRITEBACK_DELAY)
        .unwrap();
    assert!(!fixture.broker.writeback_pending());
    assert_eq!(fixture.decrypt(&path), b"abWXYZgh");
}

#[test]
fn completed_writes_are_visible_to_an_existing_peer_before_writeback() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("peer-finish", b"abcdefgh");
    let (writer, plaintext) = fixture.open(&path, true);
    let (_reader, peer) = fixture.open(&path, false);
    let write_id = "11111111111111111111111111111111";
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: writer.clone(),
                    write_id: write_id.to_string(),
                    range: ByteRange::new(2, 6).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    write_all_at(&plaintext, b"WXYZ", 2).unwrap();
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::FinishWrite {
                    handle: writer,
                    write_id: write_id.to_string(),
                    range: ByteRange::new(2, 6).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );

    let mut contents = [0_u8; 8];
    read_exact_at(&peer, &mut contents, 0).unwrap();
    assert_eq!(&contents, b"abWXYZgh");
    assert_eq!(fixture.decrypt(&path), b"abcdefgh");
}

#[test]
fn syncing_one_handle_flushes_completed_writes_from_a_peer_handle() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("peer-sync", b"abcdefgh");
    let (syncing_handle, _syncing_file) = fixture.open(&path, true);
    let (writing_handle, writing_file) = fixture.open(&path, true);
    let write_id = "11111111111111111111111111111111";
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: writing_handle.clone(),
                    write_id: write_id.to_string(),
                    range: ByteRange::new(2, 6).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    write_all_at(&writing_file, b"WXYZ", 2).unwrap();
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::FinishWrite {
                    handle: writing_handle,
                    write_id: write_id.to_string(),
                    range: ByteRange::new(2, 6).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle: syncing_handle,
                    ranges: Vec::new(),
                    durable: true,
                },
                None,
            )
            .response,
        Response::Success
    );

    assert_eq!(fixture.decrypt(&path), b"abWXYZgh");
}

#[test]
fn ordinary_writes_can_overlap_without_becoming_append_or_materialization_races() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("busy-write", b"abcdefgh");
    let (active_handle, _active_file) = fixture.open(&path, true);
    let (waiting_handle, _waiting_file) = fixture.open(&path, true);
    let active_write_id = "11111111111111111111111111111111";
    let waiting_write_id = "22222222222222222222222222222222";
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: active_handle.clone(),
                    write_id: active_write_id.to_string(),
                    range: ByteRange::new(0, 1).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: waiting_handle.clone(),
                    write_id: waiting_write_id.to_string(),
                    range: ByteRange::new(1, 2).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_error(
        fixture
            .broker
            .handle(
                Request::BeginAppend {
                    handle: waiting_handle.clone(),
                    write_id: "33333333333333333333333333333333".to_string(),
                },
                None,
            )
            .response,
        libc::EAGAIN,
    );
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::CancelWrite {
                    handle: active_handle,
                    write_id: active_write_id.to_string(),
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::CancelWrite {
                    handle: waiting_handle,
                    write_id: waiting_write_id.to_string(),
                },
                None,
            )
            .response,
        Response::Success
    );
}

#[test]
fn explicit_sync_does_not_fail_only_because_an_ordinary_write_is_in_flight() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("concurrent-sync", b"abcdefgh");
    let (writing_handle, writing_file) = fixture.open(&path, true);
    let (syncing_handle, _syncing_file) = fixture.open(&path, true);
    let write_id = "11111111111111111111111111111111";

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: writing_handle.clone(),
                    write_id: write_id.to_string(),
                    range: ByteRange::new(2, 6).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle: syncing_handle,
                    ranges: Vec::new(),
                    durable: true,
                },
                None,
            )
            .response,
        Response::Success
    );

    write_all_at(&writing_file, b"WXYZ", 2).unwrap();
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::FinishWrite {
                    handle: writing_handle.clone(),
                    write_id: write_id.to_string(),
                    range: ByteRange::new(2, 6).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle: writing_handle,
                    ranges: Vec::new(),
                    durable: true,
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_eq!(fixture.decrypt(&path), b"abWXYZgh");
}

#[test]
fn opening_a_peer_reuses_pending_shared_plaintext_without_forcing_writeback() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("peer-open", b"abcdefgh");
    let (writer, plaintext) = fixture.open(&path, true);
    let write_id = "11111111111111111111111111111111";
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: writer.clone(),
                    write_id: write_id.to_string(),
                    range: ByteRange::new(2, 6).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    write_all_at(&plaintext, b"WXYZ", 2).unwrap();
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::FinishWrite {
                    handle: writer,
                    write_id: write_id.to_string(),
                    range: ByteRange::new(2, 6).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_eq!(fixture.decrypt(&path), b"abcdefgh");

    let (_reader, peer) = fixture.open(&path, false);

    let mut contents = [0_u8; 8];
    read_exact_at(&peer, &mut contents, 0).unwrap();
    assert_eq!(&contents, b"abWXYZgh");
    assert_eq!(fixture.decrypt(&path), b"abcdefgh");
}

#[test]
fn concurrent_syncs_for_different_files_do_not_deadlock() {
    const FILE_COUNT: usize = 16;

    let fixture = Arc::new(Fixture::new());
    let mut handles = Vec::with_capacity(FILE_COUNT);
    for index in 0..FILE_COUNT {
        let path = fixture.encrypted(&format!("independent-{index}"), b"data");
        let (handle, _plaintext) = fixture.open(&path, true);
        handles.push(handle);
    }

    let barrier = Arc::new(Barrier::new(FILE_COUNT + 1));
    let (sender, receiver) = mpsc::channel();
    let mut threads = Vec::with_capacity(FILE_COUNT);
    for handle in handles {
        let fixture = Arc::clone(&fixture);
        let barrier = Arc::clone(&barrier);
        let sender = sender.clone();
        threads.push(thread::spawn(move || {
            barrier.wait();
            let response = fixture
                .broker
                .handle(
                    Request::Sync {
                        handle,
                        ranges: vec![ByteRange::new(0, 1).unwrap()],
                        durable: false,
                    },
                    None,
                )
                .response;
            sender.send(response).unwrap();
        }));
    }
    drop(sender);
    barrier.wait();

    let deadline = Instant::now() + Duration::from_secs(2);
    for _ in 0..FILE_COUNT {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert_eq!(
            receiver
                .recv_timeout(remaining)
                .expect("concurrent local filesystem sync deadlocked"),
            Response::Success
        );
    }
    for thread in threads {
        thread.join().unwrap();
    }
}

#[test]
fn append_reserves_the_current_shared_end_of_file() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("append", b"abcdefgh");
    let (handle, plaintext) = fixture.open(&path, true);
    let write_id = "11111111111111111111111111111111";

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginAppend {
                    handle: handle.clone(),
                    write_id: write_id.to_string(),
                },
                None,
            )
            .response,
        Response::Offset { offset: 8 }
    );
    write_all_at(&plaintext, b"XYZ", 8).unwrap();
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::FinishWrite {
                    handle: handle.clone(),
                    write_id: write_id.to_string(),
                    range: ByteRange::new(8, 11).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginAppend {
                    handle: handle.clone(),
                    write_id: "22222222222222222222222222222222".to_string(),
                },
                None,
            )
            .response,
        Response::Offset { offset: 11 }
    );
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::CancelWrite {
                    handle,
                    write_id: "22222222222222222222222222222222".to_string(),
                },
                None,
            )
            .response,
        Response::Success
    );
}

#[test]
fn final_close_flushes_an_abandoned_active_write() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("abandoned-write", b"abcdefgh");
    let (handle, plaintext) = fixture.open(&path, true);
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: handle.clone(),
                    write_id: "11111111111111111111111111111111".to_string(),
                    range: ByteRange::new(0, u64::MAX).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    write_all_at(&plaintext, b"XYZ", 8).unwrap();

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Close {
                    handle,
                    ranges: Vec::new()
                },
                None
            )
            .response,
        Response::Success
    );

    assert_eq!(fixture.decrypt(&path), b"abcdefghXYZ");
}

#[test]
fn sync_ignores_ranges_beyond_eof_and_reports_plaintext_read_failures() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("range-errors", b"data");
    let (handle, _) = fixture.open(&path, true);

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle: handle.clone(),
                    ranges: vec![ByteRange::new(100, 101).unwrap()],
                    durable: false,
                },
                None,
            )
            .response,
        Response::Success
    );

    {
        let local = lock(&fixture.broker.handles).get(&handle).unwrap().clone();
        let shared = Arc::clone(&lock(&local).shared);
        let mut shared = lock(&shared.inner);
        shared.plaintext = File::open(fixture.root.path()).unwrap();
        shared.baseline = PlaintextIdentity::from_metadata(&shared.plaintext.metadata().unwrap());
    }
    let response = fixture
        .broker
        .handle(
            Request::Sync {
                handle,
                ranges: vec![ByteRange::new(0, 1).unwrap()],
                durable: false,
            },
            None,
        )
        .response;
    assert!(
        matches!(response, Response::Error { message, .. } if message.contains("failed to read local plaintext range"))
    );
}

#[test]
fn broker_protocol_errors_preserve_their_message() {
    let error = BrokerError::protocol_error(anyhow::anyhow!("invalid backing path"));

    assert_eq!(error.errno, libc::EPROTO);
    assert_eq!(error.message, "invalid backing path");
}

#[test]
fn request_cache_replays_and_claims_one_open_handle() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("cached-open", b"data");
    let request = Request::Open {
        path: BackingPath::from_path(&path),
        flags: libc::O_RDWR,
    };
    let first = fixture
        .broker
        .handle_request("open-request".to_string(), request.clone(), None);
    let replay = fixture
        .broker
        .handle_request("open-request".to_string(), request, None);

    assert_eq!(first.response, replay.response);
    assert_eq!(lock(&fixture.broker.handles).len(), 1);
    assert!(matches!(
        fixture
            .broker
            .handle_request(
                "open-request".to_string(),
                Request::Close {
                    handle: "different".to_string(),
                    ranges: Vec::new(),
                },
                None,
            )
            .response,
        Response::Error {
            errno: libc::EPROTO,
            ..
        }
    ));
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Claim {
                    request_id: "open-request".to_string(),
                },
                None,
            )
            .response,
        Response::Success
    );
    if let Some(CachedRequest::Completed { completed_at, .. }) = lock(&fixture.broker.requests)
        .entries
        .get_mut("open-request")
    {
        *completed_at = Instant::now() - REQUEST_CACHE_TTL - Duration::from_secs(1);
    }
    fixture.broker.expire_requests();
    assert_eq!(lock(&fixture.broker.handles).len(), 1);
}

#[test]
fn expired_unclaimed_open_is_aborted_without_writeback() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("abandoned-open", b"data");
    let response = fixture.broker.handle_request(
        "abandoned-request".to_string(),
        Request::Open {
            path: BackingPath::from_path(&path),
            flags: libc::O_RDWR,
        },
        None,
    );
    assert!(matches!(response.response, Response::Open { .. }));
    if let Some(CachedRequest::Completed { completed_at, .. }) = lock(&fixture.broker.requests)
        .entries
        .get_mut("abandoned-request")
    {
        *completed_at = Instant::now() - REQUEST_CACHE_TTL - Duration::from_secs(1);
    }

    fixture.broker.expire_requests();

    assert!(lock(&fixture.broker.handles).is_empty());
}

#[test]
fn final_flush_persists_ranges_registered_by_writable_mappings() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("mapped", b"abcdef");
    let (handle, plaintext) = fixture.open(&path, true);
    write_all_at(&plaintext, b"mapped", 0).unwrap();
    assert_eq!(
        fixture
            .broker
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

    fixture.broker.flush_all().unwrap();

    assert_eq!(fixture.decrypt(&path), b"mapped");
}

#[test]
fn explicit_sync_persists_changes_from_registered_writable_mappings() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("mapped-sync", b"abcdef");
    let (handle, plaintext) = fixture.open(&path, true);
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::PotentiallyDirty {
                    handle: handle.clone(),
                    range: ByteRange::new(0, 6).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    write_all_at(&plaintext, b"mapped", 0).unwrap();

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle,
                    ranges: Vec::new(),
                    durable: true,
                },
                None,
            )
            .response,
        Response::Success
    );

    assert_eq!(fixture.decrypt(&path), b"mapped");
}

#[test]
fn final_flush_abandons_all_peer_writes_before_synchronizing() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("peer-active-final-flush", b"abcdefgh");
    let (first_handle, first_file) = fixture.open(&path, true);
    let (second_handle, second_file) = fixture.open(&path, true);
    let first_flushed = lock(&fixture.broker.handles)
        .keys()
        .next()
        .expect("a live handle exists")
        .clone();
    let (active_handle, active_file) = if first_flushed == first_handle {
        (second_handle, second_file)
    } else {
        (first_handle, first_file)
    };
    let write_id = "11111111111111111111111111111111";
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: active_handle,
                    write_id: write_id.to_string(),
                    range: ByteRange::new(2, 6).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    write_all_at(&active_file, b"WXYZ", 2).unwrap();

    fixture.broker.flush_all().unwrap();

    assert_eq!(fixture.decrypt(&path), b"abWXYZgh");
}

#[test]
fn mapping_registration_survives_an_intermediate_sync_until_close() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("mapped-twice", b"abcdef");
    let (handle, plaintext) = fixture.open(&path, true);
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::PotentiallyDirty {
                    handle: handle.clone(),
                    range: ByteRange::new(0, 6).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    write_all_at(&plaintext, b"first!", 0).unwrap();
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle,
                    ranges: vec![ByteRange::new(0, 6).unwrap()],
                    durable: true,
                },
                None,
            )
            .response,
        Response::Success
    );
    write_all_at(&plaintext, b"second", 0).unwrap();

    fixture.broker.flush_all().unwrap();

    assert_eq!(fixture.decrypt(&path), b"second");
}

#[test]
fn retained_handle_remains_usable_after_one_process_closes_it() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("forked", b"before");
    let (handle, plaintext) = fixture.open(&path, true);

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Retain {
                    handles: vec![handle.clone(), handle.clone()],
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Close {
                    handle: handle.clone(),
                    ranges: Vec::new(),
                },
                None,
            )
            .response,
        Response::Success
    );
    write_all_at(&plaintext, b"after!", 0).unwrap();
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle: handle.clone(),
                    ranges: vec![ByteRange::new(0, 6).unwrap()],
                    durable: true,
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_eq!(fixture.decrypt(&path), b"after!");
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Close {
                    handle,
                    ranges: Vec::new()
                },
                None
            )
            .response,
        Response::Success
    );
}

#[test]
fn failed_fork_retains_can_be_released_atomically() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("failed-fork", b"data");
    let (handle, _plaintext) = fixture.open(&path, true);

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Retain {
                    handles: vec![handle.clone()],
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::ReleaseRetain {
                    handles: vec![handle.clone(), handle.clone()],
                },
                None,
            )
            .response,
        Response::Success
    );
    let local = lock(&fixture.broker.handles).get(&handle).unwrap().clone();
    assert_eq!(lock(&local).references, 1);
}

#[test]
fn broker_rejects_invalid_descriptors_paths_and_handle_operations() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("valid", b"data");
    let (handle, _plaintext) = fixture.open(&path, true);
    let invalid_range = ByteRange { start: 1, end: 1 };
    assert_error(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: handle.clone(),
                    write_id: "11111111111111111111111111111111".to_string(),
                    range: invalid_range,
                },
                None,
            )
            .response,
        libc::EPROTO,
    );
    assert_error(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle: handle.clone(),
                    ranges: vec![invalid_range],
                    durable: false,
                },
                None,
            )
            .response,
        libc::EPROTO,
    );
    assert_error(
        fixture
            .broker
            .handle(
                Request::Materialize {
                    handle,
                    range: Some(invalid_range),
                },
                None,
            )
            .response,
        libc::EPROTO,
    );
    let open_descriptor: OwnedFd = tempfile::tempfile().unwrap().into();
    assert_error(
        fixture
            .broker
            .handle(
                Request::Open {
                    path: BackingPath::from_path(&path),
                    flags: libc::O_RDWR,
                },
                Some(open_descriptor),
            )
            .response,
        libc::EPROTO,
    );
    let sync_descriptor: OwnedFd = tempfile::tempfile().unwrap().into();
    assert_error(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle: "missing".to_string(),
                    ranges: Vec::new(),
                    durable: false,
                },
                Some(sync_descriptor),
            )
            .response,
        libc::EPROTO,
    );
    assert_error(
        fixture
            .broker
            .handle(
                Request::PotentiallyDirty {
                    handle: "missing".to_string(),
                    range: ByteRange::new(1, 2).unwrap(),
                },
                None,
            )
            .response,
        libc::EBADF,
    );
    assert_error(
        fixture
            .broker
            .handle(
                Request::Retain {
                    handles: vec!["missing".to_string()],
                },
                None,
            )
            .response,
        libc::EBADF,
    );
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Close {
                    handle: "missing".to_string(),
                    ranges: Vec::new(),
                },
                None,
            )
            .response,
        Response::Success
    );

    let missing = fixture.root.path().join("missing");
    assert_error(
        fixture
            .broker
            .handle(
                Request::Open {
                    path: BackingPath::from_path(&missing),
                    flags: libc::O_RDWR,
                },
                None,
            )
            .response,
        libc::ENOENT,
    );

    let outside = tempfile::tempdir().unwrap();
    let outside_path = outside.path().join("encrypted");
    let mut outside_plaintext = tempfile::tempfile().unwrap();
    outside_plaintext.write_all(b"data").unwrap();
    fixture
        .cipher
        .encrypt(&mut outside_plaintext, &outside_path)
        .unwrap();
    drop(outside_plaintext);
    assert_error(
        fixture
            .broker
            .handle(
                Request::Open {
                    path: BackingPath::from_path(&outside_path),
                    flags: libc::O_RDWR,
                },
                None,
            )
            .response,
        libc::EACCES,
    );

    assert_error(
        fixture
            .broker
            .handle(
                Request::Open {
                    path: BackingPath::from_path(&path),
                    flags: libc::O_ACCMODE,
                },
                None,
            )
            .response,
        libc::EINVAL,
    );
}

#[test]
fn read_only_handles_reject_dirty_ranges_and_changed_snapshots() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("readonly", b"before");
    let (handle, plaintext) = fixture.open(&path, false);

    assert_error(
        fixture
            .broker
            .handle(
                Request::PotentiallyDirty {
                    handle: handle.clone(),
                    range: ByteRange::new(0, 1).unwrap(),
                },
                None,
            )
            .response,
        libc::EBADF,
    );
    assert_error(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle: handle.clone(),
                    ranges: vec![ByteRange::new(0, 1).unwrap()],
                    durable: false,
                },
                None,
            )
            .response,
        libc::EBADF,
    );

    write_all_at(&plaintext, b"after!", 0).unwrap();
    assert_error(
        fixture
            .broker
            .handle(
                Request::Close {
                    handle,
                    ranges: Vec::new(),
                },
                None,
            )
            .response,
        libc::EBADF,
    );
    assert_eq!(fixture.decrypt(&path), b"before");
}

#[test]
fn final_close_detects_unreported_growth_and_shrinkage() {
    let fixture = Fixture::new();
    let original = vec![b'a'; COPY_BUFFER_SIZE + 17];
    let path = fixture.encrypted("resized", &original);
    let (handle, plaintext) = fixture.open(&path, true);
    let grown = original.len() as u64 + 31;
    plaintext.set_len(grown).unwrap();
    write_all_at(&plaintext, b"tail", grown - 4).unwrap();

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Close {
                    handle: handle.clone(),
                    ranges: Vec::new(),
                },
                None,
            )
            .response,
        Response::Success
    );
    let decrypted = fixture.decrypt(&path);
    assert_eq!(decrypted.len(), grown as usize);
    assert_eq!(&decrypted[grown as usize - 4..], b"tail");

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle: handle.clone(),
                    ranges: vec![ByteRange::new(0, 4).unwrap()],
                    durable: false,
                },
                None,
            )
            .response,
        Response::Success,
        "a lost close response can be retried through an idempotent sync"
    );
    plaintext.set_len(9).unwrap();
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Close {
                    handle,
                    ranges: Vec::new()
                },
                None
            )
            .response,
        Response::Success
    );
    assert_eq!(fixture.decrypt(&path), vec![b'a'; 9]);
}

#[test]
fn dirty_ranges_are_merged_and_expired_handles_are_reclaimed() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("ranges", b"0123456789");
    let (handle, plaintext) = fixture.open(&path, true);
    write_all_at(&plaintext, b"abcdef", 1).unwrap();
    for (start, end) in [(4, 7), (1, 3), (3, 5)] {
        assert_eq!(
            fixture
                .broker
                .handle(
                    Request::PotentiallyDirty {
                        handle: handle.clone(),
                        range: ByteRange::new(start, end).unwrap(),
                    },
                    None,
                )
                .response,
            Response::Success
        );
    }
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Close {
                    handle: handle.clone(),
                    ranges: Vec::new(),
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_eq!(fixture.decrypt(&path), b"0abcdef789");

    let closed = lock(&fixture.broker.handles).get(&handle).unwrap().clone();
    let mut closed = lock(&closed);
    closed.closed_at = Some(Instant::now() - CLOSED_HANDLE_TTL - Duration::from_secs(1));
    drop(closed);
    fixture.broker.expire_closed();
    assert!(!lock(&fixture.broker.handles).contains_key(&handle));
}

#[test]
fn closed_handle_retention_is_bounded_before_the_ttl_expires() {
    const EXPECTED_LIMIT: usize = 128;
    let fixture = Fixture::new();
    let path = fixture.encrypted("bounded", b"data");

    for _ in 0..=EXPECTED_LIMIT {
        let (handle, _plaintext) = fixture.open(&path, false);
        assert_eq!(
            fixture
                .broker
                .handle(
                    Request::Close {
                        handle,
                        ranges: Vec::new()
                    },
                    None
                )
                .response,
            Response::Success
        );
    }

    assert!(lock(&fixture.broker.handles).len() <= EXPECTED_LIMIT);
}

#[test]
fn retain_overflow_is_atomic_and_internal_error_helpers_preserve_context() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("overflow", b"data");
    let (handle, _plaintext) = fixture.open(&path, true);
    let local = lock(&fixture.broker.handles).get(&handle).unwrap().clone();
    lock(&local).references = usize::MAX;

    assert_error(
        fixture
            .broker
            .handle(
                Request::Retain {
                    handles: vec![handle.clone()],
                },
                None,
            )
            .response,
        libc::EOVERFLOW,
    );
    assert_eq!(lock(&local).references, usize::MAX);

    let error = BrokerError::io("context", std::io::Error::other("failure"));
    assert_eq!(error.errno, libc::EIO);
    assert!(error.message.contains("context"));
    let chained = BrokerError::anyhow(
        "encrypt",
        anyhow::Error::new(std::io::Error::from_raw_os_error(libc::ENOSPC)),
    );
    assert_eq!(chained.errno, libc::ENOSPC);
    assert!(chained.into_io().to_string().contains("encrypt"));

    let file = tempfile::tempfile().unwrap();
    assert_eq!(
        read_exact_at(&file, &mut [0_u8; 1], 0).unwrap_err().kind(),
        std::io::ErrorKind::UnexpectedEof
    );
}

#[test]
fn broker_rejects_conflicting_write_ids_and_read_only_write_protocols() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("write-protocol", b"abcdefgh");
    let (read_only, _reader) = fixture.open(&path, false);
    let range = ByteRange::new(1, 3).unwrap();
    let write_id = "11111111111111111111111111111111";

    for request in [
        Request::BeginWrite {
            handle: read_only.clone(),
            write_id: write_id.to_string(),
            range,
        },
        Request::BeginAppend {
            handle: read_only.clone(),
            write_id: write_id.to_string(),
        },
        Request::FinishWrite {
            handle: read_only,
            write_id: write_id.to_string(),
            range,
        },
    ] {
        assert_error(fixture.broker.handle(request, None).response, libc::EBADF);
    }

    let (writable, _writer) = fixture.open(&path, true);
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: writable.clone(),
                    write_id: write_id.to_string(),
                    range,
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: writable.clone(),
                    write_id: write_id.to_string(),
                    range,
                },
                None,
            )
            .response,
        Response::Success,
        "an idempotent retry must retain the original reservation"
    );
    assert_error(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: writable.clone(),
                    write_id: write_id.to_string(),
                    range: ByteRange::new(3, 5).unwrap(),
                },
                None,
            )
            .response,
        libc::EPROTO,
    );
    assert_error(
        fixture
            .broker
            .handle(
                Request::FinishWrite {
                    handle: writable.clone(),
                    write_id: write_id.to_string(),
                    range: ByteRange::new(0, 4).unwrap(),
                },
                None,
            )
            .response,
        libc::EPROTO,
    );
    assert_error(
        fixture
            .broker
            .handle(
                Request::FinishWrite {
                    handle: writable.clone(),
                    write_id: "22222222222222222222222222222222".to_string(),
                    range,
                },
                None,
            )
            .response,
        libc::EPROTO,
    );

    let append_id = "33333333333333333333333333333333";
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginAppend {
                    handle: writable.clone(),
                    write_id: append_id.to_string(),
                },
                None,
            )
            .response,
        Response::Offset { offset: 8 }
    );
    assert_error(
        fixture
            .broker
            .handle(
                Request::BeginAppend {
                    handle: writable.clone(),
                    write_id: append_id.to_string(),
                },
                None,
            )
            .response,
        libc::EPROTO,
    );
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::CancelWrite {
                    handle: writable.clone(),
                    write_id: append_id.to_string(),
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::CancelWrite {
                    handle: writable,
                    write_id: "missing".to_string(),
                },
                None,
            )
            .response,
        Response::Success
    );
}

#[test]
fn abort_and_release_retain_clean_up_only_valid_live_handles() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("abort", b"data");
    let (handle, _writer) = fixture.open(&path, true);
    let write_id = "11111111111111111111111111111111";
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: handle.clone(),
                    write_id: write_id.to_string(),
                    range: ByteRange::new(0, 4).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Abort {
                    handle: handle.clone(),
                },
                None,
            )
            .response,
        Response::Success
    );
    assert!(!lock(&fixture.broker.handles).contains_key(&handle));
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Abort {
                    handle: "missing".to_string(),
                },
                None,
            )
            .response,
        Response::Success
    );
    assert_error(
        fixture
            .broker
            .handle(
                Request::Claim {
                    request_id: "missing".to_string(),
                },
                None,
            )
            .response,
        libc::EPROTO,
    );

    let (released, _reader) = fixture.open(&path, false);
    let local = lock(&fixture.broker.handles)
        .get(&released)
        .unwrap()
        .clone();
    lock(&local).references = 0;
    assert_error(
        fixture
            .broker
            .handle(
                Request::ReleaseRetain {
                    handles: vec![released],
                },
                None,
            )
            .response,
        libc::EBADF,
    );
    assert_error(
        fixture
            .broker
            .handle(
                Request::ReleaseRetain {
                    handles: vec!["missing".to_string()],
                },
                None,
            )
            .response,
        libc::EBADF,
    );
}

#[test]
fn release_retain_marks_the_last_reference_closed_and_expiration_finishes_writes() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("release-retain", b"plaintext");
    let (handle, _content) = fixture.open(&path, true);
    let write_id = "unfinished".to_string();
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::BeginWrite {
                    handle: handle.clone(),
                    write_id: write_id.clone(),
                    range: ByteRange::new(0, 1).unwrap(),
                },
                None,
            )
            .response,
        Response::Success
    );

    assert_eq!(
        fixture
            .broker
            .handle(
                Request::ReleaseRetain {
                    handles: vec![handle.clone()],
                },
                None,
            )
            .response,
        Response::Success
    );
    let local = lock(&fixture.broker.handles).get(&handle).unwrap().clone();
    assert_eq!(lock(&local).references, 0);
    assert!(lock(&local).closed_at.is_some());

    lock(&local).closed_at = Some(Instant::now() - CLOSED_HANDLE_TTL - Duration::from_secs(1));
    fixture.broker.expire_closed();
    assert!(!lock(&fixture.broker.handles).contains_key(&handle));
    let shared = lock(&local).shared.clone();
    let mutations = lock(&shared.mutations);
    assert!(mutations.ordinary.is_empty());
    assert!(mutations.append.is_none());
    assert!(!mutations.exclusive);
}

#[test]
fn truncating_a_live_shared_file_updates_every_descriptor_and_ciphertext() {
    let fixture = Fixture::new();
    let path = fixture.encrypted("live-truncate", b"plaintext");
    let (_first, first) = fixture.open(&path, true);
    let mut reply = fixture.broker.handle(
        Request::Open {
            path: BackingPath::from_path(&path),
            flags: libc::O_RDWR | libc::O_TRUNC,
        },
        None,
    );
    let Response::Open { handle, .. } = reply.response else {
        panic!("unexpected truncate response: {:?}", reply.response);
    };
    let second = reply.descriptors.remove(0);

    assert_eq!(first.metadata().unwrap().len(), 0);
    assert_eq!(second.metadata().unwrap().len(), 0);
    assert_eq!(
        fixture
            .broker
            .handle(
                Request::Sync {
                    handle,
                    ranges: Vec::new(),
                    durable: true,
                },
                None,
            )
            .response,
        Response::Success
    );
    assert!(fixture.decrypt(&path).is_empty());
}
