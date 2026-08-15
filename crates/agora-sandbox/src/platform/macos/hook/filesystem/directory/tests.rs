use super::*;
use crate::nfs::protocol::{RemoteEntry, RemoteFileType, RemoteMetadata};

fn remote_entry(name: &str, file_type: RemoteFileType) -> RemoteEntry {
    RemoteEntry {
        name: name.to_string(),
        metadata: RemoteMetadata {
            file_type,
            size: 0,
            modified_seconds: 1,
            modified_nanoseconds: 0,
            identity: format!("identity-{name}"),
        },
    }
}

#[test]
fn remote_cursor_yields_posix_directory_entries_and_rewinds() {
    let mut cursor = DirectoryCursor::remote(vec![
        remote_entry("file.txt", RemoteFileType::File),
        remote_entry("directory", RemoteFileType::Directory),
    ]);

    for (index, (name, file_type)) in [
        (".", libc::DT_DIR),
        ("..", libc::DT_DIR),
        ("file.txt", libc::DT_REG),
        ("directory", libc::DT_DIR),
    ]
    .into_iter()
    .enumerate()
    {
        let entry = cursor.next_remote().unwrap().unwrap();
        assert_eq!(
            unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes(),
            name.as_bytes()
        );
        assert_eq!(unsafe { (*entry).d_type }, file_type);
        assert_eq!(unsafe { (*entry).d_seekoff }, (index + 1) as u64);
    }
    assert!(cursor.next_remote().unwrap().is_none());

    cursor.reset();
    let entry = cursor.next_remote().unwrap().unwrap();
    assert_eq!(
        unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes(),
        b"."
    );
}

#[test]
fn remote_cursor_reports_absent_and_overlong_entries() {
    let view = DirectoryView::passthrough(std::env::temp_dir());
    let mut local = DirectoryCursor::filter(&view);
    assert!(local.next_remote().unwrap().is_none());

    let name = "x".repeat(unsafe { std::mem::zeroed::<libc::dirent>() }.d_name.len());
    let mut remote = DirectoryCursor::remote(vec![remote_entry(&name, RemoteFileType::File)]);
    assert!(remote.next_remote().unwrap().is_some());
    assert!(remote.next_remote().unwrap().is_some());
    let error = remote.next_remote().unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<io::Error>()
            .and_then(|error| error.raw_os_error()),
        Some(libc::ENAMETOOLONG)
    );
}

#[test]
fn directory_wrappers_surface_remote_and_alias_name_overflow() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("physical"), b"file").unwrap();
    let path = CString::new(directory.path().as_os_str().as_bytes()).unwrap();

    unsafe {
        let remote_directory = libc::opendir(path.as_ptr());
        assert!(!remote_directory.is_null());
        let too_long = "x".repeat(std::mem::zeroed::<libc::dirent>().d_name.len());
        lock(directory_cursors()).insert(
            remote_directory as usize,
            DirectoryCursor::remote(vec![remote_entry(&too_long, RemoteFileType::File)]),
        );
        assert!(!sandbox_readdir(remote_directory).is_null());
        assert!(!sandbox_readdir(remote_directory).is_null());
        assert!(sandbox_readdir(remote_directory).is_null());
        assert_eq!(*libc::__error(), libc::ENAMETOOLONG);
        sandbox_rewinddir(remote_directory);
        assert!(!sandbox_readdir(remote_directory).is_null());
        assert_eq!(sandbox_closedir(remote_directory), 0);

        let aliased_directory = libc::opendir(path.as_ptr());
        assert!(!aliased_directory.is_null());
        let mut aliases = HashMap::new();
        aliases.insert(
            b"physical".to_vec(),
            vec![b'y'; std::mem::zeroed::<libc::dirent>().d_name.len()],
        );
        lock(directory_cursors()).insert(
            aliased_directory as usize,
            DirectoryCursor {
                sources: vec![DirectorySource {
                    directory: aliased_directory as usize,
                    lower: false,
                    owned: false,
                }],
                source_index: 0,
                hidden: HashSet::new(),
                aliases,
                seen: HashSet::new(),
                remote_names: HashSet::new(),
                remote: None,
            },
        );
        while !sandbox_readdir(aliased_directory).is_null() {}
        assert_eq!(*libc::__error(), libc::ENAMETOOLONG);
        assert_eq!(sandbox_closedir(aliased_directory), 0);
    }
}

#[test]
fn allocated_getcwd_and_realpath_return_owned_logical_paths() {
    let directory = tempfile::tempdir().unwrap();
    let lower = directory.path().join("lower");
    std::fs::create_dir(&lower).unwrap();
    let file = lower.join("file");
    std::fs::write(&file, b"file").unwrap();
    let runtime = FilesystemHookRuntime::new(directory.path().join("workdir/fs")).unwrap();
    let path = CString::new(file.as_os_str().as_bytes()).unwrap();
    let canonical_file = std::fs::canonicalize(&file).unwrap();

    with_test_runtime(&runtime, || unsafe {
        let cwd = sandbox_getcwd(std::ptr::null_mut(), 0);
        assert!(!cwd.is_null());
        assert!(!CStr::from_ptr(cwd).to_bytes().is_empty());
        libc::free(cwd.cast());

        assert!(sandbox_getcwd(std::ptr::null_mut(), 1).is_null());
        assert_eq!(*libc::__error(), libc::ERANGE);

        let resolved = sandbox_realpath(path.as_ptr(), std::ptr::null_mut());
        assert!(!resolved.is_null());
        assert_eq!(
            CStr::from_ptr(resolved).to_bytes(),
            canonical_file.as_os_str().as_bytes()
        );
        libc::free(resolved.cast());

        let mut resolved = [0_i8; libc::PATH_MAX as usize];
        assert_eq!(
            sandbox_realpath(path.as_ptr(), resolved.as_mut_ptr()),
            resolved.as_mut_ptr()
        );
        assert_eq!(
            CStr::from_ptr(resolved.as_ptr()).to_bytes(),
            canonical_file.as_os_str().as_bytes()
        );

        let native = sandbox_realpath(c"/dev/null".as_ptr(), std::ptr::null_mut());
        assert!(!native.is_null());
        assert_eq!(CStr::from_ptr(native).to_bytes(), b"/dev/null");
        libc::free(native.cast());
    });
}
