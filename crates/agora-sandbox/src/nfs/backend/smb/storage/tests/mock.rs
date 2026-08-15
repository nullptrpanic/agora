use super::super::{SmbRoot, SmbSession, SmbSlot, SmbStorage, open_direct_handle};
use crate::nfs::SmbRemoteConfig;
use crate::nfs::backend::RemoteStorage;
use crate::nfs::protocol::{RemoteFileType, RemotePath};
use smb2::Tree;
use smb2::client::Connection;
use smb2::msg::close::CloseResponse;
use smb2::msg::create::{CreateAction, CreateDisposition, CreateResponse};
use smb2::msg::echo::{EchoRequest, EchoResponse};
use smb2::msg::flush::FlushResponse;
use smb2::msg::header::{ErrorResponse, Header};
use smb2::msg::query_directory::QueryDirectoryResponse;
use smb2::msg::query_info::QueryInfoResponse;
use smb2::msg::read::ReadResponse;
use smb2::msg::set_info::SetInfoResponse;
use smb2::msg::write::WriteResponse;
use smb2::pack::{FileTime, Pack, WriteCursor};
use smb2::transport::MockTransport;
use smb2::types::flags::FileAccessMask;
use smb2::types::status::NtStatus;
use smb2::types::{Command, FileId, OplockLevel, TreeId};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::time::Duration;

fn response(command: Command, body: &dyn Pack) -> Vec<u8> {
    response_with_status(command, NtStatus::SUCCESS, body)
}

fn response_with_status(command: Command, status: NtStatus, body: &dyn Pack) -> Vec<u8> {
    let mut header = Header::new_request(command);
    header.flags.set_response();
    header.credits = 32;
    header.status = status;
    let mut cursor = WriteCursor::new();
    header.pack(&mut cursor);
    body.pack(&mut cursor);
    cursor.into_inner()
}

fn error_response(command: Command, status: NtStatus) -> Vec<u8> {
    response_with_status(
        command,
        status,
        &ErrorResponse {
            error_context_count: 0,
            error_data: Vec::new(),
        },
    )
}

fn create_response(file_id: FileId, size: u64, action: CreateAction) -> Vec<u8> {
    response(
        Command::Create,
        &CreateResponse {
            oplock_level: OplockLevel::None,
            flags: 0,
            create_action: action,
            creation_time: FileTime(100),
            last_access_time: FileTime(101),
            last_write_time: FileTime(102),
            change_time: FileTime(103),
            allocation_size: size,
            end_of_file: size,
            file_attributes: 0,
            file_id,
            create_contexts: Vec::new(),
        },
    )
}

fn close_response(size: u64) -> Vec<u8> {
    response(
        Command::Close,
        &CloseResponse {
            flags: 0,
            creation_time: FileTime(100),
            last_access_time: FileTime(101),
            last_write_time: FileTime(102),
            change_time: FileTime(103),
            allocation_size: size,
            end_of_file: size,
            file_attributes: 0,
        },
    )
}

fn query_info_response(output_buffer: Vec<u8>) -> Vec<u8> {
    response(Command::QueryInfo, &QueryInfoResponse { output_buffer })
}

fn basic_information(attributes: u32) -> Vec<u8> {
    let mut output = Vec::with_capacity(40);
    for value in [100_u64, 101, 102, 103] {
        output.extend_from_slice(&value.to_le_bytes());
    }
    output.extend_from_slice(&attributes.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output
}

fn standard_information(size: u64, directory: bool) -> Vec<u8> {
    let mut output = Vec::with_capacity(24);
    output.extend_from_slice(&size.to_le_bytes());
    output.extend_from_slice(&size.to_le_bytes());
    output.extend_from_slice(&1_u32.to_le_bytes());
    output.push(0);
    output.push(u8::from(directory));
    output.extend_from_slice(&0_u16.to_le_bytes());
    output
}

fn metadata_responses(size: u64, index: u64, directory: bool) -> Vec<Vec<u8>> {
    vec![
        query_info_response(basic_information(if directory { 0x10 } else { 0 })),
        query_info_response(standard_information(size, directory)),
        query_info_response(index.to_le_bytes().to_vec()),
    ]
}

fn compound_response(responses: &[Vec<u8>]) -> Vec<u8> {
    let mut frame = Vec::new();
    for (index, response) in responses.iter().enumerate() {
        let mut response = response.clone();
        if index + 1 < responses.len() {
            let remainder = response.len() % 8;
            if remainder != 0 {
                response.resize(response.len() + (8 - remainder), 0);
            }
            let next = u32::try_from(response.len()).unwrap();
            response[20..24].copy_from_slice(&next.to_le_bytes());
        }
        frame.extend_from_slice(&response);
    }
    frame
}

async fn session() -> (SmbSession, Arc<MockTransport>) {
    let mock = Arc::new(MockTransport::new());
    mock.enable_auto_rewrite_msg_id();
    let connection = Connection::from_transport(
        Box::new(Arc::clone(&mock)),
        Box::new(Arc::clone(&mock)),
        "test-server",
    );
    mock.queue_response(response(Command::Echo, &EchoResponse));
    connection
        .execute(Command::Echo, &EchoRequest, None)
        .await
        .unwrap();
    mock.clear_sent();
    let tree = Tree {
        tree_id: TreeId(7),
        share_name: "share".to_string(),
        server: "test-server".to_string(),
        is_dfs: false,
        encrypt_data: false,
    };
    (SmbSession::from_connection(connection, tree), mock)
}

async fn storage(remote_path: &str) -> (SmbStorage, Arc<MockTransport>) {
    let (session, mock) = session().await;
    let config = SmbRemoteConfig::new("/remote", "test-server", "share").unwrap();
    let config = if remote_path.is_empty() {
        config
    } else {
        config.with_remote_path(remote_path).unwrap()
    };
    (
        SmbStorage {
            roots: vec![SmbRoot::with_slots(
                config,
                vec![SmbSlot {
                    session: Some(session),
                    generation: 1,
                }],
            )],
        },
        mock,
    )
}

async fn storage_with_two_slots() -> (SmbStorage, Arc<MockTransport>, Arc<MockTransport>) {
    let (first_session, first_mock) = session().await;
    let (second_session, second_mock) = session().await;
    let config = SmbRemoteConfig::new("/remote", "test-server", "share").unwrap();
    (
        SmbStorage {
            roots: vec![SmbRoot::with_slots(
                config,
                vec![
                    SmbSlot {
                        session: Some(first_session),
                        generation: 1,
                    },
                    SmbSlot {
                        session: Some(second_session),
                        generation: 1,
                    },
                ],
            )],
        },
        first_mock,
        second_mock,
    )
}

fn file_id(value: u64) -> FileId {
    FileId {
        persistent: value,
        volatile: value + 1,
    }
}

#[tokio::test]
async fn smb_session_can_execute_storage_protocol_over_an_injected_transport() {
    let (mut session, mock) = session().await;
    let file_id = FileId {
        persistent: 11,
        volatile: 22,
    };
    mock.queue_responses(vec![
        response(
            Command::Create,
            &CreateResponse {
                oplock_level: OplockLevel::None,
                flags: 0,
                create_action: CreateAction::FileOpened,
                creation_time: FileTime(100),
                last_access_time: FileTime(101),
                last_write_time: FileTime(102),
                change_time: FileTime(103),
                allocation_size: 16,
                end_of_file: 7,
                file_attributes: 0,
                file_id,
                create_contexts: Vec::new(),
            },
        ),
        response(
            Command::QueryInfo,
            &QueryInfoResponse {
                output_buffer: 42_u64.to_le_bytes().to_vec(),
            },
        ),
    ]);

    let (opened, index) = open_direct_handle(
        &mut session,
        "folder/report.txt",
        CreateDisposition::FileOpen,
        FileAccessMask::new(FileAccessMask::FILE_READ_DATA),
    )
    .await
    .unwrap();

    assert_eq!(opened.file_id, file_id);
    assert_eq!(opened.end_of_file, 7);
    assert_eq!(index, 42);
    mock.assert_fully_consumed();
}

#[tokio::test]
async fn direct_file_handles_preserve_data_size_and_metadata_over_scripted_smb() {
    let (storage, mock) = storage("").await;
    let path = RemotePath::new(0, "folder/report.txt").unwrap();
    let remote_file_id = file_id(40);
    mock.queue_responses(vec![
        create_response(remote_file_id, 7, CreateAction::FileOpened),
        query_info_response(900_u64.to_le_bytes().to_vec()),
    ]);

    let (mut handle, opened, created) =
        storage.open_file(&path, libc::O_RDWR, 0o600).await.unwrap();
    assert!(!created);
    assert_eq!(opened.size, 7);
    assert_eq!(opened.identity, "file:7:102:100:103:900");

    mock.queue_response(response(
        Command::Read,
        &ReadResponse {
            data_offset: 0x50,
            data_remaining: 0,
            flags: 0,
            data: b"hello".to_vec(),
        },
    ));
    let mut destination = tempfile::tempfile().unwrap();
    assert_eq!(
        storage
            .read_at(&mut handle, 1, 5, &mut destination)
            .await
            .unwrap(),
        5
    );
    destination.seek(SeekFrom::Start(0)).unwrap();
    let mut data = Vec::new();
    destination.read_to_end(&mut data).unwrap();
    assert_eq!(data, b"hello");

    mock.queue_response(response(
        Command::Write,
        &WriteResponse {
            count: 3,
            remaining: 0,
            write_channel_info_offset: 0,
            write_channel_info_length: 0,
        },
    ));
    let mut source = tempfile::tempfile().unwrap();
    source.write_all(b"new").unwrap();
    assert_eq!(
        storage
            .write_at(&mut handle, 7, &mut source, 3)
            .await
            .unwrap(),
        (3, 10)
    );

    mock.queue_response(response(Command::SetInfo, &SetInfoResponse));
    assert_eq!(storage.set_length(&mut handle, 12).await.unwrap(), 12);

    mock.queue_responses(metadata_responses(12, 901, false));
    let metadata = storage.file_metadata(&mut handle).await.unwrap();
    assert_eq!(metadata.file_type, RemoteFileType::File);
    assert_eq!(metadata.size, 12);
    assert_eq!(metadata.identity, "file:12:102:100:103:901");

    mock.queue_response(response(Command::Flush, &FlushResponse));
    mock.queue_responses(metadata_responses(13, 902, false));
    let metadata = storage.flush_file(&mut handle).await.unwrap();
    assert_eq!(metadata.size, 13);
    assert_eq!(metadata.identity, "file:13:102:100:103:902");

    mock.queue_responses(metadata_responses(13, 902, false));
    mock.queue_response(response(
        Command::Read,
        &ReadResponse {
            data_offset: 0x50,
            data_remaining: 0,
            flags: 0,
            data: b"snapshot-data".to_vec(),
        },
    ));
    mock.queue_responses(metadata_responses(13, 902, false));
    let mut snapshot = tempfile::tempfile().unwrap();
    let metadata = storage
        .read_into(&mut handle, &mut snapshot, 32)
        .await
        .unwrap();
    assert_eq!(metadata.size, 13);
    snapshot.seek(SeekFrom::Start(0)).unwrap();
    let mut data = Vec::new();
    snapshot.read_to_end(&mut data).unwrap();
    assert_eq!(data, b"snapshot-data");

    mock.queue_response(close_response(13));
    storage.close_file(&mut handle).await.unwrap();
    mock.assert_fully_consumed();
}

#[tokio::test]
async fn directory_listing_streams_multiple_pages_and_closes_the_handle() {
    let (storage, mock) = storage("").await;
    let directory = RemotePath::new(0, "folder").unwrap();
    let directory_id = file_id(50);
    mock.queue_responses(vec![
        create_response(directory_id, 0, CreateAction::FileOpened),
        response(
            Command::QueryDirectory,
            &QueryDirectoryResponse {
                output_buffer: super::directory_page(&[("first.txt", 5, false)]),
            },
        ),
        response(
            Command::QueryDirectory,
            &QueryDirectoryResponse {
                output_buffer: super::directory_page(&[("nested", 0, true)]),
            },
        ),
        error_response(Command::QueryDirectory, NtStatus::NO_MORE_FILES),
        close_response(0),
    ]);
    let mut entries = Vec::new();

    storage
        .list(&directory, &mut |entry| {
            entries.push(entry);
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "first.txt");
    assert_eq!(entries[0].metadata.size, 5);
    assert_eq!(entries[1].name, "nested");
    assert_eq!(entries[1].metadata.file_type, RemoteFileType::Directory);
    mock.assert_fully_consumed();
}

#[tokio::test]
async fn independent_path_operation_does_not_wait_for_a_busy_smb_session() {
    let (storage, first_mock, second_mock) = storage_with_two_slots().await;
    let storage = Arc::new(storage);
    second_mock.queue_responses(vec![
        create_response(file_id(55), 4, CreateAction::FileOpened),
        query_info_response(550_u64.to_le_bytes().to_vec()),
        close_response(4),
    ]);
    let busy_slot = storage.roots[0].slot(0).unwrap();
    let busy_session = busy_slot.lock_owned().await;
    let blocked_storage = Arc::clone(&storage);
    let blocked = tokio::spawn(async move {
        blocked_storage
            .stat(&RemotePath::new(0, "blocked.txt").unwrap())
            .await
    });
    while storage.roots[0]
        .next_slot
        .load(std::sync::atomic::Ordering::Relaxed)
        == 0
    {
        tokio::task::yield_now().await;
    }

    let metadata = tokio::time::timeout(
        Duration::from_millis(500),
        storage.stat(&RemotePath::new(0, "independent.txt").unwrap()),
    )
    .await
    .expect("an independent SMB path operation waited for the busy session")
    .unwrap();

    assert_eq!(metadata.size, 4);
    blocked.abort();
    drop(busy_session);
    first_mock.assert_fully_consumed();
    second_mock.assert_fully_consumed();
}

#[tokio::test]
async fn open_file_handles_remain_pinned_to_their_smb_session_slot() {
    let (storage, first_mock, second_mock) = storage_with_two_slots().await;
    let first_path = RemotePath::new(0, "first.txt").unwrap();
    let second_path = RemotePath::new(0, "second.txt").unwrap();
    first_mock.queue_responses(vec![
        create_response(file_id(56), 1, CreateAction::FileOpened),
        query_info_response(560_u64.to_le_bytes().to_vec()),
    ]);
    second_mock.queue_responses(vec![
        create_response(file_id(57), 1, CreateAction::FileOpened),
        query_info_response(570_u64.to_le_bytes().to_vec()),
    ]);
    let (_first, _, _) = storage
        .open_file(&first_path, libc::O_RDONLY, 0)
        .await
        .unwrap();
    let (mut second, _, _) = storage
        .open_file(&second_path, libc::O_RDONLY, 0)
        .await
        .unwrap();
    second_mock.queue_response(response(
        Command::Read,
        &ReadResponse {
            data_offset: 0x50,
            data_remaining: 0,
            flags: 0,
            data: b"x".to_vec(),
        },
    ));
    let busy_slot = storage.roots[0].slot(0).unwrap();
    let busy_session = busy_slot.lock_owned().await;
    let mut destination = tempfile::tempfile().unwrap();

    let read = tokio::time::timeout(
        Duration::from_millis(500),
        storage.read_at(&mut second, 0, 1, &mut destination),
    )
    .await
    .expect("the second handle used the busy first session slot")
    .unwrap();

    assert_eq!(read, 1);
    drop(busy_session);
    first_mock.assert_fully_consumed();
    second_mock.assert_fully_consumed();
}

#[tokio::test]
async fn resetting_an_smb_root_expires_handles_from_every_session_slot() {
    let (storage, first_mock, second_mock) = storage_with_two_slots().await;
    let first_path = RemotePath::new(0, "first.txt").unwrap();
    let second_path = RemotePath::new(0, "second.txt").unwrap();
    first_mock.queue_responses(vec![
        create_response(file_id(58), 1, CreateAction::FileOpened),
        query_info_response(580_u64.to_le_bytes().to_vec()),
    ]);
    second_mock.queue_responses(vec![
        create_response(file_id(59), 1, CreateAction::FileOpened),
        query_info_response(590_u64.to_le_bytes().to_vec()),
    ]);
    let (mut first, _, _) = storage
        .open_file(&first_path, libc::O_RDONLY, 0)
        .await
        .unwrap();
    let (mut second, _, _) = storage
        .open_file(&second_path, libc::O_RDONLY, 0)
        .await
        .unwrap();

    storage.reset(0).await;

    let mut destination = tempfile::tempfile().unwrap();
    super::assert_errno(
        storage.read_at(&mut first, 0, 1, &mut destination).await,
        libc::ESTALE,
    );
    super::assert_errno(
        storage.read_at(&mut second, 0, 1, &mut destination).await,
        libc::ESTALE,
    );
    first_mock.assert_fully_consumed();
    second_mock.assert_fully_consumed();
}

#[tokio::test]
async fn reconnecting_one_smb_slot_does_not_expire_another_slots_handle() {
    let (storage, first_mock, second_mock) = storage_with_two_slots().await;
    let first_path = RemotePath::new(0, "first.txt").unwrap();
    let second_path = RemotePath::new(0, "second.txt").unwrap();
    first_mock.queue_responses(vec![
        create_response(file_id(60), 1, CreateAction::FileOpened),
        query_info_response(600_u64.to_le_bytes().to_vec()),
    ]);
    second_mock.queue_responses(vec![
        create_response(file_id(61), 1, CreateAction::FileOpened),
        query_info_response(610_u64.to_le_bytes().to_vec()),
    ]);
    let (mut first, _, _) = storage
        .open_file(&first_path, libc::O_RDONLY, 0)
        .await
        .unwrap();
    let (mut second, _, _) = storage
        .open_file(&second_path, libc::O_RDONLY, 0)
        .await
        .unwrap();
    storage.roots[0].slots[0].lock().await.invalidate_session();
    second_mock.queue_response(response(
        Command::Read,
        &ReadResponse {
            data_offset: 0x50,
            data_remaining: 0,
            flags: 0,
            data: b"y".to_vec(),
        },
    ));
    let mut destination = tempfile::tempfile().unwrap();

    super::assert_errno(
        storage.read_at(&mut first, 0, 1, &mut destination).await,
        libc::ESTALE,
    );
    assert_eq!(
        storage
            .read_at(&mut second, 0, 1, &mut destination)
            .await
            .unwrap(),
        1
    );
    first_mock.assert_fully_consumed();
    second_mock.assert_fully_consumed();
}

#[tokio::test]
async fn namespace_operations_use_real_smb_create_delete_stat_and_rename_requests() {
    let (storage, mock) = storage("base").await;
    let source = RemotePath::new(0, "source.txt").unwrap();
    let target = RemotePath::new(0, "target.txt").unwrap();
    let source_id = file_id(60);

    mock.queue_response(compound_response(&[
        create_response(file_id(61), 0, CreateAction::FileOpened),
        query_info_response(basic_information(0x10)),
        query_info_response(standard_information(0, true)),
        close_response(0),
    ]));
    storage.connect(0).await.unwrap();

    mock.queue_responses(vec![create_response(
        source_id,
        4,
        CreateAction::FileOpened,
    )]);
    mock.queue_response(query_info_response(600_u64.to_le_bytes().to_vec()));
    mock.queue_response(close_response(4));
    let metadata = storage.stat(&source).await.unwrap();
    assert_eq!(metadata.size, 4);
    assert_eq!(metadata.identity, "file:4:102:100:103:600");

    mock.queue_responses(vec![
        create_response(file_id(62), 0, CreateAction::FileCreated),
        close_response(0),
    ]);
    storage
        .create_directory(&RemotePath::new(0, "created").unwrap())
        .await
        .unwrap();

    for (path, directory) in [
        (RemotePath::new(0, "obsolete.txt").unwrap(), false),
        (RemotePath::new(0, "obsolete-dir").unwrap(), true),
    ] {
        mock.queue_response(compound_response(&[
            create_response(file_id(63), 0, CreateAction::FileOpened),
            response(Command::SetInfo, &SetInfoResponse),
            close_response(0),
        ]));
        storage.remove(&path, directory).await.unwrap();
    }

    mock.queue_response(create_response(source_id, 4, CreateAction::FileOpened));
    mock.queue_response(query_info_response(600_u64.to_le_bytes().to_vec()));
    mock.queue_response(close_response(4));
    mock.queue_responses(vec![
        create_response(source_id, 4, CreateAction::FileOpened),
        response(Command::SetInfo, &SetInfoResponse),
        close_response(4),
    ]);
    storage.rename(&source, &target).await.unwrap();
    mock.assert_fully_consumed();
}

#[tokio::test]
async fn invalid_open_modes_and_expired_handles_fail_before_network_access() {
    let (storage, mock) = storage("").await;
    let path = RemotePath::new(0, "file.txt").unwrap();
    super::assert_errno(
        storage
            .open_file(&path, libc::O_RDONLY | libc::O_TRUNC, 0)
            .await,
        libc::EINVAL,
    );
    super::assert_errno(
        storage.open_file(&path, libc::O_ACCMODE, 0).await,
        libc::EINVAL,
    );

    mock.queue_responses(vec![
        create_response(file_id(70), 1, CreateAction::FileOpened),
        query_info_response(700_u64.to_le_bytes().to_vec()),
    ]);
    let (mut handle, _, _) = storage.open_file(&path, libc::O_RDONLY, 0).await.unwrap();
    storage.reset(0).await;
    let mut destination = File::open("/dev/null").unwrap();
    super::assert_errno(
        storage.read_at(&mut handle, 0, 1, &mut destination).await,
        libc::ESTALE,
    );
    mock.assert_fully_consumed();
}

#[tokio::test]
async fn snapshot_writeback_publishes_a_new_file_atomically_under_the_remote_lock() {
    let (storage, mock) = storage("").await;
    let path = RemotePath::new(0, "folder/new.txt").unwrap();
    let staged_id = file_id(80);
    let lock_id = file_id(81);
    mock.queue_responses(vec![
        compound_response(&[
            error_response(Command::Create, NtStatus::OBJECT_NAME_NOT_FOUND),
            error_response(Command::SetInfo, NtStatus::INVALID_PARAMETER),
            error_response(Command::Close, NtStatus::INVALID_PARAMETER),
        ]),
        create_response(staged_id, 0, CreateAction::FileCreated),
        query_info_response(800_u64.to_le_bytes().to_vec()),
        response(
            Command::Write,
            &WriteResponse {
                count: 6,
                remaining: 0,
                write_channel_info_offset: 0,
                write_channel_info_length: 0,
            },
        ),
        response(Command::SetInfo, &SetInfoResponse),
        response(Command::Flush, &FlushResponse),
        close_response(6),
        compound_response(&[
            create_response(lock_id, 0, CreateAction::FileCreated),
            response(Command::SetInfo, &SetInfoResponse),
        ]),
        error_response(Command::Create, NtStatus::OBJECT_NAME_NOT_FOUND),
        create_response(staged_id, 6, CreateAction::FileOpened),
        response(Command::SetInfo, &SetInfoResponse),
        close_response(6),
        create_response(staged_id, 6, CreateAction::FileOpened),
        query_info_response(800_u64.to_le_bytes().to_vec()),
        close_response(6),
        close_response(0),
    ]);
    let mut source = tempfile::tempfile().unwrap();
    source.write_all(b"remote").unwrap();

    let metadata = storage
        .write_from_if_unchanged(&path, None, &mut source, 6)
        .await
        .unwrap();

    assert_eq!(metadata.file_type, RemoteFileType::File);
    assert_eq!(metadata.size, 6);
    assert_eq!(metadata.identity, "file:6:102:100:103:800");
    mock.assert_fully_consumed();
}
