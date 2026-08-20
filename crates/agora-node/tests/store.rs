use agora_node::store::{
    AgentIdentity, ChannelIdentity, SessionStore, StoreChannelSessionKey, StoreSessionKey,
};

#[test]
fn store_v4_is_small_private_and_identity_namespaced() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db").join("store.db");
    let store = SessionStore::open(&path).unwrap();

    let agent = AgentIdentity::new("codex", "codex", "/workspace");
    let old_bot = ChannelIdentity::new("chat", "telegram", "100");
    let new_bot = ChannelIdentity::new("chat", "telegram", "200");
    let old_key = StoreSessionKey::session(agent.clone(), old_bot.clone(), "conversation");
    let new_key = StoreSessionKey::session(agent.clone(), new_bot.clone(), "conversation");

    assert_eq!(store.get(&old_key).unwrap(), None);
    store.observe(&old_key, None, "thread-1").unwrap();
    store.observe(&old_key, None, "thread-1").unwrap();
    assert_eq!(store.get(&new_key).unwrap(), None);

    let block = StoreChannelSessionKey::new(old_bot.clone(), "conversation");
    assert!(store.disable_agent(&block, &agent).unwrap());
    assert!(!store.is_agent_enabled(&block, &agent).unwrap());
    assert!(
        store
            .is_agent_enabled(
                &StoreChannelSessionKey::new(new_bot, "conversation"),
                &agent,
            )
            .unwrap()
    );
    assert_eq!(store.disabled_agents(&block).unwrap(), vec![agent]);

    drop(store);
    let connection = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(schema_version(&connection), 4);
    assert_eq!(
        table_names(&connection),
        ["agent_sessions", "channel_session_agent_blocks"]
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[test]
fn opening_an_explicit_store_preserves_existing_parent_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("shared");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

    SessionStore::open(parent.join("store.db")).unwrap();

    assert_eq!(
        std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn session_observation_is_atomic_and_backend_ids_are_unique_per_agent_identity() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::open(temp.path().join("store.db")).unwrap();
    let first_agent = AgentIdentity::new("codex", "codex", "/one");
    let moved_agent = AgentIdentity::new("codex", "codex", "/two");
    let channel = ChannelIdentity::new("chat", "lark", "app-1");
    let first = StoreSessionKey::session(first_agent.clone(), channel.clone(), "one");
    let second = StoreSessionKey::session(first_agent, channel.clone(), "two");
    let other_identity = StoreSessionKey::session(moved_agent, channel, "one");

    store.observe(&first, None, "thread-1").unwrap();
    assert!(store.observe(&first, Some("thread-1"), "thread-2").is_err());
    assert_eq!(store.get(&first).unwrap().as_deref(), Some("thread-1"));
    assert!(store.observe(&first, Some("stale"), "thread-2").is_err());
    assert!(store.observe(&second, None, "thread-1").is_err());
    assert_eq!(store.get(&second).unwrap(), None);
    store.observe(&other_identity, None, "thread-1").unwrap();

    assert!(store.remove_if_matches(&first, "stale").is_err());
    assert!(store.remove_if_matches(&first, "thread-1").unwrap());
    assert!(!store.remove_if_matches(&first, "thread-1").unwrap());
}

#[test]
fn shared_sessions_do_not_depend_on_channel_identity() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::open(temp.path().join("store.db")).unwrap();
    let key = StoreSessionKey::shared(AgentIdentity::new("codex", "codex", "/workspace"));

    store.observe(&key, None, "thread-shared").unwrap();
    assert_eq!(store.get(&key).unwrap().as_deref(), Some("thread-shared"));
}

#[test]
fn migration_v3_quarantines_old_rows_without_guessing_identity() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store.db");
    create_v3_store(&path, false);

    let store = SessionStore::open(&path).unwrap();
    let active = StoreSessionKey::session(
        AgentIdentity::new("reviewer", "codex", "/workspace"),
        ChannelIdentity::new("telegram", "telegram", "100"),
        "chat-1",
    );
    assert_eq!(store.get(&active).unwrap(), None);
    drop(store);

    let connection = rusqlite::Connection::open(path).unwrap();
    assert_eq!(schema_version(&connection), 4);
    assert_eq!(row_count(&connection, "agent_sessions"), 2);
    assert_eq!(row_count(&connection, "channel_session_agent_blocks"), 1);
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM agent_sessions
                 WHERE backend_type = 'legacy_v3'
                   AND canonical_workspace = 'legacy_v3'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        2
    );
}

#[test]
fn migration_v3_rolls_back_on_invalid_legacy_data() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store.db");
    create_v3_store(&path, true);

    let error = SessionStore::open(&path).err().unwrap();
    assert!(error.to_string().contains("migrate sqlite store schema"));
    let connection = rusqlite::Connection::open(path).unwrap();
    assert_eq!(schema_version(&connection), 3);
    assert_eq!(row_count(&connection, "agent_sessions"), 3);
    assert_eq!(table_count(&connection, "agent_sessions_v3"), 0);
}

#[test]
fn unknown_schema_versions_are_rejected_without_mutation() {
    for version in [2, 5] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("store.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute("CREATE TABLE marker (value TEXT)", [])
            .unwrap();
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(connection);

        let error = SessionStore::open(&path).err().unwrap();
        assert!(error.to_string().contains(&format!(
            "unsupported sqlite store schema version: {version}"
        )));
        let connection = rusqlite::Connection::open(path).unwrap();
        assert_eq!(schema_version(&connection), version);
        assert_eq!(table_count(&connection, "marker"), 1);
        assert_eq!(table_count(&connection, "agent_sessions"), 0);
    }
}

#[test]
fn default_store_path_is_under_agora_home() {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap();
    assert_eq!(
        SessionStore::default_path().unwrap(),
        home.join(".agora").join("db").join("store.db")
    );
}

fn create_v3_store(path: &std::path::Path, invalid: bool) {
    let connection = rusqlite::Connection::open(path).unwrap();
    let schema = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/store/schema_v3.sql"),
    )
    .unwrap();
    connection.execute_batch(&schema).unwrap();
    connection.pragma_update(None, "user_version", 3).unwrap();
    connection
        .execute(
            "INSERT INTO agent_sessions VALUES
             ('shared', NULL, NULL, 'codex', 'thread-shared', 10, 11),
             ('session', 'telegram', 'chat-1', 'reviewer', 'thread-chat', 12, 13)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO channel_session_agent_blocks
             VALUES ('telegram', 'chat-1', 'codex')",
            [],
        )
        .unwrap();
    if invalid {
        connection
            .execute_batch(
                "PRAGMA ignore_check_constraints = ON;
                 INSERT INTO agent_sessions VALUES
                 ('session', 'telegram', 'chat-bad', 'broken', '', 14, 15);",
            )
            .unwrap();
    }
}

fn schema_version(connection: &rusqlite::Connection) -> i64 {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

fn table_names(connection: &rusqlite::Connection) -> Vec<String> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn table_count(connection: &rusqlite::Connection, table: &str) -> i64 {
    connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .unwrap()
}

fn row_count(connection: &rusqlite::Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}
