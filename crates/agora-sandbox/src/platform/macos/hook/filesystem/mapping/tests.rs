use super::*;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

struct Fixture {
    directory: PathBuf,
    lower: PathBuf,
    runtime: FilesystemHookRuntime,
}

impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("agora-mapping-hook-{}", uuid::Uuid::new_v4()));
        let lower = directory.join("lower");
        std::fs::create_dir_all(&lower).unwrap();
        let runtime = FilesystemHookRuntime::new(directory.join("workdir/fs")).unwrap();
        Self {
            directory,
            lower,
            runtime,
        }
    }

    fn c_path(path: &Path) -> CString {
        use std::os::unix::ffi::OsStrExt as _;
        CString::new(path.as_os_str().as_bytes()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}

fn descriptor_alias_count(descriptor: libc::c_int) -> usize {
    unsafe {
        let mut source = std::mem::zeroed::<libc::stat>();
        assert_eq!(libc::fstat(descriptor, &mut source), 0);
        (0..libc::getdtablesize().min(4096))
            .filter(|candidate| {
                let mut status = std::mem::zeroed::<libc::stat>();
                libc::fstat(*candidate, &mut status) == 0
                    && status.st_dev == source.st_dev
                    && status.st_ino == source.st_ino
            })
            .count()
    }
}

#[test]
fn descriptor_route_index_encodes_both_states_at_word_boundaries() {
    let index = MemoryStateIndex::new();
    for descriptor in [0, 31, 32, 63, 64, 65_535] {
        for state in [(false, false), (true, false), (false, true), (true, true)] {
            index.set_descriptor(descriptor, state.0, state.1);
            assert_eq!(index.descriptor_routing_state(descriptor), Some(state));
            assert_eq!(index.data_descriptor_state(descriptor), Some(state.0));
            assert_eq!(index.mapping_descriptor_state(descriptor), Some(state.1));
        }
    }
    assert_eq!(index.descriptor_routing_state(-1), Some((false, false)));
    assert_eq!(index.descriptor_routing_state(65_536), None);
}

#[test]
fn mapping_ranges_split_and_reject_address_and_file_overflow() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("mapped.bin");
    std::fs::write(&logical, vec![0_u8; 4096]).unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        let open = fixture.runtime.tracked_open(descriptor).unwrap();

        let pending = fixture
            .runtime
            .prepare_mapping(2, libc::PROT_READ, libc::MAP_SHARED, descriptor, 0)
            .unwrap()
            .unwrap();
        assert!(
            fixture
                .runtime
                .register_mapping(usize::MAX as *mut libc::c_void, 2, Some(pending))
                .is_err()
        );

        lock(&fixture.runtime.mappings).push(MemoryMapping {
            start: 100,
            end: 200,
            file_offset: 10,
            writable: true,
            open: Arc::clone(&open),
        });
        let affected = fixture.runtime.remove_mappings(125, 175);
        assert_eq!(affected.len(), 1);
        let mappings = lock(&fixture.runtime.mappings).clone();
        assert_eq!(
            mappings
                .iter()
                .map(|mapping| (mapping.start, mapping.end, mapping.file_offset))
                .collect::<Vec<_>>(),
            [(100, 125, 10), (175, 200, 85)]
        );
        drop(mappings);

        lock(&fixture.runtime.mappings).clear();

        let invalid = MappingSlice {
            address: 1,
            length: 1,
            open,
        };
        assert!(
            fixture
                .runtime
                .flush_mapping_slices(&[invalid], true)
                .is_err()
        );

        assert_eq!(
            agora_sandbox_mmap(
                usize::MAX as *mut libc::c_void,
                2,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
                -1,
                0,
            ),
            libc::MAP_FAILED
        );
        assert_eq!(*libc::__error(), libc::EOVERFLOW);
        fixture
            .runtime
            .memory_index
            .set_descriptor(descriptor, true, true);
        assert_eq!(
            agora_sandbox_mmap(
                std::ptr::null_mut(),
                usize::MAX,
                libc::PROT_READ,
                libc::MAP_SHARED,
                descriptor,
                libc::off_t::MAX,
            ),
            libc::MAP_FAILED
        );
        assert_eq!(*libc::__error(), libc::EIO);

        assert_eq!(agora_sandbox_close(descriptor), 0);
    });
}

#[test]
fn mapping_hooks_report_overflow_before_touching_native_memory() {
    let fixture = Fixture::new();

    with_test_runtime(&fixture.runtime, || unsafe {
        let address = usize::MAX as *mut libc::c_void;
        assert_eq!(agora_sandbox_msync(address, 2, libc::MS_SYNC), -1);
        assert_eq!(*libc::__error(), libc::EOVERFLOW);
        assert_eq!(agora_sandbox_munmap(address, 2), -1);
        assert_eq!(*libc::__error(), libc::EOVERFLOW);

        let dangling = std::ptr::dangling_mut::<libc::c_void>();
        assert_eq!(agora_sandbox_msync(dangling, 4096, libc::MS_SYNC), -1);
        assert_eq!(agora_sandbox_munmap(dangling, 4096), -1);
    });
}

#[test]
fn untracked_anonymous_mmap_does_not_wait_for_open_file_state() {
    let fixture = Fixture::new();

    std::thread::scope(|scope| {
        let runtime = &fixture.runtime;
        let open_files = lock(&runtime.open_files);
        let (finished, result) = mpsc::sync_channel(1);
        scope.spawn(move || {
            let mapped = with_test_runtime(runtime, || unsafe {
                agora_sandbox_mmap(
                    std::ptr::null_mut(),
                    4096,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON,
                    -1,
                    0,
                )
            });
            let outcome = if mapped == libc::MAP_FAILED {
                -1
            } else {
                unsafe { original_munmap().unwrap()(mapped, 4096) }
            };
            let _ = finished.send(outcome);
        });

        let outcome = result.recv_timeout(Duration::from_millis(250));
        drop(open_files);
        assert_eq!(outcome.unwrap(), 0);
    });
}

#[test]
fn untracked_file_backed_mmap_does_not_depend_on_the_descriptor_state_lock() {
    let fixture = Fixture::new();

    with_test_runtime(&fixture.runtime, || unsafe {
        let _open_files = lock(&fixture.runtime.open_files);
        let mapped = agora_sandbox_mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            -1,
            0,
        );
        assert_eq!(mapped, libc::MAP_FAILED);
        assert_eq!(*libc::__error(), libc::EBADF);
    });
}

#[test]
fn plain_descriptor_classification_does_not_depend_on_the_open_file_lock() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("descriptor.bin");
    std::fs::write(&logical, vec![0_u8; 4096]).unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);

        let files = lock(&fixture.runtime.open_files);
        assert!(matches!(
            mmap_route(std::ptr::null_mut(), 4096, libc::MAP_PRIVATE, descriptor,),
            MemoryRoute::Native
        ));
        drop(files);

        assert_eq!(agora_sandbox_close(descriptor), 0);
    });
}

#[test]
fn plain_read_only_private_mmap_ignores_mapping_registry_barriers() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("plain-private.bin");
    std::fs::write(&logical, vec![b'x'; 4096]).unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        assert_eq!(
            super::super::data::agora_sandbox_data_descriptor_requires_hook(descriptor),
            1
        );

        let operation = fixture
            .runtime
            .operations
            .acquire(OperationRequest::new().mapping_registry_exclusive());
        let mapped = agora_sandbox_mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            descriptor,
            0,
        );
        assert_ne!(mapped, libc::MAP_FAILED);
        assert_eq!(*(mapped.cast::<u8>()), b'x');
        assert_eq!(agora_sandbox_munmap(mapped, 4096), 0);
        drop(operation);

        assert_eq!(agora_sandbox_close(descriptor), 0);
    });
}

#[test]
fn managed_mmap_waits_for_conflicting_descriptor_operation() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("managed-contention.bin");
    std::fs::write(&logical, vec![b'x'; 4096]).unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        fixture
            .runtime
            .memory_index
            .set_descriptor(descriptor, true, true);

        std::thread::scope(|scope| {
            let runtime = &fixture.runtime;
            let operation = runtime
                .operations
                .acquire(OperationRequest::new().descriptor_exclusive(descriptor));
            let (started, started_rx) = mpsc::sync_channel(1);
            let (finished, finished_rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                started.send(()).unwrap();
                let result = with_test_runtime(runtime, || {
                    let mapped = agora_sandbox_mmap(
                        std::ptr::null_mut(),
                        4096,
                        libc::PROT_READ,
                        libc::MAP_PRIVATE,
                        descriptor,
                        0,
                    );
                    (mapped as usize, *libc::__error())
                });
                finished.send(result).unwrap();
            });

            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(matches!(
                finished_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            drop(operation);

            let (mapped, errno) = finished_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_ne!(
                mapped as *mut libc::c_void,
                libc::MAP_FAILED,
                "errno={errno}"
            );
            assert_eq!(
                original_munmap().unwrap()(mapped as *mut libc::c_void, 4096),
                0
            );
        });

        assert_eq!(agora_sandbox_close(descriptor), 0);
    });
}

#[test]
fn managed_msync_waits_for_overlapping_address_mutation() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("msync-contention.bin");
    std::fs::write(&logical, vec![b'x'; 4096]).unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        fixture
            .runtime
            .memory_index
            .set_descriptor(descriptor, true, true);
        let mapped = agora_sandbox_mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            descriptor,
            0,
        );
        assert_ne!(mapped, libc::MAP_FAILED);
        let start = mapped as usize;
        let end = start + 4096;

        std::thread::scope(|scope| {
            let runtime = &fixture.runtime;
            let operation = runtime
                .operations
                .acquire(OperationRequest::new().address_exclusive(start, end));
            let (started, started_rx) = mpsc::sync_channel(1);
            let (finished, finished_rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                started.send(()).unwrap();
                let result = with_test_runtime(runtime, || {
                    agora_sandbox_msync(start as *mut libc::c_void, end - start, libc::MS_SYNC)
                });
                finished.send(result).unwrap();
            });

            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(matches!(
                finished_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            drop(operation);
            assert_eq!(finished_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
        });

        assert_eq!(agora_sandbox_munmap(mapped, 4096), 0);
        assert_eq!(agora_sandbox_close(descriptor), 0);
    });
}

#[test]
fn managed_munmap_waits_for_overlapping_address_reader() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("munmap-contention.bin");
    std::fs::write(&logical, vec![b'x'; 4096]).unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        fixture
            .runtime
            .memory_index
            .set_descriptor(descriptor, true, true);
        let mapped = agora_sandbox_mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            descriptor,
            0,
        );
        assert_ne!(mapped, libc::MAP_FAILED);
        let start = mapped as usize;
        let end = start + 4096;

        std::thread::scope(|scope| {
            let runtime = &fixture.runtime;
            let operation = runtime
                .operations
                .acquire(OperationRequest::new().address_shared(start, end));
            let (started, started_rx) = mpsc::sync_channel(1);
            let (finished, finished_rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                started.send(()).unwrap();
                let result = with_test_runtime(runtime, || {
                    agora_sandbox_munmap(start as *mut libc::c_void, end - start)
                });
                finished.send(result).unwrap();
            });

            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(matches!(
                finished_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            drop(operation);
            assert_eq!(finished_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
        });

        assert_eq!(agora_sandbox_close(descriptor), 0);
    });
}

#[test]
fn plain_descriptor_duplication_preserves_native_mapping_classification() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("descriptor-update.bin");
    std::fs::write(&logical, vec![0_u8; 4096]).unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let source = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(source >= 0);
        let destination = libc::dup(source);
        assert!(destination >= 0);

        std::thread::scope(|scope| {
            let runtime = &fixture.runtime;
            let operation = runtime
                .operations
                .acquire(OperationRequest::new().descriptor_exclusive(destination));
            let (finished, result) = mpsc::sync_channel(1);
            scope.spawn(move || {
                runtime.duplicate_descriptor(source, destination);
                let _ = finished.send(());
            });

            assert!(matches!(
                result.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            assert_eq!(
                runtime.memory_index.mapping_descriptor_state(destination),
                Some(false)
            );
            drop(operation);
            result.recv_timeout(Duration::from_millis(250)).unwrap();
        });

        assert_eq!(
            fixture
                .runtime
                .memory_index
                .mapping_descriptor_state(destination),
            Some(false)
        );
        assert_eq!(agora_sandbox_close(destination), 0);
        assert_eq!(agora_sandbox_close(source), 0);
    });
}

#[test]
fn close_waits_for_conflicting_descriptor_operation() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("close-contention.bin");
    std::fs::write(&logical, b"close").unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);

        std::thread::scope(|scope| {
            let runtime = &fixture.runtime;
            let operation = runtime
                .operations
                .acquire(OperationRequest::new().descriptor_shared(descriptor));
            let (started, started_rx) = mpsc::sync_channel(1);
            let (finished, finished_rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                started.send(()).unwrap();
                let result = with_test_runtime(runtime, || agora_sandbox_close(descriptor));
                finished.send(result).unwrap();
            });

            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(matches!(
                finished_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            drop(operation);
            assert_eq!(finished_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
        });
    });
}

#[test]
fn descriptor_write_mutation_waits_for_conflicting_replacement() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("truncate-contention.bin");
    std::fs::write(&logical, b"truncate").unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);

        std::thread::scope(|scope| {
            let runtime = &fixture.runtime;
            let operation = runtime
                .operations
                .acquire(OperationRequest::new().descriptor_exclusive(descriptor));
            let (started, started_rx) = mpsc::sync_channel(1);
            let (finished, finished_rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                started.send(()).unwrap();
                let result = with_test_runtime(runtime, || agora_sandbox_ftruncate(descriptor, 4));
                finished.send(result).unwrap();
            });

            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(matches!(
                finished_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            drop(operation);
            assert_eq!(finished_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
        });

        assert_eq!(agora_sandbox_close(descriptor), 0);
    });
}

#[test]
fn close_transition_keeps_managed_mmap_on_the_coordinated_path() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("close-transition.bin");
    std::fs::write(&logical, vec![0_u8; 4096]).unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        fixture
            .runtime
            .memory_index
            .set_descriptor(descriptor, true, true);

        std::thread::scope(|scope| {
            let runtime = &fixture.runtime;
            let (close_started, close_started_rx) = mpsc::sync_channel(1);
            let (allow_close, allow_close_rx) = mpsc::sync_channel(1);
            let (close_finished, close_finished_rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                let result = with_test_runtime(runtime, || {
                    super::super::descriptor::sandbox_close(descriptor, |descriptor| {
                        close_started.send(()).unwrap();
                        allow_close_rx.recv().unwrap();
                        libc::close(descriptor)
                    })
                });
                close_finished.send(result).unwrap();
            });

            close_started_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            let (mapping_finished, mapping_finished_rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                let mapped = with_test_runtime(runtime, || {
                    agora_sandbox_mmap(
                        std::ptr::null_mut(),
                        4096,
                        libc::PROT_READ,
                        libc::MAP_SHARED,
                        descriptor,
                        0,
                    )
                });
                mapping_finished
                    .send((mapped as usize, *libc::__error()))
                    .unwrap();
            });

            let early_mapping = mapping_finished_rx.recv_timeout(Duration::from_millis(50));
            allow_close.send(()).unwrap();
            assert_eq!(
                close_finished_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap(),
                0
            );
            let (mapped, errno) = match early_mapping {
                Ok(outcome) => outcome,
                Err(mpsc::RecvTimeoutError::Timeout) => mapping_finished_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap(),
                Err(error) => panic!("managed mmap result channel failed: {error}"),
            };
            if mapped as *mut libc::c_void != libc::MAP_FAILED {
                assert_eq!(
                    original_munmap().unwrap()(mapped as *mut libc::c_void, 4096),
                    0
                );
            }

            assert!(
                matches!(early_mapping, Err(mpsc::RecvTimeoutError::Timeout)),
                "managed mmap bypassed the descriptor close transaction"
            );
            assert_eq!(mapped as *mut libc::c_void, libc::MAP_FAILED);
            assert_eq!(errno, libc::EBADF);
        });
    });
}

#[test]
fn managed_write_waits_for_conflicting_descriptor_mutation() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("write-contention.bin");
    std::fs::write(&logical, b".").unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);

        std::thread::scope(|scope| {
            let runtime = &fixture.runtime;
            let operation = runtime
                .operations
                .acquire(OperationRequest::new().descriptor_exclusive(descriptor));
            let (started, started_rx) = mpsc::sync_channel(1);
            let (finished, finished_rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                started.send(()).unwrap();
                let result = with_test_runtime(runtime, || {
                    super::super::data::agora_sandbox_write(descriptor, b"x".as_ptr().cast(), 1)
                });
                finished.send(result).unwrap();
            });

            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(matches!(
                finished_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            drop(operation);
            assert_eq!(finished_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        });

        assert_eq!(agora_sandbox_close(descriptor), 0);
    });
}

#[test]
fn writer_publication_waits_for_a_stable_descriptor_identity() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("writer-publication/fs"),
        b"writer-publication-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("writer-publication.bin");
    let path = Fixture::c_path(&logical);

    with_test_runtime(&runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(
            super::super::data::agora_sandbox_write(descriptor, b"pending".as_ptr().cast(), 7,),
            7
        );

        std::thread::scope(|scope| {
            let runtime = &runtime;
            let logical = &logical;
            let operation = runtime
                .operations
                .acquire(OperationRequest::new().descriptor_exclusive(descriptor));
            let (started, started_rx) = mpsc::sync_channel(1);
            let (finished, finished_rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                started.send(()).unwrap();
                finished
                    .send(runtime.publish_open_writers(logical))
                    .unwrap();
            });

            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(matches!(
                finished_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            drop(operation);
            finished_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
        });

        assert_eq!(agora_sandbox_close(descriptor), 0);
    });
}

#[test]
fn dup2_waits_before_replacing_the_native_destination() {
    let fixture = Fixture::new();
    let source_path = fixture.lower.join("dup2-source.bin");
    let destination_path = fixture.lower.join("dup2-destination.bin");
    std::fs::write(&source_path, b"source").unwrap();
    std::fs::write(&destination_path, b"destination").unwrap();
    let source_path = Fixture::c_path(&source_path);
    let destination_path = Fixture::c_path(&destination_path);

    with_test_runtime(&fixture.runtime, || unsafe {
        let source = agora_sandbox_open_with_mode(source_path.as_ptr(), libc::O_RDONLY, 0);
        let destination =
            agora_sandbox_open_with_mode(destination_path.as_ptr(), libc::O_RDONLY, 0);
        assert!(source >= 0);
        assert!(destination >= 0);

        let mut source_status = std::mem::zeroed::<libc::stat>();
        let mut destination_status = std::mem::zeroed::<libc::stat>();
        assert_eq!(libc::fstat(source, &mut source_status), 0);
        assert_eq!(libc::fstat(destination, &mut destination_status), 0);
        assert_ne!(source_status.st_ino, destination_status.st_ino);

        std::thread::scope(|scope| {
            let runtime = &fixture.runtime;
            let operation = runtime
                .operations
                .acquire(OperationRequest::new().descriptor_exclusive(source));
            let (started, started_rx) = mpsc::sync_channel(1);
            let (finished, finished_rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                started.send(()).unwrap();
                let result = with_test_runtime(runtime, || agora_sandbox_dup2(source, destination));
                finished.send(result).unwrap();
            });

            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while runtime.operations.waiting_count_for_test() == 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "dup2 did not reach descriptor coordination"
                );
                std::thread::yield_now();
            }

            let mut waiting_status = std::mem::zeroed::<libc::stat>();
            assert_eq!(libc::fstat(destination, &mut waiting_status), 0);
            assert_eq!(waiting_status.st_ino, destination_status.st_ino);
            assert!(matches!(
                finished_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));

            drop(operation);
            assert_eq!(
                finished_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                destination
            );
        });

        let mut replaced_status = std::mem::zeroed::<libc::stat>();
        assert_eq!(libc::fstat(destination, &mut replaced_status), 0);
        assert_eq!(replaced_status.st_ino, source_status.st_ino);
        assert_eq!(agora_sandbox_close(destination), 0);
        assert_eq!(agora_sandbox_close(source), 0);
    });
}

#[test]
fn dup_waits_before_allocating_a_native_destination() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("dup-source.bin");
    std::fs::write(&logical, b"source").unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let source = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(source >= 0);
        let aliases = descriptor_alias_count(source);

        std::thread::scope(|scope| {
            let runtime = &fixture.runtime;
            let operation = runtime
                .operations
                .acquire(OperationRequest::new().descriptor_exclusive(source));
            let (started, started_rx) = mpsc::sync_channel(1);
            let (finished, finished_rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                started.send(()).unwrap();
                let result = with_test_runtime(runtime, || agora_sandbox_dup(source));
                finished.send(result).unwrap();
            });

            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while runtime.operations.waiting_count_for_test() == 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "dup did not reach descriptor coordination"
                );
                std::thread::yield_now();
            }
            assert_eq!(descriptor_alias_count(source), aliases);
            assert!(matches!(
                finished_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));

            drop(operation);
            let duplicate = finished_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(duplicate >= 0);
            assert_eq!(descriptor_alias_count(source), aliases + 1);
            assert_eq!(agora_sandbox_close(duplicate), 0);
        });

        assert_eq!(agora_sandbox_close(source), 0);
    });
}

#[test]
fn fcntl_dup_waits_before_allocating_a_native_destination() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("fcntl-dup-source.bin");
    std::fs::write(&logical, b"source").unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let source = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(source >= 0);
        let aliases = descriptor_alias_count(source);
        let minimum = 128;

        std::thread::scope(|scope| {
            let runtime = &fixture.runtime;
            let operation = runtime
                .operations
                .acquire(OperationRequest::new().descriptor_exclusive(source));
            let (started, started_rx) = mpsc::sync_channel(1);
            let (finished, finished_rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                started.send(()).unwrap();
                let result = with_test_runtime(runtime, || {
                    super::super::agora_sandbox_fcntl_shim(source, libc::F_DUPFD_CLOEXEC, minimum)
                });
                finished.send(result).unwrap();
            });

            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while runtime.operations.waiting_count_for_test() == 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "fcntl duplicate did not reach descriptor coordination"
                );
                std::thread::yield_now();
            }
            assert_eq!(descriptor_alias_count(source), aliases);
            assert!(matches!(
                finished_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));

            drop(operation);
            let duplicate = finished_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(duplicate >= minimum);
            assert_eq!(descriptor_alias_count(source), aliases + 1);
            assert_ne!(libc::fcntl(duplicate, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
            assert_eq!(agora_sandbox_close(duplicate), 0);
        });

        assert_eq!(agora_sandbox_close(source), 0);
    });
}

#[test]
fn unreferenced_finish_waits_for_registry_publishers() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("finish-contention.bin");
    std::fs::write(&logical, b"finish").unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        let (open, _) = fixture.runtime.take_descriptor(descriptor).unwrap();
        let publisher = fixture
            .runtime
            .operations
            .acquire(OperationRequest::new().descriptor_registry_shared());

        std::thread::scope(|scope| {
            let runtime = &fixture.runtime;
            let candidate = Arc::clone(&open);
            let (started, started_rx) = mpsc::sync_channel(1);
            let (finished, finished_rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                started.send(()).unwrap();
                finished
                    .send(runtime.finish_unreferenced(vec![candidate]))
                    .unwrap();
            });

            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(matches!(
                finished_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            drop(publisher);
            finished_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
        });

        assert!(open.finished.load(Ordering::Acquire));
        assert_eq!(libc::close(descriptor), 0);
    });
}

#[test]
fn untracked_munmap_does_not_depend_on_the_mapping_state_lock() {
    let fixture = Fixture::new();
    let mapped = unsafe {
        original_mmap().unwrap()(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    assert_ne!(mapped, libc::MAP_FAILED);

    std::thread::scope(|scope| {
        let runtime = &fixture.runtime;
        let mappings = lock(&runtime.mappings);
        let (finished, result) = mpsc::sync_channel(1);
        let address = mapped as usize;
        scope.spawn(move || {
            let outcome = with_test_runtime(runtime, || unsafe {
                agora_sandbox_munmap(address as *mut libc::c_void, 4096)
            });
            let _ = finished.send(outcome);
        });

        let outcome = result.recv_timeout(Duration::from_millis(250));
        drop(mappings);
        let outcome = outcome.unwrap();
        if outcome != 0 {
            assert_eq!(unsafe { original_munmap().unwrap()(mapped, 4096) }, 0);
        }
        assert_eq!(outcome, 0);
    });
}

#[test]
fn tracked_mapping_classification_does_not_depend_on_the_mapping_state_lock() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("classified.bin");
    std::fs::write(&logical, vec![0_u8; 4096]).unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        fixture
            .runtime
            .memory_index
            .set_descriptor(descriptor, true, true);
        let mapped = agora_sandbox_mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            descriptor,
            0,
        );
        assert_ne!(mapped, libc::MAP_FAILED);

        let mappings = lock(&fixture.runtime.mappings);
        assert!(matches!(
            mapping_range_route(mapped as usize, mapped as usize + 4096),
            MemoryRoute::Managed(_)
        ));
        drop(mappings);

        assert_eq!(agora_sandbox_munmap(mapped, 4096), 0);
        assert_eq!(agora_sandbox_close(descriptor), 0);
    });
}

#[test]
fn shared_mapping_flushes_after_native_protection_changes() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("shared.bin");
    std::fs::write(&logical, vec![b'.'; 4096]).unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        let mapped = agora_sandbox_mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ,
            libc::MAP_SHARED,
            descriptor,
            0,
        );
        assert_ne!(mapped, libc::MAP_FAILED);
        assert_eq!(
            libc::mprotect(mapped, 4096, libc::PROT_READ | libc::PROT_WRITE),
            0
        );
        std::ptr::copy_nonoverlapping(b"mapped".as_ptr(), mapped.cast::<u8>(), 6);

        fixture.runtime.flush_memory_mappings().unwrap();
        assert_eq!(agora_sandbox_msync(mapped, 4096, libc::MS_SYNC), 0);
        assert_eq!(libc::mprotect(mapped, 4096, libc::PROT_READ), 0);
        assert_eq!(
            libc::mprotect(mapped, 4096, libc::PROT_READ | libc::PROT_WRITE),
            0
        );
        assert_eq!(agora_sandbox_munmap(mapped, 4096), 0);
        assert_eq!(agora_sandbox_close(descriptor), 0);

        let descriptor = agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        let mut content = [0_u8; 6];
        assert_eq!(libc::read(descriptor, content.as_mut_ptr().cast(), 6), 6);
        assert_eq!(&content, b"mapped");
        assert_eq!(agora_sandbox_close(descriptor), 0);
    });
}
