use super::{OverlayStore, SourceIdentity, StagedWrite, WriteReservation};
use crate::filesystem::{EntryState, FileAttributes, FileCipher, Materializer};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

struct Fixture {
    directory: PathBuf,
    lower: PathBuf,
    store: OverlayStore,
}

impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("agora-overlay-{}", uuid::Uuid::new_v4()));
        let lower = directory.join("lower");
        let root = directory.join("fs");
        std::fs::create_dir_all(&lower).unwrap();
        let store = OverlayStore::new(root).unwrap();
        Self {
            directory,
            lower,
            store,
        }
    }

    fn encrypted() -> (Self, FileCipher) {
        let directory =
            std::env::temp_dir().join(format!("agora-overlay-{}", uuid::Uuid::new_v4()));
        let lower = directory.join("lower");
        let root = directory.join("fs");
        std::fs::create_dir_all(&lower).unwrap();
        let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
        let store = OverlayStore::encrypted(root, cipher.clone()).unwrap();
        (
            Self {
                directory,
                lower,
                store,
            },
            cipher,
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}

#[test]
fn internal_transaction_paths_bypass_overlay_publication() {
    let fixture = Fixture::new();
    let internal = fixture.store.root().join("internal-created");

    let (staged, existed, lease, directory) = fixture
        .store
        .transaction(|transaction| {
            let (staged, existed, lease) = transaction.stage_file_open(&internal, true, true)?;
            let directory = transaction.prepare_directory(fixture.store.root())?;
            Ok((staged, existed, lease, directory))
        })
        .unwrap();

    assert!(existed);
    assert!(lease.is_none());
    assert_eq!(staged.destination(), internal);
    assert_eq!(directory, fixture.store.root());
    std::fs::write(staged.destination(), b"internal").unwrap();
    fixture.store.commit_created_file(staged, 0o600).unwrap();
    assert_eq!(std::fs::read(internal).unwrap(), b"internal");
}

#[test]
fn native_metadata_passthrough_rejects_internal_and_dangling_symlink_paths() {
    let fixture = Fixture::new();
    assert!(
        !fixture
            .store
            .native_metadata_passthrough(fixture.store.root(), true, |_| true)
            .unwrap()
    );

    let dangling = fixture.lower.join("dangling");
    symlink("missing-target", &dangling).unwrap();
    assert!(
        !fixture
            .store
            .native_metadata_passthrough(&dangling, true, |_| true)
            .unwrap()
    );
}

#[test]
fn encrypted_exclusive_reservation_is_removed_when_the_lease_cannot_open() {
    let (fixture, _) = Fixture::encrypted();
    let logical = fixture.lower.join("exclusive-create");
    assert_eq!(
        errno(&fixture.store.file_destination(&logical, false).unwrap_err()),
        Some(libc::ENOENT)
    );
    let destination = fixture.store.file_destination(&logical, true).unwrap();
    let lease = OverlayStore::write_lease_path(&destination).unwrap();
    std::fs::create_dir(&lease).unwrap();

    assert!(fixture.store.stage_file_open(&logical, true, true).is_err());
    assert!(!destination.exists());
}

#[test]
fn failed_symlink_creation_restores_absent_and_whiteout_metadata() {
    let fixture = Fixture::new();
    let invalid_target = Path::new(std::ffi::OsStr::from_bytes(b"invalid\0target"));

    let absent = fixture.lower.join("absent-link");
    assert!(
        fixture
            .store
            .transaction(|transaction| transaction.create_symlink(&absent, invalid_target))
            .is_err()
    );
    assert_eq!(fixture.store.state(&absent).unwrap(), None);
    assert!(!fixture.store.destination(&absent).unwrap().exists());

    let whiteout = fixture.lower.join("whiteout-link");
    fixture
        .store
        .set_state_for_test(&whiteout, EntryState::Whiteout)
        .unwrap();
    assert!(
        fixture
            .store
            .transaction(|transaction| transaction.create_symlink(&whiteout, invalid_target))
            .is_err()
    );
    assert_eq!(
        fixture.store.state(&whiteout).unwrap(),
        Some(EntryState::Whiteout)
    );
}

#[test]
fn removing_an_entry_reports_parent_directory_permissions() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("entry");
    std::fs::write(&file, b"data").unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

    let error = OverlayStore::remove_existing(&file).unwrap_err();

    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(errno(&error), Some(libc::EACCES));
}

#[test]
fn abandoned_reservation_survives_when_its_lock_file_disappears() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("reserved");
    std::fs::write(&destination, b"reserved").unwrap();
    let staged = StagedWrite {
        logical: PathBuf::from("/reserved"),
        destination: destination.clone(),
        reservation: Some(WriteReservation {
            file: std::fs::File::open(&destination).unwrap(),
            lock_path: directory.path().join("missing-lock"),
        }),
    };

    drop(staged);

    assert_eq!(std::fs::read(destination).unwrap(), b"reserved");
}

fn errno(error: &anyhow::Error) -> Option<i32> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .and_then(std::io::Error::raw_os_error)
}

#[test]
fn overlay_lock_serializes_threads() {
    let directory =
        std::env::temp_dir().join(format!("agora-overlay-lock-{}", uuid::Uuid::new_v4()));
    let store = Arc::new(OverlayStore::new(directory.join("fs")).unwrap());
    let (first_entered_tx, first_entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let first_store = Arc::clone(&store);
    let first = std::thread::spawn(move || {
        first_store
            .with_lock(|| {
                first_entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
    });
    first_entered_rx.recv().unwrap();

    let (second_entered_tx, second_entered_rx) = mpsc::channel();
    let second_store = Arc::clone(&store);
    let second = std::thread::spawn(move || {
        second_store
            .with_lock(|| {
                second_entered_tx.send(()).unwrap();
                Ok(())
            })
            .unwrap();
    });
    let entered_while_locked = second_entered_rx
        .recv_timeout(Duration::from_millis(100))
        .is_ok();
    release_tx.send(()).unwrap();
    first.join().unwrap();
    second.join().unwrap();
    assert!(!entered_while_locked);

    drop(store);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn sequential_overlay_transactions_reuse_the_lock_descriptor() {
    let fixture = Fixture::new();
    let initial_opens = fixture.store.lock_open_count();

    assert_eq!(fixture.store.state(&fixture.lower).unwrap(), None);
    assert_eq!(fixture.store.state(&fixture.lower).unwrap(), None);

    assert_eq!(fixture.store.lock_open_count(), initial_opens);
}

#[test]
fn overlay_lock_recovers_when_application_replaces_cached_descriptor() {
    let fixture = Fixture::new();
    let lock_descriptor = fixture
        .store
        .lock_pool
        .lock()
        .unwrap()
        .files
        .last()
        .unwrap()
        .as_raw_fd();
    let mut sockets = [-1; 2];
    assert_eq!(
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sockets.as_mut_ptr()) },
        0
    );
    assert_eq!(
        unsafe { libc::dup2(sockets[0], lock_descriptor) },
        lock_descriptor
    );

    let result = fixture.store.state(&fixture.lower);
    let marker = b"x";
    assert_eq!(
        unsafe { libc::write(lock_descriptor, marker.as_ptr().cast(), marker.len()) },
        marker.len() as isize
    );
    let mut received = [0_u8; 1];
    assert_eq!(
        unsafe { libc::read(sockets[1], received.as_mut_ptr().cast(), received.len()) },
        received.len() as isize
    );
    assert_eq!(received, *marker);

    drop(fixture);
    unsafe {
        libc::close(lock_descriptor);
        libc::close(sockets[0]);
        libc::close(sockets[1]);
    }
    assert_eq!(result.unwrap(), None);
}

#[test]
fn overlay_transaction_batches_queries_under_one_lock_entry() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("transaction-file");
    std::fs::write(&logical, b"lower").unwrap();
    let before = fixture.store.transaction_count_for_test();

    fixture
        .store
        .transaction(|transaction| {
            assert!(transaction.visible_exists(&logical)?);
            assert_eq!(transaction.resolve_final(&logical, false)?, logical);
            assert_eq!(transaction.attributes(&logical)?, None);
            assert_eq!(
                transaction.records(&[logical.as_path()])?,
                vec![(None, None)]
            );
            Ok(())
        })
        .unwrap();

    assert_eq!(fixture.store.transaction_count_for_test() - before, 1);
}

#[test]
fn overlay_transaction_records_treat_the_logical_root_as_unmodified() {
    let fixture = Fixture::new();

    let records = fixture
        .store
        .transaction(|transaction| transaction.records(&[Path::new("/")]))
        .unwrap();

    assert_eq!(records, vec![(None, None)]);
}

#[test]
fn overlay_transaction_records_reconcile_each_unique_path_once() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("deep/ancestor/transaction-file");
    std::fs::create_dir_all(logical.parent().unwrap()).unwrap();
    std::fs::write(&logical, b"lower").unwrap();
    fixture.store.prepare_write(&logical, false).unwrap();
    let paths = logical
        .ancestors()
        .take_while(|path| *path != Path::new("/"))
        .collect::<Vec<_>>();
    let before = fixture.store.reconciliation_count_for_test();

    fixture
        .store
        .transaction(|transaction| transaction.records(&paths))
        .unwrap();

    assert_eq!(
        fixture.store.reconciliation_count_for_test() - before,
        paths.len()
    );
}

#[test]
fn overlay_transaction_records_stop_at_a_missing_upper_ancestor() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("deep/ancestor/transaction-file");
    std::fs::create_dir_all(logical.parent().unwrap()).unwrap();
    std::fs::write(&logical, b"lower").unwrap();
    let paths = logical
        .ancestors()
        .take_while(|path| *path != Path::new("/"))
        .collect::<Vec<_>>();
    let before = fixture.store.reconciliation_count_for_test();

    let records = fixture
        .store
        .transaction(|transaction| transaction.records(&paths))
        .unwrap();

    assert_eq!(records, vec![(None, None); paths.len()]);
    assert_eq!(fixture.store.reconciliation_count_for_test() - before, 1);
}

#[test]
fn overlay_transaction_records_reject_descendants_of_a_whiteout() {
    let fixture = Fixture::new();
    let directory = fixture.lower.join("removed-tree");
    std::fs::create_dir(&directory).unwrap();
    fixture.store.remove(&directory, true).unwrap();
    let child = directory.join("created-after-removal");
    std::fs::write(&child, b"lower").unwrap();

    let error = fixture
        .store
        .transaction(|transaction| transaction.records(&[child.as_path()]))
        .unwrap_err();

    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn overlay_transactions_serialize_distinct_stores_for_one_workspace() {
    let directory = std::env::temp_dir().join(format!(
        "agora-overlay-transaction-lock-{}",
        uuid::Uuid::new_v4()
    ));
    let root = directory.join("fs");
    let first_store = Arc::new(OverlayStore::new(&root).unwrap());
    let second_store = Arc::new(OverlayStore::new(&root).unwrap());
    let (first_entered_tx, first_entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let first = std::thread::spawn(move || {
        first_store
            .transaction(|_| {
                first_entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
    });
    first_entered_rx.recv().unwrap();

    let (second_entered_tx, second_entered_rx) = mpsc::channel();
    let second = std::thread::spawn(move || {
        second_store
            .transaction(|_| {
                second_entered_tx.send(()).unwrap();
                Ok(())
            })
            .unwrap();
    });

    assert!(
        second_entered_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    release_tx.send(()).unwrap();
    first.join().unwrap();
    second.join().unwrap();
    second_entered_rx.recv().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn read_uses_lower_without_materializing_host_files() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("file");
    std::fs::write(&source, b"first").unwrap();

    let mapped = fixture.store.prepare_read(&source).unwrap();
    assert_eq!(mapped, source);
    assert_eq!(std::fs::read(&mapped).unwrap(), b"first");
    assert_eq!(fixture.store.metadata.state(&source).unwrap(), None);
    assert_eq!(fixture.store.prepare_read(&source).unwrap(), mapped);

    std::fs::write(&source, b"second").unwrap();
    assert_eq!(fixture.store.prepare_read(&source).unwrap(), mapped);
    assert_eq!(std::fs::read(mapped).unwrap(), b"second");
}

#[test]
fn creating_upper_data_builds_a_continuous_marker_chain() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("one/two/file");
    std::fs::create_dir_all(logical.parent().unwrap()).unwrap();

    let upper = fixture.store.prepare_write(&logical, true).unwrap();
    std::fs::write(upper, b"upper").unwrap();

    let root = fixture.store.root();
    assert!(root.join(".metadata").is_file());
    for ancestor in logical
        .parent()
        .unwrap()
        .ancestors()
        .take_while(|ancestor| *ancestor != Path::new("/"))
    {
        let marker = root
            .join(ancestor.strip_prefix("/").unwrap())
            .join(".metadata");
        assert!(marker.is_file(), "missing {}", marker.display());
    }
}

#[test]
fn creating_an_upper_directory_adds_its_own_marker() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("created");

    let upper = fixture.store.create_directory(&logical, 0o700).unwrap();

    assert!(upper.join(".metadata").is_file());
}

#[test]
fn externally_removed_cow_backing_clears_metadata_and_reveals_lower() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("cow-file");
    std::fs::write(&logical, b"lower").unwrap();
    let upper = fixture.store.prepare_write(&logical, false).unwrap();
    std::fs::write(&upper, b"upper").unwrap();
    std::fs::remove_file(&upper).unwrap();

    assert_eq!(fixture.store.prepare_read(&logical).unwrap(), logical);
    assert_eq!(fixture.store.state(&logical).unwrap(), None);
}

#[test]
fn externally_removed_cached_backing_clears_cache_and_reveals_lower() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("cached-file");
    std::fs::write(&logical, b"lower").unwrap();
    let upper = fixture
        .store
        .prepare_executable(&logical, |temporary| {
            std::fs::write(temporary, b"cached")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();
    std::fs::remove_file(upper).unwrap();

    assert_eq!(fixture.store.prepare_read(&logical).unwrap(), logical);
    assert_eq!(fixture.store.state(&logical).unwrap(), None);
}

#[test]
fn whiteout_without_backing_remains_authoritative() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("whiteout");
    std::fs::write(&logical, b"lower").unwrap();
    fixture.store.remove(&logical, false).unwrap();

    assert!(fixture.store.prepare_read(&logical).is_err());
    assert!(
        fixture
            .store
            .plain_destination(logical.parent().unwrap())
            .unwrap()
            .join(".metadata")
            .is_file()
    );
    assert_eq!(
        fixture.store.state(&logical).unwrap(),
        Some(EntryState::Whiteout)
    );
}

#[test]
fn encrypted_unlink_publishes_metadata_once() {
    let (fixture, _) = Fixture::encrypted();
    let logical = fixture.lower.join("single-publication-unlink");
    std::fs::write(&logical, b"lower").unwrap();
    fixture
        .store
        .metadata
        .ensure_marker(&fixture.lower)
        .unwrap();
    let before = fixture.store.metadata.publication_count_for_test();

    fixture.store.remove(&logical, false).unwrap();

    assert_eq!(
        fixture.store.metadata.publication_count_for_test() - before,
        1
    );
}

#[test]
fn encrypted_directory_removal_does_not_reserve_a_file_backing_name() {
    let (fixture, _) = Fixture::encrypted();
    let directory = fixture.lower.join("removed-directory");
    std::fs::create_dir(&directory).unwrap();

    fixture.store.remove(&directory, true).unwrap();

    assert_eq!(
        fixture.store.metadata.encrypted_name(&directory).unwrap(),
        None
    );
    assert_eq!(
        fixture.store.state(&directory).unwrap(),
        Some(EntryState::Whiteout)
    );
}

#[test]
fn whiteout_removes_an_unexpected_upper_object_without_revealing_lower() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("whiteout-orphan");
    std::fs::write(&logical, b"lower").unwrap();
    fixture.store.remove(&logical, false).unwrap();
    let upper = fixture.store.plain_destination(&logical).unwrap();
    std::fs::write(&upper, b"orphan").unwrap();

    assert!(fixture.store.prepare_read(&logical).is_err());
    assert!(!upper.exists());
    assert_eq!(
        fixture.store.state(&logical).unwrap(),
        Some(EntryState::Whiteout)
    );
}

#[test]
fn whiteout_ancestor_hides_lower_children_created_later() {
    let fixture = Fixture::new();
    let logical_directory = fixture.lower.join("removed-tree");
    std::fs::create_dir(&logical_directory).unwrap();
    fixture.store.remove(&logical_directory, true).unwrap();
    let late_child = logical_directory.join("late-child");
    std::fs::write(&late_child, b"lower").unwrap();

    assert!(fixture.store.prepare_read(&late_child).is_err());
}

#[test]
fn unrecorded_upper_file_is_removed_and_lower_is_visible() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("orphan");
    std::fs::write(&logical, b"lower").unwrap();
    fixture.store.ensure_parent_locked(&logical).unwrap();
    let upper = fixture.store.plain_destination(&logical).unwrap();
    std::fs::write(&upper, b"orphan").unwrap();

    assert_eq!(fixture.store.prepare_read(&logical).unwrap(), logical);
    assert!(!upper.exists());
}

#[test]
fn attribute_only_lower_override_survives_reconciliation() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("attribute-only");
    std::fs::write(&logical, b"lower").unwrap();
    let attributes = FileAttributes::created_file(0o600);
    fixture
        .store
        .set_attributes(&logical, attributes.clone())
        .unwrap();

    assert_eq!(fixture.store.prepare_read(&logical).unwrap(), logical);
    assert_eq!(
        fixture.store.attributes(&logical).unwrap(),
        Some(attributes)
    );
    assert!(
        fixture
            .store
            .plain_destination(logical.parent().unwrap())
            .unwrap()
            .join(".metadata")
            .is_file()
    );
}

#[test]
fn unmarked_parent_invalidates_descendant_metadata_and_reveals_lower() {
    let fixture = Fixture::new();
    let directory = fixture.lower.join("untrusted-parent");
    let logical = directory.join("child");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(&logical, b"lower").unwrap();
    let upper = fixture.store.prepare_write(&logical, false).unwrap();
    std::fs::write(&upper, b"upper").unwrap();
    assert_eq!(
        fixture.store.state(&logical).unwrap(),
        Some(EntryState::Cow)
    );
    let upper_directory = fixture.store.plain_destination(&directory).unwrap();
    std::fs::remove_file(upper_directory.join(".metadata")).unwrap();

    assert_eq!(fixture.store.prepare_read(&logical).unwrap(), logical);
    assert!(!upper_directory.exists());
    assert_eq!(fixture.store.state(&logical).unwrap(), None);
}

#[test]
fn externally_removed_upper_directory_reveals_lower_in_the_same_store() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("cow-directory");
    let upper = fixture.store.create_directory(&logical, 0o700).unwrap();
    std::fs::create_dir(&logical).unwrap();
    std::fs::remove_dir_all(upper).unwrap();

    assert_eq!(fixture.store.prepare_directory(&logical).unwrap(), logical);
    assert_eq!(fixture.store.state(&logical).unwrap(), None);
}

#[test]
fn externally_removed_merge_directory_invalidates_cached_child_metadata() {
    for fixture in [Fixture::new(), Fixture::encrypted().0] {
        let logical_directory = fixture.lower.join("merge-directory");
        let first = logical_directory.join("first");
        let second = logical_directory.join("second");
        std::fs::create_dir(&logical_directory).unwrap();
        std::fs::write(&first, b"lower first").unwrap();
        std::fs::write(&second, b"lower second").unwrap();
        for path in [&first, &second] {
            let upper = fixture.store.prepare_write(path, false).unwrap();
            std::fs::write(upper, b"upper").unwrap();
            fixture.store.prepare_read(path).unwrap();
        }
        let upper_directory = fixture.store.plain_destination(&logical_directory).unwrap();
        std::fs::remove_dir_all(&upper_directory).unwrap();

        assert_eq!(fixture.store.prepare_read(&first).unwrap(), first);
        assert!(!upper_directory.exists());
        assert_eq!(fixture.store.prepare_read(&second).unwrap(), second);
        assert!(!upper_directory.exists());
    }
}

#[test]
fn unrecorded_upper_symlink_is_removed_and_lower_is_visible() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("orphan-symlink");
    std::fs::write(&logical, b"lower").unwrap();
    fixture.store.ensure_parent_locked(&logical).unwrap();
    let upper = fixture.store.plain_destination(&logical).unwrap();
    symlink("somewhere", &upper).unwrap();

    assert_eq!(fixture.store.prepare_read(&logical).unwrap(), logical);
    assert!(upper.symlink_metadata().is_err());
}

#[test]
fn encrypted_orphan_is_removed_during_directory_reconciliation() {
    let (fixture, cipher) = Fixture::encrypted();
    let logical_directory = fixture.lower.join("encrypted-orphans");
    std::fs::create_dir(&logical_directory).unwrap();
    let upper_directory = fixture
        .store
        .ensure_directory_locked(&logical_directory)
        .unwrap();
    let encrypted_name = cipher.encrypt_name(b"orphan").unwrap();
    let orphan = upper_directory.join(encrypted_name);
    std::fs::write(&orphan, b"orphan").unwrap();

    fixture.store.directory_view(&logical_directory).unwrap();

    assert!(!orphan.exists());
}

#[test]
fn stale_encrypted_writer_cannot_recreate_a_reconciled_cow_file() {
    let (fixture, _) = Fixture::encrypted();
    let logical = fixture.lower.join("stale-writer");
    std::fs::write(&logical, b"lower").unwrap();
    let (staged, _, lease) = fixture
        .store
        .stage_file_open(&logical, false, false)
        .unwrap();
    let destination = staged.destination().to_path_buf();
    let lease = lease.unwrap();
    fixture.store.commit_write(staged).unwrap();
    std::fs::remove_file(&destination).unwrap();

    assert_eq!(fixture.store.prepare_read(&logical).unwrap(), logical);
    let mut late_plaintext = tempfile::tempfile().unwrap();
    use std::io::Write as _;
    late_plaintext.write_all(b"late upper").unwrap();
    assert_eq!(
        fixture
            .store
            .publish_encrypted(&mut late_plaintext, &lease)
            .unwrap(),
        None
    );
    assert!(!destination.exists());
}

#[test]
fn encrypted_writeback_rejects_invalid_or_stale_lease_destinations() {
    let (fixture, _) = Fixture::encrypted();
    let mut plaintext = tempfile::tempfile().unwrap();
    plaintext.write_all(b"plaintext").unwrap();

    let empty = tempfile::tempfile().unwrap();
    assert_eq!(
        fixture
            .store
            .publish_encrypted(&mut plaintext, &empty)
            .unwrap(),
        None
    );
    assert_eq!(
        fixture
            .store
            .overwrite_encrypted(&mut plaintext, &empty)
            .unwrap(),
        None
    );

    let oversized = tempfile::tempfile().unwrap();
    oversized
        .set_len((super::super::MAX_CONTROL_PATH_BYTES + 1) as u64)
        .unwrap();
    assert!(
        fixture
            .store
            .publish_encrypted(&mut plaintext, &oversized)
            .unwrap_err()
            .to_string()
            .contains("write lease path exceeds")
    );

    let oversized_destination = PathBuf::from("x".repeat(super::super::MAX_CONTROL_PATH_BYTES + 1));
    assert!(
        OverlayStore::write_write_lease_destination(&empty, &oversized_destination)
            .unwrap_err()
            .to_string()
            .contains("write lease path exceeds")
    );

    let outside = fixture.directory.join("outside");
    let outside_lease = tempfile::tempfile().unwrap();
    OverlayStore::write_write_lease_destination(&outside_lease, &outside).unwrap();
    assert_eq!(
        errno(
            &fixture
                .store
                .publish_encrypted(&mut plaintext, &outside_lease)
                .unwrap_err()
        ),
        Some(libc::EIO)
    );
    assert_eq!(
        errno(
            &fixture
                .store
                .overwrite_encrypted(&mut plaintext, &outside_lease)
                .unwrap_err()
        ),
        Some(libc::EIO)
    );

    let missing_destination = fixture.store.root().join("missing-current-lease");
    let missing_lease = tempfile::tempfile().unwrap();
    OverlayStore::write_write_lease_destination(&missing_lease, &missing_destination).unwrap();
    assert_eq!(
        fixture
            .store
            .publish_encrypted(&mut plaintext, &missing_lease)
            .unwrap(),
        None
    );
    assert_eq!(
        fixture
            .store
            .overwrite_encrypted(&mut plaintext, &missing_lease)
            .unwrap(),
        None
    );

    let replaced_destination = fixture.store.root().join("replaced-current-lease");
    let held_lease = tempfile::tempfile().unwrap();
    OverlayStore::write_write_lease_destination(&held_lease, &replaced_destination).unwrap();
    let current_lease = OverlayStore::write_lease_path(&replaced_destination).unwrap();
    std::fs::write(
        &current_lease,
        replaced_destination.as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    assert_eq!(
        fixture
            .store
            .overwrite_encrypted(&mut plaintext, &held_lease)
            .unwrap(),
        None
    );
}

#[test]
fn encrypted_namespace_leases_cover_file_directory_and_contention_paths() {
    let (fixture, _) = Fixture::encrypted();
    let destination = fixture.store.root().join("lease-target");
    let first = fixture
        .store
        .acquire_write_lease(&destination, libc::LOCK_EX | libc::LOCK_NB)
        .unwrap()
        .unwrap();
    assert_eq!(
        errno(
            &fixture
                .store
                .acquire_write_lease(&destination, libc::LOCK_EX | libc::LOCK_NB)
                .unwrap_err()
        ),
        Some(libc::EBUSY)
    );
    drop(first);

    let leases = fixture
        .store
        .acquire_namespace_leases(&destination, false)
        .unwrap();
    assert_eq!(leases.len(), 1);
    drop(leases);

    let directory = fixture.store.root().join("lease-directory");
    std::fs::create_dir(&directory).unwrap();
    let child_destination = directory.join("child");
    let child_lease_path = OverlayStore::write_lease_path(&child_destination).unwrap();
    std::fs::write(
        &child_lease_path,
        child_destination.as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    let leases = fixture
        .store
        .acquire_namespace_leases(&directory, true)
        .unwrap();
    assert_eq!(leases.len(), 1);
    drop(leases);

    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&child_lease_path)
        .unwrap();
    OverlayStore::flock(&held, libc::LOCK_EX | libc::LOCK_NB).unwrap();
    assert_eq!(
        errno(
            &fixture
                .store
                .acquire_namespace_leases(&directory, true)
                .unwrap_err()
        ),
        Some(libc::EBUSY)
    );
    OverlayStore::flock(&held, libc::LOCK_UN).unwrap();
}

#[test]
fn pending_plain_create_is_not_reconciled_as_an_orphan() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("pending-plain-create");
    let (staged, existed, lease) = fixture.store.stage_file_open(&logical, true, true).unwrap();
    let destination = staged.destination().to_path_buf();
    assert!(!existed);
    let lease = lease.expect("plain open should retain a staging lease");
    std::fs::write(&destination, b"pending").unwrap();

    assert!(fixture.store.prepare_read(&logical).is_err());
    assert!(destination.is_file());
    fixture.store.commit_created_file(staged, 0o600).unwrap();
    drop(lease);
    assert_eq!(fixture.store.prepare_read(&logical).unwrap(), destination);
}

#[test]
fn pending_encrypted_exclusive_create_is_not_reconciled_as_an_orphan() {
    let (fixture, _) = Fixture::encrypted();
    let logical = fixture.lower.join("pending-create");
    let (staged, existed, lease) = fixture.store.stage_file_open(&logical, true, true).unwrap();
    let destination = staged.destination().to_path_buf();
    assert!(!existed);
    assert!(lease.is_some());

    assert!(fixture.store.prepare_read(&logical).is_err());
    assert!(destination.is_file());
    fixture.store.commit_created_file(staged, 0o600).unwrap();
    assert_eq!(
        fixture.store.state(&logical).unwrap(),
        Some(EntryState::Cow)
    );
}

#[test]
fn externally_removed_root_marker_discards_all_upper_data() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("root-reset");
    std::fs::write(&logical, b"lower").unwrap();
    let upper = fixture.store.prepare_write(&logical, false).unwrap();
    std::fs::write(&upper, b"upper").unwrap();
    std::fs::remove_file(fixture.store.root().join(".metadata")).unwrap();

    assert_eq!(fixture.store.prepare_read(&logical).unwrap(), logical);
    assert!(!upper.exists());
    assert!(fixture.store.root().join(".metadata").is_file());
}

#[test]
fn reopening_after_root_marker_removal_discards_all_upper_data() {
    let directory = tempfile::tempdir().unwrap();
    let lower = directory.path().join("lower");
    let root = directory.path().join("fs");
    let logical = lower.join("root-reopen");
    std::fs::create_dir(&lower).unwrap();
    std::fs::write(&logical, b"lower").unwrap();
    let upper = {
        let store = OverlayStore::new(&root).unwrap();
        let upper = store.prepare_write(&logical, false).unwrap();
        std::fs::write(&upper, b"upper").unwrap();
        upper
    };
    std::fs::remove_file(root.join(".metadata")).unwrap();

    let reopened = OverlayStore::new(&root).unwrap();

    assert_eq!(reopened.prepare_read(&logical).unwrap(), logical);
    assert!(!upper.exists());
}

#[test]
fn encrypted_root_reads_use_the_backing_root_without_leaf_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("fs");
    let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
    let store = OverlayStore::encrypted(&root, cipher).unwrap();

    assert_eq!(store.prepare_read(Path::new("/")).unwrap(), Path::new("/"));
}

#[test]
fn encrypted_metadata_key_matches_the_encrypted_physical_name_without_plaintext() {
    let (fixture, cipher) = Fixture::encrypted();
    let logical = Path::new("/tmp/secret.txt");
    let destination = fixture.store.prepare_write(logical, true).unwrap();
    std::fs::write(&destination, b"encrypted-placeholder").unwrap();
    let root = fixture.store.root();

    let contents = std::fs::read(root.join("tmp/.metadata")).unwrap();
    assert!(
        !contents
            .windows(b"secret.txt".len())
            .any(|part| part == b"secret.txt")
    );
    let metadata: serde_json::Value = serde_json::from_slice(&contents).unwrap();
    assert_eq!(metadata["version"], 3);
    assert!(metadata.get("backing_names").is_none());
    let alias = metadata["entries"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap();

    assert!(alias.starts_with("enc_"));
    assert_eq!(cipher.decrypt_name(alias).unwrap(), b"secret.txt");
    assert_eq!(destination, root.join("tmp").join(alias));
    assert!(destination.is_file());
    assert!(!root.join("tmp/secret.txt").exists());
    assert!(metadata["entries"][alias].get("name").is_none());
}

#[test]
fn opening_an_overlay_does_not_scan_unrelated_nested_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("fs");
    let unrelated = root.join("unrelated");
    std::fs::create_dir_all(&unrelated).unwrap();
    std::fs::write(
        root.join(".metadata"),
        serde_json::to_vec(&serde_json::json!({"version": 3, "entries": {}})).unwrap(),
    )
    .unwrap();
    std::fs::write(unrelated.join(".metadata"), b"not-json").unwrap();

    let store = OverlayStore::new(&root).unwrap();

    assert_eq!(store.prepare_read(Path::new("/")).unwrap(), Path::new("/"));
    assert!(store.state(Path::new("/unrelated/entry")).is_err());
}

#[test]
fn write_intent_copies_up_and_preserves_cow_after_host_changes() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("file");
    std::fs::write(&source, b"host").unwrap();

    let mapped = fixture.store.prepare_write(&source, false).unwrap();
    std::fs::write(&mapped, b"sandbox").unwrap();
    std::fs::write(&source, b"changed host").unwrap();

    assert_eq!(fixture.store.prepare_read(&source).unwrap(), mapped);
    assert_eq!(std::fs::read(mapped).unwrap(), b"sandbox");
    assert_eq!(std::fs::read(source).unwrap(), b"changed host");
}

#[test]
fn visible_path_prefers_cow_content_and_rejects_whiteouts() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("file");
    std::fs::write(&source, b"host").unwrap();

    assert_eq!(
        fixture.store.visible_path(&source).unwrap(),
        source.canonicalize().unwrap()
    );

    let mapped = fixture.store.prepare_write(&source, false).unwrap();
    std::fs::write(&mapped, b"sandbox").unwrap();
    assert_eq!(fixture.store.visible_path(&source).unwrap(), mapped);
    assert_eq!(fixture.store.visible_path(&mapped).unwrap(), mapped);

    fixture.store.remove(&source, false).unwrap();
    assert!(fixture.store.visible_path(&source).is_err());
}

#[test]
fn create_delete_and_recreate_use_cow_and_whiteouts() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("created");

    let mapped = fixture.store.prepare_write(&source, true).unwrap();
    std::fs::write(&mapped, b"created").unwrap();
    assert_eq!(
        fixture.store.metadata.state(&source).unwrap(),
        Some(EntryState::Cow)
    );

    fixture.store.remove(&source, false).unwrap();
    assert_eq!(
        fixture.store.metadata.state(&source).unwrap(),
        Some(EntryState::Whiteout)
    );
    assert!(fixture.store.prepare_read(&source).is_err());

    let recreated = fixture.store.prepare_write(&source, true).unwrap();
    std::fs::write(&recreated, b"recreated").unwrap();
    assert_eq!(fixture.store.prepare_read(&source).unwrap(), recreated);
}

#[test]
fn directory_view_keeps_lower_entries_lazy_and_tracks_whiteouts() {
    let fixture = Fixture::new();
    let lower_file = fixture.lower.join("lower");
    let removed_file = fixture.lower.join("removed");
    let cow_file = fixture.lower.join("cow");
    std::fs::write(&lower_file, b"lower").unwrap();
    std::fs::write(&removed_file, b"removed").unwrap();
    std::fs::write(&cow_file, b"host cow").unwrap();
    fixture.store.remove(&removed_file, false).unwrap();
    let cow = fixture.store.prepare_write(&cow_file, false).unwrap();
    std::fs::write(cow, b"sandbox cow").unwrap();

    let view = fixture.store.directory_view(&fixture.lower).unwrap();
    let upper_names = std::fs::read_dir(view.primary())
        .unwrap()
        .map(|entry| {
            let name = entry.unwrap().file_name();
            view.aliases().get(&name).cloned().unwrap_or(name)
        })
        .filter(|name| !view.hidden().contains(name))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(upper_names.len(), 1);
    assert!(upper_names.contains(std::ffi::OsStr::new("cow")));
    assert_eq!(view.lower(), Some(fixture.lower.as_path()));
    assert!(view.hidden().contains(std::ffi::OsStr::new("removed")));

    let lower_names = std::fs::read_dir(view.lower().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(lower_names.contains(std::ffi::OsStr::new("lower")));
    assert!(lower_names.contains(std::ffi::OsStr::new("cow")));
    assert!(lower_names.contains(std::ffi::OsStr::new("removed")));
}

#[test]
fn reading_a_lower_directory_does_not_materialize_an_upper_directory() {
    let fixture = Fixture::new();
    let directory = fixture.lower.join("read-only");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o555)).unwrap();

    let visible = fixture.store.prepare_directory(&directory).unwrap();

    assert_eq!(visible, directory);
    assert!(
        !fixture
            .store
            .plain_destination(&directory)
            .unwrap()
            .exists()
    );
}

#[test]
fn removing_a_lower_directory_requires_the_merged_view_to_be_empty() {
    let fixture = Fixture::new();
    let directory = fixture.lower.join("directory");
    let child = directory.join("child");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(&child, b"host").unwrap();

    let error = fixture.store.remove(&directory, true).unwrap_err();
    assert_eq!(
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .and_then(std::io::Error::raw_os_error),
        Some(libc::ENOTEMPTY)
    );

    fixture.store.remove(&child, false).unwrap();
    fixture.store.remove(&directory, true).unwrap();
    assert!(fixture.store.prepare_read(&directory).is_err());
    assert!(directory.exists());
}

#[test]
fn removing_an_encrypted_upper_directory_rejects_aliased_children() {
    let (fixture, _) = Fixture::encrypted();
    let directory = fixture.lower.join("directory");
    let child = directory.join("child");
    fixture.store.create_directory(&directory, 0o700).unwrap();
    let staged = fixture.store.stage_write(&child, true).unwrap();
    std::fs::write(staged.destination(), b"child").unwrap();
    fixture.store.commit_write(staged).unwrap();

    let error = fixture.store.remove(&directory, true).unwrap_err();

    assert_eq!(errno(&error), Some(libc::ENOTEMPTY));
    assert!(fixture.store.prepare_read(&child).unwrap().is_file());
}

#[test]
fn rename_and_mkdir_never_change_lower_paths() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("source");
    let target = fixture.lower.join("target");
    std::fs::write(&source, b"host").unwrap();

    fixture.store.rename(&source, &target).unwrap();
    assert!(fixture.store.prepare_read(&source).is_err());
    assert_eq!(
        fixture.store.state(&source).unwrap(),
        Some(EntryState::Whiteout)
    );
    assert_eq!(fixture.store.state(&target).unwrap(), Some(EntryState::Cow));
    assert_eq!(
        std::fs::read(fixture.store.prepare_read(&target).unwrap()).unwrap(),
        b"host"
    );
    assert_eq!(std::fs::read(&source).unwrap(), b"host");
    assert!(!target.exists());

    let directory = fixture.lower.join("created-dir");
    let mapped = fixture.store.create_directory(&directory, 0o750).unwrap();
    assert!(mapped.is_dir());
    assert_eq!(
        mapped.metadata().unwrap().permissions().mode() & 0o777,
        0o750
    );
    assert!(!directory.exists());
}

#[test]
fn same_directory_rename_publishes_metadata_once() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("rename-source");
    let target = fixture.lower.join("rename-target");
    std::fs::write(&source, b"lower").unwrap();
    let upper = fixture.store.prepare_write(&source, false).unwrap();
    std::fs::write(upper, b"upper").unwrap();
    let before = fixture.store.metadata.publication_count_for_test();

    fixture.store.rename(&source, &target).unwrap();

    assert_eq!(
        fixture.store.metadata.publication_count_for_test() - before,
        1
    );
    assert_eq!(
        std::fs::read(fixture.store.prepare_read(&target).unwrap()).unwrap(),
        b"upper"
    );
}

#[test]
fn renaming_between_hard_links_to_the_same_file_is_a_noop() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("hard-link-source");
    let target = fixture.lower.join("hard-link-target");
    std::fs::write(&source, b"lower").unwrap();
    std::fs::hard_link(&source, &target).unwrap();

    fixture.store.rename(&source, &target).unwrap();

    assert_eq!(std::fs::read(&source).unwrap(), b"lower");
    assert_eq!(std::fs::read(&target).unwrap(), b"lower");
    assert_eq!(fixture.store.state(&source).unwrap(), None);
    assert_eq!(fixture.store.state(&target).unwrap(), None);
}

#[cfg(target_os = "macos")]
#[test]
fn case_only_file_rename_updates_the_physical_directory_entry() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("case-file");
    let target = fixture.lower.join("CASE-FILE");
    std::fs::write(&source, b"lower").unwrap();
    if !target.exists() {
        return;
    }

    fixture.store.rename(&source, &target).unwrap();

    let view = fixture.store.directory_view(&fixture.lower).unwrap();
    let visible = std::fs::read_dir(view.primary())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| !view.hidden().contains(name))
        .collect::<std::collections::BTreeSet<_>>();
    assert!(visible.contains(target.file_name().unwrap()));
    assert!(!visible.contains(source.file_name().unwrap()));
    assert_eq!(
        std::fs::read(fixture.store.prepare_read(&target).unwrap()).unwrap(),
        b"lower"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn case_only_upper_file_rename_preserves_its_contents() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("upper-case-file");
    let target = fixture.lower.join("UPPER-CASE-FILE");
    let upper = fixture.store.prepare_write(&source, true).unwrap();
    std::fs::write(upper, b"upper").unwrap();
    if !fixture.store.destination(&target).unwrap().exists() {
        return;
    }

    fixture.store.rename(&source, &target).unwrap();

    assert_eq!(
        std::fs::read(fixture.store.prepare_read(&target).unwrap()).unwrap(),
        b"upper"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn case_only_encrypted_directory_rename_does_not_replace_itself() {
    let (fixture, cipher) = Fixture::encrypted();
    let source = fixture.lower.join("case-directory");
    let target = fixture.lower.join("CASE-DIRECTORY");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("child"), b"lower").unwrap();
    if !target.exists() {
        return;
    }

    fixture.store.rename(&source, &target).unwrap();

    let physical_parent = fixture.store.plain_destination(&fixture.lower).unwrap();
    let names = std::fs::read_dir(physical_parent)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(names.contains(target.file_name().unwrap()));
    assert!(!names.contains(source.file_name().unwrap()));
    let mut plaintext = tempfile::tempfile().unwrap();
    cipher
        .decrypt(
            &fixture.store.prepare_read(&target.join("child")).unwrap(),
            &mut plaintext,
        )
        .unwrap();
    let mut contents = Vec::new();
    plaintext.read_to_end(&mut contents).unwrap();
    assert_eq!(contents, b"lower");
}

#[test]
fn whiteout_reconciliation_cleans_an_interrupted_encrypted_unlink() {
    let (fixture, _) = Fixture::encrypted();
    let logical = fixture.lower.join("unlink-recovery");
    std::fs::write(&logical, b"lower").unwrap();
    let destination = fixture.store.prepare_write(&logical, false).unwrap();
    std::fs::write(&destination, b"upper").unwrap();
    let lease = OverlayStore::write_lease_path(&destination).unwrap();
    std::fs::write(&lease, destination.as_os_str().as_bytes()).unwrap();
    fixture
        .store
        .metadata
        .ensure_encrypted_name(&logical)
        .unwrap();
    fixture
        .store
        .metadata
        .set_with_attributes(&logical, EntryState::Whiteout, None)
        .unwrap();

    assert_eq!(
        fixture.store.reconcile_entry_locked(&logical).unwrap(),
        Some(EntryState::Whiteout)
    );

    assert!(!destination.exists());
    assert!(!lease.exists());
    assert!(fixture.store.prepare_read(&logical).is_err());
}

#[test]
fn rename_preserves_sources_and_destinations_when_posix_checks_fail() {
    let fixture = Fixture::new();
    let file = fixture.lower.join("file");
    let directory = fixture.lower.join("directory");
    let child = directory.join("child");
    let other_directory = fixture.lower.join("other-directory");
    std::fs::write(&file, b"file").unwrap();
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(&child, b"child").unwrap();
    std::fs::create_dir_all(&other_directory).unwrap();

    fixture.store.rename(&file, &file).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"file");
    assert_eq!(fixture.store.state(&file).unwrap(), None);

    let error = fixture.store.rename(&file, &directory).unwrap_err();
    assert_eq!(errno(&error), Some(libc::EISDIR));
    assert_eq!(std::fs::read(&file).unwrap(), b"file");
    assert_eq!(std::fs::read(&child).unwrap(), b"child");
    assert_eq!(fixture.store.state(&file).unwrap(), None);
    assert!(!fixture.store.destination(&file).unwrap().exists());

    let error = fixture.store.rename(&directory, &file).unwrap_err();
    assert_eq!(errno(&error), Some(libc::ENOTDIR));
    assert_eq!(std::fs::read(&file).unwrap(), b"file");
    assert_eq!(std::fs::read(&child).unwrap(), b"child");
    assert_eq!(fixture.store.state(&directory).unwrap(), None);
    assert!(!fixture.store.destination(&directory).unwrap().exists());

    let error = fixture
        .store
        .rename(&other_directory, &directory)
        .unwrap_err();
    assert_eq!(errno(&error), Some(libc::ENOTEMPTY));
    assert_eq!(std::fs::read(&child).unwrap(), b"child");
    assert_eq!(fixture.store.state(&other_directory).unwrap(), None);
    assert!(
        !fixture
            .store
            .destination(&other_directory)
            .unwrap()
            .exists()
    );

    let error = fixture
        .store
        .rename(&directory, &directory.join("nested"))
        .unwrap_err();
    assert_eq!(errno(&error), Some(libc::EINVAL));
    assert_eq!(std::fs::read(&child).unwrap(), b"child");
    assert_eq!(fixture.store.state(&directory).unwrap(), None);
    assert!(!fixture.store.destination(&directory).unwrap().exists());
}

#[test]
fn cached_copy_up_refreshes_from_lower_before_a_later_write() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("source");
    std::fs::write(&source, b"first").unwrap();
    let staged = fixture.store.stage_write(&source, false).unwrap();
    assert!(matches!(
        fixture.store.state(&source).unwrap(),
        Some(EntryState::Cached { .. })
    ));
    drop(staged);

    std::fs::write(&source, b"second").unwrap();
    let staged = fixture.store.stage_write(&source, false).unwrap();
    assert_eq!(std::fs::read(staged.destination()).unwrap(), b"second");
    assert_eq!(
        fixture.store.visible_path(&source).unwrap(),
        source.canonicalize().unwrap()
    );
}

#[test]
fn removing_a_cached_lower_file_clears_its_staged_copy() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("removed-cached-lower");
    std::fs::write(&logical, b"lower").unwrap();
    let staged = fixture.store.stage_write(&logical, false).unwrap();
    let destination = staged.destination().to_path_buf();
    drop(staged);
    std::fs::remove_file(&logical).unwrap();

    assert!(fixture.store.prepare_read(&logical).is_err());
    assert!(!destination.exists());
    assert_eq!(fixture.store.state_for_test(&logical).unwrap(), None);
}

#[test]
fn creating_after_a_cached_lower_file_disappears_reuses_no_stale_data() {
    let fixture = Fixture::new();
    let logical = fixture.lower.join("recreated-cached-lower");
    std::fs::write(&logical, b"lower").unwrap();
    let staged = fixture.store.stage_write(&logical, false).unwrap();
    let old_destination = staged.destination().to_path_buf();
    drop(staged);
    std::fs::remove_file(&logical).unwrap();

    let staged = fixture.store.stage_write(&logical, true).unwrap();
    assert!(!old_destination.exists());
    std::fs::write(staged.destination(), b"new").unwrap();
    fixture.store.commit_write(staged).unwrap();
    assert_eq!(
        std::fs::read(fixture.store.prepare_read(&logical).unwrap()).unwrap(),
        b"new"
    );
}

#[test]
fn symlink_resolution_reports_a_cycle_after_the_bounded_walk() {
    let fixture = Fixture::new();
    let first = fixture.lower.join("first-link");
    let second = fixture.lower.join("second-link");
    symlink("second-link", &first).unwrap();
    symlink("first-link", &second).unwrap();

    assert_eq!(
        errno(&fixture.store.resolve_final(&first, false).unwrap_err()),
        Some(libc::ELOOP)
    );
}

#[test]
fn whiteouts_and_existing_entries_reject_directory_and_symlink_creation() {
    let fixture = Fixture::new();
    let existing = fixture.lower.join("existing");
    std::fs::write(&existing, b"existing").unwrap();
    assert_eq!(
        errno(
            &fixture
                .store
                .transaction(|transaction| {
                    transaction.create_symlink(&existing, Path::new("target"))
                })
                .unwrap_err()
        ),
        Some(libc::EEXIST)
    );

    let whiteout = fixture.lower.join("whiteout-directory");
    fixture
        .store
        .set_state_for_test(&whiteout, EntryState::Whiteout)
        .unwrap();
    let error = fixture
        .store
        .with_lock(|| fixture.store.ensure_directory_locked(&whiteout))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
    let error = match fixture.store.directory_view(&whiteout) {
        Ok(_) => panic!("whiteout unexpectedly produced a directory view"),
        Err(error) => error,
    };
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );

    let missing = fixture.lower.join("missing-directory");
    let error = fixture
        .store
        .with_lock(|| fixture.store.ensure_directory_locked(&missing))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn directory_rename_preserves_lower_symlinks() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("source");
    let target = fixture.lower.join("target");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file"), b"contents").unwrap();
    symlink("file", source.join("link")).unwrap();

    fixture.store.rename(&source, &target).unwrap();

    let mapped = fixture.store.prepare_directory(&target).unwrap();
    let link = fixture.store.prepare_read(&target.join("link")).unwrap();
    assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("file"));
    assert_eq!(std::fs::read(link).unwrap(), b"contents");
    assert!(mapped.is_dir());
}

#[test]
fn paths_are_normalized_and_logical_control_names_are_isolated() {
    let fixture = Fixture::new();
    assert!(fixture.store.prepare_read(Path::new("relative")).is_err());
    assert_eq!(
        fixture.store.normalize(Path::new("/tmp/a/../b")).unwrap(),
        Path::new("/tmp/b")
    );
    let logical = Path::new("/.metadata");
    let mapped = fixture.store.prepare_write(logical, true).unwrap();
    assert_ne!(mapped, fixture.store.root().join(".metadata"));
    assert_eq!(fixture.store.logical_path(&mapped).unwrap(), logical);
}

#[test]
fn internal_paths_bypass_overlay_state_and_control_aliases_round_trip() {
    let fixture = Fixture::new();
    let internal_file = fixture.store.root().join("internal");
    std::fs::write(&internal_file, b"internal").unwrap();
    assert_eq!(
        fixture.store.prepare_read(&internal_file).unwrap(),
        internal_file
    );
    let staged = fixture.store.stage_write(&internal_file, false).unwrap();
    assert_eq!(staged.destination(), internal_file);
    fixture.store.commit_write(staged).unwrap();
    assert_eq!(
        fixture
            .store
            .prepare_directory(fixture.store.root())
            .unwrap(),
        fixture.store.root()
    );

    let logical_control = Path::new("/.metadata");
    let encoded = fixture.store.prepare_write(logical_control, true).unwrap();
    std::fs::write(&encoded, b"logical metadata").unwrap();
    let view = fixture.store.directory_view(Path::new("/")).unwrap();
    assert_eq!(
        view.aliases().get(encoded.file_name().unwrap()),
        Some(&std::ffi::OsString::from(".metadata"))
    );
}

#[test]
fn checksum_matches_standard_md5_and_executable_publication_is_reused() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("tool");
    std::fs::write(&source, b"").unwrap();
    assert_eq!(
        OverlayStore::checksum(&source).unwrap(),
        "d41d8cd98f00b204e9800998ecf8427e"
    );

    let mut preparations = 0;
    let destination = fixture
        .store
        .prepare_executable(&source, |temporary| {
            preparations += 1;
            std::fs::write(temporary, b"prepared")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();
    fixture
        .store
        .prepare_executable(&source, |_| {
            preparations += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(preparations, 1);
    assert_eq!(std::fs::read(destination).unwrap(), b"prepared");
}

#[test]
fn loader_images_are_plain_cached_and_rebuilt_after_destination_changes() {
    let fixture = Fixture::new();
    let source = fixture
        .lower
        .join("Fixture.app/Contents/Frameworks/libfixture.dylib");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"loader image").unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644)).unwrap();

    let destination = fixture.store.prepare_loader_image(&source).unwrap();
    let first_identity = SourceIdentity::from_metadata(&destination.symlink_metadata().unwrap());
    let reused = fixture.store.prepare_loader_image(&source).unwrap();

    assert_eq!(destination, reused);
    assert_eq!(
        SourceIdentity::from_metadata(&reused.symlink_metadata().unwrap()),
        first_identity
    );
    assert_eq!(std::fs::read(&destination).unwrap(), b"loader image");
    assert_eq!(fixture.store.prepare_read(&source).unwrap(), source);
    assert!(matches!(
        fixture.store.state(&source).unwrap(),
        Some(EntryState::Cached {
            materializer: Materializer::Loader,
            ..
        })
    ));

    std::fs::write(&destination, b"changed destination").unwrap();
    fixture.store.prepare_loader_image(&source).unwrap();

    assert_eq!(std::fs::read(&destination).unwrap(), b"loader image");

    std::fs::write(&source, b"updated loader image").unwrap();
    fixture.store.prepare_loader_image(&source).unwrap();

    assert_eq!(std::fs::read(destination).unwrap(), b"updated loader image");
}

#[test]
fn loader_image_cache_rebuilds_an_unversioned_preparation() {
    let fixture = Fixture::new();
    let source = fixture
        .lower
        .join("Fixture.app/Contents/Frameworks/liblegacy.dylib");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"loader image").unwrap();

    let destination = fixture.store.prepare_loader_image(&source).unwrap();
    let previous = SourceIdentity::from_metadata(&destination.symlink_metadata().unwrap());
    let state = fixture.store.state_for_test(&source).unwrap().unwrap();
    let mut stored = serde_json::to_value(state).unwrap();
    stored["variant"] = serde_json::Value::Null;
    fixture
        .store
        .set_state_for_test(&source, serde_json::from_value(stored).unwrap())
        .unwrap();

    fixture.store.prepare_loader_image(&source).unwrap();

    assert_ne!(
        SourceIdentity::from_metadata(&destination.symlink_metadata().unwrap()),
        previous
    );
    assert_eq!(std::fs::read(destination).unwrap(), b"loader image");
}

#[test]
fn loader_image_preparation_never_replaces_sandbox_cow_content() {
    let fixture = Fixture::new();
    let source = fixture
        .lower
        .join("Fixture.app/Contents/Frameworks/libfixture.dylib");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"lower loader").unwrap();
    let upper = fixture.store.prepare_write(&source, false).unwrap();
    std::fs::write(&upper, b"sandbox loader").unwrap();

    let error = fixture.store.prepare_loader_image(&source).unwrap_err();

    assert_eq!(std::fs::read(&upper).unwrap(), b"sandbox loader");
    assert!(matches!(
        fixture.store.state(&source).unwrap(),
        Some(EntryState::Cow)
    ));
    assert!(error.to_string().contains("sandbox-modified loader image"));
}

#[test]
fn loader_image_preparation_respects_sandbox_whiteouts() {
    let fixture = Fixture::new();
    let source = fixture
        .lower
        .join("Fixture.app/Contents/Frameworks/libfixture.dylib");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"lower loader").unwrap();
    fixture.store.remove(&source, false).unwrap();

    let error = fixture.store.prepare_loader_image(&source).unwrap_err();

    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind),
        Some(std::io::ErrorKind::NotFound)
    );
    assert!(matches!(
        fixture.store.state(&source).unwrap(),
        Some(EntryState::Whiteout)
    ));
}

#[test]
fn loader_trees_are_clean_cached_wrappers_and_preserve_symlinks() {
    let fixture = Fixture::new();
    let source = fixture
        .lower
        .join("Fixture.app/Contents/Frameworks/Signed.framework/Versions/A");
    let resources = source.join("Resources");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(source.join("Signed"), b"loader").unwrap();
    std::fs::write(resources.join("Info.plist"), b"plist").unwrap();
    symlink("Resources", source.join("CurrentResources")).unwrap();

    let destination = fixture.store.prepare_loader_tree(&source).unwrap();
    let first_identity = SourceIdentity::from_metadata(&destination.symlink_metadata().unwrap());

    assert_eq!(
        std::fs::read(destination.join("Signed")).unwrap(),
        b"loader"
    );
    assert_eq!(
        std::fs::read_link(destination.join("CurrentResources")).unwrap(),
        Path::new("Resources")
    );
    assert!(!destination.join(".metadata").exists());
    assert!(!destination.join("Resources/.metadata").exists());
    assert_eq!(
        fixture.store.prepare_read(&source.join("Signed")).unwrap(),
        source.join("Signed")
    );
    assert!(destination.join("Signed").exists());
    assert!(
        fixture
            .store
            .directory_view(&source)
            .unwrap()
            .is_passthrough()
    );
    assert!(matches!(
        fixture.store.state(&source).unwrap(),
        Some(EntryState::Cached {
            materializer: Materializer::LoaderTree,
            ..
        })
    ));

    let reused = fixture.store.prepare_loader_tree(&source).unwrap();
    assert_eq!(
        SourceIdentity::from_metadata(&reused.symlink_metadata().unwrap()),
        first_identity
    );

    std::fs::write(destination.join("Resources/Info.plist"), b"tampered").unwrap();
    fixture.store.prepare_loader_tree(&source).unwrap();
    assert_eq!(
        std::fs::read(destination.join("Resources/Info.plist")).unwrap(),
        b"plist"
    );

    std::fs::write(resources.join("Info.plist"), b"updated plist").unwrap();
    fixture.store.prepare_loader_tree(&source).unwrap();
    assert_eq!(
        std::fs::read(destination.join("Resources/Info.plist")).unwrap(),
        b"updated plist"
    );
}

#[test]
fn loader_tree_migrates_file_caches_but_never_replaces_cow_descendants() {
    let fixture = Fixture::new();
    let source = fixture
        .lower
        .join("Fixture.app/Contents/Frameworks/Signed.framework/Versions/A");
    std::fs::create_dir_all(&source).unwrap();
    let image = source.join("Signed");
    std::fs::write(&image, b"loader").unwrap();

    fixture.store.prepare_loader_image(&image).unwrap();
    assert!(
        fixture
            .store
            .root()
            .join(source.strip_prefix(Path::new("/")).unwrap())
            .join(".metadata")
            .exists()
    );
    let destination = fixture.store.prepare_loader_tree(&source).unwrap();
    assert!(!destination.join(".metadata").exists());

    let upper = fixture.store.prepare_write(&image, false).unwrap();
    std::fs::write(&upper, b"sandbox loader").unwrap();
    assert!(fixture.store.state(&source).unwrap().is_none());
    assert!(matches!(
        fixture.store.state(&image).unwrap(),
        Some(EntryState::Cow)
    ));

    let error = fixture.store.prepare_loader_tree(&source).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("sandbox-modified framework loader tree")
    );
    assert_eq!(std::fs::read(upper).unwrap(), b"sandbox loader");
}

#[test]
fn encrypted_loader_trees_keep_signed_contents_plain_and_control_files_outside() {
    let (fixture, _cipher) = Fixture::encrypted();
    let source = fixture
        .lower
        .join("Fixture.app/Contents/Frameworks/Signed.framework/Versions/A");
    std::fs::create_dir_all(source.join("Resources")).unwrap();
    std::fs::write(source.join("Signed"), b"loader").unwrap();
    std::fs::write(source.join("Resources/Info.plist"), b"plist").unwrap();

    let destination = fixture.store.prepare_loader_tree(&source).unwrap();

    assert_eq!(
        std::fs::read(destination.join("Signed")).unwrap(),
        b"loader"
    );
    assert!(!destination.join(".metadata").exists());
    assert!(!destination.join("Resources/.metadata").exists());
    assert_eq!(
        fixture.store.prepare_read(&source.join("Signed")).unwrap(),
        source.join("Signed")
    );
    assert!(destination.join("Signed").exists());
}

#[test]
fn loader_trees_preserve_signed_metadata_named_resources() {
    let fixture = Fixture::new();
    let source = fixture
        .lower
        .join("Fixture.app/Contents/Frameworks/Signed.framework/Versions/A");
    let resources = source.join("Resources");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(source.join("Signed"), b"loader").unwrap();
    std::fs::write(resources.join(".metadata"), b"signed resource").unwrap();

    let destination = fixture.store.prepare_loader_tree(&source).unwrap();

    assert_eq!(
        std::fs::read(destination.join("Resources/.metadata")).unwrap(),
        b"signed resource"
    );
    assert_eq!(
        fixture
            .store
            .prepare_read(&resources.join(".metadata"))
            .unwrap(),
        resources.join(".metadata")
    );
    assert!(
        fixture
            .store
            .directory_view(&resources)
            .unwrap()
            .is_passthrough()
    );
    assert!(destination.join("Resources/.metadata").exists());
}

#[test]
fn executable_publication_skips_checksum_when_the_destination_identity_is_unchanged() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("large-tool");
    std::fs::write(&source, b"source").unwrap();
    let mut preparations = 0;
    let destination = fixture
        .store
        .prepare_executable(&source, |temporary| {
            preparations += 1;
            std::fs::write(temporary, b"prepared")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();

    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o111)).unwrap();
    let state = fixture.store.state_for_test(&source).unwrap().unwrap();
    let EntryState::Cached {
        checksum,
        materializer,
        source: cached_source,
        variant,
        destination: _,
    } = state
    else {
        panic!("unexpected executable state");
    };
    fixture
        .store
        .set_state_for_test(
            &source,
            EntryState::Cached {
                checksum,
                materializer,
                source: cached_source,
                variant,
                destination: Some(SourceIdentity::from_metadata(
                    &destination.symlink_metadata().unwrap(),
                )),
            },
        )
        .unwrap();

    fixture
        .store
        .prepare_executable(&source, |_| {
            preparations += 1;
            Ok(())
        })
        .unwrap();

    assert_eq!(preparations, 1);
    assert_eq!(std::fs::read(&source).unwrap(), b"source");
}

#[test]
fn directory_rename_materializes_the_visible_tree_without_changing_the_lower_tree() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("source-directory");
    let nested = source.join("nested");
    let target = fixture.lower.join("target-directory");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(source.join("root-file"), b"root").unwrap();
    std::fs::write(nested.join("nested-file"), b"nested").unwrap();
    std::fs::write(source.join("removed"), b"removed").unwrap();
    fixture
        .store
        .remove(&source.join("removed"), false)
        .unwrap();

    fixture.store.rename(&source, &target).unwrap();

    assert!(fixture.store.prepare_read(&source).is_err());
    let mapped = fixture.store.prepare_read(&target).unwrap();
    assert_eq!(
        std::fs::read(
            fixture
                .store
                .prepare_read(&target.join("root-file"))
                .unwrap()
        )
        .unwrap(),
        b"root"
    );
    assert_eq!(
        std::fs::read(
            fixture
                .store
                .prepare_read(&target.join("nested/nested-file"))
                .unwrap()
        )
        .unwrap(),
        b"nested"
    );
    assert!(fixture.store.prepare_read(&target.join("removed")).is_err());
    assert!(mapped.is_dir());
    assert_eq!(std::fs::read(source.join("root-file")).unwrap(), b"root");
    assert_eq!(
        std::fs::read(nested.join("nested-file")).unwrap(),
        b"nested"
    );
    assert!(!target.exists());
}

#[test]
fn encrypted_directory_rename_preserves_nested_files() {
    let (fixture, cipher) = Fixture::encrypted();
    let source = fixture.lower.join("source-directory");
    let nested = source.join("nested");
    let target = fixture.lower.join("target-directory");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(source.join("root-file"), b"root").unwrap();
    std::fs::write(nested.join("nested-file"), b"nested").unwrap();

    fixture.store.rename(&source, &target).unwrap();

    let mut root_plaintext = tempfile::tempfile().unwrap();
    cipher
        .decrypt(
            &fixture
                .store
                .prepare_read(&target.join("root-file"))
                .unwrap(),
            &mut root_plaintext,
        )
        .unwrap();
    let mut nested_plaintext = tempfile::tempfile().unwrap();
    cipher
        .decrypt(
            &fixture
                .store
                .prepare_read(&target.join("nested/nested-file"))
                .unwrap(),
            &mut nested_plaintext,
        )
        .unwrap();

    let mut root_contents = String::new();
    root_plaintext.read_to_string(&mut root_contents).unwrap();
    let mut nested_contents = String::new();
    nested_plaintext
        .read_to_string(&mut nested_contents)
        .unwrap();

    assert_eq!(root_contents, "root");
    assert_eq!(nested_contents, "nested");
    assert_eq!(std::fs::read(source.join("root-file")).unwrap(), b"root");
    assert_eq!(
        std::fs::read(nested.join("nested-file")).unwrap(),
        b"nested"
    );
    assert!(!target.exists());
}

#[test]
fn overlay_handles_missing_cached_entries_cow_ancestors_and_type_errors() {
    let fixture = Fixture::new();
    let cached = fixture.lower.join("cached");
    std::fs::write(&cached, b"host").unwrap();
    let mapped = fixture
        .store
        .prepare_executable(&cached, |temporary| {
            std::fs::write(temporary, b"prepared")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();
    std::fs::remove_file(&mapped).unwrap();
    assert_eq!(
        fixture.store.visible_path(&cached).unwrap(),
        cached.canonicalize().unwrap()
    );
    std::fs::remove_file(&cached).unwrap();
    assert!(fixture.store.prepare_read(&cached).is_err());
    assert_eq!(fixture.store.state(&cached).unwrap(), None);

    let cow_directory = fixture.lower.join("cow-directory");
    let mapped_directory = fixture
        .store
        .create_directory(&cow_directory, 0o700)
        .unwrap();
    assert!(
        fixture
            .store
            .create_directory(&cow_directory, 0o700)
            .is_err()
    );
    let child = cow_directory.join("child");
    assert!(fixture.store.prepare_write(&child, false).is_err());
    let mapped_child = fixture.store.prepare_write(&child, true).unwrap();
    std::fs::write(&mapped_child, b"child").unwrap();
    assert_eq!(fixture.store.prepare_read(&child).unwrap(), mapped_child);
    assert_eq!(
        fixture.store.prepare_directory(&cow_directory).unwrap(),
        mapped_directory
    );
    assert!(
        fixture
            .store
            .directory_view(&cow_directory)
            .unwrap()
            .lower()
            .is_none()
    );

    let host_file = fixture.lower.join("host-file");
    let host_directory = fixture.lower.join("host-directory");
    std::fs::write(&host_file, b"file").unwrap();
    std::fs::create_dir(&host_directory).unwrap();
    assert!(fixture.store.remove(&host_file, true).is_err());
    assert!(fixture.store.remove(&host_directory, false).is_err());
    assert!(
        fixture
            .store
            .remove(&fixture.lower.join("missing"), false)
            .is_err()
    );
    assert!(fixture.store.prepare_directory(&host_file).is_err());

    let internal = fixture.store.root().join(".vfs.lock");
    assert_eq!(
        fixture.store.logical_path(&internal).unwrap(),
        Path::new("/.vfs.lock")
    );
    assert!(
        fixture
            .store
            .directory_view(Path::new("/"))
            .unwrap()
            .hidden()
            .contains(std::ffi::OsStr::new(".vfs.lock"))
    );
}

#[test]
fn executable_metadata_and_failed_publication_are_consistent() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("tool");
    std::fs::write(&source, b"tool").unwrap();
    fixture
        .store
        .prepare_executable(&source, |temporary| {
            std::fs::write(temporary, b"prepared")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();

    fixture.store.mark_executable(&source).unwrap();
    assert!(matches!(
        fixture.store.state(&source).unwrap(),
        Some(EntryState::Cached {
            materializer: Materializer::Executable,
            ..
        })
    ));
    fixture
        .store
        .mark_executable(&fixture.lower.join("missing"))
        .unwrap();

    std::fs::write(&source, b"changed tool").unwrap();
    let failed = fixture.store.prepare_executable(&source, |temporary| {
        std::fs::write(temporary, b"partial")?;
        anyhow::bail!("preparation failed")
    });
    assert!(
        failed
            .unwrap_err()
            .to_string()
            .contains("preparation failed")
    );
    let parent = fixture
        .store
        .destination(&source)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    assert!(std::fs::read_dir(parent).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".agora-executable-")
    }));
}

#[test]
fn executable_cache_rebuilds_when_the_recorded_checksum_is_wrong() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("large-tool");
    std::fs::write(&source, b"source executable").unwrap();
    let destination = fixture
        .store
        .prepare_executable(&source, |temporary| {
            std::fs::write(temporary, b"prepared executable")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();
    let Some(EntryState::Cached {
        materializer,
        source: Some(source_identity),
        ..
    }) = fixture.store.state(&source).unwrap()
    else {
        panic!("missing executable source identity");
    };
    fixture
        .store
        .set_state_for_test(
            &source,
            EntryState::Cached {
                checksum: Some("intentionally-invalid".to_string()),
                materializer,
                source: Some(source_identity),
                variant: Some(format!(
                    "{}/{}/prepare-v2",
                    std::env::consts::OS,
                    std::env::consts::ARCH
                )),
                destination: None,
            },
        )
        .unwrap();

    let rebuilt = fixture.store.prepare_executable(&source, |temporary| {
        std::fs::write(temporary, b"rebuilt executable")?;
        std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    });

    assert_eq!(rebuilt.unwrap(), destination);
    assert_eq!(std::fs::read(destination).unwrap(), b"rebuilt executable");
}

#[test]
fn executable_cache_rebuilds_when_the_destination_content_changes() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("tampered-tool");
    std::fs::write(&source, b"source executable").unwrap();
    let destination = fixture
        .store
        .prepare_executable(&source, |temporary| {
            std::fs::write(temporary, b"prepared executable")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();
    std::fs::write(&destination, b"tampered executable").unwrap();
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut preparations = 0;
    fixture
        .store
        .prepare_executable(&source, |temporary| {
            preparations += 1;
            std::fs::write(temporary, b"rebuilt executable")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();

    assert_eq!(preparations, 1);
    assert_eq!(std::fs::read(destination).unwrap(), b"rebuilt executable");
}

#[test]
fn executable_cache_rebuilds_when_the_target_variant_is_wrong() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("variant-tool");
    std::fs::write(&source, b"source executable").unwrap();
    let destination = fixture
        .store
        .prepare_executable(&source, |temporary| {
            std::fs::write(temporary, b"first executable")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();
    let state = fixture.store.state(&source).unwrap().unwrap();
    let mut stored = serde_json::to_value(state).unwrap();
    stored["variant"] = serde_json::Value::String("different-target".to_string());
    fixture
        .store
        .set_state_for_test(&source, serde_json::from_value(stored).unwrap())
        .unwrap();

    fixture
        .store
        .prepare_executable(&source, |temporary| {
            std::fs::write(temporary, b"target executable")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();

    assert_eq!(std::fs::read(destination).unwrap(), b"target executable");
}

#[test]
fn executable_cache_rebuilds_a_legacy_unversioned_target_variant() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("legacy-variant-tool");
    std::fs::write(&source, b"source executable").unwrap();
    let mut preparations = 0;
    let destination = fixture
        .store
        .prepare_executable(&source, |temporary| {
            preparations += 1;
            std::fs::write(temporary, b"legacy executable")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();
    let state = fixture.store.state_for_test(&source).unwrap().unwrap();
    let mut stored = serde_json::to_value(state).unwrap();
    stored["variant"] = serde_json::Value::String(format!(
        "{}/{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    fixture
        .store
        .set_state_for_test(&source, serde_json::from_value(stored).unwrap())
        .unwrap();

    fixture
        .store
        .prepare_executable(&source, |temporary| {
            preparations += 1;
            std::fs::write(temporary, b"current executable")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();

    assert_eq!(preparations, 2);
    assert_eq!(std::fs::read(destination).unwrap(), b"current executable");
}

#[test]
fn executable_cache_rebuilds_when_the_destination_is_a_symlink() {
    let fixture = Fixture::new();
    let source = fixture.lower.join("symlink-tool");
    std::fs::write(&source, b"source executable").unwrap();
    let destination = fixture
        .store
        .prepare_executable(&source, |temporary| {
            std::fs::write(temporary, b"prepared executable")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();
    let external = fixture.lower.join("external-cache-target");
    std::fs::write(&external, b"prepared executable").unwrap();
    std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_file(&destination).unwrap();
    symlink(&external, &destination).unwrap();

    fixture
        .store
        .prepare_executable(&source, |temporary| {
            std::fs::write(temporary, b"rebuilt executable")?;
            std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })
        .unwrap();

    assert!(destination.symlink_metadata().unwrap().is_file());
    assert_eq!(std::fs::read(destination).unwrap(), b"rebuilt executable");
}

#[test]
fn visible_symlinks_follow_overlay_state_of_their_canonical_target() {
    let fixture = Fixture::new();
    let target = fixture.lower.join("target");
    let link = fixture.lower.join("link");
    std::fs::write(&target, b"host").unwrap();
    symlink(&target, &link).unwrap();
    let target = target.canonicalize().unwrap();

    assert_eq!(fixture.store.prepare_read(&target).unwrap(), target);
    assert_eq!(fixture.store.prepare_read(&link).unwrap(), link);
    assert_eq!(fixture.store.visible_path(&link).unwrap(), target);

    let cow = fixture.store.prepare_write(&target, false).unwrap();
    std::fs::write(&cow, b"sandbox").unwrap();
    assert_eq!(fixture.store.visible_path(&link).unwrap(), cow);

    fixture.store.remove(&target, false).unwrap();
    assert!(fixture.store.visible_path(&link).is_err());
}

#[test]
fn special_files_root_children_and_upper_directories_remain_overlay_local() {
    let fixture = Fixture::new();
    let fifo = fixture.lower.join("fifo");
    let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
    assert_eq!(fixture.store.prepare_read(&fifo).unwrap(), fifo);

    let host_directory = fixture.lower.join("host-directory");
    std::fs::create_dir(&host_directory).unwrap();
    assert_eq!(
        fixture.store.prepare_write(&host_directory, false).unwrap(),
        host_directory
    );

    let root_child = Path::new("/").join(format!("agora-overlay-{}", uuid::Uuid::new_v4()));
    let mapped_root_child = fixture.store.prepare_write(&root_child, true).unwrap();
    assert_eq!(
        mapped_root_child,
        fixture
            .store
            .root()
            .join(root_child.strip_prefix("/").unwrap())
    );
    assert!(!root_child.exists());

    let directory = fixture.lower.join("upper-directory");
    fixture.store.create_directory(&directory, 0o700).unwrap();
    let child = fixture
        .store
        .prepare_write(&directory.join("child"), true)
        .unwrap();
    std::fs::write(child, b"child").unwrap();
    let error = fixture.store.remove(&directory, true).unwrap_err();
    assert_eq!(
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .and_then(std::io::Error::raw_os_error),
        Some(libc::ENOTEMPTY)
    );
    assert!(!directory.exists());
}

#[test]
fn missing_cow_falls_back_while_whiteout_and_missing_paths_remain_unavailable() {
    let fixture = Fixture::new();

    let cow_file = fixture.lower.join("cow-file");
    std::fs::write(&cow_file, b"host").unwrap();
    let mapped_cow_file = fixture.store.prepare_write(&cow_file, false).unwrap();
    std::fs::remove_file(mapped_cow_file).unwrap();
    let recreated_cow_file = fixture.store.prepare_write(&cow_file, false).unwrap();
    assert_eq!(std::fs::read(recreated_cow_file).unwrap(), b"host");

    let removed_file = fixture.lower.join("removed-file");
    std::fs::write(&removed_file, b"host").unwrap();
    fixture.store.remove(&removed_file, false).unwrap();
    assert!(fixture.store.prepare_write(&removed_file, false).is_err());
    assert!(fixture.store.remove(&removed_file, false).is_err());

    let missing = fixture.lower.join("missing");
    assert!(fixture.store.prepare_read(&missing).is_err());
    assert!(fixture.store.prepare_write(&missing, false).is_err());

    let removed_directory = fixture.lower.join("removed-directory");
    std::fs::create_dir(&removed_directory).unwrap();
    fixture.store.remove(&removed_directory, true).unwrap();
    assert!(fixture.store.prepare_directory(&removed_directory).is_err());

    let cow_directory = fixture.lower.join("cow-directory");
    fixture
        .store
        .create_directory(&cow_directory, 0o700)
        .unwrap();
    assert!(
        fixture
            .store
            .visible_path(&cow_directory.join("missing-child"))
            .is_err()
    );
}
