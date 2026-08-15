use super::{AccessPlan, OpenIntent, OpenTarget, VirtualFilesystem};
use crate::filesystem::crypto::FileCipher;
use crate::filesystem::{AccessRequest, Credentials, FileAttributes};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

fn errno(error: &anyhow::Error) -> Option<i32> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .and_then(std::io::Error::raw_os_error)
}

fn fixture(name: &str) -> (std::path::PathBuf, VirtualFilesystem) {
    let root = std::env::temp_dir().join(format!("agora-vfs-{name}-{}", uuid::Uuid::new_v4()));
    let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
    let filesystem = VirtualFilesystem::encrypted(&root, cipher).unwrap();
    (root, filesystem)
}

fn attributes_with_mode(path: &Path, mode: u32) -> FileAttributes {
    let mut attributes = FileAttributes::from_metadata(&path.symlink_metadata().unwrap());
    attributes.mode = (attributes.mode & u32::from(libc::S_IFMT)) | mode;
    attributes
}

#[test]
fn authorized_open_denies_before_staging_in_one_transaction() {
    let (root, filesystem) = fixture("authorized-open-denied");
    let logical = root.parent().unwrap().join(format!(
        "agora-vfs-authorized-open-denied-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&logical, b"lower").unwrap();
    let mut attributes = FileAttributes::from_metadata(&logical.metadata().unwrap());
    attributes.mode = u32::from(libc::S_IFREG) | 0o444;
    filesystem.set_attributes(&logical, attributes).unwrap();
    let before = filesystem.transaction_count_for_test();

    let error = filesystem
        .prepare_authorized_open(
            &logical,
            OpenIntent::new(libc::O_WRONLY | libc::O_TRUNC, 0o666).unwrap(),
            &Credentials::effective(),
        )
        .err()
        .expect("write open should be denied");

    assert_eq!(errno(&error), Some(libc::EACCES));
    assert_eq!(filesystem.transaction_count_for_test() - before, 1);
    assert_eq!(filesystem.state_for_test(&logical).unwrap(), None);
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_file(logical).unwrap();
}

#[test]
fn authorized_open_stages_from_the_authorized_transaction() {
    let (root, filesystem) = fixture("authorized-open-staged");
    let logical = root.parent().unwrap().join(format!(
        "agora-vfs-authorized-open-staged-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&logical, b"lower").unwrap();
    let before = filesystem.transaction_count_for_test();

    let plan = filesystem
        .prepare_authorized_open(
            &logical,
            OpenIntent::new(libc::O_WRONLY, 0o666).unwrap(),
            &Credentials::effective(),
        )
        .unwrap();

    assert_eq!(plan.logical(), logical);
    assert_eq!(filesystem.transaction_count_for_test() - before, 1);
    let (_logical, _prepared) = plan.into_parts();
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_file(logical).unwrap();
}

#[test]
fn authorized_open_resolves_an_existing_endpoint_once() {
    let (root, filesystem) = fixture("authorized-open-single-resolution");
    let logical = root.parent().unwrap().join(format!(
        "agora-vfs-authorized-open-single-resolution-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&logical, b"lower").unwrap();
    filesystem
        .set_attributes(
            &logical,
            FileAttributes::from_metadata(&logical.metadata().unwrap()),
        )
        .unwrap();
    let before = filesystem.resolution_count_for_test();

    let plan = filesystem
        .prepare_authorized_open(
            &logical,
            OpenIntent::new(libc::O_RDONLY, 0).unwrap(),
            &Credentials::effective(),
        )
        .unwrap();

    let expected = logical.parent().unwrap().ancestors().count() + 1;
    assert_eq!(filesystem.resolution_count_for_test() - before, expected);
    drop(plan);
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_file(logical).unwrap();
}

#[test]
fn change_directory_resolves_the_endpoint_once() {
    let (root, filesystem) = fixture("chdir-single-resolution");
    let logical = root.parent().unwrap().join(format!(
        "agora-vfs-chdir-single-resolution-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&logical).unwrap();
    let before = filesystem.resolution_count_for_test();

    let (mapped, resolved) = filesystem
        .prepare_change_directory(&logical, &Credentials::effective())
        .unwrap();

    assert_eq!(mapped, logical);
    assert_eq!(resolved, logical);
    let expected = logical.parent().unwrap().ancestors().count() + 1;
    assert_eq!(filesystem.resolution_count_for_test() - before, expected);
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(logical).unwrap();
}

#[test]
fn directory_creation_reuses_the_parent_resolution_from_search() {
    let (root, filesystem) = fixture("mkdir-parent-single-resolution");
    let parent = root.parent().unwrap().join(format!(
        "agora-vfs-mkdir-parent-single-resolution-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&parent).unwrap();
    let logical = parent.join("child");
    let before = filesystem.resolution_count_for_test();

    filesystem
        .create_directory_authorized(&logical, 0o755, &Credentials::effective())
        .unwrap();

    let expected = logical.parent().unwrap().ancestors().count();
    assert_eq!(filesystem.resolution_count_for_test() - before, expected);
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(parent).unwrap();
}

#[test]
fn authorized_open_delegates_untouched_lower_permissions_to_native_open() {
    let (root, filesystem) = fixture("authorized-open-native-lower");
    let logical = root.parent().unwrap().join(format!(
        "agora-vfs-authorized-open-native-lower-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&logical, b"lower").unwrap();
    let original_mode = logical.metadata().unwrap().permissions().mode();
    std::fs::set_permissions(&logical, std::fs::Permissions::from_mode(0o000)).unwrap();
    let before = filesystem.transaction_count_for_test();

    let plan = filesystem
        .prepare_authorized_open(
            &logical,
            OpenIntent::new(libc::O_RDONLY, 0).unwrap(),
            &Credentials::effective(),
        )
        .unwrap();

    assert_eq!(filesystem.transaction_count_for_test() - before, 1);
    let (_, prepared) = plan.into_parts();
    assert!(matches!(
        prepared.target(),
        OpenTarget::Path(mapped) if mapped == &logical
    ));
    std::fs::set_permissions(&logical, std::fs::Permissions::from_mode(original_mode)).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_file(logical).unwrap();
}

#[test]
fn authorized_open_preserves_logical_chmod_during_plain_copy_up() {
    let root = std::env::temp_dir().join(format!(
        "agora-vfs-authorized-open-plain-chmod-{}",
        uuid::Uuid::new_v4()
    ));
    let filesystem = VirtualFilesystem::plain(&root).unwrap();
    let logical = root.parent().unwrap().join(format!(
        "agora-vfs-authorized-open-plain-chmod-lower-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&logical, b"lower").unwrap();
    std::fs::set_permissions(&logical, std::fs::Permissions::from_mode(0o444)).unwrap();
    let credentials = Credentials::effective();
    filesystem
        .chmod_authorized(&logical, 0o644, true, &credentials)
        .unwrap();

    let plan = filesystem
        .prepare_authorized_open(
            &logical,
            OpenIntent::new(libc::O_WRONLY, 0).unwrap(),
            &credentials,
        )
        .unwrap();
    let (_, prepared) = plan.into_parts();
    let OpenTarget::Path(mapped) = prepared.target() else {
        panic!("plain copy-up should return an upper path");
    };

    assert_eq!(
        mapped.metadata().unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert_eq!(
        filesystem.attributes(&logical).unwrap().unwrap().mode & 0o777,
        0o644
    );
    std::fs::OpenOptions::new()
        .write(true)
        .open(mapped)
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_file(logical).unwrap();
}

#[test]
fn authorized_mutations_deny_without_publishing_and_use_one_transaction() {
    let (root, filesystem) = fixture("authorized-mutations-denied");
    let parent = root.parent().unwrap().join(format!(
        "agora-vfs-authorized-mutations-denied-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&parent).unwrap();
    filesystem
        .set_attributes(&parent, attributes_with_mode(&parent, 0o555))
        .unwrap();
    let credentials = Credentials::effective();

    let directory = parent.join("directory");
    let before = filesystem.transaction_count_for_test();
    let error = filesystem
        .create_directory_authorized(&directory, 0o755, &credentials)
        .unwrap_err();
    assert_eq!(errno(&error), Some(libc::EACCES));
    assert_eq!(filesystem.transaction_count_for_test() - before, 1);
    assert_eq!(filesystem.state_for_test(&directory).unwrap(), None);

    let link = parent.join("link");
    let before = filesystem.transaction_count_for_test();
    let error = filesystem
        .create_symlink_authorized(&link, Path::new("target"), &credentials)
        .unwrap_err();
    assert_eq!(errno(&error), Some(libc::EACCES));
    assert_eq!(filesystem.transaction_count_for_test() - before, 1);
    assert_eq!(filesystem.state_for_test(&link).unwrap(), None);

    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(parent).unwrap();
}

#[test]
fn authorized_remove_uses_parent_permissions_not_entry_write_permission() {
    let (root, filesystem) = fixture("authorized-remove");
    let parent = root.parent().unwrap().join(format!(
        "agora-vfs-authorized-remove-{}",
        uuid::Uuid::new_v4()
    ));
    let logical = parent.join("read-only");
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(&logical, b"lower").unwrap();
    filesystem
        .set_attributes(&logical, attributes_with_mode(&logical, 0o444))
        .unwrap();
    let before = filesystem.transaction_count_for_test();

    filesystem
        .remove_authorized(&logical, false, &Credentials::effective())
        .unwrap();

    assert_eq!(filesystem.transaction_count_for_test() - before, 1);
    assert_eq!(
        filesystem.state_for_test(&logical).unwrap(),
        Some(super::EntryState::Whiteout)
    );
    assert!(logical.is_file());
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(parent).unwrap();
}

#[test]
fn authorized_rename_and_chmod_leave_state_unchanged_on_denial() {
    let (root, filesystem) = fixture("authorized-rename-chmod-denied");
    let parent = root.parent().unwrap().join(format!(
        "agora-vfs-authorized-rename-chmod-denied-{}",
        uuid::Uuid::new_v4()
    ));
    let source_parent = parent.join("source-parent");
    let target_parent = parent.join("target-parent");
    let source = source_parent.join("source");
    let target = target_parent.join("target");
    std::fs::create_dir_all(&source_parent).unwrap();
    std::fs::create_dir(&target_parent).unwrap();
    std::fs::write(&source, b"lower").unwrap();
    filesystem
        .set_attributes(&target_parent, attributes_with_mode(&target_parent, 0o555))
        .unwrap();
    let credentials = Credentials::effective();
    let before = filesystem.transaction_count_for_test();

    let error = filesystem
        .rename_authorized(&source, &target, &credentials)
        .unwrap_err();

    assert_eq!(errno(&error), Some(libc::EACCES));
    assert_eq!(filesystem.transaction_count_for_test() - before, 1);
    assert_eq!(filesystem.state_for_test(&source).unwrap(), None);
    assert_eq!(filesystem.state_for_test(&target).unwrap(), None);
    assert!(source.is_file());
    assert!(!target.exists());

    let mut foreign = attributes_with_mode(&source, 0o644);
    foreign.uid = foreign.uid.wrapping_add(1);
    filesystem.set_attributes(&source, foreign.clone()).unwrap();
    let before = filesystem.transaction_count_for_test();
    let error = filesystem
        .chmod_authorized(&source, 0o600, true, &credentials)
        .unwrap_err();
    assert_eq!(errno(&error), Some(libc::EPERM));
    assert_eq!(filesystem.transaction_count_for_test() - before, 1);
    assert_eq!(filesystem.attributes(&source).unwrap(), Some(foreign));

    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(parent).unwrap();
}

#[test]
fn authorized_query_returns_native_for_untouched_lower_and_denies_logical_mode() {
    let (root, filesystem) = fixture("authorized-query-access");
    let logical = root.parent().unwrap().join(format!(
        "agora-vfs-authorized-query-access-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&logical, b"lower").unwrap();
    let credentials = Credentials::effective();
    let before = filesystem.transaction_count_for_test();

    assert!(matches!(
        filesystem
            .check_access(&logical, true, AccessRequest::READ, &credentials)
            .unwrap(),
        AccessPlan::Native(path) if path == logical
    ));
    assert_eq!(filesystem.transaction_count_for_test() - before, 1);

    filesystem
        .set_attributes(&logical, attributes_with_mode(&logical, 0o000))
        .unwrap();
    let before = filesystem.transaction_count_for_test();
    let error = filesystem
        .check_access(&logical, true, AccessRequest::READ, &credentials)
        .unwrap_err();
    assert_eq!(errno(&error), Some(libc::EACCES));
    assert_eq!(filesystem.transaction_count_for_test() - before, 1);

    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_file(logical).unwrap();
}

#[test]
fn authorized_query_plans_metadata_directory_and_canonical_paths_in_one_transaction() {
    let (root, filesystem) = fixture("authorized-query-plans");
    let parent = root.parent().unwrap().join(format!(
        "agora-vfs-authorized-query-plans-{}",
        uuid::Uuid::new_v4()
    ));
    let logical = parent.join("file");
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(&logical, b"lower").unwrap();
    let credentials = Credentials::effective();

    let before = filesystem.transaction_count_for_test();
    let plan = filesystem
        .prepare_authorized_metadata(&logical, true, &credentials)
        .unwrap();
    assert_eq!(plan.logical(), logical);
    assert_eq!(plan.mapped(), logical);
    assert_eq!(filesystem.transaction_count_for_test() - before, 1);

    let before = filesystem.transaction_count_for_test();
    let canonical = filesystem
        .canonicalize_authorized(&logical, &credentials)
        .unwrap();
    assert_eq!(canonical, logical.canonicalize().unwrap());
    assert_eq!(filesystem.transaction_count_for_test() - before, 1);

    let before = filesystem.transaction_count_for_test();
    let (mapped, resolved) = filesystem
        .prepare_change_directory(&parent, &credentials)
        .unwrap();
    assert_eq!(mapped, parent);
    assert_eq!(resolved, parent);
    assert_eq!(filesystem.transaction_count_for_test() - before, 1);

    let before = filesystem.transaction_count_for_test();
    let view = filesystem
        .directory_view_authorized(&parent, &credentials)
        .unwrap();
    assert_eq!(view.logical(), parent);
    assert_eq!(filesystem.transaction_count_for_test() - before, 1);

    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(parent).unwrap();
}

#[test]
fn encrypted_open_reads_lower_without_entering_the_vfs() {
    let (root, filesystem) = fixture("read");
    let source = root
        .parent()
        .unwrap()
        .join(format!("agora-vfs-source-{}", uuid::Uuid::new_v4()));
    let marker = b"host plaintext marker";
    std::fs::write(&source, marker).unwrap();

    let prepared = filesystem.prepare_open(&source, libc::O_RDONLY, 0).unwrap();
    let OpenTarget::Path(mapped) = prepared.target() else {
        panic!("lower file should remain outside the encrypted VFS");
    };
    assert_eq!(mapped, &source);
    assert_eq!(std::fs::read(mapped).unwrap(), marker);
    let backing = filesystem.prepare_read(&source).unwrap();
    assert_eq!(backing, source);
    assert_eq!(
        filesystem.prepare_metadata(&source, true).unwrap(),
        (source.clone(), None, source.clone())
    );

    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_file(source).unwrap();
}

#[test]
fn native_metadata_passthrough_requires_an_unmodified_lower_path() {
    let (root, filesystem) = fixture("native-metadata-passthrough");
    let source = root.parent().unwrap().join(format!(
        "agora-vfs-native-metadata-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&source, b"lower content").unwrap();

    assert!(
        filesystem
            .native_metadata_passthrough(&source, true, &Credentials::effective())
            .unwrap()
    );
    filesystem.remove(&source, false).unwrap();
    assert!(
        !filesystem
            .native_metadata_passthrough(&source, true, &Credentials::effective())
            .unwrap()
    );

    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_file(source).unwrap();
}

#[test]
fn native_metadata_passthrough_checks_a_lower_symlink_target() {
    use std::os::unix::fs::symlink;

    let (root, filesystem) = fixture("native-metadata-symlink");
    let parent = root.parent().unwrap();
    let target = parent.join(format!("agora-vfs-native-target-{}", uuid::Uuid::new_v4()));
    let link = parent.join(format!("agora-vfs-native-link-{}", uuid::Uuid::new_v4()));
    std::fs::write(&target, b"lower content").unwrap();
    symlink(&target, &link).unwrap();

    assert!(
        filesystem
            .native_metadata_passthrough(&link, true, &Credentials::effective())
            .unwrap()
    );
    filesystem.remove(&target, false).unwrap();
    assert!(
        !filesystem
            .native_metadata_passthrough(&link, true, &Credentials::effective())
            .unwrap()
    );
    assert!(
        filesystem
            .native_metadata_passthrough(&link, false, &Credentials::effective())
            .unwrap()
    );

    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_file(link).unwrap();
    std::fs::remove_file(target).unwrap();
}

#[test]
fn searchable_ancestor_attributes_keep_lower_metadata_on_the_native_fast_path() {
    let (root, filesystem) = fixture("native-metadata-searchable-ancestor");
    let parent = root.parent().unwrap().join(format!(
        "agora-vfs-native-searchable-{}",
        uuid::Uuid::new_v4()
    ));
    let source = parent.join("lower");
    std::fs::create_dir_all(&parent).unwrap();
    std::fs::write(&source, b"lower content").unwrap();
    let mut attributes = FileAttributes::from_metadata(&parent.metadata().unwrap());
    attributes.mode = u32::from(libc::S_IFDIR) | 0o700;
    filesystem
        .set_attributes(&parent, attributes.clone())
        .unwrap();

    assert!(
        filesystem
            .native_metadata_passthrough(&source, true, &Credentials::effective())
            .unwrap()
    );

    attributes.mode = u32::from(libc::S_IFDIR) | 0o600;
    filesystem.set_attributes(&parent, attributes).unwrap();
    assert!(
        !filesystem
            .native_metadata_passthrough(&source, true, &Credentials::effective())
            .unwrap()
    );

    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(parent).unwrap();
}

#[test]
fn encrypted_writes_to_special_files_remain_passthrough() {
    let (root, filesystem) = fixture("special-file-passthrough");
    let logical = Path::new("/dev/null");

    let mut prepared = filesystem.prepare_open(logical, libc::O_WRONLY, 0).unwrap();
    let OpenTarget::Path(mapped) = prepared.target() else {
        panic!("special file should remain a native path");
    };
    assert_eq!(mapped, logical);
    filesystem.commit_open(&mut prepared).unwrap();

    assert_eq!(filesystem.state_for_test(logical).unwrap(), None);
    assert!(filesystem.exists(logical).unwrap());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_writeback_publishes_ciphertext_and_restores_the_next_open() {
    let (root, filesystem) = fixture("write");
    let logical = Path::new("/tmp/agora-vfs-created");
    let marker = b"sandbox plaintext marker";
    let mut prepared = filesystem
        .prepare_open(logical, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
        .unwrap();
    let OpenTarget::Descriptor(file) = prepared.target_mut() else {
        panic!("encrypted regular file did not use an anonymous descriptor");
    };
    file.write_all(marker).unwrap();
    filesystem.commit_open(&mut prepared).unwrap();
    let (target, writeback, _) = prepared.into_parts();
    let OpenTarget::Descriptor(_) = target else {
        panic!("expected descriptor");
    };
    filesystem.commit_writeback(&writeback.unwrap()).unwrap();

    let backing = filesystem.prepare_read(logical).unwrap();
    let stored = std::fs::read(&backing).unwrap();
    assert!(!stored.windows(marker.len()).any(|window| window == marker));

    let mut reopened = filesystem.prepare_open(logical, libc::O_RDONLY, 0).unwrap();
    let OpenTarget::Descriptor(file) = reopened.target_mut() else {
        panic!("encrypted regular file did not use an anonymous descriptor");
    };
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut restored = Vec::new();
    file.read_to_end(&mut restored).unwrap();
    assert_eq!(restored, marker);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn brokered_open_and_metadata_do_not_decrypt_an_existing_file_twice() {
    let (root, filesystem) = fixture("brokered-open");
    let logical = Path::new("/tmp/agora-vfs-brokered-open");
    let marker = b"authenticated plaintext block";
    let mut created = filesystem
        .prepare_open(logical, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
        .unwrap();
    let OpenTarget::Descriptor(file) = created.target_mut() else {
        panic!("encrypted regular file did not use an anonymous descriptor");
    };
    file.write_all(marker).unwrap();
    filesystem.commit_open(&mut created).unwrap();

    let backing = filesystem.prepare_read(logical).unwrap();
    let ciphertext = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&backing)
        .unwrap();
    let offset = crate::filesystem::crypto::CONTENT_HEADER_SIZE as u64 + 12;
    let mut byte = [0_u8; 1];
    ciphertext.read_exact_at(&mut byte, offset).unwrap();
    byte[0] ^= 0xff;
    ciphertext.write_all_at(&byte, offset).unwrap();

    let metadata = filesystem
        .prepare_authorized_metadata(logical, true, &Credentials::effective())
        .unwrap();
    assert_eq!(metadata.into_parts().2, Some(marker.len() as u64));
    let brokered = filesystem
        .prepare_authorized_broker_open(
            logical,
            OpenIntent::new(libc::O_RDONLY, 0).unwrap(),
            &Credentials::effective(),
        )
        .unwrap();
    assert!(matches!(
        brokered.into_parts().1.target(),
        OpenTarget::Descriptor(_)
    ));
    assert!(filesystem.prepare_open(logical, libc::O_RDONLY, 0).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn unchanged_encrypted_write_open_does_not_republish_ciphertext() {
    let (root, filesystem) = fixture("unchanged-write-open");
    let logical = Path::new("/tmp/agora-vfs-unchanged-write-open");
    let mut created = filesystem
        .prepare_open(logical, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
        .unwrap();
    let OpenTarget::Descriptor(file) = created.target_mut() else {
        panic!("encrypted regular file did not use an anonymous descriptor");
    };
    file.write_all(b"unchanged content").unwrap();
    filesystem.commit_open(&mut created).unwrap();
    drop(created);

    let backing = filesystem.prepare_read(logical).unwrap();
    let original_ciphertext = std::fs::read(&backing).unwrap();
    let mut reopened = filesystem.prepare_open(logical, libc::O_RDWR, 0).unwrap();
    filesystem.commit_open(&mut reopened).unwrap();
    assert_eq!(std::fs::read(&backing).unwrap(), original_ciphertext);

    let (_, writeback, _) = reopened.into_parts();
    filesystem.commit_writeback(&writeback.unwrap()).unwrap();
    assert_eq!(std::fs::read(&backing).unwrap(), original_ciphertext);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn unchanged_encrypted_snapshot_does_not_overwrite_a_newer_writer() {
    let (root, filesystem) = fixture("stale-writeback");
    let logical = Path::new("/tmp/agora-vfs-stale-writeback");
    let mut created = filesystem
        .prepare_open(logical, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
        .unwrap();
    let OpenTarget::Descriptor(file) = created.target_mut() else {
        panic!("encrypted regular file did not use an anonymous descriptor");
    };
    file.write_all(b"original content").unwrap();
    filesystem.commit_open(&mut created).unwrap();
    drop(created);

    let mut writer = filesystem.prepare_open(logical, libc::O_RDWR, 0).unwrap();
    filesystem.commit_open(&mut writer).unwrap();
    let mut stale = filesystem.prepare_open(logical, libc::O_RDWR, 0).unwrap();
    filesystem.commit_open(&mut stale).unwrap();

    let OpenTarget::Descriptor(file) = writer.target_mut() else {
        panic!("encrypted regular file did not use an anonymous descriptor");
    };
    file.seek(SeekFrom::Start(0)).unwrap();
    file.set_len(0).unwrap();
    file.write_all(b"newer content").unwrap();
    let (_, writer_writeback, _) = writer.into_parts();
    let (_, stale_writeback, _) = stale.into_parts();
    filesystem
        .commit_writeback(&writer_writeback.unwrap())
        .unwrap();
    filesystem
        .commit_writeback(&stale_writeback.unwrap())
        .unwrap();

    let mut reopened = filesystem.prepare_open(logical, libc::O_RDONLY, 0).unwrap();
    let OpenTarget::Descriptor(file) = reopened.target_mut() else {
        panic!("encrypted regular file did not use an anonymous descriptor");
    };
    let mut restored = String::new();
    file.read_to_string(&mut restored).unwrap();
    assert_eq!(restored, "newer content");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_writeback_waits_for_the_vfs_publication_lock() {
    let (root, filesystem) = fixture("writeback-lock");
    let logical = Path::new("/tmp/agora-vfs-writeback-lock");
    let mut prepared = filesystem
        .prepare_open(logical, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
        .unwrap();
    filesystem.commit_open(&mut prepared).unwrap();
    let OpenTarget::Descriptor(file) = prepared.target_mut() else {
        panic!("encrypted regular file did not use an anonymous descriptor");
    };
    file.write_all(b"dirty writeback").unwrap();
    let (_, writeback, _) = prepared.into_parts();
    let writeback = writeback.unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join(".vfs.lock"))
        .unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);

    let (completed_tx, completed_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = filesystem.commit_writeback(&writeback);
        completed_tx.send(()).unwrap();
        result
    });
    assert!(
        completed_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
    worker.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_write_intent_copies_up_lower_content_without_changing_lower() {
    let (root, filesystem) = fixture("copy-up");
    let source = root
        .parent()
        .unwrap()
        .join(format!("agora-vfs-copy-up-{}", uuid::Uuid::new_v4()));
    std::fs::write(&source, b"lower content").unwrap();

    let mut prepared = filesystem.prepare_open(&source, libc::O_RDWR, 0).unwrap();
    let OpenTarget::Descriptor(file) = prepared.target_mut() else {
        panic!("encrypted copy-up should expose an anonymous descriptor");
    };
    let mut initial = String::new();
    file.read_to_string(&mut initial).unwrap();
    assert_eq!(initial, "lower content");
    file.seek(SeekFrom::Start(0)).unwrap();
    file.set_len(0).unwrap();
    file.write_all(b"upper content").unwrap();
    filesystem.commit_open(&mut prepared).unwrap();
    let (target, writeback, _) = prepared.into_parts();
    let OpenTarget::Descriptor(_) = target else {
        panic!("expected descriptor");
    };
    filesystem.commit_writeback(&writeback.unwrap()).unwrap();

    assert_eq!(std::fs::read(&source).unwrap(), b"lower content");
    let backing = filesystem.prepare_read(&source).unwrap();
    assert_ne!(backing, source);
    assert!(
        !std::fs::read(&backing)
            .unwrap()
            .windows(b"upper content".len())
            .any(|window| window == b"upper content")
    );

    let mut reopened = filesystem.prepare_open(&source, libc::O_RDONLY, 0).unwrap();
    let OpenTarget::Descriptor(file) = reopened.target_mut() else {
        panic!("upper encrypted file should use an anonymous descriptor");
    };
    let mut restored = String::new();
    file.read_to_string(&mut restored).unwrap();
    assert_eq!(restored, "upper content");

    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_file(source).unwrap();
}

#[test]
fn encrypted_open_honors_exclusive_create_and_truncate() {
    let (root, filesystem) = fixture("flags");
    let logical = Path::new("/tmp/agora-vfs-flags");
    let mut created = filesystem
        .prepare_open(logical, libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    filesystem.commit_open(&mut created).unwrap();
    let (target, writeback, _) = created.into_parts();
    let OpenTarget::Descriptor(_) = target else {
        panic!("expected descriptor");
    };
    filesystem.commit_writeback(&writeback.unwrap()).unwrap();

    assert!(
        filesystem
            .prepare_open(logical, libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600,)
            .is_err()
    );
    let truncated = filesystem
        .prepare_open(logical, libc::O_RDWR | libc::O_TRUNC, 0)
        .unwrap();
    let OpenTarget::Descriptor(file) = truncated.target() else {
        panic!("expected descriptor");
    };
    assert_eq!(file.metadata().unwrap().len(), 0);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_exclusive_create_reserves_the_missing_path_until_open_commits() {
    let (root, first_filesystem) = fixture("exclusive-reservation");
    let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
    let second_filesystem = VirtualFilesystem::encrypted(&root, cipher).unwrap();
    let logical = Path::new("/tmp/agora-vfs-exclusive-reservation");

    let first = first_filesystem
        .prepare_open(logical, libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600)
        .unwrap();
    let error = match second_filesystem.prepare_open(
        logical,
        libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
        0o600,
    ) {
        Ok(_) => panic!("a second exclusive create unexpectedly succeeded"),
        Err(error) => error,
    };
    assert_eq!(errno(&error), Some(libc::EEXIST));

    drop(first);
    assert!(
        second_filesystem
            .prepare_open(logical, libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600,)
            .is_ok()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn abandoned_exclusive_reservation_keeps_a_later_published_create() {
    let (root, first_filesystem) = fixture("exclusive-reservation-publish");
    let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
    let second_filesystem = VirtualFilesystem::encrypted(&root, cipher).unwrap();
    let logical = Path::new("/tmp/agora-vfs-exclusive-reservation-publish");

    let first = first_filesystem
        .prepare_open(logical, libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600)
        .unwrap();
    let mut second = second_filesystem
        .prepare_open(logical, libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    let OpenTarget::Descriptor(file) = second.target_mut() else {
        panic!("expected encrypted descriptor");
    };
    file.write_all(b"published").unwrap();
    second_filesystem.commit_open(&mut second).unwrap();

    drop(first);

    let mut reopened = second_filesystem
        .prepare_open(logical, libc::O_RDONLY, 0)
        .unwrap();
    let OpenTarget::Descriptor(file) = reopened.target_mut() else {
        panic!("expected encrypted descriptor");
    };
    let mut restored = String::new();
    file.read_to_string(&mut restored).unwrap();
    assert_eq!(restored, "published");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_rename_retargets_a_live_writable_snapshot() {
    let (root, first_filesystem) = fixture("write-lease");
    let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
    let second_filesystem = VirtualFilesystem::encrypted(&root, cipher).unwrap();
    let logical = Path::new("/tmp/agora-vfs-write-lease");
    let renamed = Path::new("/tmp/agora-vfs-write-lease-renamed");
    let mut prepared = first_filesystem
        .prepare_open(logical, libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    let OpenTarget::Descriptor(file) = prepared.target_mut() else {
        panic!("expected encrypted descriptor");
    };
    file.write_all(b"before rename").unwrap();
    first_filesystem.commit_open(&mut prepared).unwrap();
    let (mut target, writeback, _) = prepared.into_parts();
    let writeback = writeback.unwrap();

    second_filesystem.rename(logical, renamed).unwrap();
    assert!(!second_filesystem.exists(logical).unwrap());
    assert!(second_filesystem.exists(renamed).unwrap());

    let OpenTarget::Descriptor(file) = &mut target else {
        panic!("expected encrypted descriptor");
    };
    file.write_all(b" after rename").unwrap();
    first_filesystem.commit_writeback(&writeback).unwrap();

    let mut reopened = second_filesystem
        .prepare_open(renamed, libc::O_RDONLY, 0)
        .unwrap();
    let OpenTarget::Descriptor(file) = reopened.target_mut() else {
        panic!("expected encrypted descriptor");
    };
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "before rename after rename");

    drop(writeback);
    drop(target);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_rename_detaches_the_replaced_target_snapshot() {
    let (root, first_filesystem) = fixture("rename-target-write-lease");
    let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
    let second_filesystem = VirtualFilesystem::encrypted(&root, cipher).unwrap();
    let source = Path::new("/tmp/agora-vfs-rename-source");
    let target = Path::new("/tmp/agora-vfs-rename-target");

    let mut target_prepared = first_filesystem
        .prepare_open(target, libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    let OpenTarget::Descriptor(file) = target_prepared.target_mut() else {
        panic!("expected encrypted descriptor");
    };
    file.write_all(b"replaced target").unwrap();
    first_filesystem.commit_open(&mut target_prepared).unwrap();
    let (mut target_descriptor, target_writeback, _) = target_prepared.into_parts();
    let target_writeback = target_writeback.unwrap();

    let mut source_prepared = second_filesystem
        .prepare_open(source, libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    let OpenTarget::Descriptor(file) = source_prepared.target_mut() else {
        panic!("expected encrypted descriptor");
    };
    file.write_all(b"renamed source").unwrap();
    second_filesystem.commit_open(&mut source_prepared).unwrap();
    drop(source_prepared);

    second_filesystem.rename(source, target).unwrap();

    let OpenTarget::Descriptor(file) = &mut target_descriptor else {
        panic!("expected encrypted descriptor");
    };
    file.write_all(b" stale write").unwrap();
    assert_eq!(
        first_filesystem
            .commit_writeback(&target_writeback)
            .unwrap(),
        None
    );

    let mut reopened = second_filesystem
        .prepare_open(target, libc::O_RDONLY, 0)
        .unwrap();
    let OpenTarget::Descriptor(file) = reopened.target_mut() else {
        panic!("expected encrypted descriptor");
    };
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "renamed source");

    drop(target_writeback);
    drop(target_descriptor);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_unlink_detaches_a_live_writable_snapshot() {
    let (root, first_filesystem) = fixture("unlink-write-lease");
    let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
    let second_filesystem = VirtualFilesystem::encrypted(&root, cipher).unwrap();
    let logical = Path::new("/tmp/agora-vfs-unlink-write-lease");
    let mut prepared = first_filesystem
        .prepare_open(logical, libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    let OpenTarget::Descriptor(file) = prepared.target_mut() else {
        panic!("expected encrypted descriptor");
    };
    file.write_all(b"before unlink").unwrap();
    first_filesystem.commit_open(&mut prepared).unwrap();
    let (mut target, writeback, _) = prepared.into_parts();
    let writeback = writeback.unwrap();

    second_filesystem.remove(logical, false).unwrap();
    assert!(!second_filesystem.exists(logical).unwrap());

    let OpenTarget::Descriptor(file) = &mut target else {
        panic!("expected encrypted descriptor");
    };
    file.write_all(b" after unlink").unwrap();
    first_filesystem.commit_writeback(&writeback).unwrap();
    assert!(!second_filesystem.exists(logical).unwrap());

    drop(writeback);
    drop(target);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_descriptors_preserve_the_requested_access_mode() {
    let (root, filesystem) = fixture("access-mode");
    let logical = Path::new("/tmp/agora-vfs-access-mode");
    let mut created = filesystem
        .prepare_open(logical, libc::O_CREAT | libc::O_WRONLY, 0o600)
        .unwrap();
    let OpenTarget::Descriptor(file) = created.target_mut() else {
        panic!("expected encrypted descriptor");
    };
    assert_eq!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) } & libc::O_ACCMODE,
        libc::O_WRONLY
    );
    assert_eq!(
        unsafe { libc::read(file.as_raw_fd(), std::ptr::null_mut(), 0) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
    file.write_all(b"contents").unwrap();
    filesystem.commit_open(&mut created).unwrap();
    let (target, writeback, _) = created.into_parts();
    let OpenTarget::Descriptor(_) = target else {
        panic!("expected encrypted descriptor");
    };
    filesystem.commit_writeback(&writeback.unwrap()).unwrap();

    let reopened = filesystem.prepare_open(logical, libc::O_RDONLY, 0).unwrap();
    let OpenTarget::Descriptor(file) = reopened.target() else {
        panic!("expected encrypted descriptor");
    };
    assert_eq!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) } & libc::O_ACCMODE,
        libc::O_RDONLY
    );
    assert_eq!(
        unsafe { libc::write(file.as_raw_fd(), std::ptr::null(), 0) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_write_opens_do_not_wait_for_each_other() {
    let (root, filesystem) = fixture("concurrent-write-open");
    let logical = Path::new("/tmp/agora-vfs-write-lock");
    let mut created = filesystem
        .prepare_open(logical, libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    filesystem.commit_open(&mut created).unwrap();
    let (target, writeback, _) = created.into_parts();
    let OpenTarget::Descriptor(_) = target else {
        panic!("expected descriptor");
    };
    filesystem.commit_writeback(&writeback.unwrap()).unwrap();
    let first = filesystem.prepare_open(logical, libc::O_RDWR, 0).unwrap();
    let second = filesystem.prepare_open(logical, libc::O_RDWR, 0).unwrap();
    drop(first);
    drop(second);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_open_rejects_missing_files_and_keeps_directories_as_paths() {
    let (root, filesystem) = fixture("types");
    assert!(
        filesystem
            .prepare_open(Path::new("/tmp/agora-vfs-missing"), libc::O_RDONLY, 0)
            .is_err()
    );

    let directory = root
        .parent()
        .unwrap()
        .join(format!("agora-vfs-directory-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let prepared = filesystem
        .prepare_open(&directory, libc::O_RDONLY, 0)
        .unwrap();
    assert!(matches!(prepared.target(), OpenTarget::Path(_)));
    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn plain_vfs_delegates_overlay_operations_without_writeback() {
    let directory = std::env::temp_dir().join(format!("agora-vfs-plain-{}", uuid::Uuid::new_v4()));
    let root = directory.join("fs");
    let lower = directory.join("lower");
    std::fs::create_dir_all(&lower).unwrap();
    let source = lower.join("source");
    std::fs::write(&source, b"plain").unwrap();
    let filesystem = VirtualFilesystem::plain(&root).unwrap();

    let prepared = filesystem.prepare_open(&source, libc::O_RDONLY, 0).unwrap();
    let OpenTarget::Path(mapped) = prepared.target() else {
        panic!("plain filesystem should expose a mapped path");
    };
    assert_eq!(mapped, &source);
    assert_eq!(std::fs::read(mapped).unwrap(), b"plain");
    assert_eq!(filesystem.prepare_metadata(&source, true).unwrap().1, None);

    let created = lower.join("created");
    let staged = filesystem.stage_write(&created, true).unwrap();
    std::fs::write(staged.destination(), b"created").unwrap();
    filesystem.commit_write(staged).unwrap();
    assert_eq!(
        std::fs::read(filesystem.prepare_read(&created).unwrap()).unwrap(),
        b"created"
    );

    let child = lower.join("directory");
    filesystem.create_directory(&child, 0o700).unwrap();
    assert!(filesystem.prepare_directory(&child).unwrap().is_dir());
    assert!(filesystem.directory_view(&child).unwrap().lower().is_none());
    let renamed = lower.join("renamed");
    filesystem.rename(&created, &renamed).unwrap();
    filesystem.remove(&renamed, false).unwrap();
    assert!(filesystem.prepare_read(&renamed).is_err());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn authorized_socket_bind_creates_only_an_overlay_socket() {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let directory = Path::new("/tmp").join(format!("as-{}", &suffix[..8]));
    let root = directory.join("fs");
    let lower = directory.join("lower");
    std::fs::create_dir_all(&lower).unwrap();
    let logical = lower.join("service.sock");
    let filesystem = VirtualFilesystem::plain(&root).unwrap();

    let listener = filesystem
        .bind_socket_authorized(&logical, &Credentials::effective(), |mapped| {
            UnixListener::bind(mapped).map_err(Into::into)
        })
        .unwrap();
    let mapped = filesystem.prepare_read(&logical).unwrap();

    assert!(!logical.exists());
    assert!(mapped.symlink_metadata().unwrap().file_type().is_socket());
    let _client = UnixStream::connect(&mapped).unwrap();
    let _accepted = listener.accept().unwrap();

    filesystem
        .remove_authorized(&logical, false, &Credentials::effective())
        .unwrap();
    assert!(!mapped.exists());
    drop(listener);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn failed_authorized_socket_bind_does_not_publish_an_overlay_entry() {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let directory = Path::new("/tmp").join(format!("as-{}", &suffix[..8]));
    let root = directory.join("fs");
    let lower = directory.join("lower");
    std::fs::create_dir_all(&lower).unwrap();
    let logical = lower.join("service.sock");
    let filesystem = VirtualFilesystem::plain(&root).unwrap();

    let error = filesystem
        .bind_socket_authorized::<()>(&logical, &Credentials::effective(), |_mapped| {
            Err(std::io::Error::from_raw_os_error(libc::EADDRINUSE).into())
        })
        .unwrap_err();

    assert_eq!(errno(&error), Some(libc::EADDRINUSE));
    assert!(!logical.exists());
    assert!(!filesystem.exists(&logical).unwrap());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn authorized_socket_bind_never_replaces_an_existing_lower_socket() {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let workdir = Path::new("/tmp").join(format!("ab-{}", &suffix[..8]));
    let lower = Path::new("/tmp").join(format!("lb-{}", &suffix[..8]));
    std::fs::create_dir_all(&lower).unwrap();
    let logical = lower.join("service.sock");
    let lower_listener = UnixListener::bind(&logical).unwrap();
    let filesystem = VirtualFilesystem::plain(workdir.join("fs")).unwrap();
    let mut invoked = false;

    let error = filesystem
        .bind_socket_authorized::<()>(&logical, &Credentials::effective(), |_mapped| {
            invoked = true;
            Ok(())
        })
        .unwrap_err();

    assert_eq!(errno(&error), Some(libc::EADDRINUSE));
    assert!(!invoked);
    assert!(logical.exists());
    drop(lower_listener);
    std::fs::remove_file(logical).unwrap();
    std::fs::remove_dir_all(workdir).unwrap();
    std::fs::remove_dir_all(lower).unwrap();
}

#[test]
fn encrypted_socket_bind_keeps_the_socket_native_under_an_encrypted_name() {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let workdir = Path::new("/tmp").join(format!("ae-{}", &suffix[..8]));
    let lower = Path::new("/tmp").join(format!("el-{}", &suffix[..8]));
    std::fs::create_dir_all(&lower).unwrap();
    let logical = lower.join("service.sock");
    let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
    let filesystem = VirtualFilesystem::encrypted(workdir.join("fs"), cipher).unwrap();

    let listener = filesystem
        .bind_socket_authorized(&logical, &Credentials::effective(), |mapped| {
            UnixListener::bind(mapped).map_err(Into::into)
        })
        .unwrap();
    let mapped = filesystem.prepare_read(&logical).unwrap();

    assert!(!logical.exists());
    assert_ne!(mapped.file_name(), logical.file_name());
    assert!(mapped.symlink_metadata().unwrap().file_type().is_socket());
    let _client = UnixStream::connect(&mapped).unwrap();
    let _accepted = listener.accept().unwrap();

    filesystem
        .remove_authorized(&logical, false, &Credentials::effective())
        .unwrap();
    drop(listener);
    std::fs::remove_dir_all(workdir).unwrap();
    std::fs::remove_dir_all(lower).unwrap();
}
