use super::*;
use agora_sandbox::runner::FilesystemMode;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};

fn write_config(root: &Path, contents: &str) -> PathBuf {
    let path = root.join("sandbox.json");
    std::fs::write(&path, contents).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn load_error(path: &Path) -> String {
    RunConfig::load(path)
        .err()
        .expect("configuration must be rejected")
        .to_string()
}

#[test]
fn unified_config_resolves_runtime_settings_and_redacts_secrets() {
    let root = tempfile::tempdir().unwrap();
    let path = write_config(
        root.path(),
        r#"{
          "workdir": "state",
          "tls": "auto",
          "filesystem": {
            "local": { "encrypt": "encrypted", "key": "local-secret" },
            "nfs": [
              {
                "type": "smb",
                "dir": "/smb",
                "server": "smb://127.0.0.1:10445/workspace/projects/current",
                "username": "openclaw",
                "password": "remote-secret"
              },
              {
                "type": "smb",
                "dir": "/archive",
                "server": "smb://127.0.0.2/archive"
              }
            ]
          },
          "log": { "file": "logs/sandbox.jsonl" }
        }"#,
    );

    let loaded = RunConfig::load(&path).unwrap();
    assert_eq!(loaded.workdir(), root.path().join("state"));
    assert_eq!(
        loaded.log_file(),
        root.path().join("state/logs/sandbox.jsonl")
    );
    let runtime = loaded.into_runtime(PathBuf::from("/tmp/hook.dylib"));

    assert!(matches!(runtime.network.tls, TlsMode::Auto));
    assert_eq!(runtime.filesystem_mode(), FilesystemMode::Encrypted);
    assert_eq!(
        runtime.encrypted_workspace_key(),
        Some(&b"local-secret"[..])
    );
    assert_eq!(runtime.smb_remotes().len(), 2);
    let remote = &runtime.smb_remotes()[0];
    assert_eq!(remote.logical_root(), Path::new("/smb"));
    assert_eq!(remote.server(), "127.0.0.1:10445");
    assert_eq!(remote.share(), "workspace");
    assert_eq!(remote.remote_path(), "projects/current");
    assert_eq!(remote.username(), "openclaw");
    assert_eq!(
        runtime.smb_remotes()[1].logical_root(),
        Path::new("/archive")
    );
    let debug = format!("{runtime:?}");
    assert!(!debug.contains("local-secret"));
    assert!(!debug.contains("remote-secret"));
}

#[test]
fn config_paths_expand_home_and_resolve_relative_to_the_config() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve_path(root.path(), Path::new("nested/state")).unwrap(),
        root.path().join("nested/state")
    );
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    assert_eq!(
        resolve_path(root.path(), Path::new("~/.agora-sandbox")).unwrap(),
        home.join(".agora-sandbox")
    );
    assert_eq!(
        resolve_path(root.path(), Path::new("/var/tmp/sandbox.log")).unwrap(),
        PathBuf::from("/var/tmp/sandbox.log")
    );
}

#[test]
fn empty_config_uses_all_runtime_defaults() {
    let root = tempfile::tempdir().unwrap();
    let path = write_config(root.path(), "{}");

    let loaded = RunConfig::load(&path).unwrap();
    assert_eq!(loaded.workdir(), SandboxConfig::default_workdir());
    assert_eq!(
        loaded.log_file(),
        SandboxConfig::default_workdir().join("runtime/logs/sandbox.log")
    );
    let runtime = loaded.into_runtime(PathBuf::from("/tmp/hook.dylib"));
    assert!(matches!(runtime.network.tls, TlsMode::Off));
    assert_eq!(runtime.filesystem_mode(), FilesystemMode::Plain);
    assert!(runtime.encrypted_workspace_key().is_none());
    assert!(runtime.smb_remotes().is_empty());
}

#[test]
fn config_rejects_the_removed_audit_section() {
    let root = tempfile::tempdir().unwrap();
    let path = write_config(root.path(), r#"{ "audit": {} }"#);

    assert!(load_error(&path).contains("failed to parse sandbox config"));
}

#[test]
fn config_rejects_unknown_fields_and_invalid_local_encryption() {
    let root = tempfile::tempdir().unwrap();
    let unknown = write_config(
        root.path(),
        r#"{
          "workdir": "state",
          "tls": "off",
          "filesystem": { "local": { "encrypt": "plain" } },
          "unexpected": true
        }"#,
    );
    assert!(load_error(&unknown).contains("failed to parse sandbox config"));

    let invalid = write_config(
        root.path(),
        r#"{
          "workdir": "state",
          "tls": "off",
          "filesystem": { "local": { "encrypt": "plain", "key": "unused" } }
        }"#,
    );
    assert!(load_error(&invalid).contains("key is not allowed"));

    let missing = write_config(
        root.path(),
        r#"{
          "workdir": "state",
          "tls": "off",
          "filesystem": { "local": { "encrypt": "encrypted" } }
        }"#,
    );
    assert!(load_error(&missing).contains("key is required"));
}

#[test]
fn config_rejects_malformed_smb_uris() {
    for server in [
        "files.example.com/share",
        "smb:///share",
        "smb://files.example.com",
        "smb://user@files.example.com/share",
        "smb://files.example.com/",
    ] {
        let error =
            smb_remote(PathBuf::from("/smb"), server, String::new(), String::new()).unwrap_err();
        assert!(!error.to_string().is_empty(), "{server} must be rejected");
    }
}

#[test]
fn config_file_accepts_normal_permissions_but_not_a_symlink() {
    let root = tempfile::tempdir().unwrap();
    let path = write_config(
        root.path(),
        r#"{
          "workdir": "state",
          "tls": "off",
          "filesystem": { "local": { "encrypt": "plain" } }
        }"#,
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(RunConfig::load(&path).is_ok());

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(RunConfig::load(&path).is_ok());

    let link = root.path().join("sandbox-link.json");
    symlink(&path, &link).unwrap();
    assert!(load_error(&link).contains("failed to open sandbox config"));

    assert!(load_error(root.path()).contains("not a regular file"));
    let fifo = root.path().join("sandbox.fifo");
    let fifo = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(
        load_error(Path::new(std::ffi::OsStr::from_bytes(fifo.as_bytes())))
            .contains("not a regular file")
    );
}

#[test]
fn semantic_session_identity_ignores_json_formatting_but_tracks_runtime_changes() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let hook = root.path().join("hook.dylib");
    std::fs::write(&hook, b"hook-a").unwrap();
    let first_path = root.path().join("first.json");
    let second_path = root.path().join("second.json");
    let changed_path = root.path().join("changed.json");
    std::fs::write(
        &first_path,
        format!(
            r#"{{
              "workdir": "{}",
              "filesystem": {{ "local": {{ "encrypt": "encrypted", "key": "secret-a" }} }},
              "log": {{ "file": "runtime/logs/sandbox.log" }}
            }}"#,
            workdir.display()
        ),
    )
    .unwrap();
    std::fs::write(
        &second_path,
        format!(
            r#"{{"log":{{"file":"runtime/logs/sandbox.log"}},"filesystem":{{"local":{{"key":"secret-a","encrypt":"encrypted"}}}},"workdir":"{}"}}"#,
            workdir.display()
        ),
    )
    .unwrap();
    std::fs::write(
        &changed_path,
        format!(
            r#"{{"workdir":"{}","filesystem":{{"local":{{"encrypt":"encrypted","key":"secret-b"}}}}}}"#,
            workdir.display()
        ),
    )
    .unwrap();

    let first = RunConfig::load(&first_path).unwrap();
    let second = RunConfig::load(&second_path).unwrap();
    let changed = RunConfig::load(&changed_path).unwrap();

    let original_identity = first.session_identity(&hook).unwrap();
    assert_eq!(original_identity, second.session_identity(&hook).unwrap());
    assert_ne!(
        first.session_identity(&hook).unwrap(),
        changed.session_identity(&hook).unwrap()
    );
    std::fs::write(&hook, b"hook-b").unwrap();
    assert_ne!(original_identity, first.session_identity(&hook).unwrap());
}

#[test]
fn semantic_session_identity_normalizes_missing_path_aliases() {
    let root = tempfile::tempdir().unwrap();
    let hook = root.path().join("hook.dylib");
    std::fs::write(&hook, b"hook").unwrap();
    let first_path = root.path().join("first.json");
    let second_path = root.path().join("second.json");
    std::fs::write(&first_path, r#"{ "workdir": "state" }"#).unwrap();
    std::fs::write(
        &second_path,
        r#"{ "workdir": "missing/../state", "log": { "file": "runtime/./logs/sandbox.log" } }"#,
    )
    .unwrap();

    let first = RunConfig::load(&first_path).unwrap();
    let second = RunConfig::load(&second_path).unwrap();

    assert_eq!(first.workdir(), second.workdir());
    assert_eq!(first.log_file(), second.log_file());
    assert_eq!(
        first.session_identity(&hook).unwrap(),
        second.session_identity(&hook).unwrap()
    );
}
