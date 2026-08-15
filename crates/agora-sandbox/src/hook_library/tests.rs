#[cfg(target_os = "macos")]
#[test]
fn embedded_hook_matches_build_md5() {
    use md5::{Digest, Md5};

    assert!(super::EMBEDDED_HOOK.len() > 4);
    assert_eq!(&super::EMBEDDED_HOOK[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    let digest = Md5::digest(super::EMBEDDED_HOOK);
    assert_eq!(
        super::hex_digest(digest.as_slice()),
        super::EMBEDDED_HOOK_MD5
    );
}

#[cfg(target_os = "macos")]
#[test]
fn materializes_once_and_reuses_matching_bytes() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let root = tempfile::tempdir().unwrap();
    let first = super::materialize(root.path()).unwrap();
    let first_metadata = std::fs::metadata(&first).unwrap();
    assert_eq!(std::fs::read(&first).unwrap(), super::EMBEDDED_HOOK);
    assert_eq!(first_metadata.permissions().mode() & 0o777, 0o500);
    assert_eq!(
        first.parent().unwrap().file_name().unwrap(),
        super::EMBEDDED_HOOK_MD5
    );

    let second = super::materialize(root.path()).unwrap();
    let second_metadata = std::fs::metadata(&second).unwrap();
    assert_eq!(first, second);
    assert_eq!(first_metadata.ino(), second_metadata.ino());
    assert_eq!(
        first_metadata.modified().unwrap(),
        second_metadata.modified().unwrap()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn rejects_a_symlinked_runtime_directory_without_writing_outside() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), root.path().join("runtime")).unwrap();

    let error = super::materialize(root.path()).unwrap_err();

    assert!(error.to_string().contains("symbolic link"));
    assert!(!outside.path().join("hook").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn rejects_a_symlinked_lock_without_touching_its_target() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = tempfile::tempdir().unwrap();
    let hooks = root.path().join("runtime/hook");
    std::fs::create_dir_all(&hooks).unwrap();
    let outside = root.path().join("outside-lock");
    std::fs::write(&outside, b"sentinel").unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o644)).unwrap();
    symlink(&outside, hooks.join(".lock")).unwrap();

    let error = super::materialize(root.path()).unwrap_err();

    assert!(error.to_string().contains("symbolic link"));
    assert_eq!(std::fs::read(&outside).unwrap(), b"sentinel");
    assert_eq!(
        std::fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[cfg(target_os = "macos")]
#[test]
fn restores_a_corrupt_materialized_hook_atomically() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let path = super::materialize(root.path()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&path, b"corrupt").unwrap();

    assert_eq!(super::materialize(root.path()).unwrap(), path);
    assert_eq!(std::fs::read(path).unwrap(), super::EMBEDDED_HOOK);
}

#[cfg(target_os = "macos")]
#[test]
fn restores_an_unreadable_materialized_hook_atomically() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let path = super::materialize(root.path()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

    assert_eq!(super::materialize(root.path()).unwrap(), path);
    assert_eq!(std::fs::read(&path).unwrap(), super::EMBEDDED_HOOK);
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o500
    );
}

#[cfg(target_os = "macos")]
#[test]
fn replaces_a_destination_symlink_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let path = super::materialize(root.path()).unwrap();
    std::fs::remove_file(&path).unwrap();
    let sentinel = root.path().join("sentinel");
    std::fs::write(&sentinel, b"outside").unwrap();
    symlink(&sentinel, &path).unwrap();

    assert_eq!(super::materialize(root.path()).unwrap(), path);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside");
    assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
    assert_eq!(std::fs::read(path).unwrap(), super::EMBEDDED_HOOK);
}

#[cfg(target_os = "macos")]
#[test]
fn replaces_a_matching_destination_symlink_instead_of_reusing_it() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = tempfile::tempdir().unwrap();
    let path = super::materialize(root.path()).unwrap();
    std::fs::remove_file(&path).unwrap();
    let outside = root.path().join("outside-hook");
    std::fs::write(&outside, super::EMBEDDED_HOOK).unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o500)).unwrap();
    symlink(&outside, &path).unwrap();

    assert_eq!(super::materialize(root.path()).unwrap(), path);
    assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
    assert_eq!(std::fs::read(&outside).unwrap(), super::EMBEDDED_HOOK);
}

#[cfg(target_os = "macos")]
#[test]
fn corrects_only_the_mode_for_matching_bytes() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let root = tempfile::tempdir().unwrap();
    let path = super::materialize(root.path()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let before = std::fs::metadata(&path).unwrap();

    super::materialize(root.path()).unwrap();
    let after = std::fs::metadata(&path).unwrap();

    assert_eq!(before.ino(), after.ino());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    assert_eq!(after.permissions().mode() & 0o777, 0o500);
}

#[cfg(target_os = "macos")]
#[test]
fn rejects_a_non_directory_md5_path() {
    let root = tempfile::tempdir().unwrap();
    let hooks = root.path().join("runtime/hook");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(hooks.join(super::EMBEDDED_HOOK_MD5), b"not a directory").unwrap();

    let error = super::materialize(root.path()).unwrap_err();

    assert!(error.to_string().contains("not a directory"));
}

#[cfg(target_os = "macos")]
#[test]
fn reports_invalid_work_and_runtime_directory_paths() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    std::fs::write(&workdir, b"not a directory").unwrap();
    let error = super::materialize(&workdir).unwrap_err();
    assert!(error.to_string().contains("work directory"));

    let missing_parent = root.path().join("missing/child");
    let error = super::prepare_directory(&missing_parent).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed to create hook runtime directory")
    );

    let parent_file = root.path().join("parent-file");
    std::fs::write(&parent_file, b"not a directory").unwrap();
    let error = super::prepare_directory(&parent_file.join("child")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed to inspect hook runtime directory")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn rejects_a_directory_as_the_materialization_lock() {
    let root = tempfile::tempdir().unwrap();
    let hooks = root.path().join("runtime/hook");
    std::fs::create_dir_all(hooks.join(".lock")).unwrap();

    let error = super::lock(&hooks).unwrap_err();

    assert!(error.to_string().contains("not a regular file"));
}

#[cfg(target_os = "macos")]
#[test]
fn checksum_and_matching_file_report_native_path_errors() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    assert!(
        super::checksum(&missing)
            .unwrap_err()
            .to_string()
            .contains("MD5")
    );

    let parent = root.path().join("parent");
    std::fs::write(&parent, b"not a directory").unwrap();
    let error = super::is_matching_regular_file(&parent.join("child")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed to inspect embedded sandbox hook")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn publish_reports_staging_and_destination_failures() {
    let root = tempfile::tempdir().unwrap();
    let directory_file = root.path().join("not-a-directory");
    std::fs::write(&directory_file, b"file").unwrap();
    let error = super::publish(&directory_file, &directory_file.join("hook")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed to create embedded hook staging file")
    );

    let destination = root.path().join("existing-directory");
    std::fs::create_dir(&destination).unwrap();
    let error = super::publish(root.path(), &destination).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed to publish embedded sandbox hook")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn materialized_hook_is_signed_and_exports_interpose_symbols() {
    let root = tempfile::tempdir().unwrap();
    let path = super::materialize(root.path()).unwrap();

    let signature = std::process::Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        signature.status.success(),
        "{}",
        String::from_utf8_lossy(&signature.stderr)
    );

    let symbols = std::process::Command::new("nm")
        .arg("-gU")
        .arg(path)
        .output()
        .unwrap();
    assert!(symbols.status.success());
    let symbols = String::from_utf8(symbols.stdout).unwrap();
    for symbol in [
        "_agora_sandbox_open",
        "_agora_sandbox_dlopen",
        "_agora_sandbox_dlopen_preflight",
        "_agora_sandbox_dlerror",
        "_agora_sandbox_connect",
        "_agora_sandbox_posix_spawn",
    ] {
        assert!(symbols.contains(symbol), "missing {symbol}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn materialize_child() {
    let Some(workdir) = std::env::var_os("AGORA_SANDBOX_MATERIALIZE_CHILD_WORKDIR") else {
        return;
    };
    let path = super::materialize(std::path::Path::new(&workdir)).unwrap();
    println!("{}", path.display());
}

#[cfg(target_os = "macos")]
#[test]
fn serializes_materialization_across_processes() {
    let root = tempfile::tempdir().unwrap();
    let executable = std::env::current_exe().unwrap();
    let mut children = (0..4)
        .map(|_| {
            std::process::Command::new(&executable)
                .args([
                    "--exact",
                    "hook_library::tests::materialize_child",
                    "--nocapture",
                ])
                .env("AGORA_SANDBOX_MATERIALIZE_CHILD_WORKDIR", root.path())
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();

    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    let path = root
        .path()
        .join("runtime/hook")
        .join(super::EMBEDDED_HOOK_MD5)
        .join("libagora_sandbox.dylib");
    assert_eq!(std::fs::read(path).unwrap(), super::EMBEDDED_HOOK);
}
