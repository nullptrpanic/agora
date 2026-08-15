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
fn tracked_descriptor_classification_does_not_depend_on_the_descriptor_state_lock() {
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
            MemoryRoute::Managed(_)
        ));
        drop(files);

        assert_eq!(agora_sandbox_close(descriptor), 0);
    });
}

#[test]
fn descriptor_tracking_changes_share_the_mapping_operation_boundary() {
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
            let operation = lock(&runtime.mapping_operations);
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
                runtime.memory_index.descriptor_state(destination),
                Some(false)
            );
            drop(operation);
            result.recv_timeout(Duration::from_millis(250)).unwrap();
        });

        assert_eq!(
            fixture.runtime.memory_index.descriptor_state(destination),
            Some(true)
        );
        assert_eq!(agora_sandbox_close(destination), 0);
        assert_eq!(agora_sandbox_close(source), 0);
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
