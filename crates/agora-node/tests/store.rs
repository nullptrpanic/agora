use agora_node::config::IsolationScope;
use agora_node::store::{ChannelSessionKey, SessionKey, SessionStore};

#[test]
fn store_schema_is_embedded_from_a_sql_file() {
    let store_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("store");
    let schema = std::fs::read_to_string(store_dir.join("schema.sql")).unwrap();
    let source = std::fs::read_to_string(store_dir.join("mod.rs")).unwrap();

    assert!(schema.contains("CREATE TABLE IF NOT EXISTS agent_sessions"));
    assert!(schema.contains("CREATE TABLE IF NOT EXISTS channel_session_agent_blocks"));
    assert!(schema.contains("isolation_scope"));
    assert!(!schema.contains("scope_type"));
    assert!(source.contains("include_str!(\"schema.sql\")"));
}

#[test]
fn session_store_persists_agent_access_per_channel_session() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store.db");
    let store = SessionStore::open(&path).unwrap();
    let first = ChannelSessionKey::new("lark-1", "chat-1");
    let second = ChannelSessionKey::new("lark-1", "chat-2");
    let other_channel = ChannelSessionKey::new("telegram-1", "chat-1");

    assert!(store.is_agent_enabled(&first, "codex-dev").unwrap());
    assert!(store.disable_agent(&first, "codex-dev").unwrap());
    assert!(!store.disable_agent(&first, "codex-dev").unwrap());
    assert!(!store.is_agent_enabled(&first, "codex-dev").unwrap());
    assert!(store.is_agent_enabled(&second, "codex-dev").unwrap());
    assert!(store.is_agent_enabled(&other_channel, "codex-dev").unwrap());

    drop(store);
    let reopened = SessionStore::open(path).unwrap();
    assert_eq!(
        reopened.disabled_agents(&first).unwrap(),
        vec!["codex-dev".to_string()]
    );
    assert!(reopened.enable_agent(&first, "codex-dev").unwrap());
    assert!(!reopened.enable_agent(&first, "codex-dev").unwrap());
    assert!(reopened.is_agent_enabled(&first, "codex-dev").unwrap());
}

#[test]
fn session_store_round_trips_and_updates_a_mapping() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store.db");
    let store = SessionStore::open(&path).unwrap();
    let key = SessionKey::new("codex-dev", IsolationScope::session("lark1", "chat-1"));

    assert_eq!(store.get(&key).unwrap(), None);

    store.save(&key, "thread-1").unwrap();
    drop(store);

    let reopened = SessionStore::open(path).unwrap();
    assert_eq!(reopened.get(&key).unwrap().as_deref(), Some("thread-1"));

    reopened.save(&key, "thread-2").unwrap();
    assert_eq!(reopened.get(&key).unwrap().as_deref(), Some("thread-2"));
}

#[test]
fn session_store_allows_many_sessions_per_agent_without_reusing_one_backend_session() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::open(temp.path().join("store.db")).unwrap();
    let first = SessionKey::new("codex-dev", IsolationScope::session("lark1", "chat-1"));
    let second = SessionKey::new("codex-dev", IsolationScope::session("lark1", "chat-2"));
    let third = SessionKey::new("codex-dev", IsolationScope::session("lark1", "chat-3"));

    store.save(&first, "thread-1").unwrap();
    store.save(&second, "thread-2").unwrap();

    assert_eq!(store.get(&second).unwrap().as_deref(), Some("thread-2"));
    assert!(store.save(&third, "thread-1").is_err());
    assert_eq!(store.get(&third).unwrap(), None);
}

#[test]
fn session_store_only_removes_the_expected_mapping() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::open(temp.path().join("store.db")).unwrap();
    let key = SessionKey::new("codex-dev", IsolationScope::Shared);
    store.save(&key, "thread-2").unwrap();

    assert!(!store.remove_if_matches(&key, "thread-1").unwrap());
    assert!(store.remove_if_matches(&key, "thread-2").unwrap());
    assert_eq!(store.get(&key).unwrap(), None);
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
