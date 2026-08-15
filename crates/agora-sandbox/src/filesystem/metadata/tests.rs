use super::{
    DirectoryMetadata, EntryState, FileAttributes, METADATA_VERSION, Materializer, MetadataStore,
};
use crate::filesystem::FileCipher;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

#[test]
fn metadata_store_creates_and_validates_directory_markers() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();

    assert!(root.join(".metadata").is_file());
    store.ensure_marker(Path::new("/Users/bytedance")).unwrap();
    assert!(root.join("Users/bytedance/.metadata").is_file());
    assert!(store.has_marker(Path::new("/Users/bytedance")).unwrap());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_store_rejects_oversized_control_files_before_reading_them() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(root.join(".metadata"))
        .unwrap()
        .set_len((super::MAX_DIRECTORY_METADATA_BYTES + 1) as u64)
        .unwrap();
    store.invalidate().unwrap();

    let error = store.state(Path::new("/entry")).unwrap_err();

    assert!(error.to_string().contains("metadata exceeds"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_store_rejects_a_directory_as_a_marker() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let directory = Path::new("/project");
    store.ensure_marker(directory).unwrap();
    let marker = store.path(directory).unwrap();
    std::fs::remove_file(&marker).unwrap();
    std::fs::create_dir(&marker).unwrap();

    assert!(store.ensure_marker(directory).is_err());
    assert!(store.has_marker(directory).is_err());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_store_reports_an_unreadable_marker() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let directory = Path::new("/restricted");
    store.ensure_marker(directory).unwrap();
    let marker = store.path(directory).unwrap();
    std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o0)).unwrap();
    store.invalidate().unwrap();

    let error = store.state(&directory.join("entry")).unwrap_err();

    std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        error
            .to_string()
            .contains("failed to read filesystem metadata")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_store_reports_an_inaccessible_marker_ancestor() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let blocked = root.join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o0)).unwrap();
    let directory = Path::new("/blocked/child");

    let ensure_error = store.ensure_marker(directory).unwrap_err();
    let marker_error = store.has_marker(directory).unwrap_err();

    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        ensure_error
            .to_string()
            .contains("failed to inspect filesystem metadata")
    );
    assert!(
        marker_error
            .to_string()
            .contains("failed to inspect filesystem metadata")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_cache_evicts_old_missing_directories_at_its_capacity() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let empty_marker = std::fs::read(root.join(".metadata")).unwrap();
    for index in 0..=super::METADATA_CACHE_CAPACITY {
        let backing = root.join(format!("directory-{index}"));
        std::fs::create_dir(&backing).unwrap();
        std::fs::write(backing.join(".metadata"), &empty_marker).unwrap();
        let path = std::path::PathBuf::from(format!("/directory-{index}/entry"));
        assert_eq!(store.state(&path).unwrap(), None);
    }
    let probes = store.probe_count();

    assert_eq!(store.state(Path::new("/directory-0/entry")).unwrap(), None);
    assert_eq!(store.probe_count(), probes + 1);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cached_marker_removal_is_observed_without_a_sandbox_publication() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let directory = Path::new("/Users/bytedance");
    let entry = directory.join("entry");
    store.ensure_marker(directory).unwrap();
    store.set(&entry, EntryState::Cow).unwrap();

    assert_eq!(store.state(&entry).unwrap(), Some(EntryState::Cow));
    std::fs::remove_file(root.join("Users/bytedance/.metadata")).unwrap();
    assert!(!store.has_marker(directory).unwrap());
    assert_eq!(store.state(&entry).unwrap(), None);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_round_trips_cached_cow_and_whiteout_states() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let cached = Path::new("/tmp/cached");
    let cow = Path::new("/tmp/cow");
    let whiteout = Path::new("/tmp/whiteout");

    store
        .set(
            cached,
            EntryState::Cached {
                checksum: Some("d41d8cd98f00b204e9800998ecf8427e".to_string()),
                materializer: Materializer::Copy,
                source: None,
                variant: None,
                destination: None,
            },
        )
        .unwrap();
    store.set(cow, EntryState::Cow).unwrap();
    store.set(whiteout, EntryState::Whiteout).unwrap();

    assert!(matches!(
        store.state(cached).unwrap(),
        Some(EntryState::Cached { .. })
    ));
    assert_eq!(store.state(cow).unwrap(), Some(EntryState::Cow));
    assert_eq!(store.state(whiteout).unwrap(), Some(EntryState::Whiteout));
    let attributes = FileAttributes::created_file(0o640);
    store
        .set_with_attributes(cow, EntryState::Cow, Some(attributes.clone()))
        .unwrap();
    assert_eq!(store.attributes(cow).unwrap(), Some(attributes));
    assert_eq!(store.entries(Path::new("/tmp")).unwrap().len(), 3);

    store.remove(cached).unwrap();
    assert_eq!(store.state(cached).unwrap(), None);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_reads_multiple_records_from_one_generation_snapshot() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let cow = Path::new("/tmp/cow");
    let whiteout = Path::new("/tmp/whiteout");
    let missing = Path::new("/tmp/missing");
    let attributes = FileAttributes::created_file(0o640);
    store
        .set_with_attributes(cow, EntryState::Cow, Some(attributes.clone()))
        .unwrap();
    store.set(whiteout, EntryState::Whiteout).unwrap();

    assert_eq!(
        store.records(&[cow, whiteout, missing]).unwrap(),
        vec![
            (Some(EntryState::Cow), Some(attributes)),
            (Some(EntryState::Whiteout), None),
            (None, None),
        ]
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn plain_metadata_serializes_readable_utf8_names_in_version_three_records() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let directory = Path::new("/tmp");
    let path = directory.join("bash");

    store.set(&path, EntryState::Cow).unwrap();
    store
        .set_attributes(&path, FileAttributes::created_file(0o755))
        .unwrap();

    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store.path(directory).unwrap()).unwrap()).unwrap();
    assert_eq!(metadata["version"], 3);
    assert!(metadata.get("backing_names").is_none());
    assert!(metadata.get("attributes").is_none());
    assert!(metadata["entries"].get("bash").is_some());
    assert!(metadata["entries"]["bash"].get("name").is_none());
    assert!(metadata["entries"]["bash"].get("entry").is_some());
    assert!(metadata["entries"]["bash"].get("attributes").is_some());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_metadata_uses_the_physical_name_as_an_opaque_record_key() {
    let root = tempfile();
    let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
    let store = MetadataStore::encrypted(&root, cipher.clone()).unwrap();
    let directory = Path::new("/tmp");
    let path = directory.join("安全方案.docx");
    let attributes = FileAttributes::created_file(0o600);

    let physical_name = store.ensure_encrypted_name(&path).unwrap();
    store
        .set_with_attributes(&path, EntryState::Cow, Some(attributes.clone()))
        .unwrap();
    let deleted = directory.join("已删除.txt");
    store.set_whiteout(&deleted, true).unwrap();
    let deleted_physical_name = store.encrypted_name(&deleted).unwrap().unwrap();

    let contents = std::fs::read(store.path(directory).unwrap()).unwrap();
    assert!(
        !contents
            .windows("安全方案.docx".len())
            .any(|window| { window == "安全方案.docx".as_bytes() })
    );
    assert!(
        !contents
            .windows("已删除.txt".len())
            .any(|window| window == "已删除.txt".as_bytes())
    );
    let metadata: serde_json::Value = serde_json::from_slice(&contents).unwrap();
    assert_eq!(metadata["version"], 3);
    assert!(metadata.get("backing_names").is_none());
    assert!(metadata.get("attributes").is_none());
    let physical_name = physical_name.to_str().unwrap();
    let record = &metadata["entries"][physical_name];
    assert!(record.get("name").is_none());
    assert_eq!(record["entry"]["state"], "cow");
    assert!(record["attributes"].is_object());
    assert_eq!(
        metadata["entries"][deleted_physical_name.to_str().unwrap()]["entry"]["state"],
        "whiteout"
    );

    drop(store);
    assert_eq!(
        cipher.decrypt_name(physical_name).unwrap(),
        path.file_name().unwrap().as_bytes()
    );
    let reopened = MetadataStore::encrypted(&root, cipher).unwrap();
    assert_eq!(reopened.state(&path).unwrap(), Some(EntryState::Cow));
    assert_eq!(
        reopened.state(&deleted).unwrap(),
        Some(EntryState::Whiteout)
    );
    assert_eq!(reopened.attributes(&path).unwrap(), Some(attributes));
    assert_eq!(
        reopened.encrypted_name(&path).unwrap().as_deref(),
        Some(std::ffi::OsStr::new(physical_name))
    );
    assert_eq!(
        reopened.entries(directory).unwrap()[0].0,
        path.file_name().unwrap()
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn unchanged_metadata_is_parsed_once() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let path = Path::new("/tmp/cached");
    store.set(path, EntryState::Cow).unwrap();

    assert_eq!(store.state(path).unwrap(), Some(EntryState::Cow));
    let parsed = store.parse_count();
    assert_eq!(store.state(path).unwrap(), Some(EntryState::Cow));
    assert_eq!(store.parse_count(), parsed);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn consecutive_whiteouts_reuse_the_writer_metadata_cache() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let directory = Path::new("/tmp/cache-after-write");
    let first = directory.join("first");
    let second = directory.join("second");
    store.ensure_marker(directory).unwrap();
    assert!(store.entries(directory).unwrap().is_empty());
    let parsed = store.parse_count();

    store.set_whiteout(&first, false).unwrap();
    store.set_whiteout(&second, false).unwrap();

    assert_eq!(store.parse_count(), parsed);
    assert_eq!(store.state(&first).unwrap(), Some(EntryState::Whiteout));
    assert_eq!(store.state(&second).unwrap(), Some(EntryState::Whiteout));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_updates_reuse_the_existing_marker_inode() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let marker = root.join(".metadata");
    let inode = marker.metadata().unwrap().ino();

    store.set(Path::new("/first"), EntryState::Cow).unwrap();
    store
        .set(Path::new("/second"), EntryState::Whiteout)
        .unwrap();

    assert_eq!(marker.metadata().unwrap().ino(), inode);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn new_whiteouts_append_without_rewriting_existing_records() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let marker = root.join(".metadata");

    store.set_whiteout(Path::new("/z-last"), false).unwrap();
    let first = std::fs::read(&marker).unwrap();
    let reader = MetadataStore::new(&root).unwrap();
    assert_eq!(
        reader.state(Path::new("/z-last")).unwrap(),
        Some(EntryState::Whiteout)
    );
    store.set_whiteout(Path::new("/a-first"), false).unwrap();
    let second = std::fs::read(&marker).unwrap();

    let retained = first
        .strip_suffix(b"\n  }\n}" as &[u8])
        .expect("version-three metadata suffix");
    assert!(second.starts_with(retained));
    assert!(
        second
            .windows(b"\"a-first\"".len())
            .any(|bytes| bytes == b"\"a-first\"")
    );
    assert_eq!(
        store.state(Path::new("/z-last")).unwrap(),
        Some(EntryState::Whiteout)
    );
    assert_eq!(
        store.state(Path::new("/a-first")).unwrap(),
        Some(EntryState::Whiteout)
    );
    assert_eq!(
        reader.state(Path::new("/a-first")).unwrap(),
        Some(EntryState::Whiteout)
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn unchanged_missing_metadata_is_probed_once() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let path = Path::new("/tmp/missing");

    assert_eq!(store.state(path).unwrap(), None);
    assert_eq!(store.state(path).unwrap(), None);
    assert_eq!(store.probe_count(), 1);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_generation_invalidates_another_store_cache() {
    let root = tempfile();
    let first = MetadataStore::new(&root).unwrap();
    let second = MetadataStore::new(&root).unwrap();
    let path = Path::new("/tmp/shared");

    assert_eq!(first.state(path).unwrap(), None);
    second.set(path, EntryState::Cow).unwrap();
    assert_eq!(first.state(path).unwrap(), Some(EntryState::Cow));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn unchanged_attributes_do_not_rewrite_metadata() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let directory = Path::new("/tmp");
    let path = directory.join("cached");
    let attributes = FileAttributes::created_file(0o640);
    store.set_attributes(&path, attributes.clone()).unwrap();
    let metadata_path = store.path(directory).unwrap();
    let identity = std::fs::metadata(&metadata_path).unwrap().ino();

    store.set_attributes(&path, attributes).unwrap();

    assert_eq!(std::fs::metadata(metadata_path).unwrap().ino(), identity);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn created_attributes_use_the_effective_identity() {
    let attributes = FileAttributes::created_file(0o640);

    assert_eq!(attributes.uid, unsafe { libc::geteuid() });
    assert_eq!(attributes.gid, unsafe { libc::getegid() });
}

#[test]
fn metadata_root_attributes_and_cached_authority_are_explicit() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let attributes = FileAttributes::created_file(0o640);

    assert_eq!(store.attributes(Path::new("/")).unwrap(), None);
    store
        .set_attributes(Path::new("/"), attributes.clone())
        .unwrap();
    assert_eq!(store.attributes(Path::new("/")).unwrap(), None);
    assert!(store.ensure_encrypted_name(Path::new("/tmp/file")).is_err());
    assert!(MetadataStore::storage_name("*").is_err());
    assert!(MetadataStore::decode("*").is_err());

    let source_file = root.join("source");
    std::fs::write(&source_file, b"content").unwrap();
    let source = super::SourceIdentity::from_metadata(&source_file.metadata().unwrap());
    let materialized = FileAttributes::from_metadata(&source_file.metadata().unwrap());
    let cached = EntryState::Cached {
        checksum: None,
        materializer: Materializer::Copy,
        source: Some(source),
        variant: None,
        destination: None,
    };
    assert!(!cached.stored_attributes_are_authoritative(&materialized));
    let mut changed = materialized;
    changed.mode ^= 0o100;
    assert!(cached.stored_attributes_are_authoritative(&changed));
    assert!(EntryState::Cow.stored_attributes_are_authoritative(&attributes));
    assert!(
        !EntryState::Cached {
            checksum: None,
            materializer: Materializer::Copy,
            source: None,
            variant: None,
            destination: None,
        }
        .stored_attributes_are_authoritative(&attributes)
    );
    assert!(!EntryState::Whiteout.stored_attributes_are_authoritative(&attributes));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_is_stored_next_to_its_mirrored_directory() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();

    assert_eq!(
        store.path(Path::new("/usr/bin")).unwrap(),
        root.join("usr/bin/.metadata")
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_supports_non_utf8_names() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let directory = Path::new("/tmp");
    let path = directory.join(std::ffi::OsString::from_vec(vec![b'f', 0x80]));

    store.set(&path, EntryState::Cow).unwrap();

    assert_eq!(store.state(&path).unwrap(), Some(EntryState::Cow));
    assert_eq!(
        store.entries(directory).unwrap()[0].0,
        path.file_name().unwrap()
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store.path(directory).unwrap()).unwrap()).unwrap();
    assert!(metadata["entries"].get("base64:ZoA").is_some());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_escapes_names_that_begin_with_the_reserved_prefix() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let directory = Path::new("/tmp");
    let path = directory.join("base64:literal");

    store.set(&path, EntryState::Cow).unwrap();

    assert_eq!(store.state(&path).unwrap(), Some(EntryState::Cow));
    assert_eq!(
        store.entries(directory).unwrap()[0].0,
        path.file_name().unwrap()
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store.path(directory).unwrap()).unwrap()).unwrap();
    assert!(
        metadata["entries"]
            .get("base64:YmFzZTY0OmxpdGVyYWw")
            .is_some()
    );
    assert!(metadata["entries"].get("base64:literal").is_none());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn plain_metadata_escapes_names_that_look_like_encrypted_ciphertext() {
    let root = tempfile();
    let directory = Path::new("/tmp");
    let path = directory.join("enc_literal");
    let store = MetadataStore::new(&root).unwrap();

    store.set(&path, EntryState::Cow).unwrap();

    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store.path(directory).unwrap()).unwrap()).unwrap();
    assert!(metadata["entries"].get("enc_literal").is_none());
    assert!(metadata["entries"].get("base64:ZW5jX2xpdGVyYWw").is_some());
    drop(store);

    let reopened = MetadataStore::new(&root).unwrap();
    assert_eq!(reopened.state(&path).unwrap(), Some(EntryState::Cow));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_rejects_invalid_paths_and_records() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    assert!(store.state(Path::new("relative")).is_err());
    assert!(store.entries(Path::new("relative")).is_err());

    let path = store.path(Path::new("/tmp")).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not json").unwrap();
    assert!(
        store
            .entries(Path::new("/tmp"))
            .unwrap_err()
            .to_string()
            .contains("parse")
    );

    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": METADATA_VERSION + 1,
            "entries": {},
            "attributes": {},
            "backing_names": {}
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(
        store
            .entries(Path::new("/tmp"))
            .unwrap_err()
            .to_string()
            .contains("unsupported")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_rejects_invalid_version_three_records_and_encrypted_names() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let directory = Path::new("/tmp");
    let path = store.path(directory).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": METADATA_VERSION,
            "entries": {"empty": {}}
        }))
        .unwrap(),
    )
    .unwrap();
    store.invalidate().unwrap();
    assert!(
        store
            .entries(directory)
            .unwrap_err()
            .to_string()
            .contains("empty")
    );

    let encrypted_prefix = super::super::crypto::ENCRYPTED_NAME_PREFIX;
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": METADATA_VERSION,
            "entries": {format!("{encrypted_prefix}invalid"): {"entry": {"state": "cow"}}}
        }))
        .unwrap(),
    )
    .unwrap();
    store.invalidate().unwrap();
    assert!(
        store
            .entries(directory)
            .unwrap_err()
            .to_string()
            .contains("requires a cipher")
    );

    std::fs::remove_dir_all(root).unwrap();

    let root = tempfile();
    let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
    let store = MetadataStore::encrypted(&root, cipher.clone()).unwrap();
    let path = store.path(directory).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": METADATA_VERSION,
            "entries": {format!("{encrypted_prefix}invalid"): {"entry": {"state": "cow"}}}
        }))
        .unwrap(),
    )
    .unwrap();
    store.invalidate().unwrap();
    assert!(
        store
            .entries(directory)
            .unwrap_err()
            .to_string()
            .contains("failed to decrypt")
    );

    let invalid_logical = cipher.encrypt_name(b".").unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": METADATA_VERSION,
            "entries": {invalid_logical: {"entry": {"state": "cow"}}}
        }))
        .unwrap(),
    )
    .unwrap();
    store.invalidate().unwrap();
    assert!(
        store
            .entries(directory)
            .unwrap_err()
            .to_string()
            .contains("invalid encrypted filesystem metadata name")
    );

    let encrypted_file = cipher.encrypt_name(b"file").unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": METADATA_VERSION,
            "entries": {
                "file": {"entry": {"state": "cow"}},
                encrypted_file: {"entry": {"state": "whiteout"}}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    store.invalidate().unwrap();
    assert!(
        store
            .entries(directory)
            .unwrap_err()
            .to_string()
            .contains("duplicate")
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_rejects_invalid_serialization_generation_and_legacy_aliases() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let logical_a = MetadataStore::encode(std::ffi::OsStr::new("a"));
    let logical_b = MetadataStore::encode(std::ffi::OsStr::new("b"));
    let encrypted_name = format!("{}duplicate", super::super::crypto::ENCRYPTED_NAME_PREFIX);

    let mut metadata = DirectoryMetadata::default();
    metadata
        .encrypted_names
        .insert(logical_a.clone(), "invalid".to_string());
    assert!(
        store
            .serialize_metadata(&metadata)
            .unwrap_err()
            .to_string()
            .contains("invalid encrypted filesystem filename")
    );

    metadata.encrypted_names.clear();
    metadata
        .encrypted_names
        .insert(logical_a, encrypted_name.clone());
    metadata.encrypted_names.insert(logical_b, encrypted_name);
    assert!(
        store
            .serialize_metadata(&metadata)
            .unwrap_err()
            .to_string()
            .contains("duplicate filesystem metadata record")
    );

    let directory = Path::new("/legacy");
    let path = store.path(directory).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let backing = "0123456789abcdef0123456789abcdef";
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "entries": {},
            "attributes": {},
            "backing_names": {"a": backing, "b": backing}
        }))
        .unwrap(),
    )
    .unwrap();
    store.invalidate().unwrap();
    assert!(
        store
            .entries(directory)
            .unwrap_err()
            .to_string()
            .contains("invalid filesystem backing name")
    );

    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "entries": {"Y2F0": {"state": "cow"}},
            "attributes": {},
            "backing_names": {}
        }))
        .unwrap(),
    )
    .unwrap();
    store.invalidate().unwrap();
    assert_eq!(
        store.entries(directory).unwrap(),
        vec![(std::ffi::OsString::from("cat"), EntryState::Cow)]
    );

    store.generation.set_len(0).unwrap();
    assert!(
        store
            .state(Path::new("/missing"))
            .unwrap_err()
            .to_string()
            .contains("generation is incomplete")
    );
    std::fs::remove_dir_all(root).unwrap();

    let root = tempfile();
    let generation_path = root.join("generation");
    std::fs::write(&generation_path, b"invalid").unwrap();
    let generation = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&generation_path)
        .unwrap();
    assert!(
        MetadataStore::with_generation(&root, generation, None)
            .err()
            .unwrap()
            .to_string()
            .contains("generation is invalid")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_handles_root_entries_and_reports_storage_failures() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();

    assert_eq!(store.state(Path::new("/")).unwrap(), None);
    store.remove(Path::new("/")).unwrap();

    let path = store.path(Path::new("/unreadable")).unwrap();
    std::fs::create_dir_all(&path).unwrap();
    assert!(
        store
            .entries(Path::new("/unreadable"))
            .unwrap_err()
            .to_string()
            .contains("failed to read")
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_rejects_invalid_encoded_names() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let path = store.path(Path::new("/tmp")).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "entries": {"base64:*": {"state": "cow"}},
            "attributes": {},
            "backing_names": {}
        }))
        .unwrap(),
    )
    .unwrap();

    assert!(
        store
            .entries(Path::new("/tmp"))
            .unwrap_err()
            .to_string()
            .contains("invalid encoded")
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_rejects_duplicate_decoded_names() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let path = store.path(Path::new("/tmp")).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "entries": {
                "cat": {"state": "cow"},
                "base64:Y2F0": {"state": "whiteout"}
            },
            "attributes": {},
            "backing_names": {}
        }))
        .unwrap(),
    )
    .unwrap();

    assert!(
        store
            .entries(Path::new("/tmp"))
            .unwrap_err()
            .to_string()
            .contains("duplicate")
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_rejects_invalid_legacy_backing_names() {
    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let path = store.path(Path::new("/tmp")).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "entries": {},
            "attributes": {},
            "backing_names": {"file": "../../outside"}
        }))
        .unwrap(),
    )
    .unwrap();

    assert!(
        store
            .encrypted_name(Path::new("/tmp/file"))
            .unwrap_err()
            .to_string()
            .contains("invalid filesystem backing name")
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_store_rejects_a_file_as_its_root() {
    let root = tempfile();
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::write(&root, b"blocked").unwrap();

    assert!(
        MetadataStore::new(&root)
            .err()
            .unwrap()
            .to_string()
            .contains("failed to create")
    );

    std::fs::remove_file(root).unwrap();
}

#[test]
fn metadata_creation_reports_an_unwritable_parent() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile();
    let store = MetadataStore::new(&root).unwrap();
    let directory = Path::new("/blocked-write");
    let path = store.path(directory).unwrap();
    let parent = path.parent().unwrap();
    std::fs::create_dir_all(parent).unwrap();
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o500)).unwrap();

    assert!(
        store
            .set(&directory.join("entry"), EntryState::Cow)
            .unwrap_err()
            .to_string()
            .contains("failed to create filesystem metadata")
    );

    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

fn tempfile() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("agora-metadata-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}
