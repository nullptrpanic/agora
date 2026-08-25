use super::{
    FILE_ATTRIBUTE_DIRECTORY, SmbRoot, SmbStorage, build_open_request, build_rename_information,
    configured_storage, emit_directory_page, expect_success, is_smb_control_entry,
    metadata_from_close, metadata_from_create, metadata_from_file, remote_path, same_file_object,
    smb_errno, staging_path, stale_file, storage_error, validate_read_response_size,
    validate_remote_root, validate_transfer_size, wire_path, write_lock_path,
};
use crate::nfs::SmbRemoteConfig;
use crate::nfs::backend::{RemoteStorage, StorageResult};
use crate::nfs::protocol::{RemoteEntry, RemoteFileType, RemotePath};
use smb2::client::tree::FileInfo;
use smb2::msg::close::CloseResponse;
use smb2::msg::create::{CreateAction, CreateDisposition, CreateResponse, ShareAccess};
use smb2::msg::header::Header;
use smb2::pack::FileTime;
use smb2::types::flags::FileAccessMask;
use smb2::types::{Command, FileId, OplockLevel, TreeId, status::NtStatus};
use smb2::{Error, ErrorKind, Frame, Tree};

fn assert_errno<T>(result: StorageResult<T>, expected: libc::c_int) {
    match result {
        Ok(_) => panic!("operation unexpectedly succeeded"),
        Err(error) => assert_eq!(error.errno(), expected),
    }
}

fn directory_entry(name: &str, size: u64, directory: bool) -> Vec<u8> {
    let encoded = name.encode_utf16().collect::<Vec<_>>();
    let mut entry = Vec::with_capacity(94 + encoded.len() * 2);
    entry.extend_from_slice(&0_u32.to_le_bytes());
    entry.extend_from_slice(&0_u32.to_le_bytes());
    entry.extend_from_slice(&100_u64.to_le_bytes());
    entry.extend_from_slice(&0_u64.to_le_bytes());
    entry.extend_from_slice(&200_u64.to_le_bytes());
    entry.extend_from_slice(&0_u64.to_le_bytes());
    entry.extend_from_slice(&size.to_le_bytes());
    entry.extend_from_slice(&size.to_le_bytes());
    entry.extend_from_slice(
        &(if directory {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            0
        })
        .to_le_bytes(),
    );
    entry.extend_from_slice(&u32::try_from(encoded.len() * 2).unwrap().to_le_bytes());
    entry.extend_from_slice(&0_u32.to_le_bytes());
    entry.extend_from_slice(&[0, 0]);
    entry.extend_from_slice(&[0; 24]);
    for unit in encoded {
        entry.extend_from_slice(&unit.to_le_bytes());
    }
    entry
}

fn directory_page(entries: &[(&str, u64, bool)]) -> Vec<u8> {
    let mut page = Vec::new();
    for (index, (name, size, directory)) in entries.iter().enumerate() {
        let mut entry = directory_entry(name, *size, *directory);
        if index + 1 < entries.len() {
            let next = u32::try_from(entry.len()).unwrap();
            entry[..4].copy_from_slice(&next.to_le_bytes());
        }
        page.extend_from_slice(&entry);
    }
    page
}

#[test]
fn smb_paths_are_root_relative_and_never_escape_the_configured_prefix() {
    let path = RemotePath::new(0, "child/file.txt").unwrap();
    assert_eq!(remote_path("base/team", &path), "base/team/child/file.txt");
    assert_eq!(remote_path("", &path), "child/file.txt");
    assert_eq!(
        remote_path("base/team", &RemotePath::new(0, "").unwrap()),
        "base/team"
    );
}

#[test]
fn smb_metadata_uses_stable_remote_identity_fields() {
    let info = FileInfo {
        size: 42,
        is_directory: false,
        created: FileTime(133_485_408_000_000_000),
        modified: FileTime(133_485_408_001_234_567),
        accessed: FileTime(133_485_408_002_000_000),
    };

    let metadata = metadata_from_file(&info);

    assert_eq!(metadata.file_type, RemoteFileType::File);
    assert_eq!(metadata.size, 42);
    assert_eq!(metadata.modified_seconds, 1_704_067_200);
    assert_eq!(metadata.modified_nanoseconds, 123_456_700);
    assert_eq!(
        metadata.identity,
        "file:42:133485408001234567:133485408000000000"
    );
}

#[test]
fn smb_connection_probe_requires_the_configured_remote_path_to_be_a_directory() {
    let directory = FileInfo {
        size: 0,
        is_directory: true,
        created: FileTime(0),
        modified: FileTime(0),
        accessed: FileTime(0),
    };
    validate_remote_root(Ok(directory)).unwrap();

    let file = FileInfo {
        size: 0,
        is_directory: false,
        created: FileTime(0),
        modified: FileTime(0),
        accessed: FileTime(0),
    };
    assert_errno(validate_remote_root(Ok(file)), libc::ENOTDIR);
    assert_errno(
        validate_remote_root(Err(Error::Protocol {
            status: NtStatus::OBJECT_NAME_NOT_FOUND,
            command: Command::Create,
        })),
        libc::ENOENT,
    );
}

#[test]
fn smb_reads_reject_empty_and_oversized_responses() {
    validate_read_response_size(64, 64, 64).unwrap();
    assert!(validate_read_response_size(64, 64, 0).is_err());
    assert!(validate_read_response_size(64, 64, 65).is_err());
    assert!(validate_read_response_size(64, 4, 5).is_err());
}

#[test]
fn smb_rejects_an_opened_file_that_exceeds_the_transfer_limit() {
    validate_transfer_size(4, 4).unwrap();
    assert_eq!(
        validate_transfer_size(5, 4).unwrap_err().kind(),
        ErrorKind::Io
    );
}

#[test]
fn smb_errors_map_to_posix_errno_without_string_matching() {
    let missing = smb2::Error::Protocol {
        status: NtStatus::OBJECT_NAME_NOT_FOUND,
        command: Command::Create,
    };
    let not_empty = smb2::Error::Protocol {
        status: NtStatus::DIRECTORY_NOT_EMPTY,
        command: Command::SetInfo,
    };

    assert_eq!(smb_errno(&missing), libc::ENOENT);
    assert_eq!(smb_errno(&not_empty), libc::ENOTEMPTY);
}

#[test]
fn smb_rename_information_requests_atomic_target_replacement() {
    let buffer = build_rename_information("folder\\target.txt", true);

    assert_eq!(buffer[0], 1, "ReplaceIfExists must be true");
    assert_eq!(u32::from_le_bytes(buffer[16..20].try_into().unwrap()), 34);

    let create = build_rename_information("folder\\new.txt", false);
    assert_eq!(create[0], 0, "new files must not replace a raced target");
}

#[test]
fn smb_rename_opens_both_files_and_directories() {
    let tree = Tree {
        tree_id: TreeId(7),
        share_name: "share".to_string(),
        server: "server:445".to_string(),
        is_dfs: false,
        encrypt_data: false,
    };
    let request = build_open_request(
        &tree,
        "folder",
        CreateDisposition::FileOpen,
        FileAccessMask::new(FileAccessMask::DELETE),
        ShareAccess(ShareAccess::FILE_SHARE_DELETE),
        0,
    );

    assert_eq!(request.create_options, 0);
}

#[test]
fn smb_writeback_stages_to_an_opaque_sibling() {
    let staged = staging_path("folder/report.docx", "0123456789abcdef");

    assert_eq!(staged, "folder/.agora-write-0123456789abcdef.tmp");
    assert!(!staged.contains("report.docx"));
    assert_eq!(
        staging_path("report.docx", "fedcba9876543210"),
        ".agora-write-fedcba9876543210.tmp"
    );
}

#[test]
fn smb_writeback_lock_is_stable_opaque_and_scoped_to_the_parent() {
    let first = write_lock_path("folder/report.docx");
    let second = write_lock_path("folder/report.docx");
    let other = write_lock_path("folder/other.docx");

    assert_eq!(first, second);
    assert_ne!(first, other);
    assert!(first.starts_with("folder/.agora-lock-"));
    assert!(!first.contains("report"));
}

#[test]
fn smb_listing_hides_only_reserved_transaction_artifacts() {
    assert!(is_smb_control_entry(
        ".agora-write-0123456789abcdef0123456789abcdef.tmp"
    ));
    assert!(is_smb_control_entry(
        ".agora-lock-0123456789abcdef0123456789abcdef.lck"
    ));
    assert!(!is_smb_control_entry(".agora-write-abc.tmp"));
    assert!(!is_smb_control_entry(".agora-lock-abc.lck"));
    assert!(!is_smb_control_entry(".agora-write-report.tmp"));
    assert!(!is_smb_control_entry(".agora-lock-report.lck"));
    assert!(!is_smb_control_entry(
        ".agora-write-0123456789ABCDEF0123456789ABCDEF.tmp"
    ));
    assert!(!is_smb_control_entry(".agora-write-not-a-temp"));
    assert!(!is_smb_control_entry("report.docx"));
}

#[test]
fn smb_directory_pages_preserve_visible_entries_and_filter_only_control_entries() {
    let page = directory_page(&[
        (".", 0, true),
        ("..", 0, true),
        (
            ".agora-write-0123456789abcdef0123456789abcdef.tmp",
            4,
            false,
        ),
        ("report.txt", 12, false),
        ("資料", 0, true),
    ]);
    let mut entries = Vec::<RemoteEntry>::new();

    emit_directory_page(&page, &mut |entry| {
        entries.push(entry);
        Ok(())
    })
    .unwrap();

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "report.txt");
    assert_eq!(entries[0].metadata.file_type, RemoteFileType::File);
    assert_eq!(entries[0].metadata.size, 12);
    assert_eq!(entries[0].metadata.identity, "file:12:200:100");
    assert_eq!(entries[1].name, "資料");
    assert_eq!(entries[1].metadata.file_type, RemoteFileType::Directory);
    assert_eq!(entries[1].metadata.identity, "directory:0:200:100");
}

#[tokio::test]
async fn smb_storage_rejects_direct_access_to_reserved_transaction_artifacts() {
    let storage = SmbStorage::new(&[]);
    let control = RemotePath::new(0, ".agora-write-0123456789abcdef0123456789abcdef.tmp").unwrap();
    let ordinary = RemotePath::new(0, ".agora-write-report.tmp").unwrap();

    assert_errno(storage.stat(&control).await, libc::EACCES);
    assert_errno(
        storage.open_file(&control, libc::O_RDONLY, 0).await,
        libc::EACCES,
    );
    let mut file = tempfile::tempfile().unwrap();
    assert_errno(
        storage
            .write_from_if_unchanged(&control, None, &mut file, 0)
            .await,
        libc::EACCES,
    );
    let mut emit = |_| Ok(());
    assert_errno(storage.list(&control, &mut emit).await, libc::EACCES);
    assert_errno(storage.create_directory(&control).await, libc::EACCES);
    assert_errno(storage.remove(&control, false).await, libc::EACCES);
    assert_errno(storage.rename(&control, &ordinary).await, libc::EACCES);
    assert_errno(storage.rename(&ordinary, &control).await, libc::EACCES);

    assert_errno(storage.stat(&ordinary).await, libc::EINVAL);
}

#[test]
fn smb_publication_verification_uses_the_server_file_index() {
    let metadata = |identity: &str| crate::nfs::protocol::RemoteMetadata {
        file_type: RemoteFileType::File,
        size: 1,
        modified_seconds: 0,
        modified_nanoseconds: 0,
        identity: identity.to_string(),
    };

    assert!(same_file_object(
        &metadata("file:1:100:200:300:42"),
        &metadata("file:1:101:200:301:42")
    ));
    assert!(!same_file_object(
        &metadata("file:1:100:200:300:42"),
        &metadata("file:1:100:200:300:43")
    ));
}

#[tokio::test]
async fn smb_storage_rejects_unknown_roots_before_network_access() {
    let storage = SmbStorage::new(&[]);
    let path = RemotePath::new(0, "file.txt").unwrap();

    assert_errno(storage.connect(0).await, libc::EINVAL);
    assert_errno(storage.stat(&path).await, libc::EINVAL);
    assert_errno(
        storage.open_file(&path, libc::O_RDONLY, 0).await,
        libc::EINVAL,
    );
    let mut file = tempfile::tempfile().unwrap();
    assert_errno(
        storage
            .write_from_if_unchanged(&path, None, &mut file, 0)
            .await,
        libc::EINVAL,
    );
    let mut emit = |_| Ok(());
    assert_errno(storage.list(&path, &mut emit).await, libc::EINVAL);
    assert_errno(storage.create_directory(&path).await, libc::EINVAL);
    assert_errno(storage.remove(&path, false).await, libc::EINVAL);
    assert_errno(
        storage
            .rename(&path, &RemotePath::new(0, "renamed.txt").unwrap())
            .await,
        libc::EINVAL,
    );
    assert_errno(
        storage
            .rename(&path, &RemotePath::new(1, "renamed.txt").unwrap())
            .await,
        libc::EXDEV,
    );

    let configured = configured_storage(&[]);
    assert_errno(configured.connect(0).await, libc::EINVAL);
}

#[tokio::test]
async fn smb_storage_propagates_connection_failures_for_every_remote_operation() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let config = SmbRemoteConfig::new("/remote", address.to_string(), "share").unwrap();
    let storage = SmbStorage::new(&[config]);
    let path = RemotePath::new(0, "file.txt").unwrap();
    let renamed = RemotePath::new(0, "renamed.txt").unwrap();

    assert!(storage.connect(0).await.is_err());
    assert!(storage.stat(&path).await.is_err());
    assert!(storage.open_file(&path, libc::O_RDONLY, 0).await.is_err());
    let mut file = tempfile::tempfile().unwrap();
    assert!(
        storage
            .write_from_if_unchanged(&path, None, &mut file, 0)
            .await
            .is_err()
    );
    let mut emit = |_| Ok(());
    assert!(storage.list(&path, &mut emit).await.is_err());
    assert!(storage.create_directory(&path).await.is_err());
    assert!(storage.remove(&path, false).await.is_err());
    assert!(storage.remove(&path, true).await.is_err());
    assert!(storage.rename(&path, &renamed).await.is_err());
}

#[test]
fn smb_root_and_wire_paths_keep_backend_details_inside_the_backend() {
    let config = SmbRemoteConfig::new("/remote", "server", "share")
        .unwrap()
        .with_remote_path("base/team")
        .unwrap();
    let root = SmbRoot::new(config);
    assert_eq!(root.slots.len(), super::SMB_SESSION_POOL_SIZE);
    assert_eq!(
        root.path(&RemotePath::new(0, "child.txt").unwrap()),
        "base/team/child.txt"
    );

    let mut tree = Tree {
        tree_id: TreeId(7),
        share_name: "share".to_string(),
        server: "server:445".to_string(),
        is_dfs: false,
        encrypt_data: false,
    };
    assert_eq!(wire_path(&tree, "folder/file.txt"), "folder\\file.txt");
    tree.is_dfs = true;
    assert_eq!(wire_path(&tree, ""), "server\\share");
    assert_eq!(
        wire_path(&tree, "folder/file.txt"),
        "server\\share\\folder\\file.txt"
    );
}

#[test]
fn smb_create_and_close_metadata_preserve_type_size_and_creation_identity() {
    let created = CreateResponse {
        oplock_level: OplockLevel::None,
        flags: 0,
        create_action: CreateAction::FileOpened,
        creation_time: FileTime(100),
        last_access_time: FileTime(101),
        last_write_time: FileTime(102),
        change_time: FileTime(103),
        allocation_size: 16,
        end_of_file: 7,
        file_attributes: FILE_ATTRIBUTE_DIRECTORY,
        file_id: FileId::default(),
        create_contexts: Vec::new(),
    };
    let metadata = metadata_from_create(&created, 0x1234);
    assert_eq!(metadata.file_type, RemoteFileType::Directory);
    assert_eq!(metadata.size, 7);
    assert_eq!(metadata.modified_seconds, 0);
    assert_eq!(metadata.identity, "directory:7:102:100:103:4660");

    let closed = CloseResponse {
        flags: 0,
        creation_time: FileTime(200),
        last_access_time: FileTime(201),
        last_write_time: FileTime(202),
        change_time: FileTime(203),
        allocation_size: 32,
        end_of_file: 11,
        file_attributes: 0,
    };
    let metadata = metadata_from_close(&closed, 0x5678);
    assert_eq!(metadata.file_type, RemoteFileType::File);
    assert_eq!(metadata.size, 11);
    assert_eq!(metadata.identity, "file:11:202:200:203:22136");
}

#[test]
fn smb_frame_and_error_translation_covers_protocol_and_transport_failures() {
    let frame = Frame {
        header: Header::new_request(Command::Create),
        body: Vec::new(),
        raw: Vec::new(),
    };
    expect_success(&frame, Command::Create).unwrap();
    let mut denied = frame;
    denied.header.status = NtStatus::ACCESS_DENIED;
    assert!(expect_success(&denied, Command::Create).is_err());

    let protocol = |status| Error::Protocol {
        status,
        command: Command::Create,
    };
    let cases = vec![
        (protocol(NtStatus::ACCESS_DENIED), libc::EACCES),
        (protocol(NtStatus::OBJECT_NAME_NOT_FOUND), libc::ENOENT),
        (protocol(NtStatus::OBJECT_NAME_COLLISION), libc::EEXIST),
        (protocol(NtStatus::SHARING_VIOLATION), libc::EBUSY),
        (protocol(NtStatus::FILE_IS_A_DIRECTORY), libc::EISDIR),
        (protocol(NtStatus::NOT_A_DIRECTORY), libc::ENOTDIR),
        (protocol(NtStatus::DISK_FULL), libc::ENOSPC),
        (protocol(NtStatus::PATH_NOT_COVERED), libc::EXDEV),
        (protocol(NtStatus::OBJECT_NAME_INVALID), libc::EINVAL),
        (protocol(NtStatus::NOT_SUPPORTED), libc::ENOTSUP),
        (protocol(NtStatus::DELETE_PENDING), libc::EBUSY),
        (Error::Timeout, libc::ETIMEDOUT),
        (Error::Disconnected, libc::ENETDOWN),
        (Error::Cancelled, libc::EINTR),
        (Error::SessionExpired, libc::EIO),
        (
            Error::DfsReferralRequired {
                path: "remote".to_string(),
            },
            libc::EXDEV,
        ),
        (
            Error::FileTooLargeForSingleRead {
                size: u64::MAX,
                max_read: 1,
            },
            libc::EFBIG,
        ),
        (
            Error::Io(std::io::Error::from_raw_os_error(libc::ENOMEM)),
            libc::ENOMEM,
        ),
        (Error::invalid_data("bad frame"), libc::EPROTO),
        (protocol(NtStatus(0xDEAD_BEEF)), libc::EIO),
    ];
    for (error, expected) in cases {
        assert_eq!(smb_errno(&error), expected, "{error}");
    }

    let error = storage_error(Error::Timeout);
    assert_eq!(error.errno(), libc::ETIMEDOUT);
    assert!(error.to_string().contains("SMB operation failed"));
    assert_eq!(stale_file().errno(), libc::ESTALE);
}

mod mock;
