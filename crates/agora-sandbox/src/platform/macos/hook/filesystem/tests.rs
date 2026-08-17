use super::data::{
    agora_sandbox_aio_write as sandbox_aio_write,
    agora_sandbox_guarded_pwrite as sandbox_guarded_pwrite,
    agora_sandbox_guarded_writev as sandbox_guarded_writev,
    agora_sandbox_lio_listio as sandbox_lio_listio, agora_sandbox_lseek as sandbox_lseek,
    agora_sandbox_pread as sandbox_pread, agora_sandbox_pwrite as sandbox_pwrite,
    agora_sandbox_pwritev as sandbox_pwritev, agora_sandbox_read as sandbox_read,
    agora_sandbox_write as sandbox_write, agora_sandbox_writev as sandbox_writev,
};
use super::directory::{
    fts_bulk_entry_names_for_test, fts_directory_descent_path_for_test,
    fts_getattrlistbulk_for_test as sandbox_getattrlistbulk,
    fts_read_returns_virtual_entry_for_test,
};
use super::lifecycle::sandbox_fork_with;
use super::mapping::{
    agora_sandbox_mmap as sandbox_mmap, agora_sandbox_msync as sandbox_msync,
    agora_sandbox_munmap as sandbox_munmap,
};
use super::socket::{UnixSocketAddress, agora_sandbox_bind as sandbox_bind};
use super::{
    ByteRangeSet, DirectoryCursor, FilesystemHookGuard, FilesystemHookRuntime, LocalByteRange,
    agora_sandbox_access as sandbox_access, agora_sandbox_chdir as sandbox_chdir,
    agora_sandbox_chflags as sandbox_chflags, agora_sandbox_chmod as sandbox_chmod,
    agora_sandbox_chown as sandbox_chown, agora_sandbox_clonefile as sandbox_clonefile,
    agora_sandbox_clonefileat as sandbox_clonefileat, agora_sandbox_close as sandbox_close,
    agora_sandbox_closedir as sandbox_closedir,
    agora_sandbox_commit_synced_descriptor as sandbox_commit_synced_descriptor,
    agora_sandbox_copyfile as sandbox_copyfile, agora_sandbox_creat as sandbox_creat,
    agora_sandbox_dup as sandbox_dup, agora_sandbox_dup2 as sandbox_dup2,
    agora_sandbox_faccessat as sandbox_faccessat, agora_sandbox_fchdir as sandbox_fchdir,
    agora_sandbox_fchflags as sandbox_fchflags, agora_sandbox_fchmod as sandbox_fchmod,
    agora_sandbox_fchmodat as sandbox_fchmodat, agora_sandbox_fchown as sandbox_fchown,
    agora_sandbox_fchownat as sandbox_fchownat, agora_sandbox_fclose as sandbox_fclose,
    agora_sandbox_fdopendir as sandbox_fdopendir, agora_sandbox_fopen as sandbox_fopen,
    agora_sandbox_fremovexattr as sandbox_fremovexattr, agora_sandbox_freopen as sandbox_freopen,
    agora_sandbox_fsetxattr as sandbox_fsetxattr, agora_sandbox_fstat as sandbox_fstat,
    agora_sandbox_fstatat as sandbox_fstatat, agora_sandbox_fsync as sandbox_fsync,
    agora_sandbox_ftruncate as sandbox_ftruncate, agora_sandbox_futimens as sandbox_futimens,
    agora_sandbox_futimes as sandbox_futimes, agora_sandbox_getcwd as sandbox_getcwd,
    agora_sandbox_guarded_close as sandbox_guarded_close,
    agora_sandbox_guarded_open_with_mode as sandbox_guarded_open_with_mode,
    agora_sandbox_lchown as sandbox_lchown, agora_sandbox_link as sandbox_link,
    agora_sandbox_linkat as sandbox_linkat, agora_sandbox_lstat as sandbox_lstat,
    agora_sandbox_lutimes as sandbox_lutimes, agora_sandbox_mkdir as sandbox_mkdir,
    agora_sandbox_mkdirat as sandbox_mkdirat,
    agora_sandbox_open_with_mode as sandbox_open_with_mode,
    agora_sandbox_openat_with_mode as sandbox_openat_with_mode,
    agora_sandbox_opendir as sandbox_opendir,
    agora_sandbox_posix_spawn_file_actions_addopen as sandbox_spawn_addopen,
    agora_sandbox_readdir as sandbox_readdir, agora_sandbox_readdir_r as sandbox_readdir_r,
    agora_sandbox_readlink as sandbox_readlink, agora_sandbox_readlinkat as sandbox_readlinkat,
    agora_sandbox_realpath as sandbox_realpath, agora_sandbox_removefile as sandbox_removefile,
    agora_sandbox_removefileat as sandbox_removefileat,
    agora_sandbox_removexattr as sandbox_removexattr, agora_sandbox_rename as sandbox_rename,
    agora_sandbox_renameat as sandbox_renameat, agora_sandbox_renameatx_np as sandbox_renameatx_np,
    agora_sandbox_renamex_np as sandbox_renamex_np, agora_sandbox_rewinddir as sandbox_rewinddir,
    agora_sandbox_rmdir as sandbox_rmdir, agora_sandbox_setxattr as sandbox_setxattr,
    agora_sandbox_stat as sandbox_stat, agora_sandbox_symlink as sandbox_symlink,
    agora_sandbox_symlinkat as sandbox_symlinkat, agora_sandbox_truncate as sandbox_truncate,
    agora_sandbox_unlink as sandbox_unlink, agora_sandbox_unlinkat as sandbox_unlinkat,
    agora_sandbox_utimensat as sandbox_utimensat, agora_sandbox_utimes as sandbox_utimes,
    catch_filesystem_panic, configure_descriptor, error_errno, flush_at_exit, flush_before_exec,
    intent_from_fopen_mode, lock_filesystem_before_fork, sandbox_descriptor_mutation,
    sandbox_flock_with, sandbox_unsupported_path_mutation, tracked_current_directory,
    truncate_reservation, unlock_filesystem_after_fork, with_test_runtime,
};
use crate::audit::AuditClient;
use crate::filesystem::{EntryState, FileAttributes, FileLayer};
use crate::nfs::controller::RemoteController;
use crate::nfs::protocol::RemoteRoute;
use crate::nfs::testing::MemoryStorage;
use crate::platform::hook::network::agora_sandbox_connect as sandbox_connect;
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

struct Fixture {
    directory: PathBuf,
    lower: PathBuf,
    runtime: FilesystemHookRuntime,
}

#[test]
fn truncate_reservations_cover_the_changed_extent() {
    assert_eq!(truncate_reservation(8, 3), LocalByteRange::new(3, 8).ok());
    assert_eq!(truncate_reservation(3, 8), LocalByteRange::new(3, 8).ok());
    assert_eq!(truncate_reservation(3, 3), None);
    assert_eq!(truncate_reservation(3, -1), None);
}

impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("agora-filesystem-hook-{}", uuid::Uuid::new_v4()));
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
        use std::os::unix::ffi::OsStrExt;
        CString::new(path.as_os_str().as_bytes()).unwrap()
    }

    fn attach_nfs(&mut self) -> NfsTestServer {
        let logical_root = self.lower.join("network");
        let storage = Arc::new(MemoryStorage::default());
        storage.insert_directory(0, "");
        let runtime_directory = tempfile::Builder::new()
            .prefix("agora-nfs-")
            .tempdir_in("/tmp")
            .unwrap();
        let (socket, token, shutdown, thread) =
            NfsTestServer::start(Arc::clone(&storage), runtime_directory.path().to_path_buf());
        self.runtime.remote = Some(
            super::nfs::RemoteFilesystem::new(
                socket,
                token,
                vec![RemoteRoute {
                    root: 0,
                    logical_root: logical_root.to_string_lossy().into_owned(),
                }],
            )
            .unwrap(),
        );
        NfsTestServer {
            logical_root,
            storage,
            shutdown: Some(shutdown),
            thread: Some(thread),
            _runtime_directory: runtime_directory,
        }
    }
}

unsafe fn materialize_remote_snapshot(descriptor: libc::c_int) {
    let mapping = unsafe {
        sandbox_mmap(
            std::ptr::null_mut(),
            1,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            descriptor,
            0,
        )
    };
    assert_ne!(mapping, libc::MAP_FAILED);
    assert_eq!(unsafe { sandbox_munmap(mapping, 1) }, 0);
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}

#[test]
fn pathname_unix_sockets_bind_and_connect_through_the_overlay() {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let directory = Path::new("/tmp").join(format!("ah-{}", &suffix[..8]));
    let lower = Path::new("/tmp").join(format!("al-{}", &suffix[..8]));
    std::fs::create_dir_all(&lower).unwrap();
    let runtime = FilesystemHookRuntime::new(directory.join("fs")).unwrap();
    let logical = lower.join("service.sock");
    let address = UnixSocketAddress::new(&logical).unwrap();

    with_test_runtime(&runtime, || unsafe {
        let listener = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        assert!(listener >= 0);
        assert_eq!(
            sandbox_bind(listener, address.as_ptr(), address.len()),
            0,
            "bind failed: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(libc::listen(listener, 1), 0);
        assert!(!logical.exists());
        let mapped = runtime.filesystem.prepare_read(&logical).unwrap();
        assert!(mapped.exists());

        let client = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        assert!(client >= 0);
        assert_eq!(sandbox_connect(client, address.as_ptr(), address.len()), 0);
        let accepted = libc::accept(listener, std::ptr::null_mut(), std::ptr::null_mut());
        assert!(accepted >= 0);
        libc::close(accepted);
        libc::close(client);
        libc::close(listener);
        assert_eq!(sandbox_unlink(Fixture::c_path(&logical).as_ptr()), 0);
        assert!(!mapped.exists());
    });

    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(lower).unwrap();
}

#[test]
fn pathname_unix_connect_keeps_an_untouched_lower_socket_native() {
    use std::os::unix::net::UnixListener;

    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let directory = Path::new("/tmp").join(format!("ch-{}", &suffix[..8]));
    let lower = Path::new("/tmp").join(format!("cl-{}", &suffix[..8]));
    std::fs::create_dir_all(&lower).unwrap();
    let runtime = FilesystemHookRuntime::new(directory.join("fs")).unwrap();
    let logical = lower.join("service.sock");
    let listener = UnixListener::bind(&logical).unwrap();
    let address = UnixSocketAddress::new(&logical).unwrap();

    with_test_runtime(&runtime, || unsafe {
        let client = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        assert!(client >= 0);
        assert_eq!(sandbox_connect(client, address.as_ptr(), address.len()), 0);
        let _accepted = listener.accept().unwrap();
        libc::close(client);
    });

    assert!(logical.exists());
    drop(listener);
    std::fs::remove_file(logical).unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(lower).unwrap();
}

struct NfsTestServer {
    logical_root: PathBuf,
    storage: Arc<MemoryStorage>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
    _runtime_directory: tempfile::TempDir,
}

impl NfsTestServer {
    fn start(
        storage: Arc<MemoryStorage>,
        runtime_directory: PathBuf,
    ) -> (
        PathBuf,
        String,
        tokio::sync::oneshot::Sender<()>,
        thread::JoinHandle<()>,
    ) {
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let (shutdown, stopping) = tokio::sync::oneshot::channel();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let controller = RemoteController::start_with_storage(storage, &runtime_directory)
                    .await
                    .unwrap();
                ready
                    .send((
                        controller.runtime().socket().to_path_buf(),
                        controller.runtime().token().to_string(),
                    ))
                    .unwrap();
                let _ = stopping.await;
                controller.shutdown().await.unwrap();
            });
        });
        let (socket, token) = started.recv().unwrap();
        (socket, token, shutdown, thread)
    }

    fn anchor_count(&self) -> usize {
        std::fs::read_dir(self._runtime_directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("anchor-"))
            .count()
    }
}

impl Drop for NfsTestServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn nfs_files_use_anonymous_descriptors_without_overlay_state() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_directory(0, "docs");
    nfs.storage
        .insert_file(0, "docs/file.txt", b"remote content");
    let logical = nfs.logical_root.join("docs/file.txt");
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        let mut content = [0_u8; 14];
        assert_eq!(
            sandbox_read(descriptor, content.as_mut_ptr().cast(), content.len()),
            14
        );
        assert_eq!(&content, b"remote content");

        let mut status = std::mem::zeroed();
        assert_eq!(sandbox_fstat(descriptor, &mut status), 0);
        assert_eq!(status.st_size, 14);
        assert_eq!(sandbox_stat(path.as_ptr(), &mut status), 0);
        assert_eq!(status.st_size, 14);
        assert_eq!(sandbox_access(path.as_ptr(), libc::R_OK | libc::W_OK), 0);
        assert_eq!(sandbox_access(path.as_ptr(), libc::X_OK), -1);
        assert_eq!(*libc::__error(), libc::EACCES);

        assert_eq!(sandbox_ftruncate(descriptor, 0), 0);
        assert_eq!(sandbox_lseek(descriptor, 0, libc::SEEK_SET), 0);
        assert_eq!(sandbox_write(descriptor, b"sandbox".as_ptr().cast(), 7), 7);
        assert_eq!(sandbox_stat(path.as_ptr(), &mut status), 0);
        assert_eq!(status.st_size, 7);
        assert_eq!(
            nfs.storage.data(0, "docs/file.txt"),
            Some(b"sandbox".to_vec())
        );
        assert_eq!(sandbox_fsync(descriptor), 0);
        assert_eq!(
            nfs.storage.data(0, "docs/file.txt"),
            Some(b"sandbox".to_vec())
        );

        let duplicate = sandbox_dup(descriptor);
        assert!(duplicate >= 0);
        assert_eq!(sandbox_close(descriptor), 0);
        assert_eq!(sandbox_close(duplicate), 0);

        assert_eq!(sandbox_truncate(path.as_ptr(), -1), -1);
        assert_eq!(*libc::__error(), libc::EINVAL);
        assert_eq!(
            nfs.storage.data(0, "docs/file.txt"),
            Some(b"sandbox".to_vec())
        );

        let stream = sandbox_fopen(path.as_ptr(), c"r".as_ptr());
        assert!(!stream.is_null());
        let mut reopened = [0_u8; 7];
        assert_eq!(
            sandbox_read(
                libc::fileno(stream),
                reopened.as_mut_ptr().cast(),
                reopened.len(),
            ),
            7
        );
        assert_eq!(&reopened, b"sandbox");
        assert_eq!(sandbox_fclose(stream), 0);

        assert_eq!(sandbox_truncate(path.as_ptr(), 3), 0);
        assert_eq!(nfs.storage.data(0, "docs/file.txt"), Some(b"san".to_vec()));

        let appender = sandbox_open_with_mode(path.as_ptr(), libc::O_WRONLY | libc::O_APPEND, 0);
        assert!(appender >= 0);
        nfs.storage
            .replace(0, "docs/file.txt", b"external append base");
        assert_eq!(sandbox_write(appender, b"!".as_ptr().cast(), 1), 1);
        assert_eq!(sandbox_close(appender), 0);
        assert_eq!(
            nfs.storage.data(0, "docs/file.txt"),
            Some(b"external append base!".to_vec())
        );

        let dynamic_appender = sandbox_open_with_mode(path.as_ptr(), libc::O_WRONLY, 0);
        assert!(dynamic_appender >= 0);
        let flags = libc::fcntl(dynamic_appender, libc::F_GETFL);
        assert!(flags >= 0);
        assert_eq!(
            libc::fcntl(dynamic_appender, libc::F_SETFL, flags | libc::O_APPEND),
            0
        );
        nfs.storage
            .replace(0, "docs/file.txt", b"dynamic append base");
        assert_eq!(sandbox_write(dynamic_appender, b"!".as_ptr().cast(), 1), 1);
        assert_eq!(sandbox_close(dynamic_appender), 0);
        assert_eq!(
            nfs.storage.data(0, "docs/file.txt"),
            Some(b"dynamic append base!".to_vec())
        );

        let created_path = Fixture::c_path(&nfs.logical_root.join("created.txt"));
        let created =
            sandbox_open_with_mode(created_path.as_ptr(), libc::O_RDONLY | libc::O_CREAT, 0o600);
        assert!(created >= 0);
        assert_eq!(nfs.storage.data(0, "created.txt"), Some(Vec::new()));
        assert_eq!(sandbox_close(created), 0);
    });

    assert!(!nfs.logical_root.exists());
    assert_eq!(
        fixture.runtime.filesystem.state_for_test(&logical).unwrap(),
        None
    );
}

#[test]
fn nfs_positioned_and_vectored_writes_preserve_offsets_and_flush_exact_content() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "writes.bin", b"............");
    let path = Fixture::c_path(&nfs.logical_root.join("writes.bin"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        assert_eq!(sandbox_pwrite(descriptor, b"AB".as_ptr().cast(), 2, 2), 2);

        assert_eq!(sandbox_lseek(descriptor, 4, libc::SEEK_SET), 4);
        let sequential = [b"CD".as_slice(), b"EF".as_slice()];
        let sequential = sequential.map(|part| libc::iovec {
            iov_base: part.as_ptr().cast_mut().cast(),
            iov_len: part.len(),
        });
        assert_eq!(
            sandbox_writev(
                descriptor,
                sequential.as_ptr(),
                sequential.len() as libc::c_int,
            ),
            4
        );

        let positioned = [b"GH".as_slice(), b"IJ".as_slice()];
        let positioned = positioned.map(|part| libc::iovec {
            iov_base: part.as_ptr().cast_mut().cast(),
            iov_len: part.len(),
        });
        assert_eq!(
            sandbox_pwritev(
                descriptor,
                positioned.as_ptr(),
                positioned.len() as libc::c_int,
                8,
            ),
            4
        );
        assert_eq!(sandbox_fchmod(descriptor, 0o600), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(sandbox_lseek(descriptor, 0, libc::SEEK_CUR), 8);
        let mut tail = [0_u8; 4];
        assert_eq!(sandbox_read(descriptor, tail.as_mut_ptr().cast(), 4), 4);
        assert_eq!(&tail, b"GHIJ");
        assert_eq!(sandbox_fsync(descriptor), 0);
        assert_eq!(sandbox_close(descriptor), 0);
    });

    assert_eq!(
        nfs.storage.data(0, "writes.bin"),
        Some(b"..ABCDEFGHIJ".to_vec())
    );
}

#[test]
fn nfs_synchronous_open_flushes_each_direct_write() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "synchronous.bin", b"remote");
    let path = Fixture::c_path(&nfs.logical_root.join("synchronous.bin"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR | libc::O_SYNC, 0);
        assert!(descriptor >= 0);
        let flags = libc::fcntl(descriptor, libc::F_GETFL);
        assert_ne!(flags & libc::O_SYNC, 0);
        let flushes = nfs.storage.flush_operations();
        assert_eq!(sandbox_pwrite(descriptor, b"sync".as_ptr().cast(), 4, 0), 4);
        assert_eq!(nfs.storage.flush_operations(), flushes + 1);
        materialize_remote_snapshot(descriptor);
        assert_eq!(sandbox_pwrite(descriptor, b"!".as_ptr().cast(), 1, 4), 1);
        assert_eq!(
            nfs.storage.data(0, "synchronous.bin"),
            Some(b"sync!e".to_vec())
        );
        assert_eq!(sandbox_close(descriptor), 0);
    });

    assert_eq!(
        nfs.storage.data(0, "synchronous.bin"),
        Some(b"sync!e".to_vec())
    );
}

#[test]
fn nfs_shared_mmap_writes_back_while_private_mmap_stays_private() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "shared-map.bin", b"original");
    nfs.storage.insert_file(0, "private-map.bin", b"original");
    let shared = Fixture::c_path(&nfs.logical_root.join("shared-map.bin"));
    let private = Fixture::c_path(&nfs.logical_root.join("private-map.bin"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(shared.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        let mapping = sandbox_mmap(
            std::ptr::null_mut(),
            8,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            descriptor,
            0,
        );
        assert_ne!(mapping, libc::MAP_FAILED);
        std::ptr::copy_nonoverlapping(b"sandbox!".as_ptr(), mapping.cast(), 8);
        assert_eq!(sandbox_msync(mapping, 8, libc::MS_SYNC), 0);
        assert_eq!(
            nfs.storage.data(0, "shared-map.bin"),
            Some(b"sandbox!".to_vec())
        );
        assert_eq!(sandbox_munmap(mapping, 8), 0);
        assert_eq!(sandbox_close(descriptor), 0);

        let descriptor = sandbox_open_with_mode(private.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        let mapping = sandbox_mmap(
            std::ptr::null_mut(),
            8,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
            descriptor,
            0,
        );
        assert_ne!(mapping, libc::MAP_FAILED);
        std::ptr::copy_nonoverlapping(b"private!".as_ptr(), mapping.cast(), 8);
        assert_eq!(sandbox_munmap(mapping, 8), 0);
        assert_eq!(sandbox_close(descriptor), 0);
    });

    assert_eq!(
        nfs.storage.data(0, "private-map.bin"),
        Some(b"original".to_vec())
    );
}

#[test]
fn nfs_shared_mapping_changed_after_native_mprotect_is_flushed_on_normal_exit() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "protected-map.bin", b"original");
    let path = Fixture::c_path(&nfs.logical_root.join("protected-map.bin"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        let mapping = sandbox_mmap(
            std::ptr::null_mut(),
            8,
            libc::PROT_READ,
            libc::MAP_SHARED,
            descriptor,
            0,
        );
        assert_ne!(mapping, libc::MAP_FAILED);
        assert_eq!(
            libc::mprotect(mapping, 8, libc::PROT_READ | libc::PROT_WRITE),
            0
        );
        std::ptr::copy_nonoverlapping(b"sandbox!".as_ptr(), mapping.cast(), 8);

        flush_at_exit();

        assert_eq!(
            nfs.storage.data(0, "protected-map.bin"),
            Some(b"sandbox!".to_vec())
        );
        assert_eq!(libc::close(descriptor), 0);
        assert_eq!(libc::munmap(mapping, 8), 0);
    });
}

#[test]
fn nfs_clean_shared_mapping_does_not_conflict_with_an_external_change() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "clean-map.bin", b"original");
    let path = Fixture::c_path(&nfs.logical_root.join("clean-map.bin"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        let mapping = sandbox_mmap(
            std::ptr::null_mut(),
            8,
            libc::PROT_READ,
            libc::MAP_SHARED,
            descriptor,
            0,
        );
        assert_ne!(mapping, libc::MAP_FAILED);
        assert_eq!(*mapping.cast::<u8>(), b'o');
        nfs.storage.replace(0, "clean-map.bin", b"external");

        assert_eq!(sandbox_munmap(mapping, 8), 0);
        assert_eq!(sandbox_close(descriptor), 0);
    });

    assert_eq!(
        nfs.storage.data(0, "clean-map.bin"),
        Some(b"external".to_vec())
    );
}

#[test]
fn nfs_private_mmap_materializes_only_its_file_range() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
    let mut contents = vec![b'a'; page * 3];
    contents[page..page * 2].fill(b'b');
    contents[page * 2..].fill(b'c');
    nfs.storage.insert_file(0, "large-map.bin", &contents);
    let path = Fixture::c_path(&nfs.logical_root.join("large-map.bin"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        let mapping = sandbox_mmap(
            std::ptr::null_mut(),
            page,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            descriptor,
            page as libc::off_t,
        );
        assert_ne!(mapping, libc::MAP_FAILED);
        assert_eq!(*mapping.cast::<u8>(), b'b');
        assert_eq!(sandbox_munmap(mapping, page), 0);
        let mut beginning = [0_u8; 4];
        assert_eq!(
            sandbox_pread(
                descriptor,
                beginning.as_mut_ptr().cast(),
                beginning.len(),
                0
            ),
            beginning.len() as libc::ssize_t
        );
        assert_eq!(&beginning, b"aaaa");
        assert_eq!(sandbox_close(descriptor), 0);
    });

    let mapped = crate::filesystem::ByteRange {
        start: page as u64,
        end: (page * 2) as u64,
    };
    let mut resident = ByteRangeSet::default();
    resident.insert(mapped);
    let read_ahead = crate::filesystem::ByteRange {
        start: 0,
        end: (16 * 1024_u64).min((page * 3) as u64),
    };
    let mut expected_ranges = vec![mapped];
    expected_ranges.extend(resident.missing(read_ahead));
    assert_eq!(nfs.storage.read_ranges(), expected_ranges);
}

#[test]
fn nfs_snapshot_write_preserves_unmaterialized_remote_ranges() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
    let contents = vec![b'a'; page * 3];
    nfs.storage.insert_file(0, "partial-write.bin", &contents);
    let path = Fixture::c_path(&nfs.logical_root.join("partial-write.bin"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        let mapping = sandbox_mmap(
            std::ptr::null_mut(),
            page,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            descriptor,
            page as libc::off_t,
        );
        assert_ne!(mapping, libc::MAP_FAILED);
        assert_eq!(*mapping.cast::<u8>(), b'a');
        assert_eq!(sandbox_munmap(mapping, page), 0);
        assert_eq!(sandbox_pwrite(descriptor, b"z".as_ptr().cast(), 1, 0), 1);
        assert_eq!(sandbox_fsync(descriptor), 0);
        assert_eq!(sandbox_close(descriptor), 0);
    });

    let mut expected = contents;
    expected[0] = b'z';
    assert_eq!(nfs.storage.data(0, "partial-write.bin"), Some(expected));
    assert_eq!(
        nfs.storage.read_ranges(),
        vec![
            crate::filesystem::ByteRange {
                start: page as u64,
                end: (page * 2) as u64,
            },
            crate::filesystem::ByteRange {
                start: 1,
                end: page as u64,
            },
            crate::filesystem::ByteRange {
                start: (page * 2) as u64,
                end: (page * 3) as u64,
            },
        ]
    );
}

#[test]
fn nfs_async_writes_transition_to_a_writeback_snapshot() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "lio-write.bin", b"original");
    nfs.storage.insert_file(0, "aio-write.bin", b"original");
    let path = Fixture::c_path(&nfs.logical_root.join("lio-write.bin"));
    let aio_path = Fixture::c_path(&nfs.logical_root.join("aio-write.bin"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        let contents = *b"async";
        let mut control = std::mem::zeroed::<libc::aiocb>();
        control.aio_fildes = descriptor;
        control.aio_offset = 0;
        control.aio_buf = contents.as_ptr().cast_mut().cast();
        control.aio_nbytes = contents.len();
        control.aio_lio_opcode = libc::LIO_WRITE;
        let mut controls = [std::ptr::addr_of_mut!(control)];

        assert_eq!(
            sandbox_lio_listio(
                libc::LIO_WAIT,
                controls.as_mut_ptr(),
                controls.len() as libc::c_int,
                std::ptr::null_mut(),
            ),
            0
        );
        assert_eq!(
            libc::aio_return(&mut control),
            contents.len() as libc::ssize_t
        );
        assert_eq!(sandbox_close(descriptor), 0);

        let descriptor = sandbox_open_with_mode(aio_path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        let mut control = std::mem::zeroed::<libc::aiocb>();
        control.aio_fildes = descriptor;
        control.aio_offset = 0;
        control.aio_buf = contents.as_ptr().cast_mut().cast();
        control.aio_nbytes = contents.len();
        assert_eq!(sandbox_aio_write(&mut control), 0);
        while libc::aio_error(&control) == libc::EINPROGRESS {
            let controls = [std::ptr::addr_of!(control)];
            assert_eq!(
                libc::aio_suspend(
                    controls.as_ptr(),
                    controls.len() as libc::c_int,
                    std::ptr::null(),
                ),
                0
            );
        }
        assert_eq!(
            libc::aio_return(&mut control),
            contents.len() as libc::ssize_t
        );
        assert_eq!(sandbox_close(descriptor), 0);
    });

    assert_eq!(
        nfs.storage.data(0, "lio-write.bin"),
        Some(b"asyncnal".to_vec())
    );
    assert_eq!(
        nfs.storage.data(0, "aio-write.bin"),
        Some(b"asyncnal".to_vec())
    );
}

#[test]
fn metadata_only_remote_stat_releases_its_temporary_anchor() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "file.txt", b"remote");
    let path = Fixture::c_path(&nfs.logical_root.join("file.txt"));

    with_test_runtime(&fixture.runtime, || unsafe {
        for _ in 0..8 {
            let mut status = std::mem::zeroed();
            assert_eq!(sandbox_stat(path.as_ptr(), &mut status), 0);
        }
    });

    assert_eq!(nfs.anchor_count(), 0);
}

#[test]
fn nfs_existing_open_uses_one_remote_lookup() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "file.txt", b"remote content");
    let path = Fixture::c_path(&nfs.logical_root.join("file.txt"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        assert_eq!(sandbox_close(descriptor), 0);
    });

    assert_eq!(nfs.storage.stat_operations(), 1);
}

#[test]
fn nfs_root_is_visible_in_its_parent_directory() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    let parent = Fixture::c_path(nfs.logical_root.parent().unwrap());

    with_test_runtime(&fixture.runtime, || unsafe {
        let directory = sandbox_opendir(parent.as_ptr());
        assert!(!directory.is_null());
        let mut names = Vec::new();
        loop {
            let entry = sandbox_readdir(directory);
            if entry.is_null() {
                break;
            }
            names.push(CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes().to_vec());
        }
        assert_eq!(sandbox_closedir(directory), 0);
        assert_eq!(
            names
                .iter()
                .filter(|name| name.as_slice() == b"network")
                .count(),
            1
        );
    });

    assert!(!nfs.logical_root.exists());
}

#[test]
fn nfs_root_is_visible_to_fts_in_its_parent_directory() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    let parent = nfs.logical_root.parent().unwrap();
    let parent = Fixture::c_path(parent);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor =
            sandbox_open_with_mode(parent.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY, 0);
        assert!(descriptor >= 0);
        let names = fts_bulk_entry_names_for_test(&fixture.runtime, descriptor).unwrap();
        assert_eq!(sandbox_close(descriptor), 0);
        assert_eq!(
            names
                .iter()
                .filter(|name| name.as_slice() == b"network")
                .count(),
            1
        );
    });
    assert!(!nfs.logical_root.exists());
}

#[test]
fn fts_bulk_attributes_merge_remote_upper_and_lower_entries_with_bounded_buffers() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    let directory = nfs.logical_root.join("docs");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("shared.txt"), b"lower shared").unwrap();
    std::fs::write(directory.join("lower.txt"), b"lower").unwrap();
    let upper = directory.join("upper.txt");
    let upper_destination = fixture
        .runtime
        .filesystem
        .prepare_write(&upper, true)
        .unwrap();
    std::fs::write(upper_destination, b"upper").unwrap();
    nfs.storage.insert_directory(0, "docs");
    nfs.storage.insert_directory(0, "docs/remote-dir");
    nfs.storage
        .insert_file(0, "docs/shared.txt", b"remote shared");
    let path = Fixture::c_path(&directory);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor =
            sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY, 0);
        assert!(descriptor >= 0);
        let mut attributes = std::mem::zeroed::<libc::attrlist>();
        attributes.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        attributes.commonattr =
            libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE;
        let mut buffer = vec![0_u8; 4096];
        assert_eq!(
            sandbox_getattrlistbulk(
                descriptor,
                (&raw mut attributes).cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            ),
            4
        );
        assert_eq!(
            sandbox_getattrlistbulk(
                descriptor,
                (&raw mut attributes).cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            ),
            0
        );
        assert_eq!(sandbox_close(descriptor), 0);

        let descriptor =
            sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY, 0);
        assert!(descriptor >= 0);
        assert_eq!(
            sandbox_getattrlistbulk(
                descriptor,
                (&raw mut attributes).cast(),
                buffer.as_mut_ptr().cast(),
                1,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ERANGE);
        assert_eq!(
            sandbox_getattrlistbulk(
                descriptor,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::EFAULT);
        assert_eq!(sandbox_close(descriptor), 0);
    });
}

#[test]
fn nfs_child_from_fts_read_is_not_filtered() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_directory(0, "test");
    nfs.storage.insert_file(0, "test/file.txt", b"content");
    let child = nfs.logical_root.join("test/file.txt");

    with_test_runtime(&fixture.runtime, || {
        assert!(fts_read_returns_virtual_entry_for_test(&child).unwrap());
    });
}

#[test]
fn nfs_directory_from_fts_read_uses_its_remote_anchor_for_descent() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_directory(0, "test");
    nfs.storage.insert_directory(0, "test/nested");
    let directory = nfs.logical_root.join("test/nested");

    with_test_runtime(&fixture.runtime, || {
        let descent = fts_directory_descent_path_for_test(&directory).unwrap();
        let descent = Path::new(std::ffi::OsStr::from_bytes(&descent));
        assert_eq!(
            descent.parent().unwrap().canonicalize().unwrap(),
            nfs._runtime_directory.path().canonicalize().unwrap()
        );
        assert!(
            descent
                .file_name()
                .unwrap()
                .as_bytes()
                .starts_with(b"anchor-")
        );
    });
}

#[test]
fn nfs_stat_releases_each_temporary_anchor_after_the_native_call() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    for index in 0..128 {
        nfs.storage
            .insert_file(0, &format!("file-{index}"), b"content");
    }

    with_test_runtime(&fixture.runtime, || unsafe {
        for index in 0..128 {
            let path = Fixture::c_path(&nfs.logical_root.join(format!("file-{index}")));
            let mut status = std::mem::zeroed();
            assert_eq!(sandbox_stat(path.as_ptr(), &mut status), 0);
        }
    });

    assert_eq!(
        std::fs::read_dir(nfs._runtime_directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().as_encoded_bytes().starts_with(b"anchor-"))
            .count(),
        0
    );
}

#[test]
fn nfs_entries_override_overlay_entries_and_missing_entries_fall_back() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    let local_directory = nfs.logical_root.join("docs");
    std::fs::create_dir_all(&local_directory).unwrap();
    std::fs::write(local_directory.join("shared.txt"), b"lower shared").unwrap();
    std::fs::write(local_directory.join("lower.txt"), b"lower only").unwrap();
    std::fs::write(local_directory.join("upper.txt"), b"lower baseline").unwrap();
    let whiteouted_logical = local_directory.join("whiteouted.txt");
    std::fs::write(&whiteouted_logical, b"hidden lower").unwrap();
    fixture
        .runtime
        .filesystem
        .remove(&whiteouted_logical, false)
        .unwrap();
    std::fs::create_dir(local_directory.join("local-dir")).unwrap();
    nfs.storage.insert_directory(0, "docs");
    nfs.storage
        .insert_file(0, "docs/shared.txt", b"remote shared");
    nfs.storage
        .insert_file(0, "docs/remote.txt", b"remote only");
    nfs.storage
        .insert_file(0, "docs/whiteouted.txt", b"remote over whiteout");

    let shared = Fixture::c_path(&local_directory.join("shared.txt"));
    let lower = Fixture::c_path(&local_directory.join("lower.txt"));
    let upper = Fixture::c_path(&local_directory.join("upper.txt"));
    let whiteouted = Fixture::c_path(&whiteouted_logical);
    let local_dir = Fixture::c_path(&local_directory.join("local-dir"));
    let docs = Fixture::c_path(&local_directory);

    with_test_runtime(&fixture.runtime, || unsafe {
        let read = |path: &CString, expected: &[u8]| {
            let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
            assert!(descriptor >= 0);
            let mut content = vec![0_u8; expected.len()];
            assert_eq!(
                sandbox_read(descriptor, content.as_mut_ptr().cast(), content.len()),
                expected.len() as isize
            );
            assert_eq!(content, expected);
            assert_eq!(sandbox_close(descriptor), 0);
        };

        read(&shared, b"remote shared");
        read(&lower, b"lower only");
        read(&whiteouted, b"remote over whiteout");
        let mut status = std::mem::zeroed();
        assert_eq!(sandbox_stat(shared.as_ptr(), &mut status), 0);
        assert_eq!(status.st_size, 13);
        assert_eq!(sandbox_stat(lower.as_ptr(), &mut status), 0);
        assert_eq!(status.st_size, 10);
        assert_eq!(sandbox_access(lower.as_ptr(), libc::R_OK), 0);
        assert_eq!(sandbox_chmod(lower.as_ptr(), 0o600), 0);
        assert_eq!(sandbox_mkdir(local_dir.as_ptr(), 0o755), -1);
        assert_eq!(*libc::__error(), libc::EEXIST);
        assert!(!nfs.storage.exists(0, "docs/local-dir"));

        let descriptor = sandbox_open_with_mode(upper.as_ptr(), libc::O_RDWR | libc::O_TRUNC, 0);
        assert!(descriptor >= 0);
        assert_eq!(
            sandbox_write(descriptor, b"upper only".as_ptr().cast(), 10),
            10
        );
        assert_eq!(sandbox_close(descriptor), 0);
        assert_eq!(nfs.storage.data(0, "docs/upper.txt"), None);
        assert_eq!(
            fixture
                .runtime
                .filesystem
                .state_for_test(&local_directory.join("upper.txt"))
                .unwrap(),
            Some(EntryState::Cow)
        );
        read(&upper, b"upper only");

        nfs.storage
            .insert_file(0, "docs/upper.txt", b"remote upper");
        read(&upper, b"remote upper");

        let directory = sandbox_opendir(docs.as_ptr());
        assert!(!directory.is_null());
        let mut names = HashSet::new();
        loop {
            let entry = sandbox_readdir(directory);
            if entry.is_null() {
                break;
            }
            names.insert(
                CStr::from_ptr((*entry).d_name.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        assert_eq!(
            names,
            HashSet::from([
                ".".into(),
                "..".into(),
                "shared.txt".into(),
                "local-dir".into(),
                "lower.txt".into(),
                "upper.txt".into(),
                "whiteouted.txt".into(),
                "remote.txt".into(),
            ])
        );
        assert_eq!(sandbox_closedir(directory), 0);

        let fts_descriptor =
            sandbox_open_with_mode(docs.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY, 0);
        assert!(fts_descriptor >= 0);
        let fts_names = fts_bulk_entry_names_for_test(&fixture.runtime, fts_descriptor)
            .unwrap()
            .into_iter()
            .map(|name| String::from_utf8(name).unwrap())
            .collect::<HashSet<_>>();
        assert_eq!(
            fts_names,
            HashSet::from([
                "shared.txt".into(),
                "local-dir".into(),
                "lower.txt".into(),
                "upper.txt".into(),
                "whiteouted.txt".into(),
                "remote.txt".into(),
            ])
        );
        assert_eq!(sandbox_close(fts_descriptor), 0);

        let descriptor =
            sandbox_open_with_mode(docs.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY, 0);
        assert!(descriptor >= 0);
        let child = sandbox_openat_with_mode(descriptor, c"lower.txt".as_ptr(), libc::O_RDONLY, 0);
        assert!(child >= 0);
        let mut content = [0_u8; 10];
        assert_eq!(sandbox_read(child, content.as_mut_ptr().cast(), 10), 10);
        assert_eq!(&content, b"lower only");
        assert_eq!(sandbox_close(child), 0);
        let child = sandbox_openat_with_mode(descriptor, c"shared.txt".as_ptr(), libc::O_RDONLY, 0);
        assert!(child >= 0);
        let mut content = [0_u8; 13];
        assert_eq!(sandbox_read(child, content.as_mut_ptr().cast(), 13), 13);
        assert_eq!(&content, b"remote shared");
        assert_eq!(sandbox_close(child), 0);
        let directory = sandbox_fdopendir(descriptor);
        assert!(!directory.is_null());
        let mut names = HashSet::new();
        loop {
            let entry = sandbox_readdir(directory);
            if entry.is_null() {
                break;
            }
            names.insert(
                CStr::from_ptr((*entry).d_name.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        assert!(names.contains("remote.txt"));
        assert!(names.contains("lower.txt"));
        assert_eq!(sandbox_closedir(directory), 0);

        assert_eq!(sandbox_unlink(shared.as_ptr()), 0);
        read(&shared, b"lower shared");
        assert_eq!(sandbox_stat(shared.as_ptr(), &mut status), 0);
        assert_eq!(status.st_size, 12);

        assert_eq!(sandbox_unlink(upper.as_ptr()), 0);
        read(&upper, b"upper only");

        assert_eq!(sandbox_unlink(whiteouted.as_ptr()), 0);
        assert_eq!(
            sandbox_open_with_mode(whiteouted.as_ptr(), libc::O_RDONLY, 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOENT);
    });
}

#[test]
fn nfs_fclose_keeps_the_stream_open_when_writeback_conflicts() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "shared.txt", b"original");
    let path = Fixture::c_path(&nfs.logical_root.join("shared.txt"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let stream = sandbox_fopen(path.as_ptr(), c"r+".as_ptr());
        assert!(!stream.is_null());
        let descriptor = libc::fileno(stream);
        assert!(descriptor >= 0);
        materialize_remote_snapshot(descriptor);
        assert_eq!(
            sandbox_pwrite(descriptor, b"sandbox".as_ptr().cast(), 7, 0),
            7
        );
        nfs.storage.replace(0, "shared.txt", b"outside");

        assert_eq!(sandbox_fclose(stream), -1);
        assert_eq!(*libc::__error(), libc::ESTALE);
        assert!(fixture.runtime.tracked_open(descriptor).is_some());
        assert_ne!(libc::fcntl(descriptor, libc::F_GETFD), -1);

        fixture.runtime.take_descriptor(descriptor);
        assert_eq!(libc::fclose(stream), 0);
    });
    assert_eq!(nfs.storage.data(0, "shared.txt"), Some(b"outside".to_vec()));
}

#[test]
fn nfs_sync_close_and_dup2_preserve_descriptors_after_writeback_conflicts() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "conflict.txt", b"original");
    let path = Fixture::c_path(&nfs.logical_root.join("conflict.txt"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        materialize_remote_snapshot(descriptor);
        assert_eq!(
            sandbox_pwrite(descriptor, b"sandbox".as_ptr().cast(), 7, 0),
            7
        );
        nfs.storage.replace(0, "conflict.txt", b"outside");

        assert_eq!(sandbox_fsync(descriptor), -1);
        assert_eq!(*libc::__error(), libc::ESTALE);
        assert!(fixture.runtime.tracked_open(descriptor).is_some());

        let source = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        assert!(source >= 0);
        assert_eq!(sandbox_dup2(source, descriptor), -1);
        assert_eq!(*libc::__error(), libc::ESTALE);
        assert!(fixture.runtime.tracked_open(descriptor).is_some());

        assert_eq!(sandbox_close(descriptor), -1);
        assert_eq!(*libc::__error(), libc::ESTALE);
        assert!(fixture.runtime.tracked_open(descriptor).is_some());
        fixture.runtime.take_descriptor(descriptor);
        assert_eq!(libc::close(descriptor), 0);
        assert_eq!(libc::close(source), 0);
    });

    assert_eq!(
        nfs.storage.data(0, "conflict.txt"),
        Some(b"outside".to_vec())
    );
}

#[test]
fn nfs_dup2_closes_the_replaced_remote_handle() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "destination.txt", b"original");
    let destination_path = Fixture::c_path(&nfs.logical_root.join("destination.txt"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let destination = sandbox_open_with_mode(destination_path.as_ptr(), libc::O_RDWR, 0);
        assert!(destination >= 0);
        assert_eq!(sandbox_ftruncate(destination, 0), 0);
        assert_eq!(sandbox_write(destination, b"saved".as_ptr().cast(), 5), 5);
        let old_handle = fixture
            .runtime
            .tracked_open(destination)
            .unwrap()
            .managed_handle()
            .unwrap()
            .to_owned();
        let source = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        assert!(source >= 0);

        assert_eq!(sandbox_dup2(source, destination), destination);
        assert_eq!(
            nfs.storage.data(0, "destination.txt"),
            Some(b"saved".to_vec())
        );
        fixture
            .runtime
            .remote
            .as_ref()
            .unwrap()
            .close(&old_handle, Vec::new())
            .unwrap();

        assert_eq!(sandbox_close(destination), 0);
        assert_eq!(sandbox_close(source), 0);
    });
}

#[test]
fn nfs_unlink_discards_an_open_snapshot_without_recreating_the_path() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "open.txt", b"original");
    let path = Fixture::c_path(&nfs.logical_root.join("open.txt"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDWR | libc::O_TRUNC, 0);
        assert!(descriptor >= 0);
        assert_eq!(
            sandbox_write(descriptor, b"discarded".as_ptr().cast(), 9),
            9
        );
        assert_eq!(sandbox_unlink(path.as_ptr()), 0);
        assert_eq!(sandbox_close(descriptor), 0);
    });

    assert!(!nfs.storage.exists(0, "open.txt"));
}

#[test]
fn nfs_failed_open_audit_aborts_a_staged_remote_create() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    fixture.runtime.audit = Some(AuditClient::new(address, "audit-token"));
    let path = Fixture::c_path(&nfs.logical_root.join("denied.txt"));

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(
            sandbox_open_with_mode(
                path.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o600,
            ),
            -1
        );
    });

    assert!(!nfs.storage.exists(0, "denied.txt"));
}

#[test]
fn nfs_failed_open_audit_does_not_truncate_an_existing_remote_file() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "preserved.txt", b"preserved");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    fixture.runtime.audit = Some(AuditClient::new(address, "audit-token"));
    let path = Fixture::c_path(&nfs.logical_root.join("preserved.txt"));

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(
            sandbox_open_with_mode(path.as_ptr(), libc::O_WRONLY | libc::O_TRUNC, 0),
            -1
        );
    });

    assert_eq!(
        nfs.storage.data(0, "preserved.txt"),
        Some(b"preserved".to_vec())
    );
}

#[test]
fn nfs_directories_and_namespace_operations_use_remote_entries() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_directory(0, "docs");
    nfs.storage.insert_file(0, "docs/a.txt", b"a");
    nfs.storage.insert_file(0, "cross.txt", b"cross");
    let docs = Fixture::c_path(&nfs.logical_root.join("docs"));
    let source = Fixture::c_path(&nfs.logical_root.join("docs/a.txt"));
    let renamed = Fixture::c_path(&nfs.logical_root.join("docs/b.txt"));
    let created = Fixture::c_path(&nfs.logical_root.join("empty"));
    let cross = Fixture::c_path(&nfs.logical_root.join("cross.txt"));
    let missing_link = Fixture::c_path(&nfs.logical_root.join("missing-link"));
    let local_source_path = nfs.logical_root.join("local-source.txt");
    std::fs::create_dir_all(&nfs.logical_root).unwrap();
    std::fs::write(&local_source_path, b"local").unwrap();
    let local_source = Fixture::c_path(&local_source_path);
    let local = Fixture::c_path(&fixture.lower.join("local.txt"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let directory = sandbox_opendir(docs.as_ptr());
        assert!(!directory.is_null());
        let mut names = HashSet::new();
        loop {
            let entry = sandbox_readdir(directory);
            if entry.is_null() {
                break;
            }
            names.insert(
                CStr::from_ptr((*entry).d_name.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        assert_eq!(
            names,
            HashSet::from([".".into(), "..".into(), "a.txt".into()])
        );
        sandbox_rewinddir(directory);
        assert!(!sandbox_readdir(directory).is_null());
        assert_eq!(sandbox_closedir(directory), 0);

        let descriptor =
            sandbox_open_with_mode(docs.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY, 0);
        assert!(descriptor >= 0);
        let directory = sandbox_fdopendir(descriptor);
        assert!(!directory.is_null());
        assert!(!sandbox_readdir(directory).is_null());
        assert_eq!(sandbox_closedir(directory), 0);

        assert_eq!(sandbox_mkdir(created.as_ptr(), 0o755), 0);
        assert!(nfs.storage.exists(0, "empty"));
        assert_eq!(sandbox_rmdir(created.as_ptr()), 0);
        assert!(!nfs.storage.exists(0, "empty"));
        assert_eq!(sandbox_mkdir(docs.as_ptr(), 0o755), -1);
        assert_eq!(*libc::__error(), libc::EEXIST);

        assert_eq!(sandbox_rename(source.as_ptr(), renamed.as_ptr()), 0);
        assert!(!nfs.storage.exists(0, "docs/a.txt"));
        assert!(nfs.storage.exists(0, "docs/b.txt"));
        assert_eq!(sandbox_unlink(renamed.as_ptr()), 0);
        assert!(!nfs.storage.exists(0, "docs/b.txt"));

        assert_eq!(sandbox_chmod(cross.as_ptr(), 0o600), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(sandbox_symlink(c"target".as_ptr(), cross.as_ptr()), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            sandbox_symlink(c"target".as_ptr(), missing_link.as_ptr()),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(sandbox_rename(local_source.as_ptr(), cross.as_ptr()), -1);
        assert_eq!(*libc::__error(), libc::EXDEV);
        assert_eq!(sandbox_rename(cross.as_ptr(), local.as_ptr()), -1);
        assert_eq!(*libc::__error(), libc::EXDEV);
        assert!(nfs.storage.exists(0, "cross.txt"));

        assert_eq!(sandbox_chdir(cross.as_ptr()), -1);
        assert_eq!(*libc::__error(), libc::ENOTDIR);
        let canonical = sandbox_realpath(cross.as_ptr(), std::ptr::null_mut());
        assert!(!canonical.is_null());
        assert_eq!(
            CStr::from_ptr(canonical).to_bytes(),
            nfs.logical_root
                .join("cross.txt")
                .as_os_str()
                .as_encoded_bytes()
        );
        libc::free(canonical.cast());
    });

    std::fs::remove_dir_all(&nfs.logical_root).unwrap();
    assert!(!nfs.logical_root.exists());
    assert_eq!(
        fixture
            .runtime
            .filesystem
            .state_for_test(&nfs.logical_root.join("docs"))
            .unwrap(),
        None
    );
}

#[test]
fn nfs_opendir_descriptors_support_fchdir() {
    struct RestoreDirectory(PathBuf);

    impl Drop for RestoreDirectory {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).unwrap();
        }
    }

    let original = std::env::current_dir().unwrap();
    let _restore = RestoreDirectory(original.clone());
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_directory(0, "docs");
    let logical = nfs.logical_root.join("docs");
    let local = nfs.logical_root.join("local");
    std::fs::create_dir_all(&local).unwrap();
    let path = Fixture::c_path(&logical);
    let local_path = Fixture::c_path(&local);

    with_test_runtime(&fixture.runtime, || unsafe {
        let directory = sandbox_opendir(path.as_ptr());
        assert!(!directory.is_null());

        assert_eq!(sandbox_fchdir(libc::dirfd(directory)), 0);
        let current = sandbox_getcwd(std::ptr::null_mut(), 0);
        assert!(!current.is_null());
        assert_eq!(
            CStr::from_ptr(current).to_bytes(),
            logical.as_os_str().as_encoded_bytes()
        );
        libc::free(current.cast());

        assert_eq!(sandbox_closedir(directory), 0);
        std::env::set_current_dir(&original).unwrap();
        fixture.runtime.set_current_directory(original.clone());

        let descriptor =
            sandbox_open_with_mode(local_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY, 0);
        assert!(descriptor >= 0);
        assert_eq!(sandbox_fchdir(descriptor), 0);
        let current = sandbox_getcwd(std::ptr::null_mut(), 0);
        assert!(!current.is_null());
        assert_eq!(
            CStr::from_ptr(current).to_bytes(),
            local.as_os_str().as_encoded_bytes()
        );
        libc::free(current.cast());
        assert_eq!(sandbox_close(descriptor), 0);
        std::env::set_current_dir(&original).unwrap();
        fixture.runtime.set_current_directory(original.clone());
    });
}

#[test]
fn nfs_file_descriptors_reject_directory_operations() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_file(0, "file.txt", b"content");
    let path = Fixture::c_path(&nfs.logical_root.join("file.txt"));

    with_test_runtime(&fixture.runtime, || unsafe {
        assert!(sandbox_opendir(path.as_ptr()).is_null());
        assert_eq!(*libc::__error(), libc::ENOTDIR);

        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        assert_eq!(sandbox_fchdir(descriptor), -1);
        assert_eq!(*libc::__error(), libc::ENOTDIR);
        assert!(sandbox_fdopendir(descriptor).is_null());
        assert_eq!(*libc::__error(), libc::ENOTDIR);
        assert_eq!(super::agora_sandbox_validate_content_fcntl(descriptor), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(sandbox_close(descriptor), 0);
    });
}

#[test]
fn nfs_fts_setup_preserves_a_remote_logical_current_directory() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_directory(0, "docs");
    let logical = nfs.logical_root.join("docs");
    fixture.runtime.set_current_directory(logical.clone());

    fixture
        .runtime
        .synchronize_current_directory_for_fts()
        .unwrap();

    let current = fixture.runtime.current_directory.lock().unwrap();
    assert_eq!(current.logical, logical);
    assert!(current.remote);
}

#[test]
fn nfs_directory_rename_retargets_open_descendants() {
    let mut fixture = Fixture::new();
    let nfs = fixture.attach_nfs();
    nfs.storage.insert_directory(0, "tree");
    nfs.storage.insert_file(0, "tree/open.txt", b"old");
    let source_directory = Fixture::c_path(&nfs.logical_root.join("tree"));
    let destination_directory = Fixture::c_path(&nfs.logical_root.join("moved"));
    let file = Fixture::c_path(&nfs.logical_root.join("tree/open.txt"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(file.as_ptr(), libc::O_RDWR | libc::O_TRUNC, 0);
        assert!(descriptor >= 0);
        assert_eq!(sandbox_write(descriptor, b"new".as_ptr().cast(), 3), 3);
        assert_eq!(
            sandbox_rename(source_directory.as_ptr(), destination_directory.as_ptr()),
            0
        );
        assert_eq!(sandbox_fsync(descriptor), 0);
        assert_eq!(sandbox_close(descriptor), 0);
    });

    assert_eq!(nfs.storage.data(0, "moved/open.txt"), Some(b"new".to_vec()));
    assert!(!nfs.storage.exists(0, "tree/open.txt"));
}

#[test]
fn native_passthrough_roots_are_normalized_and_component_safe() {
    let mut fixture = Fixture::new();
    fixture
        .runtime
        .native_passthrough_roots
        .push(PathBuf::from("/opt/agora-tools"));
    assert_eq!(
        fixture
            .runtime
            .native_passthrough_path(Path::new("/dev/./null"))
            .unwrap(),
        Some(PathBuf::from("/dev/null"))
    );
    assert_eq!(
        fixture
            .runtime
            .native_passthrough_path(Path::new("/dev/../private/file"))
            .unwrap(),
        None
    );
    assert_eq!(
        fixture
            .runtime
            .native_passthrough_path(Path::new("/developer"))
            .unwrap(),
        None
    );
    assert_eq!(
        fixture
            .runtime
            .native_passthrough_path(Path::new("/opt/agora-tools/bin/go"))
            .unwrap(),
        Some(PathBuf::from("/opt/agora-tools/bin/go"))
    );
    assert_eq!(
        fixture
            .runtime
            .native_passthrough_path(Path::new("/opt/agora-tools/../private/file"))
            .unwrap(),
        None
    );
    assert_eq!(
        fixture
            .runtime
            .native_passthrough_path(Path::new("/opt/agora-toolsmith"))
            .unwrap(),
        None
    );
}

#[test]
fn allowlisted_device_opens_bypass_audit_and_tracking() {
    let mut fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    fixture.runtime.audit = Some(AuditClient::new(address, "audit-token"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(c"/dev/null".as_ptr(), libc::O_WRONLY, 0);
        assert!(descriptor >= 0);
        assert!(fixture.runtime.tracked_open(descriptor).is_none());
        assert_eq!(libc::write(descriptor, b"x".as_ptr().cast(), 1), 1);
        assert_eq!(sandbox_close(descriptor), 0);

        let directory = libc::open(c"/dev".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
        assert!(directory >= 0);
        let relative = sandbox_openat_with_mode(directory, c"null".as_ptr(), libc::O_WRONLY, 0);
        assert!(relative >= 0);
        assert!(fixture.runtime.tracked_open(relative).is_none());
        assert_eq!(sandbox_close(relative), 0);
        assert_eq!(libc::close(directory), 0);

        let stream = sandbox_fopen(c"/dev/null".as_ptr(), c"w".as_ptr());
        assert!(!stream.is_null());
        assert!(fixture.runtime.tracked_open(libc::fileno(stream)).is_none());
        assert_eq!(sandbox_fclose(stream), 0);

        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        assert_eq!(libc::posix_spawn_file_actions_init(&mut actions), 0);
        assert_eq!(
            sandbox_spawn_addopen(&mut actions, 9, c"/dev/null".as_ptr(), libc::O_WRONLY, 0,),
            0
        );
        assert_eq!(libc::posix_spawn_file_actions_destroy(&mut actions), 0);
    });
}

#[test]
fn root_no_follow_metadata_uses_the_native_root() {
    let fixture = Fixture::new();

    let (mapped, plaintext_size, attributes, _anchor) = fixture
        .runtime
        .map_metadata(
            c"/".as_ptr(),
            libc::AT_FDCWD,
            false,
            &crate::filesystem::Credentials::effective(),
        )
        .unwrap();

    assert_eq!(mapped.as_c_str(), c"/");
    assert_eq!(plaintext_size, None);
    assert_eq!(attributes, None);
}

#[test]
fn allowlisted_device_metadata_and_directories_ignore_overlay_state() {
    let fixture = Fixture::new();
    let mut attributes = FileAttributes::from_metadata(&std::fs::metadata("/dev/null").unwrap());
    attributes.mode = u32::from(libc::S_IFREG) | 0o600;
    fixture
        .runtime
        .filesystem
        .set_attributes(Path::new("/dev/null"), attributes)
        .unwrap();

    let (mapped, plaintext_size, attributes, _anchor) = fixture
        .runtime
        .map_metadata(
            c"/dev/null".as_ptr(),
            libc::AT_FDCWD,
            true,
            &crate::filesystem::Credentials::effective(),
        )
        .unwrap();
    assert_eq!(mapped.as_c_str(), c"/dev/null");
    assert_eq!(plaintext_size, None);
    assert_eq!(attributes, None);

    let view = fixture.runtime.directory_view(c"/dev".as_ptr()).unwrap();
    assert!(view.is_passthrough());
    assert_eq!(view.primary(), Path::new("/dev"));
}

#[test]
fn allowlisted_device_truncate_bypasses_audit() {
    let path = c"/dev/null";
    let expected = unsafe { libc::truncate(path.as_ptr(), 0) };
    let expected_errno = unsafe { *libc::__error() };

    let mut fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    fixture.runtime.audit = Some(AuditClient::new(address, "audit-token"));

    with_test_runtime(&fixture.runtime, || unsafe {
        let actual = sandbox_truncate(path.as_ptr(), 0);
        let actual_errno = *libc::__error();
        assert_eq!(actual, expected);
        if expected == -1 {
            assert_eq!(actual_errno, expected_errno);
        }
    });
}

#[test]
fn allowlisted_device_mutations_use_native_libc_semantics() {
    let fixture = Fixture::new();
    let missing = CString::new(format!(
        "/dev/agora-sandbox-missing-{}",
        uuid::Uuid::new_v4()
    ))
    .unwrap();
    let expected_utimes = unsafe { libc::utimes(missing.as_ptr(), std::ptr::null()) };
    let expected_utimes_errno = unsafe { *libc::__error() };
    let nested = c"/dev/null/agora-sandbox-probe";
    let nested_destination = c"/dev/null/agora-sandbox-destination";
    let expected_chmod = unsafe { libc::chmod(nested.as_ptr(), 0o600) };
    let expected_chmod_errno = unsafe { *libc::__error() };
    let expected_mkdir = unsafe { libc::mkdir(nested.as_ptr(), 0o700) };
    let expected_mkdir_errno = unsafe { *libc::__error() };
    let expected_symlink = unsafe { libc::symlink(c"target".as_ptr(), nested.as_ptr()) };
    let expected_symlink_errno = unsafe { *libc::__error() };
    let expected_symlinkat =
        unsafe { libc::symlinkat(c"target".as_ptr(), libc::AT_FDCWD, nested.as_ptr()) };
    let expected_symlinkat_errno = unsafe { *libc::__error() };
    let expected_unlinkat = unsafe { libc::unlinkat(libc::AT_FDCWD, nested.as_ptr(), 0) };
    let expected_unlinkat_errno = unsafe { *libc::__error() };
    let expected_link = unsafe { libc::link(nested.as_ptr(), nested_destination.as_ptr()) };
    let expected_link_errno = unsafe { *libc::__error() };
    let expected_rename = unsafe { libc::rename(nested.as_ptr(), nested_destination.as_ptr()) };
    let expected_rename_errno = unsafe { *libc::__error() };
    let expected_renamex =
        unsafe { libc::renamex_np(nested.as_ptr(), nested_destination.as_ptr(), 0) };
    let expected_renamex_errno = unsafe { *libc::__error() };
    let expected_renameatx = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            nested.as_ptr(),
            libc::AT_FDCWD,
            nested_destination.as_ptr(),
            0,
        )
    };
    let expected_renameatx_errno = unsafe { *libc::__error() };

    with_test_runtime(&fixture.runtime, || unsafe {
        let actual_utimes = sandbox_utimes(missing.as_ptr(), std::ptr::null());
        assert_eq!(actual_utimes, expected_utimes);
        assert_eq!(*libc::__error(), expected_utimes_errno);
        assert_eq!(
            fixture
                .runtime
                .filesystem
                .state_for_test(Path::new(missing.to_str().unwrap()))
                .unwrap(),
            None
        );

        assert_eq!(sandbox_chmod(nested.as_ptr(), 0o600), expected_chmod);
        assert_eq!(*libc::__error(), expected_chmod_errno);
        assert_eq!(sandbox_mkdir(nested.as_ptr(), 0o700), expected_mkdir);
        assert_eq!(*libc::__error(), expected_mkdir_errno);
        assert_eq!(
            sandbox_symlink(c"target".as_ptr(), nested.as_ptr()),
            expected_symlink
        );
        assert_eq!(*libc::__error(), expected_symlink_errno);
        assert_eq!(
            sandbox_symlinkat(c"target".as_ptr(), libc::AT_FDCWD, nested.as_ptr()),
            expected_symlinkat
        );
        assert_eq!(*libc::__error(), expected_symlinkat_errno);
        assert_eq!(
            sandbox_unlinkat(libc::AT_FDCWD, nested.as_ptr(), 0),
            expected_unlinkat
        );
        assert_eq!(*libc::__error(), expected_unlinkat_errno);
        assert_eq!(
            sandbox_link(nested.as_ptr(), nested_destination.as_ptr()),
            expected_link
        );
        assert_eq!(*libc::__error(), expected_link_errno);
        assert_eq!(
            sandbox_rename(nested.as_ptr(), nested_destination.as_ptr()),
            expected_rename
        );
        assert_eq!(*libc::__error(), expected_rename_errno);
        assert_eq!(
            sandbox_renamex_np(nested.as_ptr(), nested_destination.as_ptr(), 0),
            expected_renamex
        );
        assert_eq!(*libc::__error(), expected_renamex_errno);
        assert_eq!(
            sandbox_renameatx_np(
                libc::AT_FDCWD,
                nested.as_ptr(),
                libc::AT_FDCWD,
                nested_destination.as_ptr(),
                0,
            ),
            expected_renameatx
        );
        assert_eq!(*libc::__error(), expected_renameatx_errno);

        let mut resolved = [0; libc::PATH_MAX as usize];
        let result = sandbox_realpath(c"/dev/null".as_ptr(), resolved.as_mut_ptr());
        assert!(!result.is_null());
        assert_eq!(CStr::from_ptr(result), c"/dev/null");

        let mixed_destination = Fixture::c_path(&fixture.lower.join("mixed-destination"));
        assert_eq!(
            sandbox_rename(nested.as_ptr(), mixed_destination.as_ptr()),
            -1
        );
        assert_eq!(*libc::__error(), libc::EXDEV);
        assert_eq!(
            fixture
                .runtime
                .filesystem
                .state_for_test(Path::new("/dev/null/agora-sandbox-probe"))
                .unwrap(),
            None
        );

        let descriptor = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
        assert!(descriptor >= 0);
        let expected_ftruncate = libc::ftruncate(descriptor, 0);
        let expected_ftruncate_errno = *libc::__error();
        let actual_ftruncate = sandbox_ftruncate(descriptor, 0);
        assert_eq!(actual_ftruncate, expected_ftruncate);
        if expected_ftruncate == -1 {
            assert_eq!(*libc::__error(), expected_ftruncate_errno);
        }
        assert_eq!(libc::close(descriptor), 0);

        let stream = libc::tmpfile();
        assert!(!stream.is_null());
        let reopened = sandbox_freopen(c"/dev/null".as_ptr(), c"w".as_ptr(), stream);
        assert!(!reopened.is_null());
        assert_eq!(libc::fclose(reopened), 0);
    });
}

fn audit_server(
    first_response: &'static str,
    request_count: usize,
) -> (AuditClient, thread::JoinHandle<Vec<serde_json::Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        for request in 0..request_count {
            let mut prefix = [0_u8; 4];
            stream.read_exact(&mut prefix).unwrap();
            let mut frame = vec![0_u8; u32::from_be_bytes(prefix) as usize];
            stream.read_exact(&mut frame).unwrap();
            requests.push(serde_json::from_slice(&frame).unwrap());
            if request == 0 {
                stream
                    .write_all(&(first_response.len() as u32).to_be_bytes())
                    .unwrap();
                stream.write_all(first_response.as_bytes()).unwrap();
            }
        }
        requests
    });
    (AuditClient::new(address, "audit-token"), server)
}

#[test]
fn managed_fts_streams_do_not_require_current_directory_resynchronization() {
    let stream = usize::MAX as *mut libc::c_void;
    assert!(super::fts_stream_may_change_current_directory(stream));

    super::fts_streams().lock().unwrap().insert(
        stream as usize,
        super::FtsStreamState {
            compare: None,
            mappings: Vec::new(),
            presented: Vec::new(),
            traversal_paths: Vec::new(),
            anchors: Vec::new(),
        },
    );
    assert!(!super::fts_stream_may_change_current_directory(stream));
    super::fts_streams()
        .lock()
        .unwrap()
        .remove(&(stream as usize));
}

#[test]
fn read_and_write_mapping_use_the_encrypted_overlay() {
    let fixture = Fixture::new();
    let lower = fixture.lower.join("file");
    std::fs::write(&lower, b"host").unwrap();
    let path = Fixture::c_path(&lower);

    let read = fixture.runtime.map(path.as_ptr(), libc::AT_FDCWD).unwrap();
    assert_eq!(
        std::fs::read(Path::new(read.to_str().unwrap())).unwrap(),
        b"host"
    );

    let write = fixture
        .runtime
        .filesystem
        .prepare_write(&lower, false)
        .unwrap();
    std::fs::write(write, b"sandbox").unwrap();
    assert_eq!(std::fs::read(&lower).unwrap(), b"host");
    assert_eq!(
        fixture.runtime.filesystem.state_for_test(&lower).unwrap(),
        Some(EntryState::Cow)
    );
}

#[test]
fn readlink_uses_the_logical_overlay_view() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("source-link");
    let destination = fixture.lower.join("destination-link");
    symlink("target", &source).unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        let source_path = Fixture::c_path(&source);
        let destination_path = Fixture::c_path(&destination);
        assert_eq!(
            sandbox_rename(source_path.as_ptr(), destination_path.as_ptr()),
            0
        );

        let mut target = [0_u8; 64];
        assert_eq!(
            sandbox_readlink(
                source_path.as_ptr(),
                target.as_mut_ptr().cast(),
                target.len(),
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOENT);

        let length = sandbox_readlink(
            destination_path.as_ptr(),
            target.as_mut_ptr().cast(),
            target.len(),
        );
        assert_eq!(length, 6);
        assert_eq!(&target[..length as usize], b"target");

        let directory_path = Fixture::c_path(&fixture.lower);
        let directory = sandbox_open_with_mode(
            directory_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        );
        assert!(directory >= 0);
        let length = sandbox_readlinkat(
            directory,
            c"destination-link".as_ptr(),
            target.as_mut_ptr().cast(),
            target.len(),
        );
        assert_eq!(length, 6);
        assert_eq!(&target[..length as usize], b"target");
        assert_eq!(sandbox_close(directory), 0);
    });

    assert!(source.is_symlink());
    assert!(!destination.exists());
}

#[test]
fn symlink_targets_inside_the_backing_store_are_rewritten_to_logical_paths() {
    let fixture = Fixture::new();
    let target = fixture.lower.join("target");
    let link = fixture.lower.join("link");
    std::fs::write(&target, b"target").unwrap();
    let internal = fixture
        .runtime
        .filesystem
        .prepare_write(&target, false)
        .unwrap();
    assert!(fixture.runtime.filesystem.is_internal(&internal));

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(
            sandbox_symlink(
                Fixture::c_path(&internal).as_ptr(),
                Fixture::c_path(&link).as_ptr(),
            ),
            0
        );
    });

    let mapped = fixture.runtime.filesystem.prepare_read(&link).unwrap();
    assert_eq!(std::fs::read_link(mapped).unwrap(), target);
    assert!(link.symlink_metadata().is_err());
}

#[test]
fn symlink_targets_normalize_backing_components_before_rewriting() {
    let fixture = Fixture::new();
    let target = fixture.lower.join("directory/target");
    let link = fixture.lower.join("link");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, b"target").unwrap();
    let internal = fixture
        .runtime
        .filesystem
        .prepare_write(&target, false)
        .unwrap();
    let internal = internal.parent().unwrap().join("sibling/../target");

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(
            sandbox_symlink(
                Fixture::c_path(&internal).as_ptr(),
                Fixture::c_path(&link).as_ptr(),
            ),
            0
        );
    });

    let mapped = fixture.runtime.filesystem.prepare_read(&link).unwrap();
    assert_eq!(std::fs::read_link(mapped).unwrap(), target);
}

#[test]
fn faccessat_honors_overlay_whiteouts() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("whiteout");
    std::fs::write(&file, b"host").unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        let path = Fixture::c_path(&file);
        assert_eq!(sandbox_unlink(path.as_ptr()), 0);
        assert_eq!(
            sandbox_faccessat(libc::AT_FDCWD, path.as_ptr(), libc::F_OK, 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOENT);
    });

    assert_eq!(std::fs::read(file).unwrap(), b"host");
}

#[test]
fn zero_flag_extended_rename_uses_the_overlay() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("renamex-source");
    let destination = fixture.lower.join("renamex-destination");
    let source_at = fixture.lower.join("renameatx-source");
    let destination_at = fixture.lower.join("renameatx-destination");
    std::fs::write(&source, b"source").unwrap();
    std::fs::write(&source_at, b"source-at").unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        let source_path = Fixture::c_path(&source);
        let destination_path = Fixture::c_path(&destination);
        assert_eq!(
            sandbox_renamex_np(source_path.as_ptr(), destination_path.as_ptr(), 0),
            0
        );
        assert_eq!(sandbox_access(source_path.as_ptr(), libc::F_OK), -1);
        assert_eq!(sandbox_access(destination_path.as_ptr(), libc::F_OK), 0);

        let directory_path = Fixture::c_path(&fixture.lower);
        let directory = sandbox_open_with_mode(
            directory_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        );
        assert!(directory >= 0);
        assert_eq!(
            sandbox_renameatx_np(
                directory,
                c"renameatx-source".as_ptr(),
                directory,
                c"renameatx-destination".as_ptr(),
                0,
            ),
            0
        );
        assert_eq!(sandbox_close(directory), 0);
    });

    assert_eq!(std::fs::read(source).unwrap(), b"source");
    assert!(!destination.exists());
    assert_eq!(std::fs::read(source_at).unwrap(), b"source-at");
    assert!(!destination_at.exists());
}

#[test]
fn extended_rename_flags_fail_closed() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("source");
    let destination = fixture.lower.join("destination");
    std::fs::write(&source, b"source").unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(
            sandbox_renamex_np(
                Fixture::c_path(&source).as_ptr(),
                Fixture::c_path(&destination).as_ptr(),
                libc::RENAME_EXCL,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            sandbox_renameatx_np(
                libc::AT_FDCWD,
                Fixture::c_path(&source).as_ptr(),
                libc::AT_FDCWD,
                Fixture::c_path(&destination).as_ptr(),
                libc::RENAME_EXCL,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
    });

    assert_eq!(std::fs::read(source).unwrap(), b"source");
    assert!(!destination.exists());
}

#[test]
fn timestamp_mutations_fail_closed_without_touching_lower() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("timestamps");
    std::fs::write(&file, b"host").unwrap();
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&file)
        .unwrap();
    let path = Fixture::c_path(&file);
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();
    let timevals = [
        libc::timeval {
            tv_sec: 1,
            tv_usec: 0,
        },
        libc::timeval {
            tv_sec: 1,
            tv_usec: 0,
        },
    ];
    let timespecs = [
        libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
    ];

    with_test_runtime(&fixture.runtime, || unsafe {
        for result in [
            sandbox_utimes(path.as_ptr(), timevals.as_ptr()),
            sandbox_lutimes(path.as_ptr(), timevals.as_ptr()),
            sandbox_futimes(opened.as_raw_fd(), timevals.as_ptr()),
            sandbox_futimens(opened.as_raw_fd(), timespecs.as_ptr()),
            sandbox_utimensat(libc::AT_FDCWD, path.as_ptr(), timespecs.as_ptr(), 0),
        ] {
            assert_eq!(result, -1);
            assert_eq!(*libc::__error(), libc::ENOTSUP);
        }
    });

    assert_eq!(std::fs::metadata(file).unwrap().modified().unwrap(), before);
}

#[test]
fn file_flag_mutations_fail_closed() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("flags");
    std::fs::write(&file, b"host").unwrap();
    let opened = std::fs::OpenOptions::new().read(true).open(&file).unwrap();
    let path = Fixture::c_path(&file);

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(sandbox_chflags(path.as_ptr(), libc::UF_HIDDEN), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(sandbox_fchflags(opened.as_raw_fd(), libc::UF_HIDDEN), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
    });

    let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
    assert_eq!(unsafe { libc::stat(path.as_ptr(), &mut status) }, 0);
    assert_eq!(status.st_flags & libc::UF_HIDDEN, 0);
}

#[test]
fn extended_attribute_mutations_fail_closed() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("xattr");
    std::fs::write(&file, b"host").unwrap();
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&file)
        .unwrap();
    let path = Fixture::c_path(&file);
    let name = c"com.agora.test";
    let value = b"value";

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(
            sandbox_setxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            sandbox_fsetxattr(
                opened.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(sandbox_removexattr(path.as_ptr(), name.as_ptr(), 0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            sandbox_fremovexattr(opened.as_raw_fd(), name.as_ptr(), 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
    });

    assert_eq!(
        unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0,) },
        -1
    );
}

#[test]
fn lower_read_paths_remain_directly_addressable_by_the_sandbox() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("file");
    std::fs::write(&logical, b"host").unwrap();
    let lower = Fixture::c_path(&logical);
    let mapped = fixture.runtime.map(lower.as_ptr(), libc::AT_FDCWD).unwrap();

    let resolved = unsafe {
        fixture
            .runtime
            .logical_path(mapped.as_ptr(), libc::AT_FDCWD)
    }
    .unwrap();
    assert_eq!(resolved, logical);
}

#[test]
fn direct_filesystem_backing_paths_reenter_the_logical_view() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("Relocated.app/Contents/Info.plist");
    let backing = fixture
        .runtime
        .filesystem
        .root()
        .join(logical.strip_prefix(Path::new("/")).unwrap());
    let backing = Fixture::c_path(&backing);

    let resolved = unsafe {
        fixture
            .runtime
            .logical_path(backing.as_ptr(), libc::AT_FDCWD)
    }
    .unwrap();

    assert_eq!(resolved, logical);
}

#[test]
fn backing_paths_are_normalized_before_reentering_the_logical_view() {
    let fixture = Fixture::new();
    let backing = fixture
        .runtime
        .filesystem
        .root()
        .join("Applications/Fixture.app/Contents/Resources/../bin/options");
    let backing = Fixture::c_path(&backing);

    let resolved = unsafe {
        fixture
            .runtime
            .logical_path(backing.as_ptr(), libc::AT_FDCWD)
    }
    .unwrap();

    assert_eq!(
        resolved,
        Path::new("/Applications/Fixture.app/Contents/bin/options")
    );
}

#[test]
fn backing_paths_cannot_escape_above_the_filesystem_root() {
    let fixture = Fixture::new();
    let backing = fixture.runtime.filesystem.root().join("../../lower/target");

    let error = fixture.runtime.logical_or_host(&backing).unwrap_err();

    assert_eq!(error_errno(&error), libc::EACCES);
}

#[test]
fn host_paths_preserve_parent_components_for_native_symlink_resolution() {
    let fixture = Fixture::new();
    let requested = fixture.lower.join("linked/../target");

    let resolved = fixture.runtime.logical_or_host(&requested).unwrap();

    assert_eq!(resolved, requested);
}

#[test]
fn direct_filesystem_root_represents_the_logical_root() {
    let fixture = Fixture::new();
    let backing = Fixture::c_path(fixture.runtime.filesystem.root());

    let resolved = unsafe {
        fixture
            .runtime
            .logical_path(backing.as_ptr(), libc::AT_FDCWD)
    }
    .unwrap();

    assert_eq!(resolved, Path::new("/"));
}

#[test]
fn canonical_filesystem_backing_paths_reenter_the_logical_view() {
    let directory = PathBuf::from(format!(
        "/tmp/agora-filesystem-hook-canonical-{}",
        uuid::Uuid::new_v4()
    ));
    let root = directory.join("workdir/fs");
    let runtime = FilesystemHookRuntime::new(&root).unwrap();
    let canonical_root = root.canonicalize().unwrap();
    let logical = Path::new("/Applications/Relocated.app/Contents/Info.plist");
    let backing = canonical_root.join(logical.strip_prefix(Path::new("/")).unwrap());
    let backing = Fixture::c_path(&backing);

    let resolved = unsafe { runtime.logical_path(backing.as_ptr(), libc::AT_FDCWD) }.unwrap();

    assert_eq!(resolved, logical);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn decoded_backing_aliases_cannot_target_private_workspace_paths() {
    let fixture = Fixture::new();
    let private = fixture.directory.join("workdir/runtime/control");
    let backing = fixture
        .runtime
        .filesystem
        .root()
        .join(private.strip_prefix(Path::new("/")).unwrap());
    let backing = Fixture::c_path(&backing);

    let error = unsafe {
        fixture
            .runtime
            .logical_path(backing.as_ptr(), libc::AT_FDCWD)
    }
    .unwrap_err();

    assert_eq!(error_errno(&error), libc::EACCES);
}

#[test]
fn raw_backing_controls_resolve_only_as_logical_business_names() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join(".metadata");
    let logical_backing = fixture
        .runtime
        .filesystem
        .prepare_write(&logical, true)
        .unwrap();
    std::fs::write(&logical_backing, b"business metadata").unwrap();
    let raw_control = logical_backing.parent().unwrap().join(".metadata");
    let raw_control = Fixture::c_path(&raw_control);

    let mapped = fixture
        .runtime
        .map(raw_control.as_ptr(), libc::AT_FDCWD)
        .unwrap();

    assert_eq!(Path::new(mapped.to_str().unwrap()), logical_backing);
    assert_ne!(
        Path::new(mapped.to_str().unwrap()),
        Path::new(raw_control.to_str().unwrap())
    );
}

#[test]
fn loader_backing_aliases_do_not_fall_through_a_remote_route() {
    let mut fixture = Fixture::new();
    let server = fixture.attach_nfs();
    let logical = server.logical_root.join("libfixture.dylib");
    std::fs::create_dir_all(logical.parent().unwrap()).unwrap();
    std::fs::write(&logical, b"local lower").unwrap();
    let backing = fixture
        .runtime
        .filesystem
        .root()
        .join(logical.strip_prefix(Path::new("/")).unwrap());

    let error = fixture.runtime.prepare_loader_path(&backing).unwrap_err();

    assert_eq!(error_errno(&error), libc::ENOTSUP);
}

#[test]
fn external_symlink_aliases_cannot_address_private_workspace_paths() {
    let fixture = Fixture::new();
    let private = fixture.directory.join("workdir/private");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::write(private.join("secret"), b"secret").unwrap();
    let alias = fixture.directory.join("workspace-alias");
    symlink(fixture.directory.join("workdir"), &alias).unwrap();
    let alias = Fixture::c_path(&alias.join("private/secret"));

    let error =
        unsafe { fixture.runtime.logical_path(alias.as_ptr(), libc::AT_FDCWD) }.unwrap_err();
    assert_eq!(error_errno(&error), libc::EACCES);
}

#[test]
fn relative_paths_cannot_escape_into_the_private_workspace() {
    let fixture = Fixture::new();
    let private = fixture.directory.join("workdir/private/secret");
    std::fs::create_dir_all(private.parent().unwrap()).unwrap();
    std::fs::write(&private, b"secret").unwrap();
    fixture.runtime.set_current_directory(fixture.lower.clone());

    let relative = CString::new("../workdir/private/secret").unwrap();
    let error = unsafe {
        fixture
            .runtime
            .logical_path(relative.as_ptr(), libc::AT_FDCWD)
    }
    .unwrap_err();
    assert_eq!(error_errno(&error), libc::EACCES);
}

#[test]
fn open_flags_select_read_create_and_write_intents() {
    let fixture = Fixture::new();
    let lower = fixture.lower.join("file");
    std::fs::write(&lower, b"host").unwrap();
    let path = Fixture::c_path(&lower);

    let read = fixture
        .runtime
        .prepare_open(path.as_ptr(), libc::AT_FDCWD, libc::O_RDONLY, 0)
        .unwrap();
    assert_eq!(read.file.path, lower.to_string_lossy());
    assert_eq!(read.file.mode.access, crate::callback::FileAccessMode::Read);
    assert!(!read.file.mode.create);
    assert!(matches!(
        read.prepared.target(),
        crate::filesystem::OpenTarget::Path(mapped) if mapped == &lower
    ));
    let _read = read.into_prepared();
    assert_eq!(
        fixture.runtime.filesystem.state_for_test(&lower).unwrap(),
        None
    );

    let write = fixture
        .runtime
        .prepare_open(path.as_ptr(), libc::AT_FDCWD, libc::O_WRONLY, 0)
        .unwrap();
    assert_eq!(
        write.file.mode.access,
        crate::callback::FileAccessMode::Write
    );
    assert!(matches!(
        write.prepared.target(),
        crate::filesystem::OpenTarget::Path(mapped) if mapped != &lower
    ));
    let mut write = write.into_prepared();
    assert!(matches!(
        fixture.runtime.filesystem.state_for_test(&lower).unwrap(),
        Some(EntryState::Cached { .. })
    ));
    fixture.runtime.commit_open(&mut write).unwrap();
    assert_eq!(
        fixture.runtime.filesystem.state_for_test(&lower).unwrap(),
        Some(EntryState::Cow)
    );

    let created = fixture.lower.join("created");
    let prepared = fixture
        .runtime
        .prepare_open(
            Fixture::c_path(&created).as_ptr(),
            libc::AT_FDCWD,
            libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC | libc::O_APPEND | libc::O_EXCL,
            0o640,
        )
        .unwrap();
    assert_eq!(
        prepared.file.mode.access,
        crate::callback::FileAccessMode::ReadWrite
    );
    assert!(prepared.file.mode.create);
    assert!(prepared.file.mode.truncate);
    assert!(prepared.file.mode.append);
    assert!(prepared.file.mode.exclusive);
    let mut prepared = prepared.into_prepared();
    assert_eq!(
        fixture.runtime.filesystem.state_for_test(&created).unwrap(),
        None
    );
    fixture.runtime.commit_open(&mut prepared).unwrap();
    assert_eq!(
        fixture.runtime.filesystem.state_for_test(&created).unwrap(),
        Some(EntryState::Cow)
    );
}

#[test]
fn open_intent_is_shared_by_open_and_fopen_modes() {
    for (mode, flags) in [
        (b"r".as_slice(), libc::O_RDONLY),
        (b"r+".as_slice(), libc::O_RDWR),
        (
            b"w".as_slice(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
        ),
        (
            b"a+".as_slice(),
            libc::O_RDWR | libc::O_CREAT | libc::O_APPEND,
        ),
        (
            b"wx".as_slice(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_EXCL,
        ),
    ] {
        assert_eq!(
            intent_from_fopen_mode(mode).unwrap(),
            crate::filesystem::OpenIntent::new(flags, 0o666).unwrap(),
        );
    }
}

#[test]
fn encrypted_descriptors_keep_backing_ciphertext_and_write_back_on_last_close() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("encrypted-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("secret.txt");
    let path = Fixture::c_path(&logical);
    let marker = b"runtime plaintext marker";

    with_test_runtime(&runtime, || unsafe {
        let descriptor = super::agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(
            libc::write(descriptor, marker.as_ptr().cast(), marker.len()),
            marker.len() as isize
        );
        let duplicate = sandbox_dup(descriptor);
        assert!(duplicate >= 0);
        assert_eq!(sandbox_fsync(duplicate), 0);
        assert_eq!(sandbox_close(descriptor), 0);
        assert_eq!(sandbox_close(duplicate), 0);
    });

    assert!(!logical.exists());
    assert!(!directory_contains_for_test(
        runtime.filesystem.root(),
        marker
    ));
    with_test_runtime(&runtime, || unsafe {
        let descriptor = super::agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        assert_eq!(sandbox_pwrite(descriptor, b"x".as_ptr().cast(), 1, 0), -1);
        assert_eq!(*libc::__error(), libc::EBADF);
        let mut restored = vec![0_u8; marker.len()];
        assert_eq!(
            libc::read(descriptor, restored.as_mut_ptr().cast(), restored.len()),
            restored.len() as isize
        );
        assert_eq!(restored, marker);
        assert_eq!(sandbox_close(descriptor), 0);
    });
}

#[test]
fn guarded_file_operations_preserve_encrypted_descriptor_tracking() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("guarded-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("guarded.txt");
    let path = Fixture::c_path(&logical);
    let guard = 0xa60a_5a7d_b001_u64;
    let first = b"guarded";
    let second = b" write";
    let vectors = [libc::iovec {
        iov_base: second.as_ptr().cast_mut().cast(),
        iov_len: second.len(),
    }];

    with_test_runtime(&runtime, || unsafe {
        let descriptor = sandbox_guarded_open_with_mode(
            path.as_ptr(),
            &guard,
            (1_u32 << 0) | (1_u32 << 1),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(
            sandbox_guarded_pwrite(descriptor, &guard, first.as_ptr().cast(), first.len(), 0,),
            first.len() as libc::ssize_t
        );
        assert_eq!(
            sandbox_guarded_writev(
                descriptor,
                &guard,
                std::ptr::without_provenance::<libc::iovec>(1),
                1,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::EFAULT);
        assert_eq!(
            libc::lseek(descriptor, first.len() as libc::off_t, libc::SEEK_SET),
            7
        );
        assert_eq!(
            sandbox_guarded_writev(descriptor, &guard, vectors.as_ptr(), 1),
            second.len() as libc::ssize_t
        );
        assert_eq!(sandbox_guarded_close(descriptor, &guard), 0);
    });

    with_test_runtime(&runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        let mut contents = vec![0_u8; first.len() + second.len()];
        assert_eq!(
            libc::read(descriptor, contents.as_mut_ptr().cast(), contents.len()),
            contents.len() as libc::ssize_t
        );
        assert_eq!(contents, b"guarded write");
        assert_eq!(sandbox_close(descriptor), 0);
    });
}

#[test]
fn registry_aliases_not_temporary_arc_clones_determine_the_last_close() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("temporary-arc-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("temporary-arc.txt");
    let path = Fixture::c_path(&logical);

    with_test_runtime(&runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_ne!(libc::fcntl(descriptor, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
        assert_eq!(
            super::agora_sandbox_fcntl_shim(descriptor, libc::F_SETFD, 0),
            0
        );
        assert_ne!(libc::fcntl(descriptor, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
        assert_eq!(libc::write(descriptor, b"persisted".as_ptr().cast(), 9), 9);
        let temporary = runtime.tracked_open(descriptor).unwrap();
        assert_eq!(sandbox_close(descriptor), 0);
        drop(temporary);

        let reopened = sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(reopened >= 0);
        let mut contents = [0_u8; 9];
        assert_eq!(libc::read(reopened, contents.as_mut_ptr().cast(), 9), 9);
        assert_eq!(&contents, b"persisted");
        assert_eq!(sandbox_close(reopened), 0);
    });
}

#[test]
fn full_sync_discards_stale_descriptor_aliases() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("stale-alias-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("stale-alias.txt");
    let path = Fixture::c_path(&logical);

    with_test_runtime(&runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(libc::write(descriptor, b"persisted".as_ptr().cast(), 9), 9);
        let duplicate = sandbox_dup(descriptor);
        assert!(duplicate >= 0);
        let stale = libc::c_int::MAX;
        let stale_open = runtime.tracked_open(descriptor).unwrap();
        runtime.open_files.lock().unwrap().insert(stale, stale_open);
        assert_eq!(sandbox_close(descriptor), 0);
        assert!(runtime.tracked_open(stale).is_some());
        runtime.commit_all_open_files().unwrap();
        assert!(runtime.tracked_open(stale).is_none());
        assert!(runtime.tracked_open(duplicate).is_some());
        assert_eq!(sandbox_close(duplicate), 0);
    });

    with_test_runtime(&runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        let mut contents = [0_u8; 9];
        assert_eq!(libc::read(descriptor, contents.as_mut_ptr().cast(), 9), 9);
        assert_eq!(&contents, b"persisted");
        assert_eq!(sandbox_close(descriptor), 0);
    });
}

#[test]
fn unsupported_darwin_symlink_open_flags_fail_before_staging() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("darwin-open-flags");
    std::fs::write(&file, b"host").unwrap();
    let path = Fixture::c_path(&file);

    with_test_runtime(&fixture.runtime, || unsafe {
        for flag in [libc::O_NOFOLLOW_ANY, libc::O_SYMLINK] {
            assert_eq!(
                sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY | flag, 0),
                -1
            );
            assert_eq!(*libc::__error(), libc::ENOTSUP);
        }
    });

    assert_eq!(
        fixture.runtime.filesystem.state_for_test(&file).unwrap(),
        None
    );
}

#[test]
fn fclose_releases_tracking_even_when_flushing_the_stream_fails() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("fclose-failure-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("fclose-failure.txt");
    let path = Fixture::c_path(&logical);

    with_test_runtime(&runtime, || unsafe {
        let stream = sandbox_fopen(path.as_ptr(), c"w".as_ptr());
        assert!(!stream.is_null());
        let descriptor = libc::fileno(stream);
        assert!(descriptor >= 0);
        assert!(libc::fputs(c"buffered".as_ptr(), stream) >= 0);
        let replacement = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        assert!(replacement >= 0);
        assert_eq!(libc::dup2(replacement, descriptor), descriptor);
        assert_eq!(libc::close(replacement), 0);
        assert_eq!(sandbox_fclose(stream), -1);
        assert!(runtime.tracked(descriptor).is_none());
    });
}

#[test]
fn readdir_r_returns_logical_names_for_encrypted_file_backings() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("encrypted-readdir-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("visible-name.txt");

    with_test_runtime(&runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(
            Fixture::c_path(&logical).as_ptr(),
            libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(libc::write(descriptor, b"secret".as_ptr().cast(), 6), 6);
        assert_eq!(sandbox_close(descriptor), 0);

        let directory = sandbox_opendir(Fixture::c_path(&fixture.lower).as_ptr());
        assert!(!directory.is_null());
        let mut names = HashSet::new();
        loop {
            let mut entry = std::mem::zeroed::<libc::dirent>();
            let mut result = std::ptr::null_mut();
            assert_eq!(sandbox_readdir_r(directory, &mut entry, &mut result), 0);
            if result.is_null() {
                break;
            }
            names.insert(
                CStr::from_ptr(entry.d_name.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        assert_eq!(sandbox_closedir(directory), 0);
        assert!(
            names.contains("visible-name.txt"),
            "directory names: {names:?}"
        );
        assert!(
            !names.iter().any(|name| {
                name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        );
    });
}

#[test]
fn fdopendir_uses_the_merged_directory_view() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("encrypted-fdopendir-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("visible-name.txt");

    with_test_runtime(&runtime, || unsafe {
        let file = sandbox_open_with_mode(
            Fixture::c_path(&logical).as_ptr(),
            libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
            0o600,
        );
        assert!(file >= 0);
        assert_eq!(libc::write(file, b"secret".as_ptr().cast(), 6), 6);
        assert_eq!(sandbox_close(file), 0);

        let descriptor = sandbox_open_with_mode(
            Fixture::c_path(&fixture.lower).as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        );
        assert!(descriptor >= 0);
        let directory = sandbox_fdopendir(descriptor);
        assert!(!directory.is_null());
        let mut names = HashSet::new();
        loop {
            let mut entry = std::mem::zeroed::<libc::dirent>();
            let mut result = std::ptr::null_mut();
            assert_eq!(sandbox_readdir_r(directory, &mut entry, &mut result), 0);
            if result.is_null() {
                break;
            }
            names.insert(
                CStr::from_ptr(entry.d_name.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        assert_eq!(sandbox_closedir(directory), 0);
        assert!(
            names.contains("visible-name.txt"),
            "directory names: {names:?}"
        );
        assert!(
            !names.iter().any(|name| {
                name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        );
    });
}

#[test]
fn fdopendir_uses_the_descriptor_layer_when_the_upper_appears_later() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("encrypted-late-upper-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    std::fs::write(fixture.lower.join("lower.txt"), b"host").unwrap();

    with_test_runtime(&runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(
            Fixture::c_path(&fixture.lower).as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        );
        assert!(descriptor >= 0);

        let upper = fixture.lower.join("upper.txt");
        let file = sandbox_open_with_mode(
            Fixture::c_path(&upper).as_ptr(),
            libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
            0o600,
        );
        assert!(file >= 0);
        assert_eq!(sandbox_close(file), 0);

        let directory = sandbox_fdopendir(descriptor);
        assert!(!directory.is_null());
        let mut names = HashSet::new();
        loop {
            let entry = sandbox_readdir(directory);
            if entry.is_null() {
                break;
            }
            names.insert(
                CStr::from_ptr((*entry).d_name.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        assert_eq!(sandbox_closedir(directory), 0);
        assert!(names.contains("lower.txt"), "directory names: {names:?}");
        assert!(names.contains("upper.txt"), "directory names: {names:?}");
    });
}

#[test]
fn rewinddir_restarts_the_merged_directory_view() {
    let fixture = Fixture::new();
    std::fs::write(fixture.lower.join("lower.txt"), b"host").unwrap();
    let upper = fixture.lower.join("upper.txt");

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(
            Fixture::c_path(&upper).as_ptr(),
            libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(sandbox_close(descriptor), 0);

        let directory = sandbox_opendir(Fixture::c_path(&fixture.lower).as_ptr());
        assert!(!directory.is_null());
        let read_names = |directory: *mut libc::DIR| {
            let mut names = HashSet::new();
            loop {
                let entry = sandbox_readdir(directory);
                if entry.is_null() {
                    break;
                }
                names.insert(
                    CStr::from_ptr((*entry).d_name.as_ptr())
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            names
        };

        let first = read_names(directory);
        sandbox_rewinddir(directory);
        let second = read_names(directory);
        assert_eq!(second, first);
        assert!(second.contains("lower.txt"), "directory names: {second:?}");
        assert!(second.contains("upper.txt"), "directory names: {second:?}");
        assert_eq!(sandbox_closedir(directory), 0);
    });
}

#[test]
fn freopen_fails_closed_without_touching_the_host_path() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("source");
    let destination = fixture.lower.join("destination");
    std::fs::write(&source, b"source").unwrap();
    std::fs::write(&destination, b"host").unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        let stream = sandbox_fopen(Fixture::c_path(&source).as_ptr(), c"r".as_ptr());
        assert!(!stream.is_null());
        assert!(
            sandbox_freopen(
                Fixture::c_path(&destination).as_ptr(),
                c"w".as_ptr(),
                stream,
            )
            .is_null()
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(sandbox_fclose(stream), 0);
    });

    assert_eq!(std::fs::read(destination).unwrap(), b"host");
}

#[test]
fn fdopendir_failure_keeps_the_callers_descriptor_open() {
    struct RestorePermissions(PathBuf);

    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture
            .directory
            .join("encrypted-fdopendir-failure-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let upper = fixture.lower.join("upper.txt");

    with_test_runtime(&runtime, || unsafe {
        let file = sandbox_open_with_mode(
            Fixture::c_path(&upper).as_ptr(),
            libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
            0o600,
        );
        assert!(file >= 0);
        assert_eq!(sandbox_close(file), 0);

        let descriptor = sandbox_open_with_mode(
            Fixture::c_path(&fixture.lower).as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        );
        assert!(descriptor >= 0);
        let restore_permissions = RestorePermissions(fixture.lower.clone());
        std::fs::set_permissions(&fixture.lower, std::fs::Permissions::from_mode(0o000)).unwrap();

        let directory = sandbox_fdopendir(descriptor);
        assert!(directory.is_null());
        assert!(libc::fcntl(descriptor, libc::F_GETFD) >= 0);

        drop(restore_permissions);
        assert_eq!(sandbox_close(descriptor), 0);
    });
}

#[test]
fn full_filesystem_sync_writes_encrypted_contents_before_close() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("full-sync-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("full-sync.txt");
    let path = Fixture::c_path(&logical);

    with_test_runtime(&runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(libc::write(descriptor, b"synced".as_ptr().cast(), 6), 6);
        assert_eq!(
            super::agora_sandbox_fcntl_shim(descriptor, libc::F_FULLFSYNC),
            0
        );

        let reader = sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(reader >= 0);
        let mut restored = [0_u8; 6];
        assert_eq!(
            libc::read(reader, restored.as_mut_ptr().cast(), restored.len()),
            restored.len() as isize
        );
        assert_eq!(&restored, b"synced");
        assert_eq!(sandbox_close(reader), 0);
        assert_eq!(sandbox_close(descriptor), 0);
    });
}

#[test]
fn fcntl_descriptor_duplicates_share_encrypted_writeback_state() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("fcntl-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("fcntl-secret.txt");
    let path = Fixture::c_path(&logical);

    with_test_runtime(&runtime, || unsafe {
        let descriptor = super::agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(libc::write(descriptor, b"first".as_ptr().cast(), 5), 5);
        let duplicate = super::agora_sandbox_fcntl_shim(descriptor, libc::F_DUPFD_CLOEXEC, 0);
        assert!(duplicate >= 0);
        assert_eq!(sandbox_close(descriptor), 0);
        assert_eq!(libc::write(duplicate, b" second".as_ptr().cast(), 7), 7);
        assert_eq!(sandbox_close(duplicate), 0);

        let reopened = super::agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(reopened >= 0);
        let mut contents = [0_u8; 12];
        assert_eq!(libc::read(reopened, contents.as_mut_ptr().cast(), 12), 12);
        assert_eq!(&contents, b"first second");
        assert_eq!(sandbox_close(reopened), 0);
    });
}

#[test]
fn content_mutating_fcntl_is_rejected_for_managed_descriptors() {
    const F_SETSIZE: libc::c_int = 43;
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("fcntl-mutation-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("fcntl-mutation.txt");
    let path = Fixture::c_path(&logical);

    with_test_runtime(&runtime, || unsafe {
        let descriptor = super::agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(libc::write(descriptor, b"contents".as_ptr().cast(), 8), 8);
        let original_content = {
            let mut files = super::lock(&runtime.open_files);
            let open = Arc::get_mut(files.get_mut(&descriptor).unwrap()).unwrap();
            std::mem::replace(
                &mut open.content,
                super::ManagedContent::encrypted(
                    super::EncryptedContent {
                        handle: "local-handle".to_string(),
                        lazy: false,
                        state: super::LocalOpenState::create(libc::O_RDWR).unwrap(),
                        lock: tempfile::tempfile().unwrap(),
                        identity: super::LocalFileIdentity {
                            device: 1,
                            inode: 2,
                            links: 1,
                        },
                    },
                    true,
                ),
            )
        };
        assert_eq!(
            super::agora_sandbox_fcntl_shim(descriptor, F_SETSIZE, 2_i64),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        Arc::get_mut(
            super::lock(&runtime.open_files)
                .get_mut(&descriptor)
                .unwrap(),
        )
        .unwrap()
        .content = original_content;
        assert_eq!(sandbox_close(descriptor), 0);

        let reopened = super::agora_sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(reopened >= 0);
        let mut contents = [0_u8; 8];
        assert_eq!(libc::read(reopened, contents.as_mut_ptr().cast(), 8), 8);
        assert_eq!(&contents, b"contents");
        assert_eq!(sandbox_close(reopened), 0);
    });
}

#[test]
fn plain_regular_files_use_managed_content_without_changing_native_io() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("fcntl-plain.txt");
    let path = Fixture::c_path(&logical);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = super::agora_sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        let open = fixture.runtime.tracked_open(descriptor).unwrap();
        let content = open.managed();
        assert!(!content.is_broker_managed());
        assert!(!content.publishes_writes());
        assert!(content.accepts_opaque_copy());

        assert_eq!(sandbox_write(descriptor, b"plain".as_ptr().cast(), 5), 5);
        assert_eq!(sandbox_lseek(descriptor, 0, libc::SEEK_SET), 0);
        let mut contents = [0_u8; 5];
        assert_eq!(sandbox_read(descriptor, contents.as_mut_ptr().cast(), 5), 5);
        assert_eq!(&contents, b"plain");
        assert_eq!(super::agora_sandbox_validate_content_fcntl(descriptor), 0);
        assert_eq!(sandbox_close(descriptor), 0);
    });
}

#[test]
fn directory_descriptor_duplicates_keep_logical_paths_and_close_clears_tracking() {
    let fixture = Fixture::new();
    let logical = PathBuf::from("/logical-directory");
    let physical = Fixture::c_path(&fixture.lower);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = libc::open(physical.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
        assert!(descriptor >= 0);
        fixture
            .runtime
            .register_directory(descriptor, logical.clone(), false, None);

        let duplicate = sandbox_dup(descriptor);
        assert!(duplicate >= 0);
        assert_eq!(
            fixture.runtime.descriptor_logical_path(duplicate),
            Some(logical.clone())
        );

        assert_eq!(sandbox_close(duplicate), 0);
        assert!(
            !fixture
                .runtime
                .directory_descriptors
                .lock()
                .unwrap()
                .contains_key(&duplicate)
        );
        assert_eq!(sandbox_close(descriptor), 0);
        assert!(
            !fixture
                .runtime
                .directory_descriptors
                .lock()
                .unwrap()
                .contains_key(&descriptor)
        );
    });
}

#[test]
fn write_spawn_actions_are_rejected_without_staging() {
    let fixture = Fixture::new();
    let deferred = fixture.lower.join("deferred");

    with_test_runtime(&fixture.runtime, || unsafe {
        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        assert_eq!(libc::posix_spawn_file_actions_init(&mut actions), 0);
        assert_eq!(
            sandbox_spawn_addopen(
                &mut actions,
                9,
                Fixture::c_path(&deferred).as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o600,
            ),
            libc::ENOTSUP
        );
        assert_eq!(
            fixture
                .runtime
                .filesystem
                .state_for_test(&deferred)
                .unwrap(),
            None
        );
        assert_eq!(libc::posix_spawn_file_actions_destroy(&mut actions), 0);
    });
}

#[test]
fn encrypted_stat_reports_plaintext_file_size() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("encrypted-stat-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("secret.txt");
    let path = Fixture::c_path(&logical);
    let marker = b"plaintext-size";

    with_test_runtime(&runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(
            libc::write(descriptor, marker.as_ptr().cast(), marker.len()),
            marker.len() as isize
        );
        assert_eq!(sandbox_fchmod(descriptor, 0o640), 0);
        let mut descriptor_status = std::mem::zeroed::<libc::stat>();
        assert_eq!(sandbox_fstat(descriptor, &mut descriptor_status), 0);
        assert_eq!(u32::from(descriptor_status.st_mode) & 0o777, 0o640);
        let mut path_identity = std::mem::zeroed::<libc::stat>();
        assert_eq!(sandbox_stat(path.as_ptr(), &mut path_identity), 0);
        assert_eq!(descriptor_status.st_dev, path_identity.st_dev);
        assert_eq!(descriptor_status.st_ino, path_identity.st_ino);
        assert_eq!(sandbox_close(descriptor), 0);

        let mut status = std::mem::zeroed::<libc::stat>();
        assert_eq!(sandbox_stat(path.as_ptr(), &mut status), 0);
        assert_eq!(status.st_size, marker.len() as libc::off_t);
        assert_eq!(u32::from(status.st_mode) & 0o777, 0o640);
        assert_eq!(sandbox_access(path.as_ptr(), libc::R_OK), 0);
        assert_eq!(sandbox_access(path.as_ptr(), libc::X_OK), -1);
        assert_eq!(*libc::__error(), libc::EACCES);
        assert_eq!(sandbox_lstat(path.as_ptr(), &mut status), 0);
        assert_eq!(status.st_size, marker.len() as libc::off_t);
        assert_eq!(
            sandbox_fstatat(libc::AT_FDCWD, path.as_ptr(), &mut status, 0),
            0
        );
        assert_eq!(status.st_size, marker.len() as libc::off_t);
    });
}

#[test]
fn successful_stat_calls_preserve_errno() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("encrypted-stat-errno-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("stat-errno.txt");
    std::fs::write(&logical, b"contents").unwrap();
    let path = Fixture::c_path(&logical);

    with_test_runtime(&runtime, || unsafe {
        let mut status = std::mem::zeroed::<libc::stat>();
        *libc::__error() = libc::ERANGE;
        assert_eq!(sandbox_stat(path.as_ptr(), &mut status), 0);
        assert_eq!(*libc::__error(), libc::ERANGE);

        *libc::__error() = libc::ERANGE;
        assert_eq!(sandbox_lstat(path.as_ptr(), &mut status), 0);
        assert_eq!(*libc::__error(), libc::ERANGE);

        *libc::__error() = libc::ERANGE;
        assert_eq!(
            sandbox_fstatat(libc::AT_FDCWD, path.as_ptr(), &mut status, 0),
            0
        );
        assert_eq!(*libc::__error(), libc::ERANGE);

        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        *libc::__error() = libc::ERANGE;
        assert_eq!(sandbox_fstat(descriptor, &mut status), 0);
        assert_eq!(*libc::__error(), libc::ERANGE);
        assert_eq!(sandbox_close(descriptor), 0);
    });
}

#[test]
fn fstatat_reuses_an_unchanged_passthrough_directory_snapshot() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("entry");
    std::fs::write(&logical, b"lower").unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        let directory = sandbox_opendir(Fixture::c_path(&fixture.lower).as_ptr());
        assert!(!directory.is_null());
        let descriptor = libc::dirfd(directory);
        let mut status = std::mem::zeroed::<libc::stat>();
        let before = fixture.runtime.filesystem.transaction_count_for_test();

        assert_eq!(
            sandbox_fstatat(descriptor, c"entry".as_ptr(), &mut status, 0),
            0
        );
        assert_eq!(status.st_size, 5);
        assert_eq!(
            fixture.runtime.filesystem.transaction_count_for_test(),
            before,
            "an unchanged passthrough directory should not re-enter the overlay transaction"
        );

        let writable = sandbox_open_with_mode(
            Fixture::c_path(&logical).as_ptr(),
            libc::O_WRONLY | libc::O_TRUNC,
            0,
        );
        assert!(writable >= 0);
        assert_eq!(libc::write(writable, b"sandbox".as_ptr().cast(), 7), 7);
        assert_eq!(sandbox_close(writable), 0);
        let before = fixture.runtime.filesystem.transaction_count_for_test();

        assert_eq!(
            sandbox_fstatat(descriptor, c"entry".as_ptr(), &mut status, 0),
            0
        );
        assert_eq!(status.st_size, 7);
        assert!(fixture.runtime.filesystem.transaction_count_for_test() > before);
        assert_eq!(sandbox_closedir(directory), 0);
    });
}

#[test]
fn lower_read_descriptors_keep_metadata_mutations_inside_the_sandbox() {
    let fixture = Fixture::new();
    let lower = fixture.lower.join("lower-read-only");
    std::fs::write(&lower, b"host").unwrap();
    std::fs::set_permissions(&lower, std::fs::Permissions::from_mode(0o644)).unwrap();
    let path = Fixture::c_path(&lower);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        assert_eq!(sandbox_fchmod(descriptor, 0o600), 0);
        let mut status = std::mem::zeroed::<libc::stat>();
        assert_eq!(sandbox_fstat(descriptor, &mut status), 0);
        assert_eq!(u32::from(status.st_mode) & 0o777, 0o600);
        assert_eq!(sandbox_fchown(descriptor, !0, !0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(sandbox_ftruncate(descriptor, 0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(sandbox_close(descriptor), 0);
    });

    assert_eq!(std::fs::read(&lower).unwrap(), b"host");
    assert_eq!(
        lower.metadata().unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn stat_follows_overlay_symlinks_while_lstat_reports_the_link() {
    let fixture = Fixture::new();
    let target = fixture.lower.join("target");
    let link = fixture.lower.join("link");
    std::fs::write(&target, b"host").unwrap();
    symlink(&target, &link).unwrap();
    let target_path = Fixture::c_path(&target);
    let link_path = Fixture::c_path(&link);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor =
            sandbox_open_with_mode(target_path.as_ptr(), libc::O_WRONLY | libc::O_TRUNC, 0);
        assert!(descriptor >= 0);
        assert_eq!(libc::write(descriptor, b"sandbox".as_ptr().cast(), 7), 7);
        assert_eq!(sandbox_close(descriptor), 0);

        let descriptor = sandbox_open_with_mode(link_path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        let mut contents = [0_u8; 7];
        assert_eq!(
            libc::read(descriptor, contents.as_mut_ptr().cast(), contents.len()),
            7
        );
        assert_eq!(contents, *b"sandbox");
        assert_eq!(sandbox_close(descriptor), 0);

        let mut status = std::mem::zeroed::<libc::stat>();
        assert_eq!(sandbox_stat(link_path.as_ptr(), &mut status), 0);
        assert_eq!(status.st_size, 7);
        assert_eq!(status.st_mode & libc::S_IFMT, libc::S_IFREG);
        assert_eq!(sandbox_lstat(link_path.as_ptr(), &mut status), 0);
        assert_eq!(status.st_mode & libc::S_IFMT, libc::S_IFLNK);
    });
}

#[test]
fn encrypted_control_paths_resolve_as_isolated_logical_business_names() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("encrypted-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let physical = Fixture::c_path(&runtime.filesystem.root().join(".metadata"));
    let logical = fixture.lower.join(".metadata");
    let logical = Fixture::c_path(&logical);

    with_test_runtime(&runtime, || unsafe {
        assert_eq!(
            super::agora_sandbox_open_with_mode(physical.as_ptr(), libc::O_RDONLY, 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOENT);

        let descriptor = super::agora_sandbox_open_with_mode(
            logical.as_ptr(),
            libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(libc::write(descriptor, b"logical".as_ptr().cast(), 7), 7);
        assert_eq!(sandbox_close(descriptor), 0);
    });

    let parent = runtime
        .filesystem
        .root()
        .join(fixture.lower.strip_prefix("/").unwrap());
    assert!(parent.join(".metadata").is_file());
    let business_names = std::fs::read_dir(parent)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .filter(|name| {
            name != ".metadata" && !name.as_encoded_bytes().starts_with(b".agora-write-lease-")
        })
        .collect::<Vec<_>>();
    assert_eq!(business_names.len(), 1);
    assert_ne!(business_names[0], ".metadata");
}

fn directory_contains_for_test(directory: &Path, needle: &[u8]) -> bool {
    std::fs::read_dir(directory).unwrap().any(|entry| {
        let path = entry.unwrap().path();
        if path.is_dir() {
            directory_contains_for_test(&path, needle)
        } else {
            std::fs::read(path)
                .map(|contents| {
                    contents
                        .windows(needle.len())
                        .any(|window| window == needle)
                })
                .unwrap_or(false)
        }
    })
}

#[test]
fn mutations_update_only_the_overlay_view() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("source");
    let target = fixture.lower.join("target");
    std::fs::write(&source, b"host").unwrap();
    let source_path = Fixture::c_path(&source);
    let target_path = Fixture::c_path(&target);

    fixture
        .runtime
        .rename(
            libc::AT_FDCWD,
            source_path.as_ptr(),
            libc::AT_FDCWD,
            target_path.as_ptr(),
        )
        .unwrap();
    assert_eq!(std::fs::read(&source).unwrap(), b"host");
    assert!(fixture.runtime.filesystem.prepare_read(&source).is_err());
    assert_eq!(
        std::fs::read(fixture.runtime.filesystem.prepare_read(&target).unwrap()).unwrap(),
        b"host"
    );

    fixture
        .runtime
        .remove(libc::AT_FDCWD, target_path.as_ptr(), false)
        .unwrap();
    assert!(fixture.runtime.filesystem.prepare_read(&target).is_err());
}

#[test]
fn the_control_namespace_is_not_addressable_through_the_mapped_root() {
    let fixture = Fixture::new();
    let control = fixture.runtime.filesystem.root().join(".vfs.lock");
    let control = Fixture::c_path(&control);

    assert!(
        fixture
            .runtime
            .map(control.as_ptr(), libc::AT_FDCWD)
            .is_err()
    );
}

#[test]
fn directory_cursor_prefers_upper_entries_and_hides_whiteouts() {
    let fixture = Fixture::new();
    let lower = fixture.lower.join("directory");
    std::fs::create_dir(&lower).unwrap();
    std::fs::write(lower.join("removed"), b"host").unwrap();
    fixture
        .runtime
        .filesystem
        .remove(&lower.join("removed"), false)
        .unwrap();
    let view = fixture.runtime.filesystem.directory_view(&lower).unwrap();
    let mut cursor = DirectoryCursor::filter(&view);

    assert_eq!(cursor.include(b"same", false).unwrap(), b"same");
    assert!(cursor.include(b"same", true).is_none());
    assert!(cursor.include(b"removed", true).is_none());
    assert_eq!(cursor.include(b"lower-only", true).unwrap(), b"lower-only");
}

#[test]
fn filesystem_ffi_panics_fail_closed_with_io_error() {
    unsafe { *libc::__error() = 0 };

    let result = catch_filesystem_panic(-1, || panic!("hook failure"));

    assert_eq!(result, -1);
    assert_eq!(unsafe { *libc::__error() }, libc::EIO);
}

#[test]
fn filesystem_errors_preserve_errno_and_default_to_io_error() {
    let denied = anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EACCES));
    assert_eq!(error_errno(&denied), libc::EACCES);
    for (kind, errno) in [
        (std::io::ErrorKind::NotFound, libc::ENOENT),
        (std::io::ErrorKind::PermissionDenied, libc::EACCES),
        (std::io::ErrorKind::AlreadyExists, libc::EEXIST),
        (std::io::ErrorKind::InvalidInput, libc::EINVAL),
        (std::io::ErrorKind::InvalidData, libc::EINVAL),
        (std::io::ErrorKind::Interrupted, libc::EINTR),
        (std::io::ErrorKind::Unsupported, libc::ENOTSUP),
        (std::io::ErrorKind::OutOfMemory, libc::ENOMEM),
        (std::io::ErrorKind::NotADirectory, libc::ENOTDIR),
        (std::io::ErrorKind::IsADirectory, libc::EISDIR),
        (std::io::ErrorKind::DirectoryNotEmpty, libc::ENOTEMPTY),
        (std::io::ErrorKind::Other, libc::EIO),
    ] {
        assert_eq!(error_errno(&std::io::Error::from(kind).into()), errno);
    }
    let missing_socket = PathBuf::from(format!("/tmp/agora-missing-{}.sock", uuid::Uuid::new_v4()));
    let local_error = match crate::filesystem::broker::LocalClient::new(missing_socket, "token")
        .open(Path::new("/tmp"), libc::O_RDONLY)
    {
        Ok(_) => panic!("missing local broker should reject open"),
        Err(error) => error,
    };
    let local_errno = local_error.errno();
    assert_eq!(error_errno(&anyhow::Error::new(local_error)), local_errno);
    assert_eq!(error_errno(&anyhow::anyhow!("no errno")), libc::EIO);
}

#[test]
fn dirty_ranges_merge_overlaps_and_keep_disjoint_ranges() {
    let mut ranges = ByteRangeSet::default();
    ranges.insert(LocalByteRange::new(0, 4).unwrap());

    ranges.insert(LocalByteRange::new(3, 8).unwrap());
    ranges.insert(LocalByteRange::new(12, 16).unwrap());

    assert_eq!(
        ranges.as_slice(),
        &[
            LocalByteRange::new(0, 8).unwrap(),
            LocalByteRange::new(12, 16).unwrap(),
        ]
    );
}

#[test]
fn logical_current_directory_drives_relative_path_resolution() {
    let fixture = Fixture::new();
    let directory = fixture.lower.join("directory");
    let file = directory.join("file");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(&file, b"content").unwrap();
    let directory_path = Fixture::c_path(&directory);

    let (mapped, logical, remote, anchor) = fixture
        .runtime
        .prepare_change_directory(directory_path.as_ptr())
        .unwrap();
    assert_eq!(Path::new(mapped.to_str().unwrap()), directory);
    fixture
        .runtime
        .set_current_directory_state(logical, remote, anchor);

    with_test_runtime(&fixture.runtime, || unsafe {
        let mut cwd = vec![0_i8; libc::PATH_MAX as usize];
        assert_eq!(
            CStr::from_ptr(sandbox_getcwd(cwd.as_mut_ptr(), cwd.len())).to_bytes(),
            directory.as_os_str().as_encoded_bytes()
        );
        let descriptor = sandbox_open_with_mode(c"file".as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        assert_eq!(sandbox_close(descriptor), 0);
    });
}

#[test]
fn chdir_updates_the_logical_directory_after_the_native_change_succeeds() {
    struct RestoreDirectory(PathBuf);

    impl Drop for RestoreDirectory {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).unwrap();
        }
    }

    let restore = RestoreDirectory(std::env::current_dir().unwrap());
    let fixture = Fixture::new();
    let directory = fixture.lower.join("directory");
    std::fs::create_dir(&directory).unwrap();
    let canonical_directory = directory.canonicalize().unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(sandbox_chdir(Fixture::c_path(&directory).as_ptr()), 0);
        let current = sandbox_getcwd(std::ptr::null_mut(), 0);
        assert!(!current.is_null());
        assert_eq!(
            CStr::from_ptr(current).to_bytes(),
            canonical_directory.as_os_str().as_encoded_bytes()
        );
        libc::free(current.cast());
    });

    drop(restore);
}

#[test]
fn tracked_current_directory_participates_in_the_filesystem_fork_barrier() {
    struct PreparedFork;

    impl PreparedFork {
        fn new() -> Self {
            unsafe { lock_filesystem_before_fork() };
            Self
        }
    }

    impl Drop for PreparedFork {
        fn drop(&mut self) {
            unsafe { unlock_filesystem_after_fork() };
        }
    }

    let fixture = Fixture::new();
    let barrier = PreparedFork::new();
    std::thread::scope(|scope| {
        let (started, started_rx) = std::sync::mpsc::sync_channel(1);
        let (finished, finished_rx) = std::sync::mpsc::sync_channel(1);
        let runtime = &fixture.runtime;
        scope.spawn(move || {
            with_test_runtime(runtime, || {
                started.send(()).unwrap();
                finished.send(tracked_current_directory()).unwrap();
            });
        });

        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            finished_rx.recv_timeout(Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(barrier);
        assert!(
            finished_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_some()
        );
    });
}

#[test]
fn forked_child_does_not_reenter_inherited_filesystem_runtime() {
    let fixture = Fixture::new();
    with_test_runtime(&fixture.runtime, || {
        std::thread::scope(|scope| {
            let (locked, ready) = std::sync::mpsc::sync_channel(0);
            let runtime = &fixture.runtime;
            scope.spawn(move || {
                with_test_runtime(runtime, || {
                    let _guard = FilesystemHookGuard::enter().unwrap();
                    let _current_directory = runtime.current_directory.lock().unwrap();
                    locked.send(()).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(100));
                });
            });
            ready.recv().unwrap();

            let child = unsafe { libc::fork() };
            assert!(
                child >= 0,
                "fork failed: {}",
                std::io::Error::last_os_error()
            );
            if child == 0 {
                let status = i32::from(unsafe { sandbox_chdir(c".".as_ptr()) } != 0);
                unsafe { libc::_exit(status) };
            }

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let mut status = 0;
            loop {
                let waited = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
                if waited == child {
                    break;
                }
                assert_eq!(
                    waited,
                    0,
                    "waitpid failed: {}",
                    std::io::Error::last_os_error()
                );
                if std::time::Instant::now() >= deadline {
                    unsafe {
                        libc::kill(child, libc::SIGKILL);
                        libc::waitpid(child, &mut status, 0);
                    }
                    panic!("forked child deadlocked in inherited filesystem state");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
        });
    });
}

#[test]
fn fchdir_fails_before_changing_directory_when_logical_resolution_fails() {
    struct RestoreDirectory(PathBuf);

    impl Drop for RestoreDirectory {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).unwrap();
        }
    }

    let original = std::env::current_dir().unwrap();
    let _restore = RestoreDirectory(original.clone());
    let fixture = Fixture::new();
    let invalid = fixture.runtime.filesystem.root().join(".agora-entry-*");
    std::fs::create_dir(&invalid).unwrap();
    let directory = std::fs::File::open(invalid).unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        *libc::__error() = 0;
        assert_eq!(sandbox_fchdir(directory.as_raw_fd()), -1);
        assert_eq!(*libc::__error(), libc::EIO);
        assert_eq!(std::env::current_dir().unwrap(), original);
    });
}

#[test]
fn logical_directory_modes_control_traversal_and_parent_mutation() {
    let fixture = Fixture::new();
    let locked = fixture.lower.join("locked");
    let child = locked.join("child");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(&child, b"host").unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        let locked_path = Fixture::c_path(&locked);
        let child_path = Fixture::c_path(&child);
        let missing_path = Fixture::c_path(&locked.join("missing"));
        assert_eq!(sandbox_chmod(locked_path.as_ptr(), 0o000), 0);
        assert_eq!(
            sandbox_open_with_mode(child_path.as_ptr(), libc::O_RDONLY, 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::EACCES);
        let mut status = std::mem::zeroed::<libc::stat>();
        assert_eq!(sandbox_stat(missing_path.as_ptr(), &mut status), -1);
        assert_eq!(*libc::__error(), libc::EACCES);
        assert_eq!(sandbox_access(missing_path.as_ptr(), libc::F_OK), -1);
        assert_eq!(*libc::__error(), libc::EACCES);
        assert!(sandbox_opendir(locked_path.as_ptr()).is_null());
        assert_eq!(*libc::__error(), libc::EACCES);
        assert_eq!(sandbox_chdir(locked_path.as_ptr()), -1);
        assert_eq!(*libc::__error(), libc::EACCES);

        assert_eq!(sandbox_chmod(locked_path.as_ptr(), 0o500), 0);
        let created = Fixture::c_path(&locked.join("created"));
        assert_eq!(
            sandbox_open_with_mode(
                created.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o600,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::EACCES);
        assert_eq!(sandbox_mkdir(created.as_ptr(), 0o700), -1);
        assert_eq!(*libc::__error(), libc::EACCES);
        assert_eq!(sandbox_unlink(child_path.as_ptr()), -1);
        assert_eq!(*libc::__error(), libc::EACCES);
        assert_eq!(sandbox_rename(child_path.as_ptr(), created.as_ptr()), -1);
        assert_eq!(*libc::__error(), libc::EACCES);
    });

    assert_eq!(std::fs::read(child).unwrap(), b"host");
}

#[test]
fn final_symlinks_do_not_bypass_source_ancestor_search_permissions() {
    let fixture = Fixture::new();
    let locked = fixture.lower.join("locked-link-parent");
    let target_file = fixture.lower.join("target-file");
    let target_directory = fixture.lower.join("target-directory");
    let file_link = locked.join("file-link");
    let directory_link = locked.join("directory-link");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(&target_file, b"host").unwrap();
    std::fs::create_dir(&target_directory).unwrap();
    symlink(&target_file, &file_link).unwrap();
    symlink(&target_directory, &directory_link).unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(sandbox_chmod(Fixture::c_path(&locked).as_ptr(), 0o000), 0);
        let file_path = Fixture::c_path(&file_link);
        let directory_path = Fixture::c_path(&directory_link);
        let assert_denied = |result: anyhow::Result<()>| {
            assert_eq!(result.as_ref().err().map(error_errno), Some(libc::EACCES));
        };

        assert_denied(
            fixture
                .runtime
                .prepare_open(file_path.as_ptr(), libc::AT_FDCWD, libc::O_RDONLY, 0)
                .map(|_| ()),
        );
        assert_denied(
            fixture
                .runtime
                .prepare_fopen(file_path.as_ptr(), c"r".as_ptr())
                .map(|_| ()),
        );
        assert_denied(
            fixture
                .runtime
                .chmod(file_path.as_ptr(), libc::AT_FDCWD, 0o600, true),
        );
        assert_denied(
            fixture
                .runtime
                .prepare_change_directory(directory_path.as_ptr())
                .map(|_| ()),
        );
    });
}

#[test]
fn fork_does_not_deadlock_when_the_current_thread_owns_the_filesystem_guard() {
    let fixture = Fixture::new();
    with_test_runtime(&fixture.runtime, || {
        let probe = unsafe { libc::fork() };
        assert!(
            probe >= 0,
            "probe fork failed: {}",
            std::io::Error::last_os_error()
        );
        if probe == 0 {
            unsafe { libc::alarm(2) };
            let _guard = FilesystemHookGuard::enter().unwrap();
            let child = unsafe { libc::fork() };
            if child == 0 {
                unsafe { libc::_exit(0) };
            }
            let mut status = 0;
            let waited = if child < 0 {
                -1
            } else {
                unsafe { libc::waitpid(child, &mut status, 0) }
            };
            let successful =
                waited == child && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
            unsafe { libc::_exit(i32::from(!successful)) };
        }

        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(probe, &mut status, 0) }, probe);
        assert!(
            libc::WIFEXITED(status),
            "fork deadlocked while the current thread held the filesystem guard"
        );
        assert_eq!(libc::WEXITSTATUS(status), 0);
    });
}

#[test]
fn filesystem_hook_guard_blocks_catchable_signals_while_state_is_active() {
    let fixture = Fixture::new();
    with_test_runtime(&fixture.runtime, || {
        let signal = super::super::tests::SignalMaskProbe::unblocked(libc::SIGUSR2);
        let guard = FilesystemHookGuard::enter().unwrap();

        assert!(signal.is_blocked());
        drop(guard);
        assert!(!signal.is_blocked());
    });
}

unsafe extern "C" fn flock_requiring_unblocked_signals(
    _descriptor: libc::c_int,
    _operation: libc::c_int,
) -> libc::c_int {
    if super::super::tests::SignalMaskProbe::signal_is_blocked(libc::SIGUSR2) {
        -1
    } else {
        0
    }
}

#[test]
fn native_blocking_flock_runs_after_filesystem_hook_state_is_released() {
    let fixture = Fixture::new();
    with_test_runtime(&fixture.runtime, || unsafe {
        let signal = super::super::tests::SignalMaskProbe::unblocked(libc::SIGUSR2);

        assert_eq!(
            sandbox_flock_with(-1, libc::LOCK_EX, flock_requiring_unblocked_signals),
            0
        );
        assert!(!signal.is_blocked());
    });
}

unsafe extern "C" fn fork_requiring_blocked_signals() -> libc::pid_t {
    let blocked = super::super::tests::SignalMaskProbe::signal_is_blocked(libc::SIGUSR2);
    unsafe { *libc::__error() = if blocked { libc::EAGAIN } else { libc::EIO } };
    -1
}

#[test]
fn native_fork_defers_signals_until_atfork_state_is_released() {
    let fixture = Fixture::new();
    with_test_runtime(&fixture.runtime, || unsafe {
        let signal = super::super::tests::SignalMaskProbe::unblocked(libc::SIGUSR2);

        assert_eq!(sandbox_fork_with(fork_requiring_blocked_signals), -1);
        assert_eq!(*libc::__error(), libc::EAGAIN);
        assert!(!signal.is_blocked());
    });
}

#[test]
fn exiting_thread_cannot_leave_the_filesystem_fork_barrier_read_locked() {
    let fixture = Fixture::new();
    with_test_runtime(&fixture.runtime, || {
        let probe = unsafe { libc::fork() };
        assert!(
            probe >= 0,
            "probe fork failed: {}",
            std::io::Error::last_os_error()
        );
        if probe == 0 {
            unsafe { libc::alarm(2) };
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    with_test_runtime(&fixture.runtime, || {
                        std::mem::forget(FilesystemHookGuard::enter().unwrap());
                    });
                });
            });
            let child = unsafe { libc::fork() };
            if child == 0 {
                unsafe { libc::_exit(0) };
            }
            let mut status = 0;
            let waited = if child < 0 {
                -1
            } else {
                unsafe { libc::waitpid(child, &mut status, 0) }
            };
            let successful =
                waited == child && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
            unsafe { libc::_exit(i32::from(!successful)) };
        }

        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(probe, &mut status, 0) }, probe);
        assert!(
            libc::WIFEXITED(status),
            "an exited hook thread left the filesystem fork barrier locked"
        );
        assert_eq!(libc::WEXITSTATUS(status), 0);
    });
}

#[test]
fn recursive_filesystem_hooks_delegate_to_the_native_operations() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("file");
    let renamed = fixture.lower.join("renamed");
    let directory = fixture.lower.join("directory");
    let directory_at = fixture.lower.join("directory-at");
    let symlink = fixture.lower.join("symlink");
    let symlink_at = fixture.lower.join("symlink-at");
    std::fs::write(&file, b"content").unwrap();
    let file_path = Fixture::c_path(&file);
    let renamed_path = Fixture::c_path(&renamed);
    let root_path = Fixture::c_path(&fixture.lower);

    with_test_runtime(&fixture.runtime, || unsafe {
        let _guard = FilesystemHookGuard::enter().unwrap();
        let descriptor = sandbox_open_with_mode(file_path.as_ptr(), libc::O_RDWR, 0);
        assert!(descriptor >= 0);
        assert_eq!(sandbox_commit_synced_descriptor(descriptor), 0);
        let duplicate = sandbox_dup(descriptor);
        assert!(duplicate >= 0);
        let duplicate_target = libc::dup(descriptor);
        assert!(duplicate_target >= 0);
        assert_eq!(sandbox_dup2(descriptor, duplicate_target), duplicate_target);
        assert_eq!(sandbox_ftruncate(descriptor, 4), 0);
        assert_eq!(sandbox_fsync(descriptor), 0);
        assert_eq!(sandbox_fchmod(descriptor, 0o600), 0);
        assert_eq!(sandbox_utimes(file_path.as_ptr(), std::ptr::null()), 0);
        assert_eq!(sandbox_lutimes(file_path.as_ptr(), std::ptr::null()), 0);
        assert_eq!(sandbox_futimes(descriptor, std::ptr::null()), 0);
        assert_eq!(sandbox_futimens(descriptor, std::ptr::null()), 0);
        assert_eq!(
            sandbox_utimensat(libc::AT_FDCWD, file_path.as_ptr(), std::ptr::null(), 0,),
            0
        );
        assert_eq!(sandbox_chflags(file_path.as_ptr(), 0), 0);
        assert_eq!(sandbox_fchflags(descriptor, 0), 0);
        let attribute = c"com.agora.coverage";
        let value = b"x";
        assert_eq!(
            sandbox_setxattr(
                file_path.as_ptr(),
                attribute.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            ),
            0
        );
        assert_eq!(
            sandbox_removexattr(file_path.as_ptr(), attribute.as_ptr(), 0),
            0
        );
        assert_eq!(
            sandbox_fsetxattr(
                descriptor,
                attribute.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            ),
            0
        );
        assert_eq!(sandbox_fremovexattr(descriptor, attribute.as_ptr(), 0), 0);
        assert_eq!(sandbox_close(duplicate), 0);
        assert_eq!(sandbox_close(duplicate_target), 0);
        assert_eq!(sandbox_descriptor_mutation(descriptor, |_| 73), 73);
        assert_eq!(
            sandbox_unsupported_path_mutation(file_path.as_ptr(), libc::AT_FDCWD, |_, _| 74),
            74
        );
        assert_eq!(sandbox_close(descriptor), 0);

        assert_eq!(sandbox_truncate(file_path.as_ptr(), 5), 0);
        assert_eq!(sandbox_chmod(file_path.as_ptr(), 0o640), 0);
        assert_eq!(sandbox_chown(file_path.as_ptr(), !0, !0), 0);
        assert_eq!(sandbox_lchown(file_path.as_ptr(), !0, !0), 0);
        let stream = sandbox_fopen(file_path.as_ptr(), c"r".as_ptr());
        assert!(!stream.is_null());
        assert_eq!(
            sandbox_freopen(file_path.as_ptr(), c"r".as_ptr(), stream),
            stream
        );
        assert_eq!(sandbox_fclose(stream), 0);

        let root =
            sandbox_open_with_mode(root_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY, 0);
        assert!(root >= 0);
        let opened = sandbox_openat_with_mode(root, c"file".as_ptr(), libc::O_RDONLY, 0);
        assert!(opened >= 0);
        assert_eq!(sandbox_close(opened), 0);
        assert_eq!(sandbox_faccessat(root, c"file".as_ptr(), libc::R_OK, 0), 0);
        assert_eq!(sandbox_fchmodat(root, c"file".as_ptr(), 0o600, 0), 0);
        assert_eq!(sandbox_fchownat(root, c"file".as_ptr(), !0, !0, 0), 0);
        let mut actions = std::mem::MaybeUninit::<libc::posix_spawn_file_actions_t>::uninit();
        assert_eq!(libc::posix_spawn_file_actions_init(actions.as_mut_ptr()), 0);
        assert_eq!(
            sandbox_spawn_addopen(
                actions.as_mut_ptr(),
                9,
                file_path.as_ptr(),
                libc::O_RDONLY,
                0,
            ),
            0
        );
        assert_eq!(
            libc::posix_spawn_file_actions_destroy(actions.as_mut_ptr()),
            0
        );

        let mut status = std::mem::zeroed();
        assert_eq!(sandbox_stat(file_path.as_ptr(), &mut status), 0);
        assert_eq!(sandbox_lstat(file_path.as_ptr(), &mut status), 0);
        assert_eq!(sandbox_fstatat(root, c"file".as_ptr(), &mut status, 0), 0);
        assert_eq!(sandbox_access(file_path.as_ptr(), libc::R_OK), 0);
        assert_eq!(
            sandbox_mkdir(Fixture::c_path(&directory).as_ptr(), 0o700),
            0
        );
        assert_eq!(sandbox_mkdirat(root, c"directory-at".as_ptr(), 0o700), 0);
        assert_eq!(
            sandbox_symlink(c"target".as_ptr(), Fixture::c_path(&symlink).as_ptr()),
            0
        );
        assert_eq!(
            sandbox_symlinkat(c"target".as_ptr(), root, c"symlink-at".as_ptr()),
            0
        );
        let mut link_target = [0_u8; 16];
        assert_eq!(
            sandbox_readlinkat(
                root,
                c"symlink-at".as_ptr(),
                link_target.as_mut_ptr().cast(),
                link_target.len(),
            ),
            6
        );
        assert_eq!(&link_target[..6], b"target");
        assert_eq!(sandbox_rename(file_path.as_ptr(), renamed_path.as_ptr()), 0);
        assert_eq!(
            sandbox_renameat(root, c"renamed".as_ptr(), root, c"file".as_ptr()),
            0
        );
        assert_eq!(sandbox_unlink(file_path.as_ptr()), 0);
        assert_eq!(sandbox_unlink(Fixture::c_path(&symlink).as_ptr()), 0);
        assert_eq!(sandbox_unlink(Fixture::c_path(&symlink_at).as_ptr()), 0);
        assert_eq!(sandbox_rmdir(Fixture::c_path(&directory).as_ptr()), 0);
        assert_eq!(
            sandbox_unlinkat(root, c"directory-at".as_ptr(), libc::AT_REMOVEDIR),
            0
        );

        let mut cwd = vec![0_i8; libc::PATH_MAX as usize];
        assert_eq!(
            sandbox_getcwd(cwd.as_mut_ptr(), cwd.len()),
            cwd.as_mut_ptr()
        );
        let handle = sandbox_opendir(root_path.as_ptr());
        assert!(!handle.is_null());
        assert!(!sandbox_readdir(handle).is_null());
        assert_eq!(sandbox_closedir(handle), 0);
        assert_eq!(sandbox_close(root), 0);
    });

    assert!(!file.exists());
    assert!(!renamed.exists());
    assert!(!directory.exists());
    assert!(!directory_at.exists());
    assert!(!symlink.exists());
    assert!(!symlink_at.exists());
}

#[test]
fn failed_native_opens_do_not_commit_staged_writes() {
    let fixture = Fixture::new();
    let open_file = fixture.lower.join("open-file");
    let fopen_file = fixture.lower.join("fopen-file");
    std::fs::write(&open_file, b"open").unwrap();
    std::fs::write(&fopen_file, b"fopen").unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(
            sandbox_open_with_mode(
                Fixture::c_path(&open_file).as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::EEXIST);
        assert!(sandbox_fopen(Fixture::c_path(&fopen_file).as_ptr(), c"wx".as_ptr()).is_null());
        assert_eq!(*libc::__error(), libc::EEXIST);
    });

    for (file, contents) in [
        (&open_file, b"open".as_slice()),
        (&fopen_file, b"fopen".as_slice()),
    ] {
        assert_eq!(
            fixture.runtime.filesystem.state_for_test(file).unwrap(),
            None
        );
        assert_eq!(std::fs::read(file).unwrap(), contents);
    }
}

#[test]
fn permission_denial_does_not_stage_a_cached_entry() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("read-only");
    std::fs::write(&file, b"host").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(
            sandbox_open_with_mode(Fixture::c_path(&file).as_ptr(), libc::O_WRONLY, 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::EACCES);
    });

    assert_eq!(
        fixture.runtime.filesystem.state_for_test(&file).unwrap(),
        None
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"host");
}

#[test]
fn nofollow_does_not_bypass_logical_permissions_on_regular_files() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("nofollow");
    std::fs::write(&file, b"host").unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        let path = Fixture::c_path(&file);
        assert_eq!(sandbox_chmod(path.as_ptr(), 0o000), 0);
        assert_eq!(
            sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW, 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::EACCES);
    });

    assert_eq!(std::fs::read(file).unwrap(), b"host");
}

#[test]
fn truncation_intent_requires_logical_write_permission() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("read-only-truncate");
    std::fs::write(&file, b"host").unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        let path = Fixture::c_path(&file);
        assert_eq!(sandbox_chmod(path.as_ptr(), 0o400), 0);
        assert_eq!(
            sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY | libc::O_TRUNC, 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::EACCES);
    });

    assert_eq!(std::fs::read(file).unwrap(), b"host");
}

#[test]
fn access_rejects_invalid_modes_with_logical_attributes() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("invalid-access-mode");
    std::fs::write(&file, b"host").unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        let path = Fixture::c_path(&file);
        assert_eq!(sandbox_chmod(path.as_ptr(), 0o600), 0);
        let invalid = libc::R_OK | 0x100;
        assert_eq!(sandbox_access(path.as_ptr(), invalid), -1);
        assert_eq!(*libc::__error(), libc::EINVAL);
        assert_eq!(
            sandbox_faccessat(libc::AT_FDCWD, path.as_ptr(), invalid, 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::EINVAL);
    });
}

#[test]
fn chmod_rejects_a_non_owner() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("owned-by-another-user");
    std::fs::write(&file, b"host").unwrap();
    let mut attributes = FileAttributes::from_metadata(&std::fs::metadata(&file).unwrap());
    attributes.uid = unsafe { libc::geteuid() }.wrapping_add(1);
    fixture
        .runtime
        .filesystem
        .set_attributes(&file, attributes)
        .unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(sandbox_chmod(Fixture::c_path(&file).as_ptr(), 0o600), -1);
        assert_eq!(*libc::__error(), libc::EPERM);
    });

    assert_eq!(std::fs::read(file).unwrap(), b"host");
}

#[test]
fn filesystem_interposers_apply_cow_metadata_and_merged_directory_views() {
    let fixture = Fixture::new();
    let writable = fixture.lower.join("writable");
    let rename_from = fixture.lower.join("rename-from");
    let rename_to = fixture.lower.join("rename-to");
    let directory = fixture.lower.join("directory");
    let created_directory = fixture.lower.join("created-directory");
    std::fs::write(&writable, b"host").unwrap();
    std::fs::write(&rename_from, b"rename").unwrap();
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("lower"), b"lower").unwrap();
    std::fs::write(directory.join("hidden"), b"hidden").unwrap();
    std::fs::write(
        fixture
            .runtime
            .filesystem
            .prepare_write(&directory.join("upper"), true)
            .unwrap(),
        b"upper",
    )
    .unwrap();
    fixture
        .runtime
        .filesystem
        .remove(&directory.join("hidden"), false)
        .unwrap();

    with_test_runtime(&fixture.runtime, || unsafe {
        let writable = Fixture::c_path(&writable);
        let descriptor =
            sandbox_open_with_mode(writable.as_ptr(), libc::O_WRONLY | libc::O_TRUNC, 0);
        assert!(descriptor >= 0);
        assert_eq!(
            fixture.runtime.tracked(descriptor).unwrap().path,
            writable.to_string_lossy()
        );
        assert_eq!(libc::write(descriptor, b"sandbox".as_ptr().cast(), 7), 7);
        assert_eq!(sandbox_close(descriptor), 0);
        assert!(fixture.runtime.tracked(descriptor).is_none());

        let stream = sandbox_fopen(writable.as_ptr(), c"r".as_ptr());
        assert!(!stream.is_null());
        let descriptor = libc::fileno(stream);
        assert!(fixture.runtime.tracked(descriptor).is_some());
        assert_eq!(sandbox_fclose(stream), 0);
        assert!(fixture.runtime.tracked(descriptor).is_none());

        let mut status = std::mem::zeroed();
        assert_eq!(sandbox_stat(writable.as_ptr(), &mut status), 0);
        assert_eq!(sandbox_lstat(writable.as_ptr(), &mut status), 0);
        assert_eq!(
            sandbox_fstatat(libc::AT_FDCWD, writable.as_ptr(), &mut status, 0,),
            0
        );
        assert_eq!(sandbox_access(writable.as_ptr(), libc::R_OK), 0);

        let rename_from = Fixture::c_path(&rename_from);
        let rename_to = Fixture::c_path(&rename_to);
        assert_eq!(sandbox_rename(rename_from.as_ptr(), rename_to.as_ptr()), 0);
        assert_eq!(sandbox_access(rename_from.as_ptr(), libc::F_OK), -1);
        assert_eq!(sandbox_access(rename_to.as_ptr(), libc::F_OK), 0);
        assert_eq!(sandbox_unlink(rename_to.as_ptr()), 0);
        assert_eq!(sandbox_access(rename_to.as_ptr(), libc::F_OK), -1);

        let created_directory = Fixture::c_path(&created_directory);
        assert_eq!(sandbox_mkdir(created_directory.as_ptr(), 0o755), 0);
        assert_eq!(sandbox_stat(created_directory.as_ptr(), &mut status), 0);
        let created_handle = sandbox_opendir(created_directory.as_ptr());
        assert!(!created_handle.is_null());
        while !sandbox_readdir(created_handle).is_null() {}
        assert_eq!(sandbox_closedir(created_handle), 0);
        assert_eq!(sandbox_rmdir(created_directory.as_ptr()), 0);

        let directory = Fixture::c_path(&directory);
        let handle = sandbox_opendir(directory.as_ptr());
        assert!(!handle.is_null());
        let mut names = HashSet::new();
        loop {
            let entry = sandbox_readdir(handle);
            if entry.is_null() {
                break;
            }
            names.insert(
                CStr::from_ptr((*entry).d_name.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        assert_eq!(sandbox_closedir(handle), 0);
        assert!(names.contains("lower"));
        assert!(names.contains("upper"));
        assert!(!names.contains("hidden"));
        assert!(!names.contains(".agora"));
    });

    assert_eq!(std::fs::read(&writable).unwrap(), b"host");
    assert_eq!(
        std::fs::read(fixture.runtime.filesystem.prepare_read(&writable).unwrap()).unwrap(),
        b"sandbox"
    );
    assert_eq!(std::fs::read(&rename_from).unwrap(), b"rename");
    assert!(!rename_to.exists());
    assert!(!created_directory.exists());
}

#[test]
fn filesystem_interposers_cover_relative_allocation_and_error_paths() {
    let fixture = Fixture::new();
    let directory = fixture.lower.join("directory");
    let existing = directory.join("existing");
    let created = directory.join("created");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(&existing, b"existing").unwrap();
    let directory_path = Fixture::c_path(&directory);
    let directory_descriptor = unsafe { libc::open(directory_path.as_ptr(), libc::O_RDONLY) };
    assert!(directory_descriptor >= 0);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_openat_with_mode(
            directory_descriptor,
            c"existing".as_ptr(),
            libc::O_RDONLY,
            0,
        );
        assert!(descriptor >= 0);
        assert_eq!(sandbox_close(descriptor), 0);

        let created_path = Fixture::c_path(&created);
        let descriptor =
            sandbox_open_with_mode(created_path.as_ptr(), libc::O_WRONLY | libc::O_CREAT, 0o600);
        assert!(descriptor >= 0);
        assert_eq!(sandbox_close(descriptor), 0);

        let stream = sandbox_fopen(created_path.as_ptr(), c"a+".as_ptr());
        assert!(!stream.is_null());
        assert_eq!(sandbox_fclose(stream), 0);

        let directory_handle = sandbox_opendir(directory_path.as_ptr());
        assert!(!directory_handle.is_null());
        assert_eq!(sandbox_closedir(directory_handle), 0);

        let native = libc::opendir(c"/".as_ptr());
        assert!(!native.is_null());
        assert!(!sandbox_readdir(native).is_null());
        assert_eq!(sandbox_closedir(native), 0);

        let mut cwd = vec![0_i8; libc::PATH_MAX as usize];
        assert_eq!(
            sandbox_getcwd(cwd.as_mut_ptr(), cwd.len()),
            cwd.as_mut_ptr()
        );
        let allocated = sandbox_getcwd(std::ptr::null_mut(), 0);
        assert!(!allocated.is_null());
        libc::free(allocated.cast());
        assert!(sandbox_getcwd(std::ptr::null_mut(), 1).is_null());
        assert!(sandbox_getcwd(cwd.as_mut_ptr(), 1).is_null());

        let mut status = std::mem::zeroed();
        let missing = Fixture::c_path(&directory.join("missing"));
        assert_eq!(
            sandbox_open_with_mode(missing.as_ptr(), libc::O_RDONLY, 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOENT);
        assert_eq!(
            sandbox_openat_with_mode(directory_descriptor, c"missing".as_ptr(), libc::O_RDONLY, 0,),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOENT);
        assert!(sandbox_fopen(missing.as_ptr(), c"r".as_ptr()).is_null());
        assert_eq!(*libc::__error(), libc::ENOENT);
        assert_eq!(sandbox_truncate(missing.as_ptr(), 0), -1);
        assert_eq!(*libc::__error(), libc::ENOENT);
        assert_eq!(sandbox_truncate(std::ptr::null(), 0), -1);
        assert_eq!(*libc::__error(), libc::EFAULT);
        assert_eq!(
            sandbox_openat_with_mode(-1, c"relative".as_ptr(), libc::O_RDONLY, 0),
            -1
        );
        assert_eq!(
            sandbox_open_with_mode(std::ptr::null(), libc::O_RDONLY, 0),
            -1
        );
        assert!(sandbox_fopen(created_path.as_ptr(), std::ptr::null()).is_null());
        assert_eq!(sandbox_stat(std::ptr::null(), &mut status), -1);
        assert_eq!(sandbox_lstat(std::ptr::null(), &mut status), -1);
        assert_eq!(
            sandbox_fstatat(libc::AT_FDCWD, std::ptr::null(), &mut status, 0),
            -1
        );
        assert_eq!(sandbox_access(std::ptr::null(), libc::F_OK), -1);
        assert_eq!(sandbox_unlink(std::ptr::null()), -1);
        assert_eq!(sandbox_unlinkat(libc::AT_FDCWD, std::ptr::null(), 0), -1);
        assert_eq!(sandbox_rmdir(std::ptr::null()), -1);
        assert_eq!(sandbox_symlink(std::ptr::null(), created_path.as_ptr()), -1);
        assert_eq!(*libc::__error(), libc::EFAULT);
        assert_eq!(sandbox_symlink(c"target".as_ptr(), std::ptr::null()), -1);
        assert_eq!(*libc::__error(), libc::EFAULT);
        assert_eq!(sandbox_rename(std::ptr::null(), created_path.as_ptr()), -1);
        assert_eq!(
            sandbox_renameat(
                libc::AT_FDCWD,
                std::ptr::null(),
                libc::AT_FDCWD,
                created_path.as_ptr(),
            ),
            -1
        );
        assert_eq!(sandbox_mkdir(std::ptr::null(), 0o755), -1);
        assert_eq!(sandbox_mkdirat(libc::AT_FDCWD, std::ptr::null(), 0o755), -1);
        assert_eq!(sandbox_chdir(std::ptr::null()), -1);
        assert!(sandbox_opendir(std::ptr::null()).is_null());
    });

    assert_eq!(unsafe { libc::close(directory_descriptor) }, 0);
    assert_eq!(
        fixture.runtime.filesystem.state_for_test(&created).unwrap(),
        Some(EntryState::Cow)
    );
    assert!(FilesystemHookRuntime::global().is_none());
}

#[test]
fn runtime_lookup_does_not_enter_initialization_before_the_ready_gate() {
    assert!(FilesystemHookRuntime::global_when_ready(false).is_none());
}

#[test]
fn mutation_interposers_keep_path_and_spawn_action_writes_in_the_overlay() {
    let fixture = Fixture::new();
    let existing = fixture.lower.join("existing");
    let created = fixture.lower.join("created");
    let renamed = fixture.lower.join("renamed");
    let hard_link = fixture.lower.join("hard-link");
    let hard_link_at = fixture.lower.join("hard-link-at");
    let clone = fixture.lower.join("clone");
    let clone_at = fixture.lower.join("clone-at");
    let copy = fixture.lower.join("copy");
    let directory = fixture.lower.join("directory");
    let symlink = fixture.lower.join("symlink");
    let symlink_at = fixture.lower.join("symlink-at");
    let deferred = fixture.lower.join("deferred");
    let unused_action = fixture.lower.join("unused-action");
    std::fs::write(&existing, b"original").unwrap();
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644)).unwrap();
    let directory_path = Fixture::c_path(&fixture.lower);
    let existing_path = Fixture::c_path(&existing);
    let untracked_descriptor = unsafe { libc::open(existing_path.as_ptr(), libc::O_RDWR) };
    assert!(untracked_descriptor >= 0);

    with_test_runtime(&fixture.runtime, || unsafe {
        let directory_descriptor = sandbox_open_with_mode(
            directory_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        );
        assert!(directory_descriptor >= 0);
        assert_eq!(sandbox_ftruncate(directory_descriptor, 0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(sandbox_fchmod(directory_descriptor, 0o700), 0);
        assert_eq!(sandbox_fchown(directory_descriptor, !0, !0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        let created_path = Fixture::c_path(&created);
        let descriptor = sandbox_creat(created_path.as_ptr(), 0o600);
        assert!(descriptor >= 0);
        assert_eq!(libc::write(descriptor, b"created".as_ptr().cast(), 7), 7);
        assert_eq!(sandbox_fchmod(descriptor, 0o640), 0);
        assert_eq!(sandbox_fchown(descriptor, !0, !0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(sandbox_ftruncate(descriptor, 3), 0);
        assert_eq!(sandbox_close(descriptor), 0);

        assert_eq!(sandbox_chmod(existing_path.as_ptr(), 0o600), 0);
        let mut status = std::mem::zeroed::<libc::stat>();
        assert_eq!(sandbox_stat(existing_path.as_ptr(), &mut status), 0);
        assert_eq!(u32::from(status.st_mode) & 0o777, 0o600);
        assert_eq!(
            existing.metadata().unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(sandbox_chown(existing_path.as_ptr(), !0, !0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            sandbox_fchmodat(directory_descriptor, c"existing".as_ptr(), 0o640, 0),
            0
        );
        assert_eq!(sandbox_stat(existing_path.as_ptr(), &mut status), 0);
        assert_eq!(u32::from(status.st_mode) & 0o777, 0o640);
        assert_eq!(
            existing.metadata().unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            sandbox_fchownat(directory_descriptor, c"existing".as_ptr(), !0, !0, 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(sandbox_truncate(existing_path.as_ptr(), 2), 0);
        assert_eq!(sandbox_ftruncate(untracked_descriptor, 1), -1);
        assert_eq!(*libc::__error(), libc::EPERM);
        assert_eq!(sandbox_chmod(directory_path.as_ptr(), 0o700), 0);
        assert_eq!(
            sandbox_fchmodat(
                directory_descriptor,
                c"existing".as_ptr(),
                0o600,
                libc::AT_SYMLINK_NOFOLLOW,
            ),
            0
        );
        assert_eq!(
            sandbox_fchownat(
                directory_descriptor,
                c"existing".as_ptr(),
                !0,
                !0,
                libc::AT_SYMLINK_NOFOLLOW,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            sandbox_link(existing_path.as_ptr(), Fixture::c_path(&hard_link).as_ptr()),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            sandbox_linkat(
                directory_descriptor,
                c"existing".as_ptr(),
                directory_descriptor,
                c"hard-link-at".as_ptr(),
                0,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            sandbox_clonefile(existing_path.as_ptr(), Fixture::c_path(&clone).as_ptr(), 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            sandbox_clonefileat(
                directory_descriptor,
                c"existing".as_ptr(),
                directory_descriptor,
                c"clone-at".as_ptr(),
                0,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            sandbox_copyfile(
                existing_path.as_ptr(),
                Fixture::c_path(&copy).as_ptr(),
                std::ptr::null_mut(),
                libc::COPYFILE_DATA,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);

        assert_eq!(
            sandbox_mkdirat(directory_descriptor, c"directory".as_ptr(), 0o700),
            0
        );
        assert!(
            fixture
                .runtime
                .filesystem
                .prepare_read(&created)
                .unwrap()
                .exists()
        );
        let result = sandbox_renameat(
            directory_descriptor,
            c"created".as_ptr(),
            directory_descriptor,
            c"renamed".as_ptr(),
        );
        assert_eq!(result, 0, "renameat errno {}", *libc::__error());
        assert_eq!(
            sandbox_unlinkat(directory_descriptor, c"renamed".as_ptr(), 0),
            0
        );

        assert_eq!(
            sandbox_symlink(c"target".as_ptr(), Fixture::c_path(&symlink).as_ptr()),
            0
        );
        assert_eq!(sandbox_lchown(existing_path.as_ptr(), !0, !0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            sandbox_copyfile(
                directory_path.as_ptr(),
                Fixture::c_path(&fixture.lower.join("directory-copy")).as_ptr(),
                std::ptr::null_mut(),
                libc::COPYFILE_RECURSIVE,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            sandbox_unlinkat(directory_descriptor, c"existing".as_ptr(), 1 << 20),
            -1
        );
        assert_eq!(*libc::__error(), libc::EINVAL);
        assert_eq!(
            sandbox_symlinkat(
                c"target".as_ptr(),
                directory_descriptor,
                c"symlink-at".as_ptr()
            ),
            0
        );

        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        assert_eq!(libc::posix_spawn_file_actions_init(&mut actions), 0);
        assert_eq!(
            sandbox_spawn_addopen(
                &mut actions,
                8,
                std::ptr::null(),
                libc::O_WRONLY | libc::O_CREAT,
                0o600,
            ),
            libc::EFAULT
        );
        assert_eq!(
            sandbox_spawn_addopen(
                &mut actions,
                8,
                Fixture::c_path(&fixture.lower.join("missing")).as_ptr(),
                libc::O_WRONLY,
                0,
            ),
            libc::ENOENT
        );
        assert_eq!(
            sandbox_spawn_addopen(
                &mut actions,
                9,
                Fixture::c_path(&deferred).as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o600,
            ),
            libc::ENOTSUP
        );
        assert_eq!(
            fixture
                .runtime
                .filesystem
                .state_for_test(&deferred)
                .unwrap(),
            None
        );
        assert_eq!(libc::posix_spawn_file_actions_destroy(&mut actions), 0);

        let mut unused_actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        assert_eq!(libc::posix_spawn_file_actions_init(&mut unused_actions), 0);
        assert_eq!(
            sandbox_spawn_addopen(
                &mut unused_actions,
                10,
                Fixture::c_path(&unused_action).as_ptr(),
                libc::O_WRONLY | libc::O_CREAT,
                0o600,
            ),
            libc::ENOTSUP
        );
        assert_eq!(
            libc::posix_spawn_file_actions_destroy(&mut unused_actions),
            0
        );
        assert_eq!(
            fixture
                .runtime
                .filesystem
                .state_for_test(&unused_action)
                .unwrap(),
            None
        );
        assert_eq!(sandbox_close(directory_descriptor), 0);
    });

    assert_eq!(unsafe { libc::close(untracked_descriptor) }, 0);
    assert_eq!(std::fs::read(&existing).unwrap(), b"original");
    assert_eq!(
        existing.metadata().unwrap().permissions().mode() & 0o777,
        0o644
    );
    for path in [
        &created,
        &renamed,
        &hard_link,
        &hard_link_at,
        &clone,
        &clone_at,
        &copy,
        &directory,
        &symlink,
        &symlink_at,
        &deferred,
        &unused_action,
    ] {
        assert!(
            path.symlink_metadata().is_err(),
            "host path was changed: {}",
            path.display()
        );
    }
    assert_eq!(
        std::fs::read(fixture.runtime.filesystem.prepare_read(&existing).unwrap()).unwrap(),
        b"or"
    );
    assert!(fixture.runtime.filesystem.prepare_read(&renamed).is_err());
    assert!(
        fixture
            .runtime
            .filesystem
            .prepare_directory(&directory)
            .is_ok()
    );
    assert_eq!(
        std::fs::read_link(fixture.runtime.filesystem.prepare_read(&symlink).unwrap()).unwrap(),
        Path::new("target")
    );
    assert_eq!(
        std::fs::read_link(
            fixture
                .runtime
                .filesystem
                .prepare_read(&symlink_at)
                .unwrap()
        )
        .unwrap(),
        Path::new("target")
    );
}

#[test]
fn removefile_fails_closed_without_mutating_the_lower_file() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("secure-remove");
    std::fs::write(&file, b"content").unwrap();
    let path = Fixture::c_path(&file);

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(
            sandbox_removefile(path.as_ptr(), std::ptr::null_mut(), 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        let directory = libc::open(Fixture::c_path(&fixture.lower).as_ptr(), libc::O_RDONLY);
        assert!(directory >= 0);
        assert_eq!(
            sandbox_removefileat(
                directory,
                c"secure-remove".as_ptr(),
                std::ptr::null_mut(),
                0
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(libc::close(directory), 0);
    });

    assert_eq!(std::fs::read(file).unwrap(), b"content");
}

#[test]
fn filesystem_interposers_delegate_before_a_runtime_is_active() {
    let fixture = Fixture::new();
    let path = |name: &str| fixture.lower.join(name);
    let file = path("file");
    let file_path = Fixture::c_path(&file);
    let root_path = Fixture::c_path(&fixture.lower);

    unsafe {
        let descriptor = sandbox_creat(file_path.as_ptr(), 0o600);
        assert!(descriptor >= 0);
        assert_eq!(libc::write(descriptor, b"content".as_ptr().cast(), 7), 7);
        assert_eq!(sandbox_ftruncate(descriptor, 6), 0);
        assert_eq!(sandbox_fchmod(descriptor, 0o640), 0);
        assert_eq!(sandbox_fchown(descriptor, !0, !0), 0);
        assert_eq!(sandbox_close(descriptor), 0);
        assert_eq!(sandbox_truncate(file_path.as_ptr(), 5), 0);
        assert_eq!(sandbox_chmod(file_path.as_ptr(), 0o600), 0);
        assert_eq!(sandbox_chown(file_path.as_ptr(), !0, !0), 0);
        assert_eq!(sandbox_lchown(file_path.as_ptr(), !0, !0), 0);

        let directory_descriptor =
            sandbox_open_with_mode(root_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY, 0);
        assert!(directory_descriptor >= 0);
        let opened =
            sandbox_openat_with_mode(directory_descriptor, c"file".as_ptr(), libc::O_RDONLY, 0);
        assert!(opened >= 0);
        assert_eq!(sandbox_close(opened), 0);
        assert_eq!(
            sandbox_fchmodat(directory_descriptor, c"file".as_ptr(), 0o600, 0),
            0
        );
        assert_eq!(
            sandbox_fchownat(directory_descriptor, c"file".as_ptr(), !0, !0, 0),
            0
        );
        let mut status = std::mem::zeroed();
        assert_eq!(sandbox_stat(file_path.as_ptr(), &mut status), 0);
        assert_eq!(sandbox_lstat(file_path.as_ptr(), &mut status), 0);
        assert_eq!(
            sandbox_fstatat(directory_descriptor, c"file".as_ptr(), &mut status, 0),
            0
        );
        assert_eq!(sandbox_access(file_path.as_ptr(), libc::R_OK), 0);
        let stream = sandbox_fopen(file_path.as_ptr(), c"r".as_ptr());
        assert!(!stream.is_null());
        assert_eq!(sandbox_fclose(stream), 0);
        assert_eq!(sandbox_chdir(c".".as_ptr()), 0);
        let mut cwd = vec![0_i8; libc::PATH_MAX as usize];
        assert_eq!(
            sandbox_getcwd(cwd.as_mut_ptr(), cwd.len()),
            cwd.as_mut_ptr()
        );

        assert_eq!(
            sandbox_link(file_path.as_ptr(), Fixture::c_path(&path("link")).as_ptr()),
            0
        );
        assert_eq!(
            sandbox_linkat(
                directory_descriptor,
                c"file".as_ptr(),
                directory_descriptor,
                c"link-at".as_ptr(),
                0,
            ),
            0
        );
        assert_eq!(
            sandbox_symlink(c"file".as_ptr(), Fixture::c_path(&path("symlink")).as_ptr()),
            0
        );
        assert_eq!(
            sandbox_symlinkat(
                c"file".as_ptr(),
                directory_descriptor,
                c"symlink-at".as_ptr()
            ),
            0
        );
        assert_eq!(
            sandbox_clonefile(
                file_path.as_ptr(),
                Fixture::c_path(&path("clone")).as_ptr(),
                0
            ),
            0
        );
        assert_eq!(
            sandbox_clonefileat(
                directory_descriptor,
                c"file".as_ptr(),
                directory_descriptor,
                c"clone-at".as_ptr(),
                0,
            ),
            0
        );
        assert_eq!(
            sandbox_copyfile(
                file_path.as_ptr(),
                Fixture::c_path(&path("copy")).as_ptr(),
                std::ptr::null_mut(),
                libc::COPYFILE_DATA,
            ),
            0
        );

        assert_eq!(
            sandbox_mkdir(Fixture::c_path(&path("directory")).as_ptr(), 0o700),
            0
        );
        assert_eq!(
            sandbox_mkdirat(directory_descriptor, c"directory-at".as_ptr(), 0o700),
            0
        );
        let handle = sandbox_opendir(root_path.as_ptr());
        assert!(!handle.is_null());
        assert!(!sandbox_readdir(handle).is_null());
        assert_eq!(sandbox_closedir(handle), 0);
        assert_eq!(
            sandbox_rename(
                Fixture::c_path(&path("link")).as_ptr(),
                Fixture::c_path(&path("renamed")).as_ptr(),
            ),
            0
        );
        assert_eq!(
            sandbox_renameat(
                directory_descriptor,
                c"link-at".as_ptr(),
                directory_descriptor,
                c"renamed-at".as_ptr(),
            ),
            0
        );
        assert_eq!(
            sandbox_unlink(Fixture::c_path(&path("renamed")).as_ptr()),
            0
        );
        assert_eq!(
            sandbox_unlinkat(directory_descriptor, c"renamed-at".as_ptr(), 0),
            0
        );
        assert_eq!(
            sandbox_rmdir(Fixture::c_path(&path("directory")).as_ptr()),
            0
        );
        assert_eq!(
            sandbox_unlinkat(
                directory_descriptor,
                c"directory-at".as_ptr(),
                libc::AT_REMOVEDIR
            ),
            0
        );

        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        assert_eq!(libc::posix_spawn_file_actions_init(&mut actions), 0);
        assert_eq!(
            sandbox_spawn_addopen(&mut actions, 9, file_path.as_ptr(), libc::O_RDONLY, 0),
            0
        );
        assert_eq!(libc::posix_spawn_file_actions_destroy(&mut actions), 0);
        assert_eq!(sandbox_close(directory_descriptor), 0);
    }

    assert_eq!(std::fs::read(file).unwrap(), b"conte");
}

#[test]
fn filesystem_interposers_publish_open_and_close_audit_events() {
    let mut fixture = Fixture::new();
    let file = fixture.lower.join("audited");
    std::fs::write(&file, b"content").unwrap();
    let path = Fixture::c_path(&file);
    let (audit, server) = audit_server(r#""Accepted""#, 2);
    fixture.runtime.audit = Some(audit);

    with_test_runtime(&fixture.runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
        assert!(descriptor >= 0);
        assert_eq!(sandbox_close(descriptor), 0);
    });

    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["event"]["type"], "file");
    assert_eq!(requests[0]["event"]["operation"], "open");
    assert_eq!(
        requests[0]["event"]["file"]["path"],
        file.to_string_lossy().as_ref()
    );
    assert_eq!(requests[0]["event"]["trace_id"], "test-trace");
    assert_eq!(requests[1]["event"]["operation"], "close");
}

#[test]
fn filesystem_interposers_fail_closed_when_initial_audit_validation_rejects_an_operation() {
    const DENIED: &str = r#"{"Error":{"errno":13,"message":"denied"}}"#;

    let mut fixture = Fixture::new();
    let file = fixture.lower.join("denied");
    std::fs::write(&file, b"content").unwrap();
    let path = Fixture::c_path(&file);
    let (audit, server) = audit_server(DENIED, 1);
    fixture.runtime.audit = Some(audit);

    with_test_runtime(&fixture.runtime, || unsafe {
        assert_eq!(sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0), -1);
        assert_eq!(*libc::__error(), libc::EACCES);
    });

    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn runtime_descriptor_helpers_cover_aliases_and_native_descriptors() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("file");
    std::fs::write(&file, b"content").unwrap();
    let path = Fixture::c_path(&file);
    let internal = fixture.directory.join("workdir/fs");
    let internal_file = internal.join("internal");
    std::fs::write(&internal_file, b"internal").unwrap();
    let lower_directory = Fixture::c_path(&fixture.lower);
    let internal_directory = Fixture::c_path(&internal);
    let internal_file = Fixture::c_path(&internal_file);

    flush_before_exec().unwrap();

    unsafe {
        let lower_descriptor =
            libc::open(lower_directory.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
        let internal_directory_descriptor = libc::open(
            internal_directory.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY,
        );
        let internal_file_descriptor = libc::open(internal_file.as_ptr(), libc::O_RDWR);
        assert!(lower_descriptor >= 0);
        assert!(internal_directory_descriptor >= 0);
        assert!(internal_file_descriptor >= 0);

        with_test_runtime(&fixture.runtime, || {
            assert_eq!(sandbox_descriptor_mutation(-1, |_| 0), -1);
            let descriptor = sandbox_open_with_mode(path.as_ptr(), libc::O_RDONLY, 0);
            assert!(descriptor >= 0);
            let duplicate = sandbox_dup(descriptor);
            assert!(duplicate >= 0);

            fixture.runtime.commit_all_open_files().unwrap();
            assert_eq!(
                fixture
                    .runtime
                    .prepare_descriptor_mutation(descriptor)
                    .err()
                    .unwrap()
                    .downcast_ref::<std::io::Error>()
                    .and_then(std::io::Error::raw_os_error),
                Some(libc::EALREADY)
            );

            let (open, _) = fixture.runtime.take_descriptor(descriptor).unwrap();
            fixture.runtime.restore_descriptor(descriptor, open);
            assert!(fixture.runtime.tracked_open(descriptor).is_some());

            assert_eq!(
                fixture
                    .runtime
                    .descriptor_directory_view(lower_descriptor)
                    .unwrap()
                    .1,
                FileLayer::Lower
            );
            assert_eq!(
                fixture
                    .runtime
                    .descriptor_directory_view(internal_directory_descriptor)
                    .unwrap()
                    .1,
                FileLayer::Upper
            );
            assert!(
                fixture
                    .runtime
                    .prepare_descriptor_mutation(internal_directory_descriptor)
                    .is_err()
            );
            assert!(
                fixture
                    .runtime
                    .prepare_descriptor_mutation(internal_file_descriptor)
                    .is_err()
            );
            assert!(fixture.runtime.refresh_attributes(-1, "missing").is_err());

            assert_eq!(sandbox_close(duplicate), 0);
            assert_eq!(sandbox_close(descriptor), 0);
            flush_at_exit();
        });

        assert!(configure_descriptor(-1, libc::O_RDONLY, false).is_err());
        assert_eq!(libc::close(lower_descriptor), 0);
        assert_eq!(libc::close(internal_directory_descriptor), 0);
        assert_eq!(libc::close(internal_file_descriptor), 0);
    }
}

#[test]
fn exit_flush_finishes_each_open_local_handle_once() {
    let fixture = Fixture::new();
    let runtime = FilesystemHookRuntime::new_encrypted(
        fixture.directory.join("exit-flush-workdir/fs"),
        b"test-key",
        b"0123456789abcdef",
    )
    .unwrap();
    let logical = fixture.lower.join("exit-flush.txt");
    let path = Fixture::c_path(&logical);

    with_test_runtime(&runtime, || unsafe {
        let descriptor = sandbox_open_with_mode(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(descriptor >= 0);
        assert_eq!(libc::write(descriptor, b"saved".as_ptr().cast(), 5), 5);
        let open = runtime.tracked_open(descriptor).unwrap();

        flush_at_exit();

        assert!(open.finished.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(sandbox_close(descriptor), 0);
    });
}
