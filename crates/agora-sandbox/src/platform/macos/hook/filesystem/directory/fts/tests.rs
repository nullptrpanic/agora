use super::*;
use std::ffi::{CStr, CString, OsStr};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, symlink};
use std::os::unix::net::UnixListener;

fn stream_state(mappings: Vec<FtsRootMapping>) -> FtsStreamState {
    FtsStreamState {
        compare: None,
        mappings,
        presented: Vec::new(),
        traversal_paths: Vec::new(),
        anchors: Vec::new(),
    }
}

fn mapping(physical: &str, logical: &str, resolved: &str) -> FtsRootMapping {
    FtsRootMapping {
        physical: physical.as_bytes().to_vec(),
        logical: logical.as_bytes().to_vec(),
        resolved: resolved.as_bytes().to_vec(),
    }
}

unsafe extern "C" fn compare_entry_names(
    left: *const *const DarwinFtsEntry,
    right: *const *const DarwinFtsEntry,
) -> libc::c_int {
    let left = unsafe { CStr::from_ptr((**left).fts_name.as_ptr()) }.to_bytes();
    let right = unsafe { CStr::from_ptr((**right).fts_name.as_ptr()) }.to_bytes();
    match left.cmp(right) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

struct TestEntry {
    storage: Box<[u64; 64]>,
    path: CString,
    access_path: CString,
}

impl TestEntry {
    fn new(path: &str, access_path: &str, name: &str) -> Self {
        let mut storage = Box::new([0_u64; 64]);
        let path = CString::new(path).unwrap();
        let access_path = CString::new(access_path).unwrap();
        let name = CString::new(name).unwrap();
        assert!(size_of::<DarwinFtsEntry>() + name.as_bytes().len() < size_of::<[u64; 64]>());
        let entry = storage.as_mut_ptr().cast::<DarwinFtsEntry>();
        unsafe {
            (*entry).fts_path = path.as_ptr().cast_mut();
            (*entry).fts_accpath = access_path.as_ptr().cast_mut();
            (*entry).fts_pathlen = u16::try_from(path.as_bytes().len()).unwrap();
            (*entry).fts_namelen = u16::try_from(name.as_bytes().len()).unwrap();
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                (*entry).fts_name.as_mut_ptr(),
                name.as_bytes_with_nul().len(),
            );
        }
        Self {
            storage,
            path,
            access_path,
        }
    }

    fn pointer(&mut self) -> *mut DarwinFtsEntry {
        self.storage.as_mut_ptr().cast()
    }

    fn presented_path(&mut self) -> Vec<u8> {
        unsafe {
            CStr::from_ptr((*self.pointer()).fts_path)
                .to_bytes()
                .to_vec()
        }
    }

    fn presented_access_path(&mut self) -> Vec<u8> {
        unsafe {
            CStr::from_ptr((*self.pointer()).fts_accpath)
                .to_bytes()
                .to_vec()
        }
    }

    fn presented_name(&mut self) -> Vec<u8> {
        unsafe {
            CStr::from_ptr((*self.pointer()).fts_name.as_ptr())
                .to_bytes()
                .to_vec()
        }
    }
}

#[test]
fn virtual_bulk_restores_nested_active_streams() {
    assert!(!FtsVirtualBulk::is_active());
    let mut outer_marker = 0_u8;
    let outer_stream = (&mut outer_marker as *mut u8).cast();
    let outer = FtsVirtualBulk::enter(outer_stream);
    assert_eq!(FtsVirtualBulk::active_stream(), Some(outer_stream as usize));

    let mut inner_marker = 0_u8;
    let inner_stream = (&mut inner_marker as *mut u8).cast();
    {
        let _inner = FtsVirtualBulk::enter(inner_stream);
        assert_eq!(FtsVirtualBulk::active_stream(), Some(inner_stream as usize));
    }
    assert_eq!(FtsVirtualBulk::active_stream(), Some(outer_stream as usize));
    drop(outer);
    assert!(!FtsVirtualBulk::is_active());
}

#[test]
fn stream_translation_requires_a_component_boundary() {
    let state = stream_state(vec![
        mapping("/physical", "/logical", "/resolved"),
        mapping("/physical/nested", "/logical/nested", "/resolved/nested"),
    ]);

    assert_eq!(state.translate(b"/physical"), Some(b"/logical".to_vec()));
    assert_eq!(
        state.translate(b"/physical/nested/file"),
        Some(b"/logical/nested/file".to_vec())
    );
    assert_eq!(
        state.resolve(b"/physical/nested/file"),
        Some(b"/resolved/nested/file".to_vec())
    );
    assert_eq!(state.translate(b"/physicality"), None);
    assert_eq!(state.translate(b"/elsewhere"), None);
}

#[test]
fn stream_presents_logical_paths_and_restores_native_entry_storage() {
    let mut entry = TestEntry::new(
        "/physical/encoded-name",
        "/physical/encoded-name",
        "encoded-name",
    );
    let original_path = entry.path.as_ptr();
    let original_access_path = entry.access_path.as_ptr();
    let mut state = stream_state(vec![mapping(
        "/physical/encoded-name",
        "/logical/name",
        "/resolved/name",
    )]);

    state.present(entry.pointer()).unwrap();

    assert_eq!(entry.presented_path(), b"/logical/name");
    assert_eq!(entry.presented_access_path(), b"/logical/name");
    assert_eq!(entry.presented_name(), b"name");
    assert_eq!(state.presented.len(), 1);

    state.restore();
    assert_eq!(
        unsafe { (*entry.pointer()).fts_path },
        original_path.cast_mut()
    );
    assert_eq!(
        unsafe { (*entry.pointer()).fts_accpath },
        original_access_path.cast_mut()
    );
    assert_eq!(entry.presented_name(), b"encoded-name");
    assert!(state.presented.is_empty());
}

#[test]
fn stream_presentation_uses_access_path_fallback_and_rejects_longer_names() {
    let mut fallback = TestEntry::new(
        "/unmapped/encoded-name",
        "/physical/encoded-name",
        "encoded-name",
    );
    let mut state = stream_state(vec![mapping(
        "/physical/encoded-name",
        "/logical/name",
        "/resolved/name",
    )]);
    state.present(fallback.pointer()).unwrap();
    assert_eq!(fallback.presented_path(), b"/logical/name");
    state.restore();

    let mut too_short = TestEntry::new("/physical/x", "/physical/x", "x");
    let mut state = stream_state(vec![mapping(
        "/physical/x",
        "/logical/long-name",
        "/resolved/long-name",
    )]);
    let error = state.present(too_short.pointer()).unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .and_then(|error| error.raw_os_error()),
        Some(libc::ENAMETOOLONG)
    );

    let mut untouched = TestEntry::new("/native/file", "/native/file", "file");
    let mut state = stream_state(Vec::new());
    state.present(std::ptr::null_mut()).unwrap();
    state.present(untouched.pointer()).unwrap();
    assert!(state.presented.is_empty());
}

#[test]
fn fts_comparator_observes_logical_names() {
    let mut left = TestEntry::new("/physical/encoded-a", "/physical/encoded-a", "encoded-a");
    let mut right = TestEntry::new("/physical/encoded-b", "/physical/encoded-b", "encoded-b");
    let mappings = vec![
        mapping("/physical/encoded-a", "/logical/z", "/logical/z"),
        mapping("/physical/encoded-b", "/logical/a", "/logical/a"),
    ];
    let _guard = FtsCompareGuard::enter(Some(compare_entry_names), &mappings).unwrap();
    let left = left.pointer().cast_const();
    let right = right.pointer().cast_const();

    assert_eq!(unsafe { compare_entry_names(&left, &right) }, -1);
    assert_eq!(unsafe { logical_fts_compare(&left, &right) }, 1);
}

#[test]
fn stream_presents_and_restores_linked_entry_lists() {
    let mut first = TestEntry::new("/physical/one", "/physical/one", "one");
    let mut second = TestEntry::new("/physical/two", "/physical/two", "two");
    unsafe { (*first.pointer()).fts_link = second.pointer() };
    let mut state = stream_state(vec![
        mapping("/physical/one", "/logical/a", "/resolved/a"),
        mapping("/physical/two", "/logical/b", "/resolved/b"),
    ]);

    state.present_list(first.pointer()).unwrap();
    assert_eq!(first.presented_name(), b"a");
    assert_eq!(second.presented_name(), b"b");
    assert_eq!(state.presented.len(), 2);
    state.restore();
    assert_eq!(first.presented_name(), b"one");
    assert_eq!(second.presented_name(), b"two");
}

#[test]
fn directory_retargeting_reuses_traversal_storage_and_adds_a_mapping() {
    let mut entry = TestEntry::new("/physical/root/child", "/physical/root/child", "child");
    let mut state = stream_state(vec![mapping(
        "/physical/root",
        "/logical/root",
        "/resolved/root",
    )]);

    state
        .retarget_directory(
            entry.pointer(),
            CString::new("/anchor/backing").unwrap(),
            Path::new("/logical/root/child"),
        )
        .unwrap();

    assert_eq!(entry.presented_access_path(), b"/anchor/backing");
    assert_eq!(state.traversal_paths.len(), 1);
    assert_eq!(state.mappings.len(), 2);
    assert_eq!(
        state.translate(b"/anchor/backing/nested"),
        Some(b"/logical/root/child/nested".to_vec())
    );
    state
        .retarget_directory(
            entry.pointer(),
            CString::new("/anchor/backing").unwrap(),
            Path::new("/logical/root/child"),
        )
        .unwrap();
    assert_eq!(state.traversal_paths.len(), 1);
    assert_eq!(state.mappings.len(), 2);
}

#[test]
fn active_stream_mapping_registration_is_idempotent() {
    register_active_fts_mapping(Path::new("/inactive"), Path::new("/logical"));

    let mut marker = 0_u8;
    let stream = (&mut marker as *mut u8).cast::<libc::c_void>();
    let _active = FtsVirtualBulk::enter(stream);
    register_active_fts_mapping(Path::new("/missing"), Path::new("/logical"));
    lock(fts_streams()).insert(stream as usize, stream_state(Vec::new()));

    register_active_fts_mapping(Path::new("/physical"), Path::new("/logical"));
    register_active_fts_mapping(Path::new("/physical"), Path::new("/logical"));
    assert_eq!(lock(fts_streams())[&(stream as usize)].mappings.len(), 1);
    assert_eq!(
        active_fts_logical_path(Path::new("/physical/child")),
        Some(PathBuf::from("/logical/child"))
    );

    lock(fts_streams()).remove(&(stream as usize));
    assert_eq!(active_fts_logical_path(Path::new("/physical")), None);
}

#[test]
fn stream_wrappers_ignore_unknown_streams_and_restore_known_entries() {
    let mut marker = 0_u8;
    let stream = (&mut marker as *mut u8).cast::<libc::c_void>();
    let mut entry = TestEntry::new("/physical/name", "/physical/name", "name");
    restore_fts_stream(stream);
    present_fts_entry(stream, entry.pointer()).unwrap();
    present_fts_list(stream, entry.pointer()).unwrap();

    lock(fts_streams()).insert(
        stream as usize,
        stream_state(vec![mapping("/physical/name", "/logical/n", "/resolved/n")]),
    );
    present_fts_entry(stream, entry.pointer()).unwrap();
    assert_eq!(entry.presented_name(), b"n");
    restore_fts_stream(stream);
    assert_eq!(entry.presented_name(), b"name");
    lock(fts_streams()).remove(&(stream as usize));
}

#[test]
fn logical_basename_handles_roots_trailing_slashes_and_plain_names() {
    assert_eq!(logical_basename(b"/parent/name"), b"name");
    assert_eq!(logical_basename(b"/parent/name/"), b"name");
    assert_eq!(logical_basename(b"name"), b"name");
    assert_eq!(logical_basename(b"/"), b"");
}

#[test]
fn darwin_object_types_cover_regular_and_special_files() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("file");
    let directory = root.path().join("directory");
    let link = root.path().join("link");
    let socket = root.path().join("socket");
    let fifo = root.path().join("fifo");
    std::fs::write(&file, b"data").unwrap();
    std::fs::create_dir(&directory).unwrap();
    symlink(&file, &link).unwrap();
    let _listener = UnixListener::bind(&socket).unwrap();
    let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);

    assert_eq!(darwin_object_type(&file.metadata().unwrap().file_type()), 1);
    assert_eq!(
        darwin_object_type(&directory.metadata().unwrap().file_type()),
        DARWIN_VNODE_TYPE_DIRECTORY
    );
    assert_eq!(
        darwin_object_type(&link.symlink_metadata().unwrap().file_type()),
        5
    );
    assert_eq!(
        darwin_object_type(&socket.symlink_metadata().unwrap().file_type()),
        6
    );
    assert_eq!(
        darwin_object_type(&fifo.symlink_metadata().unwrap().file_type()),
        7
    );
    assert_eq!(
        darwin_object_type(&Path::new("/dev/null").metadata().unwrap().file_type()),
        4
    );
}

#[test]
fn descriptor_identity_reports_native_identity_and_bad_descriptors() {
    let file = tempfile::tempfile().unwrap();
    let identity = fts_descriptor_identity(file.as_raw_fd()).unwrap();
    let metadata = file.metadata().unwrap();
    assert_eq!(identity.device, metadata.dev() as libc::dev_t);
    assert_eq!(identity.inode, metadata.ino() as libc::ino_t);

    let error = fts_descriptor_identity(-1).unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .and_then(|error| error.raw_os_error()),
        Some(libc::EBADF)
    );
}

fn supported_attributes() -> libc::attrlist {
    let mut attributes = unsafe { std::mem::zeroed::<libc::attrlist>() };
    attributes.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    attributes.commonattr =
        libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE;
    attributes
}

#[test]
fn bulk_attribute_records_encode_compact_and_full_layouts() {
    let entry = FtsBulkEntry {
        name: b"abc".to_vec(),
        object_type: DARWIN_VNODE_TYPE_DIRECTORY,
    };
    let compact = fts_attr_record(&entry, &supported_attributes()).unwrap();
    assert_eq!(compact.len(), 60);
    assert_eq!(u32::from_ne_bytes(compact[0..4].try_into().unwrap()), 60);
    assert_eq!(i32::from_ne_bytes(compact[24..28].try_into().unwrap()), 32);
    assert_eq!(u32::from_ne_bytes(compact[28..32].try_into().unwrap()), 4);
    assert_eq!(
        u32::from_ne_bytes(compact[36..40].try_into().unwrap()),
        DARWIN_VNODE_TYPE_DIRECTORY
    );
    assert_eq!(&compact[56..59], b"abc");

    let mut full_attributes = supported_attributes();
    full_attributes.commonattr |= libc::ATTR_CMN_CRTIME;
    let full = fts_attr_record(&entry, &full_attributes).unwrap();
    assert_eq!(full.len(), 160);
    assert_eq!(i32::from_ne_bytes(full[24..28].try_into().unwrap()), 132);
    assert_eq!(&full[156..159], b"abc");
}

#[test]
fn bulk_attribute_support_rejects_unsupported_bitmaps() {
    let supported = supported_attributes();
    assert!(fts_attributes_supported(&supported));

    let mut attributes = supported;
    attributes.bitmapcount = 0;
    assert!(!fts_attributes_supported(&attributes));
    let mut attributes = supported;
    attributes.commonattr &= !libc::ATTR_CMN_NAME;
    assert!(!fts_attributes_supported(&attributes));
    let mut attributes = supported;
    attributes.volattr = 1;
    assert!(!fts_attributes_supported(&attributes));
    let mut attributes = supported;
    attributes.dirattr = 1;
    assert!(!fts_attributes_supported(&attributes));
    let mut attributes = supported;
    attributes.forkattr = 1;
    assert!(!fts_attributes_supported(&attributes));
}

#[test]
fn virtual_entry_fixture_reports_invalid_logical_paths() {
    assert!(
        fts_read_virtual_entry_for_test(Path::new("/"), FTS_D)
            .unwrap_err()
            .to_string()
            .contains("no parent")
    );
    assert!(
        fts_read_virtual_entry_for_test(Path::new("."), FTS_D)
            .unwrap_err()
            .to_string()
            .contains("no name")
    );
    let nul = Path::new(OsStr::from_bytes(b"/tmp/a\0b"));
    assert!(fts_read_virtual_entry_for_test(nul, FTS_D).is_err());
}

#[test]
fn non_directory_entries_do_not_request_fts_skip() {
    let mut entry = TestEntry::new("/file", "/file", "file");
    unsafe { (*entry.pointer()).fts_info = FTS_F };

    assert_eq!(
        unsafe { skip_fts_directory(std::ptr::null_mut(), entry.pointer()) },
        0
    );
}

#[test]
fn fts_hooks_traverse_an_upper_only_directory_without_changing_cwd() {
    const FTS_PHYSICAL: libc::c_int = 0x010;

    let root = tempfile::tempdir().unwrap();
    let lower = root.path().join("lower");
    std::fs::create_dir(&lower).unwrap();
    let runtime = FilesystemHookRuntime::new(root.path().join("workdir/fs")).unwrap();
    let logical = lower.join("created");
    let upper = runtime
        .filesystem
        .create_directory(&logical, 0o700)
        .unwrap();
    std::fs::write(upper.join("upper.txt"), b"upper").unwrap();
    let path = CString::new(logical.as_os_str().as_bytes()).unwrap();
    let original_directory = std::env::current_dir().unwrap();

    with_test_runtime(&runtime, || unsafe {
        assert!(agora_sandbox_fts_open(std::ptr::null(), FTS_PHYSICAL, None).is_null());
        assert_eq!(*libc::__error(), libc::EFAULT);

        let mut paths = [path.as_ptr().cast_mut(), std::ptr::null_mut()];
        let stream = agora_sandbox_fts_open(paths.as_mut_ptr(), FTS_PHYSICAL, None);
        assert!(!stream.is_null());
        assert!(!fts_stream_may_change_current_directory(stream));

        let root_entry = agora_sandbox_fts_read(stream);
        assert!(!root_entry.is_null());
        let children = agora_sandbox_fts_children(stream, 0);
        assert!(!children.is_null());
        let mut child_names = Vec::new();
        let mut child = children;
        while !child.is_null() {
            child_names.push(
                CStr::from_ptr((*child).fts_name.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            );
            child = (*child).fts_link;
        }
        assert!(child_names.contains(&"upper.txt".to_string()));

        let mut traversed = Vec::new();
        loop {
            let entry = agora_sandbox_fts_read(stream);
            if entry.is_null() {
                break;
            }
            traversed.push(
                CStr::from_ptr((*entry).fts_name.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        assert!(!traversed.is_empty());
        assert_eq!(agora_sandbox_fts_close(stream), 0);
        assert!(fts_stream_may_change_current_directory(stream));
        assert_eq!(std::env::current_dir().unwrap(), original_directory);
    });
}

#[test]
fn fts_visibility_accepts_an_entry_without_an_access_path() {
    let root = tempfile::tempdir().unwrap();
    let runtime = FilesystemHookRuntime::new(root.path().join("workdir/fs")).unwrap();
    let mut entry = unsafe { std::mem::zeroed::<DarwinFtsEntry>() };

    assert!(fts_entry_is_visible(&runtime, &raw mut entry).unwrap());
}

#[test]
fn virtual_fts_repair_recovers_native_entry_types_and_metadata() {
    let root = tempfile::tempdir().unwrap();
    let lower = root.path().join("lower");
    let file = lower.join("file.txt");
    let directory = lower.join("directory");
    let link = lower.join("link");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(&file, b"content").unwrap();
    symlink(&file, &link).unwrap();
    let runtime = FilesystemHookRuntime::new(root.path().join("workdir/fs")).unwrap();
    let mut marker = 0_u8;
    let stream = (&mut marker as *mut u8).cast::<libc::c_void>();
    lock(fts_streams()).insert(stream as usize, stream_state(Vec::new()));

    for (path, name, expected) in [
        (file.as_path(), "file.txt", FTS_F),
        (directory.as_path(), "directory", FTS_D),
        (directory.as_path(), ".", FTS_DOT),
        (link.as_path(), "link", FTS_SL),
        (Path::new("/dev/null"), "null", FTS_DEFAULT),
    ] {
        let path = path.to_string_lossy();
        let mut entry = TestEntry::new(&path, &path, name);
        let mut status = Box::new(unsafe { std::mem::zeroed::<libc::stat>() });
        unsafe {
            (*entry.pointer()).fts_info = FTS_NS;
            (*entry.pointer()).fts_statp = status.as_mut();
        }

        assert!(repair_virtual_fts_entry(&runtime, stream, entry.pointer()).unwrap());
        assert_eq!(unsafe { (*entry.pointer()).fts_info }, expected);
        assert_eq!(unsafe { (*entry.pointer()).fts_errno }, 0);
        assert_ne!(status.st_mode, 0);
    }

    let directory_path = directory.to_string_lossy();
    let mut known = TestEntry::new(&directory_path, &directory_path, "directory");
    let mut status = Box::new(unsafe { std::mem::zeroed::<libc::stat>() });
    unsafe {
        (*known.pointer()).fts_info = FTS_D;
        (*known.pointer()).fts_statp = status.as_mut();
    }
    assert!(!repair_virtual_fts_entry(&runtime, stream, known.pointer()).unwrap());

    let mut without_path = unsafe { std::mem::zeroed::<DarwinFtsEntry>() };
    without_path.fts_info = FTS_NS;
    assert!(!repair_virtual_fts_entry(&runtime, stream, &raw mut without_path).unwrap());

    let missing = lower.join("missing").to_string_lossy().into_owned();
    let mut missing = TestEntry::new(&missing, &missing, "missing");
    unsafe { (*missing.pointer()).fts_info = FTS_NS };
    let error = repair_virtual_fts_entry(&runtime, stream, missing.pointer()).unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .and_then(|error| error.raw_os_error()),
        Some(libc::ENOENT)
    );

    lock(fts_streams()).remove(&(stream as usize));
}

#[test]
fn reentrant_fts_hooks_delegate_to_the_native_stream() {
    const FTS_PHYSICAL: libc::c_int = 0x010;

    let root = tempfile::tempdir().unwrap();
    let lower = root.path().join("lower");
    std::fs::create_dir(&lower).unwrap();
    std::fs::write(lower.join("file.txt"), b"content").unwrap();
    let runtime = FilesystemHookRuntime::new(root.path().join("workdir/fs")).unwrap();
    let path = CString::new(lower.as_os_str().as_bytes()).unwrap();

    with_test_runtime(&runtime, || unsafe {
        let _guard = FilesystemHookGuard::enter().unwrap();
        let mut paths = [path.as_ptr().cast_mut(), std::ptr::null_mut()];
        let stream = agora_sandbox_fts_open(paths.as_mut_ptr(), FTS_PHYSICAL, None);
        assert!(!stream.is_null());
        assert!(!agora_sandbox_fts_read(stream).is_null());
        let _ = agora_sandbox_fts_children(stream, 0);
        assert_eq!(agora_sandbox_fts_close(stream), 0);
    });
}

#[test]
fn virtual_bulk_delegates_unsupported_recursive_and_unmanaged_requests() {
    let root = tempfile::tempdir().unwrap();
    let directory = std::fs::File::open(root.path()).unwrap();
    let descriptor = directory.as_raw_fd();
    let mut attributes = supported_attributes();
    let mut buffer = vec![0_u8; 4096];

    unsafe {
        let _ = sandbox_getattrlistbulk(
            descriptor,
            (&raw mut attributes).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            0,
        );
    }

    let mut marker = 0_u8;
    let stream = (&mut marker as *mut u8).cast();
    let _bulk = FtsVirtualBulk::enter(stream);
    let mut unsupported = attributes;
    unsupported.dirattr = 1;
    unsafe {
        let _ = sandbox_getattrlistbulk(
            descriptor,
            (&raw mut unsupported).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            0,
        );
    }

    let runtime = FilesystemHookRuntime::new(root.path().join("workdir/fs")).unwrap();
    with_test_runtime(&runtime, || unsafe {
        {
            let _guard = FilesystemHookGuard::enter().unwrap();
            let _ = sandbox_getattrlistbulk(
                descriptor,
                (&raw mut attributes).cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            );
        }
        assert_eq!(
            sandbox_getattrlistbulk(
                -1,
                (&raw mut attributes).cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::EBADF);
    });
}

#[test]
fn virtual_bulk_delegates_before_reading_tls_when_the_hook_is_unavailable() {
    assert!(
        active_fts_bulk_guard_with(None, || panic!(
            "FTS TLS must not be read before initialization"
        ))
        .is_none()
    );
}
