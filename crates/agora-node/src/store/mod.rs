use crate::config::IsolationScope;
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: i64 = 3;
const CREATE_SCHEMA: &str = include_str!("schema.sql");

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    agent_name: String,
    isolation_scope: IsolationScope,
}

impl SessionKey {
    pub fn new(agent_name: impl Into<String>, isolation_scope: IsolationScope) -> Self {
        Self {
            agent_name: agent_name.into(),
            isolation_scope,
        }
    }

    pub fn isolation_scope(&self) -> &IsolationScope {
        &self.isolation_scope
    }

    pub fn channel_name(&self) -> Option<&str> {
        self.isolation_scope.channel_name()
    }

    pub fn channel_session_id(&self) -> Option<&str> {
        self.isolation_scope.session_id()
    }

    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChannelSessionKey {
    channel_name: String,
    session_id: String,
}

impl ChannelSessionKey {
    pub fn new(channel_name: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            channel_name: channel_name.into(),
            session_id: session_id.into(),
        }
    }

    pub fn channel_name(&self) -> &str {
        &self.channel_name
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

#[derive(Clone)]
pub struct SessionStore {
    connection: Arc<Mutex<Connection>>,
}

impl SessionStore {
    pub fn open_default() -> Result<Self> {
        Self::open(Self::default_path()?)
    }

    pub fn default_path() -> Result<PathBuf> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set; cannot resolve agora store path")?;
        Ok(home.join(".agora").join("db").join("store.db"))
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("store path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create store directory failed: {}", parent.display()))?;

        let connection = Connection::open(path)
            .with_context(|| format!("open sqlite store failed: {}", path.display()))?;
        Self::initialize(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn get(&self, key: &SessionKey) -> Result<Option<String>> {
        let connection = self.lock_connection();
        connection
            .query_row(
                "SELECT agent_session_id
                 FROM agent_sessions
                 WHERE isolation_scope = ?1
                   AND channel_name IS ?2
                   AND channel_session_id IS ?3
                   AND agent_name = ?4",
                params![
                    key.isolation_scope().as_str(),
                    key.channel_name(),
                    key.channel_session_id(),
                    key.agent_name()
                ],
                |row| row.get(0),
            )
            .optional()
            .context("query channel-agent session mapping failed")
    }

    pub fn save(&self, key: &SessionKey, agent_session_id: &str) -> Result<()> {
        if agent_session_id.is_empty() {
            bail!("agent session id must not be empty");
        }
        let now = Self::now_millis()?;
        let connection = self.lock_connection();
        let updated = connection
            .execute(
                "UPDATE agent_sessions
                 SET agent_session_id = ?5,
                     updated_at = ?6
                 WHERE isolation_scope = ?1
                   AND channel_name IS ?2
                   AND channel_session_id IS ?3
                   AND agent_name = ?4",
                params![
                    key.isolation_scope().as_str(),
                    key.channel_name(),
                    key.channel_session_id(),
                    key.agent_name(),
                    agent_session_id,
                    now
                ],
            )
            .context("update agent session mapping failed")?;
        if updated == 0 {
            connection
                .execute(
                    "INSERT INTO agent_sessions (
                     isolation_scope,
                     channel_name,
                     channel_session_id,
                     agent_name,
                     agent_session_id,
                     created_at,
                     updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![
                        key.isolation_scope().as_str(),
                        key.channel_name(),
                        key.channel_session_id(),
                        key.agent_name(),
                        agent_session_id,
                        now
                    ],
                )
                .context("insert agent session mapping failed")?;
        }
        Ok(())
    }

    pub fn remove_if_matches(&self, key: &SessionKey, agent_session_id: &str) -> Result<bool> {
        let connection = self.lock_connection();
        let removed = connection
            .execute(
                "DELETE FROM agent_sessions
                 WHERE isolation_scope = ?1
                   AND channel_name IS ?2
                   AND channel_session_id IS ?3
                   AND agent_name = ?4
                   AND agent_session_id = ?5",
                params![
                    key.isolation_scope().as_str(),
                    key.channel_name(),
                    key.channel_session_id(),
                    key.agent_name(),
                    agent_session_id
                ],
            )
            .context("remove channel-agent session mapping failed")?;
        Ok(removed > 0)
    }

    pub fn disable_agent(&self, key: &ChannelSessionKey, agent_name: &str) -> Result<bool> {
        let connection = self.lock_connection();
        let inserted = connection
            .execute(
                "INSERT OR IGNORE INTO channel_session_agent_blocks (
                     channel_name,
                     channel_session_id,
                     agent_name
                 ) VALUES (?1, ?2, ?3)",
                params![key.channel_name(), key.session_id(), agent_name],
            )
            .context("disable agent for channel session failed")?;
        Ok(inserted > 0)
    }

    pub fn enable_agent(&self, key: &ChannelSessionKey, agent_name: &str) -> Result<bool> {
        let connection = self.lock_connection();
        let removed = connection
            .execute(
                "DELETE FROM channel_session_agent_blocks
                 WHERE channel_name = ?1
                   AND channel_session_id = ?2
                   AND agent_name = ?3",
                params![key.channel_name(), key.session_id(), agent_name],
            )
            .context("enable agent for channel session failed")?;
        Ok(removed > 0)
    }

    pub fn is_agent_enabled(&self, key: &ChannelSessionKey, agent_name: &str) -> Result<bool> {
        let connection = self.lock_connection();
        let blocked = connection
            .query_row(
                "SELECT EXISTS (
                     SELECT 1
                     FROM channel_session_agent_blocks
                     WHERE channel_name = ?1
                       AND channel_session_id = ?2
                       AND agent_name = ?3
                 )",
                params![key.channel_name(), key.session_id(), agent_name],
                |row| row.get::<_, bool>(0),
            )
            .context("query agent status for channel session failed")?;
        Ok(!blocked)
    }

    pub fn disabled_agents(&self, key: &ChannelSessionKey) -> Result<Vec<String>> {
        let connection = self.lock_connection();
        let mut statement = connection
            .prepare(
                "SELECT agent_name
                 FROM channel_session_agent_blocks
                 WHERE channel_name = ?1
                   AND channel_session_id = ?2
                 ORDER BY agent_name",
            )
            .context("prepare disabled agent query failed")?;
        let names = statement
            .query_map(params![key.channel_name(), key.session_id()], |row| {
                row.get(0)
            })
            .context("query disabled agents for channel session failed")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("read disabled agents for channel session failed")?;
        Ok(names)
    }

    fn initialize(connection: &Connection) -> Result<()> {
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA busy_timeout = 5000;",
            )
            .context("configure sqlite store failed")?;
        let version = connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .context("read sqlite store schema version failed")?;
        match version {
            0 => {
                connection
                    .execute_batch(CREATE_SCHEMA)
                    .context("create sqlite store schema failed")?;
                connection
                    .pragma_update(None, "user_version", SCHEMA_VERSION)
                    .context("write sqlite store schema version failed")?;
            }
            SCHEMA_VERSION => {
                connection
                    .execute_batch(CREATE_SCHEMA)
                    .context("verify sqlite store schema failed")?;
            }
            version => bail!("unsupported sqlite store schema version: {version}"),
        }
        Ok(())
    }

    fn lock_connection(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now_millis() -> Result<i64> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system time is before unix epoch")?
            .as_millis();
        i64::try_from(millis).context("current timestamp does not fit in sqlite integer")
    }
}
