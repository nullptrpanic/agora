#![cfg(unix)]

use agora_node::instance::{NodeInstanceGuard, StatePaths};
use agora_node::{config::NodeConfig, daemon::Daemon};
use std::os::unix::fs::PermissionsExt;

#[test]
fn second_guard_fails_until_the_first_guard_drops() {
    let home = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_home(home.path());
    let first = NodeInstanceGuard::acquire(paths.clone()).unwrap();

    let error = NodeInstanceGuard::acquire(paths.clone()).unwrap_err();
    assert!(error.to_string().contains("already running"));

    drop(first);
    NodeInstanceGuard::acquire(paths).unwrap();
}

#[test]
fn guard_repairs_private_state_permissions() {
    let home = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_home(home.path());
    std::fs::create_dir_all(paths.db_dir()).unwrap();
    std::fs::set_permissions(paths.root(), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(paths.db_dir(), std::fs::Permissions::from_mode(0o755)).unwrap();

    let _guard = NodeInstanceGuard::acquire(paths.clone()).unwrap();

    assert_eq!(
        std::fs::metadata(paths.root())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(paths.db_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(paths.lock_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn daemon_acquires_the_guard_before_opening_sqlite() {
    let home = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_home(home.path());
    let _owner = NodeInstanceGuard::acquire(paths.clone()).unwrap();
    let config: NodeConfig = serde_json::from_value(serde_json::json!({
        "channels": [],
        "agents": []
    }))
    .unwrap();

    let result = Daemon::new_with_paths(config, paths.clone());

    let error = match result {
        Ok(_) => panic!("a second daemon unexpectedly acquired the default store"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("already running"));
    assert!(!paths.store_path().exists());
}
