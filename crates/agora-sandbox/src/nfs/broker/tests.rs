use super::*;
use crate::filesystem::ByteRange;
use crate::nfs::backend::RemoteStorage;
use crate::nfs::protocol::{RemotePath, Request, RequestId, Response};
use crate::nfs::testing::MemoryStorage;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::FileExt;

fn path(value: &str) -> RemotePath {
    RemotePath::new(0, value).unwrap()
}

fn open_handle(response: &Response) -> String {
    let Response::Open { handle, .. } = response else {
        panic!("expected open response, got {response:?}");
    };
    handle.clone()
}

fn request_id(value: u128) -> RequestId {
    RequestId::new(format!("{value:032x}")).unwrap()
}

fn assert_errno(response: Response, errno: libc::c_int) {
    assert!(
        matches!(response, Response::Error { errno: actual, .. } if actual == errno),
        "unexpected response: {response:?}"
    );
}

fn write_payload(contents: &[u8]) -> (OwnedFd, [u8; 16]) {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(contents).unwrap();
    (file.into(), Md5::digest(contents).into())
}

async fn direct_write(
    broker: &Broker<MemoryStorage>,
    handle: &str,
    offset: Option<u64>,
    contents: &[u8],
) -> Response {
    let (descriptor, checksum) = write_payload(contents);
    broker
        .handle_with_descriptor(
            Request::Write {
                handle: handle.to_string(),
                offset,
                length: contents.len() as u32,
                checksum,
            },
            Some(descriptor),
        )
        .await
        .response
}

async fn materialize(broker: &Broker<MemoryStorage>, handle: &str) -> Response {
    broker
        .handle(Request::Materialize {
            handle: handle.to_string(),
            range: None,
        })
        .await
        .response
}

#[tokio::test]
async fn broker_materializes_only_missing_snapshot_ranges() {
    let root = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    storage.insert_file(0, "large.bin", b"0123456789abcdef");
    let broker = Broker::new(Arc::clone(&storage), root.path()).unwrap();
    let opened = broker
        .handle(Request::Open {
            path: path("large.bin"),
            flags: libc::O_RDONLY,
            mode: 0,
        })
        .await;
    let handle = open_handle(&opened.response);
    let snapshot = File::from(opened.descriptor.unwrap());

    let response = broker
        .handle(Request::Materialize {
            handle: handle.clone(),
            range: Some(ByteRange::new(4, 8).unwrap()),
        })
        .await
        .response;
    assert!(matches!(response, Response::Materialized { .. }));
    let mut contents = [0_u8; 12];
    snapshot.read_exact_at(&mut contents, 0).unwrap();
    assert_eq!(&contents[..4], &[0; 4]);
    assert_eq!(&contents[4..8], b"4567");
    assert_eq!(&contents[8..], &[0; 4]);

    let response = broker
        .handle(Request::Materialize {
            handle: handle.clone(),
            range: Some(ByteRange::new(6, 12).unwrap()),
        })
        .await
        .response;
    assert!(matches!(response, Response::Materialized { .. }));
    snapshot.read_exact_at(&mut contents, 0).unwrap();
    assert_eq!(&contents[4..12], b"456789ab");
    assert_eq!(
        storage.read_ranges(),
        vec![
            ByteRange { start: 4, end: 8 },
            ByteRange { start: 8, end: 12 },
        ]
    );
}

#[tokio::test]
async fn broker_partial_snapshot_includes_direct_writes_from_the_same_handle() {
    let root = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    storage.insert_file(0, "mixed.bin", b"original");
    let broker = Broker::new(Arc::clone(&storage), root.path()).unwrap();
    let opened = broker
        .handle(Request::Open {
            path: path("mixed.bin"),
            flags: libc::O_RDWR,
            mode: 0,
        })
        .await;
    let handle = open_handle(&opened.response);
    let snapshot = File::from(opened.descriptor.unwrap());
    assert!(matches!(
        direct_write(&broker, &handle, Some(0), b"sandbox").await,
        Response::Written { .. }
    ));

    let response = broker
        .handle(Request::Materialize {
            handle,
            range: Some(ByteRange::new(0, 7).unwrap()),
        })
        .await
        .response;

    assert!(matches!(response, Response::Materialized { .. }));
    let mut contents = [0_u8; 7];
    snapshot.read_exact_at(&mut contents, 0).unwrap();
    assert_eq!(&contents, b"sandbox");
}

#[tokio::test]
async fn broker_rejects_a_version_change_during_partial_materialization() {
    let root = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    storage.insert_file(0, "changing.bin", b"old contents");
    storage.replace_during_snapshot_read(b"new contents");
    let broker = Broker::new(Arc::clone(&storage), root.path()).unwrap();
    let opened = broker
        .handle(Request::Open {
            path: path("changing.bin"),
            flags: libc::O_RDONLY,
            mode: 0,
        })
        .await;
    let handle = open_handle(&opened.response);

    let response = broker
        .handle(Request::Materialize {
            handle: handle.clone(),
            range: Some(ByteRange::new(0, 4).unwrap()),
        })
        .await
        .response;

    assert_errno(response, libc::ESTALE);
    let response = broker
        .handle(Request::Materialize {
            handle,
            range: Some(ByteRange::new(0, 4).unwrap()),
        })
        .await
        .response;
    assert_errno(response, libc::ESTALE);
}

#[tokio::test]
async fn broker_rejects_invalid_snapshot_ranges() {
    let root = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    storage.insert_file(0, "ranges.bin", b"contents");
    let broker = Broker::new(storage, root.path()).unwrap();
    let opened = broker
        .handle(Request::Open {
            path: path("ranges.bin"),
            flags: libc::O_RDWR,
            mode: 0,
        })
        .await;
    let handle = open_handle(&opened.response);

    let response = broker
        .handle(Request::Materialize {
            handle: handle.clone(),
            range: Some(ByteRange { start: 4, end: 4 }),
        })
        .await
        .response;
    assert_errno(response, libc::EINVAL);

    let response = broker
        .handle(Request::Sync {
            handle,
            ranges: vec![ByteRange { start: 8, end: 2 }],
        })
        .await
        .response;
    assert_errno(response, libc::EINVAL);
}

#[tokio::test]
async fn broker_fills_only_missing_baseline_ranges_before_snapshot_writeback() {
    let root = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    storage.insert_file(0, "edited.bin", b"0123456789abcdef");
    let broker = Broker::new(Arc::clone(&storage), root.path()).unwrap();
    let opened = broker
        .handle(Request::Open {
            path: path("edited.bin"),
            flags: libc::O_RDWR,
            mode: 0,
        })
        .await;
    let handle = open_handle(&opened.response);
    let snapshot = File::from(opened.descriptor.unwrap());
    assert!(matches!(
        broker
            .handle(Request::Materialize {
                handle: handle.clone(),
                range: Some(ByteRange::new(4, 8).unwrap()),
            })
            .await
            .response,
        Response::Materialized { .. }
    ));
    snapshot.write_all_at(b"XX", 5).unwrap();

    let response = broker
        .handle(Request::Sync {
            handle,
            ranges: vec![ByteRange::new(5, 7).unwrap()],
        })
        .await
        .response;

    assert!(matches!(response, Response::Synced { .. }));
    assert_eq!(storage.data(0, "edited.bin").unwrap(), b"01234XX789abcdef");
    assert_eq!(
        storage.read_ranges(),
        vec![
            ByteRange { start: 4, end: 8 },
            ByteRange { start: 0, end: 4 },
            ByteRange { start: 8, end: 16 },
        ]
    );
}

#[tokio::test]
async fn broker_full_materialization_does_not_rebaseline_dirty_partial_snapshot_bytes() {
    let root = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    storage.insert_file(0, "mapped.bin", b"0123456789abcdef");
    let broker = Broker::new(Arc::clone(&storage), root.path()).unwrap();
    let opened = broker
        .handle(Request::Open {
            path: path("mapped.bin"),
            flags: libc::O_RDWR,
            mode: 0,
        })
        .await;
    let handle = open_handle(&opened.response);
    let snapshot = File::from(opened.descriptor.unwrap());
    assert!(matches!(
        broker
            .handle(Request::Materialize {
                handle: handle.clone(),
                range: Some(ByteRange::new(4, 8).unwrap()),
            })
            .await
            .response,
        Response::Materialized { .. }
    ));
    snapshot.write_all_at(b"XX", 5).unwrap();
    assert!(matches!(
        materialize(&broker, &handle).await,
        Response::Materialized { .. }
    ));

    let response = broker
        .handle(Request::Sync {
            handle,
            ranges: vec![ByteRange::new(5, 7).unwrap()],
        })
        .await
        .response;

    assert!(matches!(response, Response::Synced { .. }));
    assert_eq!(storage.data(0, "mapped.bin").unwrap(), b"01234XX789abcdef");
}

#[tokio::test]
async fn broker_closes_a_clean_partial_snapshot_without_downloading_the_remainder() {
    let root = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    storage.insert_file(0, "private.bin", b"0123456789abcdef");
    let broker = Broker::new(Arc::clone(&storage), root.path()).unwrap();
    let opened = broker
        .handle(Request::Open {
            path: path("private.bin"),
            flags: libc::O_RDWR,
            mode: 0,
        })
        .await;
    let handle = open_handle(&opened.response);
    assert!(matches!(
        broker
            .handle(Request::Materialize {
                handle: handle.clone(),
                range: Some(ByteRange::new(4, 8).unwrap()),
            })
            .await
            .response,
        Response::Materialized { .. }
    ));

    let response = broker
        .handle(Request::Close {
            handle,
            ranges: Vec::new(),
        })
        .await
        .response;

    assert_eq!(response, Response::Success);
    assert_eq!(storage.read_ranges(), vec![ByteRange { start: 4, end: 8 }]);
    assert_eq!(storage.data(0, "private.bin").unwrap(), b"0123456789abcdef");
}

#[tokio::test]
async fn broker_closes_a_changed_writable_mapping_without_explicit_dirty_ranges() {
    let root = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemoryStorage::default());
    storage.insert_file(0, "mapped.bin", b"0123456789abcdef");
    let broker = Broker::new(Arc::clone(&storage), root.path()).unwrap();
    let opened = broker
        .handle(Request::Open {
            path: path("mapped.bin"),
            flags: libc::O_RDWR,
            mode: 0,
        })
        .await;
    let handle = open_handle(&opened.response);
    let snapshot = File::from(opened.descriptor.unwrap());
    let mapped = ByteRange::new(4, 8).unwrap();
    assert!(matches!(
        broker
            .handle(Request::Materialize {
                handle: handle.clone(),
                range: Some(mapped),
            })
            .await
            .response,
        Response::Materialized { .. }
    ));
    assert_eq!(
        broker
            .handle(Request::PotentiallyDirty {
                handle: handle.clone(),
                range: mapped,
            })
            .await
            .response,
        Response::Success
    );
    snapshot.write_all_at(b"XX", 5).unwrap();

    let response = broker
        .handle(Request::Close {
            handle,
            ranges: Vec::new(),
        })
        .await
        .response;

    assert_eq!(response, Response::Success);
    assert_eq!(storage.data(0, "mapped.bin").unwrap(), b"01234XX789abcdef");
}

#[tokio::test]
async fn checksum_honors_its_cpu_time_budget() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"data").unwrap();

    let error = checksum_file(file, Instant::now()).await.unwrap_err();

    assert_eq!(error.errno(), libc::ETIMEDOUT);
}

#[tokio::test(flavor = "current_thread")]
async fn checksum_does_not_block_the_async_runtime_worker() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let file = tempfile::tempfile().unwrap();
    file.set_len(8 * 1024 * 1024).unwrap();
    let completed = std::sync::Arc::new(AtomicBool::new(false));
    let observed = std::sync::Arc::clone(&completed);

    let (checksum, ()) = tokio::join!(
        async {
            let checksum = checksum_file(file, Instant::now() + Duration::from_secs(1)).await;
            completed.store(true, Ordering::Release);
            checksum
        },
        async {
            tokio::task::yield_now().await;
            assert!(
                !observed.load(Ordering::Acquire),
                "checksum work must execute outside the async runtime worker"
            );
        }
    );

    checksum.unwrap();
}

#[tokio::test]
async fn broker_replays_duplicate_open_without_allocating_another_handle() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "notes.txt", b"remote contents");
    let broker = Broker::new(storage, root.path()).unwrap();
    let request = Request::Open {
        path: path("notes.txt"),
        flags: libc::O_RDONLY,
        mode: 0,
    };

    let first = broker.handle_request(request_id(1), request.clone()).await;
    let replay = broker.handle_request(request_id(1), request).await;

    assert_eq!(open_handle(&first.response), open_handle(&replay.response));
    assert!(first.descriptor.is_some());
    assert!(replay.descriptor.is_some());
    assert_eq!(broker.handle_count_for_test().await, 1);
}

#[tokio::test]
async fn broker_treats_close_as_idempotent_across_request_ids() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "notes.txt", b"remote contents");
    let broker = Broker::new(storage, root.path()).unwrap();
    let opened = broker
        .handle_request(
            request_id(2),
            Request::Open {
                path: path("notes.txt"),
                flags: libc::O_RDONLY,
                mode: 0,
            },
        )
        .await;
    let handle = open_handle(&opened.response);

    assert_eq!(
        broker
            .handle_request(
                request_id(3),
                Request::Close {
                    handle: handle.clone(),
                    ranges: Vec::new()
                },
            )
            .await
            .response,
        Response::Success
    );
    assert_eq!(
        broker
            .handle_request(
                request_id(4),
                Request::Close {
                    handle,
                    ranges: Vec::new()
                }
            )
            .await
            .response,
        Response::Success
    );
}

#[tokio::test]
async fn broker_rejects_reusing_a_request_id_for_a_different_operation() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "one.txt", b"one");
    storage.insert_file(0, "two.txt", b"two");
    let broker = Broker::new(storage, root.path()).unwrap();

    let _ = broker
        .handle_request(
            request_id(5),
            Request::Stat {
                path: path("one.txt"),
                name_capacity: 0,
            },
        )
        .await;
    let response = broker
        .handle_request(
            request_id(5),
            Request::Stat {
                path: path("two.txt"),
                name_capacity: 0,
            },
        )
        .await
        .response;

    assert!(matches!(
        response,
        Response::Error {
            errno: libc::EPROTO,
            ..
        }
    ));
}

#[tokio::test]
async fn broker_reads_remote_content_on_demand_through_a_payload_descriptor() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "notes.txt", b"remote contents");
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();

    let reply = broker
        .handle(Request::Open {
            path: path("notes.txt"),
            flags: libc::O_RDONLY,
            mode: 0,
        })
        .await;

    let handle = open_handle(&reply.response);
    let placeholder = std::fs::File::from(reply.descriptor.unwrap());
    assert_eq!(placeholder.metadata().unwrap().len(), 15);
    let read = broker
        .handle(Request::Read {
            handle: handle.clone(),
            offset: 0,
            length: 64,
        })
        .await;
    assert!(matches!(read.response, Response::Read { length: 15, .. }));
    let mut file = std::fs::File::from(read.descriptor.unwrap());
    let mut data = String::new();
    file.read_to_string(&mut data).unwrap();
    assert_eq!(data, "remote contents");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    assert_eq!(
        broker
            .handle(Request::Close {
                handle,
                ranges: Vec::new()
            })
            .await
            .response,
        Response::Success
    );
}

#[tokio::test]
async fn broker_open_does_not_download_remote_file_contents() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "large.bin", b"remote contents");
    storage.block_reads();
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();

    let opened = tokio::time::timeout(
        Duration::from_millis(100),
        broker.handle(Request::Open {
            path: path("large.bin"),
            flags: libc::O_RDONLY,
            mode: 0,
        }),
    )
    .await
    .expect("ordinary open must not wait for a whole-file download");

    assert!(matches!(opened.response, Response::Open { .. }));
}

#[tokio::test]
async fn broker_rejects_a_snapshot_changed_during_download() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "changing.txt", b"before");
    storage.replace_during_snapshot_read(b"after");
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();
    let reply = broker
        .handle(Request::Open {
            path: path("changing.txt"),
            flags: libc::O_RDONLY,
            mode: 0,
        })
        .await;
    let handle = open_handle(&reply.response);

    let response = materialize(&broker, &handle).await;

    assert_errno(response, libc::ESTALE);
}

#[tokio::test]
async fn broker_applies_the_snapshot_limit_only_when_mmap_materializes_the_file() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "large.bin", b"12345");
    let broker = Broker::new_with_limits(
        std::sync::Arc::clone(&storage),
        root.path(),
        RemoteLimits {
            max_file_bytes: 4,
            ..RemoteLimits::default()
        },
    )
    .unwrap();

    let opened = broker
        .handle(Request::Open {
            path: path("large.bin"),
            flags: libc::O_RDONLY,
            mode: 0,
        })
        .await;
    let handle = open_handle(&opened.response);
    assert!(matches!(opened.response, Response::Open { .. }));
    let response = materialize(&broker, &handle).await;

    assert_errno(response, libc::EFBIG);
    assert_eq!(broker.handle_count_for_test().await, 1);
}

#[tokio::test]
async fn broker_rejects_grown_snapshots_before_checksum_or_upload() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    let broker = Broker::new_with_limits(
        std::sync::Arc::clone(&storage),
        root.path(),
        RemoteLimits {
            max_file_bytes: 4,
            ..RemoteLimits::default()
        },
    )
    .unwrap();
    let mut opened = broker
        .handle(Request::Open {
            path: path("large.bin"),
            flags: libc::O_RDWR | libc::O_CREAT,
            mode: 0o600,
        })
        .await;
    let handle = open_handle(&opened.response);
    let mut descriptor = std::fs::File::from(opened.descriptor.take().unwrap());
    assert!(matches!(
        materialize(&broker, &handle).await,
        Response::Materialized { .. }
    ));
    descriptor.write_all(b"12345").unwrap();

    let response = broker
        .handle(Request::Sync {
            handle,
            ranges: Vec::new(),
        })
        .await
        .response;

    assert_errno(response, libc::EFBIG);
    assert_eq!(storage.data(0, "large.bin"), Some(Vec::new()));
}

#[tokio::test]
async fn broker_bounds_the_total_remote_operation_duration() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "blocked.txt", b"data");
    storage.block_stats();
    storage.block_resets();
    let broker = Broker::new_with_limits(
        std::sync::Arc::clone(&storage),
        root.path(),
        RemoteLimits {
            operation_timeout: Duration::from_millis(10),
            reset_timeout: Duration::from_millis(10),
            ..RemoteLimits::default()
        },
    )
    .unwrap();

    let outcome = tokio::time::timeout(
        Duration::from_millis(100),
        broker.handle(Request::Stat {
            path: path("blocked.txt"),
            name_capacity: 0,
        }),
    )
    .await;
    storage.release_stats();
    storage.release_resets();
    let response = outcome
        .expect("the Broker must enforce its own operation deadline")
        .response;

    assert_errno(response, libc::ETIMEDOUT);
    assert_eq!(storage.reset_operations(), 1);
}

#[tokio::test]
async fn broker_writes_on_sync_and_last_close_with_posix_open_flags() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();
    let mut reply = broker
        .handle(Request::Open {
            path: path("created.txt"),
            flags: libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            mode: 0o640,
        })
        .await;
    let handle = open_handle(&reply.response);
    let _file = std::fs::File::from(reply.descriptor.take().unwrap());
    assert!(matches!(
        direct_write(&broker, &handle, Some(0), b"first").await,
        Response::Written { length: 5, .. }
    ));
    assert_eq!(storage.data(0, "created.txt").unwrap(), b"first");

    assert!(matches!(
        broker
            .handle(Request::Sync {
                handle: handle.clone(),
                ranges: Vec::new()
            })
            .await
            .response,
        Response::Synced { metadata: Some(_) }
    ));
    assert_eq!(storage.data(0, "created.txt").unwrap(), b"first");
    assert!(matches!(
        direct_write(&broker, &handle, Some(5), b" second").await,
        Response::Written { length: 7, .. }
    ));
    assert_eq!(
        broker
            .handle(Request::Close {
                handle,
                ranges: Vec::new()
            })
            .await
            .response,
        Response::Success
    );
    assert_eq!(storage.data(0, "created.txt").unwrap(), b"first second");

    let exclusive = broker
        .handle(Request::Open {
            path: path("created.txt"),
            flags: libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            mode: 0o600,
        })
        .await;
    assert!(matches!(
        exclusive.response,
        Response::Error {
            errno: libc::EEXIST,
            ..
        }
    ));
}

#[tokio::test]
async fn broker_refuses_to_overwrite_a_remotely_changed_file() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "shared.txt", b"original");
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();
    let mut reply = broker
        .handle(Request::Open {
            path: path("shared.txt"),
            flags: libc::O_RDWR | libc::O_TRUNC,
            mode: 0,
        })
        .await;
    let handle = open_handle(&reply.response);
    let mut file = std::fs::File::from(reply.descriptor.take().unwrap());
    assert!(matches!(
        broker
            .handle(Request::Sync {
                handle: handle.clone(),
                ranges: Vec::new()
            })
            .await
            .response,
        Response::Synced { .. }
    ));
    assert!(matches!(
        materialize(&broker, &handle).await,
        Response::Materialized { .. }
    ));
    file.write_all(b"sandbox change").unwrap();
    file.sync_all().unwrap();
    storage.replace(0, "shared.txt", b"outside change");

    let response = broker
        .handle(Request::Sync {
            handle,
            ranges: Vec::new(),
        })
        .await
        .response;

    assert!(matches!(
        response,
        Response::Error {
            errno: libc::ESTALE,
            ..
        }
    ));
    assert_eq!(storage.data(0, "shared.txt").unwrap(), b"outside change");
}

#[tokio::test]
async fn storage_compare_and_write_is_atomic_with_respect_to_its_version() {
    let storage = MemoryStorage::default();
    storage.insert_file(0, "shared.txt", b"original");
    let expected = storage.stat(&path("shared.txt")).await.unwrap();
    storage.replace(0, "shared.txt", b"outside change");
    let mut source = tempfile::tempfile().unwrap();
    source.write_all(b"sandbox change").unwrap();
    source.seek(SeekFrom::Start(0)).unwrap();

    let error = storage
        .write_from_if_unchanged(&path("shared.txt"), Some(&expected), &mut source, 14)
        .await
        .unwrap_err();

    assert_eq!(error.errno(), libc::ESTALE);
    assert_eq!(storage.data(0, "shared.txt").unwrap(), b"outside change");
}

#[tokio::test(flavor = "current_thread")]
async fn broker_serializes_version_check_and_writeback_per_root() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "shared.txt", b"0");
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();

    let mut first = broker
        .handle(Request::Open {
            path: path("shared.txt"),
            flags: libc::O_RDWR,
            mode: 0,
        })
        .await;
    let first_handle = open_handle(&first.response);
    let mut first_file = std::fs::File::from(first.descriptor.take().unwrap());
    assert!(matches!(
        materialize(&broker, &first_handle).await,
        Response::Materialized { .. }
    ));
    first_file.write_all(b"1").unwrap();

    let mut second = broker
        .handle(Request::Open {
            path: path("shared.txt"),
            flags: libc::O_RDWR,
            mode: 0,
        })
        .await;
    let second_handle = open_handle(&second.response);
    let mut second_file = std::fs::File::from(second.descriptor.take().unwrap());
    assert!(matches!(
        materialize(&broker, &second_handle).await,
        Response::Materialized { .. }
    ));
    second_file.write_all(b"2").unwrap();
    storage.yield_operations();

    let (first, second) = tokio::join!(
        broker.handle(Request::Sync {
            handle: first_handle,
            ranges: Vec::new()
        }),
        broker.handle(Request::Sync {
            handle: second_handle,
            ranges: Vec::new()
        }),
    );

    let responses = [first.response, second.response];
    assert_eq!(
        responses
            .iter()
            .filter(|response| matches!(response, Response::Synced { metadata: Some(_) }))
            .count(),
        1
    );
    assert_eq!(
        responses
            .iter()
            .filter(|response| matches!(
                response,
                Response::Error {
                    errno: libc::ESTALE,
                    ..
                }
            ))
            .count(),
        1
    );
}

#[tokio::test]
async fn broker_registers_a_materialized_snapshot_before_namespace_mutation() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "shared.txt", b"original");
    let broker =
        std::sync::Arc::new(Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap());
    let opened = broker
        .handle(Request::Open {
            path: path("shared.txt"),
            flags: libc::O_RDWR,
            mode: 0,
        })
        .await;
    let handle = open_handle(&opened.response);
    storage.block_reads();
    let materializing = {
        let broker = std::sync::Arc::clone(&broker);
        tokio::spawn(async move {
            broker
                .handle(Request::Materialize {
                    handle,
                    range: None,
                })
                .await
        })
    };
    storage.wait_until_read_started().await;

    let removing = {
        let broker = std::sync::Arc::clone(&broker);
        tokio::spawn(async move {
            broker
                .handle(Request::Remove {
                    path: path("shared.txt"),
                    directory: false,
                })
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        !removing.is_finished(),
        "remove must wait until mmap materialization has registered its snapshot"
    );

    storage.release_reads();
    assert!(matches!(
        materializing.await.unwrap().response,
        Response::Materialized { .. }
    ));
    assert_eq!(removing.await.unwrap().response, Response::Success);
}

#[tokio::test]
async fn broker_does_not_publish_an_unchanged_writable_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "shared.txt", b"original");
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();
    let reply = broker
        .handle(Request::Open {
            path: path("shared.txt"),
            flags: libc::O_RDWR,
            mode: 0,
        })
        .await;
    let handle = open_handle(&reply.response);
    let file = std::fs::File::from(reply.descriptor.unwrap());
    storage.replace(0, "shared.txt", b"outside change");

    let response = broker
        .handle(Request::Close {
            handle,
            ranges: Vec::new(),
        })
        .await
        .response;

    assert_eq!(response, Response::Success);
    assert_eq!(storage.data(0, "shared.txt").unwrap(), b"outside change");
    drop(file);
}

#[tokio::test]
async fn broker_publishes_an_empty_file_created_with_read_only_access() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();
    let reply = broker
        .handle(Request::Open {
            path: path("empty.txt"),
            flags: libc::O_RDONLY | libc::O_CREAT,
            mode: 0o600,
        })
        .await;
    let handle = open_handle(&reply.response);
    drop(reply.descriptor.unwrap());

    let response = broker
        .handle(Request::Close {
            handle,
            ranges: Vec::new(),
        })
        .await
        .response;

    assert_eq!(response, Response::Success);
    assert_eq!(storage.data(0, "empty.txt"), Some(Vec::new()));
}

#[tokio::test]
async fn broker_abort_discards_a_staged_create_without_publishing_it() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();
    let reply = broker
        .handle(Request::Open {
            path: path("aborted.txt"),
            flags: libc::O_WRONLY | libc::O_CREAT,
            mode: 0o600,
        })
        .await;
    let handle = open_handle(&reply.response);
    drop(reply.descriptor.unwrap());

    assert_eq!(
        broker.handle(Request::Abort { handle }).await.response,
        Response::Success
    );
    assert!(!storage.exists(0, "aborted.txt"));
}

#[tokio::test]
async fn broker_handles_directory_and_namespace_operations() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_directory(0, "");
    storage.insert_directory(0, "docs");
    storage.insert_file(0, "docs/a.txt", b"a");
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();

    let list = broker
        .handle(Request::List {
            path: path("docs"),
            name_capacity: 80,
        })
        .await;
    let Response::List { anchor } = list.response else {
        panic!("expected list response");
    };
    let entries: Vec<crate::nfs::protocol::RemoteEntry> =
        crate::nfs::client::decode_json_descriptor(
            list.descriptor
                .expect("list response must include a descriptor"),
        )
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "a.txt");
    assert!(!anchor.contains("docs"));
    assert!(anchor.len() >= 80);
    assert!(root.path().join(anchor).is_dir());
    assert_eq!(
        broker
            .handle(Request::Rename {
                from: path("docs/a.txt"),
                to: path("docs/b.txt"),
            })
            .await
            .response,
        Response::Success
    );
    assert_eq!(
        broker
            .handle(Request::Remove {
                path: path("docs/b.txt"),
                directory: false,
            })
            .await
            .response,
        Response::Success
    );
    assert_eq!(
        broker
            .handle(Request::CreateDirectory {
                path: path("empty"),
                mode: 0o755,
            })
            .await
            .response,
        Response::Success
    );
    assert_eq!(
        broker
            .handle(Request::Remove {
                path: path("empty"),
                directory: true,
            })
            .await
            .response,
        Response::Success
    );
}

#[tokio::test]
async fn broker_streams_large_directory_lists_outside_the_control_frame() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_directory(0, "large");
    for index in 0..6_000 {
        storage.insert_file(0, &format!("large/{index:05}-{}", "x".repeat(180)), b"");
    }
    let broker = Broker::new(storage, root.path()).unwrap();

    let request_id = request_id(250);
    let request = Request::List {
        path: path("large"),
        name_capacity: 0,
    };
    let reply = broker
        .handle_request(request_id.clone(), request.clone())
        .await;

    assert!(serde_json::to_vec(&reply.response).unwrap().len() < crate::ipc::MAX_FRAME_SIZE);
    let response = reply.response.clone();
    let descriptor = reply
        .descriptor
        .expect("large directory list must use a bulk descriptor");
    let entries: Vec<crate::nfs::protocol::RemoteEntry> =
        crate::nfs::client::decode_json_descriptor(descriptor).unwrap();
    assert_eq!(entries.len(), 6_000);
    assert!(entries.iter().all(|entry| entry.name.len() == 186));

    let replay = broker.handle_request(request_id.clone(), request).await;
    assert_eq!(replay.response, response);
    let replayed: Vec<crate::nfs::protocol::RemoteEntry> =
        crate::nfs::client::decode_json_descriptor(
            replay
                .descriptor
                .expect("a replayed list must include a fresh descriptor"),
        )
        .unwrap();
    assert_eq!(replayed, entries);

    assert_eq!(
        broker.handle(Request::Claim { request_id }).await.response,
        Response::Success
    );
    let Response::List { anchor } = response else {
        panic!("expected list response");
    };
    assert!(!broker.list_payloads.lock().await.contains_key(&anchor));
}

#[tokio::test]
async fn broker_rejects_directory_lists_above_the_entry_limit() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_directory(0, "large");
    for name in ["a", "b", "c"] {
        storage.insert_file(0, &format!("large/{name}"), b"");
    }
    let broker = Broker::new_with_limits(
        std::sync::Arc::clone(&storage),
        root.path(),
        RemoteLimits {
            max_directory_entries: 2,
            ..RemoteLimits::default()
        },
    )
    .unwrap();

    let response = broker
        .handle(Request::List {
            path: path("large"),
            name_capacity: 0,
        })
        .await
        .response;

    assert_errno(response, libc::EOVERFLOW);
    assert!(
        storage.list_visits() <= 3,
        "the backend must stop after observing max_entries + 1 entries"
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn broker_stops_serializing_directory_lists_at_the_payload_limit() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_directory(0, "large");
    storage.insert_file(0, "large/entry", b"");
    let broker = Broker::new_with_limits(
        storage,
        root.path(),
        RemoteLimits {
            max_directory_payload_bytes: 16,
            ..RemoteLimits::default()
        },
    )
    .unwrap();

    let response = broker
        .handle(Request::List {
            path: path("large"),
            name_capacity: 0,
        })
        .await
        .response;

    assert_errno(response, libc::EOVERFLOW);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn broker_rename_replaces_an_existing_file() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "source.txt", b"source");
    storage.insert_file(0, "target.txt", b"target");
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();

    let response = broker
        .handle(Request::Rename {
            from: path("source.txt"),
            to: path("target.txt"),
        })
        .await
        .response;

    assert_eq!(response, Response::Success);
    assert!(!storage.exists(0, "source.txt"));
    assert_eq!(storage.data(0, "target.txt"), Some(b"source".to_vec()));
}

#[tokio::test]
async fn replaced_target_handle_cannot_recreate_or_overwrite_the_new_target() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "source.txt", b"source");
    storage.insert_file(0, "target.txt", b"target");
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();
    let mut opened = broker
        .handle(Request::Open {
            path: path("target.txt"),
            flags: libc::O_RDWR,
            mode: 0,
        })
        .await;
    let handle = open_handle(&opened.response);
    let mut descriptor = std::fs::File::from(opened.descriptor.take().unwrap());
    descriptor.write_all(b"changed target").unwrap();

    assert_eq!(
        broker
            .handle(Request::Rename {
                from: path("source.txt"),
                to: path("target.txt"),
            })
            .await
            .response,
        Response::Success
    );
    let flushes_before_sync = storage.flush_operations();
    assert_eq!(
        broker
            .handle(Request::Sync {
                handle: handle.clone(),
                ranges: Vec::new()
            })
            .await
            .response,
        Response::Synced { metadata: None }
    );
    assert_eq!(storage.flush_operations(), flushes_before_sync + 1);
    assert_eq!(
        broker
            .handle(Request::Close {
                handle,
                ranges: Vec::new()
            })
            .await
            .response,
        Response::Success
    );
    assert_eq!(storage.data(0, "target.txt"), Some(b"source".to_vec()));
}

#[tokio::test]
async fn broker_protects_the_configured_remote_root_from_namespace_mutation() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_directory(0, "");
    storage.insert_file(0, "file.txt", b"file");
    let broker = Broker::new(storage, root.path()).unwrap();

    assert!(matches!(
        broker
            .handle(Request::Remove {
                path: path(""),
                directory: true,
            })
            .await
            .response,
        Response::Error {
            errno: libc::EACCES,
            ..
        }
    ));

    for request in [
        Request::Rename {
            from: path(""),
            to: path("moved"),
        },
        Request::Rename {
            from: path("file.txt"),
            to: path(""),
        },
    ] {
        assert!(matches!(
            broker.handle(request).await.response,
            Response::Error {
                errno: libc::EBUSY,
                ..
            }
        ));
    }
    assert!(matches!(
        broker
            .handle(Request::CreateDirectory {
                path: path(""),
                mode: 0o755,
            })
            .await
            .response,
        Response::Error {
            errno: libc::EEXIST,
            ..
        }
    ));
}

#[tokio::test]
async fn broker_retargets_open_descendants_when_a_directory_is_renamed() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_directory(0, "");
    storage.insert_directory(0, "docs");
    storage.insert_file(0, "docs/open.txt", b"old");
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();
    let reply = broker
        .handle(Request::Open {
            path: path("docs/open.txt"),
            flags: libc::O_RDWR | libc::O_TRUNC,
            mode: 0,
        })
        .await;
    let handle = open_handle(&reply.response);
    drop(reply.descriptor.unwrap());
    assert!(matches!(
        broker
            .handle(Request::Sync {
                handle: handle.clone(),
                ranges: Vec::new()
            })
            .await
            .response,
        Response::Synced { .. }
    ));
    assert!(matches!(
        direct_write(&broker, &handle, Some(0), b"new").await,
        Response::Written { .. }
    ));

    assert_eq!(
        broker
            .handle(Request::Rename {
                from: path("docs"),
                to: path("renamed"),
            })
            .await
            .response,
        Response::Success
    );
    assert_eq!(
        broker
            .handle(Request::Close {
                handle,
                ranges: Vec::new()
            })
            .await
            .response,
        Response::Success
    );
    assert_eq!(storage.data(0, "renamed/open.txt"), Some(b"new".to_vec()));
    assert!(!storage.exists(0, "docs/open.txt"));
}

#[tokio::test]
async fn broker_discards_an_open_snapshot_after_the_path_is_removed() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_file(0, "open.txt", b"old");
    let broker = Broker::new(std::sync::Arc::clone(&storage), root.path()).unwrap();
    let reply = broker
        .handle(Request::Open {
            path: path("open.txt"),
            flags: libc::O_RDWR | libc::O_TRUNC,
            mode: 0,
        })
        .await;
    let handle = open_handle(&reply.response);
    let mut file = std::fs::File::from(reply.descriptor.unwrap());
    assert!(matches!(
        broker
            .handle(Request::Sync {
                handle: handle.clone(),
                ranges: Vec::new()
            })
            .await
            .response,
        Response::Synced { .. }
    ));
    file.write_all(b"unlinked data").unwrap();

    assert_eq!(
        broker
            .handle(Request::Remove {
                path: path("open.txt"),
                directory: false,
            })
            .await
            .response,
        Response::Success
    );
    assert_eq!(
        broker
            .handle(Request::Sync {
                handle: handle.clone(),
                ranges: Vec::new()
            })
            .await
            .response,
        Response::Synced { metadata: None }
    );
    assert_eq!(
        broker
            .handle(Request::Close {
                handle,
                ranges: Vec::new()
            })
            .await
            .response,
        Response::Success
    );
    assert!(!storage.exists(0, "open.txt"));
}

#[tokio::test]
async fn broker_validates_open_access_directory_and_sync_semantics() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_directory(0, "");
    storage.insert_directory(0, "docs");
    storage.insert_file(0, "file.txt", b"file");
    let broker = Broker::new(storage, root.path()).unwrap();

    for (flags, errno) in [
        (3, libc::EINVAL),
        (libc::O_RDONLY | libc::O_TRUNC, libc::EINVAL),
    ] {
        assert_errno(
            broker
                .handle(Request::Open {
                    path: path("file.txt"),
                    flags,
                    mode: 0,
                })
                .await
                .response,
            errno,
        );
    }
    let appended = broker
        .handle(Request::Open {
            path: path("file.txt"),
            flags: libc::O_RDONLY | libc::O_APPEND,
            mode: 0,
        })
        .await;
    let handle = open_handle(&appended.response);
    let descriptor = std::fs::File::from(appended.descriptor.unwrap());
    let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFL) };
    assert_eq!(flags & libc::O_ACCMODE, libc::O_RDONLY);
    assert_ne!(flags & libc::O_APPEND, 0);
    assert_eq!(
        broker
            .handle(Request::Close {
                handle,
                ranges: Vec::new()
            })
            .await
            .response,
        Response::Success
    );
    assert_errno(
        broker
            .handle(Request::Open {
                path: path("missing.txt"),
                flags: libc::O_RDONLY,
                mode: 0,
            })
            .await
            .response,
        libc::ENOENT,
    );
    assert_errno(
        broker
            .handle(Request::Open {
                path: path("file.txt"),
                flags: libc::O_RDONLY | libc::O_DIRECTORY,
                mode: 0,
            })
            .await
            .response,
        libc::ENOTDIR,
    );
    assert_errno(
        broker
            .handle(Request::Open {
                path: path("missing.txt"),
                flags: libc::O_RDONLY | libc::O_DIRECTORY,
                mode: 0,
            })
            .await
            .response,
        libc::ENOENT,
    );
    assert_errno(
        broker
            .handle(Request::Open {
                path: path("docs"),
                flags: libc::O_WRONLY,
                mode: 0,
            })
            .await
            .response,
        libc::EISDIR,
    );

    let directory = broker
        .handle(Request::Open {
            path: path("docs"),
            flags: libc::O_RDONLY,
            mode: 0,
        })
        .await;
    let handle = open_handle(&directory.response);
    assert!(
        std::fs::File::from(directory.descriptor.unwrap())
            .metadata()
            .unwrap()
            .is_dir()
    );
    assert_eq!(
        broker
            .handle(Request::Close {
                handle,
                ranges: Vec::new()
            })
            .await
            .response,
        Response::Success
    );

    assert_errno(
        broker
            .handle(Request::Access {
                path: path("file.txt"),
                mode: 8,
            })
            .await
            .response,
        libc::EINVAL,
    );
    assert_errno(
        broker
            .handle(Request::Access {
                path: path("file.txt"),
                mode: libc::X_OK,
            })
            .await
            .response,
        libc::EACCES,
    );
    assert_eq!(
        broker
            .handle(Request::Access {
                path: path("docs"),
                mode: libc::R_OK | libc::X_OK,
            })
            .await
            .response,
        Response::Success
    );
    assert_errno(
        broker
            .handle(Request::Sync {
                handle: "missing".to_string(),
                ranges: Vec::new(),
            })
            .await
            .response,
        libc::EBADF,
    );
    assert_errno(
        broker
            .handle(Request::Abort {
                handle: "missing".to_string(),
            })
            .await
            .response,
        libc::EBADF,
    );
    assert_errno(
        broker
            .handle(Request::Close {
                handle: "missing".to_string(),
                ranges: Vec::new(),
            })
            .await
            .response,
        libc::EBADF,
    );
}

#[tokio::test]
async fn broker_reclaims_unclaimed_open_and_anchor_resources() {
    let root = tempfile::tempdir().unwrap();
    let storage = std::sync::Arc::new(MemoryStorage::default());
    storage.insert_directory(0, "");
    storage.insert_file(0, "file.txt", b"file");
    let broker = Broker::new(storage, root.path()).unwrap();

    let stat_id = request_id(100);
    let stat = broker
        .handle_request(
            stat_id.clone(),
            Request::Stat {
                path: path("file.txt"),
                name_capacity: 0,
            },
        )
        .await;
    let Response::Stat { anchor, .. } = stat.response else {
        panic!("expected stat response");
    };
    assert!(root.path().join(&anchor).is_file());
    assert_eq!(
        broker
            .handle(Request::Claim {
                request_id: stat_id.clone(),
            })
            .await
            .response,
        Response::Success
    );
    assert_eq!(
        broker
            .handle(Request::Claim {
                request_id: stat_id,
            })
            .await
            .response,
        Response::Success
    );
    std::fs::remove_file(root.path().join(anchor)).unwrap();

    let anchor_id = request_id(101);
    let listed = broker
        .handle_request(
            anchor_id.clone(),
            Request::List {
                path: path(""),
                name_capacity: 0,
            },
        )
        .await;
    let Response::List { anchor, .. } = listed.response else {
        panic!("expected list response");
    };
    let open_id = request_id(102);
    let opened = broker
        .handle_request(
            open_id.clone(),
            Request::Open {
                path: path("file.txt"),
                flags: libc::O_RDONLY,
                mode: 0,
            },
        )
        .await;
    let handle = open_handle(&opened.response);
    drop(opened.descriptor);

    let expired = Instant::now() - REQUEST_CACHE_TTL - Duration::from_secs(1);
    let mut requests = broker.requests.lock().await;
    for id in [&anchor_id, &open_id] {
        let CachedRequest::Completed { completed_at, .. } = requests.entries.get_mut(id).unwrap()
        else {
            panic!("request must be completed");
        };
        *completed_at = expired;
    }
    drop(requests);
    broker.expire_requests().await;

    assert!(!root.path().join(anchor).exists());
    assert_eq!(broker.handle_count_for_test().await, 0);
    assert!(broker.closed_handles.lock().await.contains(&handle));
    assert_errno(
        broker
            .reply_for_response(Response::Open {
                handle,
                metadata: empty_file_metadata(),
            })
            .await
            .response,
        libc::EBADF,
    );
    assert_eq!(
        broker.reply_for_response(Response::Success).await.response,
        Response::Success
    );
    assert_errno(
        broker
            .handle(Request::Claim {
                request_id: request_id(999),
            })
            .await
            .response,
        libc::EPROTO,
    );
}

#[tokio::test]
async fn request_cache_waiters_capacity_and_tombstones_are_bounded() {
    let mut cache = RequestCache::default();
    let id = request_id(200);
    let request = Request::Access {
        path: path("file.txt"),
        mode: libc::R_OK,
    };
    let fingerprint = request_fingerprint(&request).unwrap();
    assert!(matches!(
        cache.begin(id.clone(), fingerprint),
        CacheDecision::Execute
    ));
    let CacheDecision::Wait(waiter) = cache.begin(id.clone(), fingerprint) else {
        panic!("duplicate pending request must wait");
    };
    assert!(cache.complete(id.clone(), Response::Success).is_empty());
    assert_eq!(waiter.await.unwrap(), Response::Success);
    assert!(matches!(
        cache.begin(id.clone(), fingerprint),
        CacheDecision::Replay(Response::Success)
    ));
    assert!(matches!(
        cache.begin(
            id,
            request_fingerprint(&Request::Access {
                path: path("different"),
                mode: libc::R_OK,
            })
            .unwrap()
        ),
        CacheDecision::Reject
    ));
    assert!(
        cache
            .complete(request_id(201), Response::Success)
            .is_empty()
    );
    assert!(cache.claim(&request_id(201)).is_none());
    assert!(cache.claim(&request_id(200)).is_none());

    for value in 0..=REQUEST_CACHE_CAPACITY {
        cache.entries.insert(
            request_id(10_000 + value as u128),
            CachedRequest::Completed {
                fingerprint,
                response: Response::Success,
                completed_at: Instant::now() + Duration::from_nanos(value as u64),
                claimed: true,
            },
        );
    }
    let _ = cache.prune(Instant::now());
    assert!(cache.entries.len() <= REQUEST_CACHE_CAPACITY + 1);

    let mut tombstones = HandleTombstones::default();
    for value in 0..=CLOSED_HANDLE_CAPACITY {
        tombstones.insert(format!("handle-{value}"));
    }
    tombstones.insert("handle-1".to_string());
    assert!(!tombstones.contains("handle-0"));
    assert!(tombstones.contains(&format!("handle-{CLOSED_HANDLE_CAPACITY}")));
}

#[tokio::test]
async fn broker_cleanup_and_path_helpers_cover_file_directory_and_cross_root_cases() {
    let root = tempfile::tempdir().unwrap();
    let broker = Broker::new(std::sync::Arc::new(MemoryStorage::default()), root.path()).unwrap();
    std::fs::write(root.path().join("file-anchor"), b"").unwrap();
    std::fs::create_dir(root.path().join("dir-anchor")).unwrap();
    std::fs::create_dir(root.path().join("nonempty-anchor")).unwrap();
    std::fs::write(root.path().join("nonempty-anchor/child"), b"").unwrap();
    broker
        .discard_abandoned_resources(vec![
            AbandonedResource::Anchor("file-anchor".to_string()),
            AbandonedResource::Anchor("dir-anchor".to_string()),
            AbandonedResource::Anchor("nonempty-anchor".to_string()),
            AbandonedResource::Anchor("missing-anchor".to_string()),
            AbandonedResource::Handle("missing-handle".to_string()),
        ])
        .await;
    assert!(!root.path().join("file-anchor").exists());
    assert!(!root.path().join("dir-anchor").exists());
    assert!(root.path().join("nonempty-anchor/child").is_file());
    std::fs::remove_dir_all(root.path().join("nonempty-anchor")).unwrap();

    let source = RemotePath::new(0, "source").unwrap();
    let child = RemotePath::new(0, "source/child").unwrap();
    let sibling = RemotePath::new(0, "source-other").unwrap();
    let target = RemotePath::new(0, "target").unwrap();
    assert_eq!(
        retarget_path(&source, &source, &target),
        Some(target.clone())
    );
    assert_eq!(
        retarget_path(&child, &source, &target),
        Some(RemotePath::new(0, "target/child").unwrap())
    );
    assert_eq!(retarget_path(&sibling, &source, &target), None);
    assert_eq!(
        retarget_path(&RemotePath::new(1, "source").unwrap(), &source, &target,),
        None
    );
    assert!(path_is_at_or_below(&child, &source));
    assert!(!path_is_at_or_below(&sibling, &source));
    assert!(!path_is_at_or_below(
        &RemotePath::new(1, "source").unwrap(),
        &source,
    ));

    assert_errno(
        broker
            .handle(Request::Rename {
                from: RemotePath::new(0, "source").unwrap(),
                to: RemotePath::new(1, "target").unwrap(),
            })
            .await
            .response,
        libc::EXDEV,
    );
    let error = storage_io("context", std::io::Error::other("failure"));
    assert_eq!(error.errno(), libc::EIO);
}

#[test]
fn close_on_exec_reports_an_invalid_descriptor() {
    use std::os::fd::AsRawFd as _;

    let file = std::mem::ManuallyDrop::new(std::fs::File::open("/dev/null").unwrap());
    let descriptor = file.as_raw_fd();
    assert_eq!(unsafe { libc::close(descriptor) }, 0);

    let error = set_close_on_exec(&file).unwrap_err();

    assert_eq!(error.errno(), libc::EBADF);
    assert!(
        error
            .to_string()
            .contains("protect anonymous remote descriptor")
    );
}
