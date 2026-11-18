use super::{FilesystemMode, FilesystemWorkspace, PlainWorkspace, lock_workspace};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;

fn temporary_directory(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("agora-workspace-{name}-{}", uuid::Uuid::new_v4()))
}

#[test]
fn workspace_lock_is_released_by_the_owner_even_with_an_inherited_fd() {
    let directory = tempfile::tempdir().unwrap();
    let owner = lock_workspace(directory.path()).unwrap();
    // dup and fork retain the same kernel open file description.
    let inherited = owner.file.try_clone().unwrap();
    assert!(lock_workspace(directory.path()).is_err());
    drop(owner);
    let next = lock_workspace(directory.path()).expect("owner finished while another FD survived");
    drop(inherited);
    assert!(lock_workspace(directory.path()).is_err());
    drop(next);
}

#[test]
fn forked_child_dropping_its_workspace_copy_does_not_unlock_the_parent() {
    let directory = tempfile::tempdir().unwrap();
    let owner = lock_workspace(directory.path()).unwrap();
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        drop(owner);
        unsafe { libc::_exit(0) };
    }
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert_eq!(status, 0);
    assert!(lock_workspace(directory.path()).is_err());
    drop(owner);
    assert!(lock_workspace(directory.path()).is_ok());
}

#[test]
fn plain_mode_reads_the_key_marker_only_under_the_workspace_lock() {
    let workdir = tempfile::tempdir().unwrap();
    let root = workdir.path().join("fs");
    std::fs::create_dir(&root).unwrap();
    let owner = lock_workspace(&root).unwrap();
    std::fs::write(root.join(".key.json"), b"encrypted workspace").unwrap();
    assert!(
        PlainWorkspace::start(workdir.path())
            .unwrap_err()
            .to_string()
            .contains("already in use")
    );
    drop(owner);
    assert!(
        PlainWorkspace::start(workdir.path())
            .unwrap_err()
            .to_string()
            .contains("encrypted filesystem state")
    );
}

#[test]
fn plain_workspace_is_persistent_and_exclusive() {
    let workdir = temporary_directory("plain");
    let workspace = FilesystemWorkspace::start(&workdir, FilesystemMode::Plain, None).unwrap();
    assert_eq!(
        workspace.root().file_name(),
        Some(std::ffi::OsStr::new("fs"))
    );
    assert_eq!(
        workspace
            .root()
            .join(".fs.lock")
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    std::fs::write(workspace.root().join("marker"), b"persisted").unwrap();

    let error = PlainWorkspace::start(&workdir).unwrap_err();
    assert!(error.to_string().contains("filesystem is already in use"));
    let error = FilesystemWorkspace::start(&workdir, FilesystemMode::Encrypted, Some(b"secret"))
        .unwrap_err();
    assert!(error.to_string().contains("filesystem is already in use"));
    drop(workspace);

    let workspace = PlainWorkspace::start(&workdir).unwrap();
    assert_eq!(
        std::fs::read(workspace.root().join("marker")).unwrap(),
        b"persisted"
    );
    std::fs::remove_dir_all(workdir).unwrap();
}

#[test]
fn workspace_rejects_keys_that_do_not_match_the_mode() {
    let workdir = temporary_directory("mode");
    let missing =
        FilesystemWorkspace::start(&workdir, FilesystemMode::Encrypted, None).unwrap_err();
    assert!(missing.to_string().contains("filesystem key is required"));

    let unexpected =
        FilesystemWorkspace::start(&workdir, FilesystemMode::Plain, Some(b"unused")).unwrap_err();
    assert!(
        unexpected
            .to_string()
            .contains("cannot be used with plain filesystem mode")
    );
}

#[test]
fn encrypted_workspace_exposes_only_derived_runtime_key_material() {
    let workdir = temporary_directory("derived-key");
    let workspace =
        FilesystemWorkspace::start(&workdir, FilesystemMode::Encrypted, Some(b"secret")).unwrap();

    assert_eq!(workspace.encrypted_cipher_key().unwrap().len(), 32);

    drop(workspace);
    std::fs::remove_dir_all(workdir).unwrap();
}

#[test]
fn plain_workspace_rejects_a_file_as_its_root() {
    let workdir = temporary_directory("blocked-root");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(workdir.join("fs"), b"not a directory").unwrap();

    let error = PlainWorkspace::start(&workdir).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed to create plain filesystem root")
    );
    std::fs::remove_dir_all(workdir).unwrap();
}

#[test]
fn plain_workspace_rejects_existing_encrypted_key_state() {
    let workdir = temporary_directory("encrypted-state");
    std::fs::create_dir_all(workdir.join("fs")).unwrap();
    std::fs::write(workdir.join("fs/.key.json"), b"encrypted state").unwrap();

    let error = PlainWorkspace::start(&workdir).unwrap_err();

    assert!(error.to_string().contains("use encrypted filesystem mode"));
    std::fs::remove_dir_all(workdir).unwrap();
}

#[test]
fn plain_workspace_rejects_a_symlinked_root_without_touching_its_target() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().join("workdir");
    let external = directory.path().join("external");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::create_dir(&external).unwrap();
    std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&external, workdir.join("fs")).unwrap();

    let accepted = PlainWorkspace::start(&workdir).is_ok();

    assert!(!accepted, "a symbolic-link filesystem root was accepted");
    assert_eq!(
        external.metadata().unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!external.join(".fs.lock").exists());
}

#[test]
fn plain_workspace_rejects_a_symlinked_lock_without_touching_its_target() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().join("workdir");
    let root = workdir.join("fs");
    let external = directory.path().join("external-lock");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(&external, b"external").unwrap();
    symlink(&external, root.join(".fs.lock")).unwrap();

    let accepted = PlainWorkspace::start(&workdir).is_ok();

    assert!(!accepted, "a symbolic-link filesystem lock was accepted");
    assert_eq!(std::fs::read(external).unwrap(), b"external");
}
